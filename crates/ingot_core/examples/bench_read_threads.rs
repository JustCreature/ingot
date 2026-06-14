use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ingot_core::{Engine, EngineConfig, FileKind, PreviewStrip, read_embedded_preview};
use rayon::prelude::*;

// Does reading the source card with more threads help or hurt? This isolates the
// READ stage (no decode/encode): for each asset it reads the JPEG / seek-reads
// the embedded preview, varying the rayon pool width. Phase-2 step-1 found
// parallel reads *hurt* on an SD card (-28%) but helped on SSD (+3.7x) — a
// device property. This measures it on the real ingest read path.
//
//   cargo run --release -p ingot_core --example bench_read_threads
//   cargo run --release -p ingot_core --example bench_read_threads -- /some/DCIM 8
//
// COLD-CACHE CAVEAT: the first read of a file hits the media; later reads hit the
// page cache. Within one invocation the second thread-count is already warm, so
// only the very first thread-count's run 1 is truly cold. For a clean COLD
// 2-vs-8: pass an explicit thread arg, reconnect/remount the card, run again with
// the other count, and compare each one's run 1.
type Work = Vec<(FileKind, PathBuf, Option<PreviewStrip>)>;

fn read_one(kind: FileKind, path: &Path, strip: Option<PreviewStrip>) -> usize {
    let bytes = match kind {
        FileKind::Raw => strip.and_then(|s| read_embedded_preview(path, s).ok()),
        FileKind::Jpeg => std::fs::read(path).ok(),
    };
    bytes.map_or(0, |b| b.len())
}

fn run(work: &Work, threads: usize) -> (Duration, usize) {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .expect("failed to build rayon pool");

    let t = Instant::now();
    let bytes = pool.install(|| {
        work.par_iter()
            .map(|(kind, path, strip)| read_one(*kind, path, *strip))
            .sum::<usize>()
    });
    (t.elapsed(), bytes)
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/Volumes/EOS_DIGITAL/DCIM".to_string());
    let source = Path::new(&path);

    let single: Option<usize> = std::env::args().nth(2).and_then(|s| s.parse().ok());

    let mut engine = Engine::new(EngineConfig::default());
    engine.scan(source); // walk + enrich -> populates embedded-preview strip refs

    let work: Work = {
        let r = engine.scan_response.read().unwrap();
        r.assets
            .values()
            .map(|a| {
                let (kind, p) = a.files.get_prioritized();
                (
                    kind,
                    p.to_path_buf(),
                    a.exif_data.embedded_preview_file_location,
                )
            })
            .collect()
    };

    let n = work.len();
    println!("source : {}", source.display());
    println!("assets : {n}");
    if n == 0 {
        eprintln!("no assets found — is the card mounted at that path?");
        return;
    }

    let thread_counts = match single {
        Some(t) => vec![t],
        None => vec![2usize, 8],
    };

    for threads in thread_counts {
        for r in 1..=3 {
            let (elapsed, bytes) = run(&work, threads);
            let fps = n as f64 / elapsed.as_secs_f64();
            let mbps = bytes as f64 / 1e6 / elapsed.as_secs_f64();
            let tag = if r == 1 { "cold?" } else { "warm " };
            println!(
                "{threads} threads run {r} ({tag}): {elapsed:?}  ({fps:.0} fps, {mbps:.0} MB/s)"
            );
        }
    }
}
