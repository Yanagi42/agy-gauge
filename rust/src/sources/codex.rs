//! Strictly read-only incremental adapter for OpenAI Codex CLI JSONL rollouts.
//!
//! Strict pure-text whitelist under `<root>` (`$CODEX_HOME` or `~/.codex`):
//!   - `<root>/sessions/YYYY/MM/DD/rollout-*.jsonl`
//!   - `<root>/archived_sessions/rollout-*.jsonl`
//!   - `<root>/session_index.jsonl` (optional title index)
//!
//! Strictly NEVER opens or touches `*.sqlite*`, `auth.json`, `config.toml`, or `history.jsonl`.
//! Every directory/file access passes `SafeFs` (non-symlink + canonical-root containment).

use chrono::{DateTime, Utc};
use memchr::memmem;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::pricing::CostBreakdown;
use crate::sources::{
    clean_single_line, compute_running_hint, is_uuid_like, non_symlink_dir,
    read_header_fingerprint, try_seek_for_tail, AdapterPoll, FileSig, FlexTimestamp, FlexU64,
    HeaderFingerprint, InternedStrSeed, OptFlexString, ParsedSession, SafeFs, SourceAdapter,
    SourceScanResult,
};
use crate::stats::{intern_str, SourceKind, TurnRecord};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RawUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

impl RawUsage {
    fn is_zero(self) -> bool {
        self.input_tokens == 0
            && self.cached_input_tokens == 0
            && self.output_tokens == 0
            && self.reasoning_output_tokens == 0
            && self.total_tokens == 0
    }

    fn same_counters(self, other: Self) -> bool {
        self.input_tokens == other.input_tokens
            && self.cached_input_tokens == other.cached_input_tokens
            && self.output_tokens == other.output_tokens
            && self.reasoning_output_tokens == other.reasoning_output_tokens
    }

    fn monotonic_ge(self, prev: Self) -> bool {
        self.input_tokens >= prev.input_tokens
            && self.cached_input_tokens >= prev.cached_input_tokens
            && self.output_tokens >= prev.output_tokens
            && self.reasoning_output_tokens >= prev.reasoning_output_tokens
    }

    fn delta_from(self, prev: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(prev.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(prev.cached_input_tokens),
            output_tokens: self.output_tokens.saturating_sub(prev.output_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_sub(prev.reasoning_output_tokens),
            total_tokens: self.total_tokens.saturating_sub(prev.total_tokens),
        }
    }
}

#[derive(Debug, Clone)]
struct CachedRollout {
    sig: FileSig,
    header: HeaderFingerprint,
    committed_offset: u64,
    session_id: String,
    session_cwd: String,
    turn_cwd: String,
    current_model: Option<Arc<str>>,
    first_user_message: String,
    is_subagent: bool,
    parent_id: Option<String>,
    subagent_nickname: String,
    subagent_role: String,
    prev_total: Option<RawUsage>,
    next_step_index: i64,
    pending_tools: Vec<Arc<str>>,
    pending_reasoning: bool,
    /// Number of trailing tools/reasoning provisionally attached to `turns.last()` at EOF
    /// (undone when tailing resumes and a new `token_count` turn arrives).
    provisional_tail_tools: usize,
    provisional_tail_thinking: bool,
    turns: Arc<[TurnRecord]>,
    last_step_running: bool,
    committed_skipped_lines: usize,
    trailing_skipped_lines: usize,
}

impl CachedRollout {
    fn workspace(&self) -> &str {
        if !self.session_cwd.is_empty() {
            &self.session_cwd
        } else {
            &self.turn_cwd
        }
    }

    fn total_skipped_lines(&self) -> usize {
        self.committed_skipped_lines + self.trailing_skipped_lines
    }
}

#[derive(Debug, Clone, Default)]
struct IndexEntry {
    title: String,
    workspace: String,
}

pub struct CodexAdapter {
    safe_fs: SafeFs,
    rollout_cache: HashMap<PathBuf, CachedRollout>,
    index_sig: Option<FileSig>,
    index_cache: HashMap<String, IndexEntry>,
    /// Lightweight `(title, workspace)` hints populated for all scanned rollouts (including 0-turn).
    session_hints: HashMap<String, (String, String)>,
    has_scanned: bool,
    last_found_root: bool,
}

impl CodexAdapter {
    pub fn new(root: PathBuf) -> Self {
        Self {
            safe_fs: SafeFs::new(root),
            rollout_cache: HashMap::new(),
            index_sig: None,
            index_cache: HashMap::new(),
            session_hints: HashMap::new(),
            has_scanned: false,
            last_found_root: false,
        }
    }

    /// Refreshes `<root>/session_index.jsonl` strictly via `SafeFs` when changed.
    /// Returns `(Some(path), changed)` when present and verified.
    fn refresh_index_cache(&mut self) -> (Option<PathBuf>, bool) {
        let path = self.safe_fs.root().join("session_index.jsonl");
        let Some((sig, freshly_canon)) = self.safe_fs.is_safe_regular_file_checked(&path) else {
            let had = self.index_sig.is_some() || !self.index_cache.is_empty();
            self.index_sig = None;
            self.index_cache.clear();
            return (None, had);
        };
        if self.index_sig == Some(sig) {
            return (Some(path), false);
        }
        if let Some(fh) = self.safe_fs.open_for_read(&path, freshly_canon) {
            self.index_cache = parse_session_index(fh);
            self.index_sig = Some(sig);
            (Some(path), true)
        } else {
            let had = self.index_sig.is_some() || !self.index_cache.is_empty();
            self.index_sig = None;
            self.index_cache.clear();
            (None, had)
        }
    }

    /// Collects strictly whitelisted rollout file paths + `(FileSig, freshly_canonicalized)` under `sessions/YYYY/MM/DD/` and `archived_sessions/`.
    /// Uses `DirEntry` metadata (0 extra syscalls on Windows) for already-verified cached rollouts.
    /// Never enumerates `<root>` directly, avoiding any touch on `*.sqlite*`, `auth.json`, `config.toml`, `history.jsonl`.
    fn collect_rollout_paths(
        &mut self,
        has_sessions: bool,
        has_archived: bool,
        seen_dirs: &mut HashSet<PathBuf>,
    ) -> Vec<(PathBuf, FileSig, bool)> {
        let mut out = Vec::new();

        if has_sessions {
            let sessions_dir = self.safe_fs.root().join("sessions");
            seen_dirs.insert(sessions_dir.clone());
            for year_dir in self.safe_digit_subdirs(&sessions_dir, 4, seen_dirs) {
                for month_dir in self.safe_digit_subdirs(&year_dir, 2, seen_dirs) {
                    for day_dir in self.safe_digit_subdirs(&month_dir, 2, seen_dirs) {
                        collect_rollout_files_in_dir(&day_dir, &mut self.safe_fs, &mut out);
                    }
                }
            }
        }

        if has_archived {
            let archived_dir = self.safe_fs.root().join("archived_sessions");
            seen_dirs.insert(archived_dir.clone());
            collect_rollout_files_in_dir(&archived_dir, &mut self.safe_fs, &mut out);
        }

        out
    }

