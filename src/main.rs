//! # ukmm-extractool
//!
//! Extracts and rebuilds UKMM mod files (`.byml`/`.sarc`/`.bnp`) to/from
//! editable YAML and native BYML — messages, actor info, generic mergeables.
//!
//! The only entry point is **interactive mode** (`-i`), which scans installed UKMM mods,
//! lets the user pick one, extracts the single `Msg_*.product.sarc` inside the ZIP,
//! converts it to JSON, and can later rebuild the ZIP from edited JSON.
//!
//! ## Pipeline
//!
//! **Extract**:  ZIP → extract → decompress (zstd/yaz0) → detect format → parse → serialize JSON
//! **Rebuild**:  JSON → build CBOR wire format → zstd compress → inject into new ZIP

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
};
use anyhow::{Context, Result};
use indexmap::IndexMap;
use msyt::{model::Entry, Msyt};
use sevenz_rust::*;
use std::io::Cursor;
use roead::sarc::Sarc;
use serde::{Deserialize, Serialize};

/// Custom zstd dictionary embedded at compile time.
///
/// This dictionary is critical for compatibility with UKMM's compression format.
/// Without it, compression may be less effective or fail for some inputs.
/// The fallback is dictionary-less zstd (with a warning to stderr).
static ZSTD_DICTIONARY: &[u8] = include_bytes!("../data/zsdic");

/// First 6 bytes of a 7z / BCML .bnp archive.
const BNP_MAGIC: &[u8] = &[0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c];

/// First 2 bytes of a raw BYML file, big-endian ("BY") or little-endian ("YB").
const BYML_MAGIC_BE: &[u8] = b"BY";
const BYML_MAGIC_LE: &[u8] = b"YB";

/// Section names to automatically strip from extracted message files.
///
/// These sections are "contaminated" data from BCML that – most of the time –
/// shouldn't be included in the output JSON or rebuilt into UKMM.
const FILTER_SECTIONS: &[&str] = &[
    "EventFlowMsg/MiniGame_Crosscountry",
    "EventFlowMsg/MiniGame_HorsebackArchery",
];

/// Top-level JSON structure produced by the rebuild step.
///
/// The forward path (extract) now always goes through the interactive mode;
/// this struct is used internally when converting JSON back to the
/// UKMM CBOR wire format during rebuild.
///
/// # JSON layout
///
/// For **message files** (Msg_*.product.sarc):
/// ```json
/// {
///   "entries": {
///     "Msg_EUen": {
///       "Npc_RecipeName": { "attributes": null, "contents": [...] },
///       "Npc_ShopItem":   { "attributes": "...", "contents": [...] }
///     }
///   }
/// }
/// ```
///
/// For **ActorInfo files** (ActorInfo.product.byml), the entries are unfolded:
/// ```json
/// {
///   "entries": {
///     "ActorInfo.product": {
///       "1021091464": [[actor_data], false],
///       "2692761260": [[actor_data], false]
///     }
///   },
///   "format": "ActorInfo"
/// }
/// ```
#[derive(Serialize, Deserialize)]
struct Output {
    /// 4-letter language code (e.g. "USen", "EUfr"), extracted from filename/section name.
    /// Optional — can be omitted from JSON; extracted from filename on rebuild.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    /// Must equal `entries.len()`. Validated by `from_json_to_cbor()`. 
    /// Optional — omitted from JSON; recomputed from `entries` on rebuild.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entry_count: Option<usize>,
    /// Section name → entries.
    /// For message files this is an object with label → Entry pairs.
    /// For ActorInfo/BYML this contains the unfolded data directly.
    /// Uses `BTreeMap` for deterministic key ordering.
    entries: BTreeMap<String, serde_json::Value>,
    /// Source format hint: `"SARC"`, `"UKMM CBOR"`, `"BYML"`, `"ActorInfo"`.
    /// Omitted from JSON when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    format: Option<String>,
}

/// Try to decompress a zstd-compressed buffer using the custom UKMM dictionary.
///
/// Falls back to dictionary-less zstd if the dictionary-based decompressor
/// can't be constructed or the decompression itself fails.
/// Try to decompress a zstd-compressed buffer using the custom UKMM dictionary.
///
/// Falls back to dictionary-less zstd if the dictionary-based decompressor
/// can't be constructed or the decompression itself fails.
///
/// # Resource limits
///
/// Decompression buffer is capped at 512 MiB to prevent CWE-400
/// (uncontrolled resource consumption) from malicious inputs.
const ZSTD_MAX_DECOMPRESS_SIZE: usize = 512 * 1024 * 1024;

fn zstd_decompress(data: &[u8]) -> Result<Vec<u8>> {
    // Attempt dictionary-based decompression first (UKMM's preferred format).
    if let Ok(mut d) = zstd::bulk::Decompressor::with_dictionary(ZSTD_DICTIONARY) {
        // upper_bound() may error for compressed data — cap buffer size.
        let size = zstd::bulk::Decompressor::upper_bound(data)
            .unwrap_or(data.len().saturating_mul(1024))
            .min(ZSTD_MAX_DECOMPRESS_SIZE);
        if let Ok(out) = d.decompress(data, size) { return Ok(out); }
    }
    eprintln!("Warning: custom dictionary decompression failed, falling back to dictionary-less zstd");
    // Streaming decoder with explicit size cap.
    let mut out = Vec::with_capacity(data.len().min(ZSTD_MAX_DECOMPRESS_SIZE));
    let mut decoder = zstd::Decoder::new(data)?;
    decoder.read_to_end(&mut out)?;
    if out.len() > ZSTD_MAX_DECOMPRESS_SIZE {
        anyhow::bail!("zstd decompressed output exceeds {ZSTD_MAX_DECOMPRESS_SIZE} bytes — possible bomb");
    }
    Ok(out)
}

/// Compress data with zstd, preferring the custom UKMM dictionary at compression level 8.
///
/// Falls back to dictionary-less zstd if the dictionary-based compressor
/// can't be constructed or the compression itself fails.
fn zstd_compress(data: &[u8]) -> Result<Vec<u8>> {
    // Attempt dictionary-based compression first.
    if let Ok(mut c) = zstd::bulk::Compressor::with_dictionary(8, ZSTD_DICTIONARY) {
        if let Ok(out) = c.compress(data) { return Ok(out); }
    }
    // Fallback: dictionary-less zstd at level 8.
    zstd::encode_all(data, 8).context("zstd compress failed")
}

/// Encode a UTF-8 string into CBOR text (major type 3).
///
/// Supports all five CBOR length encodings:
/// - 0..=23: inline (0x60 | len)
/// - 24..=255: 0x78 + 1-byte length
/// - 256..=65535: 0x79 + 2-byte big-endian length
/// - 65536..=0xFFFFFFFF: 0x7A + 4-byte big-endian length
/// - >0xFFFFFFFF: 0x7B + 8-byte big-endian length
fn cbor_write_text(buf: &mut Vec<u8>, s: &str) {
    let len = s.len();
    if len <= 23 {
        buf.push(0x60 | (len as u8));
    } else if len <= 0xFF {
        buf.push(0x78);          buf.push(len as u8);
    } else if len <= 0xFFFF {
        buf.push(0x79);          buf.extend_from_slice(&(len as u16).to_be_bytes());
    } else if len <= 0xFFFF_FFFF {
        buf.push(0x7A);          buf.extend_from_slice(&(len as u32).to_be_bytes());
    } else {
        buf.push(0x7B);          buf.extend_from_slice(&(len as u64).to_be_bytes());
    }
    buf.extend_from_slice(s.as_bytes());
}

/// Encode a CBOR map header (major type 5) with a given number of entries.
///
/// Uses the same length-encoding scheme as `cbor_write_text`:
/// 0..=23 inline, then 1/2/4/8-byte prefixes for progressively larger sizes.
fn cbor_write_map_header(buf: &mut Vec<u8>, len: usize) {
    if len <= 23 {
        buf.push(0xA0 | (len as u8));
    } else if len <= 0xFF {
        buf.push(0xB8);
        buf.push(len as u8);
    } else if len <= 0xFFFF {
        buf.push(0xB9);
        buf.extend_from_slice(&(len as u16).to_be_bytes());
    } else if len <= 0xFFFF_FFFF {
        buf.push(0xBA);
        buf.extend_from_slice(&(len as u32).to_be_bytes());
    } else {
        buf.push(0xBB);
        buf.extend_from_slice(&(len as u64).to_be_bytes());
    }
}

/// Build the UKMM-specific CBOR wire format from a JSON `Output` struct.
///
/// The resulting CBOR structure is:
///
/// ```text
/// CBOR map (1 entry)
///   key: "Mergeable"
///   value: CBOR map (1 entry)
///     key: "MessagePack"
///     value: CBOR map (N entries)
///       key: section_name (e.g. "Msg_EUen")
///       value: JSON string {"group_count":N,"entries":{...}}
/// ```
///
/// This CBOR blob is then zstd-compressed (with dictionary) and returned as a
/// self-contained compressed binary — *not* a SARC archive.
///
/// # Validation (returns an error if any check fails)
///
/// - `language` must not be empty and ≤ 64 chars (if present)
/// - `entries` must not be empty
/// - `entry_count` must match `entries.len()` (if present)
/// - Each section name: non-empty, ≤ 512 chars, no `..`, no control characters
fn from_json_to_cbor(out: &Output) -> Result<Vec<u8>> {
    // ── Input validation ──────────────────────────────────────────────────

    if let Some(ref lang) = out.language {
        if lang.is_empty() {
            anyhow::bail!("Output language field is empty — refusing to produce CBOR");
        }
        if lang.len() > 64 {
            anyhow::bail!(
                "Output language field is suspiciously long ({} chars) — refusing to produce CBOR",
                lang.len()
            );
        }
    }
    if out.entries.is_empty() {
        anyhow::bail!("Output has no entries — refusing to produce empty CBOR");
    }
    if let Some(ec) = out.entry_count {
        if ec != out.entries.len() {
            anyhow::bail!(
                "Output entry_count ({ec}) does not match entries map length ({}) — data may be corrupted",
                out.entries.len()
            );
        }
    }

    // Validate each section name for length and safety.
    for section_name in out.entries.keys() {
        if section_name.is_empty() {
            anyhow::bail!("Output contains an empty section name — refusing to produce CBOR");
        }
        if section_name.len() > 512 {
            anyhow::bail!(
                "Section name '{section_name}' is too long ({} chars) — refusing to produce CBOR",
                section_name.len()
            );
        }
        if section_name.contains("..") {
            anyhow::bail!(
                "Section name '{section_name}' contains '..' (path traversal) — refusing to produce CBOR"
            );
        }
        if section_name.chars().any(|c| c.is_control()) {
            anyhow::bail!(
                "Section name '{section_name:?}' contains control characters — refusing to produce CBOR"
            );
        }
    }

    // ── Build inner entries: section_name → Msyt JSON string ──────────────

    let mut inner_entries: BTreeMap<String, String> = BTreeMap::new();

    for (section_name, entries) in &out.entries {
        let entries_json = serde_json::to_string(entries)
            .with_context(|| format!("Failed to serialize entries for {section_name}"))?;
        let group_count = entries.as_object().map_or(0, |o| o.len()) as u32;

        // Wrap entries in the Msyt JSON envelope: {"group_count":N,"entries":{...}}
        let msyt_json = format!(
            "{{\"group_count\":{group_count},\"entries\":{entries_json}}}"
        );
        inner_entries.insert(section_name.clone(), msyt_json);
    }

    // ── Encode the CBOR structure ─────────────────────────────────────────

    let mut buf = Vec::with_capacity(65536);

    // Outer map: 1 entry (key "Mergeable" → inner map)
    buf.push(0xA1);
    cbor_write_text(&mut buf, "Mergeable");

    // Inner map: 1 entry (key "MessagePack" → section map)
    buf.push(0xA1);
    cbor_write_text(&mut buf, "MessagePack");

    // Section map: N entries (section_name → Msyt JSON string)
    cbor_write_map_header(&mut buf, inner_entries.len());
    for (key, value) in &inner_entries {
        cbor_write_text(&mut buf, key);
        cbor_write_text(&mut buf, value);
    }

    eprintln!("zstd compress...");
    let sarc = zstd_compress(&buf)?;
    Ok(sarc)
}

/// Decompress a raw input buffer through the zstd → yaz0 pipeline.
///
/// 1. If the first 4 bytes are the zstd magic `0x28B52FFD`, decompress with zstd.
/// 2. If the result starts with `Yaz0`, decompress with yaz0.
/// 3. Otherwise return the (possibly zstd-decompressed) data as-is.
///
/// This handles the common case where `.product.sarc` files are:
///   zstd-compressed → Yaz0 archive → SARC or CBOR inside.
fn decompress(raw: &[u8]) -> Result<Vec<u8>> {
    // Check for zstd magic bytes: 0x28 0xB5 0x2F 0xFD
    let is_zstd = raw.len() > 4 && raw[0..4] == [0x28, 0xB5, 0x2F, 0xFD];
    let d = if is_zstd { eprintln!("zstd..."); zstd_decompress(raw)? } else { raw.to_vec() };
    // Check for yaz0 magic after zstd decompression
    if d.len() > 4 && d[0..4] == [b'Y', b'a', b'z', b'0'] {
        eprintln!("yaz0..."); Ok(roead::yaz0::decompress(&d)?)
    } else { Ok(d) }
}

/// Heuristic: does this byte buffer look like a SARC archive?
///
/// Checks for the `SARC` magic bytes at either offset 0 or offset 0x11
/// (some SARC files have a 0x11-byte header before the magic).
/// Also requires at least 0x21 bytes to avoid false positives.
fn is_sarc(d: &[u8]) -> bool {
    d.len() > 0x20 && (d[0..4] == [b'S',b'A',b'R',b'C'] || d[0x11..0x15] == [b'S',b'A',b'R',b'C'])
}

/// Heuristic: does the first byte look like a CBOR map header?
///
/// In CBOR, major type 5 (map) uses the high 3 bits = `0b101` (0xA0).
/// We mask with `0xE0` and compare to `0xA0`.
fn looks_like_cbor(d: &[u8]) -> bool {
    !d.is_empty() && (d[0] & 0xE0) == 0xA0  }

/// Heuristic: does this byte buffer look like raw BYML?
///
/// Checks for the `BY` (big endian / Wii U) or `YB` (little endian / Switch) magic.
fn looks_like_byml(d: &[u8]) -> bool {
    d.len() > 4 && (d[0..2] == *BYML_MAGIC_BE || d[0..2] == *BYML_MAGIC_LE)
}

/// Extract the stem (filename without extension) from a path as a `String`.
///
/// Returns `"unknown"` if the filename can't be converted to UTF-8.
fn filename_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("unknown")
        .to_string()
}

/// Parse a SARC archive containing `.msbt` message files into an `Output` struct.
///
/// For each `.msbt` file inside the SARC:
/// 1. Parse the MSBT bytes via `Msyt::from_msbt_bytes()`
/// 2. Insert entries into the output map keyed by the file stem (without `.msbt` extension)
///
/// The language code is **not** extracted here — it's set by `convert_file()` from the filename.
fn parse_sarc(data: &[u8]) -> Result<Output> {
    let mut entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let sarc = Sarc::new(data)?;
    for f in sarc.files() {
        let n = match f.name { Some(s) => s, None => continue };
        if !n.ends_with(".msbt") { continue; }
        let stem = n.trim_end_matches(".msbt").to_string();
        let msyt = Msyt::from_msbt_bytes(f.data())?;
        let bt: IndexMap<String, Entry> = msyt.entries.into_iter().collect();
        entries.insert(stem, serde_json::to_value(bt)?);
    }
    Ok(Output { language: None, entry_count: None, entries, format: None })
}

