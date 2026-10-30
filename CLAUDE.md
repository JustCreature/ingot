# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Error-handling pattern (project-wide)

**Fallible functions return `Result`, never a bare `Option` that swallows the cause.** A `None` throws away *why* something failed; errors are data we need for observability, and some must be surfaced to the UI. So:

- The standard engine error type is `Box<dyn std::error::Error + Send + Sync>` (aliased per module, e.g. `PreviewError`, `MetadataError`, `ImageProcessingError`). `Send + Sync` is mandatory — errors travel across the ingest worker threads and are stored in the engine-owned asset.
- Propagate with `?` (turbojpeg/fir/io errors auto-convert); add context when collapsing a typed error to a string (`format!("preview generation failed: {e}")`).
- **Record, don't drop.** A failed asset still streams a message; its errors are collected into `PhotoAsset.image_data.errors` so the batch never aborts (one bad frame must not kill a 10k-card ingest) *and* the failure remains inspectable.
- `Option` is still correct for genuine *absence* (e.g. a frame has no embedded thumbnail), not for *failure*.

## What this project is

**Ingot** — a high-performance RAW+JPEG photo ingestion, triage and develop app, written in **Rust**. It competes with Photo Mechanic / FastRawViewer / Lightroom: ingest a camera card fast, let the photographer rate and cull *immediately* while files replicate to multiple storage targets in the background.

## Hard rules

Requirements from the project owner. None of these may be weakened without asking.

- **Never overwrite an existing file** on a target. Temp files and Ingot's own sidecars and state are the only exceptions.
- **Google Drive: only ever inside `__ingot_backups`.** The folder name is a constant with no setting, parents are the `ContainedFolder` type, and no delete, move, rename or update call may exist in that code. Never commit an OAuth client: the repo is public.
- **Culling never waits on copying.** No lock held across I/O; preview reads go first on the card; a backed-up copy must not hold a card slot.
- **Never touch the source card** when deleting or writing sidecars. Source deletion is out of scope for v1.
- **Don't unmount or touch the user's device programmatically** for benchmarks. Ask them to run `sudo purge` or re-seat the card. (`--eject` runs only when the user asks.)

## Repo structure

```
Cargo.toml               workspace root and the `ingot` binary package
src/
  main.rs                entry point, CLI commands
  cli.rs                 argument parsing and help text
  ui/
    mod.rs               window setup, AppPaths
    app.rs               everything drawn, and the background work it starts
    library.rs           culling logic and the key map; no drawing, so testable
    images.rs            JPEG decoding off the UI thread, LRU texture cache
crates/ingot_core/       engine library, no UI dependencies
  src/
    engine.rs            Engine: scan, ingest (previews), start_import, read_full_jpeg
    scan.rs              walk and pair files into assets, classify extensions
    asset.rs             PhotoAsset, AssetKey, TriageState
    metadata.rs          bounded EXIF header read, embedded preview location
    preview.rs           turbojpeg decode, resize, encode
    import.rs            ImportOptions, rename templates, plan_import, ImportHistory
    triage.rs            TriageStore (append-only JSONL), XMP sidecars
    sweep.rs             plan_sweep / execute_sweep: rejected copies to the Trash
    route.rs             Target configuration
    replicate/
      mod.rs             CopyEngine, ReplicationTarget, VerifiedReplica, claims
      gate.rs            CardGate: shared card reads, previews first
      local.rs           LocalTarget: temp, verify, no-replace publish
      drive.rs           DriveTarget: pacer, resumable upload, ContainedFolder
      drive_auth.rs      desktop sign-in (PKCE), token refresh
  examples/              bench_enrich, bench_preview, bench_ingest, bench_read_threads, bench_replicate
  testdata/test_exif_read/  real Canon CR2 and JPEG fixtures
crates/ingot_develop/    RAW develop engine, darktable ports marked in each file
  src/
    sensor.rs            rawler decode, raw prepare (black, white, crop)
    demosaic.rs          RCD, port of darktable's rcd.c
    sigmoid.rs           tone curve, port of darktable's sigmoid.c
    sharpen.rs           unsharp mask, port of darktable's sharpen.c
    color.rs             color matrices, sRGB
    pipeline.rs          DevelopParams, develop, PreviewBase (fast slider path), orientation
  examples/develop.rs    timing harness
docs/
  planning/global-plan.md  the architectural plan; read it first
  guides/google-drive.md   Drive setup
  design/                  UI design; use html_Ingot_v2.html, ingot_v2/ holds the source
  sessions/, lessons/      history from earlier phases
.github/workflows/ci.yml   fmt, clippy, build and test on Apple Silicon and Intel
```

## Commands

