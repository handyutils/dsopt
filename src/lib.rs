//! Core scanning, sizing, caching, and removal logic for `dsopt`.
//!
//! The scanner walks a filesystem root once with a rayon thread pool and reports
//! directories whose name matches a known build-artifact detector (`node_modules`
//! or `target`). Nested detectors are folded into their outermost parent so a
//! single removal reclaims the whole subtree.

pub mod app;
pub mod update;

use parallel_disk_usage::{
    data_tree::DataTree,
    os_string_display::OsStringDisplay,
    size::Bytes,
    tree_builder::{Info, TreeBuilder},
};
use rayon::ThreadPoolBuilder;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

/// Paths that are never descended into: virtual filesystems and macOS volume
/// aliases that would otherwise double-count or hang the walk.
const SKIPPED_PREFIXES: [&str; 9] = [
    "/dev",
    "/proc",
    "/sys",
    "/System/Volumes/Data",
    "/System/Volumes/VM",
    "/System/Volumes/Preboot",
    "/System/Volumes/Update",
    "/private/var/vm",
    "/private/var/db/dyld",
];

/// Directories that must never be offered for removal, even if a detector matches.
const PROTECTED_ROOTS: [&str; 4] = ["/", "/System", "/Users", "/Applications"];

/// Thread counts outside this range are rejected, matching the CLI contract.
pub const THREAD_RANGE: std::ops::RangeInclusive<usize> = 1..=8;

/// A directory shape that `dsopt` knows how to reclaim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    NodeModules,
    RustTarget,
}

impl CandidateKind {
    /// Human label shown in the TUI table.
    pub fn label(self) -> &'static str {
        match self {
            Self::NodeModules => "JavaScript dependencies",
            Self::RustTarget => "Rust build output",
        }
    }
}

/// A reclaimable directory together with the size of its whole subtree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub kind: CandidateKind,
    pub path: PathBuf,
    pub size_bytes: u64,
}

/// Identity of a directory on its filesystem, used to collapse macOS volume aliases.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

/// Outcome of a completed scan.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScanResult {
    pub candidates: Vec<Candidate>,
    pub files_scanned: u64,
    pub directories_scanned: u64,
    pub bytes_scanned: u64,
}

/// Live counters shared between the walker threads and the progress reporter.
#[derive(Default)]
pub struct ScanState {
    files: AtomicU64,
    directories: AtomicU64,
    bytes: AtomicU64,
    current_path: Mutex<String>,
}

/// Point-in-time copy of [`ScanState`], safe to render.
#[derive(Clone, Debug, Serialize)]
pub struct ProgressSnapshot {
    pub current_path: String,
    pub files_scanned: u64,
    pub directories_scanned: u64,
    pub bytes_scanned: u64,
}

