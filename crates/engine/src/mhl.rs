//! ASC MHL v2.0 histories: the `ascmhl` folder that travels with footage and
//! lets any MHL-aware tool check, years later, that no file has changed.
//!
//! Each verified copy job appends one generation: a manifest listing the
//! files it proved, and an entry in the chain file sealing that manifest
//! with its C4 ID. The output follows the ASC MHL reference implementation;
//! directory hashes, which the format makes optional, are not written.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::Serialize;

use crate::copy::{hash_file, FileResult};
use crate::error::{Error, IoContext, Result};
use crate::hash::{HashAlgo, Hasher};
use crate::scan::FileEntry;

pub(crate) const HISTORY_DIR: &str = "ascmhl";
const CHAIN_FILE: &str = "ascmhl_chain.xml";
/// What every ASC MHL history leaves out, in the reference tool's order.
pub(crate) const DEFAULT_IGNORE: [&str; 3] = [".DS_Store", "ascmhl", "ascmhl/"];

/// The tool named in each manifest.
#[derive(Debug, Clone)]
pub struct Creator {
    pub tool: String,
    pub version: String,
}

/// What a generation records: files copied to where they are now, or files
/// checked where they already were.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Process {
    Transfer,
    InPlace,
}

/// One verified file to record, with its path relative to the history root.
#[derive(Debug, Clone)]
pub struct Entry<'a> {
    pub path: &'a str,
    pub algo: HashAlgo,
    pub hash: &'a str,
    /// When the checksum was confirmed, seconds since the Unix epoch.
    pub hashed_at: Option<i64>,
}

/// What appending a generation did.
#[derive(Debug, Clone, Serialize)]
pub struct Generation {
    pub manifest: PathBuf,
    pub number: usize,
    /// Files recorded for the first time.
    pub original: usize,
    /// Files already in the history whose checksum still matches.
    pub verified: usize,
    /// Files whose checksum no longer matches what the history recorded.
    pub failed: Vec<String>,
}

struct ChainLink {
    file: String,
    c4: String,
}

#[derive(Default)]
pub(crate) struct History {
    chain: Vec<ChainLink>,
    /// Per file, the first checksum recorded in each format.
    pub(crate) known: HashMap<String, Vec<(String, String)>>,
    pub(crate) ignore: Vec<String>,
}

impl History {
    pub(crate) fn generations(&self) -> usize {
        self.chain.len()
    }
}

fn broken(message: impl Into<String>) -> Error {
    Error::Mhl(message.into())
}

fn c4_of(bytes: &[u8]) -> String {
    let mut hasher = Hasher::new(HashAlgo::C4);
    hasher.update(bytes);
    hasher.finalize()
}

fn child_text<'a>(node: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.children().find(|c| c.has_tag_name(name))?.text().map(str::trim)
}

/// Reads an existing history and checks its seal: every manifest must still
/// match the C4 ID the chain recorded for it.
pub(crate) fn load_history(dir: &Path) -> Result<History> {
    let chain_path = dir.join(CHAIN_FILE);
    let mut history = History::default();
    let chain_xml = match fs::read_to_string(&chain_path) {
        Ok(xml) => xml,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let orphans = fs::read_dir(dir)
                .map(|entries| entries.flatten().any(|e| e.path().extension().is_some_and(|x| x == "mhl")))
                .unwrap_or(false);
            if orphans {
                return Err(broken(format!("{} has manifests but no chain file", dir.display())));
            }
            return Ok(history);
        }
        Err(err) => return Err(err).at(&chain_path),
    };
    let chain = roxmltree::Document::parse(&chain_xml)
        .map_err(|err| broken(format!("{}: {err}", chain_path.display())))?;
    for link in chain.descendants().filter(|n| n.has_tag_name("hashlist")) {
        let (Some(file), Some(c4)) = (child_text(link, "path"), child_text(link, "c4")) else {
            return Err(broken(format!("{}: incomplete entry", chain_path.display())));
        };
        let manifest_path = dir.join(file);
        let bytes = fs::read(&manifest_path).at(&manifest_path)?;
        if c4_of(&bytes) != c4 {
            return Err(broken(format!(
                "{file} was changed after it was written; the history cannot be trusted"
            )));
        }
        let xml = String::from_utf8_lossy(&bytes);
        let manifest =
            roxmltree::Document::parse(&xml).map_err(|err| broken(format!("{file}: {err}")))?;
        for node in manifest.descendants() {
            if node.has_tag_name("hash") {
                let Some(path) = child_text(node, "path") else { continue };
                let formats = history.known.entry(path.to_string()).or_default();
                for value in node.children().filter(|c| c.is_element() && !c.has_tag_name("path")) {
                    let format = value.tag_name().name();
                    if !formats.iter().any(|(known, _)| known == format) {
                        formats.push((format.to_string(), value.text().unwrap_or("").trim().to_string()));
                    }
                }
            } else if node.has_tag_name("pattern") {
                let pattern = node.text().unwrap_or("").to_string();
                if !history.ignore.contains(&pattern) {
                    history.ignore.push(pattern);
                }
            }
        }
        history.chain.push(ChainLink {
            file: file.to_string(),
            c4: c4.to_string(),
        });
    }
    Ok(history)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn iso(time: DateTime<Local>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, false)
}

