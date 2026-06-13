use std::{path::Path, time::Instant};

use ingot_core::{enrich_assets, scan_source_dir};

// Measurement B: is `enrich_captured_at` CPU-bound or I/O-bound on a real card?
//
// Run (always --release; debug timings are meaningless for this):
//   cargo run --release --example bench_enrich
//   cargo run --release --example bench_enrich -- /some/other/path
//
// Hypothesis: enrichment is I/O / metadata / seek-bound on a single physical
// card (EXIF is a tiny header read, not a full-file read), so it will NOT scale
// with cores. We measure serial here; compare against a future Rayon version.
fn main() {
    // let path = std::env::args()
    //     .nth(1)
    //     .unwrap_or_else(|| "/Volumes/EOS_DIGITAL/DCIM".to_string());

    // let path = std::env::args()
    //     .nth(1)
    //     .unwrap_or_else(|| "/Volumes/GenericSSD/photodata/original".to_string());
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "crates/ingot_core/testdata/test_exif_read".to_string());

    let source = Path::new(&path);

    let t_scan = Instant::now();
    let mut response = scan_source_dir(source);
    let scan_elapsed = t_scan.elapsed();

    let n = response.assets.len();
    println!("source            : {}", source.display());
    println!("scan              : {scan_elapsed:?}");
    println!(
        "assets / collisions / errors : {n} / {} / {}",
        response.collisions.len(),
        response.errors.len()
    );

    if n == 0 {
        eprintln!("no assets found — is the card mounted at that path?");
        return;
    }

    // Run enrichment several times. Run 1 is (mostly) cold cache; later runs are
    // warm. read_capture_time re-reads the files every call, so re-running over
    // the same map is a valid repeat measurement.
    for run in 1..=3 {
        let t = Instant::now();
        enrich_assets(&mut response.assets);
        let elapsed = t.elapsed();
        let per_frame = elapsed / n as u32;
        let fps = n as f64 / elapsed.as_secs_f64();
        println!("enrich run {run}      : {elapsed:?}  ({per_frame:?}/frame, {fps:.0} frames/s)");
    }

    let with_date = response
        .assets
        .values()
        .filter(|a| a.exif_data.captured_at.is_some())
        .count();
    println!(
        "captured_at filled : {with_date}/{n}  (none: {})",
        n - with_date
    );
}
