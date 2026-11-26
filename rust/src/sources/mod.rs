//! Strictly read-only multi-agent local log source adapters & shared safety helpers.
//!
//! Safety invariants enforced across every adapter:
//!   - Pure-text whitelisted files only: NEVER open SQLite DBs (`*.sqlite*`, `*.db`),
//!     NEVER read credentials/OAuth tokens/cookies/config secrets, NEVER write inside agent dirs.
//!   - Every regular file access passes `SafeFs::is_safe_regular_file` (rejects symlinks and
//!     verifies canonical-root containment, caching verified paths for sub-10ms warm polls).

pub mod antigravity;
pub mod claude_code;
pub mod codex;
pub mod gemini_cli;

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::stats::{AgentFamily, SourceKind, TurnRecord};

pub const RUNNING_STEP_STALE_SECS: i64 = 180;
pub const ACTIVE_MTIME_WINDOW_SECS: i64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Strictly read-only filesystem guard bound to a single source root directory.
/// Rejects symlinks and verifies canonical containment inside `root` (caching verified paths).
#[derive(Debug, Clone)]
pub struct SafeFs {
    root: PathBuf,
    canon_root: Option<PathBuf>,
    verified_paths: HashSet<PathBuf>,
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

    /// Checks whether `path` is a non-symlink directory contained within `root`.
    pub fn is_safe_dir(&self, path: &Path) -> bool {
        let Ok(meta) = fs::symlink_metadata(path) else {
            return false;
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return false;
        }
        let Some(root) = &self.canon_root else {
            return false;
        };
        fs::canonicalize(path)
            .map(|c| c.starts_with(root))
            .unwrap_or(false)
    }

    /// Verifies `path` is a non-symlink regular file strictly inside `root` and returns its `FileSig`.
    pub fn is_safe_regular_file(&mut self, path: &Path) -> Option<FileSig> {
        let meta = fs::symlink_metadata(path).ok()?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            self.verified_paths.remove(path);
            return None;
        }
        if !self.verified_paths.contains(path) {
            if self.canon_root.is_none() && non_symlink_dir(&self.root) {
                self.canon_root = fs::canonicalize(&self.root).ok();
            }
            let root = self.canon_root.as_ref()?;
            let canon = fs::canonicalize(path).ok()?;
            if !canon.starts_with(root) {
                return None;
            }
            self.verified_paths.insert(path.to_path_buf());
        }
        let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        Some(FileSig {
            mtime_secs: mtime.as_secs(),
            mtime_nanos: mtime.subsec_nanos(),
            len: meta.len(),
        })
    }

    /// Opens a verified non-symlink regular file inside `root` strictly read-only.
    pub fn open_safe_regular_file(&mut self, path: &Path) -> Option<(FileSig, File)> {
        let sig = self.is_safe_regular_file(path)?;
        let fh = File::open(path).ok()?;
        Some((sig, fh))
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

    /// Evicts cached verified paths that are no longer observed on disk.
    pub fn retain_verified(&mut self, seen: &BTreeSet<PathBuf>) {
        self.verified_paths.retain(|p| seen.contains(p));
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
    pub turns: Vec<TurnRecord>,
    pub last_file_mtime_utc: Option<DateTime<Utc>>,
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

/// Adapter interface for a single local AI coding agent log root.
/// Implementations own their per-file `(mtime, size)` incremental cache.
pub trait SourceAdapter {
    fn kind(&self) -> SourceKind;
    fn root(&self) -> &Path;
    fn scan(&mut self, scan_now_utc: DateTime<Utc>) -> SourceScanResult;

    /// Optional fallback metadata lookup `(title, workspace)` for a parent session ID that
    /// had zero parsed turns (e.g. Antigravity annotations/metadata cache or index files).
    fn lookup_session_hint(&mut self, _cid: &str) -> Option<(String, String)> {
        None
    }
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

fn non_symlink_dir(path: &Path) -> bool {
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

pub fn clean_single_line(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut prev_space = true;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    let trimmed = out.trim();
    if trimmed.chars().count() > max_chars {
        let mut cut: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
        cut.push('…');
        cut
    } else {
        trimmed.to_string()
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
        let outside = std::env::temp_dir().join(format!("agy_gauge_outside_{}.txt", std::process::id()));
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
        assert!(sfs.is_safe_dir(&base));

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
    }
}
