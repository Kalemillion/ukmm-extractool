//! # ukmm-extractool
//!
//! Extracts and rebuilds UKMM mod files (`.byml`/`.sarc`) to/from
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

use anyhow::{Context, Result};
use base64::Engine as _;
use indexmap::IndexMap;
use mimalloc::MiMalloc;
use msyt::{model::Entry, Msyt};
use rayon::prelude::*;
use roead::sarc::Sarc;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// Custom zstd dictionary embedded at compile time.
///
/// This dictionary is critical for compatibility with UKMM's compression format.
/// Without it, compression may be less effective or fail for some inputs.
/// The fallback is dictionary-less zstd (with a warning to stderr).
static ZSTD_DICTIONARY: &[u8] = include_bytes!("../data/zsdic");

/// First 2 bytes of a raw BYML file, big-endian ("BY") or little-endian ("YB").
const BYML_MAGIC_BE: &[u8] = b"BY";
const BYML_MAGIC_LE: &[u8] = b"YB";

/// First 4 bytes of a zstd-compressed block (0x28, 0xB5, 0x2F, 0xFD).
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Section names to automatically strip from extracted message files.
///
/// These sections contain data that shouldn't be included in rebuilt UKMM archives.
const FILTER_SECTIONS: &[&str] = &[
    "EventFlowMsg/MiniGame_Crosscountry",
    "EventFlowMsg/MiniGame_HorsebackArchery",
];

/// Known UKMM resource file extensions (excluding `.byml`/`.sbyml` and message files).
///
/// UKMM stores mod resources at canonical resource paths with these extensions.
/// The extractool discovers them alongside `.byml`/`.sbyml` and `Msg_*.sarc` files.
const UKMM_RESOURCE_EXTS: &[&str] = &[
    "bdemo",       // Demo files (AAMP-based)
    "bfarc",       // Font archives (SARC)
    "ssarc",       // SARC variant (game data)
    "sbactorpack", // Switch actor pack
    "sbmodelsh",   // Switch model/shape
    "sstats",      // Switch stats
    "bactorpack",  // Wii U actor pack
    "bmodelsh",    // Wii U model/shape
    "stats",       // Wii U stats
    "pack",        // Generic pack (AocMainField.pack, etc.)
];

/// Check whether a filename (just the basename) is a UKMM resource file.
///
/// Matches known resource extensions, `.byml`/`.sbyml`, and `Msg_*.sarc` patterns.
fn is_ukmm_resource_file(name: &str) -> bool {
    // Message files: Msg_<lang>.product.sarc or .ssarc
    if name.starts_with("Msg_") && name.contains(".product.s") && name.ends_with("rc") {
        return true;
    }
    // BYML files
    if name.ends_with(".byml") || name.ends_with(".sbyml") {
        return true;
    }
    // Known resource extensions
    if let Some(ext) = name.rsplit('.').next() {
        if UKMM_RESOURCE_EXTS.contains(&ext) {
            return true;
        }
    }
    false
}

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

/// Direction of conversion: extract (file → YAML/BYML) or rebuild (YAML/BYML → file).
#[derive(Clone, Copy, PartialEq)]
#[allow(dead_code)]
enum Direction {
    Extract,
    Rebuild,
}

/// Extension to use for a given format in extract direction.
fn extract_extension(out: &Output) -> &'static str {
    match out.format.as_deref() {
        Some("Mergeable" | "ActorInfo" | "BYML") => "sbyml",
        Some("SarcMap" | "Binary") => "yaml",
        Some("AAMP") => "yaml",
        _ => "yaml",
    }
}

/// Extension to use for a given format in rebuild direction.
fn rebuild_extension(out: &Output) -> &'static str {
    match out.format.as_deref() {
        Some("BYML" | "Mergeable" | "ActorInfo") => "byml",
        Some("SarcMap") => "srsarc",
        Some("Binary") => "bin",
        _ => "sarc",
    }
}

/// Dispatch extract: convert parsed Output into filesystem bytes.
/// Returns (bytes, was_handled) — if not handled, caller writes YAML fallback.
fn dispatch_extract(out: &Output) -> Result<Option<Vec<u8>>> {
    match out.format.as_deref() {
        Some("Mergeable") => {
            if let Some(b64) = out.entries.values().find_map(|v| {
                v.as_object()
                    .and_then(|m| m.get("_sbyml_bytes"))
                    .and_then(|s| s.as_str())
            }) {
                return Ok(Some(base64_decode(b64)?));
            }
            Ok(None) // YAML fallback
        }
        Some("ActorInfo") => {
            let compressed = actorinfo_output_to_sbyml(out)?;
            Ok(Some(compressed))
        }
        Some("BYML") => {
            use roead::byml::Byml;
            let val = serde_json::to_value(out)?;
            if let Some(entries_map) = val.get("entries").and_then(|v| v.as_object()) {
                for entries_val in entries_map.values() {
                    if let Some(section) = entries_val.as_object() {
                        if let Some(entry) = section.get("__byml__") {
                            if let Some(json_text) =
                                entry.get("attributes").and_then(|a| a.as_str())
                            {
                                let val: serde_json::Value = serde_json::from_str(json_text)?;
                                let byml: Byml = serde_json::from_value(val)?;
                                return Ok(Some(roead::yaz0::compress(byml.to_binary(roead::Endian::Big))));
                            }
                        }
                    }
                }
                let byml: Byml = serde_json::from_value(serde_json::to_value(&out.entries)?)?;
                return Ok(Some(roead::yaz0::compress(byml.to_binary(roead::Endian::Big))));
            }
            Ok(None)
        }
        Some("SarcMap") | Some("Binary") => Ok(None), // YAML fallback
        Some("AAMP") => Ok(None), // YAML fallback
        _ => Ok(None), // YAML fallback
    }
}

/// Dispatch rebuild: convert edited Output into CBOR/SARC binary for ZIP injection.
fn dispatch_rebuild(out: &Output, stem: &str) -> Result<Vec<u8>> {
    match out.format.as_deref() {
        Some("BYML") => rebuild_byml_from_output(out),
        Some("Mergeable") => {
            if let Some(b64) = out.entries.values().find_map(|v| {
                v.as_object()
                    .and_then(|m| m.get("_sbyml_bytes"))
                    .and_then(|s| s.as_str())
            }) {
                let sbyml_bytes = base64_decode(b64)?;
                let byml_data = decompress(&sbyml_bytes)
                    .context("Failed to decompress mergeable BYML")?;
                sbyml_to_mergeable_cbor(&byml_data, stem)
            } else {
                rebuild_mergeable_from_output(out)
            }
        }
        Some("ActorInfo") => {
            let raw = rebuild_actorinfo_from_output(out)?;
            zstd_compress(&raw)
        }
        Some("SarcMap") => rebuild_sarcmap_from_output(out),
        Some("Binary") => rebuild_binary_from_output(out),
        _ => from_json_to_cbor(out),
    }
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
        if let Ok(out) = d.decompress(data, size) {
            return Ok(out);
        }
    }
    eprintln!(
        "Warning: custom dictionary decompression failed, falling back to dictionary-less zstd"
    );
    // Streaming decoder with explicit size cap.
    let mut out = Vec::with_capacity(data.len().min(ZSTD_MAX_DECOMPRESS_SIZE));
    let mut decoder = zstd::Decoder::new(data)?;
    decoder.read_to_end(&mut out)?;
    if out.len() > ZSTD_MAX_DECOMPRESS_SIZE {
        anyhow::bail!(
            "zstd decompressed output exceeds {ZSTD_MAX_DECOMPRESS_SIZE} bytes — possible bomb"
        );
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
        if let Ok(out) = c.compress(data) {
            return Ok(out);
        }
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
/// Helper: create a minicbor Encoder writing to a Vec<u8>.
fn make_encoder(buf: &mut Vec<u8>) -> minicbor::Encoder<&mut Vec<u8>> {
    minicbor::Encoder::new(buf)
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
        let msyt_json = format!("{{\"group_count\":{group_count},\"entries\":{entries_json}}}");
        inner_entries.insert(section_name.clone(), msyt_json);
    }

    // ── Encode the CBOR structure ─────────────────────────────────────────

    let mut buf = Vec::with_capacity(65536);
    let mut enc = make_encoder(&mut buf);

    // Outer map: 1 entry (key "Mergeable" → inner map)
    enc.map(1).ok();
    enc.str("Mergeable").ok();

    // Inner map: 1 entry (key "MessagePack" → section map)
    enc.map(1).ok();
    enc.str("MessagePack").ok();

    // Section map: N entries (section_name → Msyt JSON string)
    enc.map(inner_entries.len() as u64).ok();
    for (key, value) in &inner_entries {
        enc.str(key).ok();
        enc.str(value).ok();
    }
    drop(enc); // release borrow on buf before zstd_compress

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
fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let d = if data.len() > 4 && data[0..4] == ZSTD_MAGIC {
        eprintln!("zstd...");
        zstd_decompress(data)?
    } else {
        // Check for direct yaz0 (non-zstd-wrapped) — common for raw .bshop files.
        if data.len() > 4 && data[0..4] == *b"Yaz0" {
            eprintln!("yaz0...");
            return Ok(roead::yaz0::decompress(data)?);
        }
        return Ok(data.to_vec());
    };
    // Check for yaz0 inside zstd
    if d.len() > 4 && d[0..4] == *b"Yaz0" {
        eprintln!("yaz0...");
        return Ok(roead::yaz0::decompress(&d)?);
    }
    Ok(d)
}

