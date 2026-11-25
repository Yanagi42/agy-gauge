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
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::pricing::CostBreakdown;
use crate::sources::{
    clean_single_line, compute_running_hint, is_uuid_like, parse_timestamp_utc, val_to_u64,
    FileSig, ParsedSession, SafeFs, SourceAdapter, SourceScanResult,
};
use crate::stats::{SourceKind, TurnRecord};

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
    session_id: String,
    workspace: String,
    first_user_message: String,
    is_subagent: bool,
    parent_id: Option<String>,
    subagent_nickname: String,
    subagent_role: String,
    turns: Vec<TurnRecord>,
    last_step_running: bool,
    skipped_lines: usize,
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
}

impl CodexAdapter {
    pub fn new(root: PathBuf) -> Self {
        Self {
            safe_fs: SafeFs::new(root),
            rollout_cache: HashMap::new(),
            index_sig: None,
            index_cache: HashMap::new(),
            session_hints: HashMap::new(),
        }
    }

    /// Refreshes `<root>/session_index.jsonl` strictly via `SafeFs` when changed.
    /// Returns the path when present and verified so callers can retain it in `seen_paths`.
    fn refresh_index_cache(&mut self) -> Option<PathBuf> {
        let path = self.safe_fs.root().join("session_index.jsonl");
        let Some(sig) = self.safe_fs.is_safe_regular_file(&path) else {
            self.index_sig = None;
            self.index_cache.clear();
            return None;
        };
        if self.index_sig == Some(sig) {
            return Some(path);
        }
        if let Some((fresh_sig, fh)) = self.safe_fs.open_safe_regular_file(&path) {
            self.index_cache = parse_session_index(fh);
            self.index_sig = Some(fresh_sig);
            Some(path)
        } else {
            self.index_sig = None;
            self.index_cache.clear();
            None
        }
    }

    /// Collects strictly whitelisted rollout file paths under `sessions/YYYY/MM/DD/` and `archived_sessions/`.
    /// Never enumerates `<root>` directly, avoiding any touch on `*.sqlite*`, `auth.json`, `config.toml`, `history.jsonl`.
    fn collect_rollout_paths(&self, has_sessions: bool, has_archived: bool) -> Vec<PathBuf> {
        let mut out = Vec::new();

        if has_sessions {
            let sessions_dir = self.safe_fs.root().join("sessions");
            for year_dir in self.safe_digit_subdirs(&sessions_dir, 4) {
                for month_dir in self.safe_digit_subdirs(&year_dir, 2) {
                    for day_dir in self.safe_digit_subdirs(&month_dir, 2) {
                        collect_rollout_files_in_dir(&day_dir, &mut out);
                    }
                }
            }
        }

        if has_archived {
            let archived_dir = self.safe_fs.root().join("archived_sessions");
            collect_rollout_files_in_dir(&archived_dir, &mut out);
        }

        out
    }

    fn safe_digit_subdirs(&self, parent: &Path, expected_len: usize) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(parent) else {
            return Vec::new();
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let name_str = name.to_str()?;
                if !is_ascii_digits_len(name_str, expected_len) {
                    return None;
                }
                let path = entry.path();
                self.safe_fs.is_safe_dir(&path).then_some(path)
            })
            .collect();
        dirs.sort();
        dirs
    }

    fn resolve_title(&self, cached: &CachedRollout) -> String {
        // Priority: session_index.jsonl (thread_name / title by id)
        //         > subagent agent_nickname / agent_role
        //         > first user_message
        //         > session_id
        if let Some(idx) = self
            .index_cache
            .get(&cached.session_id)
            .filter(|e| !e.title.is_empty())
        {
            return idx.title.clone();
        }
        let subagent_title = format_subagent_title(&cached.subagent_nickname, &cached.subagent_role);
        if !subagent_title.is_empty() {
            return subagent_title;
        }
        if !cached.first_user_message.is_empty() {
            return cached.first_user_message.clone();
        }
        cached.session_id.clone()
    }
}

impl SourceAdapter for CodexAdapter {
    fn kind(&self) -> SourceKind {
        SourceKind::Codex
    }

