use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use ingot_core::{
    AssetKey, Engine, EngineConfig, ImportOptions, LoadEvent, LocalTarget, ScanOptions, Target,
    TargetId, TargetKind, plan_import,
};

// What the photographer feels on a slow card while everything is copying: when pictures
// appear, in order, and how long a zoom waits. The card is simulated as one serial
// channel, because warm fixtures say nothing about a card.
//
//   cargo run --release -p ingot_core --example bench_culling
//   INGOT_CARD_MB_S=20 INGOT_PHOTOS=300 INGOT_SCREEN=24 INGOT_CHUNK_KB=2048
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/testdata/test_exif_read/IMG_1939.CR2"
);

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    sorted[((sorted.len() - 1) as f64 * fraction) as usize]
}

fn main() {
    let photos = env_u64("INGOT_PHOTOS", 300) as usize;
    let screen = env_u64("INGOT_SCREEN", 24) as usize;
    let speed = env_u64("INGOT_CARD_MB_S", 20) * 1_000_000;
    let chunk = env_u64("INGOT_CHUNK_KB", 2048) as usize * 1024;

    // Hard links: a whole card of real RAW files that takes no disk space.
    let tmp = tempfile::tempdir().expect("temp dir");
    let card = tmp.path().join("card");
    std::fs::create_dir_all(&card).expect("card");
    for i in 0..photos {
        let link = card.join(format!("IMG_{i:04}.CR2"));
        std::fs::hard_link(FIXTURE, &link)
            .or_else(|_| std::fs::copy(FIXTURE, &link).map(|_| ()))
            .expect("link");
    }

    let mut engine = Engine::new(EngineConfig {
        simulated_card_speed: Some(speed),
        copy_read_chunk: chunk,
        ..Default::default()
    });
    engine.scan_names(&card, &ScanOptions::default());
    let options = ImportOptions {
        use_file_dates: true,
        ..Default::default()
    };
    let plan = plan_import(&engine.scan_response, &card, &options, None).expect("plan");
    let target = Arc::new(LocalTarget::new(
        TargetId(1),
        Target {
            root: tmp.path().join("photos"),
            kind: TargetKind::LocalNvme,
            write_permits: 4,
        },
    ));
    let mut order: Vec<AssetKey> = engine
        .scan_response
        .read()
        .unwrap()
        .assets
        .keys()
        .cloned()
        .collect();
    order.sort_by(|a, b| a.stem.cmp(&b.stem));

    let started = Instant::now();
    let session = engine.start_session(target, vec![], None, vec![]);
    for file in plan.files {
        session.queue.push(file);
    }

    let events = engine.start_loading(order.clone(), Arc::new(AtomicUsize::new(0)));
    let (mut headers, mut previews) = (0usize, 0usize);
    let (mut first_thumb, mut screen_thumbs, mut all_thumbs) = (None, None, None);
    let (mut first_preview, mut screen_previews, mut all_previews) = (None, None, None);
    let in_order = true;
    for event in events.iter() {
        match event {
            LoadEvent::Header { key, .. } => {
                assert_eq!(key, order[headers], "thumbnails out of order");
                headers += 1;
                if headers == 1 {
                    first_thumb = Some(started.elapsed());
                }
                if headers == screen {
                    screen_thumbs = Some(started.elapsed());
                }
            }
            LoadEvent::HeadersDone => all_thumbs = Some(started.elapsed()),
            LoadEvent::Preview(message) => {
                assert_eq!(message.key, order[previews], "previews out of order");
                previews += 1;
                if previews == 1 {
                    first_preview = Some(started.elapsed());
                }
                if previews == screen {
                    screen_previews = Some(started.elapsed());
                }
            }
            LoadEvent::PreviewsDone => all_previews = Some(started.elapsed()),
        }
    }
    println!(
        "{photos} photos, card {} MB/s, copy chunk {} KiB, first screen {screen}",
        speed / 1_000_000,
        chunk / 1024
    );
    println!("all in display order:    {in_order}");
    println!("first thumbnail:         {first_thumb:?}");
    println!("first screen thumbnails: {screen_thumbs:?}");
    println!("all thumbnails:          {all_thumbs:?}");
    println!("first preview:           {first_preview:?}");
    println!("first screen previews:   {screen_previews:?}");
    println!("all previews:            {all_previews:?}");

    // With the stages done, the copy has the card. How long does a zoom wait now?
    let engine = Arc::new(engine);
    let done = Arc::new(AtomicBool::new(false));
    let copied = {
        let progress = session.progress.clone();
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let (mut files, first) = (0usize, Instant::now());
            while !done.load(Ordering::Relaxed) {
                if progress.recv_timeout(Duration::from_millis(200)).is_ok() {
                    files += 1;
                }
            }
            (files, first.elapsed())
        })
    };
    let mut waits = vec![];
    for sample in 0..40 {
        std::thread::sleep(Duration::from_millis(250));
        let key = &order[sample % order.len()];
        let asked = Instant::now();
        engine.read_full_jpeg(key).expect("read");
        // The strip itself takes 100 ms on this card; the rest is waiting for the copy.
        waits.push(asked.elapsed());
    }
    done.store(true, Ordering::Relaxed);
    waits.sort();
    let (files, seconds) = copied.join().expect("copy thread");
    println!(
        "zoom read during copy:   p50 {:?}, p95 {:?}, max {:?} (includes the read itself)",
        percentile(&waits, 0.5),
        percentile(&waits, 0.95),
        percentile(&waits, 1.0)
    );
    println!(
        "copy while being zoomed: {files} files in {:.0?} ({:.1} MB/s)",
        seconds,
        files as f64 * 35.0 / seconds.as_secs_f64()
    );
    session.queue.stop();
    std::process::exit(0);
}