/// Extract all CBOR text strings (major type 3) and byte strings (major type 2)
/// from a raw byte buffer.
///
/// This is a manual CBOR parser that walks the byte stream looking for string
/// items. It skips all other CBOR types (arrays, maps, ints, floats, tags, etc.)
/// by computing their byte-length and advancing past them.
///
/// # Safety limits
///
/// - Strings longer than `MAX_STRING_LEN` (100 MiB) are skipped with a warning.
/// - On 32-bit targets, strings whose encoded length exceeds `usize::MAX` are skipped.
/// - Indefinite-length strings (CBOR AI 31) and reserved AI values (28-30) are skipped.
/// - Empty strings are silently dropped (filtered out).
///
/// # CBOR major type reference
///
/// | mt | Type      | Action |
/// |----|-----------|--------|
/// | 0  | uint      | skip   |
/// | 1  | nint      | skip   |
/// | 2  | bytes     | extract as UTF-8 |
/// | 3  | text      | extract as UTF-8 |
/// | 4  | array     | skip header |
/// | 5  | map       | skip header |
/// | 6  | tag       | skip      |
/// | 7  | float/etc | skip      |
fn extract_cbor_strings(data: &[u8]) -> Vec<String> {
    /// Maximum string length to process (100 MiB). Anything larger is skipped.
    const MAX_STRING_LEN: usize = 100 * 1024 * 1024;

    // Pre-allocate: typical CBOR blobs have a few dozen strings at most.
    let mut strings = Vec::with_capacity(128);
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        // Major type = high 3 bits, additional info = low 5 bits.
        let mt = (b >> 5) & 0x07;
        let ai = (b & 0x1f) as usize;

        match mt {
            // ── Major type 2 (byte string) & 3 (text string) ──
            2 | 3 => {
                let (sl, adv) = match ai {
                    0..=23 => (ai, 1),
                    24 if i + 1 < data.len() => (data[i + 1] as usize, 2),
                    25 if i + 2 < data.len() => {
                        (u16::from_be_bytes([data[i + 1], data[i + 2]]) as usize, 3)
                    }
                    26 if i + 4 < data.len() => {
                        let n = u32::from_be_bytes([
                            data[i + 1], data[i + 2], data[i + 3], data[i + 4],
                        ]);
                        (n as usize, 5)
                    }
                    27 if i + 8 < data.len() => {
                        let n = u64::from_be_bytes([
                            data[i + 1], data[i + 2], data[i + 3], data[i + 4],
                            data[i + 5], data[i + 6], data[i + 7], data[i + 8],
                        ]);
                        // On 32-bit targets, skip strings that don't fit in address space.
                        #[cfg(target_pointer_width = "32")]
                        if n > usize::MAX as u64 {
                            eprintln!(
                                "Warning: CBOR string length {n} exceeds addressable memory; skipping"
                            );
                            i += 9;
                            continue;
                        }
                        (n as usize, 9)
                    }

                    // Reserved AI values (28-30): valid CBOR but no defined string encoding.
                    28..=30 => {
                        eprintln!(
                            "Warning: CBOR reserved additional info {ai} for string at offset {i}; skipping byte"
                        );
                        i += 1;
                        continue;
                    }

                    // Indefinite-length strings (AI 31): not supported.
                    31 => {
                        eprintln!(
                            "Warning: CBOR indefinite-length string at offset {i} not supported; skipping"
                        );
                        i += 1;
                        continue;
                    }
                    _ => {
                        i += 1;
                        continue;
                    }
                };

                if sl > MAX_STRING_LEN {
                    eprintln!(
                        "Warning: CBOR string length {sl} exceeds safety limit of {MAX_STRING_LEN} bytes; skipping"
                    );
                    i += adv;
                    continue;
                }

                let str_start = i + adv;
                let str_end = str_start.saturating_add(sl);

                if str_end <= data.len() {
                    if let Ok(s) = std::str::from_utf8(&data[str_start..str_end]) {
                        if !s.is_empty() {
                            strings.push(s.to_string());
                        }
                    }
                }

                i = str_end.min(data.len());
                continue;
            }

            // ── Major types 4 (array) & 5 (map) ──
            // Skip the header bytes so we don't treat contained items as top-level strings.
            4 | 5 => {
                let extra = match ai {
                    0..=23 => 0,
                    24 => 1,
                    25 => 2,
                    26 => 4,
                    27 => 8,
                    // Reserved / indefinite-length containers.
                    28..=31 => {
                        eprintln!(
                            "Warning: CBOR unsupported container AI {ai} at offset {i}; skipping"
                        );
                        i += 1;
                        continue;
                    }
                    _ => {
                        i += 1;
                        continue;
                    }
                };
                i += 1 + extra;
                continue;
            }

            // ── Major type 6 (tag) ──
            6 => {
                let extra = match ai {
                    0..=23 => 0,
                    24 => 1,
                    25 => 2,
                    26 => 4,
                    27 => 8,
                    _ => 0,
                };
                i += 1 + extra;
                continue;
            }

            // ── Major type 7 (float / simple / break) ──
            7 => {
                let extra = match ai {
                    0..=23 => 0,                           // simple value
                    24 => 1,                               // 1-byte simple
                    25 => 2,                               // half-precision float
                    26 => 4,                               // single-precision float
                    27 => 8,                               // double-precision float
                    28..=31 => 0,                           // stop/break/indefinite
                    _ => 0,
                };
                i += 1 + extra;
                continue;
            }

            // ── Major type 0 (uint) & 1 (negative int) ──
            0 | 1 => {
                let extra = match ai {
                    0..=23 => 0,
                    24 => 1,
                    25 => 2,
                    26 => 4,
                    27 => 8,
                    _ => 0,
                };
                i += 1 + extra;
                continue;
            }

            _ => {
                i += 1;
                continue;
            }
        }
    }
    strings
}

/// Parse a CBOR-encoded UKMM message blob into an `Output` struct.
///
/// This is the forward-path counterpart to `from_json_to_cbor()`.
///
/// # Strategy
///
/// 1. Extract all text strings from the CBOR using `extract_cbor_strings()`.
/// 2. Walk the string list looking for `(non-JSON, JSON)` pairs where the
///    first string is a section name and the second is a Msyt JSON blob.
///    Detection: first string doesn't start with `{`, second does and
///    contains `"entries"` and either `"contents"` or `"group_count"`.
/// 3. For each JSON blob, parse the `"entries"` object into `IndexMap<String, Entry>`.
///
/// # Fallback
///
/// If no entries are found via the string-pairing heuristic, the function
/// tries `Msyt::from_msbt_bytes()` on the raw data as a last resort (treating
/// the whole blob as raw MSBT). This handles edge cases where the CBOR structure
/// is unusual.
fn parse_cbor(data: &[u8]) -> Result<Output> {
    let strings = extract_cbor_strings(data);
    let mut entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();

    // ── Pair up non-JSON names with JSON blobs ────────────────────────────
    let mut names: Vec<String> = Vec::new();
    let mut json_blobs: Vec<String> = Vec::new();
    let mut i = 0;
    while i < strings.len() {
        if i + 1 < strings.len() {
            let curr = &strings[i];
            let next = &strings[i+1];
            // Heuristic: non-JSON name followed by a JSON blob containing "entries"
            if !curr.starts_with("{") && next.starts_with("{") && next.contains("\"entries\":") && (next.contains("\"contents\":") || next.contains("\"group_count\":")) {
                names.push(curr.clone());
                json_blobs.push(next.clone());
                i += 2;
                continue;
            }
        }
        // Also accept standalone JSON blobs that look like Msyt data.
        if strings[i].contains("\"entries\":") && strings[i].contains("\"contents\":") {
            json_blobs.push(strings[i].clone());
        }
        i += 1;
    }

    // ── Deserialize each JSON blob into the entries map ───────────────────
    for (i, blob) in json_blobs.iter().enumerate() {
        let name = names.get(i).cloned().unwrap_or_else(|| format!("section_{i}"));

        // Deserialize the Msyt envelope: {"group_count":N,"entries":{...}}
        // Extract "entries" directly from the JSON string to avoid a clone.
        let entries_val: serde_json::Value = match serde_json::from_str(blob) {
            Ok(serde_json::Value::Object(mut map)) => {
                map.remove("entries")
                    .ok_or_else(|| anyhow::anyhow!("missing 'entries' key"))
            }
            Ok(_) => {
                eprintln!("Warning: skipping JSON blob at index {i} — not an object");
                continue;
            }
            Err(e) => {
                eprintln!("Warning: skipping invalid JSON at index {i}: {e}");
                continue;
            }
        }.unwrap_or_else(|_| {
            eprintln!("Warning: skipping JSON blob at index {i} — missing 'entries' key");
            serde_json::Value::Null
        });

        if entries_val.is_null() || !entries_val.is_object() {
            eprintln!("Warning: skipping JSON blob at index {i} — 'entries' is not an object");
            continue;
        }

        // Deserialize as IndexMap<String, Entry> for message data
        match serde_json::from_value::<IndexMap<String, Entry>>(entries_val) {
            Ok(im) => {
                if im.is_empty() {
                    eprintln!("Warning: section '{name}' has zero entries after deserialization");
                }
                entries.insert(name, serde_json::to_value(im)?);
            }
            Err(e) => {
                eprintln!("Warning: failed to deserialize entries for section '{name}': {e}");
            }
        }
    }

    // ── Last resort: try parsing as raw MSBT ──────────────────────────────
    if entries.is_empty() {
        let msyt = Msyt::from_msbt_bytes(data).context("No entries found in CBOR blob")?;
        let bt: IndexMap<String, Entry> = msyt.entries.into_iter().collect();
        entries.insert("section_0".to_string(), serde_json::to_value(bt)?);
    }

    Ok(Output { language: None, entry_count: None, entries, format: None })
}

/// Serialize an `Output` struct to YAML and write to a file.
///
/// Creates parent directories if they don't exist. Prints a confirmation
/// message to stderr (so stdout stays clean for pipe usage).
fn write_output(out: &Output, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut val = serde_json::to_value(out)?;

    // Strip fields that are redundant or cause validation issues.
    // At rebuild time, the format is auto-detected from the entries structure.
    if let Some(obj) = val.as_object_mut() {
        obj.remove("language");
        obj.remove("entry_count");
        obj.remove("format");
    }
    let yaml = serde_yaml::to_string(&val)?;
    fs::write(path, &yaml)?;
    eprintln!("  ✓ Wrote {} entries to {}", out.entries.len(), path.display());
    Ok(())
}

/// Detect the source format from the entries structure alone.
///
/// This is used at rebuild time when the YAML has no `format:` field
/// (stripped by [`write_output`] for cleaner output).
///
/// Rules:
/// - **Mergeable**: the single entry value is an object whose first key is
///   `"Mergeable"` — e.g. `{ "Mergeable": { "GenericByml": { ... } } }`.
/// - **ActorInfo**: every key in the entry value looks like a decimal u32 hash
///   (at least 1 entry matches) and the value is a 2-element array
///   `[ { "Map": {...} }, bool ]`.
/// - **BYML**: the entry value contains a `"__byml__"` key (legacy wrapper).
/// - **default** (message): standard Msyt entries with `"contents"` arrays.
fn detect_format(entries: &BTreeMap<String, serde_json::Value>) -> Option<&'static str> {
    // Empty → can't determine.
    if entries.is_empty() {
        return None;
    }

    // Count how many entries look like each format.
    let mut mergeable = 0usize;
    let mut actorinfo = 0usize;
    let mut byml = 0usize;
    let mut message = 0usize;

    for val in entries.values() {
        match val {
            // Mergeable: `{ "Mergeable": { ... } }` — top-level key is "Mergeable"
            serde_json::Value::Object(map) if map.contains_key("Mergeable") => {
                mergeable += 1;
            }
            // BYML: contains "__byml__" key
            serde_json::Value::Object(map) if map.contains_key("__byml__") => {
                byml += 1;
            }
            // ActorInfo: at least one key is a decimal u32, value is [obj, bool]
            serde_json::Value::Object(map) => {
                let has_hash = map.keys().any(|k| k.parse::<u32>().is_ok());
                let is_actor = has_hash
                    && map.values().any(|v| {
                        v.as_array()
                            .map(|a| a.len() == 2 && a[0].is_object() && (a[1].is_boolean() || a[1].is_null()))
                            .unwrap_or(false)
                    });
                if is_actor {
                    actorinfo += 1;
                } else {
                    message += 1;
                }
            }
            // Arrays or scalars → assume message context
            _ => message += 1,
        }
    }

    // Pick the format with the most votes (deterministic tie-breaking).
    let max = mergeable.max(actorinfo).max(byml).max(message);
    if max == 0 {
        return None;
    }
    if mergeable == max { Some("Mergeable") }
    else if actorinfo == max { Some("ActorInfo") }
    else if byml == max { Some("BYML") }
    else { None }  // message entries need no format hint
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // If a file was passed as CLI argument or drag-dropped onto the exe,
    // bypass the menu and process it directly.
    if let Some(arg) = args.first() {
        let path = arg.trim_matches('"');
        let p = Path::new(path);
        if !p.exists() {
            anyhow::bail!("File not found: {}", path);
        }
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        match ext {
            "bnp" | "7z" => return handle_bnp_interactive_for(path),
            "byml" | "sbyml" => {
                let out = convert_file(path)?;
                let stem = filename_stem(p);

                // ── ActorInfo → direct .sbyml (BYML natif) ──────────────
                if out.format.as_deref() == Some("ActorInfo") {
                    let compressed = actorinfo_output_to_sbyml(&out)?;
                    let sbyml_name = if stem.ends_with(".product") {
                        stem.to_string() + ".sbyml"
                    } else {
                        format!("{stem}.product.sbyml")
                    };
                    let output_path = p.with_file_name(&sbyml_name);
                    fs::write(&output_path, &compressed)?;
                    println!("  ✓ Converted to native BYML: {}", output_path.display());
                    println!("\nDone!\n");
                    return Ok(());
                }

                // ── Other formats → YAML ──────────────────────────────────
                let output_path = p.with_file_name(format!("{stem}.yaml"));
                write_output(&out, &output_path)?;
                println!("\nDone!\n");
                return Ok(());
            }
            _ => {} // fall through to interactive
        }
    }

    // On Linux, when launched by double-click, there's no terminal attached.
    // Re-launch inside a terminal so the user can interact with the program.
    if cfg!(target_os = "linux") && !atty::is(atty::Stream::Stdin) {
        for term in ["xterm -e", "gnome-terminal --", "konsole -e", "xfce4-terminal --"] {
            let parts: Vec<&str> = term.split_whitespace().collect();
            let (cmd, args) = parts.split_first().unwrap_or((&"xterm", &[]));
            let exe = std::env::current_exe()?;
            let mut child = std::process::Command::new(cmd);
            child.args(args);
            child.arg(&exe);
            if child.spawn().is_ok() {
                // The new terminal window owns the interaction; this process exits.
                return Ok(());
            }
        }
        // No terminal found — fall through to interactive mode (will print nothing).
    }

    // ── Main menu loop ───────────────────────────────────────────────────
    loop {
        let result = run_interactive();
        match &result {
            Ok(()) => {
                prompt("\nPress Enter to return to menu... ");
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("Return to menu.") {
                    // Cancel or no mods found — go back to menu.
                    continue;
                }
                eprintln!("Error: {e:#}");
                prompt("\nPress Enter to retry... ");
            }
        }
    }
}

/// Print a prompt to stdout, flush, and read a single line from stdin.
///
/// Returns the trimmed line (without trailing newline). Returns empty string
/// on any I/O error (e.g. EOF).
///
/// If the input looks like a `.bnp` or `.7z` file path (drag & drop at any prompt),
/// the BNP workflow is launched immediately and the program exits.
fn prompt(message: &str) -> String {
    print!("{message}");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).ok();
    let line = line.trim().to_string();

    // Detect a .bnp / .7z file dropped at any prompt.
    let path = line.trim_matches('"');
    if Path::new(path).extension().is_some_and(|e| e == "bnp" || e == "7z") && Path::new(path).exists() {
        eprintln!();
        let result = handle_bnp_interactive_for(path);
        if let Err(e) = result {
            eprintln!("Error: {e:#}");
        }
        prompt("\nPress Enter to exit... ");
        std::process::exit(0);
    }

    line
}

