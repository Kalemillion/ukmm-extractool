# ukmmsg2json

**Convert BotW message files from UKMM mods to editable JSON and back.**

Extracts text entries of *Breath of the Wild*, from UKMM's compressed CBOR message files format (`.sarc`). Edit the JSON, then convert back to a UKMM-ready `.sarc`.

---

## Workflow

### 1. Build

```bash
cargo build --release
```

### 2. Extract texts from a UKMM mod

```powershell
.\process_msg_mod.ps1 -ModsDir "$env:LOCALAPPDATA\ukmm\wiiu\mods" -ModName "MyMod"  # Wii U
.\process_msg_mod.ps1 -ModsDir "$env:LOCALAPPDATA\ukmm\nx\mods" -ModName "MyMod"    # Switch
```

Output: `mods/{wiiu|nx}/MyMod/Msg_{lang}.product.json` + `MyMod_backup.zip`

### 3. Edit the JSON

```json
{
  "entries": {
    "Animal_Cat_A_Name": {
      "contents": [
        {
          "text": "Homestead Munchkin"
        }
      ]
    },
    "Animal_Cat_A_PictureBook": {
      "contents": [
        {
          "text": "This feline creature can be found lazing\nabout in most Hylian settlements. They\nare slow and are often found snacking on\ndiscarded fish. Although they are now\ndomesticated, it is said that in the distant\npast cats were known to be highly\nintelligent and communicate with other\nanimals. Some variants are also friendly\nenough that they don't mind being held."
        }
      ]
    }
  }
}
```

### 4. Rebuild after editing

```powershell
.\process_msg_mod.ps1 -ReverseOnly -ModName "MyMod"                 # Wii U
.\process_msg_mod.ps1 -ReverseOnly -ModName "MyMod" -IncludeSwitch  # Switch
```

Output: `mods/{wiiu|nx}/MyMod/MyMod_modified.zip` — ready for UKMM. Just move into `$env:LOCALAPPDATA\ukmm\wiiu\mods` and replace the original (don't worry, you've a backup).

---

## CLI reference

```
Usage: ukmmsg2json <INPUT> [OPTIONS]

Arguments:
  <INPUT>  Input file (.sarc, .zst, or UKMM CBOR blob)

Options:
  -o, --output <PATH>      Output JSON path (default: stdout)
      --mod-dir <NAME>     Write to mods/{NAME}/
      --auto-dir           Write to mods/<input_filename>/
  -d, --decompress         Force zstd decompression
  -l, --list-languages     List entry names only
  -r, --reverse            Convert JSON back to UKMM .sarc (zstd+CBOR)
  -h, --help               Print help
```

---

## How it works

```
UKMM mod ZIP → zstd decompress (custom dict) → CBOR → JSON strings → Msyt entries → Output JSON

JSON input → Rebuild Msyt → CBOR (Mergeable::MessagePack) → zstd compress (level 8) → .sarc
```

Key dependencies: `zstd`, `roead`, `msyt`, `clap`, `serde_json`, `indexmap`.

The custom zstd dictionary (`data/zsdic`) is embedded at compile time and sourced from UKMM's `crates/uk-mod/data/zsdic`.

---

## License

GPL-3.0-or-later
