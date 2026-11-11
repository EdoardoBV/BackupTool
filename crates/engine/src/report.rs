//! The record of a copy job for people: an HTML page that prints cleanly
//! (and so becomes a PDF from any browser) and a CSV for spreadsheets.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Local};

use crate::card::CardInfo;
use crate::copy::{CopyOptions, DestOutcome, DestStatus, VerifyMode};
use crate::error::{IoContext, Result};
use crate::hash::HashAlgo;
use crate::job::JobReport;
use crate::scan::FileEntry;

/// Folder, next to the footage, that reports are written to.
pub const REPORTS_DIR: &str = "Reports";

pub struct ReportInput<'a> {
    /// What was backed up, in the user's words: the shoot, or the source name.
    pub title: &'a str,
    pub source: &'a Path,
    pub destinations: &'a [PathBuf],
    pub card: Option<&'a CardInfo>,
    pub options: &'a CopyOptions,
    /// The job's files; `report.files[i]` is the result for `files[i]`.
    pub files: &'a [FileEntry],
    pub report: &'a JobReport,
    pub finished_at: DateTime<Local>,
    /// Extra lines for the summary, such as what was written to the MHL history.
    pub notes: &'a [String],
    pub tool: &'a str,
}

fn algo_name(algo: HashAlgo) -> &'static str {
    match algo {
        HashAlgo::Xxh64 => "xxHash64",
        HashAlgo::Xxh3 => "XXH3",
        HashAlgo::Xxh128 => "XXH128",
        HashAlgo::Md5 => "MD5",
        HashAlgo::Sha1 => "SHA-1",
        HashAlgo::Sha256 => "SHA-256",
        HashAlgo::C4 => "C4",
    }
}

fn verify_name(mode: VerifyMode) -> &'static str {
    match mode {
        VerifyMode::None => "nessuna",
        VerifyMode::Destination => "ogni copia riletta e confrontata con la scheda",
        VerifyMode::SourceAndDestination => "copie e scheda rilette e confrontate",
    }
}

/// Decimal units with an Italian decimal comma: `172,4 GB`.
fn size(bytes: u64) -> String {
    let (value, unit) = match bytes {
        b if b >= 1_000_000_000 => (b as f64 / 1e9, "GB"),
        b if b >= 1_000_000 => (b as f64 / 1e6, "MB"),
        b if b >= 1_000 => return format!("{} kB", (b as f64 / 1e3).round()),
        b => return format!("{b} B"),
    };
    format!("{value:.1} {unit}").replace('.', ",")
}

fn duration(ms: u64) -> String {
    let secs = ms / 1000;
    match (secs / 3600, secs % 3600 / 60, secs % 60) {
        (0, 0, s) => format!("{s} s"),
        (0, m, s) => format!("{m} min {s:02} s"),
        (h, m, _) => format!("{h} h {m:02} min"),
    }
}

fn status(outcome: &DestOutcome) -> String {
    match (outcome.status, outcome.verified) {
        (DestStatus::Copied, true) => "Copiato e verificato".into(),
        (DestStatus::AlreadyPresent, true) => "Già presente, verificato".into(),
        (DestStatus::Copied, false) => "Copiato, non verificato".into(),
        (DestStatus::AlreadyPresent, false) => "Già presente, non verificato".into(),
        (DestStatus::Failed, _) => format!("Errore: {}", outcome.error.as_deref().unwrap_or("sconosciuto")),
    }
}

/// "i 131 file", "gli 8 file": the article follows how the number sounds
/// (otto, undici, ottanta… start with a vowel). One file reads "i file (1)"
/// so the plural sentences it goes into stay correct.
pub fn the_files(count: usize) -> String {
    if count == 1 {
        return "i file (1)".into();
    }
    let digits = count.to_string();
    let leading: usize = digits[..(digits.len() - 1) % 3 + 1].parse().unwrap_or(0);
    let vowel = leading == 8 || leading == 11 || (80..=89).contains(&leading) || (800..=899).contains(&leading);
    format!("{} {count} file", if vowel { "gli" } else { "i" })
}

fn verdict(report: &JobReport) -> (bool, String) {
    if report.safe_to_format {
        (true, format!("Tutti {} sono verificati su ogni destinazione.", the_files(report.total_files)))
    } else if report.cancelled {
        (false, format!("Copia fermata: {} file su {} completati.", report.files.len(), report.total_files))
    } else if report.failed_files > 0 {
        (false, format!("{} file su {} non verificati.", report.failed_files, report.total_files))
    } else {
        (false, "Copia eseguita senza verifica.".into())
    }
}

