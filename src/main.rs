//! ukmmsg2json — convert BotW Msg files from UKMM mods to JSON
//! 
//!
//! UKMM stores mod text files as zstd(CBOR(ResourceData::Mergeable(MessagePack))).
//! The CBOR contains JSON-serialized Msyt objects (entries with text).
//! This tool handles: zstd decompress → CBOR text extraction → JSON output.
//!
//! Also works with raw `.sarc` files from BotW game dumps.
//!
//! Usage:
//!   ukmmsg2json input -o output.json              # write to a specific file
//!   ukmmsg2json input --mod-dir mod_name          # write to mods/mod_name/
//!   ukmmsg2json input --auto-dir                  # write to mods/<input_filename>/
//!   ukmmsg2json -l input                          # list languages

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};
use anyhow::{Context, Result};
use clap::Parser;
use indexmap::IndexMap;
use msyt::{model::Entry, Msyt};
use roead::sarc::Sarc;
use serde::{Deserialize, Serialize};

static ZSTD_DICTIONARY: &[u8] = include_bytes!("../data/zsdic");

#[derive(Parser)]
struct Cli {
    /// Input file (Msg_*.product.sarc, .zst, or UKMM CBOR blob)
    input: PathBuf,

    /// Output JSON path (default: stdout, or auto to mods/ with --mod-dir or --auto-dir)
    #[arg(short = 'o', long)]
    output: Option<PathBuf>,

    /// Output subdirectory name (creates mods/{mod_name}/)
    #[arg(long)]
    mod_dir: Option<String>,

    /// Auto-create mods/<input_filename>/ output directory
    #[arg(long)]
    auto_dir: bool,

    /// Force zstd decompression
    #[arg(short = 'd', long)]
    decompress: bool,

    /// List entry names only
    #[arg(short = 'l', long)]
    list_languages: bool,

    /// Convert JSON back to UKMM .sarc (zstd+CBOR) format
    #[arg(short = 'r', long)]
    reverse: bool,
}

#[derive(Serialize, Deserialize)]
struct Output {
    language: String,
    entry_count: usize,
    entries: BTreeMap<String, IndexMap<String, Entry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<String>,
}

fn zstd_decompress(data: &[u8]) -> Result<Vec<u8>> {
    if let Ok(mut d) = zstd::bulk::Decompressor::with_dictionary(ZSTD_DICTIONARY) {
        let size = zstd::bulk::Decompressor::upper_bound(data)
            .unwrap_or(data.len().saturating_mul(1024));
        if let Ok(out) = d.decompress(data, size) { return Ok(out); }
    }
    zstd::decode_all(data).context("zstd decompress failed")
}

fn zstd_compress(data: &[u8]) -> Result<Vec<u8>> {
    // Use UKMM's compression level (8) with the shared custom dictionary
    if let Ok(mut c) = zstd::bulk::Compressor::with_dictionary(8, ZSTD_DICTIONARY) {
        if let Ok(out) = c.compress(data) { return Ok(out); }
    }
    // Fallback: regular zstd (no dictionary) at matching level
    zstd::encode_all(data, 8).context("zstd compress failed")
}

/// Write a CBOR text string (major type 3).
fn cbor_write_text(buf: &mut Vec<u8>, s: &str) {
    let len = s.len();
    if len <= 23 {
        buf.push(0x60 | (len as u8));
    } else if len <= 0xFF {
        buf.push(0x78);  // AI=24: 1-byte u8 length
        buf.push(len as u8);
    } else if len <= 0xFFFF {
        buf.push(0x79);  // AI=25: 2-byte u16 length
        buf.extend_from_slice(&(len as u16).to_be_bytes());
    } else if len <= 0xFFFF_FFFF {
        buf.push(0x7A);  // AI=26: 4-byte u32 length
        buf.extend_from_slice(&(len as u32).to_be_bytes());
    } else {
        buf.push(0x7B);  // AI=27: 8-byte u64 length
        buf.extend_from_slice(&(len as u64).to_be_bytes());
    }
    buf.extend_from_slice(s.as_bytes());
}