/// Interactive checkbox-style multi-select prompt.
///
/// Shows `items` with `[ ]` / `[x]` markers. The user enters a number to
/// toggle an item and presses Enter (empty input) to confirm.
/// Returns the list of selected items.
/// Resolve the UKMM data directory based on platform conventions.
///
/// Resolution order:
/// 1. `%LOCALAPPDATA%/ukmm` (Windows)
/// 2. `$XDG_DATA_HOME/ukmm` (Linux)
/// 3. `~/.local/share/ukmm` (Linux/macOS fallback)
/// 4. `./ukmm` (last resort)
fn ukmm_data_dir() -> PathBuf {
    // Windows: %LOCALAPPDATA% is the standard per-user app data directory.
    if let Ok(appdata) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(appdata).join("ukmm");
    }
    // Linux: XDG_DATA_HOME is the standard data directory.
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("ukmm");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local").join("share").join("ukmm");
    }
    // Last resort: relative path.
    PathBuf::from("ukmm")
}

/// A discovered UKMM mod in the interactive mod picker.
struct ModEntry {
    /// Human-readable display name (from `meta.yml` or filename stem).
    display_name: String,
    /// Path to the mod's ZIP file or directory.
    path: PathBuf,
    /// `true` if this is a loose directory (not a ZIP).
    is_dir: bool,
}

/// Extract the `name:` field from a UKMM `meta.yml` file.
///
/// Returns `None` if the file can't be read or the `name:` field is missing/empty.
fn read_meta_name(meta_path: &Path) -> Option<String> {
    let content = fs::read_to_string(meta_path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if let Some(stripped) = line.strip_prefix("name:") {
            let name = stripped.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Recursively check if a directory contains any .yaml or .sbyml files.
fn has_json_or_sbyml_recursive(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else { return false };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if has_json_or_sbyml_recursive(&path) {
                return true;
            }
        } else if let Some(ext) = path.extension().and_then(|x| x.to_str()) {
            if ext == "yaml" || ext == "sbyml" {
                return true;
            }
        }
    }
    false
}

/// Recursively collect all .yaml and .sbyml files under `dir`.
fn collect_edited_files(dir: &Path, files: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_edited_files(&path, files);
            } else if let Some(ext) = path.extension().and_then(|x| x.to_str()) {
                if ext == "yaml" || ext == "sbyml" {
                    files.push(path);
                }
            }
        }
    }
}

/// Interactive mode: scan UKMM mods, pick one, convert all message files.
///
/// # Flow
///
/// 1. Ask user to select platform (Wii U / Switch)
/// 2. Scan the corresponding UKMM mods directory for ZIPs (with `Msg_*` files)
///    and loose folders (with `meta.yml` + `Msg_*` files)
/// 3. Present a numbered list, let the user choose
/// 4. Extract/copy the mod to a temp directory
/// 5. Convert each `Msg_*.product.sarc` file to JSON under `mods/<platform>/<mod_name>/`
/// 6. Save the original mod as `<mod_name>_backup.zip`
/// 7. If output already exists, offer to rebuild instead
fn run_interactive() -> Result<()> {
    println!();
    println!("╔═════════════════════════╗");
    println!("║     ukmm-extractool     ║");
    println!("╚═════════════════════════╝");
    println!();

    let ukmm_root = ukmm_data_dir();
    let wiiu_path = ukmm_root.join("wiiu").join("mods");
    let nx_path = ukmm_root.join("nx").join("mods");

    // ── Platform / Source selection ───────────────────────────────────────
    let plat_choice = loop {
        println!("Choose your platform:");
        println!("  [1] Wii U");
        println!("  [2] Switch");
        println!("  [3] Load a .bnp file");
        println!("  [4] Info");
        let c = prompt("\nSelect 1, 2, 3 or 4: ");
        match c.as_str() {
            "1" | "2" | "3" | "4" => break c,
            _ => eprintln!("Invalid choice — enter 1, 2, 3, or 4.\n"),
        }
    };

    // Option 4: show info.
    if plat_choice == "4" {
        println!();
        println!("╔════════════════════════════════════════════════╗");
        println!("║               ukmm-extractool                  ║");
        println!("╠════════════════════════════════════════════════╣");
        println!("║  Extract and rebuild UKMM mod files:           ║");
        println!("║                                                ║");
        println!("║  • Message/*.product.sarc → structured .yaml   ║");
        println!("║    (Msyt entries, editable, round-trip)        ║");
        println!("║                                                ║");
        println!("║  • Actor/*.byml (mergeable CBOR) → .sbyml      ║");
        println!("║    (native BYML, edit with TotkBits)           ║");
        println!("║                                                ║");
        println!("║  • Actor/ActorInfo.product.byml → .sbyml       ║");
        println!("║    (CBOR ActorInfo ↔ Actors/Hashes BYML)       ║");
        println!("║                                                ║");
        println!("║  • Other .byml files handled as GenericByml    ║");
        println!("║    or fallback YAML for non-roead formats.     ║");
        println!("║                                                ║");
        println!("║  • BCML .bnp archives: texts.json +            ║");
        println!("║    actorinfo.yml extraction & rebuild.         ║");
        println!("║                                                ║");
        println!("║  Supported formats:                            ║");
        println!("║    - .byml / .sbyml (native Nintendo BYML)     ║");
        println!("║    - UKMM .sarc / CBOR mergeable archives      ║");
        println!("║    - .yaml / .yml (editable workspace)         ║");
        println!("║    - BCML .bnp (7z)                            ║");
        println!("╚════════════════════════════════════════════════╝");
        println!();
        prompt("Press Enter to continue... ");
        return run_interactive();
    }

    // Option 3: process a .bnp file with full workspace management.
    if plat_choice == "3" {
        return handle_bnp_interactive();
    }

    let is_switch = plat_choice == "2";
    let (platform, mods_dir) = if is_switch {
        ("nx", nx_path)
    } else {
        ("wiiu", wiiu_path)
    };

    if !mods_dir.is_dir() {
        anyhow::bail!("Directory not found: {}\nMake sure UKMM is installed.", mods_dir.display());
    }

    // ── Scan for mods ─────────────────────────────────────────────────────
    println!("\nScanning {} \n", mods_dir.display());

    let mut mods: Vec<ModEntry> = Vec::new();

    // Pass 1: ZIP files containing Msg_* or .byml files.
    if let Ok(entries) = fs::read_dir(&mods_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "zip") && (peek_zip_has_msg(&path) || peek_zip_has_byml(&path)) {
                let display = read_zip_meta_name(&path)
                    .unwrap_or_else(|| filename_stem(&path));
                mods.push(ModEntry { display_name: display, path, is_dir: false });
            }
        }
    }

    // Pass 2: Loose directories with meta.yml and Msg_* or .byml files.
    if let Ok(entries) = fs::read_dir(&mods_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let meta_path = path.join("meta.yml");
                if meta_path.is_file() && (!find_msg_files(&path).is_empty() || !find_byml_files(&path).is_empty()) {
                    let display = read_meta_name(&meta_path)
                        .unwrap_or_else(|| filename_stem(&path));
                    mods.push(ModEntry { display_name: display, path, is_dir: true });
                }
            }
        }
    }

    mods.sort_by_key(|a| a.display_name.to_lowercase());

    if mods.is_empty() {
        eprintln!("No mods found in {}.", mods_dir.display());
        anyhow::bail!("Return to menu.");
    }

    // ── Mod selection ─────────────────────────────────────────────────────
    let mod_label = if mods.len() == 1 { "mod" } else { "mods" };
    println!("Found {} {} with text inside:\n", mods.len(), mod_label);
    for (i, m) in mods.iter().enumerate() {
        println!("  [{:2}] {}", i + 1, m.display_name);
    }

    let selection = prompt(&format!("\nSelect a mod to process (1-{}), or press Enter to cancel: ", mods.len()));
    if selection.is_empty() {
        println!("Cancelled.\n");
        anyhow::bail!("Return to menu.");
    }
    let index: usize = match selection.parse::<usize>() {
        Ok(n) if n >= 1 && n <= mods.len() => n - 1,
        _ => {
            anyhow::bail!("Invalid selection.");
        }
    };
    let chosen = &mods[index];
    let mod_name = filename_stem(&chosen.path);

    println!("\n  Selected: {}", chosen.display_name);

    let mod_dir_arg = format!("{}/{}", platform, &mod_name);
    let mods_out_dir = PathBuf::from("mods").join(&mod_dir_arg);

    // Check for existing workspace (backup ZIP + any .json or .sbyml files recursively).
    let has_existing = mods_out_dir.join(format!("{mod_name}_backup.zip")).is_file()
        && has_json_or_sbyml_recursive(&mods_out_dir);
    let action = if has_existing {
        let a = loop {
            let c = prompt("\nA workspace has been found. What to do with it?\n[1] Rebuild (send edited files to UKMM)\n[2] Extract again (UKMM > mod files)\n[3] Restore original (from backup)\n\nSelect 1, 2, or 3: ");
            match c.trim() {
                "1" => break "rebuild",
                "2" => break "extract",
                "3" => break "restore",
                _ => eprintln!("Invalid choice — enter 1, 2, or 3.\n"),
            }
        };
        a
    } else {
        "extract"
    };

    if action == "rebuild" {
        return run_rebuild(&mod_name, &mods_out_dir, &mod_dir_arg, &chosen.path, chosen.is_dir);
    }

    if action == "restore" {
        return run_restore(&mod_name, &mods_out_dir, &chosen.path, chosen.is_dir);
    }

    // ── Extract/copy mod to temp directory ────────────────────────────────
    let temp_base = std::env::temp_dir().join("ukmm-extractool");
    let extract_dir = temp_base.join(&mod_name);
    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir)?;
    }

    if chosen.is_dir {
        println!("  Copying loose mod folder...");
        copy_dir_all(&chosen.path, &extract_dir)?;
    } else {
        println!("  Extracting ZIP...");
        let zip_file = fs::File::open(&chosen.path)?;
        let mut archive = zip::ZipArchive::new(zip_file)?;
        archive.extract(&extract_dir)?;
    }

    // ── Convert each Msg SARC to JSON ─────────────────────────────────────
    println!("\n── Converting mod files to JSON ──\n");

    let msg_files = find_msg_files(&extract_dir);
    let byml_files = find_byml_files(&extract_dir);

    if msg_files.is_empty() && byml_files.is_empty() {
        anyhow::bail!("No message or BYML files found in the mod.");
    }

    for msg_file in &msg_files {
        let sarc_path = msg_file.display().to_string();
        let relative = msg_file.strip_prefix(&extract_dir).unwrap_or(msg_file);
        // Keep subdirectory structure (e.g. "Message/Msg_EUfr.product")
        let output_path = mods_out_dir.join(relative).with_extension("yaml");
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_output(
            &convert_file(&sarc_path)?,
            &output_path,
        )?;
    }

    for byml_file in &byml_files {
        let byml_path = byml_file.display().to_string();
        let relative = byml_file.strip_prefix(&extract_dir).unwrap_or(byml_file);
        let out = convert_file(&byml_path)?;
        let parent_dir = mods_out_dir.join(relative).parent().unwrap_or(&mods_out_dir).to_path_buf();
        fs::create_dir_all(&parent_dir)?;

        // Mergeable → .sbyml (roead format) or .yaml (fallback)
        if out.format.as_deref() == Some("Mergeable") {
            if let Some(b64) = out.entries.get("_sbyml_bytes").and_then(|v| v.as_str()) {
                let sbyml_bytes = base64_decode(b64)?;
                let mut sbyml_name = mods_out_dir.join(relative);
                sbyml_name.set_extension("sbyml");
                if let Some(parent) = sbyml_name.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&sbyml_name, &sbyml_bytes)?;
                eprintln!("  ✓ Converted to native BYML: {}", sbyml_name.display());
            } else {
                // Not in roead serde format — write as YAML.
                let output_path = mods_out_dir.join(relative).with_extension("yaml");
                write_output(&out, &output_path)?;
            }
            continue;
        }

        // ActorInfo → .sbyml natif
        if out.format.as_deref() == Some("ActorInfo") {
            let compressed = actorinfo_output_to_sbyml(&out)?;
            let mut sbyml_name = mods_out_dir.join(relative);
            sbyml_name.set_extension("sbyml");
            fs::write(&sbyml_name, &compressed)?;
            eprintln!("  ✓ Converted to native BYML: {}", sbyml_name.display());
            continue;
        }

        // Autres BYML simples → .sbyml natif
        let compressed = {
            use roead::byml::Byml;
            let val = serde_json::to_value(&out)?;
            // entries has one key = stem, value = { "__byml__": { "attributes": json_str } }
            // We need to extract the actual BYML value and convert it.
            if let Some(entries_map) = val.get("entries").and_then(|v| v.as_object()) {
                let mut found = false;
                let mut result = None;
                for entries_val in entries_map.values() {
                    if let Some(section) = entries_val.as_object() {
                        if let Some(entry) = section.get("__byml__") {
                            if let Some(json_text) = entry.get("attributes").and_then(|a| a.as_str()) {
                                let val: serde_json::Value =
                                    serde_json::from_str(json_text)?;
                                let byml: Byml = serde_json::from_value(val)?;
                                let binary = byml.to_binary(roead::Endian::Big);
                                result = Some(roead::yaz0::compress(&binary));
                                found = true;
                                break;
                            }
                        }
                    }
                }
                if found {
                    result
                } else {
                    // Fallback: just convert via value_to_byml
                    let byml: Byml = serde_json::from_value(
                        serde_json::to_value(&out.entries)?
                    )?;
                    let binary = byml.to_binary(roead::Endian::Big);
                    Some(roead::yaz0::compress(&binary))
                }
            } else {
                None
            }
        };

        if let Some(sbyml_bytes) = compressed {
            let mut sbyml_name = mods_out_dir.join(relative);
            sbyml_name.set_extension("sbyml");
            fs::write(&sbyml_name, &sbyml_bytes)?;
            eprintln!("  ✓ Converted to native BYML: {}", sbyml_name.display());
        } else {
            let output_path = mods_out_dir.join(relative).with_extension("yaml");
            write_output(&out, &output_path)?;
        }
    }

    // ── Save backup ───────────────────────────────────────────────────────
    fs::create_dir_all(&mods_out_dir)?;
    let backup_name = format!("{mod_name}_backup.zip");
    let backup_path = mods_out_dir.join(&backup_name);

    if !chosen.is_dir {
        fs::copy(&chosen.path, &backup_path)?;
        println!("  ✓ Backup saved: {}", backup_path.display());
    } else {
        create_zip_from_dir(&extract_dir, &backup_path)?;
    }

    fs::remove_dir_all(&extract_dir)?;

    // ── Summary ───────────────────────────────────────────────────────────
    let total = msg_files.len() + byml_files.len();
    println!("\n── Summary ──");
    println!("  Platform:     {platform}");
    println!("  Mod:          {}", chosen.display_name);
    println!("  Files:        {total} ({} msg, {} byml)", msg_files.len(), byml_files.len());
    println!("  Output:       {}", mods_out_dir.display());
    println!("  Backup:       {backup_name}");
    println!("\nDone!\n");
    open_explorer(&mods_out_dir);

    Ok(())
}

