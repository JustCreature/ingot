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

## Part 5: The Seek Machinery — Reading Only What You Need

Parts 1–4 read either a tiny EXIF header or a JPEG *already in RAM*. The real pipeline must get those bytes off the card first, and **how many bytes you read is the whole game** (Part 7 proves the card is the bottleneck). This part is about reading the *minimum*.

### 5.1 The slurp problem

`kamadak-exif`'s `read_from_container` behaves very differently by format:

- **JPEG:** it walks the marker segments and stops after the **APP1** segment (where EXIF lives), which sits at the very front. Cheap — this is why Part 2's enrichment was fast.
- **TIFF-like (CR2):** it reads the **entire file** into its internal buffer (`exif.buf()`). Measured: a 36 MB CR2 leaves **36 MB resident** just to read a 20-byte date. That is the *slurp*, and on a card-bound pipeline it is fatal.

### 5.2 Where the bytes actually live (measured with `exiftool -n`)

For a Canon CR2 (`IMG_1939.CR2`, 34.9 MB):

| Item | Offset | Length | Ends at |
|---|---|---|---|
| IFD1 thumbnail (160×120) | 54,424 | 17,344 | **71,768** |
| IFD0 full-res preview | 71,768 | 2,039,424 | 2,111,192 |
| `DateTimeOriginal` (ExifIFD) | near front | ~20 B | — |

Everything **stage 1** needs — the date *and* the full 160×120 thumbnail — lives in the **first ~72 KiB**. The 2 MB preview begins exactly where the thumbnail ends, so a bounded prefix cleanly excludes it.

### 5.3 Stage 1 — the bounded prefix read

Read only the head of the file and parse EXIF from that:

```rust
const EXIF_HEADER_PREFIX_LEN: u64 = 128 * 1024; // 72 KiB needed + headroom

fn read_exif_header_from_path(path: &Path, max_bytes: u64) -> Option<exif::Exif> {
    let file = std::fs::File::open(path).ok()?;
    let mut head = Vec::with_capacity(max_bytes as usize);
    file.take(max_bytes).read_to_end(&mut head).ok()?;       // ≤ max_bytes, one alloc
    exif::Reader::new().read_from_container(&mut std::io::Cursor::new(head)).ok()
}
```

**The `Take`-isn't-`Seek` gotcha.** `read_from_container` requires `BufRead + **Seek**` (it seeks within the TIFF to follow IFD offsets). `Read::take(n)` yields only `Read` — no `Seek` — so you cannot lazily cap the reader in place. The fix: read the prefix into a `Vec`, then wrap it in a `Cursor` (which *is* `BufRead + Seek`). `take(n).read_to_end` both caps the read **and** tolerates files shorter than the budget (no `UnexpectedEof`); `with_capacity` makes it a single allocation.

