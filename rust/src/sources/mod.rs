//! Strictly read-only multi-agent local log source adapters & shared safety helpers.
//!
//! Safety invariants enforced across every adapter:
//!   - Pure-text whitelisted files only: NEVER open SQLite DBs (`*.sqlite*`, `*.db`),
//!     NEVER read credentials/OAuth tokens/cookies/config secrets, NEVER write inside agent dirs.
//!   - Every regular file access passes `SafeFs::is_safe_regular_file` (rejects symlinks and
//!     verifies canonical-root containment, caching verified paths for sub-millisecond warm polls).

pub mod antigravity;
pub mod claude_code;
pub mod codex;
pub mod gemini_cli;

use chrono::{DateTime, Utc};
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use crate::stats::{AgentFamily, SourceKind, TurnRecord};

pub const RUNNING_STEP_STALE_SECS: i64 = 180;
pub const ACTIVE_MTIME_WINDOW_SECS: i64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileSig {
    pub mtime_secs: u64,
    pub mtime_nanos: u32,
    pub len: u64,
}

impl FileSig {
    pub fn mtime_utc(self) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp(self.mtime_secs as i64, self.mtime_nanos)
    }
}

/// Prefix fingerprint of the first up to 64 bytes of a JSONL file to safely verify
/// append-only growth before seeking to `committed_offset` for incremental tailing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderFingerprint {
    pub len: u8,
    pub bytes: [u8; 64],
}

impl Default for HeaderFingerprint {
    fn default() -> Self {
        Self {
            len: 0,
            bytes: [0u8; 64],
        }
    }
}

impl HeaderFingerprint {
    pub fn matches_prefix(&self, current: &HeaderFingerprint) -> bool {
        let n = self.len as usize;
        if n == 0 || (current.len as usize) < n {
            return false;
        }
        self.bytes[..n] == current.bytes[..n]
    }
}

/// Reads up to the first 64 bytes from `fh` (seeking to offset 0 first).
pub fn read_header_fingerprint(fh: &mut File) -> Option<HeaderFingerprint> {
    fh.seek(SeekFrom::Start(0)).ok()?;
    let mut fp = HeaderFingerprint::default();
    let mut filled = 0usize;
    while filled < fp.bytes.len() {
        match fh.read(&mut fp.bytes[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    fp.len = filled as u8;
    Some(fp)
}

/// Verifies that `fh` still starts with `prev_hdr` and did not shrink below `committed_offset`.
/// On success, seeks `fh` to `committed_offset` and returns `Some(fresh_header)`; otherwise
/// rewinds `fh` to byte 0 and returns `None` so caller falls back to full re-parse.
pub fn try_seek_for_tail(
    fh: &mut File,
    prev_sig: FileSig,
    new_sig: FileSig,
    prev_hdr: HeaderFingerprint,
    committed_offset: u64,
) -> Option<HeaderFingerprint> {
    if committed_offset == 0 || new_sig.len < committed_offset || new_sig.len < prev_sig.len {
        let _ = fh.seek(SeekFrom::Start(0));
        return None;
    }
    let fresh_hdr = read_header_fingerprint(fh)?;
    if prev_hdr.matches_prefix(&fresh_hdr) && fh.seek(SeekFrom::Start(committed_offset)).is_ok() {
        Some(fresh_hdr)
    } else {
        let _ = fh.seek(SeekFrom::Start(0));
        None
    }
}

fn file_sig_from_meta(meta: &fs::Metadata) -> Option<FileSig> {
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(FileSig {
        mtime_secs: mtime.as_secs(),
        mtime_nanos: mtime.subsec_nanos(),
        len: meta.len(),
    })
}

#[cfg(windows)]
enum FastStat {
    Found(u32, FileSig),
    Missing,
    Fallback,
}

#[cfg(windows)]
fn fast_stat_windows(path: &Path) -> FastStat {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, WIN32_ERROR};
    use windows::Win32::Storage::FileSystem::{
        GetFileAttributesExW, GetFileExInfoStandard, FILE_ATTRIBUTE_REPARSE_POINT,
        WIN32_FILE_ATTRIBUTE_DATA,
    };

    // Stack-encode common paths (< 320 UTF-16 units) with zero heap allocations.
    let mut stack_buf = [0u16; 320];
    let mut len = 0usize;
    let mut overflow = false;
    for unit in path.as_os_str().encode_wide() {
        if len + 1 < stack_buf.len() {
            stack_buf[len] = unit;
            len += 1;
        } else {
            overflow = true;
            break;
        }
    }
    let heap_buf: Vec<u16>;
    let ptr = if !overflow {
        stack_buf[len] = 0;
        stack_buf.as_ptr()
    } else {
        heap_buf = path.as_os_str().encode_wide().chain(Some(0)).collect();
        heap_buf.as_ptr()
    };

    let mut data = WIN32_FILE_ATTRIBUTE_DATA::default();
    let res = unsafe {
        GetFileAttributesExW(
            PCWSTR(ptr),
            GetFileExInfoStandard,
            (&mut data as *mut WIN32_FILE_ATTRIBUTE_DATA).cast(),
        )
    };
    if let Err(err) = res {
        return match WIN32_ERROR::from_error(&err) {
            Some(e) if e == ERROR_FILE_NOT_FOUND || e == ERROR_PATH_NOT_FOUND => FastStat::Missing,
            _ => FastStat::Fallback,
        };
    }
    // Reparse points (symlinks, junctions, cloud placeholders) fall back to symlink_metadata
    // so symlinks are always strictly rejected.
    if (data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0) != 0 {
        return FastStat::Fallback;
    }
    let ticks = ((data.ftLastWriteTime.dwHighDateTime as u64) << 32)
        | (data.ftLastWriteTime.dwLowDateTime as u64);
    const WINDOWS_EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;
    let Some(unix_100ns) = ticks.checked_sub(WINDOWS_EPOCH_DIFF_100NS) else {
        return FastStat::Fallback;
    };
    let file_len = ((data.nFileSizeHigh as u64) << 32) | (data.nFileSizeLow as u64);
    FastStat::Found(
        data.dwFileAttributes,
        FileSig {
            mtime_secs: unix_100ns / 10_000_000,
            mtime_nanos: ((unix_100ns % 10_000_000) * 100) as u32,
            len: file_len,
        },
    )
}

/// 1-syscall non-symlink regular file stat (`GetFileAttributesExW` on Windows; `symlink_metadata` elsewhere).
pub(crate) fn stat_regular_file_fast(path: &Path) -> Option<FileSig> {
    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
        match fast_stat_windows(path) {
            FastStat::Found(attrs, sig) => {
                if (attrs & FILE_ATTRIBUTE_DIRECTORY.0) != 0 {
                    return None;
                }
                return Some(sig);
            }
            FastStat::Missing => return None,
            FastStat::Fallback => {}
        }
    }
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    file_sig_from_meta(&meta)
}

/// 1-syscall non-symlink directory stat (`GetFileAttributesExW` on Windows; `symlink_metadata` elsewhere).
pub(crate) fn stat_dir_fast(path: &Path) -> Option<FileSig> {
    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
        match fast_stat_windows(path) {
            FastStat::Found(attrs, sig) => {
                if (attrs & FILE_ATTRIBUTE_DIRECTORY.0) == 0 {
                    return None;
                }
                return Some(sig);
            }
            FastStat::Missing => return None,
            FastStat::Fallback => {}
        }
    }
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return None;
    }
    file_sig_from_meta(&meta)
}