/// Rebuild a UKMM mod ZIP from edited JSON files.
///
/// Reads all `.json` files from the output directory, converts each back to
/// a CBOR SARC blob via `from_json_to_cbor()`, then injects them into a copy
/// of the backup ZIP. Original `Message/<name>.sarc` entries are replaced;
/// all other ZIP entries are copied as-is. Converted entries use
/// `CompressionMethod::Stored` (no additional compression).
fn run_rebuild(mod_name: &str, mods_out_dir: &Path, _mod_dir_arg: &str, mod_path: &Path, is_dir: bool) -> Result<()> {
    let backup_name = format!("{mod_name}_backup.zip");
    let backup_path = mods_out_dir.join(&backup_name);
    let modified_name = format!("{mod_name}.zip");
    let modified_path = mods_out_dir.join(&modified_name);

    println!("\n── Rebuilding modified ZIP from edited files ──\n");

    // ── Collect edited files recursively from the output directory ────────
    let mut edited_files: Vec<PathBuf> = {
        let mut files = Vec::new();
        collect_edited_files(mods_out_dir, &mut files);
        files
    };

    // Dedup: if both `.sbyml` and `.yaml` exist for the same stem,
    // keep only the `.sbyml` (it's the canonical edited format).
    edited_files.sort_by(|a, b| {
        let a_stem = a.file_stem().and_then(OsStr::to_str).unwrap_or("");
        let b_stem = b.file_stem().and_then(OsStr::to_str).unwrap_or("");
        let a_is_sbyml = a.extension().is_some_and(|x| x == "sbyml");
        let b_is_sbyml = b.extension().is_some_and(|x| x == "sbyml");
        // Sort sbyml before yaml for the same stem, then by path.
        a_stem.cmp(b_stem).then_with(|| b_is_sbyml.cmp(&a_is_sbyml))
    });
    edited_files.dedup_by(|a, b| {
        a.file_stem() == b.file_stem()
    });

    if edited_files.is_empty() {
        anyhow::bail!("No edited files found in {}.", mods_out_dir.display());
    }

    // ── Convert each edited file back to a CBOR SARC or BYML blob ────────
    let mut converted: Vec<(String, Vec<u8>, bool)> = Vec::new(); // (zip_entry_name, data, is_byml)
    for file_path in &edited_files {
        let stem = file_path.file_stem().and_then(OsStr::to_str).unwrap_or("unknown");

        // ── .sbyml (native BYML) ────────────────────────────────────────
        if file_path.extension().is_some_and(|x| x == "sbyml") {
            let raw = fs::read(file_path)?;
            let data = decompress(&raw)?;

                    // Check if this is a mergeable .sbyml (not ActorInfo BYML).
            // Mergeable BYML has a single Map key with serde-tagged values;
            // ActorInfo BYML has "Actors" and "Hashes" arrays.
            let is_mergeable = match roead::byml::Byml::from_binary(&data) {
                Ok(byml) => {
                    if let Ok(map) = byml.as_map() {
                        // If it has Actors/Hashes arrays, it's ActorInfo BYML.
                        let has_actors = map.contains_key("Actors");
                        let has_hashes = map.contains_key("Hashes");
                        !has_actors && !has_hashes
                    } else { true }
                }
                Err(_) => false,
            };

            if is_mergeable {
                let cbor_bytes = sbyml_to_mergeable_cbor(&data, stem)?;
                let byml_name = format!("{stem}.byml");
                println!("  Converting (Mergeable): {} → {byml_name}", file_path.file_name().unwrap_or_default().to_string_lossy());
                converted.push((byml_name, cbor_bytes, true));
            } else {
                // Native ActorInfo BYML (Actors/Hashes format).
                let out = parse_byml_actorinfo(&data, &file_path.to_string_lossy())?;
                let raw_cbor = rebuild_actorinfo_from_output(&out)?;
                let compressed = zstd_compress(&raw_cbor)?;
                let byml_name = format!("{stem}.byml");
                println!("  Converting (ActorInfo): {} → {byml_name}", file_path.file_name().unwrap_or_default().to_string_lossy());
                converted.push((byml_name, compressed, true));
            }
            continue;
        }

        // ── .yaml ───────────────────────────────────────────────────────
        let yaml_text = fs::read_to_string(file_path)?;
        let val: serde_json::Value = serde_yaml::from_str(&yaml_text)
            .with_context(|| format!("Failed to parse {}.", file_path.display()))?;
        let mut out: Output = serde_json::from_value(val)
            .with_context(|| format!("Failed to convert YAML {} to Output.", file_path.display()))?;

        // Auto-detect format from entries structure when stripped from YAML.
        if out.format.is_none() {
            out.format = detect_format(&out.entries).map(String::from);
        }

        if out.format.as_deref() == Some("BYML") {
            let byml_bytes = rebuild_byml_from_output(&out)?;
            let byml_name = format!("{stem}.byml");
            println!("  Converting: {} → {byml_name}", file_path.file_name().unwrap_or_default().to_string_lossy());
            converted.push((byml_name, byml_bytes, true));
        } else if out.format.as_deref() == Some("Mergeable") {
            let cbor_bytes = rebuild_mergeable_from_output(&out)?;
            let byml_name = format!("{stem}.byml");
            println!("  Converting (Mergeable): {} → {byml_name}", file_path.file_name().unwrap_or_default().to_string_lossy());
            converted.push((byml_name, cbor_bytes, true));
        } else if out.format.as_deref() == Some("ActorInfo") {
            let raw_bytes = rebuild_actorinfo_from_output(&out)?;
            let compressed = zstd_compress(&raw_bytes)?;
            let byml_name = format!("{stem}.byml");
            println!("  Converting (ActorInfo): {} → {byml_name}", file_path.file_name().unwrap_or_default().to_string_lossy());
            converted.push((byml_name, compressed, true));
        } else {
            let sarc_name = format!("{stem}.sarc");
            println!("  Converting: {} → {sarc_name}", file_path.file_name().unwrap_or_default().to_string_lossy());
            let sarc_bytes = from_json_to_cbor(&out)?;
            converted.push((sarc_name, sarc_bytes, false));
        }
    }

    if converted.is_empty() {
        anyhow::bail!("No JSON files could be converted.");
    }

    // ── Build modified ZIP ────────────────────────────────────────────────
    // Strategy: copy all entries from the backup ZIP except the ones we're
    // replacing, then append the new entries under the appropriate prefix.
    let backup_file = fs::File::open(&backup_path)?;
    let mut backup_archive = zip::ZipArchive::new(backup_file)?;
    let modified_file = fs::File::create(&modified_path)?;
    let mut modified_zip = zip::ZipWriter::new(modified_file);

    let replace_names: Vec<String> = converted.iter()
        .map(|(name, _, is_byml)| {
            if *is_byml { format!("Actor/{name}") } else { format!("Message/{name}") }
        })
        .collect();

    // Copy all original entries, skipping the ones we're replacing.
    for i in 0..backup_archive.len() {
        let mut entry = backup_archive.by_index(i)?;
        let entry_name = entry.name().to_string();
        if replace_names.contains(&entry_name) {
            continue;         // Replaced below.
        }
        let options = if entry.is_dir() {
            modified_zip.add_directory::<&str, ()>(&entry_name, Default::default())?;
            continue;
        } else {
            zip::write::FileOptions::<()>::default()
                .compression_method(entry.compression())
                .last_modified_time(entry.last_modified().unwrap_or_default())
        };
        modified_zip.start_file::<&str, ()>(&entry_name, options)?;
        io::copy(&mut entry, &mut modified_zip)?;
    }

    // Append the new (or modified) entries. Messages go under `Message/`,
    // BYML files (e.g. actorinfo) go under `Actor/`. Both are stored
    // without compression — they're already zstd-compressed.
    for (entry_name, entry_bytes, _) in &converted {
        let prefix = if entry_name.ends_with(".byml") { "Actor" } else { "Message" };
        let zip_name = format!("{prefix}/{entry_name}");
        modified_zip.start_file::<&str, ()>(&zip_name, zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Stored))?;
        modified_zip.write_all(entry_bytes)?;
        println!("  Added: {zip_name}");
    }

    modified_zip.finish()?;

    println!("\n── Summary ──");
    println!("  Modified ZIP: {}", modified_path.display());
    println!("  Files converted: {}", converted.len());

    // ── Copy modified ZIP back to UKMM mods directory ────────────────────
    if !is_dir {
        fs::copy(&modified_path, mod_path)?;
        println!("  ✓ Copied to UKMM: {}", mod_path.display());
    } else {
        // For loose directories, extract the rebuilt ZIP over the original.
        let temp_extract = mods_out_dir.join("_rebuild_extract");
        if temp_extract.exists() {
            fs::remove_dir_all(&temp_extract)?;
        }
        fs::create_dir_all(&temp_extract)?;
        let zip_file = fs::File::open(&modified_path)?;
        let mut archive = zip::ZipArchive::new(zip_file)?;
        archive.extract(&temp_extract)?;
        // Remove old contents and copy new ones.
        for entry in fs::read_dir(mod_path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
        copy_dir_all(&temp_extract, mod_path)?;
        fs::remove_dir_all(&temp_extract)?;
        println!("  ✓ Extracted to UKMM directory: {}", mod_path.display());
    }

    // ── Remove the intermediate modified .zip from the output directory ──
    if modified_path.exists() {
        fs::remove_file(&modified_path)?;
    }

    println!("\nDone!\n");
    open_explorer(mods_out_dir);

    Ok(())
}

/// Restore the original backup ZIP back to the UKMM mods directory.
///
/// Copies the `_backup.zip` from the workspace back to UKMM (for ZIP mods),
/// or extracts it over the loose directory (for folder mods).
fn run_restore(mod_name: &str, mods_out_dir: &Path, mod_path: &Path, is_dir: bool) -> Result<()> {
    let backup_name = format!("{mod_name}_backup.zip");
    let backup_path = mods_out_dir.join(&backup_name);

    if !backup_path.exists() {
        anyhow::bail!("Backup not found: {}", backup_path.display());
    }

    println!("\n── Restoring original mod from backup ──\n");

    if !is_dir {
        fs::copy(&backup_path, mod_path)?;
        println!("  ✓ Restored: {}", mod_path.display());
    } else {
        let temp_extract = mods_out_dir.join("_restore_extract");
        if temp_extract.exists() {
            fs::remove_dir_all(&temp_extract)?;
        }
        fs::create_dir_all(&temp_extract)?;
        let zip_file = fs::File::open(&backup_path)?;
        let mut archive = zip::ZipArchive::new(zip_file)?;
        archive.extract(&temp_extract)?;
        for entry in fs::read_dir(mod_path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
        copy_dir_all(&temp_extract, mod_path)?;
        fs::remove_dir_all(&temp_extract)?;
        println!("  ✓ Restored to UKMM directory: {}", mod_path.display());
    }

    println!("\nDone!\n");
    Ok(())
}

/// Check whether a ZIP file contains any `Msg_*.product.sarc` files.
///
/// Opens the ZIP and scans entry names without extracting data.
/// Returns `false` for any I/O error (file not found, corrupt ZIP, etc.).
fn peek_zip_has_msg(zip_path: &Path) -> bool {
    let file = match fs::File::open(zip_path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else { return false };
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index_raw(i) else { continue };
        let name = entry.name();
        // Extract just the filename portion (after last / or \).
        if let Some(file_name) = name.split('/').next_back().or_else(|| name.split('\\').next_back()) {
            if file_name.starts_with("Msg_") && file_name.contains(".product.s") && file_name.ends_with("rc") {
                return true;
            }
        }
    }
    false
}

/// Check whether a ZIP file contains any `.byml` / `.sbyml` files.
fn peek_zip_has_byml(zip_path: &Path) -> bool {
    let file = match fs::File::open(zip_path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else { return false };
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index_raw(i) else { continue };
        let name = entry.name();
        if let Some(file_name) = name.split('/').next_back().or_else(|| name.split('\\').next_back()) {
            if file_name.ends_with(".byml") || file_name.ends_with(".sbyml") {
                return true;
            }
        }
    }
    false
}

/// Extract the `name:` field from `meta.yml` inside a ZIP archive.
///
/// Opens the ZIP, reads `meta.yml` by name, and returns the value of the
/// `name:` YAML key. Returns `None` if the file or key is missing.
fn read_zip_meta_name(zip_path: &Path) -> Option<String> {
    let file = fs::File::open(zip_path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let meta = archive.by_name("meta.yml").ok()?;

    let mut content = String::new();
    io::BufReader::with_capacity(4096, meta).read_to_string(&mut content).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if let Some(stripped) = line.strip_prefix("name:") {
            let name = stripped.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Recursively find all `Msg_*.product.sarc` files under a directory.
///
/// Matches files whose name starts with `Msg_`, contains `.product.s`,
/// and ends with `rc`. The middle segment is intentionally loose to match
/// both `.product.sarc` and `.product.ssarc` variations.
fn find_msg_files(dir: &Path) -> Vec<PathBuf> {
    let mut results = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                results.extend(find_msg_files(&path));
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("Msg_") && name.contains(".product.s") && name.ends_with("rc") {
                    results.push(path);
                }
            }
        }
    }
    results
}

/// Recursively find all `.byml` / `.sbyml` files under a directory.
fn find_byml_files(dir: &Path) -> Vec<PathBuf> {
    let mut results = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                results.extend(find_byml_files(&path));
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".byml") || name.ends_with(".sbyml") {
                    results.push(path);
                }
            }
        }
    }
    results
}

/// Data extracted from a BCML `.bnp` archive.
struct BnpData {
    /// Mod display name (from `info.json`).
    name: String,
    /// Target platform: `"wiiu"` or `"nx"`.
    platform: String,
    /// One `Output` per language, keyed by language code (e.g. `"USen"`, `"EUfr"`).
    outputs: BTreeMap<String, Output>,
    /// Raw YAML string from `logs/actorinfo.yml` (if present in the BNP).
    actorinfo_yaml: Option<String>,
    /// `true` if at least one [`FILTER_SECTIONS`] entry was removed during parsing.
    filtered_any: bool,
}

/// Convert a single BCML language block (section.msyt → entries) into our `Output`.
fn bcml_lang_to_output(language: String, sections: BTreeMap<String, serde_json::Value>) -> Output {
    let mut entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();

    for (section_name, entry_map) in sections {
        let clean_name = section_name.strip_suffix(".msyt").unwrap_or(&section_name).to_string();

        if let Some(obj) = entry_map.as_object() {
            let mut im: IndexMap<String, Entry> = IndexMap::new();
            for (key, val) in obj {
                match serde_json::from_value::<Entry>(val.clone()) {
                    Ok(e) => {
                        im.insert(key.clone(), e);
                    }
                    Err(err) => {
                        eprintln!("Warning: skipping entry '{key}' in section '{clean_name}': {err}");
                    }
                }
            }
            if !im.is_empty() {
                entries.insert(clean_name, serde_json::to_value(im).unwrap_or_default());
            }
        }
    }

    Output {
        language: Some(language),
        entry_count: None,
        entries,
        format: None,
    }
}

/// Build a BCML-format `{ lang: { section.msyt: entries } }` map with all
/// languages from a BNP. This exactly reproduces the original `logs/texts.json` structure.
fn build_bcml_texts(outputs: &BTreeMap<String, Output>) -> BTreeMap<String, serde_json::Value> {
    let mut bcml: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for (lang, out) in outputs {
        let mut sections: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        for (section_name, entries) in &out.entries {
            let msyt_name = format!("{section_name}.msyt");
            sections.insert(msyt_name, serde_json::to_value(entries).unwrap_or_default());
        }
        bcml.insert(lang.clone(), serde_json::Value::Object(
            sections.into_iter().collect()
        ));
    }
    bcml
}

/// Parse a BCML `.bnp` archive (a 7z file) into a `BnpData`.
///
/// Extracts `info.json` (for name & platform), `logs/texts.json` (for all
/// language entries), and `logs/actorinfo.yml` (ActorInfo data, if present).
/// Each language in the BCML JSON becomes a separate `Output`.
fn parse_bnp_bytes(data: &[u8]) -> Result<BnpData> {
    let len = data.len() as u64;
    let cursor = Cursor::new(data.to_vec());
    let mut reader =
        SevenZReader::new(cursor, len, Password::default()).context("Failed to open 7z archive")?;

    let mut info_json: Option<Vec<u8>> = None;
    let mut texts_json: Option<Vec<u8>> = None;
    let mut actorinfo_yaml_raw: Option<Vec<u8>> = None;

    reader
        .for_each_entries(|entry, entry_reader| {
            let mut buf = Vec::new();
            let _ = entry_reader.read_to_end(&mut buf);
            match entry.name() {
                "info.json" => info_json = Some(buf),
                "logs/texts.json" => texts_json = Some(buf),
                "logs/actorinfo.yml" => actorinfo_yaml_raw = Some(buf),
                _ => {}
            }
            Ok(true)
        })
        .context("Failed to extract files from BNP archive")?;

    let info: serde_json::Value = serde_json::from_slice(
        &info_json.ok_or_else(|| anyhow::anyhow!("BNP archive missing info.json"))?,
    )
    .context("Failed to parse info.json")?;

    let name = info
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let platform = info
        .get("platform")
        .and_then(|v| v.as_str())
        .unwrap_or("wiiu")
        .to_string();

    // Parse actorinfo.yml if present.
    let actorinfo_yaml = actorinfo_yaml_raw
        .map(|b| String::from_utf8_lossy(&b).to_string());

    // Parse BCML texts.json.
    let json_bytes: &[u8] = &texts_json.ok_or_else(|| anyhow::anyhow!("BNP archive missing logs/texts.json"))?;
    let bcml: BTreeMap<String, BTreeMap<String, serde_json::Value>> =
        serde_json::from_slice(json_bytes).context("Failed to parse BCML texts.json")?;

    let mut outputs: BTreeMap<String, Output> = BTreeMap::new();
    let mut filtered_any = false;
    for (language, sections) in bcml {
        let mut out = bcml_lang_to_output(language.clone(), sections);
        // The two bug sections always come together — check just the first.
        if out.entries.remove(FILTER_SECTIONS[0]).is_some() {
            out.entries.remove(FILTER_SECTIONS[1]);
            filtered_any = true;
        }
        outputs.insert(language, out);
    }

    Ok(BnpData { name, platform, outputs, actorinfo_yaml, filtered_any })
}

