//! The `ingot` command line app: imports a card to local folders with the same
//! options the import dialog will offer, and keeps previews flowing meanwhile.

mod cli;

use std::{
    path::Path,
    process::{Command, ExitCode},
    sync::Arc,
    time::Instant,
};

use cli::{ImportArgs, Usage};
use ingot_core::{
    Engine, EngineConfig, ImportHistory, LocalTarget, ReplicationEvent, ReplicationTarget,
    ScanOptions, Target, TargetId, TargetKind, plan_import,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match cli::parse(&args) {
        Ok(parsed) => parsed,
        Err(Usage(message)) => {
            eprintln!("{message}\n\n{}", cli::HELP);
            return ExitCode::from(2);
        }
    };
    match parsed {
        None => {
            println!("{}", cli::HELP);
            ExitCode::SUCCESS
        }
        Some(import) => match run_import(import) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        },
    }
}

fn local_target(id: u32, root: &Path) -> Arc<dyn ReplicationTarget> {
    Arc::new(LocalTarget::new(
        TargetId(id),
        Target {
            root: root.to_path_buf(),
            kind: TargetKind::LocalNvme,
            write_permits: 4,
        },
    ))
}

fn run_import(args: ImportArgs) -> Result<ExitCode, Box<dyn std::error::Error + Send + Sync>> {
    let mut engine = Engine::new(EngineConfig::default());
    let scan_started = Instant::now();
    engine.scan_with(
        &args.source,
        &ScanOptions {
            include_subfolders: args.include_subfolders,
        },
    );
    {
        let scan = engine
            .scan_response
            .read()
            .map_err(|err| format!("scan store lock poisoned: {err}"))?;
        println!(
            "scanned {} photos in {:.1?}",
            scan.assets.len(),
            scan_started.elapsed()
        );
        for err in &scan.errors {
            eprintln!("could not scan: {err}");
        }
        if !scan.ignored.is_empty() {
            println!(
                "not imported, unsupported file types: {} (e.g. {})",
                scan.ignored.len(),
                scan.ignored[0].display()
            );
        }
    }

    let history = match cli::history_path() {
        Some(path) => Some(ImportHistory::open(path)?),
        None => None,
    };
    let plan = plan_import(
        &engine.scan_response,
        &args.source,
        &args.options,
        history.as_ref(),
    )?;
    if !plan.skipped_duplicates.is_empty() {
        println!(
            "skipping {} files already imported (use --all to take them again)",
            plan.skipped_duplicates.len()
        );
    }

    if args.dry_run {
        for file in &plan.files {
            println!("{} -> {}", file.source.display(), file.relative.display());
        }
        for failure in &plan.failures {
            eprintln!("would fail: {:?}: {}", failure.source, failure.error);
        }
        return Ok(ExitCode::SUCCESS);
    }

    let previews = args.previews.then(|| engine.ingest());

    let Some(destination) = &args.destination else {
        // Add mode: photos stay where they are.
        if let Some(previews) = previews {
            let count = previews.iter().count();
            println!("built previews for {count} photos");
        }
        return Ok(ExitCode::SUCCESS);
    };

    let total_files = plan.files.len();
    let total_bytes: u64 = plan.files.iter().map(|file| file.size).sum();
    let targets = args.second_copies.len() + 1;
    println!(
        "copying {total_files} files, {:.1} GB, to {targets} target(s)",
        total_bytes as f64 / 1e9
    );

    let primary = local_target(1, destination);
    let secondaries = args
        .second_copies
        .iter()
        .enumerate()
        .map(|(index, root)| local_target(index as u32 + 2, root))
        .collect();

    let started = Instant::now();
    let import = engine.start_import(plan, primary, secondaries, history);
    let previews_thread =
        previews.map(|previews| std::thread::spawn(move || previews.iter().count()));

    let mut done = 0;
    for event in import.progress.iter() {
        match event {
            ReplicationEvent::Verified { .. } => {
                done += 1;
                if done % 25 == 0 || done == total_files * targets {
                    println!(
                        "{done}/{} copies verified, {:.0?}",
                        total_files * targets,
                        started.elapsed()
                    );
                }
            }
            ReplicationEvent::Failed {
                source,
                target,
                error,
            } => eprintln!("failed: {source:?} on {target:?}: {error}"),
        }
    }
    let report = import.wait();
    let elapsed = started.elapsed();
    if let Some(thread) = previews_thread {
        let count = thread.join().unwrap_or(0);
        println!("built previews for {count} photos");
    }

    let new_copies = report.verified.iter().filter(|r| r.newly_written()).count();
    let written: u64 = report
        .verified
        .iter()
        .filter(|r| r.newly_written())
        .map(|r| r.size())
        .sum();
    println!(
        "verified {} copies ({new_copies} new, {} already there), {} failed, {:.1} GB in {elapsed:.1?} ({:.0} MB/s); card finished after {:.1?}",
        report.verified.len(),
        report.verified.len() - new_copies,
        report.failures.len(),
        written as f64 / 1e9,
        written as f64 / 1e6 / elapsed.as_secs_f64().max(0.001),
        report.primary_done_after,
    );

    if !report.failures.is_empty() {
        return Ok(ExitCode::FAILURE);
    }
    if args.eject {
        eject(&args.source);
    }
    Ok(ExitCode::SUCCESS)
}

/// Asks macOS to eject the volume holding `source`. Only runs after a clean import,
/// and only because the user asked. A failure here costs nothing, so it is reported
/// and left at that.
fn eject(source: &Path) {
    let status = Command::new("diskutil").arg("eject").arg(source).status();
    match status {
        Ok(status) if status.success() => println!("card ejected"),
        Ok(status) => eprintln!("could not eject the card: diskutil exited with {status}"),
        Err(err) => eprintln!("could not eject the card: {err}"),
    }
}
