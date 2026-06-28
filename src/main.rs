//! # ukmmsg2json
//!
//! Converts UKMM (UK Mod Manager) `Msg_*.product.sarc` files to editable JSON and back.
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
use roead::sarc::Sarc;
use serde::{Deserialize, Serialize};

/// Custom zstd dictionary embedded at compile time.
///
/// This dictionary is critical for compatibility with UKMM's compression format.
/// Without it, compression may be less effective or fail for some inputs.
/// The fallback is dictionary-less zstd (with a warning to stderr).
static ZSTD_DICTIONARY: &[u8] = include_bytes!("../data/zsdic");

/// Top-level JSON structure produced by the rebuild step.
///
/// The forward path (extract) now always goes through the interactive mode;
/// this struct is used internally when converting JSON back to the
/// UKMM CBOR wire format during rebuild.
///
/// # JSON layout
///
/// ```json
/// {
///   "language": "EUen",
///   "entry_count": 2,
///   "entries": {
///     "Msg_EUen": {
///       "Npc_RecipeName": { "attributes": null, "contents": [...] },
///       "Npc_ShopItem":   { "attributes": "...", "contents": [...] }
///     }
///   },
///   "format": "SARC"
/// }
/// ```
#[derive(Serialize, Deserialize)]
struct Output {
    /// 4-letter language code (e.g. "USen", "EUfr"), extracted from the section filename.
    language: String,
    /// Must equal `entries.len()`. Validated by `from_json_to_cbor()`.
    entry_count: usize,
    /// Section name → ordered map of label → Entry. Uses `BTreeMap` for deterministic key
    /// ordering and `IndexMap` to preserve insertion order within each section.
    entries: BTreeMap<String, IndexMap<String, Entry>>,
    /// Source format hint: `"SARC"` or `"UKMM CBOR"`. Omitted from JSON when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<String>,
}

