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

---

## Session 2 — 2026-06-08 → 2026-06-10 — Implementing the scanner (Code Exception Mode)

### What was built / explored

All in `src/lib.rs` (the user wrote it; guide reviewed iteratively). `cargo init --lib`; library-only crate, driven by `cargo test`. Final state: **2 tests passing, `cargo clippy` clean.**

- **Types implemented** exactly as designed:
  - `enum FileKind { Raw, Jpeg }` — `#[derive(PartialEq, Eq, Clone, Copy, Debug)]`.
  - `struct AssetKey { dir: PathBuf, stem: String }` — `#[derive(PartialEq, Eq, Hash, Clone, Debug)]`; stem **lowercased** when built (case-normalization).
  - `struct AssetFiles { files: Vec<(FileKind, PathBuf)> }` — the non-empty type. `new(kind, path)` (born from one file), `insert` returns `Result<(), PathBuf>` (Err carries the **displaced** path; collision = same kind already present), `get(kind) -> Option<&Path>` (linear scan, n tiny). No `Default`.
  - `enum TriageState { Pending, Accepted, Rejected }`; `struct PhotoAsset { files, state, rating: u8, captured_at: Option<String> }` (captured_at deferred to Phase 2, stays `None`).
  - `struct Collision { key, kind, kept, displaced }`; `struct ScanResponse { assets: HashMap<AssetKey, PhotoAsset>, collisions: Vec<Collision>, errors: Vec<walkdir::Error> }`.
