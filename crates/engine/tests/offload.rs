use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

use backup_engine::{
    compare, hash_file, preflight, run_job, scan, CopyOptions, DestStatus, Error, ExistingPolicy,
    HashAlgo, JobEvent, JobSpec, Presence, VerifyMode,
};
use tempfile::TempDir;

/// Deterministic filler so failures are reproducible.
fn bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn write(root: &Path, rel: &str, data: &[u8]) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

/// A small card: nested folders, an empty file, a file that is not a whole
/// number of sectors or buffers, and OS junk that must be ignored.
fn make_card(root: &Path) {
    write(root, "DCIM/100NIKON/DSC_0001.MOV", &bytes(3 * 1024 * 1024 + 123, 1));
    write(root, "DCIM/100NIKON/DSC_0002.MOV", &bytes(70_000, 2));
    write(root, "DCIM/100NIKON/DSC_0003.MOV", &bytes(1024 * 1024, 3));
    write(root, "NIKON/EMPTY.BIN", b"");
    write(root, ".DS_Store", b"junk");
    write(root, "DCIM/._DSC_0001.MOV", b"junk");
    write(root, ".Spotlight-V100/store.db", b"junk");
    write(root, "System Volume Information/IndexerVolumeGuid", b"junk");
}

fn options() -> CopyOptions {
    CopyOptions {
        // Smaller than the test files so several chunks and a ragged tail are exercised.
        buffer_size: 1024 * 1024,
        ..CopyOptions::default()
    }
}

fn spec(source: &Path, dests: &[&Path], options: CopyOptions) -> JobSpec {
    JobSpec {
        source: source.to_owned(),
        destinations: dests.iter().map(|d| d.to_path_buf()).collect(),
        options,
    }
}

fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

fn part_files(root: &Path) -> Vec<PathBuf> {
    walkdir(root).into_iter().filter(|p| p.extension().is_some_and(|e| e == "part")).collect()
}

fn walkdir(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found
}

#[test]
fn scan_lists_footage_sorted_and_skips_os_junk() {
    let card = TempDir::new().unwrap();
    make_card(card.path());

    let files = scan(card.path(), |_| {}).unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();

    assert_eq!(
        paths,
        [
            "DCIM/100NIKON/DSC_0001.MOV",
            "DCIM/100NIKON/DSC_0002.MOV",
            "DCIM/100NIKON/DSC_0003.MOV",
            "NIKON/EMPTY.BIN",
        ]
    );
    assert_eq!(files[0].size, 3 * 1024 * 1024 + 123);
    assert_eq!(files[3].size, 0);
}