fn leaf(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Semicolon-separated, the separator Italian spreadsheet setups expect.
fn csv_field(text: &str) -> String {
    if text.contains([';', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

pub fn csv(input: &ReportInput) -> String {
    let mut header = vec![
        "File sulla scheda".to_string(),
        "Percorso sulla destinazione".to_string(),
        "Byte".to_string(),
        format!("Checksum {}", algo_name(input.options.algo)),
    ];
    header.extend(input.destinations.iter().map(|dest| leaf(dest)));
    // The byte-order mark makes Excel read the file as UTF-8.
    let mut out = format!("\u{feff}{}\r\n", header.iter().map(|h| csv_field(h)).collect::<Vec<_>>().join(";"));
    for (file, result) in input.files.iter().zip(&input.report.files) {
        let mut row = vec![
            csv_field(&file.rel_path),
            csv_field(file.dest_rel()),
            file.size.to_string(),
            result.hash.clone().unwrap_or_default(),
        ];
        row.extend(result.dests.iter().map(|outcome| csv_field(&status(outcome))));
        out.push_str(&row.join(";"));
        out.push_str("\r\n");
    }
    out
}

pub fn html(input: &ReportInput) -> String {
    let report = input.report;
    let (ok, verdict) = verdict(report);
    let started = input.finished_at - Duration::milliseconds(report.duration_ms as i64);
    let copied: u64 = report.files.iter().map(|f| f.size).sum();
    let speed = (copied * 1000)
        .checked_div(report.duration_ms)
        .map_or("–".into(), |rate| format!("{}/s", size(rate)));

    let mut facts: Vec<(&str, String)> = vec![("Sorgente", input.source.display().to_string())];
    if let Some(card) = input.card {
        if let Some(camera) = &card.camera {
            let serial = camera.serial.as_ref().map(|s| format!(", seriale {s}")).unwrap_or_default();
            let make = camera.make.as_deref().unwrap_or("");
            facts.push(("Camera", format!("{make} {}{serial}", camera.model).trim().to_string()));
        }
        let kinds = [(card.video, "clip"), (card.photo, "foto"), (card.audio, "audio"), (card.other, "altri file")];
        let content: Vec<String> =
            kinds.iter().filter(|(n, _)| *n > 0).map(|(n, what)| format!("{n} {what}")).collect();
        facts.push(("Contenuto", content.join(", ")));
    }
    facts.push(("Dimensione", format!("{} in {} file", size(report.total_bytes), report.total_files)));
    for (index, dest) in input.destinations.iter().enumerate() {
        facts.push((if index == 0 { "Destinazioni" } else { "" }, dest.display().to_string()));
    }
    facts.push(("Checksum", algo_name(input.options.algo).into()));
    facts.push(("Verifica", verify_name(input.options.verify).into()));
    facts.push(("Inizio", started.format("%d/%m/%Y %H:%M:%S").to_string()));
    facts.push(("Fine", input.finished_at.format("%d/%m/%Y %H:%M:%S").to_string()));
    facts.push(("Durata", format!("{}, in media {speed}", duration(report.duration_ms))));
    for note in input.notes {
        facts.push(("Note", note.clone()));
    }
    let facts: String = facts
        .iter()
        .map(|(name, value)| format!("<tr><th>{}</th><td>{}</td></tr>\n", name, html_escape(value)))
        .collect();

    let dest_heads: String =
        input.destinations.iter().map(|d| format!("<th>{}</th>", html_escape(&leaf(d)))).collect();
    let mut rows = String::new();
    for (index, (file, result)) in input.files.iter().zip(&report.files).enumerate() {
        let failed = result.error.is_some() || result.dests.iter().any(|d| d.status == DestStatus::Failed);
        let cells: String = result
            .dests
            .iter()
            .map(|outcome| format!("<td>{}</td>", html_escape(&status(outcome))))
            .collect();
        rows.push_str(&format!(
            "<tr{}><td class=\"n\">{}</td><td>{}</td><td class=\"n\">{}</td><td class=\"{}\">{}</td>{cells}</tr>\n",
            if failed { " class=\"failed\"" } else { "" },
            index + 1,
            html_escape(file.dest_rel()),
            size(file.size),
            // A C4 ID is 90 characters: let that one wrap.
            if result.hash.as_ref().is_some_and(|h| h.len() > 40) { "hash long" } else { "hash" },
            html_escape(result.hash.as_deref().unwrap_or("–")),
        ));
    }

    format!(
        r#"<!doctype html>
<html lang="it">
<head>
<meta charset="utf-8">
<title>Report di copia: {title}</title>
<style>
  body {{ font: 13px/1.45 "Segoe UI", -apple-system, system-ui, sans-serif; color: #1f2225; margin: 32px; font-variant-numeric: tabular-nums; }}
  h1 {{ font-size: 22px; margin: 0 0 4px; }}
  .verdict {{ margin: 16px 0 24px; padding: 12px 16px; font-size: 16px; font-weight: 700; color: #fff; background: {color}; }}
  table {{ border-collapse: collapse; width: 100%; }}
  .facts {{ width: auto; margin-bottom: 28px; }}
  .facts th {{ text-align: left; font-weight: 600; color: #5d646b; padding: 2px 24px 2px 0; vertical-align: top; white-space: nowrap; }}
  .facts td {{ padding: 2px 0; }}
  .files th {{ text-align: left; font-size: 12px; color: #5d646b; border-bottom: 2px solid #1f2225; padding: 6px 12px 6px 0; }}
  .files td {{ border-bottom: 1px solid #d7d5cf; padding: 5px 12px 5px 0; vertical-align: top; overflow-wrap: anywhere; }}
  .n {{ text-align: right; white-space: nowrap; }}
  .hash {{ font-family: Consolas, Menlo, monospace; font-size: 12px; white-space: nowrap; }}
  .hash.long {{ white-space: normal; }}
  .failed td {{ color: #b3261e; font-weight: 600; }}
  footer {{ margin-top: 28px; color: #5d646b; font-size: 12px; }}
  @media print {{ body {{ margin: 0; }} tr {{ break-inside: avoid; }} thead {{ display: table-header-group; }} }}
</style>
</head>
<body>
<h1>{title}</h1>
<p>Report di copia</p>
<p class="verdict">{banner} {verdict}</p>
<table class="facts">
{facts}</table>
<table class="files">
<thead><tr><th class="n">N.</th><th>File</th><th class="n">Dimensione</th><th>Checksum</th>{dest_heads}</tr></thead>
<tbody>
{rows}</tbody>
</table>
<footer>Generato da {tool} su {host}.</footer>
</body>
</html>
"#,
        title = html_escape(input.title),
        color = if ok { "#2c8a53" } else { "#c63a28" },
        banner = if ok { "Si può formattare." } else { "Non formattare." },
        verdict = html_escape(&verdict),
        tool = html_escape(input.tool),
        host = html_escape(&gethostname::gethostname().to_string_lossy()),
    )
}

/// Writes both files into the `Reports` folder under `folder` and returns the
/// path of the HTML one. The name carries the time, so reports never collide.
pub fn write(input: &ReportInput, folder: &Path) -> Result<PathBuf> {
    let dir = folder.join(REPORTS_DIR);
    fs::create_dir_all(&dir).at(&dir)?;
    let stamp = input.finished_at.format("%Y-%m-%d %H%M%S");
    let name: String = leaf(input.source)
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '-' } else { c })
        .collect();
    let stem = format!("{stamp} {}", name.trim());
    let html_path = dir.join(format!("{}.html", stem.trim()));
    let csv_path = dir.join(format!("{}.csv", stem.trim()));
    fs::write(&html_path, html(input)).at(&html_path)?;
    fs::write(&csv_path, csv(input)).at(&csv_path)?;
    Ok(html_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copy::FileResult;

    fn outcome(status: DestStatus, verified: bool, error: Option<&str>) -> DestOutcome {
        DestOutcome {
            status,
            verified,
            error: error.map(str::to_string),
        }
    }

    fn sample() -> (Vec<FileEntry>, JobReport) {
        let files = vec![
            FileEntry {
                rel_path: "DCIM/100NCZ_6/DSC_0001.MOV".into(),
                size: 1_500_000_000,
                modified: None,
                dest: Some("Kestrel/08.11.26 Trail/Footage/Z6 III/DSC_0001.MOV".into()),
            },
            FileEntry {
                rel_path: "DCIM/100NCZ_6/Tom & \"Jerry\"; <take 2>.MOV".into(),
                size: 900,
                modified: None,
                dest: None,
            },
        ];
        let results = vec![
            FileResult {
                rel_path: files[0].rel_path.clone(),
                size: files[0].size,
                hash: Some("95e230e90be29dd6".into()),
                verified_at: None,
                dests: vec![
                    outcome(DestStatus::Copied, true, None),
                    outcome(DestStatus::AlreadyPresent, true, None),
                ],
                error: None,
            },
            FileResult {
                rel_path: files[1].rel_path.clone(),
                size: files[1].size,
                hash: Some("0123456789abcdef".into()),
                verified_at: None,
                dests: vec![
                    outcome(DestStatus::Copied, true, None),
                    outcome(DestStatus::Failed, false, Some("checksum mismatch after copying")),
                ],
                error: None,
            },
        ];
        let report = JobReport {
            files: results,
            total_files: 2,
            total_bytes: 1_500_000_900,
            failed_files: 1,
            cancelled: false,
            safe_to_format: false,
            duration_ms: 75_000,
        };
        (files, report)
    }

    fn input<'a>(files: &'a [FileEntry], report: &'a JobReport, dests: &'a [PathBuf], options: &'a CopyOptions) -> ReportInput<'a> {
        ReportInput {
            title: "Kestrel Running / 08.11.26 Trail series",
            source: Path::new("E:/"),
            destinations: dests,
            card: None,
            options,
            files,
            report,
            finished_at: Local::now(),
            notes: &[],
            tool: "BackupTool 0.1.0",
        }
    }

    #[test]
    fn formats_sizes_and_durations_for_people() {
        assert_eq!(size(172_400_000_000), "172,4 GB");
        assert_eq!(size(755_000_000), "755,0 MB");
        assert_eq!(size(70_000), "70 kB");
        assert_eq!(size(12), "12 B");
        assert_eq!(duration(42_000), "42 s");
        assert_eq!(duration(75_000), "1 min 15 s");
        assert_eq!(duration(3_900_000), "1 h 05 min");
    }

    #[test]
    fn the_article_follows_how_the_number_sounds() {
        assert_eq!(the_files(131), "i 131 file");
        assert_eq!(the_files(8), "gli 8 file");
        assert_eq!(the_files(11), "gli 11 file");
        assert_eq!(the_files(84), "gli 84 file");
        assert_eq!(the_files(812), "gli 812 file");
        assert_eq!(the_files(8_400), "gli 8400 file");
        assert_eq!(the_files(18), "i 18 file");
        assert_eq!(the_files(1_100), "i 1100 file");
        assert_eq!(the_files(2), "i 2 file");
        assert_eq!(the_files(1), "i file (1)");
    }

    #[test]
    fn html_states_the_verdict_and_lists_every_file_safely() {
        let (files, report) = sample();
        let dests = [PathBuf::from("F:/T7 Shield"), PathBuf::from("G:/Backup")];
        let options = CopyOptions::default();

        let page = html(&input(&files, &report, &dests, &options));

        assert!(page.contains("Non formattare. 1 file su 2 non verificati."));
        assert!(page.contains("<th>T7 Shield</th><th>Backup</th>"));
        assert!(page.contains("Kestrel/08.11.26 Trail/Footage/Z6 III/DSC_0001.MOV"));
        assert!(page.contains("1,5 GB"));
        assert!(page.contains("<td>Già presente, verificato</td>"));
        // The failed row is marked and says why; odd names cannot break the page.
        assert!(page.contains("<tr class=\"failed\">"));
        assert!(page.contains("Errore: checksum mismatch after copying"));
        assert!(page.contains("Tom &amp; &quot;Jerry&quot;; &lt;take 2&gt;.MOV"));
        assert_eq!(page.matches("<td class=\"hash\">").count(), 2);
        assert!(page.contains("1 min 15 s"));
    }

    #[test]
    fn a_clean_job_reads_as_safe() {
        let (files, mut report) = sample();
        report.files.truncate(1);
        report.total_files = 1;
        report.failed_files = 0;
        report.safe_to_format = true;
        let dests = [PathBuf::from("F:/T7 Shield"), PathBuf::from("G:/Backup")];
        let options = CopyOptions::default();

        let page = html(&input(&files[..1], &report, &dests, &options));

        assert!(page.contains("Si può formattare. Tutti i file (1) sono verificati su ogni destinazione."));
        assert!(!page.contains("class=\"failed\""));
    }

    #[test]
    fn csv_has_one_row_per_file_and_quotes_awkward_names() {
        let (files, report) = sample();
        let dests = [PathBuf::from("F:/T7 Shield"), PathBuf::from("G:/Backup")];
        let options = CopyOptions::default();

        let sheet = csv(&input(&files, &report, &dests, &options));

        let lines: Vec<&str> = sheet.trim_start_matches('\u{feff}').lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "File sulla scheda;Percorso sulla destinazione;Byte;Checksum xxHash64;T7 Shield;Backup");
        assert_eq!(
            lines[1],
            "DCIM/100NCZ_6/DSC_0001.MOV;Kestrel/08.11.26 Trail/Footage/Z6 III/DSC_0001.MOV;1500000000;95e230e90be29dd6;Copiato e verificato;Già presente, verificato"
        );
        assert!(lines[2].starts_with("\"DCIM/100NCZ_6/Tom & \"\"Jerry\"\"; <take 2>.MOV\";"));
    }

    #[test]
    fn writes_both_files_into_a_reports_folder() {
        let dir = tempfile::TempDir::new().unwrap();
        let (files, report) = sample();
        let dests = [dir.path().to_owned()];
        let options = CopyOptions::default();

        let written = write(&input(&files, &report, &dests, &options), dir.path()).unwrap();

        assert_eq!(written.parent().unwrap(), dir.path().join("Reports"));
        assert!(written.extension().is_some_and(|e| e == "html"));
        assert!(written.with_extension("csv").is_file());
        assert!(fs::read_to_string(written).unwrap().starts_with("<!doctype html>"));
    }
}
