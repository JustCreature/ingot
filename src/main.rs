//! The `ingot` command line app: imports a card to local folders with the same
//! options the import dialog will offer, and keeps previews flowing meanwhile.

mod cli;
mod ui;

use std::{
    path::Path,
    process::{Command, ExitCode},
    sync::Arc,
    time::Instant,
};

use cli::{Command as CliCommand, DevelopArgs, ImportArgs, Usage};
use ingot_core::{
    DriveConfig, DriveCredentials, DriveTarget, Engine, EngineConfig, ImportHistory, LocalTarget,
    ReplicationEvent, ReplicationTarget, ScanOptions, Target, TargetId, TargetKind, plan_import,
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
    let outcome = match parsed {
        CliCommand::Help => {
            println!("{}", cli::HELP);
            return ExitCode::SUCCESS;
        }
        CliCommand::Ui(folder) => run_ui(folder),
        CliCommand::Import(import) => run_import(*import),
        CliCommand::GdriveLogin => run_gdrive_login(),
        CliCommand::Develop(develop) => run_develop(*develop),
    };
    outcome.unwrap_or_else(|err| {
        eprintln!("error: {err}");
        ExitCode::FAILURE
    })
}

fn run_ui(folder: Option<std::path::PathBuf>) -> AppResult {
    ui::run(
        ui::AppPaths {
            triage: cli::triage_path().ok_or("no app data folder on this system")?,
            history: cli::history_path().ok_or("no app data folder on this system")?,
            imports: cli::imports_path().ok_or("no app data folder on this system")?,
            gdrive: cli::gdrive_credentials_path().ok_or("no app data folder on this system")?,
        },
        folder,
    )?;
    Ok(ExitCode::SUCCESS)
}

type AppResult = Result<ExitCode, Box<dyn std::error::Error + Send + Sync>>;

fn run_gdrive_login() -> AppResult {
    let client_id = std::env::var("INGOT_GDRIVE_CLIENT_ID").map_err(|_| {
        "set INGOT_GDRIVE_CLIENT_ID to your Google OAuth client id (see docs/guides/google-drive.md)"
    })?;
    let client_secret = std::env::var("INGOT_GDRIVE_CLIENT_SECRET").ok();
    let path = cli::gdrive_credentials_path().ok_or("no app data folder on this system")?;

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(ingot_core::drive_login(
            client_id,
            client_secret,
            path.clone(),
            |url| {
                println!("Opening your browser to sign in. If it doesn't open, visit:\n{url}");
                let _ = Command::new("open").arg(url).status();
            },
        ))?;
    println!("Signed in. Saved to {}", path.display());
    Ok(ExitCode::SUCCESS)
}

/// Never replaces an existing file: the output is created new.
fn run_develop(args: DevelopArgs) -> AppResult {
    let started = Instant::now();
    let image = ingot_develop::develop_file(&args.raw, &args.params)?;
    let jpeg = ingot_core::preview::compress(turbojpeg::Image {
        pixels: image.rgb,
        width: image.width,
        pitch: image.width * 3,
        height: image.height,
        format: turbojpeg::PixelFormat::RGB,
    })?;
    let mut file = std::fs::File::create_new(&args.output)
        .map_err(|err| format!("could not create {}: {err}", args.output.display()))?;
    std::io::Write::write_all(&mut file, &jpeg)?;
    println!(
        "{}: {}x{} in {:.2?}",
        args.output.display(),
        image.width,
        image.height,
        started.elapsed()
    );
    Ok(ExitCode::SUCCESS)
}

const GDRIVE_TARGET_ID: u32 = 100;

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

fn run_import(args: ImportArgs) -> AppResult {
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
    let targets = args.second_copies.len() + 1 + usize::from(args.gdrive);
    println!(
        "copying {total_files} files, {:.1} GB, to {targets} target(s)",
        total_bytes as f64 / 1e9
    );

    let primary = local_target(1, destination);
    let mut secondaries: Vec<Arc<dyn ReplicationTarget>> = args
        .second_copies
        .iter()
        .enumerate()
        .map(|(index, root)| local_target(index as u32 + 2, root))
        .collect();
    if args.gdrive {
        let path = cli::gdrive_credentials_path().ok_or("no app data folder on this system")?;
        secondaries.push(Arc::new(DriveTarget::new(
            TargetId(GDRIVE_TARGET_ID),
            DriveConfig::default(),
            DriveCredentials::load(&path)?,
        )?));
    }

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