#[test]
fn copies_to_two_destinations_and_verifies_both() {
    let card = TempDir::new().unwrap();
    let (a, b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    make_card(card.path());
    let files = scan(card.path(), |_| {}).unwrap();
    let events = Mutex::new(Vec::new());

    let report = run_job(
        &spec(card.path(), &[a.path(), b.path()], options()),
        &files,
        &no_cancel(),
        &|event| events.lock().unwrap().push(event),
    )
    .unwrap();

    assert!(report.safe_to_format);
    assert_eq!(report.failed_files, 0);
    assert_eq!(report.files.len(), 4);
    for (file, result) in files.iter().zip(&report.files) {
        let original = fs::read(file.path_in(card.path())).unwrap();
        for (dest, outcome) in [a.path(), b.path()].iter().zip(&result.dests) {
            assert_eq!(outcome.status, DestStatus::Copied);
            assert!(outcome.verified);
            assert_eq!(fs::read(file.path_in(dest)).unwrap(), original, "{}", file.rel_path);
        }
    }
    // Junk never reaches the destination and no temporary files remain.
    assert_eq!(walkdir(a.path()).len(), 4);
    assert!(part_files(b.path()).is_empty());

    let events = events.into_inner().unwrap();
    let finished = events.iter().filter(|e| matches!(e, JobEvent::FileFinished { .. })).count();
    assert_eq!(finished, 4);
    for event in &events {
        if let JobEvent::Progress { work_done, work_total, bytes_done, bytes_total, .. } = event {
            assert!(work_done <= work_total && bytes_done <= bytes_total);
        }
    }
}

#[test]
fn reported_hash_matches_an_independent_read_for_every_algorithm() {
    let card = TempDir::new().unwrap();
    make_card(card.path());
    let files = scan(card.path(), |_| {}).unwrap();

    for algo in HashAlgo::ALL {
        let dest = TempDir::new().unwrap();
        let report = run_job(
            &spec(card.path(), &[dest.path()], CopyOptions { algo, ..options() }),
            &files,
            &no_cancel(),
            &|_| {},
        )
        .unwrap();
        assert!(report.safe_to_format, "{algo}");
        for (file, result) in files.iter().zip(&report.files) {
            // Different buffer size on purpose: chunking must not matter.
            let independent =
                hash_file(&file.path_in(dest.path()), algo, 64 * 1024, &no_cancel(), &|_| {}).unwrap();
            assert_eq!(result.hash.as_deref(), Some(independent.as_str()), "{algo} {}", file.rel_path);
        }
    }
}

#[test]
fn copy_keeps_the_source_modification_time() {
    let card = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    write(card.path(), "A001.MOV", &bytes(10_000, 7));
    let shot_at = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(card.path().join("A001.MOV"), shot_at).unwrap();
    let files = scan(card.path(), |_| {}).unwrap();

    run_job(&spec(card.path(), &[dest.path()], options()), &files, &no_cancel(), &|_| {}).unwrap();

    let copied = fs::metadata(dest.path().join("A001.MOV")).unwrap();
    assert_eq!(filetime::FileTime::from_last_modification_time(&copied), shot_at);
    assert_eq!(files[0].modified, Some(1_700_000_000));
}

#[test]
fn second_run_finds_everything_already_backed_up() {
    let card = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    make_card(card.path());
    let files = scan(card.path(), |_| {}).unwrap();
    let job = spec(card.path(), &[dest.path()], options());
    run_job(&job, &files, &no_cancel(), &|_| {}).unwrap();

    let report = run_job(&job, &files, &no_cancel(), &|_| {}).unwrap();

    assert!(report.safe_to_format);
    for result in &report.files {
        assert_eq!(result.dests[0].status, DestStatus::AlreadyPresent);
        assert!(result.dests[0].verified);
    }
}

#[test]
fn resumes_by_copying_only_what_is_missing() {
    let card = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    make_card(card.path());
    let files = scan(card.path(), |_| {}).unwrap();
    let job = spec(card.path(), &[dest.path()], options());
    run_job(&job, &files, &no_cancel(), &|_| {}).unwrap();
    fs::remove_file(dest.path().join("DCIM/100NIKON/DSC_0002.MOV")).unwrap();

    assert_eq!(
        compare(&files, dest.path()),
        [Presence::Present, Presence::Missing, Presence::Present, Presence::Present]
    );
    let report = run_job(&job, &files, &no_cancel(), &|_| {}).unwrap();

    assert!(report.safe_to_format);
    let statuses: Vec<DestStatus> = report.files.iter().map(|r| r.dests[0].status).collect();
    assert_eq!(
        statuses,
        [
            DestStatus::AlreadyPresent,
            DestStatus::Copied,
            DestStatus::AlreadyPresent,
            DestStatus::AlreadyPresent
        ]
    );
}

#[test]
fn corrupted_existing_copy_is_reported_and_left_untouched() {
    let card = TempDir::new().unwrap();
    let (good, bad) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    make_card(card.path());
    let files = scan(card.path(), |_| {}).unwrap();
    // Same size as the source, one flipped byte: only a checksum can tell.
    let mut corrupt = fs::read(card.path().join("DCIM/100NIKON/DSC_0002.MOV")).unwrap();
    corrupt[40_000] ^= 0x01;
    write(bad.path(), "DCIM/100NIKON/DSC_0002.MOV", &corrupt);
    assert_eq!(compare(&files, bad.path())[1], Presence::Present);

    let report = run_job(
        &spec(card.path(), &[good.path(), bad.path()], options()),
        &files,
        &no_cancel(),
        &|_| {},
    )
    .unwrap();

    assert!(!report.safe_to_format);
    assert_eq!(report.failed_files, 1);
    let result = &report.files[1];
    assert_eq!(result.dests[0].status, DestStatus::Copied);
    assert!(result.dests[0].verified);
    assert_eq!(result.dests[1].status, DestStatus::Failed);
    assert!(result.dests[1].error.as_deref().unwrap().contains("differs from the source"));
    assert_eq!(fs::read(bad.path().join("DCIM/100NIKON/DSC_0002.MOV")).unwrap(), corrupt);
    // The other three files still made it to both drives.
    assert_eq!(report.files.iter().filter(|r| r.is_safe()).count(), 3);
}

#[test]
fn existing_file_of_a_different_size_is_never_replaced_by_default() {
    let card = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    write(card.path(), "A001.MOV", &bytes(50_000, 9));
    write(dest.path(), "A001.MOV", b"something else");
    let files = scan(card.path(), |_| {}).unwrap();

    assert_eq!(compare(&files, dest.path()), [Presence::SizeMismatch]);
    let report =
        run_job(&spec(card.path(), &[dest.path()], options()), &files, &no_cancel(), &|_| {}).unwrap();

    assert!(!report.safe_to_format);
    assert_eq!(report.files[0].dests[0].status, DestStatus::Failed);
    assert_eq!(fs::read(dest.path().join("A001.MOV")).unwrap(), b"something else");
}

#[test]
fn overwrite_policy_replaces_an_existing_file() {
    let card = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    let footage = bytes(50_000, 9);
    write(card.path(), "A001.MOV", &footage);
    write(dest.path(), "A001.MOV", b"something else");
    let files = scan(card.path(), |_| {}).unwrap();
    let overwrite = CopyOptions { on_existing: ExistingPolicy::Overwrite, ..options() };

    let report =
        run_job(&spec(card.path(), &[dest.path()], overwrite), &files, &no_cancel(), &|_| {}).unwrap();

    assert!(report.safe_to_format);
    assert_eq!(fs::read(dest.path().join("A001.MOV")).unwrap(), footage);
}

#[test]
fn source_and_destination_verification_passes_on_a_healthy_card() {
    let card = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    make_card(card.path());
    let files = scan(card.path(), |_| {}).unwrap();
    let both = CopyOptions { verify: VerifyMode::SourceAndDestination, ..options() };

    let report =
        run_job(&spec(card.path(), &[dest.path()], both), &files, &no_cancel(), &|_| {}).unwrap();

    assert!(report.safe_to_format);
}

#[test]
fn unverified_copy_is_never_safe_to_format() {
    let card = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    make_card(card.path());
    let files = scan(card.path(), |_| {}).unwrap();
    let no_verify = CopyOptions { verify: VerifyMode::None, ..options() };

    let report =
        run_job(&spec(card.path(), &[dest.path()], no_verify), &files, &no_cancel(), &|_| {}).unwrap();

    assert_eq!(report.failed_files, 0);
    assert_eq!(walkdir(dest.path()).len(), 4);
    assert!(!report.safe_to_format);
}

#[test]
fn cancelling_mid_job_leaves_no_partial_files() {
    let card = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    make_card(card.path());
    let files = scan(card.path(), |_| {}).unwrap();
    let cancel = AtomicBool::new(false);

    // Stop as soon as the first file is done.
    let report = run_job(&spec(card.path(), &[dest.path()], options()), &files, &cancel, &|event| {
        if matches!(event, JobEvent::FileFinished { .. }) {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    })
    .unwrap();

    assert!(report.cancelled);
    assert!(!report.safe_to_format);
    assert_eq!(report.files.len(), 1);
    assert_eq!(walkdir(dest.path()).len(), 1);
    assert!(part_files(dest.path()).is_empty());
}

#[test]
fn cancelling_during_a_file_removes_its_partial_copy() {
    let card = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    write(card.path(), "BIG.MOV", &bytes(6 * 1024 * 1024, 4));
    let files = scan(card.path(), |_| {}).unwrap();
    let cancel = AtomicBool::new(false);

    let report = run_job(&spec(card.path(), &[dest.path()], options()), &files, &cancel, &|event| {
        if matches!(event, JobEvent::Progress { .. }) {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    })
    .unwrap();

    assert!(report.cancelled);
    assert!(report.files.is_empty());
    assert!(walkdir(dest.path()).is_empty());
}

#[test]
fn destination_inside_the_source_is_rejected() {
    let card = TempDir::new().unwrap();
    make_card(card.path());
    let files = scan(card.path(), |_| {}).unwrap();
    let inside = card.path().join("backup");

    let err = preflight(&spec(card.path(), &[&inside], options()), &files).unwrap_err();

    assert!(matches!(err, Error::InvalidJob(_)), "{err}");
}

#[test]
fn the_same_destination_twice_is_rejected() {
    let card = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    make_card(card.path());
    let files = scan(card.path(), |_| {}).unwrap();

    let err = preflight(&spec(card.path(), &[dest.path(), dest.path()], options()), &files).unwrap_err();

    assert!(matches!(err, Error::InvalidJob(_)), "{err}");
}

#[test]
fn missing_source_file_fails_that_file_only() {
    let card = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    make_card(card.path());
    let files = scan(card.path(), |_| {}).unwrap();
    // The card loses a clip between scan and copy.
    fs::remove_file(card.path().join("DCIM/100NIKON/DSC_0003.MOV")).unwrap();

    let report =
        run_job(&spec(card.path(), &[dest.path()], options()), &files, &no_cancel(), &|_| {}).unwrap();

    assert!(!report.safe_to_format);
    assert_eq!(report.failed_files, 1);
    assert!(report.files[2].error.is_some());
    assert_eq!(report.files.iter().filter(|r| r.is_safe()).count(), 3);
    assert!(part_files(dest.path()).is_empty());
}

fn shoot_layout() -> backup_engine::ShootLayout {
    backup_engine::ShootLayout {
        client: "Kestrel Running".into(),
        shoot: "Trail series".into(),
        date: "2026-11-08".into(),
        date_format: backup_engine::DateFormat::DayMonthYear,
        camera: "Z6 III".into(),
        reel: String::new(),
        keep_card_folders: false,
    }
}

#[test]
fn shoot_layout_copies_each_file_to_its_folder_and_verifies_it_there() {
    let card = TempDir::new().unwrap();
    let (a, b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    write(card.path(), "DCIM/100NCZ_6/DSC_0001.MOV", &bytes(200_000, 1));
    write(card.path(), "DCIM/100NCZ_6/DSC_0002.NEF", &bytes(90_000, 2));
    let mut files = scan(card.path(), |_| {}).unwrap();
    backup_engine::route(&mut files, &shoot_layout()).unwrap();
    let job = spec(card.path(), &[a.path(), b.path()], options());

    assert_eq!(compare(&files, a.path()), [Presence::Missing, Presence::Missing]);
    let report = run_job(&job, &files, &no_cancel(), &|_| {}).unwrap();

    assert!(report.safe_to_format);
    for root in [a.path(), b.path()] {
        let shoot = root.join("Kestrel Running/08.11.26 Trail series");
        assert_eq!(
            fs::read(shoot.join("Footage/Z6 III/DSC_0001.MOV")).unwrap(),
            fs::read(card.path().join("DCIM/100NCZ_6/DSC_0001.MOV")).unwrap()
        );
        assert!(shoot.join("Photo/Z6 III/DSC_0002.NEF").is_file());
        // Nothing lands at the card's own paths.
        assert!(!root.join("DCIM").exists());
    }
    assert_eq!(compare(&files, b.path()), [Presence::Present, Presence::Present]);
    // Running it again recognises the shoot as already backed up.
    let again = run_job(&job, &files, &no_cancel(), &|_| {}).unwrap();
    assert!(again.safe_to_format);
    assert!(again.files.iter().all(|r| r.dests.iter().all(|d| d.status == DestStatus::AlreadyPresent)));
}

#[test]
fn second_card_with_the_same_file_names_is_not_mistaken_for_the_first() {
    let (card_a, card_b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let dest = TempDir::new().unwrap();
    // Two cards from the same camera, both starting at DSC_0001.
    write(card_a.path(), "DCIM/100NCZ_6/DSC_0001.MOV", &bytes(120_000, 1));
    write(card_b.path(), "DCIM/100NCZ_6/DSC_0001.MOV", &bytes(120_000, 2));
    let run = |card: &Path| {
        let mut files = scan(card, |_| {}).unwrap();
        backup_engine::route(&mut files, &shoot_layout()).unwrap();
        run_job(&spec(card, &[dest.path()], options()), &files, &no_cancel(), &|_| {}).unwrap()
    };
    assert!(run(card_a.path()).safe_to_format);

    let second = run(card_b.path());

    // Same name, same size, different footage: the checksum catches it and
    // the first card's clip is not overwritten.
    assert!(!second.safe_to_format);
    assert_eq!(second.files[0].dests[0].status, DestStatus::Failed);
    assert_eq!(
        fs::read(dest.path().join("Kestrel Running/08.11.26 Trail series/Footage/Z6 III/DSC_0001.MOV")).unwrap(),
        fs::read(card_a.path().join("DCIM/100NCZ_6/DSC_0001.MOV")).unwrap()
    );
}

#[test]
fn two_files_bound_for_the_same_path_are_rejected_before_copying() {
    let card = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    write(card.path(), "A/CLIP.MOV", &bytes(1_000, 1));
    write(card.path(), "B/clip.mov", &bytes(1_000, 2));
    let mut files = scan(card.path(), |_| {}).unwrap();
    for file in &mut files {
        file.dest = Some(format!("Footage/{}", file.rel_path.rsplit('/').next().unwrap()));
    }

    let err = run_job(&spec(card.path(), &[dest.path()], options()), &files, &no_cancel(), &|_| {})
        .unwrap_err();

    assert!(matches!(err, Error::InvalidJob(_)), "{err}");
    assert!(walkdir(dest.path()).is_empty());
}

fn creator() -> backup_engine::mhl::Creator {
    backup_engine::mhl::Creator {
        tool: "BackupTool".into(),
        version: "test".into(),
    }
}

#[test]
fn mhl_records_only_what_was_proven_on_each_destination() {
    let card = TempDir::new().unwrap();
    let (good, bad) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    make_card(card.path());
    let files = scan(card.path(), |_| {}).unwrap();
    let mut corrupt = fs::read(card.path().join("DCIM/100NIKON/DSC_0002.MOV")).unwrap();
    corrupt[40_000] ^= 0x01;
    write(bad.path(), "DCIM/100NIKON/DSC_0002.MOV", &corrupt);
    let job = spec(card.path(), &[good.path(), bad.path()], options());
    let report = run_job(&job, &files, &no_cancel(), &|_| {}).unwrap();
    let record = |index: usize, root: &Path| {
        backup_engine::mhl::record_destination(
            root, "", &files, &report.files, index, HashAlgo::Xxh64, &creator(), &[],
        )
        .unwrap()
        .unwrap()
    };

    let on_good = record(0, good.path());
    let on_bad = record(1, bad.path());

    assert_eq!(on_good.original, 4);
    // The copy that did not match the card is not vouched for.
    assert_eq!(on_bad.original, 3);
    let manifest = fs::read_to_string(on_bad.manifest).unwrap();
    assert!(!manifest.contains("DSC_0002.MOV"));
    assert!(manifest.contains("DCIM/100NIKON/DSC_0001.MOV"));
}

#[test]
fn mhl_records_nothing_for_an_unverified_or_interrupted_copy() {
    let card = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    make_card(card.path());
    let files = scan(card.path(), |_| {}).unwrap();
    let record = |report: &backup_engine::JobReport| {
        backup_engine::mhl::record_destination(
            dest.path(), "", &files, &report.files, 0, HashAlgo::Xxh64, &creator(), &[],
        )
        .unwrap()
    };

    // Stopped after the first file: only that one is on record.
    let cancel = AtomicBool::new(false);
    let partial = run_job(&spec(card.path(), &[dest.path()], options()), &files, &cancel, &|event| {
        if matches!(event, JobEvent::FileFinished { .. }) {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    })
    .unwrap();
    assert_eq!(record(&partial).unwrap().original, 1);

    // Copied without verification: nothing was proven, nothing is written.
    let other = TempDir::new().unwrap();
    let no_verify = CopyOptions { verify: VerifyMode::None, ..options() };
    let unverified =
        run_job(&spec(card.path(), &[other.path()], no_verify), &files, &no_cancel(), &|_| {}).unwrap();
    let recorded = backup_engine::mhl::record_destination(
        other.path(), "", &files, &unverified.files, 0, HashAlgo::Xxh64, &creator(), &[],
    )
    .unwrap();
    assert!(recorded.is_none());
    assert!(!other.path().join("ascmhl").exists());
}