/// Parse raw BYML bytes into a `serde_json::Value`.
fn byml_to_value(data: &[u8]) -> Result<serde_json::Value> {
    use roead::byml::Byml;
    let byml = Byml::from_binary(data).context("Failed to parse BYML data")?;
    serde_json::to_value(&byml).context("Failed to serialize BYML to JSON")
}

/// Convert a `serde_json::Value` back to BYML binary bytes (big-endian, Wii U format).
fn value_to_byml(val: &serde_json::Value) -> Result<Vec<u8>> {
    use roead::byml::Byml;
    let byml: Byml =
        serde_json::from_value(val.clone()).context("Failed to deserialize JSON to BYML")?;
    Ok(byml.to_binary(roead::Endian::Big))
}

/// Parse a decompressed BYML file and wrap it in an `Output`.
///
/// The BYML content is stored as raw JSON under a pseudo-section named after
/// the file stem, so it can round-trip through the existing workspace/rebuild
/// machinery.
fn parse_byml_file_output(data: &[u8], path: &str) -> Result<Output> {
    let val = byml_to_value(data)?;
    let stem = filename_stem(Path::new(path));
    let json_text = serde_json::to_string_pretty(&val)?;

    let mut entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut im: IndexMap<String, Entry> = IndexMap::new();
    im.insert(
        "__byml__".to_string(),
        Entry {
            attributes: Some(json_text),
            contents: vec![],
        },
    );
    entries.insert(stem, serde_json::to_value(im)?);

    Ok(Output {
        language: None,
        entry_count: None,
        entries,
        format: Some("BYML".into()),
    })
}

/// Rebuild a BYML binary from an edited JSON that was produced by
/// `parse_byml_file_output`. Returns the raw `.byml` bytes.
fn rebuild_byml_from_output(out: &Output) -> Result<Vec<u8>> {
    // Find the first section that has a __byml__ entry.
    for entries_val in out.entries.values() {
        if let Some(section) = entries_val.as_object() {
            if let Some(entry) = section.get("__byml__") {
                if let Some(json_text) = entry.get("attributes").and_then(|a| a.as_str()) {
                    let val: serde_json::Value =
                        serde_json::from_str(json_text).context("Failed to parse BYML JSON content")?;
                    return value_to_byml(&val);
                }
            }
        }
    }
    anyhow::bail!("No BYML data found in output");
}

/// Convert an ActorInfo Output (unfolded format) to standard Nintendo BYML
/// and yaz0-compress it into `.sbyml` bytes.
///
/// This takes the CBOR Mergeable ActorInfo format and produces native BYML
/// with `Actors` (array of actor BYML maps) and `Hashes` (array of u32).
/// The result is yaz0-compressed and ready to write as `.sbyml`.
fn actorinfo_output_to_sbyml(out: &Output) -> Result<Vec<u8>> {
    use roead::byml::Byml;

    let mut actors = Vec::new();
    let mut hashes = Vec::new();

    for entries_val in out.entries.values() {
        if let Some(section) = entries_val.as_object() {
            for (hash_str, actor_entry) in section {
                if hash_str == "__byml__" { continue; }
                // Each entry: [ { "Map": { "name": {"String":"..."}, ... } }, false ]
                let arr = actor_entry.as_array()
                    .context("Actor entry should be an array")?;
                if arr.is_empty() { continue; }

                // The actor entry is [ { "Map": { ... } }, false ].
                // arr[0] is already { "Map": { ... } } which is the correct roead
                // serde JSON format for a Byml Map. Deserialize it directly.
                let actor_data = serde_json::from_value::<Byml>(arr[0].clone())
                    .context("Failed to convert actor data to BYML")?;

                // Verify it has a "name" field
                let has_name = actor_data.as_map()
                    .ok()
                    .and_then(|m| m.get("name"))
                    .and_then(|v| v.as_string().ok())
                    .is_some();
                if !has_name {
                    eprintln!("  Skipping actor {hash_str} (no name field, partial diff entry)");
                    continue;
                }

                actors.push(actor_data);

                // Parse hash for the Hashes array
                let hash: u32 = hash_str.parse()
                    .with_context(|| format!("Invalid actor hash: {hash_str}"))?;
                hashes.push(if hash > i32::MAX as u32 {
                    Byml::U32(hash)
                } else {
                    Byml::I32(hash as i32)
                });
            }
        }
    }

    if actors.is_empty() {
        anyhow::bail!("No actors found in output");
    }

    // Build BYML directly via roead types, not via JSON serde.
    // Use the serde JSON approach which is proven to work in value_to_byml().
    // Wrap in roead's serde format: { "Map": { "Actors": [...], "Hashes": [...] } }
    let actors_val: Vec<serde_json::Value> = actors.into_iter()
        .filter_map(|b| serde_json::to_value(&b).ok())
        .collect();
    let hashes_val: Vec<serde_json::Value> = hashes.into_iter()
        .filter_map(|b| serde_json::to_value(&b).ok())
        .collect();

    let root_val = serde_json::json!({
        "Map": {
            "Actors": { "Array": actors_val },
            "Hashes": { "Array": hashes_val },
        }
    });

    let byml: Byml = serde_json::from_value(root_val)
        .context("Failed to convert JSON to BYML")?;
    let binary = byml.to_binary(roead::Endian::Big);
    // Yaz0 compress → .sbyml
    let compressed = roead::yaz0::compress(&binary);
    Ok(compressed)
}

// ─────────────────────────────────────────────────────────────────────────────
// ActorInfo CBOR (Mergeable / ActorInfo) support
// ─────────────────────────────────────────────────────────────────────────────

/// Heuristic: does the byte buffer look like a UKMM "Mergeable" CBOR?
///
/// Checks the first ~11 bytes for the structure:
/// `map(1){ "Mergeable": map(1){ ... } }`
fn looks_like_mergeable_cbor(d: &[u8]) -> bool {
    d.len() > 13
        && d[0] == 0xA1                                          // map(1)
        && d[1] == 0x69                                          // text(9)
        && &d[2..11] == b"Mergeable"
        && d[11] == 0xA1                                         // inner map(1)
}

/// Heuristic: does the byte buffer look like a UKMM "Mergeable" / "ActorInfo" CBOR?
///
/// Checks the first ~22 bytes for the structure:
/// `map(1){ "Mergeable": map(1){ "ActorInfo": ... } }`
fn looks_like_actorinfo_cbor(d: &[u8]) -> bool {
    d.len() > 22
        && d[0] == 0xA1                                          // map(1)
        && d[1] == 0x69                                          // text(9)
        && &d[2..11] == b"Mergeable"
        && d[11] == 0xA1                                         // map(1)
        && d[12] == 0x69                                         // text(9)
        && &d[13..22] == b"ActorInfo"
}

/// Heuristic: does the byte buffer look like a UKMM Mergeable / MessagePack CBOR?
///
/// Checks the first ~24 bytes for the structure:
/// `map(1){ "Mergeable": map(1){ "MessagePack": ... } }`
fn looks_like_messagepack_cbor(d: &[u8]) -> bool {
    d.len() > 24
        && d[0] == 0xA1                                          // map(1)
        && d[1] == 0x69                                          // text(9)
        && &d[2..11] == b"Mergeable"
        && d[11] == 0xA1                                         // inner map(1)
        && d[12] == 0x6B                                         // text(11)
        && &d[13..24] == b"MessagePack"
}

/// Recursively decode a CBOR byte buffer into a `serde_json::Value`.
///
/// Handles all major types needed for UKMM Mergeable structures:
/// uint, nint, bytes (→ base64 JSON string), text, array, map, tag, float.
fn cbor_to_json(data: &[u8], offset: &mut usize) -> Result<serde_json::Value> {
    if *offset >= data.len() {
        anyhow::bail!("Unexpected end of CBOR data at offset {offset}");
    }
    let b = data[*offset];
    let mt = (b >> 5) & 0x07;
    let ai = (b & 0x1f) as usize;
    *offset += 1;

    let (value, _adv) = match ai {
        0..=23 => (ai as u64, 0),
        24 if *offset < data.len() => { let v = data[*offset] as u64; *offset += 1; (v, 0) }
        25 if *offset + 2 <= data.len() => {
            let v = u16::from_be_bytes([data[*offset], data[*offset+1]]) as u64;
            *offset += 2; (v, 0)
        }
        26 if *offset + 4 <= data.len() => {
            let v = u32::from_be_bytes([data[*offset], data[*offset+1], data[*offset+2], data[*offset+3]]) as u64;
            *offset += 4; (v, 0)
        }
        27 if *offset + 8 <= data.len() => {
            let v = u64::from_be_bytes([
                data[*offset], data[*offset+1], data[*offset+2], data[*offset+3],
                data[*offset+4], data[*offset+5], data[*offset+6], data[*offset+7],
            ]);
            *offset += 8; (v, 0)
        }
        28..=31 => anyhow::bail!("Unsupported CBOR additional info {ai} at offset {}", *offset - 1),
        _ => anyhow::bail!("Invalid CBOR additional info {ai}"),
    };

    match mt {
        0 => Ok(serde_json::Value::Number(value.into())),          // uint
        1 => Ok(serde_json::Value::Number(                         // nint
            serde_json::Number::from(-(value as i64) - 1)
        )),
        2 => {                                                     // bytes → base64 string
            let end = offset.saturating_add(value as usize);
            if end > data.len() {
                anyhow::bail!("CBOR byte string out of bounds");
            }
            let s = data[*offset..end].to_vec();
            *offset = end;
            Ok(serde_json::Value::String(base64_encode(&s)))
        }
        3 => {                                                     // text
            let end = offset.saturating_add(value as usize);
            if end > data.len() {
                anyhow::bail!("CBOR text string out of bounds at offset {} len {value}", *offset);
            }
            let slice = &data[*offset..end];
            let s = match std::str::from_utf8(slice) {
                Ok(s) => s.to_string(),
                Err(e) => {
                    let pos = *offset;
                    anyhow::bail!("CBOR text string is not valid UTF-8 at offset {pos}: {e}");
                }
            };
            *offset = end;
            Ok(serde_json::Value::String(s))
        }
        4 => {                                                     // array
            let mut arr = Vec::with_capacity(value as usize);
            for _ in 0..value {
                arr.push(cbor_to_json(data, offset)?);
            }
            Ok(serde_json::Value::Array(arr))
        }
        5 => {                                                     // map
            let mut map = serde_json::Map::with_capacity(value as usize);
            for _ in 0..value {
                let k = cbor_to_json(data, offset)?;
                let v = cbor_to_json(data, offset)?;
                // Key must be a string for JSON objects
                let key = match &k {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                map.insert(key, v);
            }
            Ok(serde_json::Value::Object(map))
        }
        6 => {                                                     // tag: skip tag, decode next
            cbor_to_json(data, offset)
        }
        7 => {                                                     // float / simple
            // NOTE: the float data bytes have ALREADY been consumed by the initial
            // AI/value parsing above (ai 25/26/27 read 2/4/8 bytes and advance *offset).
            // We must NOT read them again — use `value` instead.
            let float_val: f64 = match ai {
                // Simple values (false=20, true=21, null=22, undefined=23)
                20 => return Ok(serde_json::Value::Bool(false)),
                21 => return Ok(serde_json::Value::Bool(true)),
                22 | 23 => return Ok(serde_json::Value::Null),
                // 1-byte simple (AI 24) — value is the simple type, used for
                // extended simple values (e.g. 0xF8 + byte). We just return null.
                24 => return Ok(serde_json::Value::Null),
                // f16: value was decoded as u16 from the initial parsing
                25 => f16_to_f64(value as u16),
                // f32: value was decoded as u32 from the initial parsing
                26 => f32::from_bits(value as u32) as f64,
                // f64: value was decoded as u64 from the initial parsing
                27 => f64::from_bits(value),
                _ => return Ok(serde_json::Value::Null),
            };
            serde_json::Number::from_f64(float_val)
                .map(serde_json::Value::Number)
                .ok_or_else(|| anyhow::anyhow!("CBOR float value out of range"))
        }
        _ => anyhow::bail!("Unknown CBOR major type {mt}"),
    }
}

/// Convert an f16 (half-precision) bit pattern to f64.
fn f16_to_f64(bits: u16) -> f64 {
    let sign = ((bits >> 15) as f64) * -2.0 + 1.0;
    let exp = (bits >> 10) & 0x1f;
    let mant = bits & 0x3ff;
    match exp {
        0 => sign * (mant as f64) / 1024.0 / 16384.0,             // subnormal
        31 => f64::NAN,                                            // inf/nan → NAN
        _ => sign * (1.0 + (mant as f64) / 1024.0) * 2.0f64.powi((exp as i32) - 15),
    }
}

/// Minimal base64 encode (RFC 4648) for byte strings.
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        out.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else { out.push('='); }
        if chunk.len() > 2 {
            out.push(CHARS[(triple & 0x3F) as usize] as char);
        } else { out.push('='); }
    }
    out
}

