use std::{path::Path, time::Instant};

use ingot_core::{Engine, EngineConfig, IngestEvent};

// End-to-end Phase-2 throughput over a real SD card: scan + enrich (stage 1,
// header-only) then ingest (stage 2/3 — source read + decode + encode), measured
// as frames/s. This is the number that says whether the pipeline is read- or
// CPU-bound.
//
//   cargo run --release -p ingot_core --example bench_ingest
//   cargo run --release -p ingot_core --example bench_ingest -- /some/DCIM
//
// Run 1 of ingest is cold (bytes come off the media); runs 2-3 are warm (page
// cache). For a clean cold number, reconnect the card and read run 1 only.
fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/Volumes/EOS_DIGITAL/DCIM".to_string());
    let source = Path::new(&path);

    let mut engine = Engine::new(EngineConfig::default());

    let t_scan = Instant::now();
    engine.scan(source); // walk + enrich
    let scan_elapsed = t_scan.elapsed();

    let n = engine.scan_response.read().unwrap().assets.len();
    println!("source       : {}", source.display());
    println!("config       : {:?}", engine.config());
    println!("assets       : {n}");
    if n == 0 {
        eprintln!("no assets found — is the card mounted at that path?");
        return;
    }
    println!(
        "scan+enrich  : {scan_elapsed:?}  ({:.0} fps)",
        n as f64 / scan_elapsed.as_secs_f64()
    );

    for run in 1..=3 {
        let t = Instant::now();
        let rx = engine.ingest();
        let (mut ok, mut err) = (0u32, 0u32);
        for IngestEvent::Preview(msg) in rx.iter() {
            if msg.error {
                err += 1;
            } else {
                ok += 1;
            }
        }
        let elapsed = t.elapsed();
        println!(
            "ingest run {run} : {elapsed:?}  ({:.0} fps)  ok={ok} err={err}",
            n as f64 / elapsed.as_secs_f64()
        );
    }
}