    fn safe_digit_subdirs(
        &mut self,
        parent: &Path,
        expected_len: usize,
        seen_dirs: &mut HashSet<PathBuf>,
    ) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(parent) else {
            return Vec::new();
        };
        let mut dirs: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() || !ft.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if !is_ascii_digits_len(name_str, expected_len) {
                continue;
            }
            let path = entry.path();
            if self.safe_fs.is_safe_dir(&path) {
                seen_dirs.insert(path.clone());
                dirs.push(path);
            }
        }
        dirs.sort();
        dirs
    }

    fn resolve_title(&self, cached: &CachedRollout) -> String {
        if let Some(idx) = self
            .index_cache
            .get(&cached.session_id)
            .filter(|e| !e.title.is_empty())
        {
            return idx.title.clone();
        }
        let subagent_title =
            format_subagent_title(&cached.subagent_nickname, &cached.subagent_role);
        if !subagent_title.is_empty() {
            return subagent_title;
        }
        if !cached.first_user_message.is_empty() {
            return cached.first_user_message.clone();
        }
        cached.session_id.clone()
    }

    fn scan_internal(&mut self, scan_now_utc: DateTime<Utc>, skip_if_unchanged: bool) -> AdapterPoll {
        self.safe_fs.refresh_root();

        let sessions_dir = self.safe_fs.root().join("sessions");
        let archived_dir = self.safe_fs.root().join("archived_sessions");
        let has_sessions = self.safe_fs.is_safe_dir(&sessions_dir);
        let has_archived = self.safe_fs.is_safe_dir(&archived_dir);

        if !has_sessions && !has_archived {
            let changed = !self.has_scanned
                || self.last_found_root
                || !self.rollout_cache.is_empty()
                || !self.index_cache.is_empty();
            self.rollout_cache.clear();
            self.session_hints.clear();
            self.index_sig = None;
            self.index_cache.clear();
            self.safe_fs.clear_verified();
            self.has_scanned = true;
            self.last_found_root = false;
            return if skip_if_unchanged && !changed {
                AdapterPoll::Unchanged {
                    found_root: false,
                    files_cached: 0,
                    skipped_empty: 0,
                    skipped_lines: 0,
                }
            } else {
                AdapterPoll::Changed(SourceScanResult::default())
            };
        }

        let mut seen_paths: HashSet<PathBuf> =
            HashSet::with_capacity(self.rollout_cache.len() + 4);
        let mut seen_dirs: HashSet<PathBuf> = HashSet::with_capacity(16);
        let mut any_changed = !self.has_scanned || !self.last_found_root;

        let (idx_path, idx_changed) = self.refresh_index_cache();
        if let Some(p) = idx_path {
            seen_paths.insert(p);
        }
        if idx_changed {
            any_changed = true;
        }

        let rollout_paths = self.collect_rollout_paths(has_sessions, has_archived, &mut seen_dirs);
        let mut files_cached = 0usize;
        let mut files_reparsed = 0usize;
        let mut skipped_empty = 0usize;
        let mut skipped_lines = 0usize;
        let mut valid_paths: Vec<PathBuf> = Vec::with_capacity(rollout_paths.len());

        for (path, sig, freshly_canon) in rollout_paths {
            seen_paths.insert(path.clone());

            let need_reparse = match self.rollout_cache.get(&path) {
                Some(entry) if entry.sig == sig => {
                    files_cached += 1;
                    false
                }
                _ => true,
            };

            if need_reparse {
                let Some(fh) = self.safe_fs.open_for_read(&path, freshly_canon) else {
                    continue;
                };
                let prev = self.rollout_cache.remove(&path);
                let fresh = parse_or_tail_rollout_file(fh, &path, sig, prev);
                files_reparsed += 1;
                any_changed = true;
                self.rollout_cache.insert(path.clone(), fresh);
            }

            if let Some(entry) = self.rollout_cache.get(&path) {
                skipped_lines += entry.total_skipped_lines();
                if entry.turns.is_empty() {
                    skipped_empty += 1;
                }
                valid_paths.push(path);
            }
        }

        let prev_len = self.rollout_cache.len();
        self.rollout_cache.retain(|k, _| seen_paths.contains(k));
        if self.rollout_cache.len() != prev_len {
            any_changed = true;
        }
        self.safe_fs.retain_verified(&seen_paths);
        self.safe_fs.retain_verified_dirs(&seen_dirs);

        self.has_scanned = true;
        self.last_found_root = true;

        if skip_if_unchanged && !any_changed {
            return AdapterPoll::Unchanged {
                found_root: true,
                files_cached,
                skipped_empty,
                skipped_lines,
            };
        }

        self.session_hints.clear();
        let mut sessions = Vec::with_capacity(valid_paths.len());

        for path in valid_paths {
            let Some(cached) = self.rollout_cache.get(&path) else {
                continue;
            };
            let title = self.resolve_title(cached);
            let raw_workspace = if !cached.workspace().is_empty() {
                cached.workspace().to_string()
            } else {
                self.index_cache
                    .get(&cached.session_id)
                    .map(|e| e.workspace.clone())
                    .unwrap_or_default()
            };

            let hint_entry = self
                .session_hints
                .entry(cached.session_id.clone())
                .or_insert_with(|| (String::new(), String::new()));
            if hint_entry.0.is_empty() && !title.is_empty() && title != cached.session_id {
                hint_entry.0 = title.clone();
            }
            if hint_entry.1.is_empty() && !raw_workspace.is_empty() {
                hint_entry.1 = raw_workspace.clone();
            }

            if cached.turns.is_empty() {
                continue;
            }

            let workspace = if raw_workspace.is_empty() {
                "(未知工作区)".to_string()
            } else {
                raw_workspace
            };
            let parent_id = if cached.is_subagent {
                cached
                    .parent_id
                    .as_deref()
                    .filter(|p| !p.is_empty() && *p != cached.session_id.as_str())
                    .map(String::from)
            } else {
                None
            };
            let last_file_mtime_utc = cached.sig.mtime_utc();
            let is_running_hint =
                compute_running_hint(cached.last_step_running, last_file_mtime_utc, scan_now_utc);

            sessions.push(ParsedSession {
                conversation_id: cached.session_id.clone(),
                title,
                workspace,
                source: SourceKind::Codex,
                is_subagent: cached.is_subagent,
                parent_id,
                parent_title: String::new(),
                turns: Arc::clone(&cached.turns),
                last_file_mtime_utc,
                last_step_running: cached.last_step_running,
                is_running_hint,
            });
        }

        AdapterPoll::Changed(SourceScanResult {
            sessions,
            skipped_empty,
            skipped_lines,
            files_cached,
            files_reparsed,
            found_root: true,
        })
    }
}

impl SourceAdapter for CodexAdapter {
    fn kind(&self) -> SourceKind {
        SourceKind::Codex
    }

    fn root(&self) -> &Path {
        self.safe_fs.root()
    }

    fn watched_dirs(&self) -> Vec<PathBuf> {
        let root = self.safe_fs.root();
        let mut out = Vec::new();
        for sub in ["sessions", "archived_sessions"] {
            let p = root.join(sub);
            if non_symlink_dir(&p) {
                out.push(p);
            }
        }
        out
    }

    fn lookup_session_hint(&mut self, cid: &str) -> Option<(String, String)> {
        self.safe_fs.refresh_root();
        let _ = self.refresh_index_cache();

        let idx = self.index_cache.get(cid);
        let hint = self.session_hints.get(cid);

        let title = idx
            .filter(|e| !e.title.is_empty())
            .map(|e| e.title.clone())
            .or_else(|| hint.filter(|h| !h.0.is_empty()).map(|h| h.0.clone()))
            .unwrap_or_default();
        let workspace = hint
            .filter(|h| !h.1.is_empty())
            .map(|h| h.1.clone())
            .or_else(|| {
                idx.filter(|e| !e.workspace.is_empty())
                    .map(|e| e.workspace.clone())
            })
            .unwrap_or_default();

        if !title.is_empty() || !workspace.is_empty() {
            Some((title, workspace))
        } else {
            None
        }
    }

    fn scan(&mut self, scan_now_utc: DateTime<Utc>) -> SourceScanResult {
        match self.scan_internal(scan_now_utc, false) {
            AdapterPoll::Changed(res) => res,
            AdapterPoll::Unchanged {
                found_root,
                files_cached,
                skipped_empty,
                skipped_lines,
            } => SourceScanResult {
                found_root,
                files_cached,
                skipped_empty,
                skipped_lines,
                ..Default::default()
            },
            AdapterPoll::Delta { .. } => SourceScanResult::default(),
        }
    }

    fn poll_changes(&mut self, scan_now_utc: DateTime<Utc>) -> AdapterPoll {
        self.scan_internal(scan_now_utc, self.has_scanned)
    }

    fn adopt_priced_turns(
        &mut self,
        _cid: &str,
        old_turns: &Arc<[TurnRecord]>,
        priced_turns: &Arc<[TurnRecord]>,
    ) {
        for entry in self.rollout_cache.values_mut() {
            if Arc::ptr_eq(&entry.turns, old_turns) {
                entry.turns = Arc::clone(priced_turns);
                break;
            }
        }
    }
}

