# ukmmsg2json

**Extract Zelda BotW message files from UKMM mods and BCML `.bnp` archives to editable JSON — and back.**

[![MIT Licence](https://img.shields.io/badge/licence-MIT-blue.svg)](LICENSE)

A CLI companion that converts game message data from:

| Source | Format | What's inside |
|--------|--------|---------------|
| **UKMM** `.zip` | `Msg_*.product.sarc` (SARC archive) |
| **BCML** `.bnp` | `logs/texts.json` inside a 7z archive |

---

## Get the tool

Download the latest `ukmmsg2json.exe` (Windows) or `ukmmsg2json` (Linux/MacOS)
from the [Releases page](https://github.com/Kalemillion/ukmmsg2json/releases).

Portable — 0 installation needed, just run the binary.

---

## Usage

```bash
ukmmsg2json.exe
```

### UKMM mods — Wii U / Switch

1. Pick your platform — **Wii U** (1) or **Switch** (2)
2. The tool scans your UKMM mods directory (`%LOCALAPPDATA%/ukmm/{wiiu,nx}/mods/`)
3. Pick a mod from the list
4. Each `Msg_*.product.sarc` is extracted and converted to JSON
5. Everything lands in `mods/<platform>/<mod_name>/`
6. The original mod ZIP is backed up as `<mod_name>_backup.zip`

**Rebuilding:** Run again, pick the same mod, choose **[1] Send .json into UKMM**.
The modified message file is rebuilt and injected back into the original ZIP.
All other mod files stay untouched.

**Restore:** Pick **[3] Restore original (from backup)** to undo all edits.

### BCML `.bnp` files

1. Pick **3 — Load a .bnp file**
2. Drag & drop or type the path to a `.bnp` file
3. The tool reads `info.json` (mod name, platform) and `logs/texts.json`
4. Choose output format:
   - **[1] Single `texts.json`** — BCML-compatible file with selected languages
   - **[2] Individual files** — one `Msg_<lang>.product.json` per language
5. A language picker lets you choose which languages to export (e.g. `1,3,5-7` or `all`)
6. Everything lands in `mods/<platform>/<mod_name>/`
7. The original `.bnp` is backed up as `<mod_name>_backup.bnp`

**Rebuilding:** Run again with the same `.bnp`, choose **[1] Send .json into BNP**.
The tool reconstructs `logs/texts.json` from your edited JSONs and re-packages
the 7z archive. A warning is shown if the original `.bnp` was moved.

---

## JSON format

```json
{
  "entries": {
    "ActorType/Prey": {
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
            "text": "This feline creature can be found lazing\nabout in most Hylian settlements."
          }
        ]
      }
    }
  }
}
```

---

## Building from source

```bash
git clone https://github.com/Kalemillion/ukmmsg2json.git
cd ukmmsg2json
cargo build --release
```

Binary at `target/release/ukmmsg2json.exe`.

### Development

```bash
cargo test                     # 27+ unit tests
cargo clippy -- -D warnings    # Lint (must pass CI)
cargo fmt -- --check           # Formatting (rustfmt defaults)
cargo deny check               # Supply-chain audit
cargo audit                    # Vulnerability scan
```

---

## Licence

MIT — see [LICENSE](LICENSE).
