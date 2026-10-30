# Ingot

A fast RAW + JPEG photo importer and culling app, written in Rust. Insert a card and start rating right away while your photos copy to local disks and Google Drive in the background.

> **Status: early.** The app works end to end, but it has only been tried on Canon CR2 and JPEG files. Real card imports and real Google Drive uploads haven't been run yet. See [Limits](#limits).

## What it does

- **Cull while copying.** Ratings and flags never wait on the copy. Preview reads go ahead of copy reads on the card.
- **Verified copies.** Every file is read from the card once, written to a temp file, checked by hash, and only then given its real name. An existing file is never overwritten.
- **Import options like Lightroom's.** Into a subfolder; one folder, original folders or by date; rename templates; duplicate skipping; second copies; filters; dry run.
- **Google Drive backup.** Uploads carry on after the card is finished, using the local copy. Ingot only ever works inside its own `__ingot_backups` folder and has no code that deletes from Drive.
- **RAW develop.** A scene-referred pipeline ported from [darktable](https://github.com/darktable-org/darktable), with Lightroom-style sliders.
- **Safe cleanup.** Rejected copies go to the Trash, only with proof that they are verified copies. The source is never touched.

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
cargo run --release -- /Volumes/EOS_DIGITAL/DCIM     # open it on a card or folder
```

Press `/` in the app to see the shortcuts.

| Keys | Action |
|---|---|
| `0`–`5` | Rating |
| `P` / `X` / `U` | Pick / reject / clear flag |
| `Shift` + any of those | Also move to the next photo |
| `A` | Move on after a flag, on or off |
| Arrow keys, `Home`, `End` | Move |
| `G` / `E` or `Enter` / `D` | Grid / loupe / develop |
| `Z` | Zoom to 1:1 |
| `\` | Before and after |
| `Cmd+Shift+C` / `Cmd+Shift+V` | Copy / paste develop settings |
| `Cmd+Shift+I` | Import |
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
