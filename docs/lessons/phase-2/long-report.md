# Phase 2 — CPU-Bound Processing & Streaming: Comprehensive Student Textbook

> **Audience:** someone who missed every session. This document teaches Phase 2 from first
> principles: reading capture times out of EXIF, the difference between I/O-bound and CPU-bound
> work and why parallelism helps one and hurts the other, and building a fast JPEG preview
> pipeline. Every term is defined on first use; every measured number carries its hardware context.
>
> **Hardware used for all measurements:** MacBook Pro, Intel **i7-1068NG7** (Ice Lake, **4 physical
> cores / 8 logical** via Hyper-Threading, 2.3 GHz base / ~4.1 GHz single-core turbo). Source media:
> a Canon SD card in a USB reader, and an external USB SSD (`GenericSSD`). Test corpus: an 879-asset
> Canon card, frames at **6240×4160 (~26 MP)**. Rust release builds throughout.

---

## Part 1: Capture-Time Enrichment

### 1.1 The motivation

Phase 1 produced a map of `PhotoAsset`s keyed by `(dir, stem)`. Each asset knew which files it had
(RAW, JPEG, or both) but **not when it was shot**. Routing photos into `YYYY/YYYY-MM-DD/` folders
needs that date. Phase 2 step 1 reads the capture time from each frame's EXIF metadata and stores it
on the asset, where it feeds `build_destination_path`.

### 1.2 EXIF, and why the date type matters

**EXIF** (Exchangeable Image File Format) is a block of metadata cameras embed in JPEG and RAW files.
The tag we want is `DateTimeOriginal` — the moment the shutter fired — stored as the ASCII string
`"YYYY:MM:DD HH:MM:SS"` (note the **colons in the date**, which is *not* ISO 8601).

The first design decision was the Rust type for this value. The field had been a placeholder
`Option<String>`. The candidates from the `chrono` crate:

- `DateTime<Utc>` — an absolute instant in UTC.
- `DateTime<Local>` — an instant in the machine's local zone.
- `NaiveDateTime` — a date **and** time with **no timezone** attached.
- `NaiveDate` — just a date.

The load-bearing fact: **`DateTimeOriginal` carries no timezone.** It is the camera's wall-clock
reading. EXIF 2.31 added a separate `OffsetTimeOriginal` tag, but Canon CR2s routinely omit it. So
you cannot know the UTC offset.

That eliminates `DateTime<Utc>`. To construct a UTC instant you would have to *invent* an offset.
Worse, routing needs a *date* (`build_destination_path` takes a `NaiveDate`). Converting a naive
camera reading → UTC instant → back to a local date round-trips through the invented offset, and a
frame shot at 00:30 could land in the **previous day's folder**. That is silent routing corruption —
exactly the class of bug Phase 1 fought to avoid.

So the type that tells the truth about the data is the one that **refuses to pretend it knows the
zone**: `NaiveDateTime`. We kept it `Option` because RAW-only frames and JPEGs with stripped EXIF
genuinely have no date, and we chose `NaiveDateTime` over `NaiveDate` so sub-second precision (for
future burst-grouping) is preserved — the routing call projects `.date()` at the boundary:
`captured_at.map(|dt| dt.date())`. Crucially, `build_destination_path`'s signature did **not** change;
Phase 1 had already shaped it to take the date as a parameter.

### 1.3 Reading EXIF with `kamadak-exif`

`kamadak-exif` is the *package* name on crates.io; it **imports as `exif`** (a common quirk — the
crate name differs from the package). It is read-only and ideal for pulling dates.

The crate does not hand you typed accessors like `.date_time_original()`. It parses the EXIF blob into
a flat collection of `Field`s, each keyed by a `Tag` and an `In` (which **Image File Directory**, or
IFD — the primary image vs the thumbnail). You look up a tag and get a `Value`, an enum whose variant
for date tags is `Value::Ascii(Vec<Vec<u8>>)` (EXIF ASCII fields are arrays of byte-strings).

The reading sequence, with the *why* for each step:

