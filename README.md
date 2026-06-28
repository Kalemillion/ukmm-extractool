# ukmmsg2json

**Convert BotW message files from UKMM mods to editable JSON and back.**

Extracts text entries of *Breath of the Wild* from UKMM's compressed CBOR message files (`.sarc`).
Edit the JSON, then convert back to a UKMM-ready `.sarc`.

---

## Quick start

### 1. Download

Grab the latest binary from the [Releases page](https://github.com/NiceneNerd/ukmmsg2json/releases).
**No Rust, CMake, or Visual Studio required** — just unzip and run.

| Platform | Archive |
|----------|---------|
| Windows | `ukmmsg2json-x86_64-pc-windows-msvc.zip` |
| Linux   | `ukmmsg2json-x86_64-unknown-linux-gnu.tar.gz` |
| macOS   | `ukmmsg2json-x86_64-apple-darwin.tar.gz` |

### 2. Extract texts from a UKMM mod

Run the tool **without arguments** to launch the interactive mod picker:

```powershell
.\ukmmsg2json.exe
```

It will ask you to pick a platform (Wii U / Switch), scan your UKMM mods folder,
list all available mods, and let you choose one. The tool then extracts the mod,
converts all `Msg_*.product.s*rc` files to JSON, and creates a backup ZIP.

Or use the CLI directly for a single file:

```powershell
.\ukmmsg2json.exe "Msg_EUfr.product.sarc" -o output.json
```

### 3. Edit the JSON

```json
{
  "Animal_Cat_A_Name": {
    "contents": [{ "text": "Mon nouveau texte" }]
  },
  "Animal_Cat_A_PictureBook": {
    "contents": [{ "text": "Nouvelle description..." }]
  }
}
```

### 4. Rebuild — JSON back to UKMM `.sarc`

After editing the JSON files, run the tool again in interactive mode.
It detects the existing output and offers the choice:

```
Output already exists. [1] Extract again  [2] Rebuild from edited JSONs
```

Select **2** to convert all JSONs back to `.sarc` and produce a `_modified.zip`
ready for UKMM — just drop it into your mods folder.

Or use the CLI directly:

```powershell
.\ukmmsg2json.exe "Msg_EUfr.product.json" -r -o "Msg_EUfr.product.sarc"
```

---

## CLI reference

```
Usage: ukmmsg2json [INPUT] [OPTIONS]

Arguments:
  [INPUT]  Input file (.sarc, .zst, .json for reverse).
           Omit to launch interactive mode.

Options:
  -o, --output <PATH>      Output JSON path (default: stdout)
      --mod-dir <NAME>     Write to mods/{NAME}/
      --auto-dir           Write to mods/<input_filename>/
  -i, --interactive        Launch interactive mod picker
  -d, --decompress         Force zstd decompression
  -l, --list-languages     List section names only
  -r, --reverse            Convert JSON back to UKMM .sarc (zstd+CBOR)
  -h, --help               Print help
```

---

## Build from source

```bash
cargo build --release
```

Requires [Rust](https://rustup.rs/), [CMake](https://cmake.org/), and a C++ compiler (VS Build Tools on Windows).

---

## How it works

```
UKMM mod ZIP → zstd (custom dict) → CBOR → JSON strings → Msyt entries → Output JSON
JSON input → Msyt rebuild → CBOR (Mergeable::MessagePack) → zstd → .sarc
```

The custom zstd dictionary (`data/zsdic`) is embedded at compile time and sourced from UKMM's `crates/uk-mod/data/zsdic`.

---

## License

GPL-3.0-or-later