**Truncation tolerance.** kamadak parses the IFD directories and their *inline* field values (the date string at its offset, the strip offsets as numbers) — all near the front, all inside 128 KiB. A field whose *data* lands beyond the prefix (e.g. the thumbnail bytes if the prefix were too small, or the 2 MB preview) simply fails its `buf.get(range)?` slice and funnels to `None`; the parse itself does not abort. Verified: at 128 KiB both date and thumb extract; at 64 KiB (below the thumb's end of 71,768) the date still resolves but the thumbnail funnels to `None` — exactly the boundary you'd predict.

**Measured:** 128 KiB vs 36 MB = **279× fewer bytes, 264× faster** (warm; cold is larger because the slurp physically pulls 36 MB off the card).

### 5.4 Stage 2 — the targeted seek-read

Stage 1 already parsed the IFD0 `StripOffsets`/`StripByteCounts` (inline, in the prefix), so it **caches the strip reference** on the asset — stage 2 needs no second header parse. Then:

```rust
pub fn read_embedded_preview(path: &Path, strip: PreviewStrip) -> Result<Vec<u8>, …> {
    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();                     // free fstat, 0 content bytes
    if strip.len == 0 || strip.offset.checked_add(strip.len as u64)? > file_len {
        return Err(…);                                          // corrupt ref → funnel
    }
    file.seek(SeekFrom::Start(strip.offset))?;
    let mut buf = vec![0u8; strip.len];
    file.read_exact(&mut buf)?;                                 // exactly the strip (~2 MB)
    Ok(buf)
}
```

One `seek` + one `read_exact` of exactly the ~2 MB strip — never the 36 MB file. **The allocation bound is a fact, not a guess:** a strip cannot extend past its file, so `offset + len ≤ file_len` (checked against a free `fstat`, which reads zero content bytes) rejects a corrupt `StripByteCounts` before the `vec![0; len]` can OOM. An earlier version used a magic 64 MB cap — wrong twice over: too small for future 150 MP medium-format previews, and an arbitrary number where the file length is the real invariant.

This gives **three preview tiers**: the free 160×120 IFD1 thumb (placeholder, from the header) → a generated **512** grid thumb → a generated **1920** loupe preview, the latter two decoded from the 2 MB embedded strip (RAW) or the standalone `.JPG` (JPEG-only).

### 5.5 Why the embedded preview is *smaller* than the standalone JPEG

A surprising observation: at the **same resolution**, the CR2's embedded preview (~2–3 MB) is smaller than the standalone `.JPG` (~5–8 MB). It is not lower resolution — it is **harder-compressed**, on three axes: a lower JPEG quality factor (coarser quantization tables), **4:2:0** chroma subsampling (half the colour data) vs the deliverable JPEG's 4:2:2, and the simple fact that it is a *throwaway* — the camera knows the RAW is the master, so the embedded preview only has to be good enough to chimp on the camera's screen.

This sets up the **pair decision**. For a RAW+JPEG pair you can preview from either. `get_prioritized` is **RAW-first**, so a pair uses the *embedded* preview — fewer bytes, which (Part 7) means proportionally faster on a card-bound pipeline. The cost is the lower embedded quality; the accepted tradeoff is speed for triage. (Metadata reads RAW-first too, so the strip ref and the preview source agree — see the Part 9 bug where they once disagreed.)

---

## Part 6: Streaming Ingest & Engine-Owned State

### 6.1 Who owns the data

The **engine owns the asset store**: `Engine.scan_response: Arc<RwLock<ScanResponse>>`. The UI *borrows* it to present. An earlier design inverted this — the app owned the data and the engine was a stateless pipeline — and it was reversed for a concrete reason: later phases (**clustering, replication, XMP writing**) *re-process the heavy owned data*. If the app owned it, every such call would either clone multi-MB previews (a memory disaster at thousands of frames) or thread `&mut` borrows that a background thread cannot hold. The engine is therefore a **stateful aggregate** — data plus the methods over it, in one place.

### 6.2 Two passes: `scan()` = skeleton, `ingest()` = previews

- **`scan()` is synchronous and cheap.** Walk the card + `enrich_assets` (the stage-1 header reads): every asset gets `captured_at` (→ the greyed date-folder tree), the 160×120 thumb (→ instant grid placeholder), and the cached strip ref. This is the "skeleton" and it finishes in ~14 s cold for 879 frames.
- **`ingest()` streams in the background.** It spawns a worker, returns a **bounded** `crossbeam_channel::Receiver<IngestEvent>`, and emits a **lean notification per asset** — `IngestEvent::Preview { key, error }`, *not* the image bytes. The UI, on each message, takes a brief read lock and reads that asset's freshly-stored preview from the engine store by key. The message is lean precisely *because* the data already lives in the engine.

### 6.3 Lock discipline — the subtle part that makes engine-owned data work

The naïve worker holds the write lock across the whole loop. This is broken two ways, and we hit both:

1. **No streaming.** While the worker holds the write lock, the UI's `read()` blocks until the *entire* run finishes. The user sees nothing until the end — the opposite of streaming.
2. **Deadlock.** The bounded channel (cap 64) fills; the worker blocks on `send` #65 *while still holding the write lock*; the UI is blocked on the read lock and never drains the channel; neither side can proceed. (With an 11-asset fixture this hides — 11 < 64 and the worker finishes first; with 879 it hangs instantly.)

The fix is **lock scope**: do the heavy work with **no lock held**.

```rust
work_items.into_par_iter().for_each(|(key, kind, path, strip)| {
    // 1. read + decode + encode — entirely lock-free
    let (thumb, preview, errors) = match read_source_bytes(kind, &path, strip) {
        Err(e) => (None, None, vec![e]),
        Ok(jpeg) => /* make_thumbnail + make_preview, collect errors */,
    };
    // 2. brief write lock — scope drops it before the send
    { let mut w = scan_response.write().unwrap();
      if let Some(a) = w.assets.get_mut(&key) { a.image_data.thumb = thumb; a.image_data.preview = preview; /*…*/ } }
    // 3. notify — lock already released; a dropped receiver just stops us
    let _ = tx.send(IngestEvent::Preview(IngestMessage { key, error: failed }));
});
```

The critical section is a `HashMap` `get_mut` + a move — microseconds — so the many CPU threads barely contend, and the UI slips a read lock between writes. A task blocked on a full channel **holds no lock**, so the UI always drains: the bounded channel reverts to its proper role as **backpressure / a memory governor**, not a deadlock. (Why `Arc<RwLock>` at all: a `'static` spawned thread cannot borrow `&self`, so shared ownership is mandatory; `RwLock` gives many-readers-or-one-writer, and keeping the write section tiny is what makes that cheap.)

### 6.4 The error-handling pattern (project-wide)

A bare `Option` that returns `None` on failure **throws away the cause**. Errors are data we need — for observability, and because some must reach the UI. So the rule, adopted across the engine:

- Fallible functions return **`Result<_, Box<dyn std::error::Error + Send + Sync>>`**. The `Send + Sync` is mandatory, not decoration: these errors travel across the ingest worker threads and are stored inside the engine-owned asset (`image_data.errors`), so the whole `PhotoAsset` must stay `Send`.
- **Record, don't drop.** A failed asset still streams its notification; its errors are collected on the asset and remain inspectable; the batch never aborts (one bad frame must not kill a 10k-card ingest). Add context when boxing (`format!("preview generation failed: {e}")`).
- **`Option` is reserved for genuine *absence*** (a frame legitimately has no embedded thumbnail), never for *failure*. The preview pipeline was migrated from `Option` to `Result` for exactly this reason — `?` auto-converts the underlying turbojpeg/fir errors into the box.

---

## Part 7: The End-to-End Read-Bound Result

This is the headline measurement of the phase — the one that retroactively justifies every "read less, don't parallelize the card" decision above.

### 7.1 The numbers (879-asset Canon card, i7-1068NG7, 8 logical)

| Measurement | Cold | Warm |
|---|---|---|
| scan + enrich (stage 1) | 14.4 s (61 fps) | 0.07 s (13,000 fps) |
| ingest (read + decode + 2 encodes) | **153.8 s (6 fps)** | **~26 s (33 fps)** |
| read-only stage, **2 threads** | 158 s (6 fps, **20 MB/s**) | 0.59 s |
| read-only stage, **8 threads** | 158 s (6 fps, **20 MB/s**) | 0.46 s |

### 7.2 Read-bound, proven three independent ways

1. **Cold ≫ warm:** ingest is 153.8 s cold vs ~26 s warm — a **6× gap** that is pure card-read latency (the code is identical).
2. **Cold-ingest ≈ cold-read-only:** 153.8 s (full pipeline) ≈ 158 s (reads with *no* decode/encode). The entire decode+encode stage is **hidden under I/O wait** — the CPU is mostly idle during a cold ingest, because the pipeline overlaps reading and decoding and the card is the gate.
3. **2 threads ≈ 8 threads, cold:** both 158 s, both 20 MB/s. **You cannot parallelize past one device.**

### 7.3 The card is a fixed ~20 MB/s pipe

This is the crux, and it reconciles with Part 2. There, *small* scattered header reads got *slower* with concurrency (−28%, seek contention destroying read-ahead). Here, *large* 2 MB reads are **throughput-bound**, so concurrency is *neutral* (2 ≈ 8). The two results say the same thing from opposite ends: **parallelizing card reads never helps** — at best neutral, at worst harmful. Hence cap `card_read_permits` at **1–2**.

Crucially, this means the **two-tier read/CPU split is not a throughput win** — the card is the wall regardless of how the threads are arranged. Its real value is *resource hygiene*: freeing the CPU threads that would otherwise sit parked in a blocking `read()`, and avoiding the small-read penalty. We measured the ceiling before building the split, so we build it for the right reason.

### 7.4 The CPU floor and the regime flip

The warm ingest (~26 s, **33 fps**) is the **CPU floor** — pure decode + two encodes for 879 frames on 8 logical cores, the card removed from the equation. It is stable across every warm run. The consequence: on an **SSD/NVMe** source (or any warm read) the pipeline *flips* to **CPU-bound** and you hit ~33 fps — back in the Part 4 parallel-scaling regime (4.26×). **The device decides which regime you are in**, exactly as Part 2 first showed for enrichment.

### 7.5 The only cold speed lever: fewer bytes

Since the card is byte-rate-limited, the *only* way to make cold triage faster is to **read less**. This is why embedded-over-JPEG for pairs matters: at 20 MB/s, ~3 MB vs ~6 MB is ~150 ms vs ~300 ms per pair — reading the smaller source **halves** paired-asset read time. And it is why we never slurp the full RAW. Note that *output* resolution does **not** change read bytes — you read the whole source regardless of whether you emit a 512 or a 1920 — so the lever is the *source*, not the target.

### 7.6 Streaming saves the UX, not the clock

Cold *completion* is 153.8 s, but **time-to-interaction is ~14 s**: the skeleton sweep paints the date tree and all 160×120 placeholders, then previews stream in at 6 fps. The photographer starts culling immediately while the 153.8 s runs behind them. The read-bound total is *acceptable* precisely because the architecture is streaming — completion time was never the UX metric.

### 7.7 Caches and pre-warming (the conceptual close)

The cold→warm gap is the **OS page cache** (on macOS the *unified buffer cache*): bytes held in RAM after the first read, served at GB/s versus the card's 20 MB/s. The tempting idea is to "warm" it at startup so later actions are instant. The hard truth, forced by 7.3: **warming does not beat the device — it only *moves* the unavoidable 20 MB/s read earlier, overlapping it with the user's think-time.** You cannot warm 3 GB of card in under ~150 s. So warming is *latency hiding* (prefetch), not a throughput multiplier. For ingot specifically, the best "warm-up" is simply **running `scan`+`ingest` eagerly** — the engine stores decoded previews, which is strictly better than priming the page cache (that would leave the decode undone). The one genuine throughput win is **across distinct physical devices** (each has its own 20 MB/s pipe), which is what Phase 6 builds.

---

### 7.8 The two-tier split — read-width is a device property, sized independently

Part 7.1–7.3 measured a *serial* SD reader on the Intel box (2 ≈ 8 threads, ~20 MB/s — concurrency neutral). A second machine told a different story. On an **M4 Pro (14 cores), same card**, sweeping the *merged* single-pool width (`RAYON_NUM_THREADS`):

| threads | ingest | note |
|---|---|---|
| 1 | 136 s (6 fps) | serial |
| 2 | 60 s (15 fps) | 2.3× |
| **4** | **53 s (17 fps)** | **best** |
| 8 | 55 s (16 fps) | flat |
| 14 | 60 s (14 fps) | regresses |

This card path has **read queue depth ~4** (1→4 = 2.6×, then >4 *hurts*) — unlike the Intel serial reader. So **read-concurrency optimum is a device property**, and we now have two points bracketing it: serial SD ≈ 1–2, queue-depth reader ≈ 4, SSD/NVMe higher.

The problem the merged pool exposes: with **one** width knob you cannot satisfy both tiers. Tuned for reads (4) it *starves* the 14-core CPU; tuned for the CPU (14) it *thrashes* the card (4-wide is already past this card's depth). The fix is the plan's two tiers, now built: a **reader tier** (`card_read_permits` threads, doing only `read_source_bytes`) feeding a **CPU tier** (`cpu_threads`, decode/encode) over a **bounded channel** (backpressure / memory governor — only ~`cpu_threads` raw buffers alive). Two independent widths.

```rust
std::thread::scope(|s| {
    s.spawn(move || read_pool.install(|| {           // reader tier: read_threads wide
        work.into_par_iter().for_each(|item| { let _ = raw_tx.send((key, read_source_bytes(...))); });
    }));                                              // raw_tx drops here → channel closes
    cpu_pool.install(|| {                             // CPU tier: cpu_threads wide
        raw_rx.into_iter().par_bridge().for_each(|(key, bytes)| { /* decode → store → notify */ });
    });
});
```

**Measured payoff (Intel serial reader, same card, separated build):** read=2 ingest **99.5 / 84.7 / 84.5 s** vs read=4 **133 / 162 / 167 s** (worsening). Two findings: (1) the **separated read=2 cold (~90 s) beats the old merged cold (154 s)** — a dedicated reader tier overlapping the CPU tier is a *real* throughput win, not just hygiene, even on a serial card; (2) **read=4 thrashes the serial reader** (the §2.3 small-read penalty), confirming the read width must match the device. Default `card_read_permits = 4` (M4 reader); a serial SD card wants 2 — it is tunable for exactly this reason.

**Caching anomaly (open).** On both the M4 and a later Intel session the cold→warm speedup *failed to materialise* (e.g. `scan+enrich` 6.5 s, not the warm 0.07 s; ingest never reaching the ~26 s CPU floor), unlike the clean 6× warm-up of §7.1. External removable volumes appear to cache/evict unpredictably across machines/sessions. Unexplained — flagged for a controlled investigation (mount options, free RAM, eviction policy), not yet a lesson.

---

## Part 8: Benchmark Results (consolidated)

All on i7-1068NG7 (4 physical / 8 logical), Rust release; cross-device note: M4 Pro (14 cores).

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

**Seek machinery (stage 1):** 128 KiB header read vs 36 MB slurp = **279× fewer bytes, 264× faster** (warm). Measured CR2 offsets: IFD1 thumb 54,424..71,768; IFD0 preview 71,768..2,111,192.

**End-to-end ingest, 879-asset card:**

| Phase | Cold | Warm |
|---|---|---|
| scan + enrich | 14.4 s (61 fps) | 0.07 s |
| ingest (read + decode + 2 encodes) | **153.8 s (6 fps)** | **~26 s (33 fps)** |
| read-only, 2 threads | 158 s (20 MB/s) | 0.59 s |
| read-only, 8 threads | 158 s (20 MB/s) | 0.46 s |

Read-bound (cold≫warm 6×; cold-ingest≈cold-read; 2≈8 threads). Card ceiling ≈ 20 MB/s, device-fixed. CPU floor ≈ 33 fps (warm); SSD source → CPU-bound. 879/879, 0 errors. `cargo test` 11 passing, clippy clean.

---

## Part 9: Common Errors Encountered

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
| Ingest **deadlocks** above 64 assets (fixture of 11 "worked") | Worker held the `write()` lock across the whole loop; filled the bounded(64) channel; blocked on `send` while holding the lock; UI blocked on the read lock → neither proceeds | Lock scope: snapshot inputs → heavy work **lock-free** → brief per-asset write lock → `send` after release (Part 6.3) |
| Every **paired** asset errors ("no embedded preview strip") | Metadata read JPEG-first (strip ref captured from the JPEG = `None`) but the preview source was RAW-first → demanded a strip that was never cached | Make both use the same priority (`get_prioritized`, RAW-first) so metadata + preview agree on the source |
| `Take` can't be handed to `read_from_container` | `read_from_container` needs `BufRead + Seek`; `Read::take` yields only `Read` | Read the prefix into a `Vec`, wrap in `Cursor` (which is `BufRead + Seek`) |
| `read_embedded_preview` could panic / over-allocate on a corrupt frame | `checked_add(len).unwrap()` panics on overflow; a magic 64 MB cap was arbitrary (and too small for 150 MP bodies) | Bound by the **file length** (free `fstat`): `offset + len ≤ file_len`; funnel overflow to `Err`, never `unwrap` |
| Preview failures vanished silently | Pipeline returned `Option` — `None` threw away *why* it failed | Return `Result<_, Box<dyn Error + Send + Sync>>`, record on `image_data.errors` (Part 6.4) |

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
- **Read only what you need.** kamadak slurps the whole CR2 (36 MB for a 20-byte date); a 128 KiB header
  read + a targeted ~2 MB seek-read of the embedded preview replace it (279× fewer bytes). Everything
  stage 1 needs is in the first ~72 KiB, measured.
- **The full ingest is read-bound on the card**, proven three ways: cold ≫ warm (6×), cold-ingest ≈
  cold-read-only (CPU hidden under I/O), and **2 ≈ 8 threads cold** on a *serial* SD reader (~20 MB/s).
  CPU floor ≈ 33 fps — an SSD source (or warm) flips the pipeline CPU-bound. The only cold lever is
  reading *fewer bytes* (embedded-over-JPEG for pairs).
- **Read-width optimum is a device property; the read and CPU tiers must be sized independently.** A
  second machine (M4 Pro) showed a card path with read queue-depth ~4 (1→4 = 2.6×, >4 regresses), vs the
  Intel serial reader's 2≈8. A merged pool can't set read=4 *and* cpu=14, so we split into a reader tier
  + CPU tier over a bounded channel. The split is a *real* cold win (Intel separated read=2 ~90 s beats
  merged 154 s), not just hygiene — provided the read width matches the device (read=4 thrashes a serial
  reader). Default `card_read_permits = 4`, tunable.
- **Engine owns the data; the worker never holds a lock during heavy work.** Engine-owned `Arc<RwLock>`
  state + a background worker is only correct with strict lock scope (snapshot → lock-free read/decode/
  encode → brief per-asset write → send); holding the write lock across the loop both kills streaming and
  deadlocks the bounded channel. Streaming makes time-to-interaction ~14 s, not the 154 s completion.
- **Errors are data:** fallible functions return `Result<_, Box<dyn Error + Send + Sync>>` and record the
  cause; never swallow it in a bare `Option`. Caches/pre-warming *move* the unavoidable read into idle
  time — they never beat the device.