    fn root(&self) -> &Path {
        self.safe_fs.root()
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
        self.safe_fs.refresh_root();
        let mut res = SourceScanResult::default();

        let sessions_dir = self.safe_fs.root().join("sessions");
        let archived_dir = self.safe_fs.root().join("archived_sessions");
        let has_sessions = self.safe_fs.is_safe_dir(&sessions_dir);
        let has_archived = self.safe_fs.is_safe_dir(&archived_dir);

        if !has_sessions && !has_archived {
            self.rollout_cache.clear();
            self.session_hints.clear();
            self.index_sig = None;
            self.index_cache.clear();
            self.safe_fs.retain_verified(&BTreeSet::new());
            return res;
        }
        res.found_root = true;

        let mut seen_paths = BTreeSet::new();
        if let Some(idx_path) = self.refresh_index_cache() {
            seen_paths.insert(idx_path);
        }

        let rollout_paths = self.collect_rollout_paths(has_sessions, has_archived);
        self.session_hints.clear();

        for path in rollout_paths {
            let Some(sig) = self.safe_fs.is_safe_regular_file(&path) else {
                continue;
            };
            seen_paths.insert(path.clone());

            let cached = match self.rollout_cache.get(&path) {
                Some(entry) if entry.sig == sig => {
                    res.files_cached += 1;
                    entry.clone()
                }
                _ => {
                    let Some((fresh_sig, fh)) = self.safe_fs.open_safe_regular_file(&path) else {
                        continue;
                    };
                    let fresh = parse_rollout_file(fh, &path, fresh_sig);
                    res.files_reparsed += 1;
                    self.rollout_cache.insert(path.clone(), fresh.clone());
                    fresh
                }
            };

            res.skipped_lines += cached.skipped_lines;

            let title = self.resolve_title(&cached);
            let raw_workspace = if !cached.workspace.is_empty() {
                cached.workspace.clone()
            } else {
                self.index_cache
                    .get(&cached.session_id)
                    .map(|e| e.workspace.clone())
                    .unwrap_or_default()
            };

            // Record hint even for 0-turn rollouts so subagent parent lookup can resolve title/cwd.
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
                res.skipped_empty += 1;
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
                    .clone()
                    .filter(|p| !p.is_empty() && p != &cached.session_id)
            } else {
                None
            };
            let last_file_mtime_utc = cached.sig.mtime_utc();
            let is_running_hint =
                compute_running_hint(cached.last_step_running, last_file_mtime_utc, scan_now_utc);

            res.sessions.push(ParsedSession {
                conversation_id: cached.session_id,
                title,
                workspace,
                source: SourceKind::Codex,
                is_subagent: cached.is_subagent,
                parent_id,
                parent_title: String::new(),
                turns: cached.turns,
                last_file_mtime_utc,
                is_running_hint,
            });
        }

        self.rollout_cache.retain(|k, _| seen_paths.contains(k));
        self.safe_fs.retain_verified(&seen_paths);
        res
    }
}