/// Heuristic: does this byte buffer look like a SARC archive?
///
/// Checks for the `SARC` magic bytes at either offset 0 or offset 0x11
/// (some SARC files have a 0x11-byte header before the magic).
/// Also requires at least 0x21 bytes to avoid false positives.
fn is_sarc(d: &[u8]) -> bool {
    d.len() > 0x20
        && (d[0..4] == *b"SARC" || d[0x11..0x15] == *b"SARC")
}

/// Heuristic: does the first byte look like a CBOR map header?
///
/// In CBOR, major type 5 (map) uses the high 3 bits = `0b101` (0xA0).
/// We mask with `0xE0` and compare to `0xA0`.
fn looks_like_cbor(d: &[u8]) -> bool {
    !d.is_empty() && (d[0] & 0xE0) == 0xA0
}

/// Heuristic: does this byte buffer look like raw BYML?
///
/// Checks for the `BY` (big endian / Wii U) or `YB` (little endian / Switch) magic.
fn looks_like_byml(d: &[u8]) -> bool {
    d.len() > 4 && (d[0..2] == *BYML_MAGIC_BE || d[0..2] == *BYML_MAGIC_LE)
}

/// Heuristic: does this byte buffer look like an AAMP file?
///
/// AAMP magic is `AAMP` at offset 0 (0x41 0x41 0x4D 0x50).
fn looks_like_aamp(d: &[u8]) -> bool {
    d.len() > 8 && d[0..4] == *b"AAMP"
}

/// Parse an AAMP (`.bshop` / `.aamp`) binary file into an `Output`.
///
/// The AAMP is serialized to roead's `!io` text format (YAML-based) and
/// stored as a single entry keyed by the file stem, so it round-trips cleanly.
fn parse_aamp_file_output(data: &[u8], path: &str) -> Result<Output> {
    let pio = roead::aamp::ParameterIO::from_binary(data).context("Failed to parse AAMP file")?;
    let text = pio.to_text();
    let stem = filename_stem(Path::new(path));
    let mut entries = BTreeMap::new();
    entries.insert(stem, serde_json::Value::String(text));
    Ok(Output {
        language: None,
        entry_count: None,
        entries,
        format: Some("AAMP".into()),
    })
}

/// Rebuild an AAMP file from the `Output`'s text representation.
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
        let n = match f.name {
            Some(s) => s,
            None => continue,
        };
        if !n.ends_with(".msbt") {
            continue;
        }
        let stem = n.trim_end_matches(".msbt").to_string();
        let msyt = Msyt::from_msbt_bytes(f.data())?;
        let bt: IndexMap<String, Entry> = msyt.entries.into_iter().collect();
        entries.insert(stem, serde_json::to_value(bt)?);
    }
    Ok(Output {
        language: None,
        entry_count: None,
        entries,
        format: None,
    })
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

    let mut strings = Vec::with_capacity(128);
    let mut dec = minicbor::Decoder::new(data);
    let _ = extract_strings_inner(&mut dec, &mut strings, MAX_STRING_LEN);
    strings
}

/// Recursively walk CBOR data, collecting all text and byte strings.
fn extract_strings_inner(
    dec: &mut minicbor::Decoder<'_>,
    strings: &mut Vec<String>,
    max_len: usize,
) -> Result<()> {
    use minicbor::data::Type;
    loop {
        let ty = match dec.datatype() {
            Ok(ty) => ty,
            Err(_) => break, // end of input
        };
        match ty {
            Type::U8 | Type::U16 | Type::U32 | Type::U64 => {
                let _ = dec.u64()?;
            }
            Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::Int => {
                let _ = dec.i64()?;
            }
            Type::F16 => {
                let _ = dec.f16()?;
            }
            Type::F32 => {
                let _ = dec.f32()?;
            }
            Type::F64 => {
                let _ = dec.f64()?;
            }
            Type::Bool => {
                let _ = dec.bool()?;
            }
            Type::Null => {
                dec.null()?;
            }
            Type::Undefined => {
                dec.undefined()?;
            }
            Type::Simple => {
                let _ = dec.simple()?;
            }
            Type::Bytes | Type::BytesIndef => {
                let raw = dec.bytes()?;
                if !raw.is_empty() && raw.len() <= max_len {
                    if let Ok(s) = std::str::from_utf8(raw) {
                        if !s.is_empty() {
                            strings.push(s.to_string());
                        }
                    }
                }
            }
            Type::String | Type::StringIndef => {
                let s = dec.str()?;
                if !s.is_empty() && s.len() <= max_len {
                    strings.push(s.to_string());
                }
            }
            Type::Array | Type::ArrayIndef => {
                let len = dec.array()?;
                if let Some(n) = len {
                    for _ in 0..n {
                        extract_strings_inner(dec, strings, max_len)?;
                    }
                } else {
                    loop {
                        match dec.datatype() {
                            Err(_) | Ok(Type::Break) => break,
                            _ => {
                                extract_strings_inner(dec, strings, max_len)?;
                            }
                        }
                    }
                    let _ = dec.skip();
                }
            }
            Type::Map | Type::MapIndef => {
                let len = dec.map()?;
                if let Some(n) = len {
                    for _ in 0..n {
                        extract_strings_inner(dec, strings, max_len)?;
                        extract_strings_inner(dec, strings, max_len)?;
                    }
                } else {
                    loop {
                        match dec.datatype() {
                            Err(_) | Ok(Type::Break) => break,
                            _ => {
                                extract_strings_inner(dec, strings, max_len)?;
                            }
                        }
                    }
                    let _ = dec.skip();
                }
            }
            Type::Tag => {
                let _ = dec.tag()?;
            }
            Type::Break | Type::Unknown(_) => {
                let _ = dec.skip();
            }
        }
    }
    Ok(())
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
            let next = &strings[i + 1];
            // Heuristic: non-JSON name followed by a JSON blob containing "entries"
            if !curr.starts_with("{")
                && next.starts_with("{")
                && next.contains("\"entries\":")
                && (next.contains("\"contents\":") || next.contains("\"group_count\":"))
            {
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
        let name = names
            .get(i)
            .cloned()
            .unwrap_or_else(|| format!("section_{i}"));

        // Deserialize the Msyt envelope: {"group_count":N,"entries":{...}}
        // Extract "entries" directly from the JSON string to avoid a clone.
        let entries_val: serde_json::Value = match serde_json::from_str(blob) {
            Ok(serde_json::Value::Object(mut map)) => map
                .remove("entries")
                .ok_or_else(|| anyhow::anyhow!("missing 'entries' key")),
            Ok(_) => {
                eprintln!("Warning: skipping JSON blob at index {i} — not an object");
                continue;
            }
            Err(e) => {
                eprintln!("Warning: skipping invalid JSON at index {i}: {e}");
                continue;
            }
        }
        .unwrap_or_else(|_| {
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

    Ok(Output {
        language: None,
        entry_count: None,
        entries,
        format: None,
    })
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

    // Strip fields that are still present (already skipped when None by serde).
    // At rebuild time, the format is auto-detected from the entries structure.
    if let Some(obj) = val.as_object_mut() {
        obj.remove("language");
        obj.remove("entry_count");
        obj.remove("format");
    }
    let yaml = serde_yaml::to_string(&val)?;
    fs::write(path, &yaml)?;
    eprintln!(
        "  ✓ Wrote {} entries to {}",
        out.entries.len(),
        path.display()
    );
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
    if entries.is_empty() {
        return None;
    }

    let mut mergeable = 0usize;
    let mut actorinfo = 0usize;
    let mut byml = 0usize;
    let mut sarcmap = 0usize;
    let mut binary = 0usize;
    let mut message = 0usize;

    for (key, val) in entries.iter() {
        // Check by entry key first for formats with distinctive key names.
        if key == "sarc_map" {
            sarcmap += 1;
        } else if key == "_data" {
            binary += 1;
        } else if key == "_sbyml_bytes" {
            mergeable += 1;
        } else {
            match val {
                // Mergeable (raw JSON with "Mergeable" key)
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
                                .map(|a| {
                                    a.len() == 2
                                        && a[0].is_object()
                                        && (a[1].is_boolean() || a[1].is_null())
                                })
                                .unwrap_or(false)
                        });
                    if is_actor {
                        actorinfo += 1;
                    } else {
                        message += 1;
                    }
                }
                _ => message += 1,
            }
        }

        // Early exit: if one format already dominates, return immediately.
        let total = mergeable + actorinfo + byml + sarcmap + binary + message;
        if total == 1 {
            continue;
        }
        let half = total / 2;
        if mergeable > half {
            return Some("Mergeable");
        }
        if actorinfo > half {
            return Some("ActorInfo");
        }
        if byml > half {
            return Some("BYML");
        }
        if sarcmap > half {
            return Some("SarcMap");
        }
        if binary > half {
            return Some("Binary");
        }
    }

    let max = mergeable
        .max(actorinfo)
        .max(byml)
        .max(sarcmap)
        .max(binary)
        .max(message);
    if max == 0 {
        return None;
    }
    if mergeable == max {
        Some("Mergeable")
    } else if actorinfo == max {
        Some("ActorInfo")
    } else if byml == max {
        Some("BYML")
    } else if sarcmap == max {
        Some("SarcMap")
    } else if binary == max {
        Some("Binary")
    } else {
        None
    }
}