fn is_ascii_digits_len(s: &str, expected_len: usize) -> bool {
    s.len() == expected_len && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_rollout_filename(name: &str) -> bool {
    name.starts_with("rollout-") && name.ends_with(".jsonl")
}

fn collect_rollout_files_in_dir(
    dir: &Path,
    safe_fs: &mut SafeFs,
    out: &mut Vec<(PathBuf, FileSig, bool)>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(PathBuf, FileSig, bool)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name_str = name.to_str()?;
            if !is_rollout_filename(name_str) {
                return None;
            }
            let path = entry.path();
            let (sig, freshly_canon) = safe_fs.stat_verified_dir_entry_checked(&path, &entry)?;
            Some((path, sig, freshly_canon))
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    out.extend(files);
}

fn format_subagent_title(nickname: &str, role: &str) -> String {
    match (nickname.is_empty(), role.is_empty()) {
        (false, false) => {
            if nickname.eq_ignore_ascii_case(role) {
                clean_single_line(nickname, 100)
            } else {
                clean_single_line(&format!("{nickname} ({role})"), 100)
            }
        }
        (false, true) => clean_single_line(nickname, 100),
        (true, false) => clean_single_line(role, 100),
        (true, true) => String::new(),
    }
}

fn session_id_from_filename(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .trim();
    let stripped = stem.strip_prefix("rollout-").unwrap_or(stem);
    if stripped.len() >= 36 {
        let tail = &stripped[stripped.len() - 36..];
        if is_uuid_like(tail)
            && (stripped.len() == 36 || stripped.as_bytes()[stripped.len() - 37] == b'-')
        {
            return tail.to_string();
        }
    }
    if !stripped.is_empty() {
        stripped.to_string()
    } else {
        stem.to_string()
    }
}

fn parse_session_index(fh: File) -> HashMap<String, IndexEntry> {
    let mut out: HashMap<String, IndexEntry> = HashMap::new();
    let mut reader = BufReader::with_capacity(32 * 1024, fh);
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    loop {
        buf.clear();
        let Ok(n) = reader.read_until(b'\n', &mut buf) else {
            break;
        };
        if n == 0 {
            break;
        }
        let line_cow = String::from_utf8_lossy(&buf);
        let trimmed = line_cow.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let Some(id) = map
            .get("id")
            .or_else(|| map.get("thread_id"))
            .or_else(|| map.get("session_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };

        let title = map
            .get("thread_name")
            .and_then(Value::as_str)
            .map(|s| clean_single_line(s, 100))
            .filter(|s| !s.is_empty())
            .or_else(|| {
                map.get("title")
                    .and_then(Value::as_str)
                    .map(|s| clean_single_line(s, 100))
                    .filter(|s| !s.is_empty())
            });
        let workspace = map
            .get("cwd")
            .or_else(|| map.get("workspace"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);

        let entry = out.entry(id.to_string()).or_default();
        if let Some(t) = title {
            entry.title = t;
        }
        if let Some(ws) = workspace {
            entry.workspace = ws;
        }
    }
    out
}

fn maybe_relevant_rollout_line(trimmed: &str, need_user_msg: bool) -> bool {
    let b = trimmed.as_bytes();
    if !(b.first() == Some(&b'{') && b.last() == Some(&b'}') && memmem::find(b, b"\"type\"").is_some()) {
        // Malformed or truncated line: parse with serde_json so skipped_lines stays accurate.
        return true;
    }
    const ALWAYS_WATCHED: &[&[u8]] = &[
        b"\"session_meta\"",
        b"\"turn_context\"",
        b"\"token_count\"",
        b"\"agent_reasoning\"",
        b"\"task_started\"",
        b"\"task_complete\"",
        b"\"turn_aborted\"",
        b"\"function_call\"",
        b"\"custom_tool_call\"",
        b"\"local_shell_call\"",
        b"\"web_search_call\"",
        b"\"mcp_tool_call\"",
        b"\"reasoning\"",
    ];
    if ALWAYS_WATCHED.iter().any(|kw| memmem::find(b, kw).is_some()) {
        return true;
    }
    if need_user_msg
        && (memmem::find(b, b"\"user_message\"").is_some()
            || memmem::find(b, b"\"message\"").is_some())
    {
        return true;
    }
    false
}

// ---------- Zero-DOM Serde Deserializer for Codex Rollouts ----------

struct RawCodexSource {
    is_subagent: bool,
    parent_id: Option<String>,
    nickname: Option<String>,
    role: Option<String>,
}

struct RawCodexTokenInfo {
    total_token_usage: Option<RawUsage>,
    last_token_usage: Option<RawUsage>,
    model: Option<Arc<str>>,
}

struct RawCodexPayload {
    payload_type: Option<String>,
    id: Option<String>,
    cwd: Option<String>,
    model: Option<Arc<str>>,
    collab_model: Option<Arc<str>>,
    source: Option<RawCodexSource>,
    agent_nickname: Option<String>,
    agent_role: Option<String>,
    name: Option<Arc<str>>,
    tool_name: Option<Arc<str>>,
    role: Option<String>,
    user_text: Option<String>,
    info: Option<RawCodexTokenInfo>,
}

struct RawCodexLine {
    timestamp_utc: Option<DateTime<Utc>>,
    record_type: Option<String>,
    payload: Option<RawCodexPayload>,
}

struct CodexLineSeed {
    need_user_msg: bool,
}

impl<'de> de::DeserializeSeed<'de> for CodexLineSeed {
    type Value = RawCodexLine;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V {
            need_user_msg: bool,
        }
        impl<'de> Visitor<'de> for V {
            type Value = RawCodexLine;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Codex rollout JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = RawCodexLine {
                    timestamp_utc: None,
                    record_type: None,
                    payload: None,
                };
                while let Some(k) = map.next_key::<&str>()? {
                    match k {
                        "timestamp" => {
                            let FlexTimestamp(ts) = map.next_value()?;
                            out.timestamp_utc = ts;
                        }
                        "type" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                out.record_type = Some(s);
                            }
                        }
                        "payload" => {
                            out.payload = map.next_value_seed(CodexPayloadSeed {
                                need_user_msg: self.need_user_msg,
                            })?;
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(out)
            }
        }
        deserializer.deserialize_map(V {
            need_user_msg: self.need_user_msg,
        })
    }
}

struct CodexPayloadSeed {
    need_user_msg: bool,
}

impl<'de> de::DeserializeSeed<'de> for CodexPayloadSeed {
    type Value = Option<RawCodexPayload>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V {
            need_user_msg: bool,
        }
        impl<'de> Visitor<'de> for V {
            type Value = Option<RawCodexPayload>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Codex payload object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut p = RawCodexPayload {
                    payload_type: None,
                    id: None,
                    cwd: None,
                    model: None,
                    collab_model: None,
                    source: None,
                    agent_nickname: None,
                    agent_role: None,
                    name: None,
                    tool_name: None,
                    role: None,
                    user_text: None,
                    info: None,
                };
                let mut msg_text: Option<String> = None;
                let mut text_field: Option<String> = None;
                let mut content_text: Option<String> = None;

                while let Some(k) = map.next_key::<&str>()? {
                    match k {
                        "type" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                p.payload_type = Some(s);
                            }
                        }
                        "id" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let t = s.trim();
                                if !t.is_empty() {
                                    p.id = Some(t.to_string());
                                }
                            }
                        }
                        "cwd" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let t = s.trim();
                                if !t.is_empty() {
                                    p.cwd = Some(t.to_string());
                                }
                            }
                        }
                        "model" => {
                            p.model = map.next_value_seed(InternedStrSeed)?;
                        }
                        "collaboration_mode" => {
                            p.collab_model = map.next_value_seed(CodexCollabModelSeed)?;
                        }
                        "source" => {
                            p.source = map.next_value_seed(CodexSourceSeed)?;
                        }
                        "agent_nickname" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let t = s.trim();
                                if !t.is_empty() {
                                    p.agent_nickname = Some(t.to_string());
                                }
                            }
                        }
                        "agent_role" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let t = s.trim();
                                if !t.is_empty() {
                                    p.agent_role = Some(t.to_string());
                                }
                            }
                        }
                        "name" => {
                            p.name = map.next_value_seed(InternedStrSeed)?;
                        }
                        "tool_name" => {
                            p.tool_name = map.next_value_seed(InternedStrSeed)?;
                        }
                        "role" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                p.role = Some(s);
                            }
                        }
                        "message" if self.need_user_msg => {
                            msg_text = map.next_value_seed(CodexUserTextSeed)?;
                        }
                        "text" if self.need_user_msg => {
                            text_field = map.next_value_seed(CodexUserTextSeed)?;
                        }
                        "content" if self.need_user_msg => {
                            content_text = map.next_value_seed(CodexUserTextSeed)?;
                        }
                        "info" => {
                            p.info = map.next_value_seed(CodexTokenInfoSeed)?;
                        }
                        // Skip huge `arguments`, `input`, `output`, `encrypted_content`, `rate_limits`, `summary`, etc.!
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                p.user_text = msg_text.or(text_field).or(content_text);
                Ok(Some(p))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }
        deserializer.deserialize_any(V {
            need_user_msg: self.need_user_msg,
        })
    }
}