```sh
cargo run --release                     # the app
cargo run --release -- import --help    # (or `help`) the command line
cargo test --workspace                  # the develop tests take about a minute in debug
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
INGOT_SHOTS=<dir> cargo test --bin ingot render_views -- --ignored   # render each view to PNG
cargo test -p ingot_core really_moves -- --ignored                   # adds one file to the real Trash
cargo run --release -p ingot_core --example bench_replicate -- <source> <target_root> [secondary_root]
cargo run --release -p ingot_develop --example develop -- <raw> <new.ppm>
```

Build tools: `make setup-local` (jpeg-turbo, cmake, nasm). PRs need CI green: fmt, clippy and tests on both Mac architectures.

## Learnings

Mistakes actually made on this project, and what they taught. Add to this list when something goes wrong.

**Data safety**
- An early `LocalTarget` called `remove_file(dst)` when verification failed, which could delete a good file already there. Verify the temp file *before* giving it the real name, and have no code path that removes a destination.
- Routing by `file_stem` gave a RAW and its JPEG the same destination, so one replaced the other. Keep the real file name and test a RAW+JPEG pair.
- `rename` silently replaces an existing file, even after a check. Publish with `hard_link`, which fails atomically; exFAT has no hard links, so fall back to check-then-rename there.
- Treating "destination exists" as a failure blocked re-runs and lost photos that merely shared a name. Compare by hash: same is done, different gets `-2`.
- A file the scanner displaced in a collision was silently never copied. When a list decides what gets backed up, check what falls off it, and report skipped files.
- Importing a folder into itself makes a "copy" that *is* the source. The sweep compares canonical paths for that reason.
- Copying loads each whole file into RAM, which is fine for photos and dangerous for videos. Don't add videos before streaming copy exists.

**Concurrency**
- Holding a card read permit while blocked on a full channel starves preview reads, which blocks culling. Hold a permit only for the read. A separate slot provides the backpressure.
- Buffer slots released on channel send were counted on top of the channel's own capacity, so more files sat in RAM than documented. A test that counts reads against the stated bound caught it. Test limits, don't just state them.
- Spawning a store task per message drains the channel into tasks queued on a semaphore, each holding a file. That removes both backpressure and the RAM bound. Cap in-flight tasks before spawning.
- `Engine::scan` once held the write lock through 14 s of card I/O. Build outside the lock, then swap.
- Saving and syncing a sidecar on every slider tick would stall a drag. Render on every tick and save on release.
- Two source files can route to one name. Claims must *wait*, not skip to the next name, or a re-run writes duplicates of itself.
- Hash on one thread: a parallel hash takes CPU from preview generation and gains nothing against a slow disk.

**Measuring and verifying**
- A benchmark sweep was invalid because zsh's `set -- $cfg` doesn't split words, so every run used the same settings. Print the parameters a run actually used, and distrust identical results.
- Synthetic numbers on an internal SSD say nothing about a card. Always state what was measured on.
- Green tests are not enough for image code. All 26 develop tests passed while photos came out sideways, because `rawler` reports `Normal` orientation for CR2 and the real value is only in EXIF. Look at real output, and compare against the camera's own JPEG of the same shot (there is a test for this).
- `rtk` filters test output, and can hide failures and counts. Use `rtk proxy cargo test` when the result matters.
- `tail` on `cargo add` hid a failed add. Check the manifest afterwards. Feature names also change between versions (`rustls-tls` became `rustls`, and `query` and `form` became features in reqwest 0.13).
- Python `str.replace` edits silently do nothing once `cargo fmt` has reflowed the code. `assert old in s` first.
- A fake `HOME` breaks rustup. Build first, then set `HOME` only for the built binary.
- This environment can't capture the screen, so `render_views` draws each view to PNG instead. Headless runs don't prove how the app feels to use.

**UI (egui)**
- The default font has no `★`, `⌘`, `←` or `⚠`. In the real app these become boxes; the test harness panics on them, which is how it was found. Draw shapes or use words.
- Windows need several frames before they appear in tests (a sizing pass comes first), and a window's title can have the same label as its button.
- Don't assume a texture fits: 6240 px exceeds the 2048 limit seen in tests. `fit_texture` shrinks to the real maximum.
- Keep the context from the current frame. A context stored at startup was not the one the harness ran.
- Portrait previews need the EXIF orientation applied: embedded JPEGs aren't rotated, and re-encoding drops the tag.

**Google Drive**
- Drive allows duplicate names, so two uploads can create two folders with one name. Resolve folders under a lock and cache the ids.
- Pacer state must start a full burst in the past, or the first calls of a run are throttled for nothing (a test caught this).
- The access token goes to the upload address, so check that its host is the expected one.
- The mock server only behaves the way we assumed Google does. Only a real account proves it.