pub(crate) fn path_under(root: &Path, rel: &str) -> PathBuf {
    let mut path = root.to_owned();
    path.extend(rel.split('/').filter(|part| !part.is_empty()));
    path
}

/// Writes through a temporary file so a crash never leaves half a manifest.
fn write_whole(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents).at(&tmp)?;
    fs::rename(&tmp, path).at(path)
}

/// Appends a generation to the history rooted at `root`, creating the
/// history if there is none. `ignore` lists extra patterns (gitignore style)
/// for things under the root this history deliberately does not track.
///
/// Returns `None` when there is nothing to record. Fails without writing if
/// the existing history has been tampered with.
pub fn append_generation(
    root: &Path,
    entries: &[Entry],
    creator: &Creator,
    ignore: &[String],
    process: Process,
) -> Result<Option<Generation>> {
    if let Some(entry) = entries.iter().find(|entry| !entry.algo.in_mhl()) {
        return Err(broken(format!("{} is not an ASC MHL checksum type", entry.algo)));
    }
    let dir = root.join(HISTORY_DIR);
    let history = load_history(&dir)?;
    let now = Local::now();

    let mut entries: Vec<&Entry> = entries
        .iter()
        // Files of a history nested deeper down belong to that history.
        .filter(|entry| !entry.path.split('/').any(|part| part == HISTORY_DIR))
        .collect();
    entries.sort_by_key(|entry| entry.path);
    if entries.is_empty() {
        return Ok(None);
    }

    let (mut original, mut verified, mut failed) = (0, 0, Vec::new());
    let mut hashes = String::new();
    for entry in entries {
        let algo = entry.algo;
        let file = path_under(root, entry.path);
        let meta = fs::metadata(&file).at(&file)?;
        let hashed_at = entry
            .hashed_at
            .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0))
            .map_or(now, |utc| utc.with_timezone(&Local));

        // (format, checksum, action) lines for this file.
        let mut lines: Vec<(String, String, &str)> = Vec::new();
        match history.known.get(entry.path) {
            None => {
                original += 1;
                lines.push((algo.id().into(), entry.hash.into(), "original"));
            }
            Some(formats) => {
                let ok = match formats.iter().find(|(format, _)| format == algo.id()) {
                    Some((_, recorded)) => recorded == entry.hash,
                    None => {
                        // The history knows this file only by another kind of
                        // checksum. Recompute that one, so the new checksum
                        // is tied to the content the history vouches for.
                        let (format, recorded) = &formats[0];
                        let known_algo: HashAlgo = format.parse().map_err(|_| {
                            broken(format!("the history uses checksum type {format}, which is not supported"))
                        })?;
                        let current = hash_file(&file, known_algo, 8 * 1024 * 1024, &AtomicBool::new(false), &|_| {})?;
                        let ok = current == *recorded;
                        lines.push((format.clone(), current, if ok { "verified" } else { "failed" }));
                        ok
                    }
                };
                if ok {
                    verified += 1;
                } else {
                    failed.push(entry.path.to_string());
                }
                lines.push((algo.id().into(), entry.hash.into(), if ok { "verified" } else { "failed" }));
            }
        }

        let modified = meta.modified().map(DateTime::<Local>::from).unwrap_or(now);
        hashes.push_str(&format!(
            "    <hash>\n      <path size=\"{}\" lastmodificationdate=\"{}\">{}</path>\n",
            meta.len(),
            iso(modified),
            escape(entry.path)
        ));
        // The schema fixes the order of checksum elements: alphabetical.
        lines.sort();
        for (format, hash, action) in lines {
            hashes.push_str(&format!(
                "      <{format} action=\"{action}\" hashdate=\"{}\">{hash}</{format}>\n",
                iso(hashed_at)
            ));
        }
        hashes.push_str("    </hash>\n");
    }

    let mut patterns: Vec<&str> = DEFAULT_IGNORE.to_vec();
    for pattern in history.ignore.iter().chain(ignore) {
        if !patterns.contains(&pattern.as_str()) {
            patterns.push(pattern.as_str());
        }
    }
    let patterns: String =
        patterns.iter().map(|p| format!("      <pattern>{}</pattern>\n", escape(p))).collect();

    let process = match process {
        Process::Transfer => "transfer",
        Process::InPlace => "in-place",
    };
    let manifest_xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <hashlist version=\"2.0\" xmlns=\"urn:ASC:MHL:v2.0\">\n  \
           <creatorinfo>\n    \
             <creationdate>{}</creationdate>\n    \
             <hostname>{}</hostname>\n    \
             <tool version=\"{}\">{}</tool>\n  \
           </creatorinfo>\n  \
           <processinfo>\n    \
             <process>{process}</process>\n    \
             <ignore>\n{patterns}    </ignore>\n  \
           </processinfo>\n  \
           <hashes>\n{hashes}  </hashes>\n\
         </hashlist>\n",
        iso(now),
        escape(&gethostname::gethostname().to_string_lossy()),
        escape(&creator.version),
        escape(&creator.tool),
    );

    let number = history.chain.len() + 1;
    let root_name = root.file_name().map_or("volume".into(), |name| name.to_string_lossy());
    let file_name = format!(
        "{number:04}_{root_name}_{}Z.mhl",
        now.with_timezone(&Utc).format("%Y-%m-%d_%H%M%S")
    );
    fs::create_dir_all(&dir).at(&dir)?;
    let manifest = dir.join(&file_name);
    write_whole(&manifest, &manifest_xml)?;

    let mut chain_xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ascmhldirectory xmlns=\"urn:ASC:MHL:DIRECTORY:v2.0\">\n",
    );
    let sealed = ChainLink {
        file: file_name,
        c4: c4_of(manifest_xml.as_bytes()),
    };
    for (index, link) in history.chain.iter().chain([&sealed]).enumerate() {
        chain_xml.push_str(&format!(
            "  <hashlist sequencenr=\"{}\">\n    <path>{}</path>\n    <c4>{}</c4>\n  </hashlist>\n",
            index + 1,
            escape(&link.file),
            link.c4
        ));
    }
    chain_xml.push_str("</ascmhldirectory>\n");
    write_whole(&dir.join(CHAIN_FILE), &chain_xml)?;

    Ok(Some(Generation {
        manifest,
        number,
        original,
        verified,
        failed,
    }))
}

