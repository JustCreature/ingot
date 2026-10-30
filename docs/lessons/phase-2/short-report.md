# Phase 2 — CPU-Bound Processing & Streaming: Reference Document

> Refresh card for someone who did the sessions. Hardware for all numbers: **i7-1068NG7, 4 physical /
> 8 logical**, Rust release. Corpus: 879-asset Canon card, 6240×4160 (~26 MP) frames. SD card = USB
> reader; SSD = external USB `GenericSSD`.

---

## 1. Capture-time type: `Option<NaiveDateTime>`

EXIF `DateTimeOriginal` is `"YYYY:MM:DD HH:MM:SS"` with **no timezone** (camera wall clock;
`OffsetTimeOriginal` often absent on Canon). So `DateTime<Utc>` is wrong — it forces an invented offset
and a UTC→local-date round-trip can push a midnight frame into the previous day's folder. Use
`NaiveDateTime` (honest about the missing zone), keep `Option` (RAW-only / stripped EXIF have no date),
project `.date()` at the routing boundary. `build_destination_path`'s signature was unchanged.

---

## 2. Reading EXIF (`kamadak-exif`, imports as `exif`)

Open → `BufReader` (mutable; it seeks) → `Reader::new().read_from_container` → `get_field(DateTimeOriginal,
In::PRIMARY)` → match `Value::Ascii(ref vec)` → `from_utf8` → `parse_from_str(s, "%Y:%m:%d %H:%M:%S")`.
Every step is `?`/`.ok()?` so **all failure funnels to `None`** — one bad frame can't abort a 10k batch.
`ref vec` borrows instead of moving out from behind `&Field` (≡ `match &field.value`). Dead-clock
`"0000:00:00..."` fails parsing → `None` for free.

```rust
let bytes = match field.value { Value::Ascii(ref v) => v.first()?, _ => return None };
NaiveDateTime::parse_from_str(std::str::from_utf8(bytes).ok()?, "%Y:%m:%d %H:%M:%S").ok()
```

---

## 3. Compute-then-apply

`enrich_captured_at`: `iter().map(|(k,a)| (k.clone(), read_capture_time(a))).collect()` → then write back
with `get_mut`. The per-asset unit is `Send` + side-effect-free, so serial→parallel is `iter()`→
`par_iter()`, and it matches the streaming endgame (apply = send to channel).

---

## 4. I/O-bound vs CPU-bound; parallelism vs concurrency

**CPU-bound** = cores busy (threads help). **I/O-bound** = waiting on a device (threads don't make the
device faster). **Parallelism** = cores doing work; **concurrency** = operations in flight. Enrichment
(few-KB EXIF header reads) is I/O-bound; preview decode/resize/encode is CPU-bound.

---

## 5. Cold vs warm cache (one-shot rule)

First read = cold (device); later = warm (page cache). Enrichment: cold **5.51 ms/frame (181 fps)**,
warm **~45 µs/frame (22k fps)** — **~120×**. A real card ingest is always cold (each file read once), so
warm numbers describe a path that never runs. **Cold is one-shot per cache state** — evict (`sudo purge`
/ remount) between cold runs.

---

## 6. Queue depth: parallelism flips sign by device

| Device | serial cold | parallel cold | effect |
|---|---|---|---|
| SD card (USB) | 4.85 s · 181 fps | 6.20 s · 142 fps | **−28% hurts** |
| SSD (`GenericSSD`) | 0.61 s · 1432 fps | 0.17 s · 5256 fps | **+3.7× helps** |

SD card = serial device (one path, no queue depth); concurrent random reads thrash read-ahead. SSD =
many NAND dies + command queue (NCQ/NVMe); concurrent reads overlap. **Optimal source-read concurrency
is a device property** → tunable source-read semaphore tier (1–2 for cards, higher for SSD). Enrichment
ships serial by default.

---

## 7. Why file I/O concurrency needs threads

Sockets can be "not ready" → one thread drives thousands via kqueue/epoll. **Regular files are always
"ready"**; `read()` blocks the thread until bytes arrive. So 1 in-flight read = 1 parked thread — but
parked threads burn ~0 CPU, so D threads (D = queue depth) can exceed core count. `tokio::fs` is a
blocking thread pool underneath on macOS; true single-thread-many-in-flight needs `io_uring` (Linux
only). D = the source-read semaphore permit count.

---

## 8. The embedded pyramid (extract > generate)

| Image | IFD0 preview | IFD1 thumb | mid-size? |
|---|---|---|---|
| CR2 | 6240×4160 JPEG, 1.5–2 MB | 160×120, ~13 KB | none |
| JPG | (file) 6240×4160 | 160×120, ~13 KB | none |

160×120 thumb = **free extract** (~11 MB/879 in RAM). No mid-size → 1920 grid preview must be generated.
CR2 IFD0 is full-res ⇒ **RAW-only and JPEG paths converge** into one decode→resize→encode pipeline,
zero RAW decode. Invariant: previews never come from decoding RAW sensor data.