struct CodexCollabModelSeed;
impl<'de> de::DeserializeSeed<'de> for CodexCollabModelSeed {
    type Value = Option<Arc<str>>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<Arc<str>>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("collaboration_mode object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut m = None;
                while let Some(k) = map.next_key::<&str>()? {
                    if k == "settings" {
                        m = map.next_value_seed(CodexSettingsModelSeed)?;
                    } else {
                        let _: IgnoredAny = map.next_value()?;
                    }
                }
                Ok(m)
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct CodexSettingsModelSeed;
impl<'de> de::DeserializeSeed<'de> for CodexSettingsModelSeed {
    type Value = Option<Arc<str>>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<Arc<str>>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("settings object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut m = None;
                while let Some(k) = map.next_key::<&str>()? {
                    if k == "model" {
                        m = map.next_value_seed(InternedStrSeed)?;
                    } else {
                        let _: IgnoredAny = map.next_value()?;
                    }
                }
                Ok(m)
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct CodexSourceSeed;
impl<'de> de::DeserializeSeed<'de> for CodexSourceSeed {
    type Value = Option<RawCodexSource>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<RawCodexSource>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("session_meta source")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut sub = None;
                while let Some(k) = map.next_key::<&str>()? {
                    if k == "subagent" {
                        sub = Some(map.next_value_seed(CodexSubagentSeed)?);
                    } else {
                        let _: IgnoredAny = map.next_value()?;
                    }
                }
                Ok(sub)
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct CodexSubagentSeed;
impl<'de> de::DeserializeSeed<'de> for CodexSubagentSeed {
    type Value = RawCodexSource;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawCodexSource;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("subagent metadata")
            }
            fn visit_str<E>(self, s: &str) -> Result<Self::Value, E> {
                let t = s.trim();
                Ok(RawCodexSource {
                    is_subagent: true,
                    parent_id: None,
                    nickname: None,
                    role: (!t.is_empty()).then(|| t.to_string()),
                })
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut res = RawCodexSource {
                    is_subagent: true,
                    parent_id: None,
                    nickname: None,
                    role: None,
                };
                while let Some(k) = map.next_key::<&str>()? {
                    if k == "thread_spawn" {
                        let (pid, nick, role) = map.next_value_seed(CodexThreadSpawnSeed)?;
                        res.parent_id = pid;
                        res.nickname = nick;
                        res.role = role;
                    } else {
                        let _: IgnoredAny = map.next_value()?;
                    }
                }
                Ok(res)
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(RawCodexSource {
                    is_subagent: true,
                    parent_id: None,
                    nickname: None,
                    role: None,
                })
            }
            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(RawCodexSource {
                    is_subagent: true,
                    parent_id: None,
                    nickname: None,
                    role: None,
                })
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(RawCodexSource {
                    is_subagent: true,
                    parent_id: None,
                    nickname: None,
                    role: None,
                })
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct CodexThreadSpawnSeed;
impl<'de> de::DeserializeSeed<'de> for CodexThreadSpawnSeed {
    type Value = (Option<String>, Option<String>, Option<String>);
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = (Option<String>, Option<String>, Option<String>);
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("thread_spawn object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut pid = None;
                let mut nick = None;
                let mut role = None;
                while let Some(k) = map.next_key::<&str>()? {
                    match k {
                        "parent_thread_id" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let t = s.trim();
                                if !t.is_empty() {
                                    pid = Some(t.to_string());
                                }
                            }
                        }
                        "agent_nickname" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let t = s.trim();
                                if !t.is_empty() {
                                    nick = Some(t.to_string());
                                }
                            }
                        }
                        "agent_role" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let t = s.trim();
                                if !t.is_empty() {
                                    role = Some(t.to_string());
                                }
                            }
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok((pid, nick, role))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok((None, None, None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok((None, None, None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct CodexUserTextSeed;
impl<'de> de::DeserializeSeed<'de> for CodexUserTextSeed {
    type Value = Option<String>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<String>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("string or parts array")
            }
            fn visit_str<E>(self, s: &str) -> Result<Self::Value, E> {
                let c = clean_single_line(s, 100);
                Ok((!c.is_empty()).then_some(c))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut joined = String::new();
                while let Some(part) = seq.next_element_seed(CodexTextPartSeed)? {
                    if let Some(t) = part {
                        if joined.len() < 400 {
                            if !joined.is_empty() {
                                joined.push(' ');
                            }
                            joined.push_str(&t);
                        }
                    }
                }
                let c = clean_single_line(&joined, 100);
                Ok((!c.is_empty()).then_some(c))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct CodexTextPartSeed;
impl<'de> de::DeserializeSeed<'de> for CodexTextPartSeed {
    type Value = Option<String>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<String>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("text part object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut txt = None;
                while let Some(k) = map.next_key::<&str>()? {
                    if k == "text" {
                        if let OptFlexString(Some(s)) = map.next_value()? {
                            txt = Some(s);
                        }
                    } else {
                        let _: IgnoredAny = map.next_value()?;
                    }
                }
                Ok(txt)
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct CodexTokenInfoSeed;
impl<'de> de::DeserializeSeed<'de> for CodexTokenInfoSeed {
    type Value = Option<RawCodexTokenInfo>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<RawCodexTokenInfo>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("token_count info object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut info = RawCodexTokenInfo {
                    total_token_usage: None,
                    last_token_usage: None,
                    model: None,
                };
                while let Some(k) = map.next_key::<&str>()? {
                    match k {
                        "total_token_usage" => {
                            info.total_token_usage = map.next_value_seed(CodexRawUsageSeed)?;
                        }
                        "last_token_usage" => {
                            info.last_token_usage = map.next_value_seed(CodexRawUsageSeed)?;
                        }
                        "model" => {
                            info.model = map.next_value_seed(InternedStrSeed)?;
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(Some(info))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct CodexRawUsageSeed;
impl<'de> de::DeserializeSeed<'de> for CodexRawUsageSeed {
    type Value = Option<RawUsage>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<RawUsage>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Codex usage object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut u = RawUsage::default();
                let mut cached_primary: Option<u64> = None;
                let mut cached_fallback: Option<u64> = None;
                while let Some(k) = map.next_key::<&str>()? {
                    match k {
                        "input_tokens" => {
                            let FlexU64(v) = map.next_value()?;
                            u.input_tokens = v;
                        }
                        "cached_input_tokens" => {
                            let FlexU64(v) = map.next_value()?;
                            cached_primary = Some(v);
                        }
                        "cache_read_input_tokens" => {
                            let FlexU64(v) = map.next_value()?;
                            cached_fallback = Some(v);
                        }
                        "output_tokens" => {
                            let FlexU64(v) = map.next_value()?;
                            u.output_tokens = v;
                        }
                        "reasoning_output_tokens" => {
                            let FlexU64(v) = map.next_value()?;
                            u.reasoning_output_tokens = v;
                        }
                        "total_tokens" => {
                            let FlexU64(v) = map.next_value()?;
                            u.total_tokens = v;
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                u.cached_input_tokens = cached_primary.or(cached_fallback).unwrap_or(0);
                Ok(Some(u))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }
        deserializer.deserialize_any(V)
    }
}

fn parse_or_tail_rollout_file(
    mut fh: File,
    path: &Path,
    sig: FileSig,
    prev: Option<CachedRollout>,
) -> CachedRollout {
    if let Some(prev_state) = prev {
        if let Some(fresh_hdr) = try_seek_for_tail(
            &mut fh,
            prev_state.sig,
            sig,
            prev_state.header,
            prev_state.committed_offset,
        ) {
            return parse_rollout_from_offset(fh, path, sig, fresh_hdr, prev_state);
        }
    }
    let header = read_header_fingerprint(&mut fh).unwrap_or_default();
    let _ = fh.seek(SeekFrom::Start(0));
    let init = CachedRollout {
        sig,
        header,
        committed_offset: 0,
        session_id: String::new(),
        session_cwd: String::new(),
        turn_cwd: String::new(),
        current_model: None,
        first_user_message: String::new(),
        is_subagent: false,
        parent_id: None,
        subagent_nickname: String::new(),
        subagent_role: String::new(),
        prev_total: None,
        next_step_index: 0,
        pending_tools: Vec::new(),
        pending_reasoning: false,
        provisional_tail_tools: 0,
        provisional_tail_thinking: false,
        turns: Arc::from(Vec::new()),
        last_step_running: false,
        committed_skipped_lines: 0,
        trailing_skipped_lines: 0,
    };
    parse_rollout_from_offset(fh, path, sig, header, init)
}

fn parse_rollout_from_offset(
    fh: File,
    path: &Path,
    sig: FileSig,
    header: HeaderFingerprint,
    mut state: CachedRollout,
) -> CachedRollout {
    let mut reader = BufReader::with_capacity(64 * 1024, fh);
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut offset = state.committed_offset;
    let mut committed_offset = state.committed_offset;
    let mut trailing_skipped_lines = 0usize;

    let mut turns_vec: Option<Vec<TurnRecord>> = None;
    fn ensure_turns_mut<'a>(
        opt: &'a mut Option<Vec<TurnRecord>>,
        base: &Arc<[TurnRecord]>,
    ) -> &'a mut Vec<TurnRecord> {
        opt.get_or_insert_with(|| base.to_vec())
    }

    // Undo any provisional trailing tool/reasoning attachment before processing new bytes.
    if state.provisional_tail_tools > 0 || state.provisional_tail_thinking {
        let tv = ensure_turns_mut(&mut turns_vec, &state.turns);
        if let Some(last) = tv.last_mut() {
            if state.provisional_tail_tools > 0 && last.tool_names.len() >= state.provisional_tail_tools {
                let keep = last.tool_names.len() - state.provisional_tail_tools;
                let mut prev_tools = last.tool_names.to_vec();
                let restored: Vec<Arc<str>> = prev_tools.drain(keep..).collect();
                last.tool_names = prev_tools.into_boxed_slice();
                state.pending_tools.splice(0..0, restored);
            }
            if state.provisional_tail_thinking {
                last.thinking_chars = 0;
                state.pending_reasoning = true;
            }
        }
        state.provisional_tail_tools = 0;
        state.provisional_tail_thinking = false;
    }

    loop {
        buf.clear();
        let Ok(n) = reader.read_until(b'\n', &mut buf) else {
            break;
        };
        if n == 0 {
            break;
        }
        offset += n as u64;
        let has_newline = buf.last() == Some(&b'\n');

        let line_cow = String::from_utf8_lossy(&buf);
        let trimmed = line_cow.trim();
        if trimmed.is_empty() {
            if has_newline {
                committed_offset = offset;
            }
            continue;
        }
        if !maybe_relevant_rollout_line(trimmed, state.first_user_message.is_empty()) {
            if has_newline {
                committed_offset = offset;
            }
            continue;
        }

        let seed = CodexLineSeed {
            need_user_msg: state.first_user_message.is_empty(),
        };
        let mut de = serde_json::Deserializer::from_str(trimmed);
        let Ok(parsed) = de::DeserializeSeed::deserialize(seed, &mut de) else {
            if has_newline && offset < sig.len {
                state.committed_skipped_lines += 1;
                committed_offset = offset;
            } else {
                trailing_skipped_lines += 1;
            }
            continue;
        };
        if has_newline {
            committed_offset = offset;
        }

        let record_type = parsed.record_type.as_deref().unwrap_or("");
        let Some(payload) = parsed.payload else {
            continue;
        };

        match record_type {
            "session_meta" => {
                if let Some(id) = payload.id {
                    state.session_id = id;
                }
                if let Some(cwd) = payload.cwd {
                    state.session_cwd = cwd;
                }
                if state.current_model.is_none() {
                    if let Some(m) = payload.model {
                        state.current_model = Some(m);
                    }
                }
                if let Some(src) = payload.source {
                    if src.is_subagent {
                        state.is_subagent = true;
                        let has_parent_id = src.parent_id.is_some();
                        if let Some(pid) = src.parent_id {
                            state.parent_id = Some(pid);
                        }
                        if let Some(nick) = src.nickname {
                            state.subagent_nickname = nick;
                        }
                        if let Some(role) = src.role {
                            if state.subagent_role.is_empty() || has_parent_id {
                                state.subagent_role = role;
                            }
                        }
                    }
                }
                if state.subagent_nickname.is_empty() {
                    if let Some(nick) = payload.agent_nickname {
                        state.subagent_nickname = nick;
                    }
                }
                if state.subagent_role.is_empty() {
                    if let Some(role) = payload.agent_role {
                        state.subagent_role = role;
                    }
                }
            }

            "turn_context" => {
                if let Some(m) = payload.model.or(payload.collab_model) {
                    state.current_model = Some(m);
                }
                if let Some(cwd) = payload.cwd {
                    state.turn_cwd = cwd;
                }
            }

            "response_item" => {
                let item_type = payload.payload_type.as_deref().unwrap_or("");
                match item_type {
                    "function_call"
                    | "custom_tool_call"
                    | "local_shell_call"
                    | "web_search_call"
                    | "mcp_tool_call" => {
                        let tool_name = payload
                            .name
                            .or(payload.tool_name)
                            .unwrap_or_else(|| intern_str(item_type));
                        state.pending_tools.push(tool_name);
                    }
                    "reasoning" => {
                        state.pending_reasoning = true;
                    }
                    "message" => {
                        if state.first_user_message.is_empty()
                            && payload.role.as_deref() == Some("user")
                        {
                            if let Some(msg) = payload.user_text {
                                state.first_user_message = msg;
                            }
                        }
                    }
                    _ => {}
                }
            }

            "event_msg" => {
                let ev_type = payload.payload_type.as_deref().unwrap_or("");
                match ev_type {
                    "user_message" => {
                        if state.first_user_message.is_empty() {
                            if let Some(msg) = payload.user_text {
                                state.first_user_message = msg;
                            }
                        }
                    }
                    "agent_reasoning" => {
                        state.pending_reasoning = true;
                    }
                    "task_started" => {
                        state.last_step_running = true;
                    }
                    "task_complete" | "turn_aborted" => {
                        state.last_step_running = false;
                    }
                    "token_count" => {
                        let Some(info) = payload.info else {
                            continue;
                        };
                        let curr_total = info.total_token_usage;
                        let last_usage = info.last_token_usage;

                        let raw_turn = match (curr_total, state.prev_total) {
                            (Some(curr), Some(prev)) => {
                                if curr.same_counters(prev) {
                                    continue;
                                } else if curr.monotonic_ge(prev) {
                                    state.prev_total = Some(curr);
                                    curr.delta_from(prev)
                                } else {
                                    state.prev_total = Some(curr);
                                    match last_usage {
                                        Some(last) => last,
                                        None => curr,
                                    }
                                }
                            }
                            (Some(curr), None) => {
                                state.prev_total = Some(curr);
                                if curr.is_zero() {
                                    continue;
                                }
                                curr
                            }
                            (None, _) => match last_usage {
                                Some(last) => last,
                                None => continue,
                            },
                        };

                        let cache_read_tokens =
                            raw_turn.cached_input_tokens.min(raw_turn.input_tokens);
                        let input_tokens = raw_turn
                            .input_tokens
                            .saturating_sub(raw_turn.cached_input_tokens);
                        let output_tokens = raw_turn.output_tokens;
                        let reasoning_tokens = raw_turn.reasoning_output_tokens.min(output_tokens);

                        if input_tokens + cache_read_tokens + output_tokens == 0 {
                            continue;
                        }

                        let saw_reasoning = std::mem::take(&mut state.pending_reasoning);
                        let tool_names = std::mem::take(&mut state.pending_tools).into_boxed_slice();
                        let thinking_chars = if saw_reasoning && reasoning_tokens == 0 {
                            1
                        } else {
                            0
                        };

                        let model = state
                            .current_model
                            .clone()
                            .or(info.model)
                            .unwrap_or_else(|| intern_str("unknown"));

                        let tv = ensure_turns_mut(&mut turns_vec, &state.turns);
                        tv.push(TurnRecord {
                            step_index: state.next_step_index,
                            timestamp_utc: parsed.timestamp_utc,
                            model,
                            is_checkpoint: false,
                            input_tokens,
                            cache_read_tokens,
                            cache_write_tokens: 0,
                            cache_write_1h_tokens: 0,
                            output_tokens,
                            reasoning_tokens,
                            tool_names,
                            thinking_chars,
                            gen_secs: None,
                            cost: CostBreakdown::default(),
                        });
                        state.next_step_index += 1;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // Provisionally attach trailing tools/reasoning to the last turn at EOF (recoverable on next tail!).
    if !state.pending_tools.is_empty() || state.pending_reasoning {
        let has_turns = turns_vec
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(!state.turns.is_empty());
        if has_turns {
            let tv = ensure_turns_mut(&mut turns_vec, &state.turns);
            if let Some(last) = tv.last_mut() {
                if !state.pending_tools.is_empty() {
                    let added = state.pending_tools.len();
                    let mut combined = last.tool_names.to_vec();
                    combined.extend(state.pending_tools.drain(..));
                    last.tool_names = combined.into_boxed_slice();
                    state.provisional_tail_tools = added;
                }
                if state.pending_reasoning && last.reasoning_tokens == 0 && last.thinking_chars == 0
                {
                    last.thinking_chars = 1;
                    state.pending_reasoning = false;
                    state.provisional_tail_thinking = true;
                }
            }
        }
    }

    if let Some(tv) = turns_vec {
        state.turns = tv.into();
    }
    if state.session_id.is_empty() {
        state.session_id = session_id_from_filename(path);
    }

    state.sig = sig;
    state.header = header;
    state.committed_offset = committed_offset;
    state.trailing_skipped_lines = trailing_skipped_lines;
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(tag: &str) -> Self {
            let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "agy_gauge_codex_{tag}_{}_{}",
                std::process::id(),
                n
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, rel: &str, content: &str) -> PathBuf {
            let p = self.0.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&p, content).unwrap();
            p
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scan_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-24T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn test_inclusive_input_and_reasoning_token_math() {
        let root = TempRoot::new("math");
        root.write(
            "sessions/2026/11/24/rollout-2026-11-24T10-00-00-11111111-1111-1111-1111-111111111111.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:00:00Z","type":"session_meta","payload":{"id":"sess-math","cwd":"C:\\work\\demo","source":"cli"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:01Z","type":"turn_context","payload":{"model":"gpt-5.3-codex","cwd":"C:\\work\\demo"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10000,"cached_input_tokens":7500,"output_tokens":600,"reasoning_output_tokens":250,"total_tokens":10600},"last_token_usage":{"input_tokens":10000,"cached_input_tokens":7500,"output_tokens":600,"reasoning_output_tokens":250,"total_tokens":10600}}}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:10Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":300,"cached_input_tokens":500,"output_tokens":80,"reasoning_output_tokens":120}}}}"#, "\n",
            ),
        );

        let mut adapter = CodexAdapter::new(root.path().to_path_buf());
        let res = adapter.scan(scan_now());
        assert!(res.found_root);
        assert_eq!(res.sessions.len(), 1);
        let sess = &res.sessions[0];
        assert_eq!(sess.source, SourceKind::Codex);
        assert_eq!(sess.conversation_id, "sess-math");
        assert_eq!(sess.workspace, r"C:\work\demo");
        assert_eq!(sess.turns.len(), 2);

        let t0 = &sess.turns[0];
        assert_eq!(&*t0.model, "gpt-5.3-codex");
        assert_eq!(t0.input_tokens, 2500);
        assert_eq!(t0.cache_read_tokens, 7500);
        assert_eq!(t0.cache_write_tokens, 0);
        assert_eq!(t0.cache_write_1h_tokens, 0);
        assert_eq!(t0.output_tokens, 600);
        assert_eq!(t0.reasoning_tokens, 250);
        assert_eq!(t0.total_input(), 10000);
        assert_eq!(t0.total_tokens(), 10600);
        assert!(t0.has_thinking());
        assert_eq!(t0.thinking_chars, 0);
        assert_eq!(t0.cost, CostBreakdown::default());

        let t1 = &sess.turns[1];
        assert_eq!(t1.input_tokens, 0);
        assert_eq!(t1.cache_read_tokens, 300);
        assert_eq!(t1.output_tokens, 80);
        assert_eq!(t1.reasoning_tokens, 80);
        assert_eq!(t1.total_input(), 300);
        assert!(t1.has_thinking());
    }

    #[test]
    fn test_cumulative_dedup_monotonic_delta_and_null_info_skip() {
        let root = TempRoot::new("dedup");
        root.write(
            "sessions/2026/11/24/rollout-dedup.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:00:00Z","type":"session_meta","payload":{"id":"sess-dedup","cwd":"/repo","source":"cli"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:01Z","type":"turn_context","payload":{"model":"gpt-5.2-codex"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:02Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"primary":{"used_percent":12}}}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2000,"cached_input_tokens":500,"output_tokens":100,"reasoning_output_tokens":0,"total_tokens":2100},"last_token_usage":{"input_tokens":2000,"cached_input_tokens":500,"output_tokens":100,"reasoning_output_tokens":0,"total_tokens":2100}}}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:06Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2000,"cached_input_tokens":500,"output_tokens":100,"reasoning_output_tokens":0,"total_tokens":2100},"last_token_usage":{"input_tokens":2000,"cached_input_tokens":500,"output_tokens":100,"reasoning_output_tokens":0,"total_tokens":2100}}}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:07Z","type":"response_item","payload":{"type":"function_call","name":"shell_command"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:08Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:12Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":5000,"cached_input_tokens":3000,"output_tokens":250,"reasoning_output_tokens":40,"total_tokens":5250},"last_token_usage":{"input_tokens":3000,"cached_input_tokens":2500,"output_tokens":150,"reasoning_output_tokens":40,"total_tokens":3150}}}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:13Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":5000,"cached_input_tokens":3000,"output_tokens":250,"reasoning_output_tokens":40,"total_tokens":5250},"last_token_usage":{"input_tokens":3000,"cached_input_tokens":2500,"output_tokens":150,"reasoning_output_tokens":40,"total_tokens":3150}}}}"#, "\n",
            ),
        );

        let mut adapter = CodexAdapter::new(root.path().to_path_buf());
        let res = adapter.scan(scan_now());
        assert_eq!(res.sessions.len(), 1);
        let turns = &res.sessions[0].turns;
        assert_eq!(turns.len(), 2);

        assert_eq!(turns[0].input_tokens, 1500);
        assert_eq!(turns[0].cache_read_tokens, 500);
        assert_eq!(turns[0].output_tokens, 100);
        assert_eq!(turns[0].reasoning_tokens, 0);
        assert!(!turns[0].has_thinking());

        assert_eq!(turns[1].input_tokens, 500);
        assert_eq!(turns[1].cache_read_tokens, 2500);
        assert_eq!(turns[1].output_tokens, 150);
        assert_eq!(turns[1].reasoning_tokens, 40);
        assert!(turns[1].has_thinking());
        let t1_tools: Vec<&str> = turns[1].tool_names.iter().map(AsRef::as_ref).collect();
        assert_eq!(t1_tools, vec!["shell_command"]);

        let sum_in: u64 = turns.iter().map(TurnRecord::total_input).sum();
        let sum_out: u64 = turns.iter().map(|t| t.output_tokens).sum();
        assert_eq!(sum_in, 5000);
        assert_eq!(sum_out, 250);
    }

    #[test]
    fn test_reset_and_missing_total_fallback_to_last_token_usage() {
        let root = TempRoot::new("reset");
        root.write(
            "sessions/2026/11/24/rollout-reset.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:00:00Z","type":"session_meta","payload":{"id":"sess-reset","cwd":"/repo"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:01Z","type":"turn_context","payload":{"model":"gpt-5.3-codex"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":4000,"cached_input_tokens":2000,"output_tokens":300,"reasoning_output_tokens":50},"last_token_usage":{"input_tokens":4000,"cached_input_tokens":2000,"output_tokens":300,"reasoning_output_tokens":50}}}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:10Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":120,"reasoning_output_tokens":20},"last_token_usage":{"input_tokens":900,"cached_input_tokens":300,"output_tokens":100,"reasoning_output_tokens":15}}}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:11Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":120,"reasoning_output_tokens":20},"last_token_usage":{"input_tokens":900,"cached_input_tokens":300,"output_tokens":100,"reasoning_output_tokens":15}}}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:15Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2500,"cached_input_tokens":1400,"output_tokens":220,"reasoning_output_tokens":30},"last_token_usage":{"input_tokens":1500,"cached_input_tokens":1000,"output_tokens":100,"reasoning_output_tokens":10}}}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:20Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":800,"cached_input_tokens":200,"output_tokens":50,"reasoning_output_tokens":0}}}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:25Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0}}}}"#, "\n",
            ),
        );

        let mut adapter = CodexAdapter::new(root.path().to_path_buf());
        let res = adapter.scan(scan_now());
        assert_eq!(res.sessions.len(), 1);
        let turns = &res.sessions[0].turns;
        assert_eq!(turns.len(), 4);

        assert_eq!((turns[0].input_tokens, turns[0].cache_read_tokens, turns[0].output_tokens, turns[0].reasoning_tokens), (2000, 2000, 300, 50));
        assert_eq!((turns[1].input_tokens, turns[1].cache_read_tokens, turns[1].output_tokens, turns[1].reasoning_tokens), (600, 300, 100, 15));
        assert_eq!((turns[2].input_tokens, turns[2].cache_read_tokens, turns[2].output_tokens, turns[2].reasoning_tokens), (500, 1000, 100, 10));
        assert_eq!((turns[3].input_tokens, turns[3].cache_read_tokens, turns[3].output_tokens, turns[3].reasoning_tokens), (600, 200, 50, 0));
    }

    #[test]
    fn test_turn_context_model_cwd_tools_and_reasoning_attachment() {
        let root = TempRoot::new("tools");
        root.write(
            "sessions/2026/11/24/rollout-tools.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:00:00Z","type":"session_meta","payload":{"id":"sess-tools","cwd":"","source":"cli"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:01Z","type":"turn_context","payload":{"model":"gpt-5.2-codex","cwd":"C:\\work\\fallback-cwd"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:02Z","type":"response_item","payload":{"type":"reasoning","summary":[{"text":"thinking"}]}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:03Z","type":"response_item","payload":{"type":"function_call","name":"exec_command"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:03Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:04Z","type":"response_item","payload":{"type":"local_shell_call"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:04Z","type":"response_item","payload":{"type":"web_search_call"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:04Z","type":"response_item","payload":{"type":"mcp_tool_call","name":"mcp__docs__search"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:05Z","type":"response_item","payload":{"type":"function_call_output","output":"ok"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:06Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":0,"output_tokens":100,"reasoning_output_tokens":0}}}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:07Z","type":"turn_context","payload":{"model":"gpt-5.3-codex","cwd":"C:\\work\\fallback-cwd"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:08Z","type":"event_msg","payload":{"type":"agent_reasoning","text":"checking diff"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:10Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2000,"cached_input_tokens":500,"output_tokens":200,"reasoning_output_tokens":0}}}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:15Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2500,"cached_input_tokens":800,"output_tokens":250,"reasoning_output_tokens":0}}}}"#, "\n",
            ),
        );

        let mut adapter = CodexAdapter::new(root.path().to_path_buf());
        let res = adapter.scan(scan_now());
        assert_eq!(res.skipped_lines, 0);
        assert_eq!(res.sessions.len(), 1);
        let sess = &res.sessions[0];
        assert_eq!(sess.workspace, r"C:\work\fallback-cwd");
        assert_eq!(sess.turns.len(), 3);

        assert_eq!(&*sess.turns[0].model, "gpt-5.2-codex");
        let t0_tools: Vec<&str> = sess.turns[0].tool_names.iter().map(AsRef::as_ref).collect();
        assert_eq!(
            t0_tools,
            vec![
                "exec_command",
                "apply_patch",
                "local_shell_call",
                "web_search_call",
                "mcp__docs__search",
            ]
        );
        assert_eq!(sess.turns[0].reasoning_tokens, 0);
        assert_eq!(sess.turns[0].thinking_chars, 1);
        assert!(sess.turns[0].has_thinking());

        assert_eq!(&*sess.turns[1].model, "gpt-5.3-codex");
        assert!(sess.turns[1].tool_names.is_empty());
        assert_eq!(sess.turns[1].thinking_chars, 1);
        assert!(sess.turns[1].has_thinking());

        assert_eq!(&*sess.turns[2].model, "gpt-5.3-codex");
        assert!(sess.turns[2].tool_names.is_empty());
        assert_eq!(sess.turns[2].thinking_chars, 0);
        assert!(!sess.turns[2].has_thinking());
    }

    #[test]
    fn test_subagent_thread_spawn_title_priority_and_lookup_hint() {
        let root = TempRoot::new("subagent");
        root.write(
            "session_index.jsonl",
            concat!(
                r#"{"id":"s-indexed","thread_name":"Old Name"}"#, "\n",
                r#"{"id":"s-indexed","thread_name":"Final Indexed Title"}"#, "\n",
                r#"{"id":"parent-0turn","title":"Parent From Index"}"#, "\n",
            ),
        );
        root.write(
            "sessions/2026/11/24/rollout-parent.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:00:00Z","type":"session_meta","payload":{"id":"parent-0turn","cwd":"C:\\work\\parent-repo","source":"cli"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Parent prompt"}}"#, "\n",
            ),
        );
        root.write(
            "sessions/2026/11/24/rollout-s1.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:01:00Z","type":"session_meta","payload":{"id":"s-indexed","cwd":"C:\\work\\repo","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-0turn","depth":1,"agent_nickname":"Scout","agent_role":"explorer"}}}}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:01:01Z","type":"event_msg","payload":{"type":"user_message","message":"Inspect files"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:01:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"cached_input_tokens":0,"output_tokens":50}}}}"#, "\n",
            ),
        );
        root.write(
            "sessions/2026/11/24/rollout-s2.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:02:00Z","type":"session_meta","payload":{"id":"s-subagent","cwd":"","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-0turn","depth":1,"agent_nickname":"Scout","agent_role":"explorer"}}}}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:02:01Z","type":"event_msg","payload":{"type":"user_message","message":"Search logs"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:02:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":400,"cached_input_tokens":0,"output_tokens":40}}}}"#, "\n",
            ),
        );
        root.write(
            "sessions/2026/11/24/rollout-s3.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:03:00Z","type":"session_meta","payload":{"id":"s-usermsg","cwd":"/repo","source":"cli"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:03:01Z","type":"event_msg","payload":{"type":"user_message","message":"  Fix\n  build   errors  "}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:03:02Z","type":"event_msg","payload":{"type":"user_message","message":"Second message ignored"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:03:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":300,"cached_input_tokens":0,"output_tokens":30}}}}"#, "\n",
            ),
        );
        root.write(
            "sessions/2026/11/24/rollout-s4.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:04:00Z","type":"session_meta","payload":{"id":"s-fallback-id","cwd":"/repo","source":"cli"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:04:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":200,"cached_input_tokens":0,"output_tokens":20}}}}"#, "\n",
            ),
        );

        let mut adapter = CodexAdapter::new(root.path().to_path_buf());
        let res = adapter.scan(scan_now());
        assert_eq!(res.skipped_empty, 1);
        assert_eq!(res.sessions.len(), 4);

        let by_id: HashMap<&str, &ParsedSession> = res
            .sessions
            .iter()
            .map(|s| (s.conversation_id.as_str(), s))
            .collect();

        assert_eq!(by_id["s-indexed"].title, "Final Indexed Title");
        assert!(by_id["s-indexed"].is_subagent);
        assert_eq!(
            by_id["s-indexed"].parent_id.as_deref(),
            Some("parent-0turn")
        );

        assert_eq!(by_id["s-subagent"].title, "Scout (explorer)");
        assert!(by_id["s-subagent"].is_subagent);
        assert_eq!(
            by_id["s-subagent"].parent_id.as_deref(),
            Some("parent-0turn")
        );

        assert_eq!(by_id["s-usermsg"].title, "Fix build errors");
        assert!(!by_id["s-usermsg"].is_subagent);
        assert_eq!(by_id["s-usermsg"].parent_id, None);

        assert_eq!(by_id["s-fallback-id"].title, "s-fallback-id");

        let hint = adapter.lookup_session_hint("parent-0turn");
        assert_eq!(
            hint,
            Some((
                "Parent From Index".to_string(),
                r"C:\work\parent-repo".to_string()
            ))
        );
        assert_eq!(adapter.lookup_session_hint("nonexistent-id"), None);
    }

    #[test]
    fn test_archived_sessions_and_zero_sqlite_or_secret_touch() {
        let root = TempRoot::new("archived");
        root.write(
            "sessions/2026/11/24/rollout-live.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:00:00Z","type":"session_meta","payload":{"id":"live-1","cwd":"/live"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":200,"output_tokens":50}}}}"#, "\n",
            ),
        );
        root.write(
            "archived_sessions/rollout-old.jsonl",
            concat!(
                r#"{"timestamp":"2026-10-01T08:00:00Z","type":"session_meta","payload":{"id":"arch-1","cwd":"/arch"}}"#, "\n",
                r#"{"timestamp":"2026-10-01T08:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":600,"cached_input_tokens":100,"output_tokens":30}}}}"#, "\n",
            ),
        );
        root.write("state_5.sqlite", "SQLite format 3\0not-json");
        root.write("state_5.sqlite-wal", "wal-binary-garbage{{{");
        root.write("logs_2.sqlite-wal", "wal-binary-garbage{{{");
        root.write("auth.json", "{SECRET_TOKEN_MUST_NOT_BE_READ");
        root.write("config.toml", "api_key = 'must-not-read'");
        root.write("history.jsonl", "not-json-history-line");
        root.write("sessions/2026/11/24/other.jsonl", "not-a-rollout-file");
        root.write("sessions/badyear/11/24/rollout-bad.jsonl", "invalid-year-dir");
        root.write("archived_sessions/not-rollout.jsonl", "ignored-archived");

        let mut adapter = CodexAdapter::new(root.path().to_path_buf());
        let res = adapter.scan(scan_now());
        assert!(res.found_root);
        assert_eq!(res.skipped_lines, 0);
        assert_eq!(res.files_reparsed, 2);
        assert_eq!(res.sessions.len(), 2);

        let ids: BTreeSet<&str> = res
            .sessions
            .iter()
            .map(|s| s.conversation_id.as_str())
            .collect();
        assert!(ids.contains("live-1"));
        assert!(ids.contains("arch-1"));

        let arch_only = TempRoot::new("arch_only");
        arch_only.write(
            "archived_sessions/rollout-only.jsonl",
            r#"{"timestamp":"2026-10-02T08:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10}}}}"#,
        );
        let mut arch_adapter = CodexAdapter::new(arch_only.path().to_path_buf());
        let arch_res = arch_adapter.scan(scan_now());
        assert!(arch_res.found_root);
        assert_eq!(arch_res.sessions.len(), 1);
        assert_eq!(arch_res.sessions[0].conversation_id, "only");
    }

    #[test]
    fn test_incremental_cache_hit_reparse_tailing_and_eviction() {
        let root = TempRoot::new("cache");
        let p1 = root.write(
            "sessions/2026/11/24/rollout-c1.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:00:00Z","type":"session_meta","payload":{"id":"c1","cwd":"/r1"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"cached_input_tokens":0,"output_tokens":50}}}}"#, "\n",
                // Provisional trailing tool before turn 2 arrives
                r#"{"timestamp":"2026-11-24T10:00:10Z","type":"response_item","payload":{"type":"function_call","name":"exec_command"}}"#, "\n",
            ),
        );
        let p2 = root.write(
            "archived_sessions/rollout-c2.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T09:00:00Z","type":"session_meta","payload":{"id":"c2","cwd":"/r2"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T09:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":300,"cached_input_tokens":0,"output_tokens":30}}}}"#, "\n",
            ),
        );

        let mut adapter = CodexAdapter::new(root.path().to_path_buf());

        let r1 = adapter.scan(scan_now());
        assert_eq!(r1.files_reparsed, 2);
        assert_eq!(r1.files_cached, 0);
        assert_eq!(r1.sessions.len(), 2);
        let c1_init = r1.sessions.iter().find(|s| s.conversation_id == "c1").unwrap();
        assert_eq!(c1_init.turns[0].tool_names.len(), 1); // provisionally attached

        let r2 = adapter.scan(scan_now());
        assert_eq!(r2.files_reparsed, 0);
        assert_eq!(r2.files_cached, 2);
        assert_eq!(r2.sessions.len(), 2);

        // Append turn 2 -> byte-offset tailing restores provisional tool and attaches it to turn 2!
        let mut updated = fs::read_to_string(&p1).unwrap();
        updated.push_str(
            r#"{"timestamp":"2026-11-24T10:01:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1200,"cached_input_tokens":500,"output_tokens":120}}}}"#,
        );
        updated.push('\n');
        fs::write(&p1, updated).unwrap();

        let r3 = adapter.scan(scan_now());
        assert_eq!(r3.files_reparsed, 1);
        assert_eq!(r3.files_cached, 1);
        let c1 = r3.sessions.iter().find(|s| s.conversation_id == "c1").unwrap();
        assert_eq!(c1.turns.len(), 2);
        assert!(c1.turns[0].tool_names.is_empty());
        let t1_tools: Vec<&str> = c1.turns[1].tool_names.iter().map(AsRef::as_ref).collect();
        assert_eq!(t1_tools, vec!["exec_command"]);

        fs::remove_file(&p2).unwrap();
        let r4 = adapter.scan(scan_now());
        assert_eq!(r4.files_reparsed, 0);
        assert_eq!(r4.files_cached, 1);
        assert_eq!(r4.sessions.len(), 1);
        assert!(!adapter.rollout_cache.contains_key(&p2));
    }

    #[test]
    fn test_malformed_lines_and_filename_uuid_fallback() {
        let root = TempRoot::new("malformed");
        root.write(
            "sessions/2026/11/24/rollout-2026-11-24T10-00-00-8c1095e2-1558-4a06-bfde-446204f978aa.jsonl",
            concat!(
                "not valid json\n",
                "{\"timestamp\":\"2026-11-24T10:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":500,\"cached_input_tokens\":100,\"output_tokens\":50}}}}\n",
                "{\"truncated\":\n",
            ),
        );

        let mut adapter = CodexAdapter::new(root.path().to_path_buf());
        let res = adapter.scan(scan_now());
        assert_eq!(res.skipped_lines, 2);
        assert_eq!(res.sessions.len(), 1);
        assert_eq!(
            res.sessions[0].conversation_id,
            "8c1095e2-1558-4a06-bfde-446204f978aa"
        );
        assert_eq!(res.sessions[0].workspace, "(未知工作区)");
        assert_eq!(&*res.sessions[0].turns[0].model, "unknown");
    }
}