**Licensing and sources**
- Check the licence and read the README before planning around a project. `lightcraft` was described as a decompile, and its own docs say clean-room MIT/Apache. darktable's GPL-3 forced a real decision: a Rust rewrite is still a derivative, so Ingot became GPL-3.
- Port with attribution in each file and in `NOTICE`, and say which parts are ports and which are Ingot's own.

## Licence

GPL-3.0-or-later (MIT until commit 6bc36c7). `crates/ingot_develop` ports darktable code. Keep the attribution header in ported files and update `NOTICE`.

## Working on this repo

- Implement changes directly: write code, run builds and tests, and verify the result.
- **Measurement over intuition.** Performance questions are settled with benchmarks (`crates/ingot_core/examples/`), not guesses. Optimal I/O concurrency is a device property.
- Phase status, artifacts, and measured numbers are tracked in the `## Status` section of this file.

## Architecture invariants (from the plan)

These are deliberate design decisions, not suggestions. Preserve them in any implementation:

- **Asset pairing is keyed on `(dir, stem)`, case-normalized — never stem alone.** Canon rolls `IMG_9999 → IMG_0001`, and separate folders/cards reuse names; a flat stem-keyed map silently merges unrelated frames.
- **Zero RAW decoding for previews.** Previews come from the parallel `.JPG` or the JPEG embedded in the RAW — never from decoding RAW pixel data.
- **Copy-verify-then-delete, gated at the type level.** Replication copies and verifies (size + BLAKE3) first. Deletion is a separate user-triggered sweep that is *structurally unable* to run on a source file without a `VerifiedReplica`. Default to trash, not hard delete.
- **Read-once, fan-out copy.** Read each source file once and fan out to all targets; never re-read per target. Write to temp, then atomic-rename.
- **Two semaphore tiers.** Cap source-card *reads* (1–2) separately from per-target *writes*. Concurrency only helps across *distinct physical devices* — saturating one card/disk is a device property, not a thread-count property.
- **Streaming UI.** Each photo streams to the grid the instant it is processed; the UI never waits for a batch. At scale this needs a virtualized grid + LRU texture cache (the 24 GB RGBA problem) and a 1:1 loupe view for focus checking.
- **Ratings → XMP sidecars, never EXIF, never pixel rewrites.** There is no standard EXIF rating tag; write `xmp:Rating` (`Rejected → -1`, else `0..5`) in one `IMG_1.xmp` shared by a RAW and JPEG of the same stem, beside the copies. Keep stars (0–5) and accept/reject as separate internal concepts.
- **Crate roles are split deliberately:** `kamadak-exif` reads header metadata only; `rawler` decodes sensor data and finds previews the CR2 fast path can't; turbojpeg handles JPEG. Prefer `rusqlite` over `sled` if a catalog is added; store preview blobs as files on disk with paths in the DB. Size Rayon pools with `std::thread::available_parallelism()`.

## Two scoping decisions — RESOLVED 2026-06-08

1. **RAW-only frames (no parallel JPEG): IN SCOPE.** A `PhotoAsset` with only a RAW is a first-class asset, never an edge case. Adds a RAW-aware crate (`rawler`/`rawloader` or `libraw`) for full-size embedded-preview extraction in Phase 2.
2. **Source-card deletion: OUT OF SCOPE for v1.** To be exposed later as a separate, explicitly-warned button. The normal delete function never touches the source card — it operates only on *copied targets*, gated on `VerifiedReplica` + rejected-state.

## Status

**Current state (2026-10-30):** the app works end to end: import, cull, develop, trash rejected copies. Phases 3 and 4 are built apart from what "Not built" lists. Waiting on the owner's first hands-on test. 138 tests, plus 2 ignored ones that need a GPU or touch the Trash.

**Built (`crates/ingot_core/src/replicate/`):** `ReplicationTarget` trait, `LocalTarget`, `VerifiedReplica`, `CopyEngine::replicate`, `ReplicationReport`; `examples/bench_replicate.rs`. Details in global-plan Phase 3 Step 2b. 

**Measured (40 × 30 MB synthetic files, warm cache, source and target on the same internal SSD; not a card number):** read=2, write=4: depth 1 → 247 MB peak, 4 → 305 MB, 16 → 546 MB, with throughput 450–570 MB/s and no trend. Depth controls RAM, not speed, so 4 is the default in the bench.

**Import options (`import.rs`, `plan_import`):** subfolder; organize into one folder, by original folders, or by date (any chrono format); rename templates (`{name} {folder} {custom} {filenum} {seq:N} {date:FMT}`) with start number and extension case; selection by kind, date range, or hand-picked paths; suspected-duplicate skip (name + capture time + size) from an append-only history; second copies; include-subfolders; eject; add-only; dry run. CLI: `cargo run -- import --help`. All targets share one relative path.

