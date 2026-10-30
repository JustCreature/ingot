# Ingot

A fast RAW + JPEG photo importer and culling app, written in Rust. Choose where photos go, pick the card, and start choosing images right away while everything copies in the background. Finalize keeps your picks and takes back the rest.

> **Status: early.** The app works end to end, but it has only been tried on Canon CR2 and JPEG files. Real card imports and real Google Drive uploads haven't been run yet. See [Limits](#limits).

## What it does

- **Destination, then source.** Name the import and choose the destination and options. Choosing the card is what starts it: everything begins copying and the grid opens.
- **Pictures load in the order you see them.** First every thumbnail, then every preview, one by one from the first photo, and photos near where you jump to go first. A full-size image or RAW loads only when you zoom or develop.
- **Choose in the grid.** Pick (`P`) the images you want. Rejecting (`X`) holds a photo back from the copy until you change your mind. The copy is a background job: it only uses the card when nothing you are looking at needs it, and it reads in small chunks so it gets out of the way quickly.
- **Stop any time.** Stop import ends the copy without closing your photos. Finished copies stay unless you choose to send them to the Trash, nothing is left half written, and Resume copying picks up what's left.
- **Finalize.** Stops queued copies of what you didn't choose, finishes the rest, and moves copies this import made of unchosen photos to the Trash. It shows the list first, and you can keep unflagged photos too.
- **Clusters.** Similar photos are grouped automatically. A sensitivity slider goes from "same scene" to "same subject position" and regroups instantly.
- **Verified copies.** Every file is read from the card once, written to a temp file, checked by hash, and only then given its real name. An existing file is never overwritten.
- **Import options like Lightroom's.** Into a subfolder; one folder, original folders or by date; rename templates; duplicate skipping; second copies; filters; dry run.
- **Google Drive backup.** Uploads wait for Finalize and send only what you kept, using the local copy, because Ingot never deletes from Drive. It only ever works inside its own `__ingot_backups` folder.
- **RAW develop.** A scene-referred pipeline ported from [darktable](https://github.com/darktable-org/darktable), with Lightroom-style sliders.
- **Safe removal.** A copy only goes with proof that this import wrote it, only if it is unchanged, and only to the Trash. Files that were already there, and the source, are never touched.

## Install

Requires macOS and a [Rust toolchain](https://rustup.rs). CI builds and tests on Apple Silicon and Intel; other systems are untested.

```sh
git clone https://github.com/JustCreature/ingot
cd ingot
make setup-local        # installs jpeg-turbo, cmake and nasm with Homebrew
```

## Run

```sh
cargo run --release                                  # open the app
cargo run --release -- /Volumes/EOS_DIGITAL/DCIM     # offer this card or folder as the source
```

Press `/` in the app to see the shortcuts.

| Keys | Action |
|---|---|
| `0`–`5` | Rating |
| `P` or `Space` / `X` / `U` | Pick / reject / clear flag |
| `Shift` + any of those | Also move to the next photo |
| `A` | Move on after a flag, on or off |
| Arrow keys, `Home`, `End` | Move |
| `G` / `L` / `E` or `Enter` / `D` | Grid / clusters / loupe / develop |
| `N` | Compare the selection |
| `Cmd`-click, `Shift`-click, `Cmd+A` | Select several; flags and ratings apply to all |
| `Z` | Zoom to 1:1 |
| `\` | Before and after |
| `Cmd+Shift+C` / `Cmd+Shift+V` | Copy / paste develop settings |
| `Cmd+Shift+I` | New import |
| `Tab` / `Esc` | Hide panels / back |

## Command line

```sh
# Import, with a second copy and a Drive backup
ingot import /Volumes/EOS_DIGITAL/DCIM --dest ~/Photos --subfolder Wedding \
  --rename '{custom}-{seq:4}' --custom-text Anna --second-copy /Volumes/Backup --gdrive

# See where each file would go, without changing anything
ingot import /Volumes/EOS_DIGITAL/DCIM --dest ~/Photos --organize date --dry-run

# Develop one RAW to a JPEG
ingot develop IMG_1800.CR2 out.jpg --exposure 1 --contrast 2 --wb daylight
```

Use `cargo run --release --` in place of `ingot` when running from the repo. `ingot help` lists every option. Google Drive needs one-time setup: see [docs/guides/google-drive.md](docs/guides/google-drive.md).

## Limits

- **Formats:** all 29 file types [rawler](https://github.com/dnglab/dnglab) reads are recognised, but only CR2 has been tried on real files. X-Trans and linear DNG can't be developed yet, though they import and copy.
- **Dates in the app** come from each file's modified time, which is known without opening it. On a card that is the capture time. A folder whose files were re-saved or copied without keeping their dates would be filed and grouped by the later date.
- **Videos** are skipped. The app and command line show how many files were left out.
- **Not built yet:** highlight reconstruction, denoise, lens correction, color balance, tone curve, HSL, crop, presets, collections and a catalog.
- **Develop look:** demosaic, tone curve and sharpening are darktable ports. Highlights, shadows, whites, blacks, saturation and vibrance are simpler versions of Ingot's own.
- **Sidecars** are written beside copies but aren't read back; ratings are restored from Ingot's own store. A sidecar from another program is left alone.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Architecture, repo layout and lessons learned are in [CLAUDE.md](CLAUDE.md). The plan is in [docs/planning/global-plan.md](docs/planning/global-plan.md).

## Licence

[GPL-3.0-or-later](LICENSE). See [NOTICE](NOTICE) for third-party material.
