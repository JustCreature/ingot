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

## N. Full benchmark table

**Enrichment (cold, 879 frames):** SD 4.85 s serial / 6.20 s parallel (181/142 fps); SSD 0.61 s / 0.17 s
(1432/5256 fps). Cold/warm: SD ≈120×, SSD ≈13×. 879/879 parsed, 0 errors. Tests 5 passing, clippy clean.
**Preview scaling (999×, 1/2):** 100.5 / 54 / 30 / 23.5 s @ 1/2/4/8 threads = 4.26×.
**Scaled decode (8 thr):** 1/1 48 s · 1/2 22.7 s · 3/8 17.7 s · 1/4 15.1 s.
**Locked:** decode 3/8 · Lanczos3 · RGB/U8x3 · per-frame `Decompressor`.

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
