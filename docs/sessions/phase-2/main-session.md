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

## Next step

Step 2 — previews: decode parallel `.JPG` / extract embedded full-size preview for RAW-only, downsample
to ~1920px JPEG bytes in memory, stream over a channel. First CPU-bound work where parallelism *does*
pay — and where the deferred Rayon budget gets spent.
