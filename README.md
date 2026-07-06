# ukmm-extractool

**Extract and rebuild UKMM mod files to/from editable YAML and native BYML.**

[![MIT Licence](https://img.shields.io/badge/licence-MIT-blue.svg)](LICENSE)

A CLI tool that handles every format found in [UKMM](https://github.com/NiceneNerd/UKMM) mods:

| Source | Input | Output |
|--------|-------|--------|
| **UKMM** `.zip` | `Message/Msg_*.product.sarc` → structured `.yaml` (Msyt entries) |
| | `Actor/*.byml` (mergeable CBOR) → `.sbyml` (native Nintendo BYML) |
| | `Actor/ActorInfo.product.byml` → `.sbyml` (Actors/Hashes arrays) |
| | Other `.byml` files → `.sbyml` (if roead format) or `.yaml` fallback |
| **BCML** `.bnp` | `logs/texts.json` + `logs/actorinfo.yml` → editable workspace |

---

## Get the tool

Download the latest `ukmm-extractool.exe` (Windows) or `ukmm-extractool` (Linux/MacOS)
from the [Releases page](https://github.com/Kalemillion/ukmm-extractool/releases).

Portable — no installation needed, just run the binary.

---

## Usage

```bash
ukmm-extractool.exe
```

### UKMM mods — Wii U / Switch

1. Pick your platform — **Wii U** (1) or **Switch** (2)
2. The tool scans your UKMM mods directory (`%LOCALAPPDATA%/ukmm/{wiiu,nx}/mods/`)
3. Pick a mod from the list
4. Converts all mod files:
   - `Message/*.sarc` → structured `.yaml`
   - `*.byml` (roead format) → native `.sbyml` (edit with TotkBits)
   - `*.byml` (other) → editable `.yaml`
5. Original mod ZIP is backed up as `<mod_name>_backup.zip`

**Rebuilding:** Run again, pick the same mod, choose **[1] Rebuild**.
Edited `.sbyml` and `.yaml` files are converted back and injected into the ZIP.

**Restore:** Pick **[3] Restore original (from backup)** to undo all edits.

### BCML `.bnp` files

1. Pick **3 — Load a .bnp file**
2. Drag & drop or type the path to a `.bnp` file
3. `logs/texts.json` and `logs/actorinfo.yml` are extracted
4. Everything lands in `mods/<platform>/<mod_name>/`

**Rebuilding:** Same mod → **[1] Send edited files into BNP**.

---

## Output .yaml example

```yaml
entries:
  Animal_Cat_A_Name:
    contents:
    - text: Homestead Munchkin
  Animal_Cat_A_PictureBook:
    contents:
    - text: |-
        This feline creature can be found lazing
        about in most Hylian settlements. They
        are slow and are often found snacking on
        discarded fish. Although they are now
        domesticated, it is said that in the distant
        past cats were known to be highly
        intelligent and communicate with other
        animals. Some variants are also friendly
        enough that they don't mind being held.
```

---

## Building from source

```bash
git clone https://github.com/Kalemillion/ukmm-extractool.git
cd ukmm-extractool
cargo build --release
```

Binary at `target/release/ukmm-extractool.exe`.

### Development

```bash
cargo test                     # 29+ unit tests
cargo clippy -- -D warnings    # Lint (must pass CI)
cargo fmt -- --check           # Formatting (rustfmt defaults)
cargo deny check               # Supply-chain audit
```

---

## Licence

MIT — see [LICENSE](LICENSE).