/// Try to decompress a zstd-compressed buffer using the custom UKMM dictionary.
///
/// Falls back to dictionary-less zstd if the dictionary-based decompressor
/// can't be constructed or the decompression itself fails.
fn zstd_decompress(data: &[u8]) -> Result<Vec<u8>> {
    // Attempt dictionary-based decompression first (UKMM's preferred format).
    if let Ok(mut d) = zstd::bulk::Decompressor::with_dictionary(ZSTD_DICTIONARY) {
        // upper_bound() may error for some compressed data — fall back to a generous estimate.
        let size = zstd::bulk::Decompressor::upper_bound(data)
            .unwrap_or(data.len().saturating_mul(1024));
        if let Ok(out) = d.decompress(data, size) { return Ok(out); }
    }
    eprintln!("Warning: custom dictionary decompression failed, falling back to dictionary-less zstd");
    zstd::decode_all(data).context("zstd decompress failed")
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
/// - `language` must not be empty and ≤ 64 chars
/// - `entries` must not be empty
/// - `entry_count` must match `entries.len()`
/// - Each section name: non-empty, ≤ 512 chars, no `..`, no control characters
fn from_json_to_cbor(out: &Output) -> Result<Vec<u8>> {
    // ── Input validation ──────────────────────────────────────────────────

    if out.language.is_empty() {
        anyhow::bail!("Output language field is empty — refusing to produce CBOR");
    }
    if out.language.len() > 64 {
        anyhow::bail!(
            "Output language field is suspiciously long ({} chars) — refusing to produce CBOR",
            out.language.len()
        );
    }
    if out.entries.is_empty() {
        anyhow::bail!("Output has no entries — refusing to produce empty CBOR");
    }
    if out.entry_count != out.entries.len() {
        anyhow::bail!(
            "Output entry_count ({}) does not match entries map length ({}) — data may be corrupted",
            out.entry_count,
            out.entries.len()
        );
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
        let group_count = entries.len() as u32;

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
/// This handles the common case where `.product.s*rc` files are:
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
/// 1. Extract the language code from the **second underscore-delimited segment** (first 4 chars)
///    of the filename (e.g. `Msg_EUen.product` → `"EUen"`)
/// 2. Parse the MSBT bytes via `Msyt::from_msbt_bytes()`
/// 3. Insert entries into the output map keyed by the file stem (without `.msbt` extension)
fn parse_sarc(data: &[u8]) -> Result<Output> {
    let mut lang = "unknown".to_string();
    let mut entries: BTreeMap<String, IndexMap<String, Entry>> = BTreeMap::new();
    let sarc = Sarc::new(data)?;
    for f in sarc.files() {
        let n = match f.name { Some(s) => s, None => continue };
        if !n.ends_with(".msbt") { continue; }
        let stem = n.trim_end_matches(".msbt").to_string();
        // Extract language from second segment: e.g. "Msg_EUen" → "EUen"
        if lang == "unknown" {
            if let Some(c) = stem.split('_').nth(1).map(|s| s.chars().take(4).collect::<String>()) {
                if c.len() == 4 { lang = c; }
            }
        }
        let msyt = Msyt::from_msbt_bytes(f.data())?;
        let bt: IndexMap<String, Entry> = msyt.entries.into_iter().collect();
        entries.insert(stem, bt);
    }
    Ok(Output { language: lang, entry_count: entries.len(), entries, format: Some("SARC".into()) })
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

    let mut strings = Vec::new();
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
/// 4. Extract the language code from section names containing `/`.
///
/// # Fallback
///
/// If no entries are found via the string-pairing heuristic, the function
/// tries `Msyt::from_msbt_bytes()` on the raw data as a last resort (treating
/// the whole blob as raw MSBT). This handles edge cases where the CBOR structure
/// is unusual.
fn parse_cbor(data: &[u8]) -> Result<Output> {
    let strings = extract_cbor_strings(data);
    let mut entries: BTreeMap<String, IndexMap<String, Entry>> = BTreeMap::new();
    let mut lang = "unknown".to_string();

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

    // ── Extract language code from section names ──────────────────────────
    // Section names look like "Message/Msg_EUen.product" — extract "EUen".
    for n in &names {
        if n.contains("/") {
            let path = n.replace("\\", "/");
            if let Some(last) = path.split('/').next_back() {
                if let Some(c) = last.split('_').nth(1).map(|s| s.chars().take(4).collect::<String>()) {
                    if c.len() == 4 && c.chars().all(|ch| ch.is_alphanumeric()) {
                        lang = c;
                    }
                }
            }
        }
    }

    // ── Deserialize each JSON blob into the entries map ───────────────────
    for (i, blob) in json_blobs.iter().enumerate() {
        let name = names.get(i).cloned().unwrap_or_else(|| format!("section_{i}"));

        let val: serde_json::Value = match serde_json::from_str(blob) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Warning: skipping invalid JSON at index {i}: {e}");
                continue;
            }
        };

        let Some(entries_val) = val.get("entries") else {
            eprintln!("Warning: skipping JSON blob at index {i} — missing 'entries' key");
            continue;
        };

        if !entries_val.is_object() {
            eprintln!("Warning: skipping JSON blob at index {i} — 'entries' is not an object");
            continue;
        }

        // Sanity check: at least one entry should have a "contents" array.
        let mut has_contents = false;
        if let Some(obj) = entries_val.as_object() {
            for (_, entry_val) in obj {
                if entry_val.get("contents").is_some_and(|c| c.is_array()) {
                    has_contents = true;
                    break;
                }
            }
        }
        if !has_contents {
            eprintln!(
                "Warning: JSON blob at index {i} has 'entries' but no entry contains a 'contents' array — may not be valid Msyt data"
            );
        }

        match serde_json::from_value::<IndexMap<String, Entry>>(entries_val.clone()) {
            Ok(im) => {
                if im.is_empty() {
                    eprintln!("Warning: section '{name}' has zero entries after deserialization");
                }
                entries.insert(name, im);
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
        entries.insert("section_0".to_string(), bt);
    }

    Ok(Output { language: lang, entry_count: entries.len(), entries, format: Some("UKMM CBOR".into()) })
}

/// Serialize an `Output` struct to pretty-printed JSON and write to a file.
///
/// Creates parent directories if they don't exist. Prints a confirmation
/// message to stderr (so stdout stays clean for pipe usage).
fn write_output(out: &Output, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(out)?;
    fs::write(path, &json)?;
    eprintln!("  ✓ Wrote {} entries to {}", out.entry_count, path.display());
    Ok(())
}

fn main() -> Result<()> {
    run_interactive()
}

/// Print a prompt to stdout, flush, and read a single line from stdin.
///
/// Returns the trimmed line (without trailing newline). Returns empty string
/// on any I/O error (e.g. EOF).
fn prompt(message: &str) -> String {
    print!("{message}");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).ok();
    line.trim().to_string()
}

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

/// Interactive mode: scan UKMM mods, pick one, convert all message files.
///
/// # Flow
///
/// 1. Ask user to select platform (Wii U / Switch)
/// 2. Scan the corresponding UKMM mods directory for ZIPs (with `Msg_*` files)
///    and loose folders (with `meta.yml` + `Msg_*` files)
/// 3. Present a numbered list, let the user choose
/// 4. Extract/copy the mod to a temp directory
/// 5. Convert each `Msg_*.product.s*rc` file to JSON under `mods/<platform>/<mod_name>/`
/// 6. Save the original mod as `<mod_name>_backup.zip`
/// 7. If output already exists, offer to rebuild instead
fn run_interactive() -> Result<()> {
    println!();
    println!("╔═════════════════════════╗");
    println!("║       ukmmsg2json       ║");
    println!("╚═════════════════════════╝");
    println!();

    let ukmm_root = ukmm_data_dir();
    let wiiu_path = ukmm_root.join("wiiu").join("mods");
    let nx_path = ukmm_root.join("nx").join("mods");

    // ── Platform selection ────────────────────────────────────────────────
    println!("Choose your platform:");
    println!("  [1] Wii U");
    println!("  [2] Switch");
    let plat_choice = prompt("\nSelect 1 or 2 (default = 1): ");
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

    // Pass 1: ZIP files containing Msg_* files.
    if let Ok(entries) = fs::read_dir(&mods_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "zip") && peek_zip_has_msg(&path) {
                let display = read_zip_meta_name(&path)
                    .unwrap_or_else(|| filename_stem(&path));
                mods.push(ModEntry { display_name: display, path, is_dir: false });
            }
        }
    }

    // Pass 2: Loose directories with meta.yml and Msg_* files.
    if let Ok(entries) = fs::read_dir(&mods_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let meta_path = path.join("meta.yml");
                if meta_path.is_file() && !find_msg_files(&path).is_empty() {
                    let display = read_meta_name(&meta_path)
                        .unwrap_or_else(|| filename_stem(&path));
                    mods.push(ModEntry { display_name: display, path, is_dir: true });
                }
            }
        }
    }

    mods.sort_by_key(|a| a.display_name.to_lowercase());

    if mods.is_empty() {
        anyhow::bail!("No mods found in {}.", mods_dir.display());
    }

    // ── Mod selection ─────────────────────────────────────────────────────
    let mod_label = if mods.len() == 1 { "mod" } else { "mods" };
    println!("Found {} {}:\n", mods.len(), mod_label);
    for (i, m) in mods.iter().enumerate() {
        println!("  [{:2}] {}", i + 1, m.display_name);
    }

    let selection = prompt(&format!("\nSelect a mod to process (1-{}), or press Enter to cancel: ", mods.len()));
    if selection.is_empty() {
        println!("Cancelled.\n");
        return Ok(());
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

    // Check if previous output exists (backup ZIP + JSON files).
    let has_existing = mods_out_dir.join(format!("{mod_name}_backup.zip")).is_file()
        && mods_out_dir.read_dir()
            .map(|mut d| d.any(|e| e.as_ref().is_ok_and(|e| e.path().extension().is_some_and(|x| x == "json"))))
            .unwrap_or(false);

    let action = if has_existing {
        let a = prompt("Output already exists. [1] Extract again  [2] Rebuild from edited JSON  (default = 1): ");
        if a.trim() == "2" { "rebuild" } else { "extract" }
    } else {
        "extract"
    };

    if action == "rebuild" {
        return run_rebuild(&mod_name, &mods_out_dir, &mod_dir_arg);
    }

    // ── Extract/copy mod to temp directory ────────────────────────────────
    let temp_base = std::env::temp_dir().join("ukmmsg2json");
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
    println!("\n── Converting Msg SARC files to JSON ──\n");

    let msg_files = find_msg_files(&extract_dir);
    if msg_files.is_empty() {
        anyhow::bail!("No Msg_*.product.s*rc files found in the mod.");
    }

    for msg_file in &msg_files {
        let sarc_path = msg_file.display().to_string();
        let stem = filename_stem(msg_file);
        let output_path = mods_out_dir.join(format!("{stem}.json"));
        write_output(
            &convert_file(&sarc_path)?,
            &output_path,
        )?;
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
    println!("\n── Summary ──");
    println!("  Platform:     {platform}");
    println!("  Mod:          {}", chosen.display_name);
    println!("  JSON files:   {}", msg_files.len());
    println!("  Output:       {}", mods_out_dir.display());
    println!("  Backup:       {backup_name}");
    println!("\nDone!\n");

    Ok(())
}

/// Rebuild a UKMM mod ZIP from edited JSON files.
///
/// Reads all `.json` files from the output directory, converts each back to
/// a CBOR SARC blob via `from_json_to_cbor()`, then injects them into a copy
/// of the backup ZIP. Original `Message/<name>.sarc` entries are replaced;
/// all other ZIP entries are copied as-is. Converted entries use
/// `CompressionMethod::Stored` (no additional compression).
fn run_rebuild(mod_name: &str, mods_out_dir: &Path, _mod_dir_arg: &str) -> Result<()> {
    let backup_name = format!("{mod_name}_backup.zip");
    let backup_path = mods_out_dir.join(&backup_name);
    let modified_name = format!("{mod_name}.zip");
    let modified_path = mods_out_dir.join(&modified_name);

    println!("\n── Rebuilding modified ZIP from edited JSONs ──\n");

    // ── Collect JSON files from the output directory ──────────────────────
    let json_files: Vec<PathBuf> = match fs::read_dir(mods_out_dir) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .map(|e| e.path())
            .collect(),
        Err(_) => vec![],
    };

    if json_files.is_empty() {
        anyhow::bail!("No JSON files found in {}.", mods_out_dir.display());
    }

    // ── Convert each JSON back to a CBOR SARC blob ─────────────────────────
    let mut converted: Vec<(String, Vec<u8>)> = Vec::new();
    for json_path in &json_files {
        let stem = json_path.file_stem().and_then(OsStr::to_str).unwrap_or("unknown");
        let sarc_name = format!("{stem}.sarc");
        println!("  Converting: {} → {sarc_name}", json_path.file_name().unwrap_or_default().to_string_lossy());

        let json_text = fs::read_to_string(json_path)?;
        let out: Output = serde_json::from_str(&json_text)
            .with_context(|| format!("Failed to parse {}.", json_path.display()))?;
        let sarc_bytes = from_json_to_cbor(&out)?;
        converted.push((sarc_name, sarc_bytes));
    }

    if converted.is_empty() {
        anyhow::bail!("No JSON files could be converted.");
    }

    // ── Build modified ZIP ────────────────────────────────────────────────
    // Strategy: copy all entries from the backup ZIP except the ones we're
    // replacing, then append the new SARC entries under `Message/`.
    let backup_file = fs::File::open(&backup_path)?;
    let mut backup_archive = zip::ZipArchive::new(backup_file)?;
    let modified_file = fs::File::create(&modified_path)?;
    let mut modified_zip = zip::ZipWriter::new(modified_file);

    let replace_names: Vec<String> = converted.iter()
        .map(|(name, _)| format!("Message/{name}"))
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

    // Append the new (or modified) SARC entries under `Message/`.
    // Stored without compression — they're already zstd-compressed.
    for (sarc_name, sarc_bytes) in &converted {
        let entry_name = format!("Message/{sarc_name}");
        modified_zip.start_file::<&str, ()>(&entry_name, zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Stored))?;
        modified_zip.write_all(sarc_bytes)?;
        println!("  Added: {entry_name}");
    }

    modified_zip.finish()?;

    println!("\n── Summary ──");
    println!("  Modified ZIP: {}", modified_path.display());
    println!("  Files converted: {}", converted.len());
    println!("\nDone!\n");

    Ok(())
}

/// Check whether a ZIP file contains any `Msg_*.product.s*rc` files.
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

/// Recursively find all `Msg_*.product.s*rc` files under a directory.
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

/// Read, decompress, and parse a single message file into an `Output` struct.
///
/// This is the same pipeline as `main()` uses for forward conversion,
/// extracted as a reusable function for the interactive mode.
fn convert_file(path: &str) -> Result<Output> {
    let raw = fs::read(path)?;
    let data = decompress(&raw)?;
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
        let out = Output {
            language: "EUen".into(),
            entry_count: 1,
            entries: BTreeMap::from([
                ("ActorType/ArmorHead".into(), IndexMap::from([
                    ("Key1".into(), Entry {
                        attributes: None,
                        contents: vec![msyt::model::Content::Text("Hello".into())],
                    }),
                ])),
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
}