**Copy decisions (2026-10-30):** an identical file on a target counts as done; a different file under one name is kept and the new one gets `-2`; undated frames use the file date; a history entry is only written once a file is on every target; a waiting preview read goes first on the card (`CardGate`).

**Google Drive (`replicate/drive.rs`, `drive_auth.rs`):** secondary target, modelled on rclone's backend (HTTP/1.1, 4 transfers, pacer with truncated exponential backoff, resumable sessions, folder-id cache). The whole file is one resumable PUT because it is already in RAM. Verified by BLAKE3 of the bytes against the card, then Drive's MD5 against the bytes. **Hard rule: Ingot only touches `__ingot_backups` in My Drive.** It is a constant with no setting, parents are the `ContainedFolder` type, and no delete/move/update call exists. Tested against a local mock server plus one real TLS round trip to Google's token endpoint that was correctly refused. **Never yet run with a real Google account.** Setup: `docs/guides/google-drive.md`.

**Develop engine (`crates/ingot_develop`, 2026-10-30):** ports darktable's scene-referred order: raw prepare, white balance, clip highlights, RCD demosaic, exposure, color matrix to Rec.2020, sigmoid, sRGB. Decoding is rawler. `ingot develop <raw> <out.jpg>`. Measured on 24 MP Canon CR2, i7-1068NG7: decode 0.35 s, develop 1.5 s, 560 MB peak. Bayer only: X-Trans and linear DNG return an error. Only CR2 has been tried on real files. Remaining darktable work: highlight reconstruction, PPG border, X-Trans, denoise, sharpening, color balance, lens correction, and more.

**App (`src/ui/`, 2026-10-30):** `cargo run --release` opens it. Grid, loupe with 1:1, develop with Lightroom-style sliders, import dialog, trash sweep, Lightroom hotkeys (`/` lists them). `INGOT_SHOTS=<dir> cargo test --bin ingot render_views -- --ignored` renders each view to PNG. Sidecars are written beside copies only, never on the source; decisions live in `triage.jsonl` in the app data folder.

**Not built:** highlight reconstruction, denoise, lens correction, color balance, tone curve, HSL, crop, X-Trans and linear DNG develop, video import, saved presets, collections, catalog. Sidecars aren't read back on open. Untested for real: card imports, Google Drive, RAW formats other than CR2. Highlights, shadows, levels, saturation and vibrance are Ingot's own simple versions, not darktable ports.

**Phase-2 step-4 measured numbers (879-asset Canon card, 8 cores):** scan+enrich cold 14.4 s (61 fps) / warm 0.07 s; ingest cold **154 s (6 fps)** / warm **~26 s (33 fps)**; read-only cold **2≈8 threads ≈ 158 s ≈ 20 MB/s**. **Read-bound on card** (cold≫warm 6×, cold-ingest≈cold-read, 2≈8). Card ceiling is device-fixed; concurrency can't beat it → cap `card_read_permits` 1–2 (frees CPU threads, zero throughput cost). Only cold lever = fewer bytes (embedded-for-pairs ✓). On SSD/NVMe → flips CPU-bound (~33 fps). Streaming = time-to-interaction ~14 s, not 154 s.

---

### Phase 1 — Asset Pairing & Core Data Structures — ✅ COMPLETE (2026-06-11)

**Objectives (met):**
- Why `(dir, stem)` case-normalized is the only safe asset key (filename rollover, multi-folder DCIM, multi-card collisions).
- Modeling RAW/JPEG presence so the empty asset is unrepresentable, with RAW-only as a first-class asset.
- A pure, I/O-free date-based ISO routing function.

**Artifacts:**
- `src/lib.rs` — asset model + scanner + routing. Public API: `scan_source_dir -> ScanResponse { assets, collisions, errors }`; `build_destination_path(&Target, NaiveDate, &Path) -> Option<PathBuf>`; types `FileKind`, `AssetKey {dir, stem}`, `AssetFiles` (non-empty, `new`/`insert`/`get`), `PhotoAsset`, `TriageState`, `Collision`, `Target`/`TargetKind`.
- `docs/sessions/phase-1/main-session.md` — session log (3 sessions).
- Deps: `walkdir`, `chrono`; dev: `tempfile`. On-disk fixture `photodata/testing/source/` retained but tests use programmatic tempdir fixtures.

**Key numbers (measured):** assets 11 (7 paired / 2 RAW-only / 2 JPEG-only) ✓ | same-kind collisions surfaced 1 ✓ | routing path exact-match ✓ | `cargo test` **3 passing** | `cargo clippy` clean | naive stem-key counterfactual = 8 (cross-folder dups proven distinct, 11≠8).

