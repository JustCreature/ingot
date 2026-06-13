# High-Performance Photo Ingestion Engine — Architectural Plan (v2)

## Project Overview

**The problem:** Commercial photo managers (Lightroom et al.) are bloated, aggressively decode full RAW files during import when they don't need to, and couple I/O with UI rendering — so the import phase feels sluggish exactly when the photographer wants to start working.

**The goal:** A fast, highly concurrent ingestion and triage tool for photographers shooting RAW + JPEG simultaneously. Users view, rate, and cull *immediately* while background processes safely replicate files to multiple storage targets.

**Prior art worth studying before writing code:** Photo Mechanic and FastRawViewer (the commercial culling tools this competes with — note *why* they exist: fast 1:1 focus checking), and `rapid-photo-downloader` (open-source Python; has already hit most ingest edge cases around card structure, naming, and duplicate handling).

**Design UI** The design can be found in docs/design, there is .zip and it's unarchived version in docs/design/ingot_v2 and there is a standalone html with the whole project in docs/design/html_Ingot_v2.html. Let's rely on the standalone html and only consult docs/design/ingot_v2 in the very first time and later only when necessary when looking for some specific details.

### Core principles

- **Logical asset pairing.** A `.CR2` and `.JPG` that belong together are one logical entity. Actions on one apply to both. *Pairing is keyed on directory + filename stem, not stem alone* (see Phase 1).
- **Verified replication, then deletion.** The engine copies everything first and verifies it. Deletion is a separate, user-triggered sweep that is **structurally unable to run on a source file until that file has a verified copy elsewhere.**
- **Streaming UI.** The UI never waits for a batch. Each photo streams to the grid the instant it's processed.
- **Zero RAW decoding for previews.** Previews come from the parallel `.JPG` or the JPEG *embedded inside* the RAW — never from decoding the RAW image data.
- **Device-bound honesty.** Concurrency only helps across *distinct physical devices*. Saturating one SD card or one disk is a function of that device, not thread count.

### Open scoping decisions — RESOLVED 2026-06-08

1. **RAW-only frames (no parallel JPEG): in scope?** **RESOLVED: IN SCOPE**, and the *primary* case — most pros shoot RAW-only. A `PhotoAsset` with only a RAW is a first-class asset, not an edge case. *Consequence:* the full-size embedded preview must be pulled out of the CR2 in Phase 2. **Update (2026-06-13):** `kamadak-exif` *can* reach the full-size IFD0 embedded JPEG for CR2 by slicing `StripOffsets`/`StripByteCounts` (not just the 160×120 IFD1 thumb) — so no RAW crate is needed for Canon. A RAW-aware crate (`rawler`/`rawloader` or `libraw`) becomes necessary only when adding **other formats** (Nikon NEF SubIFD, Sony ARW/Fuji RAF maker tags, DNG preview IFD), which hide the embedded preview elsewhere. The seek *mechanism* (parse header → strip offset → `pread`) is shared; only the offset *source* is per-format.
2. **Do we ever delete from the card at all?** **RESOLVED: source-card deletion OUT OF SCOPE for v1**, to be exposed later as a separate, explicitly-warned button. The normal delete function must never touch the source card — it operates only on *copied targets*, gated on `VerifiedReplica` + rejected-state.

---

## Phase 1: Asset Pairing & Core Data Structures

Map the filesystem into structures the engine understands.

### Step 1 — Define the asset

Filename stem alone is **not** a unique key: Canon rolls `IMG_9999` → `IMG_0001`, and two cards or two DCIM subfolders can contain unrelated frames with identical names. Keying a flat `HashMap<String, _>` on the stem will silently merge unrelated photos. Qualify the key with the containing directory (or, if you want cross-folder dedup, a content hash).

```rust
/// Unique across folders and cards. Stem is case-normalized so IMG_0001.CR2
/// and img_0001.cr2 pair correctly on case-insensitive filesystems.
#[derive(Clone, PartialEq, Eq, Hash)]
struct AssetKey {
    dir: PathBuf,
    stem: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TriageState { Pending, Accepted, Rejected }

struct PhotoAsset {
    key: AssetKey,
    raw:  Option<PathBuf>,            // IMG_0001.CR2
    jpeg: Option<PathBuf>,            // IMG_0001.JPG
    captured_at: Option<CaptureTime>, // DateTimeOriginal (+ SubSecTimeOriginal)
    state: TriageState,
    rating: u8,                       // 0..=5 stars; "rejected" lives in `state`
    phash: Option<ImageHash>,         // Phase 5
    cluster_group: Option<u32>,       // Phase 5
}
```

