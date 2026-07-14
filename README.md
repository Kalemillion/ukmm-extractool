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

---

## Get the tool

Download the latest `ukmm-extractool.exe` (Windows) or `ukmm-extractool` (Linux/MacOS)
from the [Releases page](https://github.com/Kalemillion/ukmm-extractool/releases).

Portable — no installation needed, just run the binary.

---

## Usage

### Interactive mode (no arguments)

```bash
ukmm-extractool
```

Launches an interactive menu:
```
  ❯ Extract a mod         Extraire un mod
    Rebuild a mod         Reconstruire un mod
    Restore a mod         Restaurer un mod
    List available mods   Lister les mods disponibles
    Information           Informations
    Quit                  Quitter
```

Bilingual (EN/FR) — auto-detected from system locale. Override with `UKMM_LANG=fr` or `--lang fr`.

### CLI subcommands

```bash
# Extract a UKMM mod to the workspace
ukmm-extractool extract "C:\Users\...\ukmm\wiiu\mods\MyMod.zip"

# Rebuild a mod from edited files (from workspace directory)
cd mods/wiiu/MyMod
ukmm-extractool rebuild

# Restore original from backup
ukmm-extractool restore mods/wiiu/MyMod

# List all UKMM mods
ukmm-extractool list
```

A bare file path also works (legacy auto-detect):
```bash
ukmm-extractool ActorInfo.product.byml
```

### Workflow

1. Run without arguments → interactive menu
2. Choose **Extract a mod** → pick a mod from the list (Wii U 🩵 / Switch 🔴)
3. All resource files are converted:
   - `Message/*.sarc` → structured `.yaml`
   - `ActorInfo.product.byml` → native `.sbyml` (Actors/Hashes arrays)
   - Mergeable `*.byml` (roead format) → native `.sbyml`
   - Other `*.byml` → editable `.yaml`
   - `*.bactorpack` / `*.bfarc` → `.yaml` (SarcMap format)
4. Original mod ZIP is backed up as `<mod_name>_backup.zip`

**Rebuilding:** Run again, pick the same mod, choose **Rebuild**.
Edited `.sbyml` and `.yaml` files are converted back to CBOR and injected into the ZIP.

**Restore:** Pick **Restore** to undo all edits from the backup.

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
cargo test                     # 35+ unit tests
cargo clippy -- -D warnings    # Lint (must pass CI)
cargo fmt -- --check           # Formatting (rustfmt defaults)
cargo deny check               # Supply-chain audit
```

---

## Licence

MIT — see [LICENSE](LICENSE).
