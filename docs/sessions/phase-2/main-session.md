# Phase 2 Session Log

## Overview

Phase 2 — CPU-Bound Processing & Streaming. This session completed **step 1: capture-time
enrichment** — reading `DateTimeOriginal` with `kamadak-exif` to populate `PhotoAsset.captured_at`,
then measuring whether the work is CPU- or I/O-bound on real storage (SD card vs SSD) to decide the
read-concurrency strategy.

---

## Session 1 — Capture-time enrichment + read-concurrency measurement (2026-06-11)

### What was built / explored

- **Type decision:** promoted `captured_at: Option<String>` → `Option<NaiveDateTime>`. Reasoning:
  EXIF `DateTimeOriginal` carries *no timezone* (it's the camera's wall clock), so a naive type tells
  the truth; `DateTime<Utc>` would force inventing an offset and could mis-route a midnight frame to
  the previous day's folder. `Option` stays because RAW-only / stripped-EXIF frames legitimately have
  no date. `build_destination_path` is unchanged — caller projects `.date()` at the routing boundary.
- **`read_capture_time(&PhotoAsset) -> Option<NaiveDateTime>`** (private): JPEG-preferred, RAW-fallback
  via `files.get(Jpeg).or_else(get(Raw))`; opens → `BufReader` → `exif::Reader::read_from_container`
  → `get_field(DateTimeOriginal, In::PRIMARY)` → match `Value::Ascii` → `parse_from_str("%Y:%m:%d %H:%M:%S")`.
  Every failure mode funnels to `None` via a chain of `?` / `.ok()?` — one bad frame cannot abort the pass.
- **`enrich_captured_at(&mut HashMap<AssetKey, PhotoAsset>)`** (public): compute-then-apply — build a
  `Vec<(AssetKey, Option<NaiveDateTime>)>` then write back. Kept **serial** (see measurement below).
  Shape chosen to match the streaming endgame (results trickle out, channel = apply step).
- **`examples/bench_enrich.rs`** — release-only timing harness: times scan + 3× enrich (run 1 cold,
  2–3 warm), reports frames/s and filled count. Used for the device comparison.
- Deps added: `kamadak-exif` (imports as `exif`). `rayon` added then left unused after revert.

### Errors and fixes / wrong hypotheses

- **Hypothesis (partly wrong, productively):** "enrichment is I/O-bound, so Rayon won't help." True on
  the SD card; **false on the SSD**, where `par_iter` was 3.7× *faster*. The corrected model: parallel
  reads help iff the *device* has queue depth / internal parallelism. The right knob is a bounded,
  device-tuned read concurrency — not a hardcoded serial vs parallel.
- **Methodology trap caught:** the cold measurement is one-shot per cache state; runs 2–3 measure the
  page cache, not the device. Looking only at warm numbers would have argued backwards (warm parallel
  always "wins" on work that never happens in a real once-cold ingest).
- **Cache eviction:** `sudo purge` needs a password this session can't supply; user purges/reconnects
  the card manually (saved as a memory preference). Don't unmount programmatically.

### Key discussion points (mental models)