> **Rating model:** keep stars (0–5) and reject/accept as separate concepts internally. On export to XMP they fold into Adobe's convention: `xmp:Rating = -1` means rejected, `0` unrated, `1..5` stars. This maps cleanly: `Rejected → -1`, otherwise the star value.

### Step 2 — Directory scanning

Use `walkdir` to scan the card. Group into `HashMap<AssetKey, PhotoAsset>`, merging files that share a normalized (dir, stem). Normalize extension case (`.CR2`/`.cr2`, `.JPG`/`.jpeg`) when classifying RAW vs JPEG.

### Step 3 — Multi-target routing

Accept a `Vec<Target>` of user destinations. Build destination paths from **`DateTimeOriginal`** (the capture time in EXIF) — not file mtime, not the generic `DateTime` tag. Use ISO, sortable, non-redundant folder names:

```text
[root]/2026/2026-05-12/IMG_0001.CR2
```

(`2026-05-12`, not `05-12-2026` — ISO sorts correctly and isn't ambiguous; the year isn't duplicated inside the leaf.)

```rust
enum TargetKind { LocalNvme, LocalSpinning, Network }

struct Target {
    root: PathBuf,
    kind: TargetKind,
    write_permits: usize,   // semaphore size: e.g. 16 NVMe, 8 spinning, 4 network
}

fn destination_path(target: &Target, asset: &PhotoAsset, file: &Path) -> PathBuf { /* ... */ }
```

---

## Phase 2: CPU-Bound Processing & Streaming

Heavy lifting on a dedicated pool so UI and OS stay responsive.

### Step 1 — Rayon pool

Prefer the standard library over the `num_cpus` crate:

```rust
let threads = std::thread::available_parallelism()
    .map(|n| n.get().saturating_sub(1).max(1)) // leave one core for UI/OS
    .unwrap_or(1);
rayon::ThreadPoolBuilder::new().num_threads(threads).build_global()?;
```

### Step 2 — Parallel parsing & downsampling

For each asset, on the pool:

- **Capture date:** read `DateTimeOriginal` with `kamadak-exif`. This crate is a *read-only metadata parser* — it is used **only** for reading the date here, nothing else. Also read `SubSecTimeOriginal` as a burst-ordering tiebreak, since EXIF time is 1-second resolution and bursts will tie.
- **Downsample** to screen resolution (≈1920px long edge) for the loupe and ≈512px for the grid thumb. **Keep previews as compressed JPEG bytes in memory, not decoded RGBA** (see Phase 4 — RAM scaling).
- **Retain access to the full-size embedded/parallel JPEG on demand** for 1:1 loupe view (see Phase 4 — the single most important culling feature). For Canon CR2 the IFD0 embedded preview is *full sensor resolution*, so genuine 1:1 focus checking needs **zero RAW decode** — but some formats embed a reduced preview, so "full res" is only guaranteed up to the largest embedded preview the format provides.

#### The three read stages (Lightroom's "instant import" model)

The card is the bottleneck, so the engine reads it as little as possible and as late as possible. Three distinct read strategies, chosen so triage starts in seconds while replication runs in the background. **Note:** these *read stages* are a different axis from the *concurrency tiers* of Phase 3 (read-permits vs write-permits) — don't conflate the two.

| Stage | Read | Who runs it | Feeds |
|---|---|---|---|
| **1 — skeleton** | **header-only**, tiny (seek machinery) | **every** asset | `DateTimeOriginal` → greyed target-folder tree; embedded 160×120 IFD1 thumb → instant grid placeholder |
| **2 — embedded seek** | `pread` just the embedded-JPEG strip (~2–3 MB) | assets **with a RAW** (RAW-only always; pair only if measurement favours it) | the good 512 grid thumb + 1920 loupe preview |
| **3 — full read + fan-out** | whole file, **once** | **replication** (every kept file) | the targets; **and** previews for JPEG-source assets (the copy read tees into the decode) |

Key facts driving this:

- **Two embedded images, not one.** A camera writes a tiny ~160×120 IFD1 thumbnail (placeholder, header-only) **and** a large full-res IFD0 embedded JPEG (the *good* preview source). The grid thumb is downscaled from the *large* one — never from the 160×120.
- **Seek machinery is mandatory, not an optimization.** `kamadak-exif` slurps the **entire** file into `buf()` for a TIFF-like CR2 (measured: 36 MB resident to extract a ~2 MB strip). For RAW-only — the *most common* real case (most pros shoot RAW-only) — reading the embedded preview must be a **targeted `pread`** (parse header → `StripOffsets`/`StripByteCounts` from IFD0 → read only that strip), turning 36 MB into ~3 MB (~12×). Stage 1 should **cache the IFD0 strip offset** it already parsed, so stage 2 is a pure `pread` with no second header parse — keeping the skeleton sweep fast *and* read-once.
- **The embedded preview is smaller than the standalone JPEG at the same resolution** because it is a deliberately throwaway, harder-compressed JPEG (lower quality factor, 4:2:0 chroma, coarser quantization) — the RAW is the master. So the standalone `.JPG` is *higher quality*; prefer it when fidelity matters (1:1 focus), and only the byte-count argues for the embedded path.
- **Preview-source choice per asset type:**
  - **RAW-only** → stage 2 seek-extract (no choice; the only viable preview is the embedded JPEG). Top-priority fast path.
  - **JPEG-only** → stage 3. A JPEG is a monolithic stream (no targeted shortcut to a small preview), but its full read is the **same read replication owes** — fan it out (read once → decode previews **and** write to targets). It is *not* an extra "scan for previews."
  - **Pair (RAW+JPEG)** → **measurement decides**: seek the ~3 MB embedded strip (half the bytes, lower quality, non-contiguous read) vs sequentially read the ~6 MB JPEG (higher quality, readahead-friendly). Half the bytes does *not* guarantee half the time on flash; and both files get copied regardless. Measure bytes + wall time on a real card and eyeball the embedded 1:1 quality before locking.
- **Two subsystems, not three peers.** A *preview subsystem* (stage 1 skeleton + stage 2/3 good-preview) tuned for triage latency, and a *replication subsystem* (stage 3) that copies everything and lends its read to previews when the file being copied is itself a JPEG. RAW-only frames still hit stage 3 — for copying, not previewing.

### Step 3 — Streaming channel (two-pass)

A multi-producer channel (`crossbeam-channel`; prefer it over `std::sync::mpsc` so the read/CPU stages can be MPMC without a rewrite). The stream is **two-pass**, matching the read stages:

```rust
// Pass A — skeleton sweep across ALL assets first (header-only, fast):
//   the date-folder tree + placeholder grid fill before any heavy preview renders.
SkeletonReady { key, captured_at, thumb_jpeg /* 160×120 */, raw_strip_offset }
// Pass B — good previews stream in behind the skeleton (stage 2 seek / stage 3 read):
PreviewReady  { key, thumb_jpeg /* 512 */, preview_jpeg /* 1920 */ }
```

The skeleton sweep runs across *all* assets before pass B does heavy work on any single one — otherwise asset 1's 1920 encode blocks asset 50's date, and the tree fills slowly. The consumer owns the single asset store (from `scan()`); it applies `captured_at` from pass A (lights up the tree), paints placeholders, then upgrades cells as pass B arrives. The engine stays stateless about the store — the message carries the deltas, not a duplicate store.

---

## Phase 3: Asynchronous I/O Engine

Maximize throughput with concurrent, verified file operations.

> **Reality check on `tokio::fs`:** it's backed by a blocking threadpool, not true async disk I/O. For local copies you are device-bound regardless of how it's framed. Concurrency buys throughput *across distinct devices* (card → NVMe + network simultaneously), and lets I/O overlap UI work — that's the real win, and it's enough.

### Step 1 — Two semaphore tiers (not one)

The original capped only the *write* targets. But reading many files concurrently off **one** card thrashes it — the card is almost always the bottleneck. Cap both ends:

```rust
struct CopyEngine {
    /// Per physical SOURCE (card): 1–2 permits. Cards hate concurrent reads.
    source_read: Semaphore,
    /// Per physical TARGET: cap in-flight writes (NVMe 16, network 4, ...).
    target_write: HashMap<TargetId, Semaphore>,
}
```

### Step 2 — Read-once, fan-out (the key correction)