fn is_ascii_digits_len(s: &str, expected_len: usize) -> bool {
    s.len() == expected_len && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_rollout_filename(name: &str) -> bool {
    name.starts_with("rollout-") && name.ends_with(".jsonl")
}

fn collect_rollout_files_in_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name_str = name.to_str()?;
            is_rollout_filename(name_str).then(|| entry.path())
        })
        .collect();
    files.sort();
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
    let mut line = String::new();
    loop {
        line.clear();
        let Ok(n) = reader.read_line(&mut line) else {
            break;
        };
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
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

fn maybe_relevant_rollout_line(trimmed: &str) -> bool {
    if !(trimmed.starts_with('{') && trimmed.ends_with('}') && trimmed.contains("\"type\"")) {
        // Malformed or truncated line: parse with serde_json so skipped_lines stays accurate.
        return true;
    }
    const WATCHED: &[&str] = &[
        "\"session_meta\"",
        "\"turn_context\"",
        "\"token_count\"",
        "\"user_message\"",
        "\"agent_reasoning\"",
        "\"task_started\"",
        "\"task_complete\"",
        "\"turn_aborted\"",
        "\"function_call\"",
        "\"custom_tool_call\"",
        "\"local_shell_call\"",
        "\"web_search_call\"",
        "\"mcp_tool_call\"",
        "\"reasoning\"",
    ];
    WATCHED.iter().any(|kw| trimmed.contains(kw))
}

fn parse_raw_usage(val: Option<&Value>) -> Option<RawUsage> {
    let obj = val?.as_object()?;
    Some(RawUsage {
        input_tokens: val_to_u64(obj.get("input_tokens")),
        cached_input_tokens: val_to_u64(
            obj.get("cached_input_tokens")
                .or_else(|| obj.get("cache_read_input_tokens")),
        ),
        output_tokens: val_to_u64(obj.get("output_tokens")),
        reasoning_output_tokens: val_to_u64(obj.get("reasoning_output_tokens")),
        total_tokens: val_to_u64(obj.get("total_tokens")),
    })
}

fn extract_user_message_text(payload: &Map<String, Value>) -> Option<String> {
    for key in ["message", "text", "content"] {
        match payload.get(key) {
            Some(Value::String(s)) => {
                let cleaned = clean_single_line(s, 100);
                if !cleaned.is_empty() {
                    return Some(cleaned);
                }
            }
            Some(Value::Array(parts)) => {
                let mut joined = String::new();
                for part in parts {
                    if let Some(t) = part
                        .as_object()
                        .and_then(|o| o.get("text"))
                        .and_then(Value::as_str)
                    {
                        if !joined.is_empty() {
                            joined.push(' ');
                        }
                        joined.push_str(t);
                    }
                }
                let cleaned = clean_single_line(&joined, 100);
                if !cleaned.is_empty() {
                    return Some(cleaned);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_rollout_file(fh: File, path: &Path, sig: FileSig) -> CachedRollout {
    let mut reader = BufReader::with_capacity(64 * 1024, fh);
    let mut line = String::new();

    let mut session_id = String::new();
    let mut session_cwd = String::new();
    let mut turn_cwd = String::new();
    let mut current_model: Option<String> = None;
    let mut first_user_message = String::new();

    let mut is_subagent = false;
    let mut parent_id: Option<String> = None;
    let mut subagent_nickname = String::new();
    let mut subagent_role = String::new();

    let mut pending_tools: Vec<String> = Vec::new();
    let mut pending_reasoning = false;
    let mut prev_total: Option<RawUsage> = None;
    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut next_step_index: i64 = 0;
    let mut last_step_running = false;
    let mut skipped_lines = 0usize;

    loop {
        line.clear();
        let Ok(n) = reader.read_line(&mut line) else {
            break;
        };
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !maybe_relevant_rollout_line(trimmed) {
            continue;
        }
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) else {
            skipped_lines += 1;
            continue;
        };

        let record_type = map.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(payload) = map.get("payload").and_then(Value::as_object) else {
            continue;
        };

        match record_type {
            "session_meta" => {
                if let Some(id) = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    session_id = id.to_string();
                }
                if let Some(cwd) = payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    session_cwd = cwd.to_string();
                }
                if current_model.is_none() {
                    if let Some(m) = payload
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        current_model = Some(m.to_string());
                    }
                }
                if let Some(source) = payload.get("source").and_then(Value::as_object) {
                    if let Some(subagent) = source.get("subagent") {
                        is_subagent = true;
                        if let Some(sub_obj) = subagent.as_object() {
                            if let Some(spawn) =
                                sub_obj.get("thread_spawn").and_then(Value::as_object)
                            {
                                if let Some(pid) = spawn
                                    .get("parent_thread_id")
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                                {
                                    parent_id = Some(pid.to_string());
                                }
                                if let Some(nick) = spawn
                                    .get("agent_nickname")
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                                {
                                    subagent_nickname = nick.to_string();
                                }
                                if let Some(role) = spawn
                                    .get("agent_role")
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                                {
                                    subagent_role = role.to_string();
                                }
                            }
                        } else if let Some(kind_str) = subagent
                            .as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            if subagent_role.is_empty() {
                                subagent_role = kind_str.to_string();
                            }
                        }
                    }
                }
                // Defensive fallback if nickname/role also appear directly on session_meta payload.
                if subagent_nickname.is_empty() {
                    if let Some(nick) = payload
                        .get("agent_nickname")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        subagent_nickname = nick.to_string();
                    }
                }
                if subagent_role.is_empty() {
                    if let Some(role) = payload
                        .get("agent_role")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        subagent_role = role.to_string();
                    }
                }
            }

            "turn_context" => {
                if let Some(model) = payload
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        payload
                            .get("collaboration_mode")
                            .and_then(Value::as_object)
                            .and_then(|c| c.get("settings"))
                            .and_then(Value::as_object)
                            .and_then(|s| s.get("model"))
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                    })
                {
                    current_model = Some(model.to_string());
                }
                if let Some(cwd) = payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    turn_cwd = cwd.to_string();
                }
            }

            "response_item" => {
                let item_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
                match item_type {
                    "function_call"
                    | "custom_tool_call"
                    | "local_shell_call"
                    | "web_search_call"
                    | "mcp_tool_call" => {
                        let tool_name = payload
                            .get("name")
                            .or_else(|| payload.get("tool_name"))
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(item_type);
                        pending_tools.push(tool_name.to_string());
                    }
                    "reasoning" => {
                        pending_reasoning = true;
                    }
                    _ => {}
                }
            }

            "event_msg" => {
                let ev_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
                match ev_type {
                    "user_message" => {
                        if first_user_message.is_empty() {
                            if let Some(msg) = extract_user_message_text(payload) {
                                first_user_message = msg;
                            }
                        }
                    }
                    "agent_reasoning" => {
                        pending_reasoning = true;
                    }
                    "task_started" => {
                        last_step_running = true;
                    }
                    "task_complete" | "turn_aborted" => {
                        last_step_running = false;
                    }
                    "token_count" => {
                        let Some(info) = payload.get("info").and_then(Value::as_object) else {
                            // Rate-limit-only or status event with `info: null` -> skip.
                            continue;
                        };
                        let curr_total = parse_raw_usage(info.get("total_token_usage"));
                        let last_usage = parse_raw_usage(info.get("last_token_usage"));

                        let raw_turn = match (curr_total, prev_total) {
                            (Some(curr), Some(prev)) => {
                                if curr.same_counters(prev) {
                                    // Duplicate cumulative snapshot -> skip.
                                    continue;
                                } else if curr.monotonic_ge(prev) {
                                    prev_total = Some(curr);
                                    curr.delta_from(prev)
                                } else {
                                    // Cumulative counter reset smaller -> update baseline & fall back to last_token_usage.
                                    prev_total = Some(curr);
                                    match last_usage {
                                        Some(last) => last,
                                        None => curr,
                                    }
                                }
                            }
                            (Some(curr), None) => {
                                prev_total = Some(curr);
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

                        // OpenAI inclusive input & inclusive output math:
                        //   cache_read = min(cached, input)
                        //   uncached_input = input.saturating_sub(cached)
                        //   output = output (already includes reasoning)
                        //   reasoning = min(reasoning_output, output) (informational)
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

                        let saw_reasoning = std::mem::take(&mut pending_reasoning);
                        let tool_names = std::mem::take(&mut pending_tools);
                        let thinking_chars = if saw_reasoning && reasoning_tokens == 0 {
                            1
                        } else {
                            0
                        };

                        let timestamp_utc = parse_timestamp_utc(map.get("timestamp"));
                        let model = current_model
                            .clone()
                            .or_else(|| {
                                info.get("model")
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                                    .map(String::from)
                            })
                            .unwrap_or_else(|| "unknown".to_string());

                        turns.push(TurnRecord {
                            step_index: next_step_index,
                            timestamp_utc,
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
                        next_step_index += 1;
                    }
                    _ => {}
                }
            }

            _ => {}
        }
    }

    if session_id.is_empty() {
        session_id = session_id_from_filename(path);
    }
    let workspace = if !session_cwd.is_empty() {
        session_cwd
    } else {
        turn_cwd
    };

    CachedRollout {
        sig,
        session_id,
        workspace,
        first_user_message,
        is_subagent,
        parent_id,
        subagent_nickname,
        subagent_role,
        turns,
        last_step_running,
        skipped_lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
                // Edge case: cached > input and reasoning > output via last_token_usage fallback (missing total_token_usage)
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
        assert_eq!(t0.model, "gpt-5.3-codex");
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
                // Rate-limit-only null info event: must skip
                r#"{"timestamp":"2026-11-24T10:00:02Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"primary":{"used_percent":12}}}}"#, "\n",
                // Turn 1
                r#"{"timestamp":"2026-11-24T10:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2000,"cached_input_tokens":500,"output_tokens":100,"reasoning_output_tokens":0,"total_tokens":2100},"last_token_usage":{"input_tokens":2000,"cached_input_tokens":500,"output_tokens":100,"reasoning_output_tokens":0,"total_tokens":2100}}}}"#, "\n",
                // Exact duplicate of Turn 1: must skip
                r#"{"timestamp":"2026-11-24T10:00:06Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2000,"cached_input_tokens":500,"output_tokens":100,"reasoning_output_tokens":0,"total_tokens":2100},"last_token_usage":{"input_tokens":2000,"cached_input_tokens":500,"output_tokens":100,"reasoning_output_tokens":0,"total_tokens":2100}}}}"#, "\n",
                // Buffered tool + null info event before Turn 2: tool must survive null & dup skips
                r#"{"timestamp":"2026-11-24T10:00:07Z","type":"response_item","payload":{"type":"function_call","name":"shell_command"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:08Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#, "\n",
                // Turn 2 monotonic advance: delta raw_in=3000, raw_cached=2500 -> uncached=500, cr=2500, out=150, reas=40
                r#"{"timestamp":"2026-11-24T10:00:12Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":5000,"cached_input_tokens":3000,"output_tokens":250,"reasoning_output_tokens":40,"total_tokens":5250},"last_token_usage":{"input_tokens":3000,"cached_input_tokens":2500,"output_tokens":150,"reasoning_output_tokens":40,"total_tokens":3150}}}}"#, "\n",
                // Duplicate of Turn 2: must skip
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
        assert_eq!(turns[1].tool_names, vec!["shell_command"]);

        // Sum of emitted turns matches final cumulative total_token_usage exactly.
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
                // Turn 1 baseline
                r#"{"timestamp":"2026-11-24T10:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":4000,"cached_input_tokens":2000,"output_tokens":300,"reasoning_output_tokens":50},"last_token_usage":{"input_tokens":4000,"cached_input_tokens":2000,"output_tokens":300,"reasoning_output_tokens":50}}}}"#, "\n",
                // Turn 2: total_token_usage resets smaller -> fall back to last_token_usage and re-anchor prev_total
                r#"{"timestamp":"2026-11-24T10:00:10Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":120,"reasoning_output_tokens":20},"last_token_usage":{"input_tokens":900,"cached_input_tokens":300,"output_tokens":100,"reasoning_output_tokens":15}}}}"#, "\n",
                // Duplicate of Turn 2 after reset: must be skipped against re-anchored prev_total
                r#"{"timestamp":"2026-11-24T10:00:11Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":120,"reasoning_output_tokens":20},"last_token_usage":{"input_tokens":900,"cached_input_tokens":300,"output_tokens":100,"reasoning_output_tokens":15}}}}"#, "\n",
                // Turn 3: monotonic advance from re-anchored total {1000,400,120,20} -> delta {1500,1000,100,10}
                r#"{"timestamp":"2026-11-24T10:00:15Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2500,"cached_input_tokens":1400,"output_tokens":220,"reasoning_output_tokens":30},"last_token_usage":{"input_tokens":1500,"cached_input_tokens":1000,"output_tokens":100,"reasoning_output_tokens":10}}}}"#, "\n",
                // Turn 4: total_token_usage absent -> fall back to last_token_usage
                r#"{"timestamp":"2026-11-24T10:00:20Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":800,"cached_input_tokens":200,"output_tokens":50,"reasoning_output_tokens":0}}}}"#, "\n",
                // Zero-token fallback -> must skip
                r#"{"timestamp":"2026-11-24T10:00:25Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0}}}}"#, "\n",
            ),
        );

        let mut adapter = CodexAdapter::new(root.path().to_path_buf());
        let res = adapter.scan(scan_now());
        assert_eq!(res.sessions.len(), 1);
        let turns = &res.sessions[0].turns;
        assert_eq!(turns.len(), 4);

        // Turn 1
        assert_eq!((turns[0].input_tokens, turns[0].cache_read_tokens, turns[0].output_tokens, turns[0].reasoning_tokens), (2000, 2000, 300, 50));
        // Turn 2 (reset -> last_token_usage)
        assert_eq!((turns[1].input_tokens, turns[1].cache_read_tokens, turns[1].output_tokens, turns[1].reasoning_tokens), (600, 300, 100, 15));
        // Turn 3 (monotonic after reset)
        assert_eq!((turns[2].input_tokens, turns[2].cache_read_tokens, turns[2].output_tokens, turns[2].reasoning_tokens), (500, 1000, 100, 10));
        // Turn 4 (missing total -> last_token_usage)
        assert_eq!((turns[3].input_tokens, turns[3].cache_read_tokens, turns[3].output_tokens, turns[3].reasoning_tokens), (600, 200, 50, 0));
    }

    #[test]
    fn test_turn_context_model_cwd_tools_and_reasoning_attachment() {
        let root = TempRoot::new("tools");
        root.write(
            "sessions/2026/11/24/rollout-tools.jsonl",
            concat!(
                // Empty session_meta.cwd -> falls back to turn_context.cwd
                r#"{"timestamp":"2026-11-24T10:00:00Z","type":"session_meta","payload":{"id":"sess-tools","cwd":"","source":"cli"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:01Z","type":"turn_context","payload":{"model":"gpt-5.2-codex","cwd":"C:\\work\\fallback-cwd"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:02Z","type":"response_item","payload":{"type":"reasoning","summary":[{"text":"thinking"}]}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:03Z","type":"response_item","payload":{"type":"function_call","name":"exec_command"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:03Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:04Z","type":"response_item","payload":{"type":"local_shell_call"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:04Z","type":"response_item","payload":{"type":"web_search_call"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:04Z","type":"response_item","payload":{"type":"mcp_tool_call","name":"mcp__docs__search"}}"#, "\n",
                // Tool output line must be ignored cleanly
                r#"{"timestamp":"2026-11-24T10:00:05Z","type":"response_item","payload":{"type":"function_call_output","output":"ok"}}"#, "\n",
                // Turn 1 with 0 reasoning_output_tokens, but response_item reasoning seen -> thinking_chars = 1
                r#"{"timestamp":"2026-11-24T10:00:06Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":0,"output_tokens":100,"reasoning_output_tokens":0}}}}"#, "\n",
                // Switch model for Turn 2 + event_msg agent_reasoning flag (also 0 reasoning_output_tokens)
                r#"{"timestamp":"2026-11-24T10:00:07Z","type":"turn_context","payload":{"model":"gpt-5.3-codex","cwd":"C:\\work\\fallback-cwd"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:08Z","type":"event_msg","payload":{"type":"agent_reasoning","text":"checking diff"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:10Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2000,"cached_input_tokens":500,"output_tokens":200,"reasoning_output_tokens":0}}}}"#, "\n",
                // Turn 3 with neither reasoning item nor reasoning tokens -> has_thinking() == false
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

        assert_eq!(sess.turns[0].model, "gpt-5.2-codex");
        assert_eq!(
            sess.turns[0].tool_names,
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

        assert_eq!(sess.turns[1].model, "gpt-5.3-codex");
        assert!(sess.turns[1].tool_names.is_empty());
        assert_eq!(sess.turns[1].thinking_chars, 1);
        assert!(sess.turns[1].has_thinking());

        assert_eq!(sess.turns[2].model, "gpt-5.3-codex");
        assert!(sess.turns[2].tool_names.is_empty());
        assert_eq!(sess.turns[2].thinking_chars, 0);
        assert!(!sess.turns[2].has_thinking());
    }

    #[test]
    fn test_subagent_thread_spawn_title_priority_and_lookup_hint() {
        let root = TempRoot::new("subagent");
        // session_index.jsonl with rename override for s-indexed and entry for zero-turn parent
        root.write(
            "session_index.jsonl",
            concat!(
                r#"{"id":"s-indexed","thread_name":"Old Name"}"#, "\n",
                r#"{"id":"s-indexed","thread_name":"Final Indexed Title"}"#, "\n",
                r#"{"id":"parent-0turn","title":"Parent From Index"}"#, "\n",
            ),
        );
        // 0-turn parent rollout (provides cwd for lookup_session_hint)
        root.write(
            "sessions/2026/11/24/rollout-parent.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:00:00Z","type":"session_meta","payload":{"id":"parent-0turn","cwd":"C:\\work\\parent-repo","source":"cli"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Parent prompt"}}"#, "\n",
            ),
        );
        // 1. Subagent whose title is overridden by session_index.jsonl
        root.write(
            "sessions/2026/11/24/rollout-s1.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:01:00Z","type":"session_meta","payload":{"id":"s-indexed","cwd":"C:\\work\\repo","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-0turn","depth":1,"agent_nickname":"Scout","agent_role":"explorer"}}}}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:01:01Z","type":"event_msg","payload":{"type":"user_message","message":"Inspect files"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:01:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"cached_input_tokens":0,"output_tokens":50}}}}"#, "\n",
            ),
        );
        // 2. Subagent titled by agent_nickname/agent_role over user_message
        root.write(
            "sessions/2026/11/24/rollout-s2.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:02:00Z","type":"session_meta","payload":{"id":"s-subagent","cwd":"","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-0turn","depth":1,"agent_nickname":"Scout","agent_role":"explorer"}}}}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:02:01Z","type":"event_msg","payload":{"type":"user_message","message":"Search logs"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:02:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":400,"cached_input_tokens":0,"output_tokens":40}}}}"#, "\n",
            ),
        );
        // 3. Normal session titled by first user_message
        root.write(
            "sessions/2026/11/24/rollout-s3.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:03:00Z","type":"session_meta","payload":{"id":"s-usermsg","cwd":"/repo","source":"cli"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:03:01Z","type":"event_msg","payload":{"type":"user_message","message":"  Fix\n  build   errors  "}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:03:02Z","type":"event_msg","payload":{"type":"user_message","message":"Second message ignored"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:03:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":300,"cached_input_tokens":0,"output_tokens":30}}}}"#, "\n",
            ),
        );
        // 4. Session with no index/subagent/user_message -> falls back to session_id
        root.write(
            "sessions/2026/11/24/rollout-s4.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:04:00Z","type":"session_meta","payload":{"id":"s-fallback-id","cwd":"/repo","source":"cli"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:04:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":200,"cached_input_tokens":0,"output_tokens":20}}}}"#, "\n",
            ),
        );

        let mut adapter = CodexAdapter::new(root.path().to_path_buf());
        let res = adapter.scan(scan_now());
        assert_eq!(res.skipped_empty, 1); // parent-0turn has 0 turns
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

        // lookup_session_hint merges index title + 0-turn rollout cwd
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
        // Live session
        root.write(
            "sessions/2026/11/24/rollout-live.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:00:00Z","type":"session_meta","payload":{"id":"live-1","cwd":"/live"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":200,"output_tokens":50}}}}"#, "\n",
            ),
        );
        // Archived session
        root.write(
            "archived_sessions/rollout-old.jsonl",
            concat!(
                r#"{"timestamp":"2026-10-01T08:00:00Z","type":"session_meta","payload":{"id":"arch-1","cwd":"/arch"}}"#, "\n",
                r#"{"timestamp":"2026-10-01T08:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":600,"cached_input_tokens":100,"output_tokens":30}}}}"#, "\n",
            ),
        );
        // Forbidden files & non-whitelisted paths (contain invalid JSON / binary bytes to prove they are NEVER opened)
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

        // Also verify archived_sessions-only root sets found_root == true.
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
    fn test_incremental_cache_hit_reparse_and_eviction() {
        let root = TempRoot::new("cache");
        let p1 = root.write(
            "sessions/2026/11/24/rollout-c1.jsonl",
            concat!(
                r#"{"timestamp":"2026-11-24T10:00:00Z","type":"session_meta","payload":{"id":"c1","cwd":"/r1"}}"#, "\n",
                r#"{"timestamp":"2026-11-24T10:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"cached_input_tokens":0,"output_tokens":50}}}}"#, "\n",
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

        // Scan 1: cold parse
        let r1 = adapter.scan(scan_now());
        assert_eq!(r1.files_reparsed, 2);
        assert_eq!(r1.files_cached, 0);
        assert_eq!(r1.sessions.len(), 2);

        // Scan 2: warm incremental cache hit
        let r2 = adapter.scan(scan_now());
        assert_eq!(r2.files_reparsed, 0);
        assert_eq!(r2.files_cached, 2);
        assert_eq!(r2.sessions.len(), 2);

        // Append a second turn to p1 -> only p1 reparsed
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

        // Delete p2 -> evicted from rollout_cache & SafeFs verified_paths
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
        assert_eq!(res.sessions[0].turns[0].model, "unknown");
    }
}