/// Strictly read-only filesystem guard bound to a single source root directory.
/// Rejects symlinks and verifies canonical containment inside `root` (caching verified paths & dirs).
#[derive(Debug, Clone)]
pub struct SafeFs {
    root: PathBuf,
    canon_root: Option<PathBuf>,
    verified_paths: HashSet<PathBuf>,
    verified_dirs: HashSet<PathBuf>,
}

impl SafeFs {
    pub fn new(root: PathBuf) -> Self {
        let canon_root = if non_symlink_dir(&root) {
            fs::canonicalize(&root).ok()
        } else {
            None
        };
        Self {
            root,
            canon_root,
            verified_paths: HashSet::new(),
            verified_dirs: HashSet::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Re-resolves canonical root if it did not exist at construction time.
    pub fn refresh_root(&mut self) {
        if self.canon_root.is_none() && non_symlink_dir(&self.root) {
            self.canon_root = fs::canonicalize(&self.root).ok();
        }
    }

    fn verify_canonical_inside_root(&mut self, path: &Path) -> bool {
        if self.canon_root.is_none() && non_symlink_dir(&self.root) {
            self.canon_root = fs::canonicalize(&self.root).ok();
        }
        let Some(root) = self.canon_root.as_ref() else {
            return false;
        };
        fs::canonicalize(path)
            .map(|c| c.starts_with(root))
            .unwrap_or(false)
    }

    /// Checks whether `path` is a non-symlink directory contained within `root`.
    /// Uses a 1-syscall fast stat on Windows and caches canonical containment so
    /// `fs::canonicalize` is never repeated on warm polls.
    pub fn is_safe_dir(&mut self, path: &Path) -> bool {
        if stat_dir_fast(path).is_none() {
            self.verified_dirs.remove(path);
            return false;
        }
        if self.verified_dirs.contains(path) {
            return true;
        }
        let ok = self.verify_canonical_inside_root(path);
        if ok {
            self.verified_dirs.insert(path.to_path_buf());
        }
        ok
    }

    /// Returns `(mtime, len)` for a verified non-symlink directory inside `root`.
    pub fn safe_dir_sig(&mut self, path: &Path) -> Option<FileSig> {
        let Some(sig) = stat_dir_fast(path) else {
            self.verified_dirs.remove(path);
            return None;
        };
        if !self.verified_dirs.contains(path) {
            if !self.verify_canonical_inside_root(path) {
                return None;
            }
            self.verified_dirs.insert(path.to_path_buf());
        }
        Some(sig)
    }

    /// Returns `true` if `path` has already passed canonical containment verification.
    #[inline]
    pub fn is_verified_file(&self, path: &Path) -> bool {
        self.verified_paths.contains(path)
    }

    /// 1-syscall fast stat for a path ALREADY in `verified_paths` (zero `fs::canonicalize()`).
    /// Returns `None` if `path` is not yet verified, missing, or became a symlink/non-file.
    pub fn stat_verified_file(&mut self, path: &Path) -> Option<FileSig> {
        if !self.verified_paths.contains(path) {
            return None;
        }
        let Some(sig) = stat_regular_file_fast(path) else {
            self.verified_paths.remove(path);
            return None;
        };
        Some(sig)
    }

    /// Zero-extra-syscall stat for an already-verified file discovered via `fs::read_dir` on Windows
    /// (uses `DirEntry::file_type` + `DirEntry::metadata` from `WIN32_FIND_DATAW`), falling back to
    /// full `is_safe_regular_file_checked` verification for newly discovered paths.
    /// Returns `(FileSig, freshly_canonicalized)`.
    pub fn stat_verified_dir_entry_checked(
        &mut self,
        path: &Path,
        entry: &fs::DirEntry,
    ) -> Option<(FileSig, bool)> {
        if self.verified_paths.contains(path) {
            if let Ok(ft) = entry.file_type() {
                if ft.is_symlink() || !ft.is_file() {
                    self.verified_paths.remove(path);
                    return None;
                }
                if let Ok(meta) = entry.metadata() {
                    if !meta.file_type().is_symlink() && meta.is_file() {
                        if let Some(sig) = file_sig_from_meta(&meta) {
                            return Some((sig, false));
                        }
                    } else {
                        self.verified_paths.remove(path);
                        return None;
                    }
                }
            }
            return self.stat_verified_file(path).map(|s| (s, false));
        }
        self.is_safe_regular_file_checked(path)
    }

    pub fn stat_verified_dir_entry(&mut self, path: &Path, entry: &fs::DirEntry) -> Option<FileSig> {
        self.stat_verified_dir_entry_checked(path, entry).map(|(s, _)| s)
    }

    /// Verifies `path` is a non-symlink regular file strictly inside `root` and returns
    /// `(FileSig, freshly_canonicalized)`. On cold discovery `freshly_canonicalized` is `true`
    /// so `open_for_read` can skip a redundant second `fs::canonicalize()` 10ns later.
    pub fn is_safe_regular_file_checked(&mut self, path: &Path) -> Option<(FileSig, bool)> {
        let Some(sig) = stat_regular_file_fast(path) else {
            self.verified_paths.remove(path);
            return None;
        };
        if !self.verified_paths.contains(path) {
            if !self.verify_canonical_inside_root(path) {
                return None;
            }
            self.verified_paths.insert(path.to_path_buf());
            return Some((sig, true));
        }
        Some((sig, false))
    }

    /// Verifies `path` is a non-symlink regular file strictly inside `root` and returns its `FileSig`.
    /// For already-verified paths, skips `fs::canonicalize` (1-syscall stat); new paths run full canonical check.
    pub fn is_safe_regular_file(&mut self, path: &Path) -> Option<FileSig> {
        self.is_safe_regular_file_checked(path).map(|(sig, _)| sig)
    }

    /// Opens a verified non-symlink regular file inside `root` strictly read-only.
    /// Avoids double `fs::canonicalize()` when `is_safe_regular_file_checked` just canonicalized it.
    pub fn open_safe_regular_file(&mut self, path: &Path) -> Option<(FileSig, File)> {
        let (sig, freshly_canon) = self.is_safe_regular_file_checked(path)?;
        let fh = self.open_for_read(path, freshly_canon)?;
        Some((sig, fh))
    }

    /// Opens a verified regular file strictly read-only. If `freshly_canonicalized` is `false`
    /// (warm cached path that was stat'd without `canonicalize`), re-verifies symlink status and
    /// canonical root containment immediately before `File::open`.
    pub fn open_for_read(&mut self, path: &Path, freshly_canonicalized: bool) -> Option<File> {
        if !self.verified_paths.contains(path) {
            return None;
        }
        if !freshly_canonicalized {
            if stat_regular_file_fast(path).is_none() || !self.verify_canonical_inside_root(path) {
                self.verified_paths.remove(path);
                return None;
            }
        }
        File::open(path).ok()
    }

    /// Opens a verified regular file strictly read-only, re-checking symlink status and canonical
    /// containment inside `root` immediately before every actual `File::open` call.
    pub fn open_already_verified(&mut self, path: &Path) -> Option<File> {
        self.open_for_read(path, false)
    }

    /// Reads a verified regular file up to `max_bytes`, avoiding duplicate canonicalize on cold discovery.
    pub fn read_for_read_limited(
        &mut self,
        path: &Path,
        sig: FileSig,
        max_bytes: usize,
        freshly_canonicalized: bool,
    ) -> Option<String> {
        let fh = self.open_for_read(path, freshly_canonicalized)?;
        let mut buf = Vec::with_capacity((sig.len as usize).min(max_bytes));
        fh.take(max_bytes as u64).read_to_end(&mut buf).ok()?;
        Some(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Reads a verified regular file up to `max_bytes`, enforcing canonical containment before opening.
    pub fn read_already_verified_limited(
        &mut self,
        path: &Path,
        sig: FileSig,
        max_bytes: usize,
    ) -> Option<String> {
        self.read_for_read_limited(path, sig, max_bytes, false)
    }

    /// Reads a verified regular file strictly read-only up to `max_bytes`.
    pub fn read_to_string_limited(
        &mut self,
        path: &Path,
        max_bytes: usize,
    ) -> Option<(FileSig, String)> {
        let (sig, fh) = self.open_safe_regular_file(path)?;
        let mut buf = Vec::with_capacity((sig.len as usize).min(max_bytes));
        fh.take(max_bytes as u64).read_to_end(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf).into_owned();
        Some((sig, text))
    }

    /// Evicts cached verified file paths that are no longer observed on disk.
    pub fn retain_verified(&mut self, seen: &HashSet<PathBuf>) {
        self.verified_paths.retain(|p| seen.contains(p));
    }

    /// Evicts cached verified directory paths that are no longer observed on disk.
    pub fn retain_verified_dirs(&mut self, seen: &HashSet<PathBuf>) {
        self.verified_dirs.retain(|p| seen.contains(p));
    }

    /// Clears all verified caches (e.g. when source root disappears).
    pub fn clear_verified(&mut self) {
        self.verified_paths.clear();
        self.verified_dirs.clear();
    }
}

/// Unpriced session produced by a `SourceAdapter`. Pricing, cross-session subagent linking,
/// archive sync/backfill, and aggregation are handled centrally in `stats::IncrementalScanner`.
#[derive(Debug, Clone)]
pub struct ParsedSession {
    pub conversation_id: String,
    pub title: String,
    pub workspace: String,
    pub source: SourceKind,
    pub is_subagent: bool,
    pub parent_id: Option<String>,
    pub parent_title: String,
    pub turns: Arc<[TurnRecord]>,
    pub last_file_mtime_utc: Option<DateTime<Utc>>,
    pub last_step_running: bool,
    pub is_running_hint: bool,
}

#[derive(Debug, Default)]
pub struct SourceScanResult {
    pub sessions: Vec<ParsedSession>,
    pub skipped_empty: usize,
    pub skipped_lines: usize,
    pub files_cached: usize,
    pub files_reparsed: usize,
    pub found_root: bool,
}

/// Result of polling a `SourceAdapter` for incremental changes.
pub enum AdapterPoll {
    /// Zero files reparsed, zero files added/removed, zero metadata/annotation changes.
    Unchanged {
        found_root: bool,
        files_cached: usize,
        skipped_empty: usize,
        skipped_lines: usize,
    },
    /// Targeted O(1) update for specific hinted sessions under this adapter; all other cached
    /// sessions in `IncrementalScanner` remain untouched.
    Delta {
        updated_sessions: Vec<ParsedSession>,
        removed_cids: Vec<String>,
        files_cached: usize,
        files_reparsed: usize,
        skipped_empty: usize,
        skipped_lines: usize,
    },
    /// At least one file or metadata entry changed (or first poll).
    Changed(SourceScanResult),
}

/// Adapter interface for a single local AI coding agent log root.
/// Implementations own their per-file `(mtime, size)` incremental cache and tail offsets.
pub trait SourceAdapter {
    fn kind(&self) -> SourceKind;
    fn root(&self) -> &Path;
    fn scan(&mut self, scan_now_utc: DateTime<Utc>) -> SourceScanResult;

    /// Fast change check that skips constructing `Vec<ParsedSession>` when nothing on disk changed.
    fn poll_changes(&mut self, scan_now_utc: DateTime<Utc>) -> AdapterPoll {
        AdapterPoll::Changed(self.scan(scan_now_utc))
    }

    /// Targeted fast refresh for specific changed file paths reported by `ReadDirectoryChangesW`.
    /// Returns `Some(poll)` when all hinted paths were handled accurately, or `None` to fall back
    /// cleanly to a full `poll_changes()` sweep.
    fn poll_hinted(&mut self, _paths: &[PathBuf], _scan_now_utc: DateTime<Utc>) -> Option<AdapterPoll> {
        None
    }

    /// Replaces the adapter's cached unpriced `Arc<[TurnRecord]>` with the scanner's priced
    /// `Arc<[TurnRecord]>` so adapter cache, scanner, and snapshot share ONE single allocation
    /// (eliminating dual turn-array retention).
    fn adopt_priced_turns(
        &mut self,
        _cid: &str,
        _old_turns: &Arc<[TurnRecord]>,
        _priced_turns: &Arc<[TurnRecord]>,
    ) {
    }

    /// Optional fallback metadata lookup `(title, workspace)` for a parent session ID that
    /// had zero parsed turns (e.g. Antigravity annotations/metadata cache or index files).
    fn lookup_session_hint(&mut self, _cid: &str) -> Option<(String, String)> {
        None
    }

    /// Whitelisted subdirectories safe to watch with `ReadDirectoryChangesW`
    /// (avoids false wakeups from SQLite WAL/SHM or debug directories).
    fn watched_dirs(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

/// Case-insensitive (on Windows) path equality helper for matching watcher hints.
pub(crate) fn path_eq_platform(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        let sa = a.to_string_lossy();
        let sb = b.to_string_lossy();
        let ia = sa.bytes().map(|c| if c == b'/' { b'\\' } else { c.to_ascii_lowercase() });
        let ib = sb.bytes().map(|c| if c == b'/' { b'\\' } else { c.to_ascii_lowercase() });
        ia.eq(ib)
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

/// Strips `root` prefix from `path` (case- and slash-insensitive on Windows), returning the
/// relative component slice strings if `path` lies strictly inside `root`.
pub(crate) fn relative_components_under(root: &Path, path: &Path) -> Option<Vec<String>> {
    let root_comps: Vec<String> = root
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let path_comps: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if path_comps.len() <= root_comps.len() {
        return None;
    }
    for (rc, pc) in root_comps.iter().zip(path_comps.iter()) {
        let same = if cfg!(windows) {
            rc.eq_ignore_ascii_case(pc)
        } else {
            rc == pc
        };
        if !same {
            return None;
        }
    }
    Some(path_comps[root_comps.len()..].to_vec())
}

/// Filter selecting which agent families `discover_default_sources` scans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceDiscoveryFilter {
    All,
    AgyOnly,
    Families(Vec<AgentFamily>),
}

impl SourceDiscoveryFilter {
    pub fn allows(&self, family: AgentFamily) -> bool {
        match self {
            SourceDiscoveryFilter::All => true,
            SourceDiscoveryFilter::AgyOnly => family == AgentFamily::Antigravity,
            SourceDiscoveryFilter::Families(list) => list.contains(&family),
        }
    }

    /// Parses a comma-separated CLI `--agents` spec (`all`, `agy`, `claude`, `codex`, `gemini`).
    pub fn parse_csv(spec: &str) -> Option<Self> {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return None;
        }
        if trimmed.eq_ignore_ascii_case("all") {
            return Some(SourceDiscoveryFilter::All);
        }
        let mut families = Vec::new();
        for part in trimmed.split(',') {
            if let Some(f) = AgentFamily::from_str_loose(part) {
                if !families.contains(&f) {
                    families.push(f);
                }
            }
        }
        if families.is_empty() {
            None
        } else {
            Some(SourceDiscoveryFilter::Families(families))
        }
    }
}

fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

pub fn default_antigravity_cli_dir() -> PathBuf {
    match user_home_dir() {
        Some(home) => home.join(".gemini").join("antigravity-cli"),
        None => PathBuf::from(".gemini").join("antigravity-cli"),
    }
}

fn push_unique_source(out: &mut Vec<(SourceKind, PathBuf)>, kind: SourceKind, path: PathBuf) {
    let key = path.to_string_lossy().to_ascii_lowercase();
    if !out
        .iter()
        .any(|(k, p)| *k == kind && p.to_string_lossy().to_ascii_lowercase() == key)
    {
        out.push((kind, path));
    }
}

pub(crate) fn non_symlink_dir(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| !m.file_type().is_symlink() && m.is_dir())
        .unwrap_or(false)
}

/// Discovers installed local Tier-1 pure-text agent roots (strictly read-only directory checks).
pub fn discover_default_sources(filter: &SourceDiscoveryFilter) -> Vec<(SourceKind, PathBuf)> {
    let mut out = Vec::new();
    let home = user_home_dir();

    // 1. Antigravity family (CLI / 2.0 App / IDE under ~/.gemini/antigravity*)
    if filter.allows(AgentFamily::Antigravity) {
        let gemini_base = home
            .as_ref()
            .map(|h| h.join(".gemini"))
            .unwrap_or_else(|| PathBuf::from(".gemini"));
        let agy_candidates = [
            (SourceKind::Cli, gemini_base.join("antigravity-cli")),
            (SourceKind::App, gemini_base.join("antigravity")),
            (SourceKind::Ide, gemini_base.join("antigravity-ide")),
        ];
        for (kind, dir) in agy_candidates {
            if non_symlink_dir(&dir) && non_symlink_dir(&dir.join("brain")) {
                push_unique_source(&mut out, kind, dir);
            }
        }
    }

    // 2. Claude Code ($CLAUDE_CONFIG_DIR comma-separated, else ~/.claude and ~/.config/claude)
    if filter.allows(AgentFamily::ClaudeCode) {
        let mut candidates = Vec::new();
        if let Ok(raw) = std::env::var("CLAUDE_CONFIG_DIR") {
            for part in raw.split(',') {
                let t = part.trim();
                if !t.is_empty() {
                    candidates.push(PathBuf::from(t));
                }
            }
        }
        if candidates.is_empty() {
            if let Some(h) = &home {
                candidates.push(h.join(".claude"));
                candidates.push(h.join(".config").join("claude"));
            }
        }
        for dir in candidates {
            if non_symlink_dir(&dir) && non_symlink_dir(&dir.join("projects")) {
                push_unique_source(&mut out, SourceKind::ClaudeCode, dir);
            }
        }
    }

    // 3. OpenAI Codex CLI ($CODEX_HOME, else ~/.codex; pure-text rollouts only, never SQLite)
    if filter.allows(AgentFamily::Codex) {
        let codex_root = std::env::var_os("CODEX_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".codex")));
        if let Some(dir) = codex_root {
            if non_symlink_dir(&dir)
                && (non_symlink_dir(&dir.join("sessions"))
                    || non_symlink_dir(&dir.join("archived_sessions")))
            {
                push_unique_source(&mut out, SourceKind::Codex, dir);
            }
        }
    }

    // 4. Gemini CLI ($GEMINI_CLI_HOME checking <root>/tmp and <root>/.gemini/tmp, else ~/.gemini)
    if filter.allows(AgentFamily::GeminiCli) {
        let mut candidates = Vec::new();
        if let Some(raw) = std::env::var_os("GEMINI_CLI_HOME").filter(|s| !s.is_empty()) {
            let root = PathBuf::from(raw);
            if non_symlink_dir(&root) && non_symlink_dir(&root.join("tmp")) {
                candidates.push(root);
            } else if non_symlink_dir(&root)
                && non_symlink_dir(&root.join(".gemini"))
                && non_symlink_dir(&root.join(".gemini").join("tmp"))
            {
                candidates.push(root.join(".gemini"));
            }
        } else if let Some(h) = &home {
            let root = h.join(".gemini");
            if non_symlink_dir(&root) && non_symlink_dir(&root.join("tmp")) {
                candidates.push(root);
            }
        }
        for dir in candidates {
            push_unique_source(&mut out, SourceKind::GeminiCli, dir);
        }
    }

    // Preserve legacy empty-host fallback when Antigravity is allowed and no live roots exist yet.
    if out.is_empty() && filter.allows(AgentFamily::Antigravity) {
        out.push((SourceKind::Cli, default_antigravity_cli_dir()));
    }

    out
}

/// Discovers only Antigravity family directories (used by `agy-statusline.exe` for <10ms cold runs).
pub fn discover_antigravity_sources() -> Vec<(SourceKind, PathBuf)> {
    discover_default_sources(&SourceDiscoveryFilter::AgyOnly)
}

/// Instantiates the matching strictly read-only `SourceAdapter` for `(kind, root)`.
pub fn build_adapter(kind: SourceKind, root: PathBuf) -> Box<dyn SourceAdapter> {
    match kind {
        SourceKind::Cli | SourceKind::App | SourceKind::Ide | SourceKind::Custom => {
            Box::new(antigravity::AntigravityAdapter::new(kind, root))
        }
        SourceKind::ClaudeCode => Box::new(claude_code::ClaudeCodeAdapter::new(root)),
        SourceKind::Codex => Box::new(codex::CodexAdapter::new(root)),
        SourceKind::GeminiCli => Box::new(gemini_cli::GeminiCliAdapter::new(root)),
    }
}

// ---------- Zero-DOM Serde Deserialization Primitives ----------

/// Lossy `u64` deserializer matching `val_to_u64` semantics without allocating a `Value` DOM.
/// Accepts integers, floats, numeric strings, and gracefully ignores null/unexpected JSON types.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FlexU64(pub u64);

impl<'de> Deserialize<'de> for FlexU64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FlexU64Visitor;
        impl<'de> Visitor<'de> for FlexU64Visitor {
            type Value = FlexU64;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("non-negative number or numeric string")
            }
            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
                Ok(FlexU64(v))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
                Ok(FlexU64(if v > 0 { v as u64 } else { 0 }))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
                Ok(FlexU64(if v.is_finite() && v > 0.0 { v as u64 } else { 0 }))
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                Ok(FlexU64(v.trim().parse::<u64>().unwrap_or(0)))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(FlexU64(0))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(FlexU64(0))
            }
            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(FlexU64(0))
            }
            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
                FlexU64::deserialize(d)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(FlexU64(0))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(FlexU64(0))
            }
        }
        deserializer.deserialize_any(FlexU64Visitor)
    }
}

