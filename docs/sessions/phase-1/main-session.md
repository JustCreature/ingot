# Phase 1 Session Log

## Overview

Phase 1 — Asset Pairing & Core Data Structures. Goal: turn a messy camera-card directory tree into a trustworthy in-memory model — correctly answering "what photos are here, and which files belong to which photo?" before any I/O, UI, or concurrency. This session resolved the two open scoping decisions and designed the core asset data model.

---

## Session 1 — 2026-06-08 — Scoping decisions + asset model design

### What was built / explored

- **Scoping decisions resolved** (recorded in CLAUDE.md + `docs/planning/global-plan.md`):
  1. **RAW-only frames: IN SCOPE.** A `PhotoAsset` with only a RAW is a first-class asset, never an edge case. Adds a RAW-aware crate (`rawler`/`rawloader` or `libraw`) for full-size embedded-preview extraction in Phase 2.
  2. **Source-card deletion: OUT OF SCOPE for v1.** Later exposed as a separate, explicitly-warned button. The normal delete function never touches the source card — only copied targets, gated on `VerifiedReplica` + rejected-state.

- **Adversarial test tree built by user** at `photodata/testing/source/`. Contents map to **11 true assets** keyed on `(dir, stem)`:
  - `DCIM/100CANON/`: IMG_1800/1868/1875/1881/1891/1907 (CR2+JPG paired ×6), IMG_1915 (JPG-only), IMG_1939 (CR2-only) = 8 assets
  - `DCIM/101CANON/`: IMG_1800 (paired), IMG_1915 (JPG-only), IMG_1939 (CR2-only) = 3 assets
  - Noise that must classify to "skip": `.DS_Store` (multiple), `DCIM/EOSMISC/M2100.CTG` (Canon catalog).
  - Breakdown: 7 paired, 2 RAW-only, 2 JPEG-only.

- **Asset model design — landed on: non-empty map keyed by `FileKind`.** Design path:
  - Rejected stem-only key (silent merge of rollover / multi-folder / multi-card collisions) → use `(dir, stem)`, case-normalized.
  - Rejected timestamp-in-key idea (not available at scan time without EXIF; 1s resolution isn't unique per frame — whole bursts collide; the directory is the free, sufficient disambiguator).
  - Rejected closed combination-enum (2^N−1 variants; dies under configurable kinds).
  - Rejected two `Option<PathBuf>` (permits illegal `(None,None)`; and hardcoded fields aren't extensible/configurable either — fails the user's own future requirement).
  - **Chosen:** a `FileKind`-keyed collection wrapped in a non-empty newtype. Extensible (add a `FileKind` variant, not a struct field) + illegal empty state unrepresentable by construction.

### Errors and fixes (best learning)

- **User initially proposed putting a shooting-datetime timestamp in the key.** Corrected: (a) timestamp lives in EXIF — unavailable to the path-only scanner without dragging Phase 2 into Phase 1; (b) `DateTimeOriginal` is 1-second resolution, so a 10–14 fps burst shares one timestamp → keying on it merges whole bursts; (c) the unique-but-pair-shared identifier the user wanted is just the **directory**, free from `walkdir`. Principle: key on the cheapest sufficient information.
- **User proposed "two Options + just don't write code that makes all-None."** Corrected as the convention-not-invariant trap: `Default` derive, serde deserialize (Phase 4 cache), drain-during-move, post-construction filtering all produce empty silently and still compile. Inconsistent with the type-level `VerifiedReplica` rigor already planned for Phase 3.

### Key discussion points

- **Make illegal states unrepresentable** > validate-at-runtime. Same idea that powers Phase 3's `VerifiedReplica`.
- **Non-emptiness comes free from the scan shape:** `walkdir` yields files one at a time, so an asset is *born from its first file* (`new(kind, path)`) and *grows* (`insert(kind, path)`). No empty constructor exists. Maps onto `HashMap<AssetKey, PhotoAsset>::entry` (absent → new, present → insert).
- **Duplicate-kind collision policy:** `insert` of an already-present kind must NOT silently overwrite (silent file loss — the phase's central sin). Surface it (Result / return displaced path). Can arise via case-normalization on case-sensitive filesystems.
- **Case-normalization is a filesystem-semantics decision**, not a Rust one (APFS case-insensitive/preserving vs ext4 case-sensitive). Lowercasing the key bakes in a case-insensitive assumption.
- **Storage perf aside:** `FileKind` holds only a handful of entries; a per-asset `HashMap` = thousands of tiny allocs + hashing to find among 2–3 items. A `Vec<(FileKind, PathBuf)>` linear scan is faster/cache-friendlier. Hide it behind `get/insert/has` so the backing store stays swappable + measurable. Don't build the configurable system yet (YAGNI) — just pick the shape that extends cleanly.

### Next step (where we left off)

User is about to implement (or talk through) the asset model sketch:
1. `FileKind` enum + `classify(extension) -> Option<FileKind>` (allowlist: `cr2→Raw`, `jpg|jpeg→Jpeg`, else `None`).
2. Non-empty files type — name it, `Vec`-backed for now, `new(kind, path)` + `insert(...)` + accessors (`get(kind)->Option<&Path>`, `has(kind)`), no `Default`.
3. Collision policy in `insert`'s signature (surface, don't overwrite).

Target to validate: scanner produces **11 assets**, demonstrates **4 file paths the naive stem-key would lose**, and **zero panics** on the `.CTG`/`.DS_Store` noise.

---

## Final Numbers

| Metric | Value | Notes |
|---|---|---|
| True assets in test tree | 11 | 7 paired, 2 RAW-only, 2 JPEG-only |
| Naive stem-key assets | 8 (predicted) | merges IMG_1800/1915/1939 across folders |
| Files silently lost under naive key | 4 (predicted) | to be confirmed by running the scanner |

*(Predicted numbers — not yet measured by running code. Confirm once the scanner exists.)*