- **Parallelism vs concurrency:** I/O-bound work wants *operations in flight*, not *cores busy*.
- **Why file I/O needs threads on macOS:** regular files are always "ready" to kqueue/epoll, so the
  single-thread-drives-many-sockets trick doesn't apply; a blocking `read()` parks the thread. Thus
  concurrency = N threads parked in the kernel (N can exceed cores — they're I/O-blocked, not CPU-bound).
  True single-thread many-in-flight needs `io_uring` (Linux only); macOS = bounded thread pool.
- **`ref` in patterns:** `Value::Ascii(ref vec)` borrows instead of moving out from behind `&Field`;
  equivalent to matching on `&field.value` (match ergonomics).
- The bounded read concurrency **D = the source-read semaphore permit count** from the plan — now
  empirically justified, not just asserted.

---

## Final Numbers (cold = run 1, real-ingest scenario; warm = page cache)

| Source device | scan (cold) | serial cold | parallel cold | parallel effect | parsed |
|---|---|---|---|---|---|
| **SD card** (USB reader) | 3.75 s | 4.85 s · 5.51 ms/fr · 181 fps | 6.20 s · 7.06 ms/fr · 142 fps | **−28% (hurts)** | 879/879 |
| **SSD** (`GenericSSD`) | 34 ms | 0.61 s · 0.70 ms/fr · 1432 fps | 0.17 s · 0.19 ms/fr · 5256 fps | **+3.7× (helps)** | 879/879 |

- Cold/warm gap: SD ≈ 120×, SSD ≈ 13× (faster device → less I/O penalty to hide).
- Warm (CPU-bound) parallel beats serial ~3× on both — but warm never occurs in a real first ingest.
- 0 collisions, 0 walkdir errors on the real 879-asset Canon card.
- `cargo test` 5 passing · `cargo clippy` clean.

**Conclusion:** optimal source-read concurrency is a *device property*. Default serial (1–2 permits)
for camera cards; allow higher for SSD/NVMe. Rayon's real win is deferred to step 2 (preview decode).

---

## Session 2 — Preview pipeline: CPU core + scaled decode + parallel scaling (2026-06-12)

### What was built / explored

- **Output shape decided:** two compressed-JPEG outputs per asset, streamed (not stored on `PhotoAsset`):
  free embedded **160×120 thumbnail** (always-in-RAM placeholder) + generated **~1920px preview**.
  Message-to-be: `ProcessedPreview { key, thumb_jpeg, preview_jpeg }`.
- **Crate stack:** `turbojpeg` (libjpeg-turbo: scaled DCT decode + encode) + `fast_image_resize` (SIMD
  resize). Build gate cleared (needed `brew install jpeg-turbo cmake nasm` territory; linked clean).
- **`src/preview.rs`** — decode (scaled) → resize (Lanczos3 → 1920) → encode, currently split across
  `preview_from_jpeg_bytes` / `resize` / `compress` (to be composed into one `make_preview(&[u8])` unit).
- **`examples/bench_preview.rs`** — parallel timing harness over 999 iterations of one in-RAM JPEG;
  swept `RAYON_NUM_THREADS` and decode scaling factors.
- **Embedded-image probe (exiftool):** measured what Canon actually embeds — see Final Numbers.

### Errors and fixes / wrong hypotheses

- **`&mut [u8]` signature blocked Rayon:** `preview_from_jpeg_bytes` took `&mut`, but the JPEG input is
  read-only and a `Fn` closure can't hand `&mut` to N threads. Fix: take `&[u8]` (shared, `Sync`) — the
  `&mut` was both wrong and the thing preventing parallelism.
- **Two `Image` types collided:** `turbojpeg::Image` vs `fast_image_resize::images::Image` — `Image::new`
  "didn't exist" because the import shadowed it. Alias one (`FirImage`).
- **Pitch/format mismatch:** RGB format with `pitch = 4*width` (RGBA sizing) breaks `from_vec_u8`'s
  tight-stride assumption. RGB ⇒ `pitch = 3*width`, buffer `3*w*h`.
- **`fast_image_resize` wasn't actually added** (Gate 0 `cargo add` only took `turbojpeg`); `Resizer`/
  `Image::new` were missing because the crate wasn't in the tree.
- **Data-flow:** passing bare `Vec<u8>` between stages dropped the width/height/format the encoder needs;
  thread `turbojpeg::Image<Vec<u8>>` through every stage, emit `Vec<u8>` only at `compress`.

### Key discussion points (mental models)

- **Scaled (DCT-domain) decode** is the dominant lever: libjpeg-turbo decodes at `M/8` factors, so you
  never decode 26 MP to make a 2 MP tile. Rule: smallest `M/8` whose long edge ≥ target (6240 → **3/8** = 2340).
- **Resize is self-funding, not free:** it shrinks the image so the *encode* gets cheaper, paying back
  its own cost — so "with vs without resize" came out a wash.
- **Filter is a quality decision, not a timing one** (resize is a small fraction of total): Lanczos3 vs
  Bilinear ≈ same time; chose **Lanczos3** on appearance (no visible difference at tile size). Grid
  sharpness is secondary anyway — focus is checked in the 1:1 loupe, not the grid tile.
- **Turbo Boost caveat:** "speedup vs 1 thread" conflates parallel scaling with clock dropoff
  (single-thread ~4 GHz turbo vs all-core ~3 GHz), so apparent 4-core scaling understates true efficiency.
- **Camera embeds a pyramid:** prefer *extracting* an embedded JPEG over *generating* one. CR2 IFD0 is a
  full-res JPEG ⇒ RAW-only path converges with the JPEG path (one downstream pipeline; zero RAW decode).

### Final Numbers

**Embedded images (exiftool, Canon 6240×4160 / 26 MP):**

| Image | Embedded preview (IFD0) | Thumbnail (IFD1) | Mid (~1920)? |
|---|---|---|---|
| CR2 | 6240×4160 JPEG, 1.5–2 MB | 160×120, ~13–17 KB | none |
| JPG | (file itself) 6240×4160 | 160×120, ~13 KB | none |

→ tiny thumb = free extract (~11 MB for 879 in RAM); grid preview must be generated (no mid-size embedded).

**Parallel scaling** (999×, in-RAM bytes, decode 1/2, i7-1068NG7 = 4 physical / 8 logical):

| Threads | Time | Speedup vs 1 |
|---|---|---|
| 1 | ~100.5 s (100.6 ms/fr) | 1.0× |
| 2 | ~54 s | 1.85× |
| 4 | ~30 s | 3.33× |
| 8 | ~23.5 s (23.8 ms/fr) | **4.26×** |

→ near-linear to 4 physical cores; HT adds ~6%. **Inverse of step 1** (I/O-bound, parallel hurt).

**Scaled-decode** (real pipeline decode→resize-1920→encode, 8 threads, 999×):

| Factor | Decoded | Time | vs 1/1 |
|---|---|---|---|
| 1/1 (6240) | 26 MP | ~48 s | 1.0× |
| 1/2 (3120) | 6.5 MP | ~22.7 s | 2.1× |
| **3/8 (2340)** | 3.6 MP | ~17.7 s | **2.7×** |
| 1/4 (1560, upscales) | 1.6 MP | ~15.1 s | 3.2× |

→ **decode dominates**; chose **3/8** (smallest downscale-only factor); 1/4 is ~15% faster but upscales (rejected).

**Decisions locked:** decode **3/8** · filter **Lanczos3** · RGB/U8x3 · per-frame `Decompressor` (per-thread reuse = later optimization).

### Next step

Step 2 continues: (1) compose `make_preview(&[u8]) -> Option<Vec<u8>>` + real `pick_scale`; (2) extract
IFD1 thumb; (3) RAW-only CR2 IFD0 path; (4) `ProcessedPreview` channel streaming + two-tier concurrency
(bounded card reads feeding the CPU pool).

---

## Session 3 — Compose preview unit + IFD1 thumb extraction + EXIF builder refactor (2026-06-13)

### What was built / explored

- **`src/preview.rs` composed into the real unit:**
  - `make_preview_from_jpeg_bytes(&[u8]) -> Option<Vec<u8>>` — decode → resize → encode, all `?`-funnelled
    (the earlier `panic!("lol")` scaffolding removed; one bad frame now returns `None`, never aborts the batch).
  - `pick_scaling_factor(src_dim, target_dim)` — replaces the hardcoded factor. Iterates
    `turbojpeg::Decompressor::supported_scaling_factors()`, filters to factors whose scaled edge ≥ target,
    `min_by_key` on the scaled edge → smallest downscale-only factor. Collapsed from a HashMap+index-sort
    (which relied on the factor list's order) to a 4-line iterator that depends on **no ordering at all**.
    Fallback `ScalingFactor::ONE` for already-small sources.
  - `decompress` now derives the factor from the real header **long edge** (`header.width.max(header.height)`),
    not a literal — so it re-derives 3/8 for the 6240px Canon by rule, and adapts to any sensor/crop.
  - `resize` made **orientation-aware**: clamp the long edge to 1920, scale the short edge proportionally,
    assign 1920 to width or height by `src.width >= src.height`. (Old code pinned width=1920 → squashed
    portraits / inverted aspect.)
- **IFD1 thumbnail extraction (`src/lib.rs`, option B — takes `&Exif`):**
  - `get_thumbnail(exif: &Exif) -> Option<Vec<u8>>` — reads `Tag::JPEGInterchangeFormat` (offset) +
    `JPEGInterchangeFormatLength` (length) at `In::THUMBNAIL` (IFD1), both `Value::Long`, then a
    **fallible slice** `exif.buf().get(offset..offset+len)?` → `.to_vec()`. Free extract: a complete
    standalone JPEG, zero transcode (~13 KB).
- **EXIF read refactor (one open, one parse, fan-out):**
  - `read_exif_container(&PhotoAsset) -> Option<Exif>` — JPEG-preferred / RAW-fallback open + parse.
  - `get_capture_time(&Exif)` (renamed from the old `&PhotoAsset` reader) + `get_thumbnail(&Exif)` both
    consume the **same** parsed container.
  - `extract_exif_data(&Exif) -> ExifAssetData { captured_at, thumbnail }` — the **single extension point**:
    adding a future property = struct field + extractor fn + one line here, compiler-enforced via exhaustive
    struct literal.
  - `enrich_assets` folded from two passes (date pass + thumb pass, all containers materialised) into **one
    `filter_map` pass**; the container drops inside the closure → bounded memory, one read serves all extractors.
  - `PhotoAsset.captured_at` → grouped under `exif_data: ExifAssetData`.
- **Test strengthened:** `enrich_assets_test_thumbnail_filled_successfully` now asserts the JPEG markers —
  `starts_with(&[0xFF,0xD8])` (SOI) + `ends_with(&[0xFF,0xD9])` (EOI) + a size band — proving a complete
  JPEG was carved at the right boundaries, not just that bytes exist.

### Errors and fixes / wrong hypotheses

- **`pick_scaling_factor` returned 1/2 instead of the locked 3/8.** Cause: passed `header.height` (short
  edge, 4160) — constraining the *short* edge ≥ 1920 forces a larger factor. Fix: pass the **long edge**
  (`width.max(height)` = 6240); 6240×3/8 = 2340 ≥ 1920, 6240×1/4 = 1560 < 1920 → 3/8 by rule.
- **Resize "fix" regressed landscape.** A formula using `long/short` (instead of `short/long`) inverted the
  aspect → 1920×2880 squash. Real issue was deeper: `Image::new(1920, short)` hardcodes 1920 to *width*, so
  no single formula handles portrait. Fix = orientation conditional that moves 1920 between width/height.
- **`.collect()` type error** — outer `.map()` over `Option<(K, ExifAssetData)>` won't collect into `Vec<(K, …)>`.
  Fix: `filter_map` (drops `None`, unwraps `Some`) — the funnel doing its job (unparseable EXIF → asset
  silently keeps `None` defaults).
- **Latent `.unwrap()` panic** in a leftover debug block inside `get_thumbnail` (`get_capture_time(exif).unwrap()`
  + per-frame `fs::write` + `println!`) — would detonate on any thumb-bearing frame lacking a date. Removed.
- **`**&v.first()?`** redundant deref-of-ref (clippy `deref_addrof`) → `*v.first()?`.
- **Over-abstraction avoided:** considered a `trait Extractor` + registry for extensibility; rejected — the
  properties are heterogeneously typed (`NaiveDateTime`, `Vec<u8>`, future GPS/lens/ISO), so a struct-of-Options
  + one builder fn is *more* extensible and compiler-checked than type-erased trait objects.

### Key discussion points (mental models)

- **The embedded pyramid, two reaches:** IFD1 (`In::THUMBNAIL`) via `JPEGInterchangeFormat`/`Length` = the
  160×120 thumb; IFD0 (`In::PRIMARY`) = full-res preview (next step, via Strip tags on CR2). Both are
  byte-slices out of the parsed EXIF buffer — extract, never generate, zero RAW decode.
- **Offset reference frame:** EXIF offsets are relative to the TIFF header, which is exactly what
  kamadak-exif's `Exif::buf()` returns → `buf.get(offset..offset+len)` aligns. `get()` over indexing keeps
  the funnel (out-of-range → `None`, no panic).
- **Extensibility = exhaustive struct literal.** `extract_exif_data` is the open/closed boundary: the
  traversal loop (`enrich_assets`) never changes when a property is added; only the builder does, and the
  compiler refuses to build until the new field is wired.
- **One open / one parse / drop-after-use** is both the memory fix *and* the literal precursor to the step-4
  streaming loop (swap "apply" for "send `ProcessedPreview`").
- **Memory tiers (5000 frames):** thumbs ~65 MB (always resident) · compressed previews ~0.75–2.4 GB (≈1.5 GB
  typical, persist to disk per plan) · **decoded RGBA ~47 GB** (never resident → virtualized grid + LRU
  texture cache). A 1920px preview is ~64× larger decoded than compressed — that ratio is *why* tier 3 must
  be an LRU cache, not an array.

### Decisions / state

- `pick_scaling_factor` rule (smallest downscale-only `M/8`) locked; re-derives 3/8 for 6240→1920.
- Thumb extraction = option B (`&Exif`-taking), free byte-slice, both JPG and RAW paths.
- Open question deferred to step 4: previews persisted to disk (SQLite blob path, per plan) vs held in RAM.
- EXIF orientation (portrait frames stored landscape + `Orientation` tag) = known display-time item, parked.
- `cargo test` green (thumbnail markers + size band); clippy expected clean after the `deref_addrof` fix.