**Lessons:**
- The asset *identity* is fully recoverable from the path (`(dir, stem)`), case-normalized — no need to read file contents; a timestamp key is both unavailable at scan time and non-unique (1s EXIF resolution merges bursts).
- Making illegal states unrepresentable (non-empty `AssetFiles`, no `Default`; `_opt` date constructor) beats runtime validation, and is consistent with Phase 3's planned `VerifiedReplica`.
- Batch anomalies (duplicate-kind collisions, walkdir errors) are *data to collect and return*, not errors that abort the scan — one bad frame must never kill a 10k-card ingest.
- Pure functions (`build_destination_path`) decouple cleanly: take the date as a param now, let Phase 2 supply `DateTimeOriginal`.

**Known open items carried forward:**
- `expect("path stem error")` on `file_stem()` is the lone remaining panic in the scan path (defensible; could route to `errors` for full resilience).
- `AssetFiles` backed by `Vec` linear scan — fine at current N; interface (`get`/`insert`) hides it so it stays swappable/measurable.

---

### Phase 2 — CPU-Bound Processing & Streaming — ✅ COMPLETE (finalised 2026-06-14, `--v--FORCE`)

**Objectives (met):**
- I/O-bound vs CPU-bound, and why parallelism helps one and hurts the other (measured both directions, both flip by device).
- Reading *only the bytes you need*: bounded EXIF header read + targeted embedded-preview seek-read (vs whole-file slurp).
- Streaming ingest with **engine-owned state** + correct lock discipline (no lock held during heavy work).
- End-to-end measurement proving the pipeline is **read-bound on the card** (~20 MB/s, device-fixed).

**Artifacts:** `crates/ingot_core/src/` — `metadata.rs` (`read_exif_header` 128 KiB prefix, `read_embedded_preview` seek, `enrich_assets`), `preview.rs` (`make_preview`/`make_thumbnail`, `Result`-based), `engine.rs` (`Engine`/`EngineConfig`/`IngestEvent`/`read_source_bytes`, `Arc<RwLock<ScanResponse>>` streaming worker), `asset.rs` (`ImageData`, `PreviewStrip`, error aliases). Benches: `bench_enrich`, `bench_preview`, `bench_ingest`, `bench_read_threads`. Reports: `docs/lessons/phase-2/{long,short}-report.md`. `cargo test --workspace` 11 green, clippy clean.

**Key numbers (measured, 879-asset Canon card, i7-1068NG7 4P/8L):** enrichment cold serial 4.85 s (181 fps) vs parallel 6.20 s (−28% on SD; +3.7× on SSD — device property). Preview parallel scaling 4.26× (1→8 threads). Scaled decode 3/8 = 2.7× over 1/1. Header read 128 KiB vs 36 MB slurp = 264× faster. **End-to-end ingest: cold 154 s (6 fps) / warm ~26 s (33 fps); read-only 2≈8 threads ≈ 20 MB/s — read-bound, three ways.**

**Lessons (proved by measurement):**
- Optimal source-read concurrency is a **device property**, not a constant; the card is a fixed ~20 MB/s pipe you can't parallelize past (2≈8 threads) → cap `card_read_permits` 1–2.
- Cold ingest ≈ cold read-only → CPU is hidden under I/O; CPU floor ~33 fps, and an SSD source flips the pipeline CPU-bound. The only cold lever is fewer bytes (embedded-for-pairs).
- Engine-owned `Arc<RwLock>` + background worker is only correct with strict lock scope; holding the write lock across the loop kills streaming *and* deadlocks the bounded channel.
- Errors are data: `Result<_, Box<dyn Error + Send + Sync>>`, recorded not swallowed.

**Open items carried forward (deferred-with-rationale / next-phase):** (1) two-tier read/CPU split — ✅ **DONE 2026-06-14** (see Phase-2 addendum below); (2) previews-to-disk vs in-RAM — Phase 4; (3) EXIF `Orientation` — display-time; (4) per-thread `Decompressor` reuse; (5) multi-format embedded extraction (`rawler`/`libraw`); (6) `main.rs` demo cleanups (`ingot_bench_preview.JPG`, `tx.send().expect`→graceful, lock-poison policy, Arc-wrap previews); (7) **Phase 6** (auto-detect/auto-start, per-device parallel, loupe prefetch).

#### Phase-2 addendum — two-tier read/CPU split — ✅ DONE (2026-06-14)