/// Minimal base64 decode (RFC 4648).
fn base64_decode(input: &str) -> Result<Vec<u8>> {
    fn decode_char(c: u8) -> Result<u8> {
        match c {
            b'A'..=b'Z' => Ok(c - b'A'),
            b'a'..=b'z' => Ok(c - b'a' + 26),
            b'0'..=b'9' => Ok(c - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            b'=' => Ok(0),
            _ => anyhow::bail!("Invalid base64 character: {c:#02x}"),
        }
    }

    let input = input.trim_end_matches('=');
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let bytes = input.as_bytes();
    for chunk in bytes.chunks(4) {
        let mut buf = [0u8; 4];
        for (i, &b) in chunk.iter().enumerate() {
            buf[i] = decode_char(b)?;
        }
        let triple = ((buf[0] as u32) << 18)
            | ((buf[1] as u32) << 12)
            | ((buf[2] as u32) << 6)
            | (buf[3] as u32);
        out.push((triple >> 16) as u8);
        if chunk.len() > 2 {
            out.push((triple >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(triple as u8);
        }
    }
    Ok(out)
}

/// Recursively encode a `serde_json::Value` as CBOR bytes.
fn json_to_cbor(val: &serde_json::Value, buf: &mut Vec<u8>) {
    match val {
        serde_json::Value::Null => buf.push(0xF6),
        serde_json::Value::Bool(b) => buf.push(if *b { 0xF5 } else { 0xF4 }),
        serde_json::Value::Number(n) => {
            if let Some(v) = n.as_u64() {
                cbor_write_uint(buf, 0, v);                       // major type 0
            } else if let Some(v) = n.as_i64() {
                if v >= 0 {
                    cbor_write_uint(buf, 0, v as u64);
                } else {
                    cbor_write_uint(buf, 1, (-1 - v) as u64);      // major type 1
                }
            } else if let Some(v) = n.as_f64() {
                // UKMM CBOR format uses f32 for all float values.
                // Using f64 would cause "unexpected type f64 at position …: expected f32".
                buf.push(0xFA);                                    // f32
                buf.extend_from_slice(&(v as f32).to_bits().to_be_bytes());
            }
        }
        serde_json::Value::String(s) => cbor_write_text(buf, s),
        serde_json::Value::Array(arr) => {
            cbor_write_uint(buf, 4, arr.len() as u64);            // major type 4
            for item in arr {
                json_to_cbor(item, buf);
            }
        }
        serde_json::Value::Object(map) => {
            cbor_write_uint(buf, 5, map.len() as u64);            // major type 5
            for (k, v) in map {
                cbor_write_text(buf, k);
                json_to_cbor(v, buf);
            }
        }
    }
}

/// Write a CBOR uint/nint/array/map header with the given major type and value.
fn cbor_write_uint(buf: &mut Vec<u8>, major_type: u8, value: u64) {
    let mt = major_type << 5;
    match value {
        0..=23 => buf.push(mt | value as u8),
        24..=0xFF => { buf.push(mt | 24); buf.push(value as u8); }
        0x100..=0xFFFF => { buf.push(mt | 25); buf.extend_from_slice(&(value as u16).to_be_bytes()); }
        0x10000..=0xFFFF_FFFF => { buf.push(mt | 26); buf.extend_from_slice(&(value as u32).to_be_bytes()); }
        _ => { buf.push(mt | 27); buf.extend_from_slice(&value.to_be_bytes()); }
    }
}

/// Parse a UKMM "Mergeable" / "ActorInfo" CBOR blob into an `Output`.
///
/// The CBOR structure is:
/// ```cbor
/// { "Mergeable": { "ActorInfo": { <hash>: [ { ...byml_node... } ] } } }
/// ```
///
/// The entire decoded content is stored as a JSON string under a `__byml__`
/// pseudo-section (same pattern as BYML), allowing round-trip through the
/// workspace machinery.
fn parse_actorinfo_cbor(data: &[u8], path: &str) -> Result<Output> {
    let val = cbor_to_json(data, &mut 0)?;
    let stem = filename_stem(Path::new(path));

    // Extract the inner actor info map: { "Mergeable": { "ActorInfo": { <hash>: [...], ... } } }
    // and unfold it: each hash becomes a top-level entry in the section.
    let actor_map = val.pointer("/Mergeable/ActorInfo")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let mut entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    entries.insert(stem, serde_json::Value::Object(actor_map));

    Ok(Output {
        language: None,
        entry_count: None,
        entries,
        format: Some("ActorInfo".into()),
    })
}

/// Rebuild an ActorInfo CBOR binary from an edited JSON.
///
/// The JSON should have the unfolded format produced by `parse_actorinfo_cbor`:
/// `{ "entries": { "<stem>": { "<hash>": [actor_data, deleted], ... } } }`
///
/// Returns the raw CBOR bytes wrapped in `Mergeable/ActorInfo`, ready for zstd compression.
fn rebuild_actorinfo_from_output(out: &Output) -> Result<Vec<u8>> {
    for entries_val in out.entries.values() {
        if let Some(section) = entries_val.as_object() {
            let is_unfolded = section.keys().any(|k| {
                k.parse::<u64>().is_ok() || k.starts_with("U32")
            });

            if is_unfolded {
                // Unfolded format: each key is a hash, value is [actor_data, deleted]
                let mut actor_map = section.clone();
                actor_map.remove("__byml__");
                if !actor_map.is_empty() {
                    // Write CBOR directly with u32 integer keys for hashes.
                    // json_to_cbor would encode all object keys as text strings,
                    // but UKMM expects the hash keys as CBOR unsigned integers.
                    let mut buf = Vec::with_capacity(65536);
                    // { "Mergeable": { "ActorInfo": { u32_hash: value, ... } } }
                    cbor_write_uint(&mut buf, 5, 1);              // outer map: 1 entry
                    cbor_write_text(&mut buf, "Mergeable");
                    cbor_write_uint(&mut buf, 5, 1);              // inner map: 1 entry
                    cbor_write_text(&mut buf, "ActorInfo");
                    cbor_write_uint(&mut buf, 5, actor_map.len() as u64);
                    for (hash_str, value) in &actor_map {
                        let hash: u64 = hash_str.parse()
                            .with_context(|| format!("Invalid ActorInfo hash key: {hash_str}"))?;
                        cbor_write_uint(&mut buf, 0, hash);       // u32 key
                        json_to_cbor(value, &mut buf);            // value via standard CBOR
                    }
                    return Ok(buf);
                }
            }

            // Fallback: old __byml__ format with attributes JSON string
            if let Some(byyml) = section.get("__byml__") {
                if let Some(json_text) = byyml.get("attributes").and_then(|a| a.as_str()) {
                    let val: serde_json::Value =
                        serde_json::from_str(json_text).context("Failed to parse ActorInfo JSON")?;
                    let mut buf = Vec::with_capacity(65536);
                    json_to_cbor(&val, &mut buf);
                    return Ok(buf);
                }
            }
        }
    }
    anyhow::bail!("No ActorInfo data found in output");
}

/// Convert native BYML `.sbyml` back to zstd-compressed CBOR for injection
/// into the UKMM archive. The DataType (e.g. "GenericByml", "EventInfo") is
/// derived from the filename stem.
///
/// Inverse of [`parse_mergeable_cbor`].
fn sbyml_to_mergeable_cbor(byml_data: &[u8], default_type: &str) -> Result<Vec<u8>> {
    use roead::byml::Byml;

    let byml = Byml::from_binary(byml_data)
        .context("Failed to parse mergeable BYML")?;
    let val: serde_json::Value = serde_json::to_value(&byml)
        .context("Failed to serialize mergeable BYML to JSON")?;

    // Re-wrap: { "Mergeable": { "<DataType>": { ... } } }
    // DataType derived from the filename stem (e.g. EffectInfo, EventInfo.product).
    let outer = serde_json::json!({
        "Mergeable": {
            default_type: val
        }
    });

    let mut buf = Vec::with_capacity(65536);
    json_to_cbor(&outer, &mut buf);
    zstd_compress(&buf)
}

/// Rebuild a generic Mergeable CBOR binary from an edited file.
///
/// Two cases:
/// - `.sbyml` files are rebuilt via [`sbyml_to_mergeable_cbor`] (handled in the
///   `.sbyml` block of `run_rebuild`).
/// - `.yaml` files contain raw JSON values decoded from CBOR (not in roead
///   serde format) — encode them back to CBOR and zstd-compress.
fn rebuild_mergeable_from_output(out: &Output) -> Result<Vec<u8>> {
    for entries_val in out.entries.values() {
        if !entries_val.is_null() {
            // Skip the _sbyml_bytes sentinel if present (handled by sbyml path).
            if entries_val.as_str().is_some()
                && out.entries.contains_key("_sbyml_bytes")
            {
                continue;
            }
            let mut buf = Vec::with_capacity(65536);
            json_to_cbor(entries_val, &mut buf);
            return zstd_compress(&buf);
        }
    }
    anyhow::bail!("No Mergeable data found in output");
}

/// Heuristic: does the decompressed BYML data look like native ActorInfo
/// (i.e., contains "Actors" and "Hashes" arrays)?
fn looks_like_actorinfo_byml(data: &[u8]) -> bool {
    // Try to parse just enough BYML to check for Actors key.
    // We use roead to do a full parse and check the structure.
    match roead::byml::Byml::from_binary(data) {
        Ok(byml) => {
            if let Ok(map) = byml.as_map() {
                let has_actors = map.get("Actors").and_then(|v| v.as_array().ok()).is_some();
                let has_hashes = map.get("Hashes").and_then(|v| v.as_array().ok()).is_some();
                has_actors && has_hashes
            } else { false }
        }
        Err(_) => false,
    }
}

/// Parse a generic UKMM Mergeable CBOR blob into an `Output`.
///
/// These have the structure:
/// ```cbor
/// { "Mergeable": "<DataType>": { ... actual data ... } }
/// ```
///
/// The CBOR is decoded to a JSON value. The data payload is extracted and
/// converted to native BYML (yaz0-compressed `.sbyml`), with the DataType
/// For round-trip fidelity, the DataType is derived from the filename stem.
///
/// The `.sbyml` bytes are returned via `format = "Mergeable"` and a sentinel
/// entry `"_sbyml_bytes"`, so the caller can write them directly.
fn parse_mergeable_cbor(data: &[u8], _path: &str) -> Result<Output> {

    let val = cbor_to_json(data, &mut 0)?;

    // val = { "Mergeable": { "<DataType>": { ... data ... } } }
    let inner = val.get("Mergeable")
        .and_then(|v| v.as_object())
        .and_then(|m| m.iter().next())
        .map(|(k, v)| (k.clone(), v.clone()))
        .context("Mergeable CBOR: expected { \"Mergeable\": { \"<DataType>\": ... } }")?;

    let (_data_type, raw_payload) = inner;

    // Check if the payload is in roead's with-serde format (a single-key object
    // where the key is "Map", "Array", "String", "Bool", "F32", "F64", "I32",
    // "U32"). If not, fall back to YAML output (the payload isn't representable
    // as native BYML).
    let is_roead_format = matches!(&raw_payload,
        serde_json::Value::Object(m) if m.len() == 1 &&
            m.contains_key("Map") || m.contains_key("Array") ||
            m.contains_key("String") || m.contains_key("Bool") ||
            m.contains_key("F32") || m.contains_key("F64") ||
            m.contains_key("I32") || m.contains_key("U32")
    );

    if !is_roead_format {
        // Not roead format — store as raw JSON in entries for YAML output.
        let stem = filename_stem(Path::new(_path));
        let mut entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        entries.insert(stem, val);
        return Ok(Output {
            language: None,
            entry_count: None,
            entries,
            format: Some("Mergeable".into()),
        });
    }

    // CBOR supports null but BYML doesn't. Replace null → false.
    fn replace_null(val: &mut serde_json::Value) {
        match val {
            serde_json::Value::Null => *val = serde_json::Value::Bool(false),
            serde_json::Value::Object(m) => m.values_mut().for_each(replace_null),
            serde_json::Value::Array(a) => a.iter_mut().for_each(replace_null),
            _ => {}
        }
    }
    let mut payload = raw_payload;
    replace_null(&mut payload);



    // Convert payload to native BYML bytes and yaz0-compress.
    let payload_str = serde_json::to_string(&payload).unwrap_or_default();
    let byml: roead::byml::Byml = serde_json::from_value(payload)
        .with_context(|| {
            format!("Failed to deserialize mergeable payload to BYML: {}...",
                &payload_str[..payload_str.len().min(200)])
        })?;
    let binary = byml.to_binary(roead::Endian::Big);
    let compressed = roead::yaz0::compress(&binary);

    let mut entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    entries.insert("_sbyml_bytes".to_string(), serde_json::json!(base64_encode(&compressed)));

    Ok(Output {
        language: None,
        entry_count: None,
        entries,
        format: Some("Mergeable".into()),
    })
}

/// Parse a native BYML ActorInfo file (with "Actors" / "Hashes" arrays) into
/// the same unfolded output format as `parse_actorinfo_cbor`.
///
/// The BYML structure is:
/// ```byml
/// Actors:
///   - { name: "Weapon_Sword_026", attackPower: 80, ... }
///   - { name: "Enemy_Bokoblin_Gold", ... }
/// Hashes: [1022151304, 2692761260, ...]
/// ```
///
/// Output format:
/// ```json
/// { "entries": { "ActorInfo.product": { "<hash>": [{ "Map": { ... } }, false], ... } } }
/// ```
fn parse_byml_actorinfo(data: &[u8], path: &str) -> Result<Output> {
    use roead::byml::Byml;
    let byml = Byml::from_binary(data).context("Failed to parse ActorInfo BYML")?;
    let map = byml.as_map().context("ActorInfo BYML root is not a map")?;

    let actors = map.get("Actors")
        .and_then(|v| v.as_array().ok())
        .context("ActorInfo BYML missing 'Actors' array")?;

    let stem = filename_stem(Path::new(path));
    let mut actor_map = serde_json::Map::new();

    for actor_byml in actors {
        let actor_map_byml = actor_byml.as_map()
            .map_err(|_| anyhow::anyhow!("Actor entry is not a map"))?;
        let name = actor_map_byml.get("name")
            .and_then(|v| v.as_string().ok())
            .context("Actor entry missing 'name' string")?;

        // Compute the hash using roead's aamp hash (same as UKMM)
        let hash = roead::aamp::hash_name(name);

        // serde_json::to_value() for a Byml already produces the roead serde format:
        // { "Map": { "name": { "String": "..." }, ... } }
        // This matches the format from the CBOR path (arr[0] = { "Map": { ... } }).
        let actor_json = serde_json::to_value(actor_byml)
            .context("Failed to serialize actor entry")?;

        // Wrap in the same format as CBOR ActorInfo: [ { "Map": { ... } }, false ]
        let wrapped = serde_json::Value::Array(vec![
            actor_json,
            serde_json::Value::Bool(false),  // not deleted
        ]);

        actor_map.insert(hash.to_string(), wrapped);
    }

    let mut entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    entries.insert(stem, serde_json::Value::Object(actor_map));

    Ok(Output {
        language: None,
        entry_count: None,
        entries,
        format: Some("ActorInfo".into()),
    })
}

/// Parse an ActorInfo YAML string (TotkBits/Bubble-Wrap BNP export format) into
/// an `Output` with the unfolded structure.
///
/// The YAML format has actor hashes as top-level keys, each containing a flat map
/// of properties (no "Map"/"Array" serde wrappers). We convert each property value
/// to the roead serde JSON format.
///
/// Example input:
/// ```yaml
/// '87080573':
///   name: Weapon_Lsword_700
///   attackPower: 80
///   generalLife: 99
///   tags: { tag2755f107: 659943687, tagb0c9e79a: !u 0xb0c9e79a }
/// ```
/// Read, decompress, and parse a single message file into an `Output` struct.
///
/// This is the same pipeline as `main()` uses for forward conversion,
/// extracted as a reusable function for the interactive mode.
///
/// After parsing, any section whose name appears in [`FILTER_SECTIONS`] is
/// automatically removed from the output and a warning is printed to stderr.
fn convert_file(path: &str) -> Result<Output> {
    let raw = fs::read(path)?;

    let data = decompress(&raw)?;

    // ── BYML detection (raw Nintendo BYML) ────────────────────────────────
    if looks_like_byml(&data) {
        // Check if this is ActorInfo BYML (has "Actors" and "Hashes" arrays)
        let stem = filename_stem(Path::new(path));
        if stem == "ActorInfo.product" && looks_like_actorinfo_byml(&data) {
            eprintln!("actorinfo (byml)...");
            return parse_byml_actorinfo(&data, path);
        }
        return parse_byml_file_output(&data, path);
    }

    // ── UKMM Mergeable CBOR detection ────────────────────────────────────
    if looks_like_actorinfo_cbor(&data) {
        eprintln!("actorinfo...");
        return parse_actorinfo_cbor(&data, path);
    }
    // MessagePack must be checked BEFORE generic mergeable — it needs the
    // old Msyt-deserializing path (parse_cbor) that extracts proper Entry
    // structures from the JSON strings inside the CBOR, not raw JSON dumps.
    if looks_like_messagepack_cbor(&data) {
        eprintln!("messagepack...");
        return parse_cbor(&data);
    }
    if looks_like_mergeable_cbor(&data) {
        eprintln!("mergeable...");
        return parse_mergeable_cbor(&data, path);
    }

    // Try SARC / CBOR, catching errors so BYML can be a last-resort fallback.
    let mut out_result = {
        if is_sarc(&data) {
            parse_sarc(&data)
        } else if looks_like_cbor(&data) {
            parse_cbor(&data).or_else(|e| {
                eprintln!("Warning: CBOR parse failed ({e}), trying SARC...");
                parse_sarc(&data)
            })
        } else {
            parse_sarc(&data)
        }
    };

    // If SARC/CBOR failed or produced nothing, try BYML as a last resort.
    // Check both magic bytes AND file extension for robustness.
    let should_try_byml = out_result.as_ref().map_or(true, |o| o.entries.is_empty())
        && (looks_like_byml(&data) || path.ends_with(".byml") || path.ends_with(".sbyml"));
    if should_try_byml {
        eprintln!("byml (fallback)...");
        out_result = parse_byml_file_output(&data, path).or_else(|e| {
            eprintln!("Warning: BYML parse failed ({e}) — skipping file");
            // Return empty output so the caller sees a valid (but empty) result.
            Ok(Output {
                language: None,
                entry_count: None,
                entries: BTreeMap::new(),
                format: None,
            })
        });
    }

    let mut out = out_result?;

    // ── Strip contaminated sections ────────────────────────────────────────
    for section in FILTER_SECTIONS {
        if out.entries.remove(*section).is_some() {
            eprintln!("  ✓ Removed contaminated section '{section}'");
        }
    }

    Ok(out)
}

/// Full interactive workflow for a .bnp file: extract → backup → rebuild.
///
/// 1. Ask for the .bnp path
/// 2. Parse `info.json` for mod name + platform
/// 3. Parse all languages from `logs/texts.json`
/// 4. Write each language as `Msg_<lang>.product.json` under `mods/<platform>/<mod_name>/`
/// 5. Save a backup of the .bnp
/// 6. If a workspace already exists, offer rebuild / extract-again / restore
fn handle_bnp_interactive() -> Result<()> {
    let bnp_path = prompt("Drag & drop or enter path to .bnp or .7z file: ");
    handle_bnp_interactive_for(bnp_path.trim_matches('"'))
}

/// Same as [`handle_bnp_interactive`] but takes the path directly (no prompt).
fn handle_bnp_interactive_for(bnp_path: &str) -> Result<()> {
    let path = Path::new(bnp_path);
    if !path.exists() {
        anyhow::bail!("File not found: {}", path.display());
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext != "bnp" && ext != "7z" {
        anyhow::bail!("Expected a .bnp or .7z file, got .{ext}");
    }

    let raw = fs::read(bnp_path)?;
    if raw.len() <= BNP_MAGIC.len() || raw[..BNP_MAGIC.len()] != *BNP_MAGIC {
        anyhow::bail!("File does not appear to be a valid 7z archive (missing 7z magic)");
    }

    let bnp = parse_bnp_bytes(&raw)?;

    if bnp.filtered_any {
        println!("  ✓ Removed BCML's entry bugs 'EventFlowMsg/MiniGame_Crosscountry' \
    & 'EventFlowMsg/MiniGame_HorsebackArchery'");
    }

    let mod_name = sanitize_filename(&bnp.name);
    let platform = &bnp.platform;
    let mods_out_dir = PathBuf::from("mods").join(platform).join(&mod_name);
    let logs_dir = mods_out_dir.join("logs");
    let backup_name = format!("{mod_name}_backup.bnp");
    let backup_path = mods_out_dir.join(&backup_name);

    // ── Check for existing workspace ───────────────────────────────────────
    let workspace_exists = backup_path.is_file()
        && logs_dir.join("texts.json").is_file();

    let action = if workspace_exists {
        let a = loop {
            let c = prompt("\nA workspace exists for this mod. What to do with it?\n[1] Send edited files into BNP\n[2] Extract again (BNP > YAML)\n[3] Restore original (from backup)\n\nSelect 1, 2, or 3: ");
            match c.trim() {
                "1" => break "rebuild",
                "2" => break "extract",
                "3" => break "restore",
                _ => eprintln!("Invalid choice — enter 1, 2, or 3.\n"),
            }
        };
        a
    } else {
        "extract"
    };

    if action == "rebuild" {
        let r = run_bnp_rebuild(&mod_name, &mods_out_dir, bnp_path, &backup_path, path);
        open_explorer(&mods_out_dir);
        return r;
    }

    if action == "restore" {
        let r = run_bnp_restore(&backup_path, bnp_path, path);
        open_explorer(&mods_out_dir);
        return r;
    }

    // ── Extract all languages ────────────────────────────────────────────
    println!("bnp mod activated\n\nLanguages: {}\n", bnp.outputs.keys().cloned().collect::<Vec<_>>().join(", "));
    println!("── Converting BNP ──\n");

    let all_langs: Vec<String> = bnp.outputs.keys().cloned().collect();
    let bcml = build_bcml_texts(&bnp.outputs);
    let json_text = serde_json::to_string_pretty(&bcml)?;
    fs::create_dir_all(&mods_out_dir)?;
    let logs_dir = mods_out_dir.join("logs");
    fs::create_dir_all(&logs_dir)?;
    fs::write(logs_dir.join("texts.json"), &json_text)?;
    eprintln!("  ✓ Wrote {} languages to logs/texts.json", all_langs.len());

    // ── Write ActorInfo YAML (preserve original format) ───────────────────
    if let Some(ref actor_yaml) = bnp.actorinfo_yaml {
        fs::write(logs_dir.join("actorinfo.yml"), actor_yaml)?;
        eprintln!("  ✓ Extracted logs/actorinfo.yml");
    }

    // ── Save backup ───────────────────────────────────────────────────────
    fs::copy(bnp_path, &backup_path)?;
    println!("  ✓ Backup saved: {}", backup_path.display());

    // ── Summary ───────────────────────────────────────────────────────────
    println!("\n── Summary ──");
    println!("  Platform:     {platform}");
    println!("  Mod:          {}", bnp.name);
    println!("  Languages:    {}", all_langs.len());
    println!("  Output:       {}", mods_out_dir.display());
    println!("  Backup:       {backup_name}");
    println!();
    open_explorer(&mods_out_dir);

    Ok(())
}

/// Rebuild a .bnp archive from edited files.
///
/// Reads `logs/texts.json` and `logs/actorinfo.yml` from the workspace,
/// then extracts the backup to a temp dir, replaces the files, and
/// re-compresses to a new `.bnp`.
fn run_bnp_rebuild(mod_name: &str, mods_out_dir: &Path, _orig_bnp_path: &str, backup_path: &Path, orig_path: &Path) -> Result<()> {
    println!("\n── Rebuilding BNP from edited files ──\n");

    let logs_dir = mods_out_dir.join("logs");

    // ── Read texts.json ───────────────────────────────────────────────────
    let texts_path = logs_dir.join("texts.json");
    if !texts_path.is_file() {
        anyhow::bail!("No logs/texts.json found in {}.", mods_out_dir.display());
    }
    let new_texts = fs::read_to_string(&texts_path)?;
    println!("  ✓ Read logs/texts.json");

    // ── Read actorinfo.yml if present ─────────────────────────────────────
    let actorinfo_path = logs_dir.join("actorinfo.yml");
    let actorinfo_yaml_str: Option<String> = if actorinfo_path.is_file() {
        Some(fs::read_to_string(&actorinfo_path)?)
    } else {
        None
    };

    // ── Check if original .bnp has been moved ─────────────────────────────
    let bnp_moved = !orig_path.exists();
    let rebuild_path = if bnp_moved {
        // Place rebuilt .bnp next to the workspace.
        let fallback = mods_out_dir.join(format!("{mod_name}.bnp"));
        eprintln!("Warning: original .bnp has been moved — saving rebuilt file to: {}",
            fallback.display());
        fallback
    } else {
        orig_path.to_path_buf()
    };

    // ── Extract backup to temp dir, replace texts.json, re-compress ───────
    let temp_dir = std::env::temp_dir().join("ukmm-extractool_bnp_rebuild");
    if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir)?;
    }
    fs::create_dir_all(&temp_dir)?;

    // Decompress the backup into the temp directory.
    let bnp_backup_file = fs::File::open(backup_path)?;
    sevenz_rust::decompress(bnp_backup_file, &temp_dir)
        .context("Failed to extract backup BNP")?;

    // Write the rebuilt texts.json.
    let texts_dir = temp_dir.join("logs");
    fs::create_dir_all(&texts_dir)?;
    fs::write(texts_dir.join("texts.json"), &new_texts)?;

    // ── Write ActorInfo YAML if present ────────────────────────────────────
    if let Some(ref actor_yaml) = actorinfo_yaml_str {
        fs::write(texts_dir.join("actorinfo.yml"), actor_yaml)?;
        println!("  ✓ Updated logs/actorinfo.yml");
    }

    // Re-compress the temp directory to a new .bnp.
    eprintln!("Compressing rebuilt BNP...");
    sevenz_rust::compress_to_path(&temp_dir, &rebuild_path)
        .context("Failed to compress rebuilt BNP")?;

    fs::remove_dir_all(&temp_dir)?;

    println!("  ✓ Rebuilt BNP: {}", rebuild_path.display());
    println!();
    open_explorer(mods_out_dir);

    Ok(())
}

/// Restore the original .bnp from backup.
fn run_bnp_restore(backup_path: &Path, _bnp_path: &str, orig_path: &Path) -> Result<()> {
    if !backup_path.exists() {
        anyhow::bail!("Backup not found: {}", backup_path.display());
    }
    println!("\n── Restoring original BNP from backup ──\n");
    fs::copy(backup_path, orig_path)?;
    println!("  ✓ Restored: {}", orig_path.display());
    println!();
    Ok(())
}

/// Sanitize a string for use as a directory name (replace path-unfriendly chars).
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == ' ' || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Open Windows Explorer at the given directory.
fn open_explorer(path: &Path) {
    // Explorer.exe on Windows needs absolute paths with backslashes.
    // Canonicalize the path to get an absolute, normalized form.
    let abs = path.canonicalize().unwrap_or_else(|_| {
        // If canonicalization fails (path doesn't exist yet), try parent.
        path.parent()
            .and_then(|p| p.canonicalize().ok())
            .unwrap_or_else(|| {
                // Last resort: use the path as-is.
                if path.is_absolute() { path.to_path_buf() }
                else {
                    // Prepend CWD for relative paths.
                    let mut cwd = std::env::current_dir().unwrap_or_default();
                    cwd.push(path);
                    cwd
                }
            })
    });

    if abs.is_dir() {
        let _ = std::process::Command::new("explorer")
            .arg(abs.as_os_str())
            .spawn();
    } else if abs.exists() {
        let arg = format!("/select,{}", abs.display());
        let _ = std::process::Command::new("explorer")
            .arg(&arg)
            .spawn();
    }
}

/// Recursively copy a directory tree.
///
/// Creates the destination directory, then recursively copies all files
/// and subdirectories from `src` to `dst`.
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &dest_path)?;
        } else {
            fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}