/// Records what a job proved on one destination: every file whose copy there
/// was read back and matched. `history_root` is the folder the history
/// belongs to, relative to the destination (empty for the destination itself).
#[allow(clippy::too_many_arguments)]
pub fn record_destination(
    dest_root: &Path,
    history_root: &str,
    files: &[FileEntry],
    results: &[FileResult],
    dest_index: usize,
    algo: HashAlgo,
    creator: &Creator,
    ignore: &[String],
) -> Result<Option<Generation>> {
    let prefix = if history_root.is_empty() { String::new() } else { format!("{history_root}/") };
    let entries: Vec<Entry> = files
        .iter()
        .zip(results)
        .filter_map(|(file, result)| {
            if !result.dests.get(dest_index)?.verified {
                return None;
            }
            Some(Entry {
                path: file.dest_rel().strip_prefix(&prefix)?,
                algo,
                hash: result.hash.as_deref()?,
                hashed_at: result.verified_at,
            })
        })
        .collect();
    append_generation(&path_under(dest_root, history_root), &entries, creator, ignore, Process::Transfer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn creator() -> Creator {
        Creator {
            tool: "BackupTool".into(),
            version: "0.1.0".into(),
        }
    }

    fn digest(algo: HashAlgo, data: &[u8]) -> String {
        let mut hasher = Hasher::new(algo);
        hasher.update(data);
        hasher.finalize()
    }

    /// Writes files under `root` and returns them as verified entries.
    fn put(root: &Path, algo: HashAlgo, files: &[(&'static str, &[u8])]) -> Vec<(&'static str, String)> {
        files
            .iter()
            .map(|(rel, data)| {
                let path = path_under(root, rel);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, data).unwrap();
                (*rel, digest(algo, data))
            })
            .collect()
    }

    fn append(root: &Path, algo: HashAlgo, hashed: &[(&'static str, String)]) -> Result<Option<Generation>> {
        let entries: Vec<Entry> = hashed
            .iter()
            .map(|(path, hash)| Entry { path, algo, hash, hashed_at: Some(1_794_000_000) })
            .collect();
        append_generation(root, &entries, &creator(), &["/Project".to_string()], Process::Transfer)
    }

    #[test]
    fn first_generation_lists_every_file_as_original_and_seals_the_manifest() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("Trail series");
        let hashed = put(&root, HashAlgo::Xxh64, &[("Footage/B.MOV", b"bbb"), ("Footage/A.MOV", b"aaaa")]);

        let generation = append(&root, HashAlgo::Xxh64, &hashed).unwrap().unwrap();

        assert_eq!((generation.number, generation.original, generation.verified), (1, 2, 0));
        assert!(generation.failed.is_empty());
        let name = generation.manifest.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("0001_Trail series_") && name.ends_with("Z.mhl"), "{name}");
        let xml = fs::read_to_string(&generation.manifest).unwrap();
        // Sorted by path, with size and the checksum we were given.
        let a = xml.find(">Footage/A.MOV</path>").unwrap();
        let b = xml.find(">Footage/B.MOV</path>").unwrap();
        assert!(a < b);
        assert!(xml.contains(&format!(
            "<path size=\"4\" lastmodificationdate=\"{}",
            &iso(DateTime::<Local>::from(fs::metadata(root.join("Footage/A.MOV")).unwrap().modified().unwrap()))[..10]
        )));
        assert!(xml.contains(&format!("action=\"original\" hashdate=\"{}\">{}</xxh64>",
            iso(DateTime::<Utc>::from_timestamp(1_794_000_000, 0).unwrap().with_timezone(&Local)),
            digest(HashAlgo::Xxh64, b"aaaa"))));
        assert!(xml.contains("<process>transfer</process>"));
        assert!(xml.contains("<pattern>ascmhl/</pattern>\n      <pattern>/Project</pattern>"));
        let chain = fs::read_to_string(root.join("ascmhl/ascmhl_chain.xml")).unwrap();
        assert!(chain.contains(&format!("<path>{name}</path>\n    <c4>{}</c4>", c4_of(xml.as_bytes()))));
    }

    #[test]
    fn later_generations_verify_known_files_and_add_new_ones() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("shoot");
        let card_one = put(&root, HashAlgo::Xxh64, &[("Footage/A.MOV", b"aaaa")]);
        append(&root, HashAlgo::Xxh64, &card_one).unwrap();
        let mut both = put(&root, HashAlgo::Xxh64, &[("Footage/B.MOV", b"bbb")]);
        both.extend(card_one);

        let second = append(&root, HashAlgo::Xxh64, &both).unwrap().unwrap();

        assert_eq!((second.number, second.original, second.verified), (2, 1, 1));
        let xml = fs::read_to_string(&second.manifest).unwrap();
        assert_eq!(xml.matches("action=\"verified\"").count(), 1);
        assert_eq!(xml.matches("action=\"original\"").count(), 1);
        let chain = fs::read_to_string(root.join("ascmhl/ascmhl_chain.xml")).unwrap();
        assert!(chain.contains("sequencenr=\"1\"") && chain.contains("sequencenr=\"2\""));
        // The whole history still loads with both seals intact.
        let history = load_history(&root.join("ascmhl")).unwrap();
        assert_eq!(history.chain.len(), 2);
        assert_eq!(history.known.len(), 2);
    }

    #[test]
    fn a_file_that_changed_since_it_was_recorded_is_marked_failed() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("shoot");
        let before = put(&root, HashAlgo::Md5, &[("A.MOV", b"aaaa")]);
        append(&root, HashAlgo::Md5, &before).unwrap();
        let after = put(&root, HashAlgo::Md5, &[("A.MOV", b"aaaX")]);

        let generation = append(&root, HashAlgo::Md5, &after).unwrap().unwrap();

        assert_eq!(generation.failed, ["A.MOV"]);
        assert_eq!(generation.verified, 0);
        assert!(fs::read_to_string(&generation.manifest).unwrap().contains("action=\"failed\""));
    }

    #[test]
    fn switching_checksum_type_ties_the_new_one_to_the_recorded_one() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("shoot");
        let md5 = put(&root, HashAlgo::Md5, &[("A.MOV", b"aaaa")]);
        append(&root, HashAlgo::Md5, &md5).unwrap();
        let xxh = vec![("A.MOV", digest(HashAlgo::Xxh64, b"aaaa"))];

        let generation = append(&root, HashAlgo::Xxh64, &xxh).unwrap().unwrap();

        assert_eq!((generation.verified, generation.failed.len()), (1, 0));
        let xml = fs::read_to_string(&generation.manifest).unwrap();
        // The old kind is recomputed from the file and written next to the new.
        assert!(xml.contains("<md5 action=\"verified\""));
        // Schema order: md5 before xxh64.
        assert!(xml.find("<md5 ").unwrap() < xml.find("<xxh64 ").unwrap());
        assert!(xml.contains(&format!(">{}</md5>", md5[0].1)));
        assert!(xml.contains(&format!(">{}</xxh64>", xxh[0].1)));

        // And if the file no longer matches the old checksum, both say so.
        fs::write(root.join("A.MOV"), b"aaaX").unwrap();
        let changed = vec![("A.MOV", digest(HashAlgo::Sha1, b"aaaX"))];
        let failed = append(&root, HashAlgo::Sha1, &changed).unwrap().unwrap();
        assert_eq!(failed.failed, ["A.MOV"]);
        assert_eq!(fs::read_to_string(&failed.manifest).unwrap().matches("action=\"failed\"").count(), 2);
    }

    #[test]
    fn a_tampered_manifest_stops_the_history_from_growing() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("shoot");
        let hashed = put(&root, HashAlgo::Xxh64, &[("A.MOV", b"aaaa")]);
        let first = append(&root, HashAlgo::Xxh64, &hashed).unwrap().unwrap();
        let edited = fs::read_to_string(&first.manifest).unwrap().replace("size=\"4\"", "size=\"5\"");
        fs::write(&first.manifest, edited).unwrap();

        let err = append(&root, HashAlgo::Xxh64, &hashed).unwrap_err();

        assert!(matches!(err, Error::Mhl(_)), "{err}");
        assert_eq!(fs::read_dir(root.join("ascmhl")).unwrap().count(), 2);
    }

    #[test]
    fn names_with_xml_characters_survive_a_round_trip() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("Tom & Jerry");
        let hashed = put(&root, HashAlgo::Xxh64, &[("B&W/take 1 & 2.mov", b"x")]);
        append(&root, HashAlgo::Xxh64, &hashed).unwrap();

        let again = append(&root, HashAlgo::Xxh64, &hashed).unwrap().unwrap();

        assert_eq!((again.original, again.verified), (0, 1));
    }

    #[test]
    fn sha256_is_refused_because_the_standard_does_not_define_it() {
        let dir = TempDir::new().unwrap();
        let hashed = put(dir.path(), HashAlgo::Sha256, &[("A.MOV", b"aaaa")]);
        assert!(matches!(append(dir.path(), HashAlgo::Sha256, &hashed), Err(Error::Mhl(_))));
        assert!(!dir.path().join("ascmhl").exists());
    }

    #[test]
    fn nothing_to_record_writes_nothing() {
        let dir = TempDir::new().unwrap();
        assert!(append(dir.path(), HashAlgo::Xxh64, &[]).unwrap().is_none());
        assert!(!dir.path().join("ascmhl").exists());
    }
}
