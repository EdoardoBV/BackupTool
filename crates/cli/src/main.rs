//! Command-line front end for the engine: the quickest way to run it on real
//! cards and drives without the app.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use backup_engine::{
    analyze, compare, hash_file, run_job, scan, CopyOptions, DestStatus, ExistingPolicy, HashAlgo, JobEvent,
    JobSpec, Phase, Presence, VerifyMode,
};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "bt", about = "Card offload with checksum verification")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the files on a source
    Scan { source: PathBuf },
    /// Show what a card holds and which camera wrote it
    Card { source: PathBuf },
    /// Show which source files are missing from a destination (by path and size)
    Compare { source: PathBuf, dest: PathBuf },
    /// Copy a source to one or more destinations and verify every copy
    Copy {
        source: PathBuf,
        #[arg(required = true)]
        dest: Vec<PathBuf>,
        /// xxh64, xxh3, xxh128, md5, sha1, sha256 or c4
        #[arg(long, default_value = "xxh64")]
        algo: HashAlgo,
        #[arg(long, value_enum, default_value = "dest")]
        verify: Verify,
        /// Replace files already at the destination instead of checking them
        #[arg(long)]
        overwrite: bool,
        /// Record the verified files in an ASC MHL history at each destination
        #[arg(long)]
        mhl: bool,
    },
    /// Check an existing backup against its ASC MHL history
    Verify {
        /// A backup folder, or a folder with several backups inside
        folder: PathBuf,
        /// Add the result to the history as a new generation
        #[arg(long)]
        record: bool,
    },
    /// Checksum a single file, reading from the device rather than the cache
    Hash {
        file: PathBuf,
        #[arg(long, default_value = "xxh64")]
        algo: HashAlgo,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Verify {
    /// Copy only, prove nothing
    None,
    /// Read every copy back and compare with the source
    Dest,
    /// Also read the source a second time
    Source,
}

fn gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / 1e9)
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> backup_engine::Result<ExitCode> {
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let cancel = Arc::clone(&cancel);
        // Ctrl-C stops cleanly so no .part files are left behind.
        let _ = ctrlc::set_handler(move || cancel.store(true, Ordering::Relaxed));
    }

    match cli.command {
        Command::Scan { source } => {
            let files = scan(&source, |_| {})?;
            for file in &files {
                println!("{:>14}  {}", file.size, file.rel_path);
            }
            let total: u64 = files.iter().map(|f| f.size).sum();
            println!("{} files, {}", files.len(), gb(total));
        }
        Command::Card { source } => {
            let files = scan(&source, |_| {})?;
            let info = analyze(&source, &files);
            println!(
                "{} clips, {} photos, {} audio, {} other files",
                info.video, info.photo, info.audio, info.other
            );
            match info.camera {
                Some(camera) => println!(
                    "camera: {} {} (serial {})",
                    camera.make.as_deref().unwrap_or(""),
                    camera.model,
                    camera.serial.as_deref().unwrap_or("unknown")
                ),
                None => println!("camera: not recognised"),
            }
        }
        Command::Compare { source, dest } => {
            let files = scan(&source, |_| {})?;
            let presence = compare(&files, &dest);
            for (file, state) in files.iter().zip(&presence) {
                match state {
                    Presence::Missing => println!("missing    {}", file.rel_path),
                    Presence::SizeMismatch => println!("different  {}", file.rel_path),
                    Presence::Present => {}
                }
            }
            let absent = presence.iter().filter(|p| **p != Presence::Present).count();
            println!("{absent} of {} files aren't on {} yet", files.len(), dest.display());
        }
        Command::Verify { folder, record } => {
            use backup_engine::verify::{find_histories, verify_history, VerifyEvent};
            let roots = find_histories(&folder);
            if roots.is_empty() {
                println!("No ASC MHL history under {}", folder.display());
                return Ok(ExitCode::FAILURE);
            }
            let creator = backup_engine::mhl::Creator {
                tool: "BackupTool".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            };
            let mut intact = true;
            for root in roots {
                let report = verify_history(&root, record, &creator, &cancel, &|event| {
                    if let VerifyEvent::Progress { bytes_done, bytes_total } = event {
                        eprint!("\rChecking {:>3}%   ", bytes_done * 100 / bytes_total.max(1));
                        let _ = std::io::stderr().flush();
                    }
                })?;
                eprintln!();
                println!(
                    "{}: {} of {} files intact, {} changed, {} missing, {} not in the history",
                    root.display(),
                    report.verified,
                    report.files,
                    report.mismatched.len(),
                    report.missing.len(),
                    report.untracked.len()
                );
                for changed in &report.mismatched {
                    println!("  CHANGED {}", changed.path);
                }
                for path in &report.missing {
                    println!("  MISSING {path}");
                }
                for path in &report.unsupported {
                    println!("  UNSUPPORTED CHECKSUM {path}");
                }
                if let Some(generation) = &report.recorded {
                    println!("  recorded as generation {}", generation.number);
                }
                intact &= report.intact();
            }
            if !intact {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Hash { file, algo } => {
            let started = Instant::now();
            let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            let hash = hash_file(&file, algo, 8 * 1024 * 1024, &cancel, &|_| {})?;
            let secs = started.elapsed().as_secs_f64().max(1e-9);
            println!("{hash}  {}", file.display());
            eprintln!("{algo}: {:.0} MB/s", size as f64 / 1e6 / secs);
        }
        Command::Copy { source, dest, algo, verify, overwrite, mhl } => {
            let files = scan(&source, |_| {})?;
            let spec = JobSpec {
                source,
                destinations: dest,
                options: CopyOptions {
                    algo,
                    verify: match verify {
                        Verify::None => VerifyMode::None,
                        Verify::Dest => VerifyMode::Destination,
                        Verify::Source => VerifyMode::SourceAndDestination,
                    },
                    on_existing: if overwrite { ExistingPolicy::Overwrite } else { ExistingPolicy::Verify },
                    ..CopyOptions::default()
                },
            };
            let total = files.len();
            let report = run_job(&spec, &files, &cancel, &|event| match event {
                JobEvent::Progress { index, phase, bytes_done, bytes_total, work_done, work_total } => {
                    let verb = match phase {
                        Phase::Copying => "Copying",
                        Phase::Verifying => "Checking",
                    };
                    eprint!(
                        "\r{verb} {} of {total} · {} of {} · {:>3}%   ",
                        index + 1,
                        gb(bytes_done),
                        gb(bytes_total),
                        work_done * 100 / work_total.max(1)
                    );
                    let _ = std::io::stderr().flush();
                }
                JobEvent::FileFinished { result, .. } => {
                    for (n, dest) in result.dests.iter().enumerate() {
                        if dest.status == DestStatus::Failed {
                            eprintln!(
                                "\rFAILED {} (destination {}): {}",
                                result.rel_path,
                                n + 1,
                                dest.error.as_deref().unwrap_or("unknown error")
                            );
                        }
                    }
                }
                JobEvent::FileStarted { .. } => {}
            })?;

            let secs = (report.duration_ms as f64 / 1000.0).max(1e-9);
            eprintln!();
            if mhl {
                let creator = backup_engine::mhl::Creator {
                    tool: "BackupTool".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                };
                for (index, dest) in spec.destinations.iter().enumerate() {
                    let recorded = backup_engine::mhl::record_destination(
                        dest, "", &files, &report.files, index, algo, &creator, &[],
                    )?;
                    match recorded {
                        Some(generation) => println!(
                            "MHL generation {} at {}: {} new, {} verified, {} failed",
                            generation.number,
                            dest.display(),
                            generation.original,
                            generation.verified,
                            generation.failed.len()
                        ),
                        None => println!("MHL: nothing verified at {}", dest.display()),
                    }
                }
            }
            println!(
                "{} of {} files done, {} failed, {} in {:.1}s ({:.0} MB/s)",
                report.files.len(),
                report.total_files,
                report.failed_files,
                gb(report.total_bytes),
                secs,
                report.total_bytes as f64 / 1e6 / secs
            );
            if report.safe_to_format {
                println!("Every file is verified on every destination. Safe to format.");
            } else if report.cancelled {
                println!("Stopped. Don't format.");
                return Ok(ExitCode::from(2));
            } else {
                println!("Not everything is verified. Don't format.");
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