/// Create a ZIP file from a directory tree.
///
/// Opens a new ZIP writer at `dst` and recursively adds all files and
/// subdirectories from `src`.
fn create_zip_from_dir(src: &Path, dst: &Path) -> Result<()> {
    let file = fs::File::create(dst)?;
    let mut zip = zip::ZipWriter::new(file);
    add_dir_to_zip(src, src, &mut zip)?;
    zip.finish()?;
    Ok(())
}

/// Recursive helper for `create_zip_from_dir`.
///
/// Walks the directory tree rooted at `dir`, adding each file and
/// subdirectory to the ZIP. Paths inside the ZIP are relative to `base`.
fn add_dir_to_zip(base: &Path, dir: &Path, mut zip: &mut zip::ZipWriter<fs::File>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = path.strip_prefix(base).unwrap();
        if entry.file_type()?.is_dir() {
            zip.add_directory::<&str, ()>(&name.to_string_lossy(), Default::default())?;
            add_dir_to_zip(base, &path, zip)?;
        } else {
            zip.start_file::<&str, ()>(&name.to_string_lossy(), Default::default())?;
            let mut f = fs::File::open(&path)?;
            io::copy(&mut f, &mut zip)?;
        }
    }
    Ok(())
}

// ============================================================================
//  Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// CBOR maps (major type 5) have the high 3 bits = `0b101`.
    #[test]
    fn test_looks_like_cbor_map() {
        // A0 = map with 0 entries → should match.
        assert!(looks_like_cbor(&[0xA0]));
        // A1 = map with 1 entry.
        assert!(looks_like_cbor(&[0xA1]));
        // B8 = map header with 1-byte length prefix (25 entries).
        assert!(looks_like_cbor(&[0xB8, 0x19]));

        // Non-map bytes and empty input should not match.
        assert!(!looks_like_cbor(&[]));
        assert!(!looks_like_cbor(b"SARCxxxx"));
        assert!(!looks_like_cbor(&[0x80]));  // array
        assert!(!looks_like_cbor(&[0x60]));  // empty text string
    }

    /// SARC files contain the `SARC` magic at offset 0 or 0x11.
    #[test]
    fn test_is_sarc() {
        // SARC at offset 0, padded to minimum length (0x21 bytes).
        let mut d = b"SARC".to_vec();
        d.resize(0x21, b'x');
        assert!(is_sarc(&d));

        // SARC at offset 0x11 (after 0x11-byte prefix).
        let mut buf = vec![0u8; 0x11];
        buf.extend_from_slice(b"SARC");
        buf.resize(0x21, 0);
        assert!(is_sarc(&buf));

        // Too short or no SARC magic → not SARC.
        assert!(!is_sarc(&[0u8; 32]));
        assert!(!is_sarc(&[]));
    }

    /// Strings ≤ 23 bytes: encoded inline as 0x60 | len.
    #[test]
    fn test_cbor_write_text_short() {
        let mut buf = Vec::new();
        cbor_write_text(&mut buf, "hello");
        // 0x65 = 0x60 | 5 (length)
        assert_eq!(buf, [0x65, b'h', b'e', b'l', b'l', b'o']);
    }

    /// Strings of exactly 24 bytes: 0x78 prefix + 1-byte length.
    #[test]
    fn test_cbor_write_text_24_byte() {
        let s = "a".repeat(24);
        let mut buf = Vec::new();
        cbor_write_text(&mut buf, &s);
        // 0x78 = text with 1-byte length prefix.
        assert_eq!(buf[0], 0x78);
        assert_eq!(buf[1], 24);
        assert_eq!(&buf[2..], s.as_bytes());
    }

    /// Strings of 256 bytes: 0x79 prefix + 2-byte big-endian length.
    #[test]
    fn test_cbor_write_text_256_byte() {
        let s = "b".repeat(256);
        let mut buf = Vec::new();
        cbor_write_text(&mut buf, &s);
        // 0x79 = text with 2-byte length prefix.
        assert_eq!(buf[0], 0x79);
        assert_eq!(buf[1], 0x01);  // 256 big-endian high byte
        assert_eq!(buf[2], 0x00);  // 256 big-endian low byte
        assert_eq!(&buf[3..], s.as_bytes());
    }

    /// Strings > 65535 bytes: 0x7A prefix + 4-byte big-endian length.
    #[test]
    fn test_cbor_write_text_u32() {
        let s = "c".repeat(70_000);
        let mut buf = Vec::new();
        cbor_write_text(&mut buf, &s);
        // 0x7A = text with 4-byte length prefix.
        assert_eq!(buf[0], 0x7A);
        let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
        assert_eq!(len, 70_000);
    }

    /// Small map headers: length encoded inline.
    #[test]
    fn test_cbor_write_map_header_small() {
        let mut buf = Vec::new();
        cbor_write_map_header(&mut buf, 3);
        assert_eq!(buf, [0xA3]);      // 0xA0 | 3
    }

    /// Map headers with 1-byte length prefix (24-255 entries).
    #[test]
    fn test_cbor_write_map_header_u8() {
        let mut buf = Vec::new();
        cbor_write_map_header(&mut buf, 100);
        // 0xB8 = map with 1-byte length prefix.
        assert_eq!(buf, [0xB8, 100]);
    }

    /// Map headers with 2-byte length prefix (256-65535 entries).
    #[test]
    fn test_cbor_write_map_header_u16() {
        let mut buf = Vec::new();
        cbor_write_map_header(&mut buf, 500);
        // 0xB9 = map with 2-byte length prefix, 0x01F4 = 500.
        assert_eq!(buf, [0xB9, 0x01, 0xF4]);
    }

    /// Map headers with 4-byte length prefix (>65535 entries).
    #[test]
    fn test_cbor_write_map_header_u32() {
        let mut buf = Vec::new();
        cbor_write_map_header(&mut buf, 100_000);
        // 0xBA = map with 4-byte length prefix, 0x000186A0 = 100_000.
        assert_eq!(buf, [0xBA, 0x00, 0x01, 0x86, 0xA0]);
    }

    /// Empty input should produce no strings.
    #[test]
    fn test_extract_cbor_strings_empty() {
        let strings = extract_cbor_strings(&[]);
        assert!(strings.is_empty());
    }

    /// Two consecutive short CBOR text strings.
    #[test]
    fn test_extract_cbor_strings_simple() {
        // 0x63 = text, 3 bytes → "foo"; then "bar".
        let data = &[0x63, b'f', b'o', b'o', 0x63, b'b', b'a', b'r'];
        let strings = extract_cbor_strings(data);
        assert_eq!(strings, vec!["foo", "bar"]);
    }

    /// CBOR string with 1-byte length prefix (24 bytes).
    #[test]
    fn test_extract_cbor_strings_24byte_len() {
        let payload = "x".repeat(24);
        let mut data = vec![0x78, 24];          // 0x78 = text, 1-byte length.
        data.extend_from_slice(payload.as_bytes());
        let strings = extract_cbor_strings(&data);
        assert_eq!(strings, vec![payload]);
    }

    /// CBOR string with 2-byte length prefix (300 bytes).
    #[test]
    fn test_extract_cbor_strings_u16_len() {
        let payload = "y".repeat(300);
        let mut data = vec![0x79];
        data.extend_from_slice(&(300u16).to_be_bytes());
        data.extend_from_slice(payload.as_bytes());
        let strings = extract_cbor_strings(&data);
        assert_eq!(strings, vec![payload]);
    }

    /// CBOR string with 4-byte length prefix (70_000 bytes).
    #[test]
    fn test_extract_cbor_strings_u32_len() {
        let payload = "z".repeat(70_000);
        let mut data = vec![0x7A];
        data.extend_from_slice(&(70_000u32).to_be_bytes());
        data.extend_from_slice(payload.as_bytes());
        let strings = extract_cbor_strings(&data);
        assert_eq!(strings, vec![payload]);
    }

    /// Empty CBOR text string (0x60): should be skipped (not pushed).
    #[test]
    fn test_extract_cbor_strings_skips_empty() {
        // 0x60 = text, 0 bytes → skip; then "abc".
        let data = &[0x60, 0x63, b'a', b'b', b'c'];
        let strings = extract_cbor_strings(data);
        assert_eq!(strings, vec!["abc"]);      // Empty string is not included.
    }

    /// CBOR byte string (major type 2) treated as UTF-8 text.
    #[test]
    fn test_extract_cbor_strings_byte_string() {
        // 0x45 = byte string, 5 bytes → "hello".
        let data = &[0x45, b'h', b'e', b'l', b'l', b'o'];
        let strings = extract_cbor_strings(data);
        assert_eq!(strings, vec!["hello"]);
    }

    /// Strings nested inside a CBOR map should still be extracted.
    #[test]
    fn test_extract_cbor_strings_within_map() {
        // A1 = map(1), key="key" (0x63), value="value" (0x65).
        let data = &[
            0xA1,                       // map header (1 entry)
            0x63, b'k', b'e', b'y',     // key: "key"
            0x65, b'v', b'a', b'l', b'u', b'e',  // value: "value"
        ];
        let strings = extract_cbor_strings(data);
        // Both key and value strings are extracted, regardless of nesting.
        assert!(strings.contains(&"key".to_string()));
        assert!(strings.contains(&"value".to_string()));
    }

    /// Map header bytes (0xB8) should be skipped, not treated as string data.
    #[test]
    fn test_extract_cbor_strings_map_header_skipped() {
        // B8 19 = map header (25 entries), followed by "foo".
        let data = &[
            0xB8, 25,              // map header (skipped)
            0x63, b'f', b'o', b'o', // text: "foo"
        ];
        let strings = extract_cbor_strings(data);
        assert_eq!(strings, vec!["foo"]);
    }

    /// Round-trip: encode a string with `cbor_write_text`, then decode with
    /// `extract_cbor_strings`. The decoded string should match the original.
    #[test]
    fn test_cbor_write_text_roundtrip() {
        let s24 = "a".repeat(24);
        let s300 = "b".repeat(300);

        let inputs = ["a", "hello", &s24, &s300];
        for s in inputs {
            let mut buf = Vec::new();
            cbor_write_text(&mut buf, s);
            let strings = extract_cbor_strings(&buf);
            assert_eq!(strings, vec![s.to_string()], "roundtrip failed for len={}", s.len());
        }
    }

    #[test]
    fn test_decompress_passthrough() {
        let data = b"hello world";
        let result = decompress(data).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn test_decompress_yaz0() {

        let original = b"Hello, this is some test data for yaz0 compression!";
        let compressed = roead::yaz0::compress(original);
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_filename_stem() {
        assert_eq!(filename_stem(Path::new("Msg_EUfr.product.sarc")), "Msg_EUfr.product");
        assert_eq!(filename_stem(Path::new("/some/path/file.json")), "file");
        assert_eq!(filename_stem(Path::new("no_ext")), "no_ext");
    }

    #[test]
    fn test_is_sarc_too_short() {
        assert!(!is_sarc(b"SARC"));      }

    #[test]
    fn test_from_json_to_cbor_produces_zstd() {
        let section: IndexMap<String, Entry> = IndexMap::from([
            ("Key1".into(), Entry {
                attributes: None,
                contents: vec![msyt::model::Content::Text("Hello".into())],
            }),
        ]);
        let out = Output {
            language: Some("EUen".into()),
            entry_count: None,
            entries: BTreeMap::from([
                ("ActorType/ArmorHead".into(), serde_json::to_value(&section).unwrap()),
            ]),
            format: Some("UKMM CBOR".into()),
        };
        let result = from_json_to_cbor(&out).unwrap();

        assert_eq!(&result[0..4], [0x28, 0xB5, 0x2F, 0xFD]);

        let decompressed = zstd_decompress(&result[..]).unwrap();

        let cbor_strings = extract_cbor_strings(&decompressed);
        let all_text: String = cbor_strings.join(" ");
        assert!(all_text.contains("Mergeable"));
        assert!(all_text.contains("MessagePack"));
        assert!(all_text.contains("Hello"));
        assert!(all_text.contains("group_count"));
        assert!(all_text.contains("entries"));
    }

    #[test]
    fn test_zstd_dictionary_integrity() {

        assert!(
            ZSTD_DICTIONARY.len() > 1024,
            "zstd dictionary is too small ({} bytes) — it may be missing or truncated",
            ZSTD_DICTIONARY.len()
        );
        assert!(
            ZSTD_DICTIONARY.len() < 1024 * 1024,
            "zstd dictionary is suspiciously large ({} bytes)",
            ZSTD_DICTIONARY.len()
        );
        assert_eq!(
            &ZSTD_DICTIONARY[0..4],
            &[0x37, 0xA4, 0x30, 0xEC],
            "zstd dictionary is missing expected magic bytes — it may be corrupted or not a zstd dictionary"
        );
    }

    /// Debug: verify cbor_to_json on a simple known CBOR, then on the actorinfo data.
    #[test]
    fn test_cbor_actorinfo_trace() {
        // First, simple test: { "Mergeable": { "ActorInfo": { "0": [ { "test": { "I32": -1 } } ] } } }
        let simple = vec![
            // map(1): { "Mergeable": map(1): { "ActorInfo": map(1): { "0": array(1): [ map(1): { "test": map(1): { "I32": nint(0) } } ] } } }
            0xA1,                               // map(1)
            0x69, b'M',b'e',b'r',b'g',b'e',b'a',b'b',b'l',b'e', // "Mergeable"
            0xA1,                               // map(1)
            0x69, b'A',b'c',b't',b'o',b'r',b'I',b'n',b'f',b'o', // "ActorInfo"
            0xA1,                               // map(1)
            0x61, b'0',                         // "0"
            0x81,                               // array(1)
            0xA1,                               // map(1)
            0x64, b't',b'e',b's',b't',          // "test"
            0xA1,                               // map(1)
            0x63, b'I',b'3',b'2',               // "I32"
            0x20,                               // nint(0) = -1
        ];
        let val = cbor_to_json(&simple, &mut 0).unwrap();
        let json = serde_json::to_string_pretty(&val).unwrap();
        eprintln!("Simple test result:\n{json}");
        assert!(json.contains("Mergeable"));
        assert!(json.contains("ActorInfo"));
        assert!(json.contains("I32"));

        // Now test the real actorinfo data if available
        let byml_path = "byml_debug/ActorInfo.product.byml";
        if std::path::Path::new(byml_path).exists() {
            let raw = std::fs::read(byml_path)
                .expect("byml_debug/ActorInfo.product.byml should be readable");
            let data = decompress(&raw).expect("decompress should succeed");
            eprintln!("\nReal data: {} bytes after decompression", data.len());
            
            // Parse as CBOR (it's a Mergeable/ActorInfo structure)
            assert!(looks_like_actorinfo_cbor(&data), "Decompressed data should look like ActorInfo CBOR");
            let val = cbor_to_json(&data, &mut 0)
                .expect("cbor_to_json should parse ActorInfo CBOR successfully");
            let json = serde_json::to_string_pretty(&val).unwrap();
            eprintln!("Parsed OK, JSON length: {}", json.len());
            // Verify the structure is correct
            assert!(json.contains("Mergeable"), "Should contain Mergeable");
            assert!(json.contains("ActorInfo"), "Should contain ActorInfo");
            assert!(json.contains("weaponCommonPoweredSharpAddRapidFireMax"), "Should contain actor property");
            assert!(json.contains("Float"), "Should contain Float type");
            assert!(json.contains("I32"), "Should contain I32 type");
            assert!(json.contains("String"), "Should contain String type");
        } else {
            eprintln!("\nSkipping real data test: {byml_path} not found");
        }
    }

    /// Parse a real .bnp file and verify the extracted BnpData structure.
    ///
    /// The `.bnp` file is not committed to git, so this test is skipped
    /// when the file is absent (e.g. in CI runners).
    #[test]
    fn test_parse_bnp_stormbreaker() {
        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/stormbreaker_1b21d.bnp"
        ));
        if !path.exists() {
            eprintln!("Skipping BNP test — stormbreaker_1b21d.bnp not found");
            return;
        }
        let raw = std::fs::read(path).unwrap();
        let bnp = parse_bnp_bytes(&raw).unwrap();

        // Should have detected platform and name from info.json.
        assert_eq!(bnp.platform, "wiiu");
        assert_eq!(bnp.name, "Stormbreaker");

        // Should have extracted all languages.
        assert!(bnp.outputs.contains_key("USen"), "missing USen");
        assert!(bnp.outputs.contains_key("EUfr"), "missing EUfr");
        assert!(bnp.outputs.contains_key("CNzh"), "missing CNzh");

        // Contaminated sections should have been stripped from all languages.
        for (lang, out) in &bnp.outputs {
            assert!(!out.entries.contains_key("EventFlowMsg/MiniGame_Crosscountry"),
                "{lang} still has MiniGame_Crosscountry");
            assert!(!out.entries.contains_key("EventFlowMsg/MiniGame_HorsebackArchery"),
                "{lang} still has MiniGame_HorsebackArchery");

            // Each language should have the Stormbreaker weapon entries.
            let weapon_val = out.entries.get("ActorType/WeaponLargeSword")
                .unwrap_or_else(|| panic!("{lang} missing ActorType/WeaponLargeSword"));
            let weapon_section = weapon_val.as_object()
                .unwrap_or_else(|| panic!("{lang} ActorType/WeaponLargeSword is not an object"));
            assert!(weapon_section.contains_key("Weapon_Lsword_700_Name"));
            assert!(weapon_section.contains_key("Weapon_Lsword_700_Desc"));
            assert!(weapon_section.contains_key("Weapon_Lsword_700_PictureBook"));

            // Verify the Name entry content.
            let name_entry_val = &weapon_section["Weapon_Lsword_700_Name"];
            let name_entry: Entry = serde_json::from_value(name_entry_val.clone())
                .unwrap_or_else(|e| panic!("{lang}: failed to deserialize Name entry: {e}"));
            assert_eq!(name_entry.contents.len(), 1);
            if let msyt::model::Content::Text(t) = &name_entry.contents[0] {
                assert_eq!(t, "Stormbreaker");
            } else {
                panic!("{lang}: expected Text content for Name entry");
            }
        }
    }

    /// Verify that `rebuild_actorinfo_from_output` writes hash keys as CBOR unsigned
    /// integers (major type 0 / u32), not text strings (major type 3).
    /// UKMM expects u32 keys in the `ActorInfo` map.
    #[test]
    fn test_rebuild_actorinfo_u32_keys() {
        // Build an unfolded Output with a known hash value.
        let hash = 1022151304u32; // roead::aamp::hash_name("Weapon_Sword_026")
        let actor_data = serde_json::json!({
            "Map": {
                "name": { "String": "Weapon_Sword_026" },
                "attackPower": { "I32": 12 },
            }
        });
        let actor_entry = serde_json::Value::Array(vec![actor_data, serde_json::Value::Bool(false)]);

        let mut section = serde_json::Map::new();
        section.insert(hash.to_string(), actor_entry);

        let mut entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        entries.insert("ActorInfo.product".into(), serde_json::Value::Object(section));

        let out = Output {
            language: None,
            entry_count: None,
            entries,
            format: Some("ActorInfo".into()),
        };

        let cbor_bytes = rebuild_actorinfo_from_output(&out).unwrap();

        // Parse the CBOR back and verify structure.
        let val = cbor_to_json(&cbor_bytes, &mut 0).unwrap();

        // Check the deeply nested path exists.
        let actor_map = val.pointer("/Mergeable/ActorInfo").unwrap().as_object().unwrap();
        assert_eq!(actor_map.len(), 1, "should have 1 actor entry");

        // The key should be the hash as a string (JSON forces string keys).
        // But the key in the raw CBOR should be an integer, not a text string.
        // Verify this by scanning the raw CBOR bytes.
        // After "ActorInfo" text string, the next CBOR item is the ActorInfo map header,
        // then the first key. If it's a u32, the first byte will have major type 0
        // (0x00-0x1B range for unsigned int).
        let actor_info_key = b"ActorInfo";
        if let Some(pos) = cbor_bytes.windows(actor_info_key.len())
            .position(|w| w == actor_info_key)
        {
            // Skip past "ActorInfo" text CBOR item.
            let after_key = pos + actor_info_key.len();
            // Next CBOR item: map header for ActorInfo (should be 0xA1 for map(1)).
            assert_eq!(cbor_bytes[after_key] & 0xE0, 0xA0, "expected map header after ActorInfo");
            // Move past map header (could be 1, 2, or 4 bytes depending on length).
            let mut offset = after_key;
            let mt = cbor_bytes[offset] >> 5;
            let ai = cbor_bytes[offset] & 0x1F;
            offset += 1;
            match ai {
                24 => { offset += 1; }  // 1-byte length
                25 => { offset += 2; }  // 2-byte length
                26 => { offset += 4; }  // 4-byte length
                27 => { offset += 8; }  // 8-byte length
                _ => {}                 // inline length (0-23)
            }
            assert_eq!(mt, 5, "expected map major type");

            // Now offset points to the first key. It should be an unsigned integer.
            let key_mt = cbor_bytes[offset] >> 5;
            assert_eq!(key_mt, 0, "actor hash key should be unsigned int (major type 0), got major type {key_mt}");
            // Verify the value matches.
            let key_val: u64 = {
                let ai2 = cbor_bytes[offset] & 0x1F;
                offset += 1;
                match ai2 {
                    0..=23 => ai2 as u64,
                    24 => cbor_bytes[offset] as u64,
                    25 => u16::from_be_bytes([cbor_bytes[offset], cbor_bytes[offset+1]]) as u64,
                    26 => u32::from_be_bytes([cbor_bytes[offset], cbor_bytes[offset+1], cbor_bytes[offset+2], cbor_bytes[offset+3]]) as u64,
                    _ => panic!("unexpected CBOR uint encoding"),
                }
            };
            assert_eq!(key_val, hash as u64, "hash value should match");
        } else {
            panic!("Could not find 'ActorInfo' key in CBOR output");
        }
    }
}