/// Encode a CBOR map header with the given number of entries.
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

/// Convert JSON output back to UKMM .sarc format (zstd + CBOR).
///
/// UKMM stores message data as minicbor_ser-serialized `ResourceData` enum:
///   ResourceData::Mergeable(MergeableResource::MessagePack(MessagePack))
///
/// minicbor_ser uses externally-tagged CBOR for enums, so the CBOR structure is:
///   {"Mergeable": {"MessagePack": {"section": "{json}", ...}}}
///
/// The raw map we built before lacked the two enum wrappers, causing UKMM to fail
/// with "unknown variant, expected Binary, Mergeable, or Sarc".
fn from_json_to_cbor(out: &Output) -> Result<Vec<u8>> {
    // Build inner MessagePack entries: section_name → JSON string of Msyt entries
    // UKMM expects the full Msyt JSON format:
    //   {"msbt":{"group_count":N,...},"entries":{"Key":{"contents":[...]},...}}
    let mut inner_entries: BTreeMap<String, String> = BTreeMap::new();

    for (section_name, entries) in &out.entries {
        let entries_json = serde_json::to_string(entries)
            .with_context(|| format!("Failed to serialize entries for {section_name}"))?;
        let group_count = entries.len() as u32;
        // MsbtInfo is flattened into Msyt. All fields except group_count are Option
        // with skip_serializing_if = "Option::is_none" — they must be OMITTED, not set to default values.
        let msyt_json = format!(
            "{{\"group_count\":{group_count},\"entries\":{entries_json}}}"
        );
        inner_entries.insert(section_name.clone(), msyt_json);
    }

    let mut buf = Vec::with_capacity(65536);

    // Layer 1: ResourceData::Mergeable(...) — 1-entry map {"Mergeable": ...}
    buf.push(0xA1);
    cbor_write_text(&mut buf, "Mergeable");

    // Layer 2: MergeableResource::MessagePack(...) — 1-entry map {"MessagePack": ...}
    buf.push(0xA1);
    cbor_write_text(&mut buf, "MessagePack");

    // Layer 3: MessagePack (BTreeMap<String, Msyt>) — map with all sections
    cbor_write_map_header(&mut buf, inner_entries.len());
    for (key, value) in &inner_entries {
        cbor_write_text(&mut buf, key);
        cbor_write_text(&mut buf, value);
    }

    eprintln!("zstd compress...");
    let sarc = zstd_compress(&buf)?;
    Ok(sarc)
}

fn decompress(raw: &[u8]) -> Result<Vec<u8>> {
    let is_zstd = raw.len() > 4 && raw[0..4] == [0x28, 0xB5, 0x2F, 0xFD];
    let d = if is_zstd { eprintln!("zstd..."); zstd_decompress(raw)? } else { raw.to_vec() };
    if d.len() > 4 && d[0..4] == [b'Y', b'a', b'z', b'0'] {
        eprintln!("yaz0..."); Ok(roead::yaz0::decompress(&d)?)
    } else { Ok(d) }
}

fn is_sarc(d: &[u8]) -> bool {
    d.len() > 0x20 && (d[0..4] == [b'S',b'A',b'R',b'C'] || d[0x11..0x15] == [b'S',b'A',b'R',b'C'])
}

/// Quick check: does the data look like a CBOR map (major type 5)?
/// UKMM-encoded CBOR always starts with a map (`0xA0`-`0xBF`).
fn looks_like_cbor(d: &[u8]) -> bool {
    !d.is_empty() && (d[0] & 0xE0) == 0xA0  // major type 5 (map)
}

fn filename_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("unknown")
        .to_string()
}