/// Process a single file passed via CLI or drag-drop.
///
/// Routes to the correct conversion based on extension and content:
/// - `.byml`/`.sbyml` → forward conversion (BYML/ActorInfo → YAML/.sbyml)
/// - `.bshop`/`.aamp`/`.sbshop` → forward conversion (AAMP → YAML)
/// - `.yaml`/`.yml` → reverse conversion (AAMP YAML → binary) if it's AAMP
fn process_single_file(path: &str) -> Result<()> {
    let p = Path::new(path);
    if !p.exists() {
        anyhow::bail!("File not found: {}", path);
    }
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");

    match ext {
        "byml" | "sbyml" => {
            let out = convert_file(path)?;
            let stem = filename_stem(p);

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
            } else {
                let output_path = p.with_file_name(format!("{stem}.yaml"));
                write_output(&out, &output_path)?;
            }
            println!("\nDone!\n");
            Ok(())
        }
        "bshop" | "aamp" | "sbshop" => {
            let out = convert_file(path)?;
            let stem = filename_stem(p);
            let output_path = p.with_file_name(format!("{stem}.yaml"));
            write_output(&out, &output_path)?;
            println!("\nDone!\n");
            Ok(())
        }
        "yaml" | "yml" => {
            try_rebuild_aamp(path)
        }
        _ => {
            anyhow::bail!("Unsupported file extension: .{ext}");
        }
    }
}