impl ScanState {
    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            current_path: self
                .current_path
                .lock()
                .expect("scan path mutex poisoned")
                .clone(),
            files_scanned: self.files.load(Ordering::Relaxed),
            directories_scanned: self.directories.load(Ordering::Relaxed),
            bytes_scanned: self.bytes.load(Ordering::Relaxed),
        }
    }

    fn record(&self, path: &Path, metadata: &fs::Metadata) {
        *self.current_path.lock().expect("scan path mutex poisoned") = path.display().to_string();
        self.bytes.fetch_add(metadata.len(), Ordering::Relaxed);
        if metadata.is_dir() {
            self.directories.fetch_add(1, Ordering::Relaxed);
        } else if metadata.is_file() {
            self.files.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Scan a root with a fresh [`ScanState`].
pub fn scan(root: &Path, threads: usize) -> Result<ScanResult, String> {
    scan_with_state(root, threads, &ScanState::default())
}

/// Scan a root, publishing live counters into `state` while the walk runs.
pub fn scan_with_state(
    root: &Path,
    threads: usize,
    state: &ScanState,
) -> Result<ScanResult, String> {
    if !THREAD_RANGE.contains(&threads) {
        return Err(format!(
            "threads must be between {} and {}",
            THREAD_RANGE.start(),
            THREAD_RANGE.end()
        ));
    }
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot access {}: {error}", root.display()))?;
    let pool = ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|error| error.to_string())?;
    let tree = pool.install(|| build_tree(root.clone(), state));
    let mut candidates = Vec::new();
    collect_candidates(&tree, Path::new(""), false, &mut candidates);
    let mut candidates = deduplicate_candidates(candidates);
    candidates.sort_by(|left, right| {
        right
            .size_bytes
            .cmp(&left.size_bytes)
            .then_with(|| left.path.cmp(&right.path))
    });
    let snapshot = state.snapshot();
    Ok(ScanResult {
        candidates,
        files_scanned: snapshot.files_scanned,
        directories_scanned: snapshot.directories_scanned,
        bytes_scanned: snapshot.bytes_scanned,
    })
}

fn build_tree(root: PathBuf, state: &ScanState) -> DataTree<OsStringDisplay, Bytes> {
    TreeBuilder {
        path: root.clone(),
        name: OsStringDisplay::os_string_from(root.into_os_string()),
        get_info: |path: &PathBuf| info_for_path(path, state),
        join_path: |path, name| path.join(name.as_os_str()),
        max_depth: u64::MAX,
    }
    .into()
}

fn info_for_path(path: &Path, state: &ScanState) -> Info<OsStringDisplay, Bytes> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Info::default();
    };
    state.record(path, &metadata);
    if !metadata.is_dir() || metadata.file_type().is_symlink() || should_skip(path) {
        return Info {
            size: Bytes::new(metadata.len()),
            children: Vec::new(),
        };
    }
    let children = fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| OsStringDisplay::os_string_from(entry.file_name()))
        .collect();
    Info {
        size: Bytes::new(metadata.len()),
        children,
    }
}

fn collect_candidates(
    tree: &DataTree<OsStringDisplay, Bytes>,
    path: &Path,
    inside_candidate: bool,
    candidates: &mut Vec<(Candidate, FileIdentity)>,
) {
    let current_path = if path.as_os_str().is_empty() {
        PathBuf::from(tree.name().as_os_str())
    } else {
        path.join(tree.name().as_os_str())
    };
    let kind = candidate_kind(&current_path);
    if let Some(kind) = kind.filter(|_| !inside_candidate) {
        if let Some(identity) = file_identity(&current_path) {
            candidates.push((
                Candidate {
                    kind,
                    path: current_path,
                    size_bytes: tree.size().inner(),
                },
                identity,
            ));
        }
        return;
    }
    for child in tree.children() {
        collect_candidates(
            child,
            &current_path,
            inside_candidate || kind.is_some(),
            candidates,
        );
    }
}

fn candidate_kind(path: &Path) -> Option<CandidateKind> {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("node_modules") => Some(CandidateKind::NodeModules),
        Some("target") => Some(CandidateKind::RustTarget),
        _ => None,
    }
}

/// Keep only the first candidate for each `(device, inode)` pair.
pub fn deduplicate_candidates(candidates: Vec<(Candidate, FileIdentity)>) -> Vec<Candidate> {
    let mut identities = HashSet::new();
    candidates
        .into_iter()
        .filter_map(|(candidate, identity)| identities.insert(identity).then_some(candidate))
        .collect()
}

#[cfg(unix)]
fn file_identity(path: &Path) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|metadata| FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn file_identity(_: &Path) -> Option<FileIdentity> {
    None
}

/// Whether `path` sits on a virtual filesystem or macOS volume alias.
pub fn should_skip(path: &Path) -> bool {
    let path = path.to_string_lossy();
    SKIPPED_PREFIXES
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")))
}