```rust
fn read_capture_time(asset: &PhotoAsset) -> Option<NaiveDateTime> {
    // Pick the file: JPEG first (small APP1 header, cheap), else the RAW (CR2/TIFF, larger
    // container but kamadak-exif reads it). AssetFiles is non-empty by construction, so in
    // practice one of these is always Some.
    let file = std::fs::File::open(
        asset.files.get(FileKind::Jpeg)
            .or_else(|| asset.files.get(FileKind::Raw))?,
    ).ok()?;

    // read_from_container seeks inside the file, so it needs a *mutable* buffered reader.
    let mut bufreader = std::io::BufReader::new(file);

    // Parse the EXIF container. Err here = no EXIF / IO error -> None.
    let exif = exif::Reader::new().read_from_container(&mut bufreader).ok()?;

    // DateTimeOriginal lives in the EXIF sub-IFD, surfaced under In::PRIMARY.
    let field = exif.get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)?;

    // Match the value as ASCII and take the first element. ref vec borrows instead of
    // moving out from behind the &Field. (Equivalent: `match &field.value`.)
    let bytes = match field.value {
        exif::Value::Ascii(ref vec) => vec.first()?,
        _ => return None,
    };

    let s = std::str::from_utf8(bytes).ok()?;

    // EXIF uses colons in the date: "%Y:%m:%d %H:%M:%S", NOT chrono's default ISO parser.
    NaiveDateTime::parse_from_str(s, "%Y:%m:%d %H:%M:%S").ok()
}
```

**The failure funnel.** Every line ends in `?` or `.ok()?`. There is not a single `match` for error
*handling* — every failure mode (can't open, no EXIF block, tag absent, wrong value type, bad UTF-8,
unparseable date) funnels to `None`. This is deliberate: on a 10k-frame card, one corrupt frame must
never abort the batch. A real gotcha this absorbs for free: cameras with a dead clock battery emit
`"0000:00:00 00:00:00"`, which fails `parse_from_str` (month 00 is invalid), so `.ok()` maps it to
`None` — the parser rejects nonsense dates without a hand-written validator.

**`ref` in the pattern.** `field` is a `&Field` (from `get_field`), so `field.value` is reached
through a shared reference. A bare pattern `Value::Ascii(vec)` would try to **move** the
`Vec<Vec<u8>>` out from behind the `&`, which is forbidden for non-`Copy` types. `ref vec` binds
`vec` as a **reference** (`&Vec<Vec<u8>>`) instead — borrow, don't move. Modern Rust gets the same
effect implicitly by matching on `&field.value` ("match ergonomics").

### 1.4 Compute-then-apply

The driver, `enrich_captured_at`, fills every asset's date. It is split deliberately:

```rust
pub fn enrich_captured_at(assets: &mut HashMap<AssetKey, PhotoAsset>) {
    // 1. COMPUTE: read each date, producing owned (key, date) pairs. No shared mutation.
    let pairs: Vec<(AssetKey, Option<NaiveDateTime>)> = assets
        .iter()
        .map(|(key, asset)| (key.clone(), read_capture_time(asset)))
        .collect();
    // 2. APPLY: write the results back.
    for (key, dt) in pairs {
        if let Some(asset) = assets.get_mut(&key) { asset.captured_at = dt; }
    }
}
```

Why this shape: the per-asset unit (`read_capture_time`) captures nothing shared and returns owned
data, so it is `Send` and side-effect-free. That makes the serial→parallel switch a one-token change
(`.iter()` → `.par_iter()`) instead of a borrow-checker fight, and it matches the streaming endgame
(the "apply" step becomes "send down a channel"). **This decision is what made Part 2's measurement
trivial to run.**

---

## Part 2: I/O-Bound vs CPU-Bound — The Heart of the Phase

This is the conceptual core. The same operation can be *bound* by completely different hardware
depending on where its time goes, and that determines whether adding threads helps or hurts.

### 2.1 Definitions

- **CPU-bound:** the limiting resource is compute — the cores are busy doing arithmetic. Adding more
  cores does more work in parallel.
- **I/O-bound:** the limiting resource is a device (disk, card, network). The CPU spends most of its
  time *waiting* for bytes. Adding cores doesn't make the device faster.