/// Try to rebuild an AAMP binary from a YAML file containing `!io` text.
/// Returns Ok(()) on success, or a descriptive error if not AAMP.
fn try_rebuild_aamp(path: &str) -> Result<()> {
    let p = Path::new(path);
    let yaml_text = fs::read_to_string(path)?;
    let val: serde_json::Value = serde_yaml::from_str(&yaml_text)?;
    if let Some(entries) = val.get("entries").and_then(|v| v.as_object()) {
        if let Some(aamp_text) = entries.values().next().and_then(|v| v.as_str()) {
            if aamp_text.contains("!io") && aamp_text.contains("param_root") {
                let pio = roead::aamp::ParameterIO::from_text(aamp_text)
                    .context("Failed to parse AAMP text")?;
                let stem = filename_stem(p);
                let out_name = stem
                    .strip_suffix(".yaml")
                    .or_else(|| stem.strip_suffix(".yml"))
                    .unwrap_or(&stem);
                let output_path = p.with_file_name(out_name);
                fs::write(&output_path, pio.to_binary())?;
                println!("  ✓ Rebuilt AAMP: {}", output_path.display());
                println!("\nDone!\n");
                return Ok(());
            }
        }
    }
    anyhow::bail!("YAML file is not AAMP format (missing !io / param_root)");
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // If a file was passed as CLI argument or drag-dropped onto the exe,
    // bypass the menu and process it directly.
    if let Some(arg) = args.first() {
        let path = arg.trim_matches('"');
        if !Path::new(path).exists() {
            anyhow::bail!("File not found: {}", path);
        }
        return process_single_file(path);
    }

    // On Linux, when launched by double-click, there's no terminal attached.
    // Re-launch inside a terminal so the user can interact with the program.
    if cfg!(target_os = "linux") && !atty::is(atty::Stream::Stdin) {
        for term in [
            "xterm -e",
            "gnome-terminal --",
            "konsole -e",
            "xfce4-terminal --",
        ] {
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
fn prompt(message: &str) -> String {
    print!("{message}");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).ok();
    let line = line.trim().to_string();

    // Detect a file dropped at any prompt and process it directly.
    let path = line.trim_matches('"');
    if Path::new(path).exists() {
        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if matches!(ext, "byml" | "sbyml" | "bshop" | "aamp" | "sbshop" | "yaml" | "yml") {
            eprintln!();
            if let Err(e) = process_single_file(path) {
                eprintln!("Error: {e:#}");
            }
            prompt("\nPress Enter to exit... ");
            std::process::exit(0);
        }
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
        return PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("ukmm");
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
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
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

/// Interactive mode: scan UKMM mods, pick one, convert all resource files.
///
/// # Flow
///
/// 1. Ask user to select platform (Wii U / Switch)
/// 2. Scan the corresponding UKMM mods directory for ZIPs (with UKMM resource files)
///    and loose folders (with `meta.yml`)
/// 3. Present a numbered list, let the user choose
/// 4. Extract/copy the mod to a temp directory
/// 5. Convert each resource file to YAML or BYML under `mods/<platform>/<mod_name>/`
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
        println!("  [3] Info");
        let c = prompt("\nSelect 1, 2 or 3: ");
        match c.as_str() {
            "1" | "2" | "3" => break c,
            _ => eprintln!("Invalid choice — enter 1, 2, or 3.\n"),
        }
    };

    // Option 3: show info.
    if plat_choice == "3" {
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
        println!("║  Supported formats:                            ║");
        println!("║    - .byml / .sbyml (native Nintendo BYML)     ║");
        println!("║    - UKMM .sarc / CBOR mergeable archives      ║");
        println!("║    - .yaml / .yml (editable workspace)         ║");
        println!("╚════════════════════════════════════════════════╝");
        println!();
        prompt("Press Enter to continue... ");
        return run_interactive();
    }

    let is_switch = plat_choice == "2";
    let (platform, mods_dir) = if is_switch {
        ("nx", nx_path)
    } else {
        ("wiiu", wiiu_path)
    };

    if !mods_dir.is_dir() {
        anyhow::bail!(
            "Directory not found: {}\nMake sure UKMM is installed.",
            mods_dir.display()
        );
    }

    // ── Scan for mods ─────────────────────────────────────────────────────
    println!("\nScanning {}\n", mods_dir.display());

    let mut mods: Vec<ModEntry> = Vec::new();

    if let Ok(entries) = fs::read_dir(&mods_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let meta_path = path.join("meta.yml");
                if meta_path.is_file() {
                    // Accept any directory with meta.yml (all UKMM mods have it).
                    let display =
                        read_meta_name(&meta_path).unwrap_or_else(|| filename_stem(&path));
                    mods.push(ModEntry {
                        display_name: display,
                        path,
                        is_dir: true,
                    });
                }
            } else if path.extension().is_some_and(|e| e == "zip") {
                let display =
                    read_zip_meta_name(&path).unwrap_or_else(|| filename_stem(&path));
                // Only show mods with meta.yml (filter loose zip files)
                if !display.is_empty() && display != filename_stem(&path) {
                    mods.push(ModEntry {
                        display_name: display,
                        path,
                        is_dir: false,
                    });
                }
            } else if path.extension().is_some_and(|e| e == "zip") {
                // Fallback: accept any ZIP it's an UKMM mod
                // Accept any ZIP with meta.yml (even without known resource extensions).
                let display =
                    read_zip_meta_name(&path).unwrap_or_else(|| filename_stem(&path));
                mods.push(ModEntry {
                    display_name: display,
                    path,
                    is_dir: false,
                });
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
    println!("Found {} {}:\n", mods.len(), mod_label);
    for (i, m) in mods.iter().enumerate() {
        println!("  [{:2}] {}", i + 1, m.display_name);
    }

    let selection = prompt(&format!(
        "\nSelect a mod to process (1-{}), or press Enter to cancel: ",
        mods.len()
    ));
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

    let mod_dir_arg = format!("{}/{}", platform, mod_name);
    let mods_out_dir = PathBuf::from("mods").join(&mod_dir_arg);

    // Check for existing workspace (backup ZIP + any .json or .sbyml files recursively).
    let has_existing = mods_out_dir
        .join(format!("{mod_name}_backup.zip"))
        .is_file()
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
        return run_rebuild(
            &mod_name,
            &mods_out_dir,
            &mod_dir_arg,
            &chosen.path,
            chosen.is_dir,
        );
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

    // ── Convert all resource files ────────────────────────────────────────
    println!("\n── Converting mod files ──\n");

    let resource_files = find_resource_files(&extract_dir);

    if resource_files.is_empty() {
        anyhow::bail!("No UKMM resource files found in the mod.");
    }

    // Process files in parallel. Each file maps to its own output; no shared state.
    let results: Vec<Result<()>> = resource_files
        .par_iter()
        .map(|file| -> Result<()> {
            let file_path = file.display().to_string();
            let relative = file.strip_prefix(&extract_dir).unwrap_or(file);
            let stem = filename_stem(file);
            let ext = file.extension().and_then(|x| x.to_str()).unwrap_or("");

            // Message SARCs: process through convert_file with SARC → structured YAML
            if stem.starts_with("Msg_") && ext.ends_with("arc") {
                let output_path = mods_out_dir.join(relative).with_extension("yaml");
                if let Some(parent) = output_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                write_output(&convert_file(&file_path)?, &output_path)?;
                return Ok(());
            }

            // Use dispatch_extract for the standard format pipeline.
            let out = match convert_file(&file_path) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("  ⚠ Skipping {}: {e}", file.display());
                    return Ok(());
                }
            };
            let parent_dir = mods_out_dir
                .join(relative)
                .parent()
                .unwrap_or(&mods_out_dir)
                .to_path_buf();
            fs::create_dir_all(&parent_dir)?;

            let output_path = mods_out_dir.join(relative).with_extension(extract_extension(&out));
            match dispatch_extract(&out)? {
                Some(bytes) => {
                    fs::write(&output_path, &bytes)?;
                    eprintln!("  ✓ Converted to native BYML: {}", output_path.display());
                }
                None => {
                    write_output(&out, &output_path)?;
                }
            }
            Ok(())
        })
        .collect();

    // Propagate first error if any.
    for r in &results {
        if let Err(e) = r {
            anyhow::bail!("{}", e);
        }
    }

    // ── Save backup (only if it doesn't already exist) ────────────────────
    let backup_name = format!("{mod_name}_backup.zip");
    let backup_path = mods_out_dir.join(&backup_name);

    if !backup_path.exists() {
        fs::create_dir_all(&mods_out_dir)?;
        if !chosen.is_dir {
            fs::copy(&chosen.path, &backup_path)?;
            println!("  ✓ Backup saved: {}", backup_path.display());
        } else {
            create_zip_from_dir(&extract_dir, &backup_path)?;
        }
    }

    fs::remove_dir_all(&extract_dir)?;

    // ── Summary ──────────────────────────────────────────────────────────
    println!("\n── Summary ──");
    println!("  Platform:     {platform}");
    println!("  Mod:          {}", chosen.display_name);
    println!("  Files:        {}", resource_files.len());
    println!("  Output:       {}", mods_out_dir.display());
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
fn run_rebuild(
    mod_name: &str,
    mods_out_dir: &Path,
    _mod_dir_arg: &str,
    mod_path: &Path,
    is_dir: bool,
) -> Result<()> {
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
    edited_files.dedup_by(|a, b| a.file_stem() == b.file_stem());

    if edited_files.is_empty() {
        anyhow::bail!("No edited files found in {}.", mods_out_dir.display());
    }

    // ── Convert each edited file back to a CBOR SARC or BYML blob ────────
    let mut converted: Vec<(String, Vec<u8>)> = Vec::new();
    for file_path in &edited_files {
        // Preserve relative path from workspace for correct ZIP entry location.
        let relative = file_path.strip_prefix(mods_out_dir).unwrap_or(file_path);
        let stem = file_path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("unknown");

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
                    } else {
                        true
                    }
                }
                Err(_) => false,
            };

            if is_mergeable {
                let cbor_bytes = sbyml_to_mergeable_cbor(&data, stem)?;
                let zip_entry = relative.with_extension("byml");
                let zip_name = zip_entry.to_string_lossy().to_string();
                println!(
                    "  Converting (Mergeable): {} → {zip_name}",
                    file_path.file_name().unwrap_or_default().to_string_lossy()
                );
                converted.push((zip_name, cbor_bytes));
            } else {
                // Native ActorInfo BYML (Actors/Hashes format).
                let out = parse_byml_actorinfo(&data, &file_path.to_string_lossy())?;
                let raw_cbor = rebuild_actorinfo_from_output(&out)?;
                let compressed = zstd_compress(&raw_cbor)?;
                let zip_entry = relative.with_extension("byml");
                let zip_name = zip_entry.to_string_lossy().to_string();
                println!(
                    "  Converting (ActorInfo): {} → {zip_name}",
                    file_path.file_name().unwrap_or_default().to_string_lossy()
                );
                converted.push((zip_name, compressed));
            }
            continue;
        }

        // ── .yaml ───────────────────────────────────────────────────────
        let yaml_text = fs::read_to_string(file_path)?;
        let val: serde_json::Value = serde_yaml::from_str(&yaml_text)
            .with_context(|| format!("Failed to parse {}.", file_path.display()))?;
        let mut out: Output = serde_json::from_value(val).with_context(|| {
            format!("Failed to convert YAML {} to Output.", file_path.display())
        })?;

        // Auto-detect format from entries structure when stripped from YAML.
        if out.format.is_none() {
            out.format = detect_format(&out.entries).map(String::from);
        }

        let zip_name = relative
            .with_extension(rebuild_extension(&out))
            .to_string_lossy()
            .to_string();

        let stem = file_path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("unknown");

        let format_label = out.format.as_deref().unwrap_or("Message");
        let cbor_bytes = dispatch_rebuild(&out, stem)?;
        println!(
            "  Converting ({format_label}): {} → {zip_name}",
            file_path.file_name().unwrap_or_default().to_string_lossy()
        );
        converted.push((zip_name, cbor_bytes));
    }

    if converted.is_empty() {
        anyhow::bail!("No JSON files could be converted.");
    }

    // ── Build modified ZIP ────────────────────────────────────────────────
    // Strategy: scan the backup ZIP to find the correct entry path for each
    // converted file (by matching on filename stem), then copy all non-replaced
    // entries and append the new ones.
    let backup_file = fs::File::open(&backup_path)?;
    let mut backup_archive = zip::ZipArchive::new(backup_file)?;
    let modified_file = fs::File::create(&modified_path)?;
    let mut modified_zip = zip::ZipWriter::new(modified_file);

    // Build a map: filename stem → full entry path in the backup ZIP.
    // E.g., "EventInfo.product" → "Event/EventInfo.product.byml"
    // This lets us find the right entry regardless of subdirectory nesting.
    let mut backup_entry_map: BTreeMap<String, String> = BTreeMap::new();
    for i in 0..backup_archive.len() {
        if let Ok(entry) = backup_archive.by_index_raw(i) {
            let name = entry.name().to_string();
            let path = Path::new(&name);
            if let Some(stem) = path.file_stem().and_then(OsStr::to_str) {
                backup_entry_map.entry(stem.to_string()).or_insert(name);
            }
        }
    }

    // Resolve the actual ZIP entry path for each converted file.
    let resolved: Vec<(String, Vec<u8>)> = converted
        .into_iter()
        .map(|(workspace_path, data)| {
            let path = Path::new(&workspace_path);
            let stem = path
                .file_stem()
                .and_then(OsStr::to_str)
                .unwrap_or("");
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let target_name = format!("{stem}.{ext}");
            // Look up the backup entry by stem; fall back to workspace path.
            let zip_name = backup_entry_map
                .remove(stem)
                .unwrap_or_else(|| {
                    eprintln!("  Warning: no backup entry found for '{target_name}', using workspace path");
                    workspace_path
                });
            (zip_name, data)
        })
        .collect();

    // Copy all original entries, skipping the ones we're replacing.
    let resolved_names: Vec<&str> = resolved.iter().map(|(n, _)| n.as_str()).collect();
    for i in 0..backup_archive.len() {
        let mut entry = backup_archive.by_index(i)?;
        let entry_name = entry.name().to_string();
        if resolved_names.contains(&entry_name.as_str()) {
            continue; // Replaced below.
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

    // Append the new (or modified) entries using their resolved ZIP paths.
    for (entry_name, entry_bytes) in &resolved {
        modified_zip.start_file::<&str, ()>(
            entry_name,
            zip::write::FileOptions::<()>::default()
                .compression_method(zip::CompressionMethod::Stored),
        )?;
        modified_zip.write_all(entry_bytes)?;
        println!("  Added: {entry_name}");
    }

    modified_zip.finish()?;

    println!("\n── Summary ──");
    println!("  Modified ZIP: {}", modified_path.display());
    println!("  Files converted: {}", resolved.len());

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

/// Check whether a ZIP file contains any UKMM resource files.
///
/// Opens the ZIP and scans entry names without extracting data.
/// Returns `false` for any I/O error (file not found, corrupt ZIP, etc.).
#[allow(dead_code)]
fn peek_zip_has_ukmm_resources(zip_path: &Path) -> bool {
    let file = match fs::File::open(zip_path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return false;
    };
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index_raw(i) else {
            continue;
        };
        let name = entry.name();
        // Extract just the filename portion (after last / or \).
        if let Some(file_name) = name
            .split('/')
            .next_back()
            .or_else(|| name.split('\\').next_back())
        {
            if is_ukmm_resource_file(file_name) {
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
    io::BufReader::with_capacity(4096, meta)
        .read_to_string(&mut content)
        .ok()?;
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

/// Recursively find all UKMM resource files under a directory.
///
/// Matches files by [`is_ukmm_resource_file`] — covers `.byml`/`.sbyml`,
/// message files (`Msg_*.product.s*rc`), and all known resource extensions
/// (`.bdemo`, `.bfarc`, `.ssarc`, etc.).
fn find_resource_files(dir: &Path) -> Vec<PathBuf> {
    let mut results = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                results.extend(find_resource_files(&path));
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if is_ukmm_resource_file(name) {
                    results.push(path);
                }
            }
        }
    }
    results
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
                    let val: serde_json::Value = serde_json::from_str(json_text)
                        .context("Failed to parse BYML JSON content")?;
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
                if hash_str == "__byml__" {
                    continue;
                }
                // Each entry: [ { "Map": { "name": {"String":"..."}, ... } }, false ]
                let arr = actor_entry
                    .as_array()
                    .context("Actor entry should be an array")?;
                if arr.is_empty() {
                    continue;
                }

                // The actor entry is [ { "Map": { ... } }, false ].
                // arr[0] is already { "Map": { ... } } which is the correct roead
                // serde JSON format for a Byml Map. Deserialize it directly.
                let actor_data = serde_json::from_value::<Byml>(arr[0].clone())
                    .context("Failed to convert actor data to BYML")?;

                // Verify it has a "name" field
                let has_name = actor_data
                    .as_map()
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
                let hash: u32 = hash_str
                    .parse()
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
    let actors_val: Vec<serde_json::Value> = actors
        .into_iter()
        .filter_map(|b| serde_json::to_value(&b).ok())
        .collect();
    let hashes_val: Vec<serde_json::Value> = hashes
        .into_iter()
        .filter_map(|b| serde_json::to_value(&b).ok())
        .collect();

    let root_val = serde_json::json!({
        "Map": {
            "Actors": { "Array": actors_val },
            "Hashes": { "Array": hashes_val },
        }
    });

    let byml: Byml = serde_json::from_value(root_val).context("Failed to convert JSON to BYML")?;
    let binary = byml.to_binary(roead::Endian::Big);
    // Yaz0 compress → .sbyml
    let compressed = roead::yaz0::compress(&binary);
    Ok(compressed)
}

// ─────────────────────────────────────────────────────────────────────────────
// ResourceData::Sarc (SarcMap) CBOR support
// ─────────────────────────────────────────────────────────────────────────────

/// Parse a UKMM `ResourceData::Sarc(SarcMap)` CBOR blob into an `Output`.
///
/// The CBOR structure is:
/// ```cbor
/// { "Sarc": { "alignment": N, "files": ["file1", "file2", ...] } }
/// ```
///
/// This represents the diff of a SARC archive — only the file listing
/// (names + alignment) is tracked, not the actual file data. The output
/// is stored as YAML with a `SarcMap` format marker.
fn parse_sarcmap_cbor(data: &[u8], _path: &str) -> Result<Output> {
    let val = cbor_to_json(data, &mut 0)?;

    let sarc_data = val
        .get("Sarc")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let mut entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    entries.insert("sarc_map".to_string(), serde_json::Value::Object(sarc_data));

    Ok(Output {
        language: None,
        entry_count: None,
        entries,
        format: Some("SarcMap".into()),
    })
}

/// Rebuild a SarcMap CBOR binary from an edited Output.
///
/// Encodes the stored JSON value back to CBOR and zstd-compresses it.
fn rebuild_sarcmap_from_output(out: &Output) -> Result<Vec<u8>> {
    if let Some(sarc_data) = out.entries.get("sarc_map") {
        let mut buf = Vec::with_capacity(65536);
        // Re-wrap with "Sarc" key: UKMM expects { "Sarc": { "alignment": N, "files": [...] } }
        json_to_cbor(&serde_json::json!({ "Sarc": sarc_data }), &mut buf);
        return zstd_compress(&buf);
    }
    anyhow::bail!("No SarcMap data found in output");
}

// ─────────────────────────────────────────────────────────────────────────────
// ResourceData::Binary CBOR support
// ─────────────────────────────────────────────────────────────────────────────

/// Parse a UKMM `ResourceData::Binary(Vec<u8>)` CBOR blob into an `Output`.
///
/// The CBOR structure is:
/// ```cbor
/// { "Binary": [byte1, byte2, ...] }
/// ```
///
/// The binary data is base64-encoded and stored in a `_data` entry.
fn parse_binary_cbor(data: &[u8], _path: &str) -> Result<Output> {
    let val = cbor_to_json(data, &mut 0)?;

    // Extract the binary array: { "Binary": [byte1, byte2, ...] }
    let binary_array = val
        .get("Binary")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect::<Vec<u8>>()
        })
        .context("Binary CBOR: expected { \"Binary\": [bytes...] }")?;

    let b64 = base64_encode(&binary_array);
    let mut entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    entries.insert("_data".to_string(), serde_json::Value::String(b64));

    Ok(Output {
        language: None,
        entry_count: None,
        entries,
        format: Some("Binary".into()),
    })
}

