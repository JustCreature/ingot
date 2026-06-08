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

**Current phase:** Phase 1 — Asset Pairing & Core Data Structures (in progress; started 2026-06-08)

**Learning objectives:**
- Why `(dir, stem)` case-normalized is the only safe asset key (filename rollover, multi-folder DCIM, multi-card collisions)
- Modeling RAW/JPEG presence so the empty asset is unrepresentable, with RAW-only as a first-class asset
- A pure, I/O-free date-based ISO routing function

**Asset model — decided:** key on `(dir, stem)` case-normalized; files held in a **non-empty map keyed by `FileKind`** (extensible enum: `Raw`, `Jpeg`, …). Born from first file (`new`), grows via `insert`; no empty constructor, no `Default`. `insert` must surface duplicate-kind collisions, never silently overwrite.

**Test tree:** `photodata/testing/source/` — 11 true assets (7 paired, 2 RAW-only, 2 JPEG-only) across `100CANON`/`101CANON`, plus `.DS_Store` + `EOSMISC/M2100.CTG` noise that must classify to skip.

**Artifacts:** none yet (design only). Session log: `docs/sessions/phase-1/main-session.md`

**Key numbers (predicted, not yet run):** true assets 11 | naive stem-key 8 | files silently lost under naive key 4.

**Open items:**
- Implement `FileKind` + `classify(ext) -> Option<FileKind>` (allowlist: `cr2→Raw`, `jpg|jpeg→Jpeg`, else `None`)
- Implement the non-empty files type (`Vec`-backed, `new`/`insert`/accessors, no `Default`, collision-surfacing `insert`)
- Implement `AssetKey` + `PhotoAsset` + the `walkdir` scanner with `HashMap::entry` merge
- Run scanner on test tree: confirm 11 assets, 4 files the naive key would lose, zero panics on `.CTG`/`.DS_Store`
- Phase 1 Step 3: pure date-based ISO routing function (no I/O)

**Last session:** 2026-06-08 — scoping decisions resolved; asset model designed (non-empty map keyed by `FileKind`).

**Next step:** implement the asset model sketch — `FileKind` + classifier, then the non-empty files type.