/// Parse from a raw SARC archive (game dump style)
fn parse_sarc(data: &[u8]) -> Result<Output> {
    let mut lang = "unknown".to_string();
    let mut entries: BTreeMap<String, IndexMap<String, Entry>> = BTreeMap::new();
    let sarc = Sarc::new(data)?;
    for f in sarc.files() {
        let n = match f.name { Some(s) => s, None => continue };
        if !n.ends_with(".msbt") { continue; }
        let stem = n.trim_end_matches(".msbt").to_string();
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

/// Simple CBOR parser: extract all text strings (major type 3) and
/// byte strings (major type 2) encoded as UTF-8 from a CBOR blob.
fn extract_cbor_strings(data: &[u8]) -> Vec<String> {
    let mut strings = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        let mt = (b >> 5) & 0x07;
        let ai = (b & 0x1f) as usize;
        match mt {
            2 | 3 => {
                let (sl, adv) = match ai {
                    0..=23 => (ai, 1),
                    24 if i+1 < data.len() => (data[i+1] as usize, 2),
                    25 if i+2 < data.len() => {
                        (u16::from_be_bytes([data[i+1], data[i+2]]) as usize, 3)
                    }
                    26 if i+4 < data.len() => {
                        (u32::from_be_bytes([data[i+1], data[i+2], data[i+3], data[i+4]]) as usize, 5)
                    }
                    27 if i+8 < data.len() => {
                        let n = u64::from_be_bytes([
                            data[i+1], data[i+2], data[i+3], data[i+4],
                            data[i+5], data[i+6], data[i+7], data[i+8],
                        ]);
                        (n as usize, 9)  // truncate; usize < u64 on 32-bit but strings this large are infeasible
                    }
                    _ => { i += 1; continue; }
                };
                let str_start = i + adv;
                if str_start + sl <= data.len() {
                    if let Ok(s) = std::str::from_utf8(&data[str_start..str_start+sl]) {
                        if !s.is_empty() { strings.push(s.to_string()); }
                    }
                }
                i = str_start + sl;
            }
            4 | 5 => {
                let extra = match ai {
                    0..=23 => 0,
                    24 => 1,
                    25 => 2,
                    26 => 4,
                    27 => 8,
                    _ => { i += 1; continue; }
                };
                i += 1 + extra;
            }
            _ => {
                i += 1;
                if (24..=27).contains(&ai) { i += 1 << (ai - 24); }
            }
        }
    }
    strings
}

/// Parse from UKMM CBOR blob by extracting JSON strings from CBOR
/// and parsing them as Msyt entries.
fn parse_cbor(data: &[u8]) -> Result<Output> {
    let strings = extract_cbor_strings(data);
    let mut entries: BTreeMap<String, IndexMap<String, Entry>> = BTreeMap::new();
    let mut lang = "unknown".to_string();

    // Collect map key names (paths) and JSON blobs
    let mut names: Vec<String> = Vec::new();
    let mut json_blobs: Vec<String> = Vec::new();
    let mut i = 0;
    while i < strings.len() {
        if i + 1 < strings.len() {
            let curr = &strings[i];
            let next = &strings[i+1];
            if !curr.starts_with("{") && next.starts_with("{") && next.contains("\"entries\":") && (next.contains("\"contents\":") || next.contains("\"group_count\":")) {
                names.push(curr.clone());
                json_blobs.push(next.clone());
                i += 2;
                continue;
            }
        }
        if strings[i].contains("\"entries\":") && strings[i].contains("\"contents\":") {
            json_blobs.push(strings[i].clone());
        }
        i += 1;
    }

    // Extract language from path keys
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

    // Parse JSON blobs
    for (i, blob) in json_blobs.iter().enumerate() {
        let name = names.get(i).cloned().unwrap_or_else(|| format!("section_{i}"));
        match serde_json::from_str::<serde_json::Value>(blob) {
            Ok(val) => {
                if let Some(entries_val) = val.get("entries") {
                    match serde_json::from_value::<IndexMap<String, Entry>>(entries_val.clone()) {
                        Ok(im) => { entries.insert(name, im); }
                        Err(e) => { eprintln!("Warning: failed to deserialize entries: {e}"); }
                    }
                }
            }
            Err(e) => { eprintln!("Warning: failed to parse JSON at index {i}: {e}"); }
        }
    }

    if entries.is_empty() {
        let msyt = Msyt::from_msbt_bytes(data).context("No entries found in CBOR blob")?;
        let bt: IndexMap<String, Entry> = msyt.entries.into_iter().collect();
        entries.insert("section_0".to_string(), bt);
    }

    Ok(Output { language: lang, entry_count: entries.len(), entries, format: Some("UKMM CBOR".into()) })
}

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
    let cli = Cli::parse();

    // ── Reverse mode: JSON → UKMM .sarc ──
    if cli.reverse {
        let json_text = fs::read_to_string(&cli.input)?;
        let out: Output = serde_json::from_str(&json_text)
            .context("Failed to parse JSON input. Expected ukmmsg2json output format.")?;
        let sarc = from_json_to_cbor(&out)?;

        let output_path = cli.output.unwrap_or_else(|| {
            let stem = filename_stem(&cli.input);
            PathBuf::from(format!("{stem}.sarc"))
        });

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&output_path, &sarc)?;
        eprintln!("  ✓ Wrote {} entries, {} sections to {}",
            out.entry_count,
            out.entries.len(),
            output_path.display());
        return Ok(());
    }

    // ── Determine output path ──
    let output_path: Option<PathBuf> = if let Some(mod_name) = &cli.mod_dir {
        // mods/{mod_name}/{stem}.json
        let dir = PathBuf::from("mods").join(mod_name);
        let stem = filename_stem(&cli.input);
        Some(dir.join(format!("{stem}.json")))
    } else if cli.auto_dir {
        // ukmmsg2json/mods/{input_stem}/{input_stem}.json
        let stem = filename_stem(&cli.input);
        let dir = PathBuf::from("mods").join(&stem);
        Some(dir.join(format!("{stem}.json")))
    } else {
        cli.output
    };

    // ── Process input file ──
    let raw = fs::read(&cli.input)?;
    let data = decompress(&raw)?;

    let out = if is_sarc(&data) {
        parse_sarc(&data)?
    } else if looks_like_cbor(&data) {
        parse_cbor(&data).or_else(|e| {
            eprintln!("Warning: CBOR parse failed ({e}), trying SARC...");
            parse_sarc(&data)
        })?
    } else {
        parse_sarc(&data)?
    };

    if cli.list_languages {
        println!("{0}", out.language);
        for k in out.entries.keys() { println!("  {k}"); }
        return Ok(());
    }

    match output_path {
        Some(p) => write_output(&out, &p)?,
        None => {
            let json = serde_json::to_string_pretty(&out)?;
            println!("{json}");
        }
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
//  TESTS
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── looks_like_cbor ──

    #[test]
    fn test_looks_like_cbor_map() {
        // CBOR empty map: { } = 0xA0
        assert!(looks_like_cbor(&[0xA0]));
        // CBOR map with 1 entry: major type 5, AI=1 = 0xA1
        assert!(looks_like_cbor(&[0xA1]));
        // CBOR map with 25 entries: 0xB8 0x19
        assert!(looks_like_cbor(&[0xB8, 0x19]));
        // non-CBOR data: empty
        assert!(!looks_like_cbor(&[]));
        // non-CBOR data: SARC magic
        assert!(!looks_like_cbor(b"SARCxxxx"));
        // CBOR array (major type 4) — not a map
        assert!(!looks_like_cbor(&[0x80]));
        // CBOR text string (major type 3) — not a map
        assert!(!looks_like_cbor(&[0x60]));
    }

    #[test]
    fn test_is_sarc() {
        // SARC at offset 0, len > 0x20
        let mut d = b"SARC".to_vec();
        d.resize(0x21, b'x');
        assert!(is_sarc(&d));
        // 0x11 bytes of header then SARC magic, total > 0x20
        let mut buf = vec![0u8; 0x11];
        buf.extend_from_slice(b"SARC");
        buf.resize(0x21, 0);
        assert!(is_sarc(&buf));
        assert!(!is_sarc(&[0u8; 32]));
        assert!(!is_sarc(&[]));
    }

    // ── cbor_write_text ──

    #[test]
    fn test_cbor_write_text_short() {
        let mut buf = Vec::new();
        cbor_write_text(&mut buf, "hello");
        // 0x60 | 5 = 0x65
        assert_eq!(buf, [0x65, b'h', b'e', b'l', b'l', b'o']);
    }

    #[test]
    fn test_cbor_write_text_24_byte() {
        let s = "a".repeat(24);
        let mut buf = Vec::new();
        cbor_write_text(&mut buf, &s);
        // 0x78, 24, then 24 bytes
        assert_eq!(buf[0], 0x78);
        assert_eq!(buf[1], 24);
        assert_eq!(&buf[2..], s.as_bytes());
    }

    #[test]
    fn test_cbor_write_text_256_byte() {
        let s = "b".repeat(256);
        let mut buf = Vec::new();
        cbor_write_text(&mut buf, &s);
        // 0x79, 0x0100 (u16 big-endian)
        assert_eq!(buf[0], 0x79);
        assert_eq!(buf[1], 0x01);
        assert_eq!(buf[2], 0x00);
        assert_eq!(&buf[3..], s.as_bytes());
    }

    #[test]
    fn test_cbor_write_text_u32() {
        let s = "c".repeat(70_000);
        let mut buf = Vec::new();
        cbor_write_text(&mut buf, &s);
        // 0x7A, u32 big-endian = 0x00011170
        assert_eq!(buf[0], 0x7A);
        let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
        assert_eq!(len, 70_000);
    }

    // ── cbor_write_map_header ──

    #[test]
    fn test_cbor_write_map_header_small() {
        let mut buf = Vec::new();
        cbor_write_map_header(&mut buf, 3);
        assert_eq!(buf, [0xA3]);  // 0xA0 | 3
    }

    #[test]
    fn test_cbor_write_map_header_u8() {
        let mut buf = Vec::new();
        cbor_write_map_header(&mut buf, 100);
        // 0xB8, 100
        assert_eq!(buf, [0xB8, 100]);
    }

    #[test]
    fn test_cbor_write_map_header_u16() {
        let mut buf = Vec::new();
        cbor_write_map_header(&mut buf, 500);
        // 0xB9, 0x01F4
        assert_eq!(buf, [0xB9, 0x01, 0xF4]);
    }

    #[test]
    fn test_cbor_write_map_header_u32() {
        let mut buf = Vec::new();
        cbor_write_map_header(&mut buf, 100_000);
        // 0xBA, 0x000186A0
        assert_eq!(buf, [0xBA, 0x00, 0x01, 0x86, 0xA0]);
    }

    // ── extract_cbor_strings ──

    #[test]
    fn test_extract_cbor_strings_empty() {
        let strings = extract_cbor_strings(&[]);
        assert!(strings.is_empty());
    }

    #[test]
    fn test_extract_cbor_strings_simple() {
        // Two CBOR text strings: "foo" (0x63) and "bar" (0x63)
        let data = &[0x63, b'f', b'o', b'o', 0x63, b'b', b'a', b'r'];
        let strings = extract_cbor_strings(data);
        assert_eq!(strings, vec!["foo", "bar"]);
    }

    #[test]
    fn test_extract_cbor_strings_24byte_len() {
        let payload = "x".repeat(24);
        let mut data = vec![0x78, 24];  // 24-byte text string
        data.extend_from_slice(payload.as_bytes());
        let strings = extract_cbor_strings(&data);
        assert_eq!(strings, vec![payload]);
    }

    #[test]
    fn test_extract_cbor_strings_u16_len() {
        let payload = "y".repeat(300);
        let mut data = vec![0x79];
        data.extend_from_slice(&(300u16).to_be_bytes());
        data.extend_from_slice(payload.as_bytes());
        let strings = extract_cbor_strings(&data);
        assert_eq!(strings, vec![payload]);
    }

    #[test]
    fn test_extract_cbor_strings_u32_len() {
        let payload = "z".repeat(70_000);
        let mut data = vec![0x7A];
        data.extend_from_slice(&(70_000u32).to_be_bytes());
        data.extend_from_slice(payload.as_bytes());
        let strings = extract_cbor_strings(&data);
        assert_eq!(strings, vec![payload]);
    }

    #[test]
    fn test_extract_cbor_strings_skips_empty() {
        // Empty text string (0x60)
        let data = &[0x60, 0x63, b'a', b'b', b'c'];
        let strings = extract_cbor_strings(data);
        assert_eq!(strings, vec!["abc"]);  // empty string skipped
    }

    #[test]
    fn test_extract_cbor_strings_byte_string() {
        // CBOR byte string (major type 2) with UTF-8 content
        let data = &[0x45, b'h', b'e', b'l', b'l', b'o'];  // 5-byte byte string
        let strings = extract_cbor_strings(data);
        assert_eq!(strings, vec!["hello"]);
    }

    #[test]
    fn test_extract_cbor_strings_within_map() {
        // CBOR: {"key": "value"} — map with 1 entry (0xA1)
        // "key" = 0x63 'k' 'e' 'y'
        // "value" = 0x65 'v' 'a' 'l' 'u' 'e'
        let data = &[
            0xA1,           // map(1)
            0x63, b'k', b'e', b'y',      // text(3) "key"
            0x65, b'v', b'a', b'l', b'u', b'e',  // text(5) "value"
        ];
        let strings = extract_cbor_strings(data);
        // Map header (4|5) skips its additional info but not contents,
        // so the strings inside the map ARE found
        assert!(strings.contains(&"key".to_string()));
        assert!(strings.contains(&"value".to_string()));
    }

    #[test]
    fn test_extract_cbor_strings_map_header_skipped() {
        // Map with 25 entries (0xB8, 25 = 0x19), then a text string
        // The map header should be skipped, then the text string found
        let data = &[
            0xB8, 25,  // map(25) — 2 bytes header
            0x63, b'f', b'o', b'o',  // text(3) "foo"
        ];
        let strings = extract_cbor_strings(data);
        assert_eq!(strings, vec!["foo"]);
    }

    // ── roundtrip: cbor_write_text ↔ extract_cbor_strings ──

    #[test]
    fn test_cbor_write_text_roundtrip() {
        let s24 = "a".repeat(24);
        let s300 = "b".repeat(300);
        // empty string is intentionally skipped by extract_cbor_strings
        let inputs = ["a", "hello", &s24, &s300];
        for s in inputs {
            let mut buf = Vec::new();
            cbor_write_text(&mut buf, s);
            let strings = extract_cbor_strings(&buf);
            assert_eq!(strings, vec![s.to_string()], "roundtrip failed for len={}", s.len());
        }
    }

    // ── decompress passthrough (non-zstd, non-yaz0) ──

    #[test]
    fn test_decompress_passthrough() {
        let data = b"hello world";
        let result = decompress(data).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn test_decompress_yaz0() {
        // Use roead to create a roundtrip yaz0 test
        let original = b"Hello, this is some test data for yaz0 compression!";
        let compressed = roead::yaz0::compress(original);
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, original);
    }

    // ── filename_stem ──

    #[test]
    fn test_filename_stem() {
        assert_eq!(filename_stem(Path::new("Msg_EUfr.product.sarc")), "Msg_EUfr.product");
        assert_eq!(filename_stem(Path::new("/some/path/file.json")), "file");
        assert_eq!(filename_stem(Path::new("no_ext")), "no_ext");
    }

    // ── is_sarc edge cases ──

    #[test]
    fn test_is_sarc_too_short() {
        assert!(!is_sarc(b"SARC"));  // only 4 bytes, needs > 0x20
    }

    // ── from_json_to_cbor basic structure ──

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
        // Should start with zstd magic bytes
        assert_eq!(&result[0..4], [0x28, 0xB5, 0x2F, 0xFD]);
        // Verify we can decompress it (try dict first, then fallback)
        let decompressed = zstd_decompress(&result[..]).unwrap();
        // Extract CBOR text strings to find our embedded JSON
        let cbor_strings = extract_cbor_strings(&decompressed);
        let all_text: String = cbor_strings.join(" ");
        assert!(all_text.contains("Mergeable"));
        assert!(all_text.contains("MessagePack"));
        assert!(all_text.contains("Hello"));
        assert!(all_text.contains("group_count"));
        assert!(all_text.contains("entries"));
    }
}