/// Rebuild a Binary CBOR from an edited Output.
///
/// Reads the base64-encoded `_data` entry, decodes it, and wraps it back
/// in the CBOR structure for UKMM.
fn rebuild_binary_from_output(out: &Output) -> Result<Vec<u8>> {
    if let Some(b64) = out.entries.get("_data").and_then(|v| v.as_str()) {
        let raw_bytes = base64_decode(b64)?;
        // Build CBOR: { "Binary": [byte1, byte2, ...] }
        let arr: Vec<serde_json::Value> = raw_bytes
            .iter()
            .map(|&b| serde_json::Value::Number(serde_json::Number::from(b)))
            .collect();
        let wrapper = serde_json::json!({ "Binary": arr });
        let mut buf = Vec::with_capacity(65536);
        json_to_cbor(&wrapper, &mut buf);
        return zstd_compress(&buf);
    }
    anyhow::bail!("No Binary data found in output");
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
        && d[11] == 0xA1 // inner map(1)
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

/// Heuristic: does the byte buffer look like a UKMM `ResourceData::Sarc` CBOR?
///
/// Checks the first ~7 bytes for the structure:
/// `map(1){ "Sarc": ... }`
fn looks_like_sarcmap_cbor(d: &[u8]) -> bool {
    d.len() > 7
        && d[0] == 0xA1                                          // map(1)
        && d[1] == 0x64                                          // text(4)
        && &d[2..6] == b"Sarc"
}

/// Heuristic: does the byte buffer look like a UKMM `ResourceData::Binary` CBOR?
///
/// Checks the first ~9 bytes for the structure:
/// `map(1){ "Binary": [...] }`
fn looks_like_binary_cbor(d: &[u8]) -> bool {
    d.len() > 9
        && d[0] == 0xA1                                          // map(1)
        && d[1] == 0x66                                          // text(6)
        && &d[2..8] == b"Binary"
}

/// Recursively decode a CBOR byte buffer into a `serde_json::Value`.
///
/// Handles all major types needed for UKMM Mergeable structures:
/// uint, nint, bytes (→ base64 JSON string), text, array, map, tag, float.
/// Uses `minicbor::Decoder` for all CBOR type/length parsing.
fn cbor_to_json(data: &[u8], offset: &mut usize) -> Result<serde_json::Value> {
    let mut dec = minicbor::Decoder::new(&data[*offset..]);
    let val = decode_value(&mut dec)?;
    *offset = data.len() - dec.input().len();
    Ok(val)
}

/// Internal recursive decoder using minicbor::Decoder.
fn decode_value(dec: &mut minicbor::Decoder<'_>) -> Result<serde_json::Value> {
    use minicbor::data::Type;
    match dec.datatype()? {
        Type::U8 | Type::U16 | Type::U32 | Type::U64 => {
            let n = dec.u64()?;
            Ok(serde_json::Value::Number(n.into()))
        }
        Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::Int => {
            let n = dec.i64()?;
            Ok(serde_json::Value::Number(serde_json::Number::from(n)))
        }
        Type::F16 => {
            let n = dec.f16()?;
            serde_json::Number::from_f64(n as f64)
                .map(serde_json::Value::Number)
                .context("CBOR f16 out of range")
        }
        Type::F32 => {
            let n = dec.f32()?;
            serde_json::Number::from_f64(n as f64)
                .map(serde_json::Value::Number)
                .context("CBOR f32 out of range")
        }
        Type::F64 => {
            let n = dec.f64()?;
            serde_json::Number::from_f64(n)
                .map(serde_json::Value::Number)
                .context("CBOR f64 out of range")
        }
        Type::Bool => Ok(serde_json::Value::Bool(dec.bool()?)),
        Type::Null => {
            dec.null()?;
            Ok(serde_json::Value::Null)
        }
        Type::Undefined => {
            dec.undefined()?;
            Ok(serde_json::Value::Null)
        }
        Type::Simple => {
            dec.simple()?;
            Ok(serde_json::Value::Null)
        }
        Type::Bytes | Type::BytesIndef => {
            let raw = dec.bytes()?;
            Ok(serde_json::Value::String(format!(
                "\x01{}",
                base64_encode(raw)
            )))
        }
        Type::String | Type::StringIndef => {
            let s = dec.str()?;
            Ok(serde_json::Value::String(s.to_string()))
        }
        Type::Array | Type::ArrayIndef => {
            let len = dec.array()?;
            let cap = len.unwrap_or(4).min(4096) as usize;
            let mut arr = Vec::with_capacity(cap);
            if let Some(n) = len {
                for _ in 0..n {
                    arr.push(decode_value(dec)?);
                }
            } else {
                loop {
                    match dec.datatype() {
                        Err(_) | Ok(Type::Break) => break,
                        _ => {
                            arr.push(decode_value(dec)?);
                        }
                    }
                }
                // Consume the break marker.
                let _ = dec.skip();
            }
            Ok(serde_json::Value::Array(arr))
        }
        Type::Map | Type::MapIndef => {
            let len = dec.map()?;
            let cap = len.unwrap_or(4).min(4096) as usize;
            let mut map = serde_json::Map::with_capacity(cap);
            if let Some(n) = len {
                for _ in 0..n {
                    let k = decode_value(dec)?;
                    let v = decode_value(dec)?;
                    let key = match &k {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    map.insert(key, v);
                }
            } else {
                loop {
                    match dec.datatype() {
                        Err(_) | Ok(Type::Break) => break,
                        _ => {
                            let k = decode_value(dec)?;
                            let v = decode_value(dec)?;
                            let key = match &k {
                                serde_json::Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            map.insert(key, v);
                        }
                    }
                }
                // Consume the break marker.
                let _ = dec.skip();
            }
            Ok(serde_json::Value::Object(map))
        }
        Type::Tag => {
            dec.tag()?;
            decode_value(dec)
        }
        Type::Break => {
            // Should not reach here in normal data; return null.
            Ok(serde_json::Value::Null)
        }
        Type::Unknown(_) => {
            // Unknown/invalid CBOR type; skip the byte and return null.
            let _ = dec.skip();
            Ok(serde_json::Value::Null)
        }
    }
}

/// Convert an f16 (half-precision) bit pattern to f64.
#[allow(dead_code)]
fn f16_to_f64(bits: u16) -> f64 {
    let sign = ((bits >> 15) as f64) * -2.0 + 1.0;
    let exp = (bits >> 10) & 0x1f;
    let mant = bits & 0x3ff;
    match exp {
        0 => sign * (mant as f64) / 1024.0 / 16384.0, // subnormal
        31 => f64::NAN,                               // inf/nan → NAN
        _ => sign * (1.0 + (mant as f64) / 1024.0) * 2.0f64.powi((exp as i32) - 15),
    }
}

/// Base64 engine (RFC 4648, standard charset, padding enabled).
const B64_ENGINE: base64::engine::GeneralPurpose =
    base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::general_purpose::PAD,
    );

/// Encode bytes to base64 (RFC 4648) using the standard engine.
fn base64_encode(data: &[u8]) -> String {
    B64_ENGINE.encode(data)
}

/// Decode a base64 string (RFC 4648).
fn base64_decode(input: &str) -> Result<Vec<u8>> {
    B64_ENGINE
        .decode(input)
        .context("Invalid base64 input")
}

/// Check whether a string looks like a base64-encoded binary blob.
///
/// Returns `true` if the string has at least 4 characters, consists only of
/// valid base64 characters (A-Z, a-z, 0-9, +, /, =), length is a multiple of 4,
/// and padding is correct. This is used to reconstruct CBOR byte strings
/// from the JSON representation during round-trip.
#[allow(dead_code)]
fn looks_like_base64(s: &str) -> bool {
    if s.len() < 4 || !s.len().is_multiple_of(4) {
        return false;
    }
    let bytes = s.as_bytes();
    // Check for valid base64 characters
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' => continue,
            b'=' => {
                // Padding only at the end, max 2 characters
                let pad_start = s.len() - (s.len() - i);
                if pad_start < s.len() - 2 {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Recursively encode a `serde_json::Value` as CBOR bytes.
fn json_to_cbor(val: &serde_json::Value, buf: &mut Vec<u8>) {
    match val {
        serde_json::Value::Null => {
            let _ = make_encoder(buf).null();
        }
        serde_json::Value::Bool(b) => {
            let _ = make_encoder(buf).bool(*b);
        }
        serde_json::Value::Number(n) => {
            if let Some(v) = n.as_u64() {
                let _ = make_encoder(buf).u64(v);
            } else if let Some(v) = n.as_i64() {
                if v >= 0 {
                    let _ = make_encoder(buf).u64(v as u64);
                } else {
                    let _ = make_encoder(buf).i64(v);
                }
            } else if let Some(v) = n.as_f64() {
                // UKMM CBOR format uses f32 for all float values.
                let _ = make_encoder(buf).f32(v as f32);
            }
        }
        serde_json::Value::String(s) => {
            // Strings prefixed with \x01 are CBOR byte strings that were
            // marked by cbor_to_json. Strip the marker, decode from base64,
            // and write as CBOR byte string (major type 2).
            // All other strings are written as CBOR text (major type 3).
            if let Some(b64) = s.strip_prefix('\x01') {
                match base64_decode(b64) {
                    Ok(bytes) => {
                        let _ = make_encoder(buf).bytes(&bytes);
                    }
                    Err(_) => {
                        let _ = make_encoder(buf).str(s);
                    }
                }
            } else {
                let _ = make_encoder(buf).str(s);
            }
        }
        serde_json::Value::Array(arr) => {
            let _ = make_encoder(buf).array(arr.len() as u64);
            for item in arr {
                json_to_cbor(item, buf);
            }
        }
        serde_json::Value::Object(map) => {
            let _ = make_encoder(buf).map(map.len() as u64);
            for (k, v) in map {
                let _ = make_encoder(buf).str(k);
                json_to_cbor(v, buf);
            }
        }
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
    let actor_map = val
        .pointer("/Mergeable/ActorInfo")
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
            let is_unfolded = section
                .keys()
                .any(|k| k.parse::<u64>().is_ok() || k.starts_with("U32"));

            if is_unfolded {
                // Unfolded format: each key is a hash, value is [actor_data, deleted]
                let mut actor_map = section.clone();
                actor_map.remove("__byml__");
                if !actor_map.is_empty() {
                    // Write CBOR directly with u32 integer keys for hashes.
                    // json_to_cbor would encode all object keys as text strings,
                    // but UKMM expects the hash keys as CBOR unsigned integers.
                    // So we write the structure manually, using json_to_cbor for
                    // the values and a short-lived Encoder for the keys.
                    let mut buf = Vec::with_capacity(65536);
                    // { "Mergeable": { "ActorInfo": { u32_hash: value, ... } } }
                    let mut enc = make_encoder(&mut buf);
                    enc.map(1).ok();   // outer map: 1 entry
                    enc.str("Mergeable").ok();
                    enc.map(1).ok();   // inner map: 1 entry
                    enc.str("ActorInfo").ok();
                    enc.map(actor_map.len() as u64).ok();
                    drop(enc); // release borrow so json_to_cbor can use buf

                    for (hash_str, value) in &actor_map {
                        let hash: u64 = hash_str
                            .parse()
                            .with_context(|| format!("Invalid ActorInfo hash key: {hash_str}"))?;
                        // Write u32 hash key via a temporary Encoder.
                        make_encoder(&mut buf).u64(hash).ok();
                        json_to_cbor(value, &mut buf);
                    }
                    return Ok(buf);
                }
            }

            // Fallback: old __byml__ format with attributes JSON string
            if let Some(byyml) = section.get("__byml__") {
                if let Some(json_text) = byyml.get("attributes").and_then(|a| a.as_str()) {
                    let val: serde_json::Value = serde_json::from_str(json_text)
                        .context("Failed to parse ActorInfo JSON")?;
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

    let byml = Byml::from_binary(byml_data).context("Failed to parse mergeable BYML")?;
    let val: serde_json::Value =
        serde_json::to_value(&byml).context("Failed to serialize mergeable BYML to JSON")?;

    // Derive the proper DataType from the stem:
    // "EventInfo.product" → "EventInfo", "ActorInfo.product" → "ActorInfo", etc.
    let type_name = default_type
        .strip_suffix(".product")
        .unwrap_or(default_type);

    // Re-wrap: { "Mergeable": { "<DataType>": { ... } } }
    let mut inner = serde_json::Map::new();
    inner.insert(type_name.to_string(), val);
    let outer = serde_json::json!({ "Mergeable": inner });

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
            if entries_val.as_str().is_some() && out.entries.contains_key("_sbyml_bytes") {
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
            } else {
                false
            }
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
    let inner = val
        .get("Mergeable")
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
    let byml: roead::byml::Byml = serde_json::from_value(payload).with_context(|| {
        format!(
            "Failed to deserialize mergeable payload to BYML: {}...",
            &payload_str[..payload_str.len().min(200)]
        )
    })?;
    let binary = byml.to_binary(roead::Endian::Big);
    let compressed = roead::yaz0::compress(&binary);

    let mut entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    entries.insert(
        "_sbyml_bytes".to_string(),
        serde_json::json!(base64_encode(&compressed)),
    );

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

    let actors = map
        .get("Actors")
        .and_then(|v| v.as_array().ok())
        .context("ActorInfo BYML missing 'Actors' array")?;

    let stem = filename_stem(Path::new(path));
    let mut actor_map = serde_json::Map::new();

    for actor_byml in actors {
        let actor_map_byml = actor_byml
            .as_map()
            .map_err(|_| anyhow::anyhow!("Actor entry is not a map"))?;
        let name = actor_map_byml
            .get("name")
            .and_then(|v| v.as_string().ok())
            .context("Actor entry missing 'name' string")?;

        // Compute the hash using roead's aamp hash (same as UKMM)
        let hash = roead::aamp::hash_name(name);

        // serde_json::to_value() for a Byml already produces the roead serde format:
        // { "Map": { "name": { "String": "..." }, ... } }
        // This matches the format from the CBOR path (arr[0] = { "Map": { ... } }).
        let actor_json =
            serde_json::to_value(actor_byml).context("Failed to serialize actor entry")?;

        // Wrap in the same format as CBOR ActorInfo: [ { "Map": { ... } }, false ]
        let wrapped = serde_json::Value::Array(vec![
            actor_json,
            serde_json::Value::Bool(false), // not deleted
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

/// Parse an ActorInfo YAML string (TotkBits/Bubble-Wrap export format) into
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

    // ── AAMP detection (bshop / aamp) ────────────────────────────────────
    if looks_like_aamp(&data) {
        eprintln!("aamp...");
        return parse_aamp_file_output(&data, path);
    }

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

    // ── UKMM ResourceData::Sarc / Binary CBOR detection ─────────────────
    if looks_like_sarcmap_cbor(&data) {
        eprintln!("sarcmap...");
        return parse_sarcmap_cbor(&data, path);
    }
    if looks_like_binary_cbor(&data) {
        eprintln!("binary...");
        return parse_binary_cbor(&data, path);
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

/// Open Windows Explorer at the given directory.
fn open_explorer(path: &Path) {
    let abs = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => path.parent()
            .and_then(|p| p.canonicalize().ok())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join(path)),
    };
    let _ = std::process::Command::new("explorer").arg(abs.as_os_str()).spawn();
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
        assert!(!looks_like_cbor(&[0x80])); // array
        assert!(!looks_like_cbor(&[0x60])); // empty text string
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
        make_encoder(&mut buf).str("hello").ok();
        // 0x65 = 0x60 | 5 (length)
        assert_eq!(buf, [0x65, b'h', b'e', b'l', b'l', b'o']);
    }

    /// Strings of exactly 24 bytes: 0x78 prefix + 1-byte length.
    #[test]
    fn test_cbor_write_text_24_byte() {
        let s = "a".repeat(24);
        let mut buf = Vec::new();
        make_encoder(&mut buf).str(&s).ok();
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
        make_encoder(&mut buf).str(&s).ok();
        // 0x79 = text with 2-byte length prefix.
        assert_eq!(buf[0], 0x79);
        assert_eq!(buf[1], 0x01); // 256 big-endian high byte
        assert_eq!(buf[2], 0x00); // 256 big-endian low byte
        assert_eq!(&buf[3..], s.as_bytes());
    }

    /// Strings > 65535 bytes: 0x7A prefix + 4-byte big-endian length.
    #[test]
    fn test_cbor_write_text_u32() {
        let s = "c".repeat(70_000);
        let mut buf = Vec::new();
        make_encoder(&mut buf).str(&s).ok();
        // 0x7A = text with 4-byte length prefix.
        assert_eq!(buf[0], 0x7A);
        let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
        assert_eq!(len, 70_000);
    }

    /// Small map headers: length encoded inline.
    #[test]
    fn test_cbor_write_map_header_small() {
        let mut buf = Vec::new();
        make_encoder(&mut buf).map(3).ok();
        assert_eq!(buf, [0xA3]); // 0xA0 | 3
    }

    /// Map headers with 1-byte length prefix (24-255 entries).
    #[test]
    fn test_cbor_write_map_header_u8() {
        let mut buf = Vec::new();
        make_encoder(&mut buf).map(100).ok();
        // 0xB8 = map with 1-byte length prefix.
        assert_eq!(buf, [0xB8, 100]);
    }

    /// Map headers with 2-byte length prefix (256-65535 entries).
    #[test]
    fn test_cbor_write_map_header_u16() {
        let mut buf = Vec::new();
        make_encoder(&mut buf).map(500).ok();
        // 0xB9 = map with 2-byte length prefix, 0x01F4 = 500.
        assert_eq!(buf, [0xB9, 0x01, 0xF4]);
    }

    /// Map headers with 4-byte length prefix (>65535 entries).
    #[test]
    fn test_cbor_write_map_header_u32() {
        let mut buf = Vec::new();
        make_encoder(&mut buf).map(100_000).ok();
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
        let mut data = vec![0x78, 24]; // 0x78 = text, 1-byte length.
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
        assert_eq!(strings, vec!["abc"]); // Empty string is not included.
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
            0xA1, // map header (1 entry)
            0x63, b'k', b'e', b'y', // key: "key"
            0x65, b'v', b'a', b'l', b'u', b'e', // value: "value"
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
            0xB8, 25, // map header (skipped)
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
            make_encoder(&mut buf).str(s).ok();
            let strings = extract_cbor_strings(&buf);
            assert_eq!(
                strings,
                vec![s.to_string()],
                "roundtrip failed for len={}",
                s.len()
            );
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
        assert_eq!(
            filename_stem(Path::new("Msg_EUfr.product.sarc")),
            "Msg_EUfr.product"
        );
        assert_eq!(filename_stem(Path::new("/some/path/file.json")), "file");
        assert_eq!(filename_stem(Path::new("no_ext")), "no_ext");
    }

    #[test]
    fn test_is_sarc_too_short() {
        assert!(!is_sarc(b"SARC"));
    }

    #[test]
    fn test_from_json_to_cbor_produces_zstd() {
        let section: IndexMap<String, Entry> = IndexMap::from([(
            "Key1".into(),
            Entry {
                attributes: None,
                contents: vec![msyt::model::Content::Text("Hello".into())],
            },
        )]);
        let out = Output {
            language: Some("EUen".into()),
            entry_count: None,
            entries: BTreeMap::from([(
                "ActorType/ArmorHead".into(),
                serde_json::to_value(&section).unwrap(),
            )]),
            format: Some("UKMM CBOR".into()),
        };
        let result = from_json_to_cbor(&out).unwrap();

        assert_eq!(&result[0..4], &ZSTD_MAGIC);

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
            0xA1, // map(1)
            0x69, b'M', b'e', b'r', b'g', b'e', b'a', b'b', b'l', b'e', // "Mergeable"
            0xA1, // map(1)
            0x69, b'A', b'c', b't', b'o', b'r', b'I', b'n', b'f', b'o', // "ActorInfo"
            0xA1, // map(1)
            0x61, b'0', // "0"
            0x81, // array(1)
            0xA1, // map(1)
            0x64, b't', b'e', b's', b't', // "test"
            0xA1, // map(1)
            0x63, b'I', b'3', b'2', // "I32"
            0x20, // nint(0) = -1
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
            assert!(
                looks_like_actorinfo_cbor(&data),
                "Decompressed data should look like ActorInfo CBOR"
            );
            let val = cbor_to_json(&data, &mut 0)
                .expect("cbor_to_json should parse ActorInfo CBOR successfully");
            let json = serde_json::to_string_pretty(&val).unwrap();
            eprintln!("Parsed OK, JSON length: {}", json.len());
            // Verify the structure is correct
            assert!(json.contains("Mergeable"), "Should contain Mergeable");
            assert!(json.contains("ActorInfo"), "Should contain ActorInfo");
            assert!(
                json.contains("weaponCommonPoweredSharpAddRapidFireMax"),
                "Should contain actor property"
            );
            assert!(json.contains("Float"), "Should contain Float type");
            assert!(json.contains("I32"), "Should contain I32 type");
            assert!(json.contains("String"), "Should contain String type");
        } else {
            eprintln!("\nSkipping real data test: {byml_path} not found");
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
        let actor_entry =
            serde_json::Value::Array(vec![actor_data, serde_json::Value::Bool(false)]);

        let mut section = serde_json::Map::new();
        section.insert(hash.to_string(), actor_entry);

        let mut entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        entries.insert(
            "ActorInfo.product".into(),
            serde_json::Value::Object(section),
        );

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
        let actor_map = val
            .pointer("/Mergeable/ActorInfo")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(actor_map.len(), 1, "should have 1 actor entry");

        // The key should be the hash as a string (JSON forces string keys).
        // But the key in the raw CBOR should be an integer, not a text string.
        // Verify this by scanning the raw CBOR bytes.
        // After "ActorInfo" text string, the next CBOR item is the ActorInfo map header,
        // then the first key. If it's a u32, the first byte will have major type 0
        // (0x00-0x1B range for unsigned int).
        let actor_info_key = b"ActorInfo";
        if let Some(pos) = cbor_bytes
            .windows(actor_info_key.len())
            .position(|w| w == actor_info_key)
        {
            // Skip past "ActorInfo" text CBOR item.
            let after_key = pos + actor_info_key.len();
            // Next CBOR item: map header for ActorInfo (should be 0xA1 for map(1)).
            assert_eq!(
                cbor_bytes[after_key] & 0xE0,
                0xA0,
                "expected map header after ActorInfo"
            );
            // Move past map header (could be 1, 2, or 4 bytes depending on length).
            let mut offset = after_key;
            let mt = cbor_bytes[offset] >> 5;
            let ai = cbor_bytes[offset] & 0x1F;
            offset += 1;
            match ai {
                24 => {
                    offset += 1;
                } // 1-byte length
                25 => {
                    offset += 2;
                } // 2-byte length
                26 => {
                    offset += 4;
                } // 4-byte length
                27 => {
                    offset += 8;
                } // 8-byte length
                _ => {} // inline length (0-23)
            }
            assert_eq!(mt, 5, "expected map major type");

            // Now offset points to the first key. It should be an unsigned integer.
            let key_mt = cbor_bytes[offset] >> 5;
            assert_eq!(
                key_mt, 0,
                "actor hash key should be unsigned int (major type 0), got major type {key_mt}"
            );
            // Verify the value matches.
            let key_val: u64 = {
                let ai2 = cbor_bytes[offset] & 0x1F;
                offset += 1;
                match ai2 {
                    0..=23 => ai2 as u64,
                    24 => cbor_bytes[offset] as u64,
                    25 => u16::from_be_bytes([cbor_bytes[offset], cbor_bytes[offset + 1]]) as u64,
                    26 => u32::from_be_bytes([
                        cbor_bytes[offset],
                        cbor_bytes[offset + 1],
                        cbor_bytes[offset + 2],
                        cbor_bytes[offset + 3],
                    ]) as u64,
                    _ => panic!("unexpected CBOR uint encoding"),
                }
            };
            assert_eq!(key_val, hash as u64, "hash value should match");
        } else {
            panic!("Could not find 'ActorInfo' key in CBOR output");
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // UKMM ResourceData formats
    // ─────────────────────────────────────────────────────────────────────────

    /// SarcMap CBOR: map(1) { "Sarc": ... }
    #[test]
    fn test_looks_like_sarcmap_cbor() {
        // CBOR: { "Sarc": { "alignment": 4, "files": [] } }
        let data = vec![
            0xA1, // map(1)
            0x64, b'S', b'a', b'r', b'c', // text(4) "Sarc"
            0xA2, // map(2)
            0x69, b'a', b'l', b'i', b'g', b'n', b'm', b'e', b'n', b't', // "alignment"
            0x04, // uint(4)
            0x65, b'f', b'i', b'l', b'e', b's', // "files"
            0x80, // array(0)
        ];
        assert!(looks_like_sarcmap_cbor(&data));
        assert!(!looks_like_sarcmap_cbor(b""));
        assert!(!looks_like_sarcmap_cbor(b"Mergeable"));
        assert!(!looks_like_sarcmap_cbor(&[0xA1, 0x64, b'S', b'a', b'r'])); // incomplete
    }

    /// Binary CBOR: map(1) { "Binary": [bytes...] }
    #[test]
    fn test_looks_like_binary_cbor() {
        // CBOR: { "Binary": [1, 2, 3] }
        let data = vec![
            0xA1, // map(1)
            0x66, b'B', b'i', b'n', b'a', b'r', b'y', // text(6) "Binary"
            0x83, // array(3)
            0x01, 0x02, 0x03, // 1, 2, 3
        ];
        assert!(looks_like_binary_cbor(&data));
        assert!(!looks_like_binary_cbor(b""));
        assert!(!looks_like_binary_cbor(b"Mergeable"));
        assert!(!looks_like_binary_cbor(&[0xA1, 0x66, b'B', b'i', b'n'])); // incomplete
    }

    /// SarcMap round-trip: parse → rebuild → verify bytes match.
    #[test]
    fn test_sarcmap_roundtrip() {
        // Build a SarcMap CBOR by hand
        let original = vec![
            0xA1, // map(1)
            0x64, b'S', b'a', b'r', b'c', // text(4) "Sarc"
            0xA2, // map(2)
            0x69, b'a', b'l', b'i', b'g', b'n', b'm', b'e', b'n', b't', // "alignment"
            0x18, 0x04, // uint(4)
            0x65, b'f', b'i', b'l', b'e', b's', // "files"
            0x82, // array(2)
            0x65, b'f', b'i', b'l', b'e', b'1', // "file1"
            0x65, b'f', b'i', b'l', b'e', b'2', // "file2"
        ];

        // Parse
        let out = parse_sarcmap_cbor(&original, "test.srsarc").unwrap();
        assert_eq!(out.format.as_deref(), Some("SarcMap"));
        let sarc_map = out.entries.get("sarc_map").unwrap().as_object().unwrap();
        assert_eq!(sarc_map.get("alignment").unwrap().as_u64(), Some(4));
        assert_eq!(sarc_map.get("files").unwrap().as_array().unwrap().len(), 2);

        // Rebuild — now wraps with "Sarc" key for UKMM compatibility.
        let rebuilt = rebuild_sarcmap_from_output(&out).unwrap();

        // The rebuilt output is zstd-compressed. Decompress to compare.
        let decompressed = zstd_decompress(&rebuilt).unwrap();

        // Parse the decompressed CBOR and verify the outer "Sarc" wrapper.
        let val = cbor_to_json(&decompressed, &mut 0).unwrap();
        let wrapped = val.as_object().unwrap();
        let sarc = wrapped.get("Sarc").unwrap().as_object().unwrap();
        assert_eq!(sarc.get("alignment").unwrap().as_u64(), Some(4));
        let files = sarc.get("files").unwrap().as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].as_str(), Some("file1"));
        assert_eq!(files[1].as_str(), Some("file2"));
    }

    /// Binary round-trip: parse → rebuild → verify bytes match.
    #[test]
    fn test_binary_roundtrip() {
        let test_data: Vec<u8> = (0..100).collect();

        // Build CBOR: { "Binary": [0, 1, 2, ..., 99] }
        let mut original = vec![
            0xA1, // map(1)
            0x66, b'B', b'i', b'n', b'a', b'r', b'y', // text(6) "Binary"
        ];
        // Array header for 100 entries
        {
            let mut enc = make_encoder(&mut original);
            enc.array(test_data.len() as u64).ok();
            for &b in &test_data {
                enc.u64(b as u64).ok();
            }
        }

        // Parse
        let out = parse_binary_cbor(&original, "test.bin").unwrap();
        assert_eq!(out.format.as_deref(), Some("Binary"));
        let stored_b64 = out.entries.get("_data").unwrap().as_str().unwrap();
        let decoded = base64_decode(stored_b64).unwrap();
        assert_eq!(decoded, test_data);

        // Rebuild
        let rebuilt = rebuild_binary_from_output(&out).unwrap();
        let decompressed = zstd_decompress(&rebuilt).unwrap();

        // Parse the decompressed CBOR
        let val = cbor_to_json(&decompressed, &mut 0).unwrap();
        let binary_arr = val.get("Binary").unwrap().as_array().unwrap();
        let roundtripped: Vec<u8> = binary_arr
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as u8))
            .collect();
        assert_eq!(roundtripped, test_data);
    }

    /// CBOR byte string: cbor_write_bytes → cbor_to_json → json_to_cbor
    #[test]
    fn test_cbor_bytestring_roundtrip() {
        let original_bytes: Vec<u8> = (0..50).collect();

        // Write CBOR byte string
        let mut buf = Vec::new();
        make_encoder(&mut buf).bytes(&original_bytes).ok();

        // Decode with cbor_to_json → should become \x01-prefixed base64 string
        let json_val = cbor_to_json(&buf, &mut 0).unwrap();
        let marked_str = json_val.as_str().unwrap();

        // Verify it has the \x01 marker prefix
        assert!(
            marked_str.starts_with('\x01'),
            "byte string should be prefixed with \\x01 marker"
        );
        let b64_str = &marked_str[1..];

        // Verify the base64 part looks valid
        assert!(
            looks_like_base64(b64_str),
            "after marker, byte string should be valid base64"
        );

        // Decode the base64 back
        let decoded = base64_decode(b64_str).unwrap();
        assert_eq!(
            decoded, original_bytes,
            "base64 round-trip should be lossless"
        );

        // Now encode back with json_to_cbor → should detect \x01 marker and emit byte string
        let mut rebuf = Vec::new();
        json_to_cbor(&json_val, &mut rebuf);

        // Verify the reconstructed CBOR has major type 2 (byte string) not 3 (text)
        assert!(rebuf.len() > 0, "reconstructed CBOR should not be empty");
        let mt = rebuf[0] >> 5;
        assert_eq!(
            mt, 2,
            "reconstructed CBOR should have major type 2 (byte string), got {mt}"
        );

        // Decode the reconstructed CBOR
        let re_json = cbor_to_json(&rebuf, &mut 0).unwrap();
        let re_marked = re_json.as_str().unwrap();
        assert!(
            re_marked.starts_with('\x01'),
            "reconstructed should also have \\x01 marker"
        );
        let re_b64 = &re_marked[1..];
        let re_decoded = base64_decode(re_b64).unwrap();
        assert_eq!(
            re_decoded, original_bytes,
            "full CBOR byte string round-trip should be lossless"
        );
    }

    /// Base64 detection
    #[test]
    fn test_looks_like_base64() {
        assert!(looks_like_base64("SGVsbG8="));
        assert!(looks_like_base64("YWJj"));
        assert!(looks_like_base64(
            "VGhpcyBpcyBhIGxvbmdlciBiYXNlNjQgc3RyaW5nIHRoYXQgc2hvdWxkIHdvcms="
        ));
        // Not base64
        assert!(!looks_like_base64("short"));
        assert!(!looks_like_base64(""));
        assert!(!looks_like_base64("abc"));
        assert!(!looks_like_base64("not!base64!!"));
        assert!(!looks_like_base64("has space"));
    }

    /// UKMM resource file detection
    #[test]
    fn test_is_ukmm_resource_file() {
        // Message files
        assert!(is_ukmm_resource_file("Msg_EUen.product.sarc"));
        assert!(is_ukmm_resource_file("Msg_EUfr.product.ssarc"));
        assert!(!is_ukmm_resource_file("Msg_EUen.product.bad"));

        // BYML files
        assert!(is_ukmm_resource_file("ActorInfo.product.byml"));
        assert!(is_ukmm_resource_file("ActorInfo.product.sbyml"));

        // Resource extensions
        assert!(is_ukmm_resource_file("Demo_101.bdemo"));
        assert!(is_ukmm_resource_file("Font_Normal.bfarc"));
        assert!(is_ukmm_resource_file("gamedata.ssarc"));
        assert!(is_ukmm_resource_file("AocMainField.pack"));

        // Not resource files
        assert!(!is_ukmm_resource_file("meta.yml"));
        assert!(!is_ukmm_resource_file("manifest.yml"));
        assert!(!is_ukmm_resource_file("readme.txt"));
        assert!(!is_ukmm_resource_file("thumb.png"));

        // Edge cases
        assert!(!is_ukmm_resource_file(""));
        assert!(!is_ukmm_resource_file("no_extension"));
    }
}