---

## 9. Preview pipeline + crate stack

`turbojpeg` (libjpeg-turbo: scaled decode + encode) + `fast_image_resize` (SIMD resize). Per frame on
in-RAM bytes: **scaled decode → resize to 1920 (Lanczos3) → encode (Q85, 4:2:0)**. Thread
`turbojpeg::Image<Vec<u8>>` between stages (carries width/height/pitch/format); emit `Vec<u8>` only at
encode. RGB ⇒ `pitch = 3*width`.

---

## 10. Scaled (DCT-domain) decode — dominant lever

libjpeg-turbo decodes at `M/8` factors by using fewer DCT coefficients per 8×8 block — never
materialises full-res pixels. Rule: smallest `M/8` with long edge ≥ target → **3/8 (2340)** for
6240→1920.

| factor | decoded | time (8 thr, 999×) | vs 1/1 |
|---|---|---|---|
| 1/1 | 26 MP | ~48 s | 1.0× |
| 1/2 | 6.5 MP | ~22.7 s | 2.1× |
| **3/8** | 3.6 MP | ~17.7 s | **2.7×** |
| 1/4 (upscales) | 1.6 MP | ~15.1 s | 3.2× |

Decode dominates. **3/8 locked** (1/4 is +15% but upscales → soft, rejected).

---

## 11. Resize self-funding; filter is visual

Resize ≈ free in net terms because it shrinks the image so the **encode** gets cheaper (pays its own
cost). Filter is a small fraction of time ⇒ Lanczos3 vs Bilinear ≈ time-neutral ⇒ choose by eye. No
visible difference at tile size (and focus is checked in the 1:1 loupe, not the grid) → **Lanczos3 kept**.

---

## 12. Parallel scaling + modern-CPU caveats

| threads | time | speedup |
|---|---|---|
| 1 | ~100.5 s | 1.0× |
| 2 | ~54 s | 1.85× |
| 4 | ~30 s | 3.33× |
| 8 | ~23.5 s | **4.26×** |

Near-linear to **4 physical cores**; HT adds ~6% (codec work is execution-port-bound; HT shares ports).
**Inverse of enrichment.** Turbo Boost caveat: 1-thread runs ~4.1 GHz turbo, all-core ~3 GHz, so
"speedup vs 1 thread" understates true efficiency (the 3.33× at 4 cores is partly clock dropoff).

---

## 13. Seek machinery — read only what you need

kamadak slurps the **whole** CR2 for a TIFF (36 MB resident for a 20-byte date); JPEG stops after APP1 (cheap). Measured CR2: IFD1 thumb 54,424..71,768, IFD0 preview 71,768..2,111,192 — everything stage 1 needs is in the first **~72 KiB**. **Stage 1:** read a 128 KiB prefix into a `Vec`, parse via `Cursor` (`read_from_container` needs `BufRead+Seek`; `Take` is `Read`-only) → **279× fewer bytes, 264× faster**; cache the IFD0 strip ref. **Stage 2:** `read_embedded_preview` = `seek(offset)` + `read_exact(len)` of exactly the ~2 MB strip, allocation bounded by `offset+len ≤ file_len` (free `fstat`). Embedded preview is smaller than the standalone JPEG at equal res (harder-compressed throwaway: low Q, 4:2:0) → pairs use it (RAW-first), trading quality for fewer bytes.

---

## 14. Streaming ingest + engine-owned state

**Engine owns the data** (`Arc<RwLock<ScanResponse>>`), UI borrows to present — *not* app-owns/engine-stateless, because clustering/replication/XMP re-process the heavy owned data (cloning previews = memory disaster). `scan()` = synchronous skeleton (date + 160 thumb + strip ref); `ingest()` spawns a worker, returns a bounded `crossbeam` `Receiver<IngestEvent>`, streams **lean notifications** (`{key, error}`), not bytes — UI reads the store by key. **Lock discipline (critical):** snapshot inputs under a brief read lock → **read+decode+encode lock-free** → brief per-asset write lock → `send` *after* release. Holding the write lock across the loop both kills streaming and **deadlocks** the bounded channel (worker blocks on `send` while holding the lock; UI blocked on read lock).

---

## 15. End-to-end: read-bound on the card

879-asset card (Intel serial SD reader): scan+enrich cold 14.4 s / warm 0.07 s; ingest cold **154 s (6 fps)** / warm **~26 s (33 fps)**; read-only cold **2 = 8 threads = 158 s ≈ 20 MB/s**. **Read-bound, three ways:** cold≫warm (6×), cold-ingest≈cold-read-only (CPU hidden under I/O), 2≈8 threads. The serial card is a fixed ~20 MB/s pipe. CPU floor ~33 fps; SSD source (or warm) flips it CPU-bound (the §12 regime). Only cold lever = fewer bytes (embedded-for-pairs). **Caches** = OS page cache; warming only *moves* the read into idle time (latency hiding), never beats the device → Phase 6. (Anomaly: cold→warm sometimes fails to materialise on external removable volumes — seen on M4 *and* Intel — unexplained, flagged.)