/// Whether `path` is a directory beneath `scan_root` that is safe to delete.
///
/// Rejects missing paths, the scan root itself, protected system directories,
/// and anything under [`SKIPPED_PREFIXES`].
pub fn is_safe_candidate(path: &Path, scan_root: &Path) -> bool {
    let (Ok(resolved), Ok(root)) = (path.canonicalize(), scan_root.canonicalize()) else {
        return false;
    };
    if !resolved.starts_with(&root) || !resolved.is_dir() || resolved == root {
        return false;
    }
    if PROTECTED_ROOTS
        .iter()
        .any(|entry| resolved == Path::new(entry))
    {
        return false;
    }
    !should_skip(&resolved)
}

/// Permanently delete a candidate directory after a safety re-check.
pub fn remove_candidate(candidate: &Candidate) -> Result<(), String> {
    if !is_safe_candidate(&candidate.path, Path::new("/")) {
        return Err("unsafe or missing path".to_owned());
    }
    fs::remove_dir_all(&candidate.path).map_err(|error| error.to_string())
}

/// Render a byte count the way the TUI and CLI both display it.
pub fn format_size(size: u64) -> String {
    let mut value = size as f64;
    for unit in ["B", "KB", "MB", "GB", "TB"] {
        if value < 1024.0 || unit == "TB" {
            return if unit == "B" {
                format!("{size} B")
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    format!("{size} B")
}

/// Serialized form of the most recent completed scan.
#[derive(Debug, Serialize, Deserialize)]
pub struct ScanCache {
    pub version: String,
    pub scanned_at: String,
    pub roots: Vec<String>,
    pub threads: usize,
    pub candidates: Vec<Candidate>,
}

/// Location of the on-disk scan cache, honouring `XDG_CACHE_HOME`.
pub fn cache_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("dsopt").join("last_scan.json")
}

pub fn load_scan_cache(path: &Path) -> Option<ScanCache> {
    let payload = fs::read_to_string(path).ok()?;
    serde_json::from_str(&payload).ok()
}

pub fn save_scan_cache(
    path: &Path,
    candidates: &[Candidate],
    roots: &[PathBuf],
    threads: usize,
) -> std::io::Result<()> {
    let cache = ScanCache {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        scanned_at: format_timestamp(unix_now()),
        roots: roots
            .iter()
            .map(|root| root.display().to_string())
            .collect(),
        threads,
        candidates: candidates.to_vec(),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(
        &temporary,
        serde_json::to_string_pretty(&cache).unwrap_or_default() + "\n",
    )?;
    fs::rename(temporary, path)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// Format Unix seconds as `YYYY-MM-DD HH:MM UTC` without pulling in a date crate.
pub fn format_timestamp(unix_seconds: u64) -> String {
    let days = (unix_seconds / 86_400) as i64;
    let seconds_of_day = unix_seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to `(y, m, d)`.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let shifted = days_since_epoch + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_byte_counts() {
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(1536), "1.5 KB");
    }

    #[test]
    fn formats_the_unix_epoch() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00 UTC");
    }

    #[test]
    fn formats_a_known_timestamp() {
        assert_eq!(format_timestamp(1_000_000_000), "2001-09-09 01:46 UTC");
    }

    #[test]
    fn skips_volume_aliases_and_virtual_filesystems() {
        assert!(should_skip(Path::new("/System/Volumes/Data")));
        assert!(should_skip(Path::new("/proc/1/fd")));
        assert!(!should_skip(Path::new("/Users/musichen/project")));
    }

    #[test]
    fn refuses_to_delete_protected_and_root_directories() {
        assert!(!is_safe_candidate(Path::new("/"), Path::new("/")));
        assert!(!is_safe_candidate(Path::new("/Users"), Path::new("/")));
        assert!(!is_safe_candidate(
            Path::new("/definitely/not/here"),
            Path::new("/")
        ));
        assert!(is_safe_candidate(Path::new("/tmp"), Path::new("/")));
    }
}