- **Parallelism:** many **cores** doing **work** at once (Rayon's domain).
- **Concurrency:** many **operations in flight** at once, independent of core count. For I/O-bound
  work this is what you want — keep the device's queue full so latency hides behind throughput.

### 2.2 The cold/warm cache trap

The OS keeps recently-read file data in the **page cache** (RAM). The *first* read of a file is
**cold** (bytes physically fetched from the device); subsequent reads are **warm** (served from RAM).
The gap is enormous and it is the most common way storage benchmarks lie.

We benchmarked `enrich_captured_at` over the 879-asset card, running it 3× in one process:

| Run | Cache | Per-frame | Throughput |
|---|---|---|---|
| 1 | **cold** | 5.51 ms | 181 frames/s |
| 2 | warm | 51.9 µs | 19,286 frames/s |
| 3 | warm | 45.3 µs | 22,065 frames/s |

**That ~120× gap is the finding.** Run 1 is I/O-bound: 5.5 ms/frame is the SD card reader's
open+seek+header-read latency; the CPU work (parse a 19-byte string) is nanoseconds and invisible.
Runs 2–3 are CPU/syscall-bound: the same code, 120× faster, because no physical I/O happens.

**Methodology rule that falls out of this:** a cold measurement is **one-shot per cache state**. The
instant run 1 finishes, the data is warm. To get a second honest cold number you must evict the cache
(`sudo purge` on macOS, or unmount/remount the card). If you only looked at run 3 (22k frames/s) you
would conclude "trivially fast, parallelise it" — exactly backwards, because a real card ingest reads
each file exactly once, **cold**. Optimising the warm path optimises a path that never runs.

### 2.3 Parallelism's effect flips with the device — queue depth

We then compared serial vs parallel (`par_iter`) **cold** on two devices:

| Device | Serial cold | Parallel cold | Effect |
|---|---|---|---|
| **SD card** (USB) | 4.85 s · 181 fps | 6.20 s · 142 fps | **−28% (HURTS)** |
| **SSD** (`GenericSSD`) | 0.61 s · 1432 fps | 0.17 s · 5256 fps | **+3.7× (HELPS)** |

Same code, opposite verdicts. The explanation is **queue depth** — a device's ability to service
multiple outstanding requests at once:

- An **SD card over a USB reader is effectively serial**: one command path, no internal parallelism.
  Eight threads issuing reads get serviced one at a time anyway, **and** their scattered random-access
  pattern destroys the sequential read-ahead the single-threaded path enjoyed. You pay Rayon's
  scheduling overhead for nothing. Net loss: −28%.
- An **SSD is internally many devices**: multiple NAND dies/channels plus a real command queue (SATA
  NCQ depth 32; NVMe far deeper). The serial version leaves most of that idle, paying full round-trip
  latency per read. Eight concurrent reads keep the queue full and the controller overlaps them across
  dies — latency hides behind throughput. Net win: 3.7×.

Note also the cold/warm gap shrank from ~120× (SD) to ~13× (SSD): a faster device has less I/O penalty
to hide, so cold creeps toward the CPU-bound warm floor.

**The architectural conclusion:** *optimal source-read concurrency is a property of the device, not a
constant.* Hardcoding "always serial" is 3.7× too slow on an SSD; hardcoding "always parallel" is 28%
too slow (and thrashing) on the camera card you'll use most. This is the empirical justification for
the plan's **source-read semaphore tier** with a *tunable* permit count (1–2 for cards, higher for
SSD/NVMe). Enrichment ships **serial by default** because the primary source is a card.

### 2.4 Why I/O concurrency needs threads (the readiness model)

If you *did* want concurrent card reads, how? Not with a single async thread. The reason one thread
can juggle 10,000 network sockets (epoll/kqueue) is that a socket can be **not ready** — "no data yet,
come back later" — so the thread parks and services whatever is ready. A **regular file is always
considered ready**: `read()` doesn't return "not ready", it **blocks the calling thread** until the
device delivers bytes. There is no "come back later" to hang an event loop on.

