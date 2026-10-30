use std::{
    path::Path,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use ingot_core::{
    CardGate, CopyEngine, Engine, EngineConfig, LocalTarget, Target, TargetId, TargetKind,
};

// Replication throughput: reads every scanned file once and copies it to a local
// primary target and, optionally, a secondary one. Reports wall time and MB/s, and
// when the card was finished with.
//
//   cargo run --release -p ingot_core --example bench_replicate -- <source> <target_root> [secondary_root]
//
// Sweep with INGOT_READ_THREADS, INGOT_READ_DEPTH and INGOT_WRITE_PERMITS. For
// peak RAM, run the built binary under `/usr/bin/time -l` and read "maximum
// resident set size". Each run writes to a new directory under <target_root> and
// removes it afterwards. For a cold number, reconnect the card first.
fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(source), Some(target_root)) = (args.next(), args.next()) else {
        eprintln!("usage: bench_replicate <source> <target_root>");
        std::process::exit(2);
    };

    let secondary_root = args.next();

    let read_permits = env_usize("INGOT_READ_THREADS", 2);
    let depth = env_usize("INGOT_READ_DEPTH", 4);
    let write_permits = env_usize("INGOT_WRITE_PERMITS", 4);

    let mut engine = Engine::new(EngineConfig::default());
    engine.scan(Path::new(&source));

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_nanos();
    let run_dir = Path::new(&target_root).join(format!("ingot-bench-{nanos}"));

    let target = LocalTarget::new(
        TargetId(1),
        Target {
            root: run_dir.clone(),
            kind: TargetKind::LocalNvme,
            write_permits,
        },
    );
    let mut copy = CopyEngine::new(CardGate::new(read_permits), depth, Arc::new(target));
    let secondary_dir =
        secondary_root.map(|root| Path::new(&root).join(format!("ingot-bench-{nanos}")));
    if let Some(root) = &secondary_dir {
        copy = copy.with_secondary(Arc::new(LocalTarget::new(
            TargetId(2),
            Target {
                root: root.clone(),
                kind: TargetKind::LocalNvme,
                write_permits,
            },
        )));
    }

    let t = Instant::now();
    let report = copy
        .replicate(&engine.scan_response)
        .await
        .expect("replication could not start");
    let elapsed = t.elapsed();

    let bytes: u64 = report
        .verified
        .iter()
        .filter(|r| r.target() == TargetId(1))
        .map(|r| r.size())
        .sum();

    println!("read={read_permits} depth={depth} write={write_permits}");
    println!(
        "verified={} failed={} {:.1} MB in {elapsed:?} ({:.1} MB/s)",
        report.verified.len(),
        report.failures.len(),
        bytes as f64 / 1e6,
        bytes as f64 / 1e6 / elapsed.as_secs_f64()
    );
    println!(
        "primary done after {:?}, deferred copies={}",
        report.primary_done_after, report.deferred_copies
    );
    for failure in report.failures.iter().take(5) {
        eprintln!("failure: {:?}: {}", failure.source, failure.error);
    }

    for dir in std::iter::once(&run_dir).chain(secondary_dir.iter()) {
        if dir.exists()
            && let Err(err) = std::fs::remove_dir_all(dir)
        {
            eprintln!("could not remove {}: {err}", dir.display());
        }
    }
}