- **`scan_source_dir`**: `walkdir` → skip non-files/no-ext (`let-else`), `classify` extension (lowercased allowlist `cr2→Raw`, `jpg|jpeg→Jpeg`), build `(dir, stem)` key, `HashMap::entry` merge via explicit `match Entry::{Occupied, Vacant}`. Walkdir errors collected into `errors` (not swallowed).
- **Collision handling (the safety keystone), final correct form:** in the Occupied arm, `insert` **first**; only `if let Err(displaced)` fetch `kept` via `if let Some(... = o.get().files.get(kind))` — no panic, and crucially fetched *after* insert so the normal pairing path (kind absent → Ok) is never skipped.
- **Public API:** engine types + `scan_source_dir` are `pub`; `AssetFiles::{new,insert,get}` made `pub` (closed the "opaque AssetFiles" gap so consumers can introspect an asset's files).
- **Tests (programmatic fixtures in tempdirs):**
  - `build_tree(root, &[files])` helper — `create_dir_all` + `File::create(root.join(file))` (empty files; scanner is content-agnostic in Phase 1).
  - `scans_test_tree_into_11_assets` — full tree → asserts 11 assets, 0 collisions, 0 errors, **and breakdown 7 paired / 2 JPEG-only / 2 RAW-only** (composition proves no-loss).
  - `scans_test_tree_spot_collision` — `IMG_1800.jpg` + `IMG_1800.JPEG` → asserts 2 assets, 1 collision, kind `Jpeg`, and the `{kept, displaced}` **set** (order-independent).
- **Dev dependency added:** `tempfile` (`[dev-dependencies]`), RAII auto-cleanup.

### Errors and fixes (best learning)

- `for x in self.files` behind `&self` → "cannot move out of shared reference"; `Vec` isn't `Copy`. Fix: `&self.files` (`iter()` borrows vs `into_iter()` consumes).
- `match` on `String` vs `&str` literals → `match ext.as_str()`.
- `e.file_type()` (FileType) ≠ extension → `e.path().extension()`.
- `AssetKey` used as HashMap key without `Hash`/`Eq` → derive them; `Eq` (total, reflexive) vs `PartialEq` (why `f64` can't be a key).
- `Debug` cascade: deriving on a struct requires `Debug` on every field type, recursively.
- `cargo test` swallows stdout for passing tests → `-- --nocapture` / `--show-output`.
- **Test fixture wrote files to CWD** (`File::create(file)` not `root.join(file)`) → 0 assets + 14 stray files polluting repo root (cleaned). The ENOENT in the debug loop = `WalkDir::new(tmp_dir)` *moved+dropped the `TempDir`* (RAII delete) before walking → use `tmp_dir.path()`.
- **Walkdir ordering trap:** asserting `[kept, displaced]` as an ordered pair failed (walkdir order not guaranteed) → assert the **set**.
- **Collision-fetch ordering bug:** extracting `kept` *before* insert both caused a borrow conflict (immutable `kept` borrow overlapping `get_mut`) **and** a logic inversion (Occupied + `get(kind)==None` is the *normal pairing* case; `else continue` would drop legitimate JPEGs). Fix: fetch `kept` inside the `Err` branch, after insert.
- 11 clippy dead-code warnings = library had **no `pub` API** (`cargo clippy` checks lib without test cfg). Fixed by marking the real public surface `pub`, not `#[allow]`.

### Key discussion points

- Ownership: `into_iter` (consume) vs `iter` (borrow) vs `iter_mut`; can't move out from behind `&self`; `Vec` non-`Copy` because it owns a heap alloc (double-free).
- `let-else` = "happy value or **diverge**", never a two-way branch; `if let Err(x)` = "do something only on error". Combinators (`and_modify`/`or_insert_with`) can't express side-outputs/`?` → drop to `match Entry`.
- HashMap internals: `Hash` picks the bucket, `Eq` disambiguates within it; invariant `a==b ⟹ hash(a)==hash(b)`.
- `collect` = generic `FromIterator` builder (needs target-type annotation); `into_iter` turns an array into an iterator yielding owned values; `BTreeSet::from([...])` is the tidier fixed-set constructor.
- `expect`/panic couples to another method's invariant and **aborts the whole batch** — inconsistent with "anomalies are data" (collect into report), so prefer `if let Some` / push-to-errors.
- Lib vs bin: Phase 1 has nothing to *run*, only to *test*; `examples/scan.rs` for visual output later, defer `main.rs`.

### Next step (where we left off)

Phase 1 **Steps 1 & 2 complete** (asset model + scanning/pairing, fully tested). Remaining: **Step 3 — multi-target routing**, a pure I/O-free `destination_path(target, <date>, file) -> PathBuf` building ISO folders `root/2026/2026-05-12/IMG_0001.CR2`, plus `enum TargetKind { LocalNvme, LocalSpinning, Network }` and `struct Target { root, kind, write_permits }` (kind/permits are seeds for Phase 3 semaphores, unused in routing).

**Open decision before writing it:** date representation —
- pull in **`chrono::NaiveDate`** now (type-safe, ISO formatting, makes invalid dates unrepresentable, reused in Phase 2 EXIF) — *guide's lean*; or
- keep Step 3 dependency-free with pre-formatted string/component inputs, introduce the date type in Phase 2 when EXIF lands.

Design notes for Step 3: take the **date as a parameter** (decouple from EXIF/Phase 2); build the filename from the **original-case** `file.file_name()`, never the lowercased `key.stem`; keep it pure (compute a path, create no dirs) so it's `assert_eq!`-testable with a hardcoded date.

---

## Session 3 — 2026-06-11 — Step 3 routing (Code Exception Mode)

### What was built / explored

- **`chrono` added** (`cargo add chrono`). Date type decision resolved: `chrono::NaiveDate` (type-safe — `from_ymd_opt` validates, so invalid dates are unconstructable; ISO `Display`; reused by Phase 2 EXIF).
- **Types:** `enum TargetKind { LocalNvme, LocalSpinning, Network }`, `struct Target { root: PathBuf, kind: TargetKind, write_permits: usize }` (both `#[derive(Clone, Debug)]`). `kind`/`write_permits` are seeds for Phase 3 semaphores — unused by routing.
- **`build_destination_path(target: &Target, captured: NaiveDate, file: &Path) -> Option<PathBuf>`** — pure, no I/O. Year via `captured.year()` (`Datelike`), date dir via ISO format, filename via `file.file_name()?` (original case, **not** the lowercased key stem). Returns `Option` to honestly handle `file_name() == None`. Builds `root/2026/2026-05-12/IMG_0001.CR2`.
- **Test `build_target_dir_works`** — hardcoded `NaiveDate::from_ymd_opt(2026,5,22)`, asserts exact `PathBuf`. No tempdir (pure computation).

### Errors and fixes

- clippy: `let Some(x) = ... else { return None }` in an `Option`-returning fn → use the **`?` operator** (`?` works on `Option`, not just `Result`, when the fn returns `Option`). Also dropped a redundant `Path::new(file_name)` — `&OsStr: AsRef<Path>`, so `.join(file_name)` works directly.

### Key discussion points

- Decouple path-building from the date *source*: take the date as a parameter so the function is pure/testable now; Phase 2 supplies the real `DateTimeOriginal`.
- `NaiveDate` is a plain calendar date (no clock/zone); `Display` is already ISO 8601. Constructor `_opt` validation = "invalid dates unrepresentable", consistent with the rest of the model.

---

## Final Numbers

| Metric | Value | Notes |
|---|---|---|
| True assets in test tree | 11 (measured ✓) | 7 paired, 2 RAW-only, 2 JPEG-only — asserted |
| Same-kind collisions surfaced | 1 (measured ✓) | `.jpg`/`.jpeg` → both `Jpeg`, kept+displaced asserted |
| Routing path | exact match ✓ | `root/2026/2026-05-22/IMG_001.CR2` asserted |
| Tests | 3 passing | + `build_target_dir_works` |
| clippy | clean | no warnings |
| Naive stem-key (counterfactual) | 8 | demonstrated by reasoning; cross-folder dups proven distinct (11≠8) |