Consequence: for blocking file reads, **one operation in flight costs one thread** parked in the
kernel. You cannot drive N concurrent file reads from one thread with kqueue. But — and this is the
reframe — those threads burn **zero CPU** while blocked, so you can have 16 threads on 8 cores without
CPU contention; 15 are asleep waiting on the device. The number you tune (call it **D**, the queue
depth) is decoupled from core count. Mechanisms: a sized Rayon pool, a bounded worker-pool + channel,
or `tokio` (whose `tokio::fs` is itself a blocking thread pool under the hood on macOS). The one true
single-thread-many-in-flight mechanism is **`io_uring`** — but it is **Linux-only**; macOS has no
equivalent, so bounded threads it is. That bound **D is the source-read semaphore permit count.**

---

## Part 3: The Preview Pipeline

Step 2's job: turn each frame into compressed JPEG bytes for the triage UI — a small **thumbnail**
(always in RAM) and a larger **grid preview** (~1920px) — streamed as each asset finishes. This is the
first genuinely **CPU-bound** work, where parallelism finally pays.

### 3.1 The invariant: zero RAW decoding

Decoding RAW sensor data (demosaicing) is hundreds of milliseconds per frame. Previews must come from
an **already-encoded JPEG** — the parallel `.JPG`, or a JPEG **embedded inside** the RAW — never from
decoding RAW pixels. This is a hard architectural line.

### 3.2 The camera already built a pyramid

A camera RAW/JPEG is not one image; it embeds a small pyramid for the camera's own UI. We measured
exactly what a Canon CR2/JPG contains with `exiftool` (after extracting each embedded image and
reading its dimensions with `sips`):

| Image | Embedded preview (IFD0) | Thumbnail (IFD1) | Mid-size (~1920)? |
|---|---|---|---|
| CR2 | **6240×4160**, 1.5–2 MB JPEG | **160×120**, ~13–17 KB | **none** |
| JPG | *(the file itself)* 6240×4160 | **160×120**, ~13 KB | **none** |

Two consequences:

1. **The 160×120 thumbnail is free.** Both CR2 and JPG carry it in EXIF IFD1. Extract the bytes — no
   decode, no resize. For 879 frames that is ~11 MB of compressed thumbnails held in RAM (≈130 MB even
   at 10k frames). The "all thumbnails always in memory" assumption holds easily.
2. **No mid-size preview exists**, so the ~1920px grid tile must be **generated** by downscaling the
   full-res image. (Some camera bodies *do* embed a medium preview — a real pipeline should *probe and
   reuse if present, generate if absent*. For these files, generate.)

**The convergence win:** because the CR2's IFD0 preview is a *full-resolution* JPEG, the RAW-only path
and the JPEG path **converge** — both yield a 6240×4160 JPEG. So one downstream decode→resize→encode
pipeline serves both, "zero RAW decode" is preserved for free, and the only per-type difference is
stage 1 (open `.JPG` file vs extract the IFD0 blob from the CR2).

### 3.3 The pipeline and the crate stack

Per frame, on JPEG bytes already in RAM:

1. **Scaled decode** (`turbojpeg` → libjpeg-turbo): JPEG → RGB pixels, decoded at a reduced size.
2. **Resize** (`fast_image_resize`): downscale to 1920px long edge, aspect-preserved.
3. **Encode** (`turbojpeg`): RGB → compressed JPEG `Vec<u8>`, quality 85, 4:2:0 subsampling.

`turbojpeg` binds **libjpeg-turbo** (SIMD-accelerated JPEG codec); `fast_image_resize` is a SIMD
resampler.

### 3.4 Scaled (DCT-domain) decode — the dominant lever

A JPEG is stored as 8×8 blocks of **DCT** (Discrete Cosine Transform) coefficients. libjpeg-turbo can
produce a **downscaled** image *during* decode, at factors `M/8` (M = 1..16), by taking fewer
coefficients per block — **far** cheaper than fully decoding then resizing, because it never
materialises the full-resolution pixels.

You never decode 26 MP to make a 2 MP tile. The rule: pick the **smallest `M/8` whose long edge ≥ the
target**. For 6240 → 1920: 1/4 gives 1560 (below 1920, would upscale — reject); **3/8 gives 2340**
(smallest factor ≥1920). Decode at 3/8, then resize 2340→1920.