**Motivation (M4 Pro data, 14 cores, same 879-asset card, merged single-pool, RAYON_NUM_THREADS sweep):** ingest 1 thread 136 s (6 fps) · **2 → 60 s (15 fps)** · **4 → 53 s (17 fps, best)** · 8 → 55 s (16 fps) · 14 → 60 s (14 fps). So the M4's card path has **read queue depth ~4** (1→4 = 2.6×, then >4 *regresses*) — unlike the Intel SD reader (serial, 2≈8, 20 MB/s). A *merged* pool can't set read=4 AND cpu=14 at once: tuned for reads (4) it starves the CPU; tuned for CPU (14) it thrashes reads. **Two data points → read-concurrency optimum is a device property, and the read and CPU widths must be sizable independently.**

**Anomaly noted:** on the M4 the cold→warm cache speedup barely appears (runs stay ~53–60 s; Intel saw 154→26 s, 6×). Hypothesis: working-set eviction / external-volume mount / driver caching difference — flagged, not yet explained.

**Implemented (`engine.rs`):** `run_processing_worker` now runs **two `rayon` pools joined by a bounded channel** — a **reader tier** (`card_read_permits`, default **4**) that does *only* `read_source_bytes` and sends raw bytes, and a **CPU tier** (`cpu_threads`, default = available) that `par_bridge`-consumes, decodes/encodes lock-free, brief-write-locks to store, and notifies. Channel cap ≈ `cpu_threads` = backpressure/memory governor. `card_read_permits` default 2→**4**. `bench_ingest` now sweeps via `INGOT_READ_THREADS` / `INGOT_CPU_THREADS` (RAYON_NUM_THREADS no longer controls the explicit pools). `cargo test` 11 green, clippy clean.

**Separated-build re-measure (Intel i7, *same* 879 card, serial SD reader):** read=2 ingest **99.5 / 84.7 / 84.5 s** (9–10 fps); read=4 **133 / 162 / 167 s** (worsening). So **separated read=2 (~90 s cold) beats the old merged design (154 s cold)** — the dedicated reader tier overlapping the CPU tier helps — while **read=4 thrashes this serial reader** (step-1 small-read penalty). Confirms: read-width optimum is device-specific (M4 reader ~4, Intel serial SD ~2), which is exactly why the tiers are now sizable independently. ⚠️ **Default is 4 (per the M4 request); a serial SD card wants 2** — tune via `card_read_permits` / `INGOT_READ_THREADS`. **Caching anomaly reproduced on Intel too** (this session `scan+enrich` 6.5 s not the warm 0.07 s; ingest never drops to the ~26 s CPU floor) → cold throughout; external-removable-volume caching/eviction is unexplained, flagged for investigation.

---

Per `docs/planning/global-plan.md`: read `DateTimeOriginal` with `kamadak-exif` (read-only, date-only) to populate `PhotoAsset.captured_at` (feeds `build_destination_path`); decode parallel `.JPG` / extract full-size embedded preview for RAW-only; downsample to ~1920px **compressed JPEG bytes in memory**; stream processed assets to the UI over a crossbeam channel.

#### Step 1 — capture-time enrichment — ✅ DONE (2026-06-11)

**Artifacts:**
- `src/lib.rs` — `captured_at: Option<NaiveDateTime>`; `read_capture_time(&PhotoAsset) -> Option<NaiveDateTime>` (private, JPEG-preferred/RAW-fallback, all-`?`/`.ok()?` failure funnel); `enrich_captured_at(&mut HashMap<…>)` (public, compute-then-apply, **serial**).
- `examples/bench_enrich.rs` — release-only timing harness (scan + 3× enrich, cold run 1 / warm 2–3, frames/s).
- `testdata/test_exif_read/` — real-EXIF fixture (incl. a CR2-only frame exercising the RAW fallback).
- Deps: `kamadak-exif` (imports as `exif`). `rayon` present but unused (reserved for step 2).
- `docs/sessions/phase-2/main-session.md` — session log.

**Key numbers (measured, 879-asset Canon set; cold = first read):**
- SD card (USB): serial cold **4.85 s** (181 fps) vs parallel cold **6.20 s** (142 fps) → parallel **−28%, hurts**.
- SSD (`GenericSSD`): serial cold **0.61 s** (1432 fps) vs parallel cold **0.17 s** (5256 fps) → parallel **+3.7×, helps**.
- Cold/warm gap: SD ≈120×, SSD ≈13×. 879/879 parsed, 0 collisions/errors. `cargo test` **5 passing** · clippy clean.
- **Lesson:** optimal source-read concurrency is a *device property*, not a constant — empirical proof of the plan's source-read semaphore tier. Default serial (1–2 permits) for cards; higher for SSD/NVMe. Hardcoding either way is wrong. File I/O concurrency = bounded threads (files don't fit kqueue readiness; io_uring is Linux-only).