Calling `tokio::fs::copy(src, dest)` once per target reads the source **N times** for N targets — a 30 MB CR2 to three drives reads 90 MB off the card. Read (or stream) the source **once**, then write to every target. Write to a temp name and atomically rename on success, so an interrupted transfer never leaves a truncated file that looks valid. Verify every write.

```rust
/// Read the source ONCE under the source-read semaphore, hashing as we go.
/// Fan out to each target under that target's write semaphore.
/// Write to `dest.part`, fsync, rename to `dest`, then verify size + checksum.
async fn replicate(
    src: &Path,
    targets: &[Target],
    engine: &CopyEngine,
) -> Result<ReplicationReport, IoError> {
    let _read = engine.source_read.acquire().await?;
    let (bytes, src_hash) = read_with_checksum(src).await?;   // one read off the card
    // for each target: acquire write permit -> write .part -> fsync -> rename
    //                  -> re-read/compare size, then checksum == src_hash
    // collect per-target VerifiedReplica on success
    todo!()
}
```

Verification for irreplaceable data should be at minimum a size match, ideally a full checksum (BLAKE3 is fast). Compute the source hash during the single read so verification is cheap.

### Step 3 — Safe deletion sweep

Deletion is the part where a path-construction bug erases a wedding, so make safety a **type-level invariant**: the deletion function cannot be *called* without proof of a verified copy.

```rust
/// Proof that a given source file exists & checksum-matches on >=1 target.
/// Produced only by `replicate`. Cannot be forged at a call site.
struct VerifiedReplica { /* source path + target + matched hash */ }

enum DeleteMode { Trash, Permanent } // default: Trash

/// You literally cannot delete a source file without handing over its proof.
async fn delete_from_source(
    rejected: &[(PhotoAsset, VerifiedReplica)],
    mode: DeleteMode,
) -> DeletionReport { /* ... */ }
```

- **Default to move-to-trash**, not hard `remove_file`.
- **Dry-run / confirmation:** show the exact list of paths that will be deleted before touching anything.
- Source-card deletion follows decision (2) above — opt-in and clearly warned, or omitted for v1. Deleting *copied target* files (e.g. culling after import) is lower-risk and still gated on the rejected-state.

---

## Phase 4: Triage UI & State Management (Milestone 1)

Frontend plus persistence of the user's workflow.

### Step 1 — The egui interface (built to scale)

"Hold all previews in RAM as images" doesn't survive a real shoot. A 1920×1080 preview decoded to RGBA is ~8 MB; 3,000 frames ≈ **24 GB** — and as GPU textures it's the same wall, since egui uploads textures on the render thread (VRAM-bound). The standard fixes:

- **Compressed previews in RAM** (a few hundred KB of JPEG each), decoded on demand.
- **Virtualize the grid** with `egui::ScrollArea::show_rows` so only visible cells exist as widgets.
- **LRU texture cache:** decode + upload only what's on screen; evict off-screen textures.
- **1:1 loupe view (critical):** checking focus at 100% zoom is *the* culling task and is why FastRawViewer / Photo Mechanic exist. A 1920px preview cannot show 1:1. Keep the full-size embedded/parallel JPEG accessible on demand and open it in a loupe/zoom view — even though the grid uses small thumbnails.
- **Keyboard-first:** arrows to navigate, hotkeys for Accept/Reject, number keys 1–5 for stars.

### Step 2 — Local disk caching

Cache downsampled previews and metadata locally so restarts read from fast local storage instead of re-scanning the card.

- Use **`rusqlite`** rather than `sled` (sled has been in perpetual beta).
- **Store large preview blobs as files on disk**, with their paths/metadata in the DB — not as DB blobs. (SQLite *can* hold blobs, but files-on-disk keeps the DB small and reads cheap.)

### Step 3 — Metadata sidecars (XMP for both, never rewrite pixels)

There is **no standard EXIF star-rating tag** — Lightroom/Bridge read `xmp:Rating`. So write XMP for **both** files, not EXIF for the JPEG and XMP for the RAW:

