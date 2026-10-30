use std::{sync::RwLock, time::Instant};

use chrono::{Duration, NaiveDate};
use ingot_core::{
    ClusterItem, DEFAULT_SENSITIVITY, Fingerprint, ImportOptions, cluster_ranges, plan_import,
    scan_source_dir,
};

// What the import screen and the sensitivity slider depend on being instant: one
// fingerprint, regrouping a large shoot, and planning from a folder listing.
//
//   cargo run --release -p ingot_core --example bench_cluster
const PHOTOS: usize = 10_000;

fn main() {
    let (w, h) = (512, 341);
    let mut rgb = vec![0u8; w * h * 3];
    for (i, value) in rgb.iter_mut().enumerate() {
        *value = (i * 7 % 251) as u8;
    }
    let t = Instant::now();
    for _ in 0..100 {
        Fingerprint::from_rgb(&rgb, w, h).expect("bad image");
    }
    println!("fingerprint, {w}x{h}: {:.2?}", t.elapsed() / 100);

    let start = NaiveDate::from_ymd_opt(2026, 5, 22)
        .expect("date")
        .and_hms_opt(9, 0, 0)
        .expect("time");
    let print = Fingerprint::from_rgb(&rgb, w, h).expect("bad image");
    let items: Vec<ClusterItem> = (0..PHOTOS)
        .map(|i| ClusterItem {
            taken: Some(start + Duration::seconds(i as i64)),
            fingerprint: Some(print),
        })
        .collect();
    let t = Instant::now();
    let groups = cluster_ranges(&items, DEFAULT_SENSITIVITY).len();
    println!(
        "regroup {PHOTOS} photos: {:.2?} ({groups} groups)",
        t.elapsed()
    );

    let dir = tempfile::tempdir().expect("temp dir");
    for i in 0..PHOTOS {
        std::fs::write(dir.path().join(format!("IMG_{i}.CR2")), b"x").expect("write");
    }
    let t = Instant::now();
    let scan = RwLock::new(scan_source_dir(dir.path()));
    println!("list {PHOTOS} files: {:.2?}", t.elapsed());
    let options = ImportOptions {
        use_file_dates: true,
        ..Default::default()
    };
    let t = Instant::now();
    let plan = plan_import(&scan, dir.path(), &options, None).expect("plan");
    println!("plan {} files: {:.2?}", plan.files.len(), t.elapsed());
}