---

## 17. Two-tier split: read-width is a device property

Second machine (M4 Pro, 14 cores, same card, merged-pool sweep): ingest 1→136 s, **2→60, 4→53 (best), 8→55, 14→60** — this card path has **read queue depth ~4**, unlike the Intel serial reader (2≈8). So read-concurrency optimum is **per-device**; a *merged* pool can't set read=4 AND cpu=14 at once. **Fix (implemented):** reader tier (`card_read_permits`) + CPU tier (`cpu_threads`) joined by a bounded channel (`thread::scope`: read_pool `into_par_iter`→`raw_tx`; cpu_pool `raw_rx.par_bridge()`→decode/store/notify). Separated **read=2 on Intel ~90 s cold beats merged 154 s** (overlap is a real win, not just hygiene); **read=4 thrashes the serial reader** (133→167 s). Default `card_read_permits = 4` (M4 reader); serial SD wants 2 — tunable (`INGOT_READ_THREADS`/config). RAYON_NUM_THREADS no longer controls ingest (explicit pools).

---

## 16. Error pattern (project-wide)

Fallible fns return `Result<_, Box<dyn Error + Send + Sync>>`, never a bare `Option` that swallows the cause. `Send + Sync` is mandatory (errors cross worker threads, stored on `image_data.errors`). Record, don't drop — a failed asset still streams; errors stay inspectable; batch never aborts. `Option` is for genuine *absence* (no embedded thumb), not failure. Preview pipeline migrated `Option`→`Result` (`?` auto-converts turbojpeg/fir errors).

---

## N. Full benchmark table

**Enrichment (cold, 879 frames):** SD 4.85 s serial / 6.20 s parallel (181/142 fps); SSD 0.61 s / 0.17 s
(1432/5256 fps). Cold/warm: SD ≈120×, SSD ≈13×. 879/879 parsed, 0 errors.
**Preview scaling (999×, 1/2):** 100.5 / 54 / 30 / 23.5 s @ 1/2/4/8 threads = 4.26×.
**Scaled decode (8 thr):** 1/1 48 s · 1/2 22.7 s · 3/8 17.7 s · 1/4 15.1 s.
**Seek machinery:** 128 KiB header vs 36 MB slurp = 279× fewer bytes, 264× faster.
**End-to-end ingest (879 card):** scan+enrich cold 14.4 s; ingest cold **154 s (6 fps)** / warm **~26 s (33 fps)**; read-only 2≈8 threads ≈ 158 s ≈ 20 MB/s. Read-bound; CPU floor 33 fps. Tests 11 passing, clippy clean.
**Locked:** decode 3/8 · Lanczos3 · RGB/U8x3 · per-frame `Decompressor` · header prefix 128 KiB · `card_read_permits` 1–2.

---

## N+1. Common errors

| Error | Cause | Fix |
|---|---|---|
| Parallel enrich slower on SD | SD is serial; concurrent reads thrash read-ahead | Serial default; device-tuned concurrency |
| "Enrich trivially fast" | Read warm cache, not cold | Measure cold; evict between runs |
| `cannot borrow src as mutable` in `Fn` closure | `&mut [u8]` input; can't share `&mut` across threads | Take `&[u8]` (the `&mut` was spurious) |
| `Image::new`/`Resizer::new` missing | `fast_image_resize` not added; `turbojpeg::Image` shadowed it | `cargo add` it; alias `FirImage` |
| `as_deref` not found on `Vec<u8>` | Stage returned bare bytes, dropped image metadata | Thread `turbojpeg::Image<Vec<u8>>` through stages |
| Skewed/striped image | `pitch=4*width` with RGB format | RGB ⇒ `pitch=3*width`, buffer `3*w*h` |
| `turbojpeg` link failure | `turbojpeg-sys` needs cmake + nasm | `brew install jpeg-turbo cmake nasm` (do as gate 0) |
| `"0000:00:00…"` | Dead clock battery | Parser rejects month 00 → `None` (auto-absorbed) |
| Ingest deadlocks >64 assets (11 "worked") | Worker held `write()` lock across the loop; filled bounded channel; blocked on `send` holding the lock; UI blocked on read lock | Lock scope: snapshot → lock-free work → brief per-asset write → send after release |
| Every paired asset errors | Metadata JPEG-first (no strip ref) but preview RAW-first → demands uncached strip | Same priority both (`get_prioritized` RAW-first) |
| `Take` rejected by `read_from_container` | Needs `BufRead+Seek`; `Take` is `Read`-only | Read prefix to `Vec`, wrap in `Cursor` |
| `read_embedded_preview` panic/over-alloc | `checked_add().unwrap()`; arbitrary 64 MB cap | Bound by `file_len` (free `fstat`); funnel overflow to `Err` |
| Preview failures vanished | Returned `Option` (lost the cause) | `Result<_, Box<dyn Error+Send+Sync>>`, record on `image_data.errors` |
