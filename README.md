# ukmmsg2json

**Convert the `Msg_*.product.sarc` inside a UKMM mod to editable JSON — and back.**

[![MIT Licence](https://img.shields.io/badge/licence-MIT-blue.svg)](LICENSE)

A CLI companion that extracts the single `Msg_*.product.sarc` from a
[UKMM](https://github.com/NiceneNerd/ukmm) mod so you can edit the game's
messages in any text editor, then rebuilds it back into a working UKMM mod.

---

## Get the tool

Download the latest `ukmmsg2json.exe` (Windows) or `ukmmsg2json` (Linux/MacOS)
from the [Releases page](https://github.com/Kalemillion/ukmmsg2json/releases).

It's portable! 0 installation needed — just put the binary anywhere and
double-click it.

---

## Usage

Run the program — that's it. No prerequite or external dependency.

```
ukmmsg2json.exe
```

### What happens

1. You pick a platform — Wii U (1) or Switch (2)
2. It scans your UKMM mods and lists them
3. You pick a mod
4. It extracts the `Msg_*.product.sarc` from the ZIP and converts it to JSON
5. Everything lands in `mods/<platform>/<mod_name>/`
6. The original mod is backed up as `<mod_name>_backup.zip`

### Editing & rebuilding

1. Edit the `.json` file in `mods/<platform>/<mod_name>/` with any text editor
2. Run the program again, pick the same mod
3. Choose **[1] Send .json into UKMM**
4. The modified mod is rebuilt and copied straight into UKMM, overwriting the original

Need to start over? Pick **[3] Restore original (from backup)** to put the
original mod back.

The rebuild keeps every other file in the mod untouched; only the message
file is replaced.

---

## JSON format

Here's what the output looks like:

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

| Field | What it is |
|-------|------------|
| `entries` | Section name → messages (order is preserved) |
| `language` | *(optional)* 4-letter language code — `USen`, `EUfr`, `JPja`… |
| `entry_count` | *(optional)* Number of message entries — validated if present |
| `format` | *(optional)* `"SARC"` or `"UKMM CBOR"` — just for reference |

By default the generated JSON only contains `entries`. The other fields are
accepted on rebuild but not required.

Each message entry contains:

| Field | What it is |
|-------|------------|
| `attributes` | Optional metadata string from the game (`null` if absent) |
| `contents` | The actual message — text and control tags mixed together |

Control tags handle formatting inside messages: `SetColour`, `ResetColour`,
`Pause`, `Icon`, `Variable`, `Choice`, `SingleChoice`, `Sound`, `Animation`,
`TextSize`, `AutoAdvance`, `Localisation`, `Font`, and `Bin` (unknown codes).

> If `entry_count` is present in the JSON but doesn't match the real number
> of entries, the rebuild will refuse to run — it's a safety check against
> corrupted edits.

---

## Building from source

Only needed if you want the latest unreleased changes or prefer compiling
yourself.

```bash
git clone https://github.com/Kalemillion/ukmmsg2json.git
cd ukmmsg2json
cargo build --release
```

The binary appears in `target/release/`.

Development commands:

```bash
cargo test                  # 26 unit tests
cargo clippy -- -D warnings # Catch common mistakes
cargo deny check            # Licence & security audit
cargo audit                 # Vulnerability scan
```

---

## Licence

MIT — see [LICENSE](LICENSE).