**Known open items from step 1:**
- `SubSecTimeOriginal` deliberately deferred (its only consumer, burst-grouping, doesn't exist yet).
- Enrichment kept serial as the default; making read-concurrency a device-tuned permit count is Phase 3 semaphore-tier work.
- Undated frames (`captured_at == None`) not yet routed — routing fallback (mtime? `unsorted/`?) still open.

#### Step 2 — previews (in progress)

Two compressed-JPEG outputs per asset, streamed (not stored on `PhotoAsset`): free embedded **160×120 thumb** (always-in-RAM placeholder) + generated **~1920px preview**. Message: `ProcessedPreview { key, thumb_jpeg, preview_jpeg }`.

**CPU core — ✅ DONE (2026-06-12):**
- `src/preview.rs` — `turbojpeg` scaled-decode → `fast_image_resize` (Lanczos3 → 1920) → `turbojpeg` encode. Currently split (`preview_from_jpeg_bytes`/`resize`/`compress`); to be composed into `make_preview(&[u8]) -> Option<Vec<u8>>` + `pick_scale`.
- `examples/bench_preview.rs` — parallel timing harness (999× one in-RAM JPEG, thread + scaling-factor sweeps).
- Deps: `turbojpeg` (libjpeg-turbo). `fast_image_resize`.
- **Numbers (i7-1068NG7, 4 phys / 8 logical):** parallel scaling **4.26×** (1→8 threads: 100.5→23.5 s; near-linear to 4 physical, HT ~6%) — **inverse of step 1's I/O-bound result**. Scaled decode 1/1→3/8 = **2.7×** (decode dominates; resize self-funding). Embedded (exiftool): CR2 IFD0 = full-res 6240×4160 JPEG, IFD1 = 160×120 ~13 KB; **no mid-size embedded** → grid preview must be generated.
- **Locked:** decode **3/8** (smallest downscale-only; 1/4 upscales, rejected) · **Lanczos3** (quality call, time-neutral) · RGB/U8x3.

**Compose + thumb — ✅ DONE (2026-06-13):**
- `src/preview.rs` — `make_preview_from_jpeg_bytes(&[u8]) -> Option<Vec<u8>>` (decode→resize→encode, all `?`-funnelled) + `pick_scaling_factor(src_dim, target)` (rule: smallest supported `M/8` with scaled edge ≥ target via `min_by_key`; **no reliance on factor-list order**; re-derives 3/8 for 6240→1920 from the real header long edge; `ScalingFactor::ONE` fallback for already-small sources) + **orientation-aware** `resize` (clamp long edge to 1920, scale short proportionally, assign 1920 to width/height by orientation).
- `src/lib.rs` — IFD1 thumb: `get_thumbnail(&Exif) -> Option<Vec<u8>>` reads `JPEGInterchangeFormat`/`JPEGInterchangeFormatLength` at `In::THUMBNAIL`, fallible slice `buf.get(offset..offset+len)?` → free standalone-JPEG `to_vec()` (~13 KB, zero transcode). EXIF reads refactored to **one open/one parse**: `read_exif_container(&PhotoAsset)` → `get_capture_time(&Exif)` + `get_thumbnail(&Exif)` via `extract_exif_data(&Exif) -> ExifAssetData {captured_at, thumbnail}` (single extension point); `enrich_assets` folded to one `filter_map` pass (container drops in-closure → bounded memory). `captured_at` moved under `exif_data`.
- Test: `enrich_assets_test_thumbnail_filled_successfully` asserts SOI `FF D8` / EOI `FF D9` + size band (proves a complete JPEG carved at correct boundaries).
- **Memory model (5000 frames):** thumbs ~65 MB resident · compressed previews ~0.75–2.4 GB (~1.5 GB typical, persist-to-disk per plan) · decoded RGBA **~47 GB** (never resident → virtualized grid + LRU texture cache; ~64× compressed→decoded blow-up is why tier 3 must be LRU).

**RAW-only IFD0 preview (step 3) — ✅ DONE (2026-06-13):**
- `src/preview.rs` — preview core generalized to `get_resized_from_jpeg_bytes(src, target_long_dim)` (decode→resize→encode); `make_preview_from_jpeg_bytes` (1920) / `make_thumbnail_from_jpeg_bytes` (512) are thin wrappers. Target now threads into `pick_scaling_factor` *and* `resize` (no hardcoded 1920); smaller targets get smaller scaled decode (512 ⇒ 1/8 ⇒ thumbnails cheaper than previews).
- `src/lib.rs` — `get_embedded_preview_from_tiff_like(&Exif) -> Option<Vec<u8>>`: IFD0 single-strip embedded JPEG via `StripOffsets`/`StripByteCounts` at `In::PRIMARY`, fallible `buf.get(..)?` slice → `to_vec`. Feeds the same `make_*` pipeline; RAW-only path converges, zero RAW decode. Test asserts SOI/EOI + 0.5–6 MB band.
- **Probe numbers (`IMG_1939.CR2`):** `buf().len()` = 36,599,837 B (whole file — kamadak-exif holds the entire CR2) · `StripByteCounts` = 2,039,424 B (single-strip full-res ~1.95 MB) · turbojpeg decoded it → 1920px output visually verified. → slice-from-`buf()` is the correct extraction (seek-read would re-read; file already resident); 36 MB/CR2 resident is bounded by step-4 read-permit tier (~1–2 alive).
- **Three preview tiers:** IFD1 160×120 placeholder → generated 512 grid thumb → generated 1920 loupe preview (also the streaming order).

#### Step 4 — streaming ingest — ✅ DONE + MEASURED (2026-06-14)

Full design in global-plan Phase 2 §"three read stages" + §"Streaming channel (engine owns the data)"; full numbers in `docs/sessions/phase-2/main-session.md` (2026-06-14).

- **Engine owns the data:** `Engine.scan_response: Arc<RwLock<ScanResponse>>`; UI borrows to present. (Reversed the earlier app-owns/engine-stateless plan — later phases re-process heavy owned data.) `scan()` = synchronous stage-1 skeleton (walk + `enrich_assets`: date + 160×120 thumb + cached IFD0 strip ref). `ingest()` spawns a background worker, returns bounded `crossbeam_channel::Receiver<IngestEvent>`, streams **lean notifications** (`IngestEvent::Preview { key, error }`) — the UI reads the data from the store by key.
- **Lock discipline:** worker snapshots inputs (brief read lock) → **read + decode + encode lock-free** → brief per-asset write lock to store → `send` after release. No lock held while blocked on the channel → no deadlock.
- **Read stages wired:** `read_source_bytes` (RAW → `read_embedded_preview` seek-read ~2 MB; JPEG → full read), both `get_prioritized()` RAW-first so metadata + preview agree → **pairs use the embedded preview**. Seek machinery: `read_exif_header` 128 KiB prefix (264× faster than slurp) + `read_embedded_preview` file-len-bounded seek.
- **Error pattern (project-wide):** `Result<_, Box<dyn Error + Send + Sync>>`, errors recorded on `image_data.errors`, never silent `None` (documented at top of this file). `preview.rs` converted from `Option`.
- **Measured (879-asset card, 8 cores):** scan+enrich cold 14.4 s (61 fps); ingest cold **154 s (6 fps)** / warm **~26 s (33 fps)**; read-only **2≈8 threads ≈ 20 MB/s**. **Read-bound on card** (cold≫warm, cold-ingest≈cold-read, 2≈8). Card ceiling is device-fixed → cap `card_read_permits` 1–2 (CPU-thread hygiene, not a speedup). CPU floor ~33 fps; SSD flips it CPU-bound. Only cold lever = fewer bytes.
- **Artifacts:** `engine.rs` (`Engine`/`EngineConfig`/`IngestEvent`/`IngestMessage`/`read_source_bytes`); `examples/bench_ingest.rs` + `bench_read_threads.rs`. `cargo test --workspace` 11 green, clippy clean.

**Phase-2 open items carried forward (next-phase or deferred-with-rationale):**
- **Two-tier read/CPU split** — deferred; measurement says it's *not* a throughput win (card is the wall), only CPU-thread hygiene. Not blocking.
- Previews-to-disk (SQLite blob path) vs held-in-RAM (~1.5 GB / 5000) — decide when the UI lands (Phase 4 LRU/texture-cache).
- EXIF `Orientation` (portrait stored landscape + tag) — display-time, parked.
- Per-thread `Decompressor` reuse — micro-opt to measure later.
- Multi-format embedded extraction (NEF/ARW/RAF/DNG) → `rawler`/`libraw` (Canon CR2 works via kamadak `StripOffsets`).
- `main.rs` demo leaves `ingot_bench_preview.JPG` artifact; `tx.send().expect` → graceful "consumer gone" later; lock-poison policy; UI must take brief read locks (Arc-wrap previews).
- **Phase 6** (new, plan end): auto-detect + auto-start on card insert, per-device parallel pipelines, prioritized/cancellable loupe prefetch — latency hiding, not throughput.

**Resolved (was open, now done):** repo junk cleanup (`test_thumb/` + `example.JPG` git-removed, benches repointed/`CARGO_MANIFEST_DIR`, `.DS_Store` gitignored); `pub` visibility (crate boundary forced the audit → internals `pub(crate)`); `println!` removed; preview test repointed; CR2-only thumb marker added; `resize` threads `target_long_dim`; redundant casts dropped.
