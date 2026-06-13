⛔ NEVER write code, edit files, or run commands without explicitly announcing Code Exception Mode first. This is a learning project — guide, don't implement.

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this project is

**Ingot** — a high-performance RAW+JPEG photo ingestion and triage engine, intended to be written in **Rust**. It competes with Photo Mechanic / FastRawViewer: ingest a camera card fast, let the photographer rate and cull *immediately* while files replicate to multiple storage targets in the background.

**This is a learning project, not a delivery project.** The point is for the user to learn by implementing it themselves. There is no source code in the repo yet — only the design and the architectural plan. Do not scaffold a Cargo project, write modules, or run build/test commands unless the user explicitly grants a Code Exception (see below).

## Operating mode (governed by the `learning-guide` skill)

The `learning-guide` skill (`.claude/skills/learning-guide/`) is always active for this repo and is the source of truth for *how* to work here. Read it before doing anything substantive. The essentials:

- **Guide, never implement.** Explain *why* at the hardware/systems level (cache lines, device saturation, semaphore tiers, ownership/type-state). Point direction and suggest experiments. Do not produce code.
- **Code Exception Mode** is the *only* way code gets written, and it requires explicit user phrasing ("just write it", "make an exception", "write the code now", "let's implement together"). You must announce you are entering it first. If unsure whether the user wants code, ask — do not write it.
- **Measurement over intuition.** "Which is faster?" is answered by naming the counters to measure, not by guessing.
- Shorthand commands handled by the skill: `--R` (generate reports), `--|` (save session), `--|--` (restore session), `--s` (status), `--v` (finalise phase), `--v--FORCE` (force finalise). Phase status, artifacts, and measured numbers are tracked in a `## Status` section of this file (added once Phase 1 work begins).

## Where things live

- `docs/planning/global-plan.md` — **the architectural plan (v2). Read this first** for the full phase breakdown, data structures, crate choices, and rationale. (Note: the empty `global-plan.md` at repo root is a stray file; the real plan is under `docs/planning/`.)
- `docs/design/` — the UI design. **Rely on the standalone `docs/design/html_Ingot_v2.html`.** The unpacked `docs/design/ingot_v2/` (`.jsx`, `styles.css`, `data.js`, images) is the source bundle — consult it only on the first pass or when you need a specific detail.
- `docs/sessions/phase-N/` and `docs/lessons/phase-N/` — created by the skill as the project progresses (session logs and learning reports). Absent until work starts.

## Architecture invariants (from the plan — preserve these in any guidance)

These are deliberate design decisions, not suggestions. When discussing implementation, hold the user to them:

- **Asset pairing is keyed on `(dir, stem)`, case-normalized — never stem alone.** Canon rolls `IMG_9999 → IMG_0001`, and separate folders/cards reuse names; a flat stem-keyed map silently merges unrelated frames.
- **Zero RAW decoding for previews.** Previews come from the parallel `.JPG` or the JPEG embedded in the RAW — never from decoding RAW pixel data.
- **Copy-verify-then-delete, gated at the type level.** Replication copies and verifies (size + BLAKE3) first. Deletion is a separate user-triggered sweep that is *structurally unable* to run on a source file without a `VerifiedReplica`. Default to trash, not hard delete.
- **Read-once, fan-out copy.** Read each source file once and fan out to all targets; never re-read per target. Write to temp, then atomic-rename.
- **Two semaphore tiers.** Cap source-card *reads* (1–2) separately from per-target *writes*. Concurrency only helps across *distinct physical devices* — saturating one card/disk is a device property, not a thread-count property.
- **Streaming UI.** Each photo streams to the grid the instant it is processed; the UI never waits for a batch. At scale this needs a virtualized grid + LRU texture cache (the 24 GB RGBA problem) and a 1:1 loupe view for focus checking.
- **Ratings → XMP for both files, never EXIF, never pixel rewrites.** There is no standard EXIF rating tag; write `xmp:Rating` sidecars (`Rejected → -1`, else `0..5`) for both the CR2 and the JPEG. Keep stars (0–5) and accept/reject as separate internal concepts.
- **Crate roles are split deliberately:** `kamadak-exif` *reads* dates only (it cannot write or extract full previews); a writer crate (`little_exif`/`img-parts` or `rexiv2`) handles XMP; RAW-only previews would need `rawler`/`rawloader`/`libraw`. Prefer `rusqlite` over `sled`; store preview blobs as files on disk with paths in the DB. Size Rayon pools with `std::thread::available_parallelism()`.

## Two scoping decisions — RESOLVED 2026-06-08

1. **RAW-only frames (no parallel JPEG): IN SCOPE.** A `PhotoAsset` with only a RAW is a first-class asset, never an edge case. Adds a RAW-aware crate (`rawler`/`rawloader` or `libraw`) for full-size embedded-preview extraction in Phase 2.
2. **Source-card deletion: OUT OF SCOPE for v1.** To be exposed later as a separate, explicitly-warned button. The normal delete function never touches the source card — it operates only on *copied targets*, gated on `VerifiedReplica` + rejected-state.

## Status

**Current phase:** Phase 2 — CPU-Bound Processing & Streaming (step 1 ✅; step 2 CPU core ✅; step 2 `make_preview` compose + IFD1 thumb ✅; RAW-only IFD0 preview + streaming next)

**Last session:** 2026-06-13 — Composed `make_preview_from_jpeg_bytes` (decode→resize→encode, `?`-funnelled) + rule-based `pick_scaling_factor` (re-derives 3/8 by long edge) + orientation-aware resize; IFD1 thumb extraction (`get_thumbnail(&Exif)`, free byte-slice); refactored EXIF reads to one open/parse + `extract_exif_data` builder + single-pass `enrich_assets`.

---

### Phase 1 — Asset Pairing & Core Data Structures — ✅ COMPLETE (2026-06-11)

**Learning objectives (met):**
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

### Phase 2 — CPU-Bound Processing & Streaming (in progress)

Per `docs/planning/global-plan.md`: read `DateTimeOriginal` with `kamadak-exif` (read-only, date-only) to populate `PhotoAsset.captured_at` (feeds `build_destination_path`); decode parallel `.JPG` / extract full-size embedded preview for RAW-only (needs `rawler`/`rawloader` or `libraw`, per resolved scoping decision #1); downsample to ~1920px **compressed JPEG bytes in memory**; stream processed assets to the UI over an mpsc/crossbeam channel.

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

**Remaining (next):** (3) RAW-only CR2 IFD0 full-res preview path (`In::PRIMARY` Strip tags → embedded 6240×4160 JPEG → feeds `make_preview`; zero RAW decode); (4) `ProcessedPreview { key, thumb_jpeg, preview_jpeg }` channel streaming + two-tier concurrency (bounded card reads feeding CPU pool).

**Known open items from step 2:**
- Per-frame `Decompressor` (per-thread reuse = later optimization to measure).
- Bench iterates one image 999× (CPU throughput proxy); real two-tier read+CPU measurement deferred to the streaming step.
- Previews-to-disk (SQLite blob path) vs held-in-RAM — decided at step 4 (channel consumer).
- EXIF `Orientation` (portrait frames stored landscape + tag) — display-time handling, parked.
- Thumb test covers JPG path; CR2-only (`img_1939`) marker assertion suggested but not yet added.