/// Optional `u64` distinguishing `null` from a present non-null value (matches `map.get(k).map(|v| !v.is_null())`).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct OptFlexU64(pub Option<u64>);

impl<'de> Deserialize<'de> for OptFlexU64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OptFlexU64Visitor;
        impl<'de> Visitor<'de> for OptFlexU64Visitor {
            type Value = OptFlexU64;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("optional non-negative number")
            }
            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
                Ok(OptFlexU64(Some(v)))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
                Ok(OptFlexU64(Some(if v > 0 { v as u64 } else { 0 })))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
                Ok(OptFlexU64(Some(if v.is_finite() && v > 0.0 {
                    v as u64
                } else {
                    0
                })))
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                Ok(OptFlexU64(Some(v.trim().parse::<u64>().unwrap_or(0))))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(OptFlexU64(Some(0)))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(OptFlexU64(None))
            }
            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(OptFlexU64(None))
            }
            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
                OptFlexU64::deserialize(d)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(OptFlexU64(Some(0)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(OptFlexU64(Some(0)))
            }
        }
        deserializer.deserialize_any(OptFlexU64Visitor)
    }
}

/// Lossy `i64` deserializer ignoring null/unexpected types.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct OptFlexI64(pub Option<i64>);

impl<'de> Deserialize<'de> for OptFlexI64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptFlexI64;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("optional i64")
            }
            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
                Ok(OptFlexI64(Some(v)))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
                Ok(OptFlexI64(i64::try_from(v).ok()))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(OptFlexI64(None))
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(OptFlexI64(None))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(OptFlexI64(None))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(OptFlexI64(None))
            }
            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(OptFlexI64(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(OptFlexI64(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(OptFlexI64(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Lossy `bool` deserializer ignoring non-bool values.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct OptFlexBool(pub Option<bool>);

impl<'de> Deserialize<'de> for OptFlexBool {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptFlexBool;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("optional bool")
            }
            fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
                Ok(OptFlexBool(Some(v)))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(OptFlexBool(None))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(OptFlexBool(None))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(OptFlexBool(None))
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(OptFlexBool(None))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(OptFlexBool(None))
            }
            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(OptFlexBool(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(OptFlexBool(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(OptFlexBool(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Lossy `String` deserializer returning `None` for non-string JSON values without failing.
#[derive(Debug, Clone, Default)]
pub(crate) struct OptFlexString(pub Option<String>);

impl<'de> Deserialize<'de> for OptFlexString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptFlexString;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("optional string")
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                Ok(OptFlexString(Some(v.to_string())))
            }
            fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
                Ok(OptFlexString(Some(v)))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(OptFlexString(None))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(OptFlexString(None))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(OptFlexString(None))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(OptFlexString(None))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(OptFlexString(None))
            }
            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(OptFlexString(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(OptFlexString(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(OptFlexString(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Zero-allocation character counter for large string fields like `thinking`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CharCount(pub u64);

impl<'de> Deserialize<'de> for CharCount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = CharCount;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("string to count characters")
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                Ok(CharCount(v.chars().count() as u64))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(CharCount(0))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(CharCount(0))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(CharCount(0))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(CharCount(0))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(CharCount(0))
            }
            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(CharCount(0))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(CharCount(0))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(CharCount(0))
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Lossy UTC timestamp deserializer accepting RFC3339 strings or Unix s/ms numbers without DOM.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FlexTimestamp(pub Option<DateTime<Utc>>);

impl<'de> Deserialize<'de> for FlexTimestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = FlexTimestamp;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("RFC3339 string or Unix timestamp")
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                Ok(FlexTimestamp(parse_rfc3339_utc(v)))
            }
            fn visit_i64<E>(self, i: i64) -> Result<Self::Value, E> {
                let dt = if i.abs() >= 1_000_000_000_000 {
                    DateTime::from_timestamp_millis(i)
                } else {
                    DateTime::from_timestamp(i, 0)
                };
                Ok(FlexTimestamp(dt))
            }
            fn visit_u64<E: de::Error>(self, u: u64) -> Result<Self::Value, E> {
                match i64::try_from(u) {
                    Ok(i) => self.visit_i64(i),
                    Err(_) => Ok(FlexTimestamp(None)),
                }
            }
            fn visit_f64<E>(self, f: f64) -> Result<Self::Value, E> {
                if !f.is_finite() {
                    return Ok(FlexTimestamp(None));
                }
                let secs = if f.abs() >= 1e12 { f / 1000.0 } else { f };
                let whole = secs.floor() as i64;
                let nanos = ((secs - whole as f64) * 1e9).clamp(0.0, 999_999_999.0) as u32;
                Ok(FlexTimestamp(DateTime::from_timestamp(whole, nanos)))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(FlexTimestamp(None))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(FlexTimestamp(None))
            }
            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(FlexTimestamp(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(FlexTimestamp(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(FlexTimestamp(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Lossy interned `Arc<str>` deserializer (returns `None` on non-string or empty/whitespace-only).
pub(crate) fn deserialize_trimmed_interned<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Arc<str>>, D::Error> {
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Option<Arc<str>>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("optional string")
        }
        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
            let t = v.trim();
            if t.is_empty() {
                Ok(None)
            } else {
                Ok(Some(crate::stats::intern_str(t)))
            }
        }
        fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(None)
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok(None)
        }
    }
    deserializer.deserialize_any(V)
}

/// Wrapper for `de::DeserializeSeed` that borrows `&str` to parse into `Option<Arc<str>>`.
pub(crate) struct InternedStrSeed;
impl<'de> de::DeserializeSeed<'de> for InternedStrSeed {
    type Value = Option<Arc<str>>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserialize_trimmed_interned(deserializer)
    }
}

// ---------- Shared Parsing & Telemetry Helpers ----------

/// Computes a lightweight "active/running" hint from already-read transcript status and file mtime.
/// Never reads additional files outside the strict per-agent whitelist.
pub fn compute_running_hint(
    last_step_running: bool,
    file_mtime_utc: Option<DateTime<Utc>>,
    now_utc: DateTime<Utc>,
) -> bool {
    let Some(mtime) = file_mtime_utc else {
        return false;
    };
    let age = (now_utc - mtime).num_seconds();
    if age < -60 {
        return false;
    }
    let age = age.max(0);
    (last_step_running && age <= RUNNING_STEP_STALE_SECS) || age <= ACTIVE_MTIME_WINDOW_SECS
}

/// Collapses whitespace into single spaces and truncates to `max_chars` with early-exit so
/// multi-kilobyte prompts never allocate more than `max_chars` characters.
pub fn clean_single_line(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(text.len().min(max_chars.saturating_mul(3) + 4));
    let mut prev_space = true;
    let mut char_count = 0usize;
    let mut truncated = false;
    let mut pending_space = false;

    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                pending_space = true;
                prev_space = true;
            }
        } else {
            if pending_space {
                if char_count >= max_chars {
                    truncated = true;
                    break;
                }
                out.push(' ');
                char_count += 1;
                pending_space = false;
            }
            if char_count >= max_chars {
                truncated = true;
                break;
            }
            out.push(ch);
            char_count += 1;
            prev_space = false;
        }
    }

    if truncated {
        let keep = max_chars.saturating_sub(1);
        let mut cut: String = out.chars().take(keep).collect();
        cut.push('…');
        cut
    } else {
        out
    }
}

pub fn val_to_u64(val: Option<&Value>) -> u64 {
    match val {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_f64().map(|f| f.max(0.0) as u64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<u64>().unwrap_or(0),
        _ => 0,
    }
}

pub fn parse_rfc3339_utc(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Parses an RFC3339 string or numeric Unix timestamp (seconds or milliseconds) into `DateTime<Utc>`.
pub fn parse_timestamp_utc(val: Option<&Value>) -> Option<DateTime<Utc>> {
    match val? {
        Value::String(s) => parse_rfc3339_utc(s),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if i.abs() >= 1_000_000_000_000 {
                    DateTime::from_timestamp_millis(i)
                } else {
                    DateTime::from_timestamp(i, 0)
                }
            } else if let Some(f) = n.as_f64().filter(|v| v.is_finite()) {
                let secs = if f.abs() >= 1e12 { f / 1000.0 } else { f };
                let whole = secs.floor() as i64;
                let nanos = ((secs - whole as f64) * 1e9).clamp(0.0, 999_999_999.0) as u32;
                DateTime::from_timestamp(whole, nanos)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn is_uuid_like(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, &ch) in b.iter().enumerate() {
        if matches!(i, 8 | 13 | 18 | 23) {
            if ch != b'-' {
                return false;
            }
        } else if !ch.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

pub fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn decode_file_uri(uri: &str) -> String {
    let raw = if let Some(rest) = uri.strip_prefix("file:///") {
        rest
    } else if let Some(rest) = uri.strip_prefix("file://") {
        rest
    } else {
        return uri.to_string();
    };
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h1), Some(h2)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                decoded.push((h1 << 4) | h2);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_safe_fs_rejects_outside_paths_and_reads_inside_files() {
        let base = std::env::temp_dir().join(format!("agy_gauge_safefs_{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("agy_gauge_outside_{}.txt", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let inside = base.join("ok.txt");
        fs::write(&inside, "hello").unwrap();
        fs::write(&outside, "secret").unwrap();

        let mut sfs = SafeFs::new(base.clone());
        let (sig, text) = sfs.read_to_string_limited(&inside, 64).expect("inside read");
        assert_eq!(text, "hello");
        assert_eq!(sig.len, 5);
        assert!(sfs.is_safe_regular_file(&outside).is_none());
        assert!(sfs.stat_verified_file(&outside).is_none());
        // Verify fast 1-syscall stat_verified_file matches std symlink_metadata bit-for-bit!
        let fast_sig = sfs.stat_verified_file(&inside).expect("verified fast stat");
        let std_meta = fs::symlink_metadata(&inside).unwrap();
        let std_sig = file_sig_from_meta(&std_meta).unwrap();
        assert_eq!(fast_sig, std_sig);
        assert!(sfs.is_safe_dir(&base));
        // Second call hits verified_dirs cache cleanly
        assert!(sfs.is_safe_dir(&base));

        // Eviction when file disappears
        fs::remove_file(&inside).unwrap();
        assert!(sfs.stat_verified_file(&inside).is_none());
        assert!(!sfs.is_verified_file(&inside));

        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn test_discovery_filter_parse_csv_and_timestamp_helpers() {
        assert_eq!(
            SourceDiscoveryFilter::parse_csv("all"),
            Some(SourceDiscoveryFilter::All)
        );
        let f = SourceDiscoveryFilter::parse_csv("claude, codex, claude-code").unwrap();
        assert!(f.allows(AgentFamily::ClaudeCode));
        assert!(f.allows(AgentFamily::Codex));
        assert!(!f.allows(AgentFamily::Antigravity));
        assert!(!f.allows(AgentFamily::GeminiCli));
        assert!(SourceDiscoveryFilter::parse_csv("unknown-agent").is_none());

        let t_iso = parse_timestamp_utc(Some(&json!("2026-11-24T10:00:00Z"))).unwrap();
        let t_ms = parse_timestamp_utc(Some(&json!(t_iso.timestamp_millis()))).unwrap();
        assert_eq!(t_iso, t_ms);
        assert!(is_uuid_like("8c1095e2-1558-4a06-bfde-446204f978aa"));
        assert!(!is_uuid_like("not-a-uuid"));
        assert_eq!(clean_single_line("  hello\n\t world  ", 20), "hello world");
        assert_eq!(clean_single_line("abcdef", 4), "abc…");
        assert_eq!(clean_single_line("abc   ", 3), "abc");
    }
}