Measured impact (real pipeline decode→resize-1920→encode, 8 threads, 999 frames):

| Decode factor | Decoded px | Time | vs 1/1 |
|---|---|---|---|
| 1/1 (6240) | 26 MP | ~48 s | 1.0× |
| 1/2 (3120) | 6.5 MP | ~22.7 s | 2.1× |
| **3/8 (2340)** | 3.6 MP | **~17.7 s** | **2.7×** |
| 1/4 (1560, upscales) | 1.6 MP | ~15.1 s | 3.2× |

**Decode dominates total cost** — going 1/1→3/8 is a 2.7× win before any other tuning. 1/4 is ~15%
faster still but it *upscales* 1560→1920, softening every tile, so it was rejected for a focus-triage
tool. **3/8 is the locked choice.**

### 3.5 Resize is self-funding; the filter is a quality decision

We benchmarked "decode→resize→encode" vs "decode→encode" (no resize) and they came out within noise.
The reason is not that resize is free — it is that **resizing shrinks the image, so the *encode* gets
cheaper**, paying back the resize cost. Resize is *self-funding*. (And you need the 1920 output anyway,
so targeting it costs nothing over not resizing.)

Because resize is a small fraction of total time, the **filter choice barely moves the clock** —
`Lanczos3` (sharp, slower) vs `Bilinear` (fast, softer) are time-neutral here. So the choice is
**visual, not numerical**. We generated samples and could see no difference at tile size — and grid
sharpness is secondary anyway, because focus is checked in the 1:1 **loupe** (full-res source), not
the grid tile. **Lanczos3** was kept (no cost, no downside).

---

## Part 4: Parallel Scaling — and the Turbo Boost Caveat

The preview pipeline is CPU-bound, so parallelism should scale with cores. We swept
`RAYON_NUM_THREADS` over 999 iterations of one in-RAM JPEG (decode 1/2):

| Threads | Time | Speedup vs 1 |
|---|---|---|
| 1 | ~100.5 s (100.6 ms/frame) | 1.0× |
| 2 | ~54 s | 1.85× |
| 4 | ~30 s | 3.33× |
| 8 | ~23.5 s (23.8 ms/frame) | **4.26×** |

**This is the inverse of Part 2.** Enrichment was I/O-bound and parallelism *hurt* on the card;
preview generation is CPU-bound and parallelism *helps*, scaling ~linearly to the **4 physical cores**
(4.26× — the 8 hyperthreads added ~6% beyond the 4× the physical cores give). JPEG codec work is
SIMD/execution-port-heavy, and two **Hyper-Threads** on one physical core *share* those ports, so HT
contributes a sliver, not 2×.

**The Turbo Boost caveat:** the 4-thread number looks sub-linear (3.33×, not 4×), but that is largely
**Turbo Boost**, not poor scaling. A single thread runs at ~4.1 GHz turbo; all-core runs throttle to
~3 GHz. "Speedup vs 1 thread" therefore *conflates* parallel efficiency with clock dropoff — the true
per-clock scaling is better than 3.33× suggests. On modern CPUs the baseline clock is not the all-core
clock; remember this whenever a speedup looks disappointing.

---

## Part 5: Benchmark Results (consolidated)

All on i7-1068NG7 (4 physical / 8 logical), Rust release.

**Enrichment, 879-asset card, cold = first read:**

| Device | scan (cold) | serial cold | parallel cold | parallel effect | parsed |
|---|---|---|---|---|---|
| SD card (USB) | 3.75 s | 4.85 s · 181 fps | 6.20 s · 142 fps | −28% | 879/879 |
| SSD (`GenericSSD`) | 34 ms | 0.61 s · 1432 fps | 0.17 s · 5256 fps | +3.7× | 879/879 |

- Cold/warm gap: SD ≈120×, SSD ≈13×. 0 collisions, 0 errors. `cargo test` 5 passing, clippy clean.

**Embedded images (Canon, 26 MP):** CR2 IFD0 = 6240×4160 JPEG (1.5–2 MB); IFD1 thumb = 160×120
(~13 KB); no mid-size. JPG carries the same 160×120 IFD1 thumb.