- **CR2:** never modify the RAW. Write a sibling `IMG_0001.xmp` containing `xmp:Rating` (and label/keywords if desired).
- **JPEG:** also write the rating to XMP (sidecar or embedded XMP packet) rather than rewriting pixel data — this avoids any recompression/corruption risk and keeps file checksums stable.
- Writing requires a *writer* crate (`kamadak-exif` can't write). Options: `little_exif` / `img-parts` (pure Rust), or `rexiv2` (bindings to the C `exiv2` library — fullest XMP/IPTC support, at the cost of a C dependency). Choose based on whether you'll accept the native dependency.
- Map state on write: `Rejected → xmp:Rating = -1`; otherwise the star value (`0..5`).

---

## Phase 5: Visual Clustering (Milestone 2)

Algorithmic grouping folded into the existing Rayon pipeline.

### Step 1 — Parallel hashing

Add a step to the Phase 2 pool: compute a perceptual hash (pHash) on the *downsampled* preview with `image_hasher`, before sending to the UI.

### Step 2 — Sensitivity grouping

- Sort assets by `DateTimeOriginal`, breaking ties with `SubSecTimeOriginal`.
- A UI slider sets the Hamming-distance threshold. Compare `asset[i]` to `asset[i-1]`; under threshold → same cluster, over → break the grid into a new visual cluster.
- **Known limitation (fine for v1):** consecutive-frame comparison is *single-linkage* clustering, so a long slow pan can chain-merge into one giant cluster (each frame is close to its neighbor even though the ends are very different). Acceptable for burst grouping; revisit with a windowed/centroid approach if it becomes a problem.

---

## Consolidated Dependencies (and exactly what each does)

| Crate | Role | Notes |
|---|---|---|
| `walkdir` | Scan card directories | — |
| `kamadak-exif` | **Read** `DateTimeOriginal` / `SubSecTimeOriginal` | Read-only. Date only. Cannot write; cannot extract full previews. |
| `image` | Decode JPEG, downsample previews | — |
| `rawler`/`rawloader` *or* `libraw` bindings | Extract full-size embedded preview from CR2 | **Only if RAW-only frames are in scope** (decision 1). |
| `rayon` | CPU-bound parallel parse/downsample/hash | Size with `std::thread::available_parallelism()`, not `num_cpus`. |
| `crossbeam-channel` / `std::sync::mpsc` | Stream processed photos to UI | — |
| `tokio` | Async I/O runtime, semaphores, fs | `fs` is threadpool-backed — device-bound for local copies. |
| `blake3` | Fast checksums for copy verification | Hash during the single source read. |
| `egui` / `eframe` | Triage UI | Virtualize grid (`show_rows`) + LRU texture cache + loupe view. |
| `rusqlite` | Metadata + cache index | Prefer over `sled`. Store blobs as files, paths in DB. |
| `little_exif` / `img-parts` *or* `rexiv2` | **Write** `xmp:Rating` sidecars | `kamadak-exif` can't write. `rexiv2` = best XMP support, C dependency. |
| `image_hasher` | Perceptual hash (pHash) for clustering | Milestone 2. |
| `trash` (optional) | Move-to-trash instead of hard delete | Backs the default `DeleteMode::Trash`. |

---

## Changes from v1 (summary)

1. **Crate roles split:** `kamadak-exif` reads the date only; a separate writer crate handles `xmp:Rating`; RAW-only previews need a RAW crate.
2. **Ratings go to XMP for both files** (`xmp:Rating`, −1..5), never to a nonexistent EXIF rating tag, and never by rewriting pixel data.
3. **Read-once, fan-out copy** replaces per-target `copy()` (no more N× source reads); writes go to temp then atomic-rename.
4. **Two semaphore tiers:** cap source-card *reads* (1–2) as well as per-target *writes*.
5. **Verification added:** size + BLAKE3 checksum before a copy counts as replicated.
6. **Deletion gated at the type level:** `delete_from_source` requires a `VerifiedReplica`; defaults to trash; dry-run/confirmation; source-card deletion is opt-in.
7. **UI scales:** compressed previews in RAM + virtualized grid + LRU texture cache (the 24 GB RGBA problem).
8. **1:1 loupe view added** — the core culling feature a 1920px preview can't serve.
9. **Pairing key fixed:** (dir, stem), case-normalized, to avoid filename-rollover and cross-folder collisions.
10. **Date handling:** `DateTimeOriginal` + `SubSecTimeOriginal`; ISO sortable folders (`2026-05-12`).
11. **Storage choices:** `rusqlite` over `sled`; blobs as files-on-disk.
12. **Honesty notes:** `tokio::fs` is threadpool-backed; consecutive-frame clustering is single-linkage and can chain.