**Preview parallel scaling** (999×, in-RAM, decode 1/2): 1→100.5 s, 2→54 s, 4→30 s, 8→23.5 s = 4.26×.

**Scaled decode** (real pipeline, 8 threads): 1/1→48 s, 1/2→22.7 s, **3/8→17.7 s**, 1/4→15.1 s.

**Locked decisions:** decode 3/8 · filter Lanczos3 · RGB/U8x3 · per-frame `Decompressor`.

---

## Part 6: Common Errors Encountered

| Error / symptom | Cause | Fix |
|---|---|---|
| Parallel enrichment *slower* on SD card (6.2 s vs 4.85 s) | One SD card is a serial device; concurrent random reads thrash read-ahead, add Rayon overhead | Keep enrichment serial by default; concurrency is device-tuned (Part 2.3) |
| "Enrichment is trivially fast" (22k fps) | Read warm-cache run 3, not the cold first read | Measure cold; evict cache between cold runs; a real ingest is always cold |
| `cannot borrow src as mutable, captured in a Fn closure` | `preview_from_jpeg_bytes(&mut [u8])` — but JPEG input is read-only and a `Fn` closure can't give `&mut` to N threads | Take `&[u8]` (shared, `Sync`); the `&mut` was both wrong and the thing blocking parallelism |
| `Image::new()` / `Resizer::new()` "don't exist" | (a) `fast_image_resize` was never actually added (`cargo add` only took the first arg); (b) `use turbojpeg::Image` shadowed the resizer's `Image` | `cargo add fast_image_resize`; alias one type (`use fast_image_resize::images::Image as FirImage`) |
| `no method as_deref found for Vec<u8>` | `resize` returned bare pixel bytes, dropping width/height/format the encoder needs | Thread `turbojpeg::Image<Vec<u8>>` through every stage; emit `Vec<u8>` only at `compress` |
| Skewed / striped decoded image (latent) | `pitch = 4*width` (RGBA stride) with `format: RGB` (3 bytes) breaks `from_vec_u8`'s tight-stride assumption | RGB ⇒ `pitch = 3*width`, buffer `3*w*h` |
| `"0000:00:00 00:00:00"` would-be crash | Dead-clock-battery cameras emit it | `parse_from_str` rejects month 00 → `.ok()` maps to `None` for free (no fix needed; design absorbs it) |
| `turbojpeg` fails to link | `turbojpeg-sys` builds libjpeg-turbo and needs cmake + nasm (SIMD assembler) | `brew install jpeg-turbo cmake nasm`; isolate this as the first build gate before writing pipeline code |
| `make_preview` "scaling is wrong" | Mislabelled bench prints; swapped "with/without resize" strings | Fix labels; trust numbers only after |

---

## Summary

- **The same operation is I/O-bound or CPU-bound depending on where its time goes, and that decides
  whether threads help.** Enrichment (tiny EXIF header reads) is I/O-bound: parallel *hurt* on the SD
  card (−28%) but *helped* 3.7× on an SSD. Preview generation (JPEG decode/resize/encode) is CPU-bound:
  parallel scaled ~linearly to 4 physical cores (4.26×). Measured both directions.
- **Optimal source-read concurrency is a device property, not a constant** — the empirical basis for a
  tunable source-read semaphore tier (low for cards, high for SSD/NVMe).
- **Cold-cache measurement is one-shot.** Warm numbers (120× faster) describe a path that never runs in
  a real first ingest. Always measure cold; evict between runs.
- **Scaled (DCT-domain) decode is the dominant cost lever** in the preview pipeline: 1/1→3/8 = 2.7×.
  Resize is self-funding (it shrinks the encode), and the filter choice is visual, not timing.
- **The camera already embedded a pyramid:** extract the free 160×120 thumbnail; the full-res IFD0 JPEG
  makes the RAW-only and JPEG paths converge into one pipeline with zero RAW decoding.
- **Modern-CPU caveats:** Hyper-Threads share execution ports (HT ≈ +6% on SIMD codec work, not 2×),
  and Turbo Boost makes the single-thread baseline run faster than all-core, so "speedup vs 1 thread"
  understates true parallel efficiency.
