//! Strictly read-only incremental adapter for Gemini CLI JSON & JSONL chat logs.
//!
//! Whitelisted files ONLY (all checks & reads guarded by `SafeFs`):
//!   - `<root>/tmp/<proj>/chats/session-*.json` and `session-*.jsonl`
//!     (or `<root>/<proj>/chats/session-*.{json,jsonl}` when `<root>` itself is named `tmp`)
//!   - Optional workspace lookups: `<tmp>/<proj>/.project_root` and `<gemini_root>/projects.json`
//!
//! NEVER reads `oauth_creds.json`, `google_accounts.json`, `jetski-standalone-oauth-token`,
//! `settings.json`, or `antigravity*`. Never opens SQLite databases. Never writes to disk.

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use crate::pricing::CostBreakdown;
use crate::sources::{
    clean_single_line, compute_running_hint, decode_file_uri, parse_timestamp_utc, val_to_u64,
    FileSig, ParsedSession, SafeFs, SourceAdapter, SourceScanResult,
};
use crate::stats::{SourceKind, TurnRecord};

const MAX_PROJECT_ROOT_BYTES: usize = 4 * 1024;
const MAX_PROJECTS_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_SESSION_JSON_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone)]
struct CachedSessionFile {
    sig: FileSig,
    conversation_id: String,
    project_hash: String,
    summary: String,
    first_user_preview: String,
    is_subagent: bool,
    parent_id: Option<String>,
    turns: Vec<TurnRecord>,
    last_step_running: bool,
    skipped_lines: usize,
}

pub struct GeminiCliAdapter {
    safe_fs: SafeFs,
    /// When `root` itself is a `tmp` directory (e.g. `~/.gemini/tmp`), allows strictly checking
    /// `<parent>/projects.json` via its own `SafeFs` without touching any other sibling file.
    parent_safe_fs: Option<SafeFs>,
    root_is_tmp: bool,
    session_cache: HashMap<PathBuf, CachedSessionFile>,
    project_root_cache: HashMap<PathBuf, (FileSig, String)>,
    projects_json_sig: Option<FileSig>,
    projects_lookup: HashMap<String, String>,
    session_hints: HashMap<String, (String, String)>,
}

impl GeminiCliAdapter {
    pub fn new(root: PathBuf) -> Self {
        let root_is_tmp = root
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("tmp"))
            .unwrap_or(false);
        let parent_safe_fs = if root_is_tmp {
            root.parent().map(|p| SafeFs::new(p.to_path_buf()))
        } else {
            None
        };
        Self {
            safe_fs: SafeFs::new(root),
            parent_safe_fs,
            root_is_tmp,
            session_cache: HashMap::new(),
            project_root_cache: HashMap::new(),
            projects_json_sig: None,
            projects_lookup: HashMap::new(),
            session_hints: HashMap::new(),
        }
    }

    fn tmp_dir(&self) -> PathBuf {
        if self.root_is_tmp {
            self.safe_fs.root().to_path_buf()
        } else {
            self.safe_fs.root().join("tmp")
        }
    }

    fn refresh_projects_json(&mut self, seen_paths: &mut BTreeSet<PathBuf>) {
        if !self.root_is_tmp {
            let path = self.safe_fs.root().join("projects.json");
            let Some(sig) = self.safe_fs.is_safe_regular_file(&path) else {
                self.projects_json_sig = None;
                self.projects_lookup.clear();
                return;
            };
            seen_paths.insert(path.clone());
            if self.projects_json_sig == Some(sig) {
                return;
            }
            if let Some((fresh_sig, text)) = self
                .safe_fs
                .read_to_string_limited(&path, MAX_PROJECTS_JSON_BYTES)
            {
                self.projects_lookup = parse_projects_json(&text);
                self.projects_json_sig = Some(fresh_sig);
            }
            return;
        }

        let mut parent_seen = BTreeSet::new();
        if let Some(parent_fs) = self.parent_safe_fs.as_mut() {
            parent_fs.refresh_root();
            let path = parent_fs.root().join("projects.json");
            if let Some(sig) = parent_fs.is_safe_regular_file(&path) {
                parent_seen.insert(path.clone());
                if self.projects_json_sig != Some(sig) {
                    if let Some((fresh_sig, text)) =
                        parent_fs.read_to_string_limited(&path, MAX_PROJECTS_JSON_BYTES)
                    {
                        self.projects_lookup = parse_projects_json(&text);
                        self.projects_json_sig = Some(fresh_sig);
                    }
                }
            } else {
                self.projects_json_sig = None;
                self.projects_lookup.clear();
            }
            parent_fs.retain_verified(&parent_seen);
        }
    }

    fn read_project_root(
        &mut self,
        proj_dir: &Path,
        seen_paths: &mut BTreeSet<PathBuf>,
    ) -> String {
        let path = proj_dir.join(".project_root");
        let Some(sig) = self.safe_fs.is_safe_regular_file(&path) else {
            self.project_root_cache.remove(&path);
            return String::new();
        };
        seen_paths.insert(path.clone());
        if let Some((cached_sig, val)) = self.project_root_cache.get(&path) {
            if *cached_sig == sig {
                return val.clone();
            }
        }
        let workspace = self
            .safe_fs
            .read_to_string_limited(&path, MAX_PROJECT_ROOT_BYTES)
            .map(|(fresh_sig, text)| {
                let ws = normalize_workspace_line(&text);
                self.project_root_cache.insert(path.clone(), (fresh_sig, ws.clone()));
                ws
            })
            .unwrap_or_default();
        workspace
    }

    fn lookup_workspace_in_projects(&self, proj_dir_name: &str, project_hash: &str) -> String {
        for key in [proj_dir_name, project_hash] {
            let trimmed = key.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(ws) = self.projects_lookup.get(trimmed) {
                return ws.clone();
            }
            let lower = trimmed.to_ascii_lowercase();
            if let Some(ws) = self.projects_lookup.get(&lower) {
                return ws.clone();
            }
        }
        String::new()
    }
}

impl SourceAdapter for GeminiCliAdapter {
    fn kind(&self) -> SourceKind {
        SourceKind::GeminiCli
    }

    fn root(&self) -> &Path {
        self.safe_fs.root()
    }

    fn lookup_session_hint(&mut self, cid: &str) -> Option<(String, String)> {
        self.session_hints.get(cid).cloned()
    }

    fn scan(&mut self, scan_now_utc: DateTime<Utc>) -> SourceScanResult {
        self.safe_fs.refresh_root();
        let mut res = SourceScanResult::default();
        let tmp_dir = self.tmp_dir();
        if !self.safe_fs.is_safe_dir(&tmp_dir) {
            self.session_cache.clear();
            self.project_root_cache.clear();
            self.projects_json_sig = None;
            self.projects_lookup.clear();
            self.session_hints.clear();
            self.safe_fs.retain_verified(&BTreeSet::new());
            if let Some(parent_fs) = self.parent_safe_fs.as_mut() {
                parent_fs.retain_verified(&BTreeSet::new());
            }
            return res;
        }
        res.found_root = true;

        let mut seen_paths = BTreeSet::new();
        self.refresh_projects_json(&mut seen_paths);

        let Ok(proj_entries) = fs::read_dir(&tmp_dir) else {
            self.session_cache.clear();
            self.project_root_cache.clear();
            self.session_hints.clear();
            self.safe_fs.retain_verified(&seen_paths);
            return res;
        };

        let mut proj_dirs: Vec<PathBuf> = proj_entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| self.safe_fs.is_safe_dir(p))
            .collect();
        proj_dirs.sort();

        let mut fresh_hints: HashMap<String, (String, String)> = HashMap::new();

        for proj_dir in proj_dirs {
            let chats_dir = proj_dir.join("chats");
            if !self.safe_fs.is_safe_dir(&chats_dir) {
                continue;
            }
            let Ok(chat_entries) = fs::read_dir(&chats_dir) else {
                continue;
            };

            let mut session_files: Vec<PathBuf> = chat_entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| is_whitelisted_session_filename(p))
                .collect();
            session_files.sort();
            if session_files.is_empty() {
                continue;
            }

            let proj_dir_name = proj_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let proj_root_ws = self.read_project_root(&proj_dir, &mut seen_paths);

            for file_path in session_files {
                let Some(sig) = self.safe_fs.is_safe_regular_file(&file_path) else {
                    continue;
                };
                seen_paths.insert(file_path.clone());

                let cached = if let Some(entry) = self.session_cache.get(&file_path) {
                    if entry.sig == sig {
                        res.files_cached += 1;
                        entry.clone()
                    } else {
                        let Some((fresh_sig, fh)) = self.safe_fs.open_safe_regular_file(&file_path)
                        else {
                            continue;
                        };
                        let fresh = parse_session_file(&file_path, fh, fresh_sig);
                        res.files_reparsed += 1;
                        self.session_cache.insert(file_path.clone(), fresh.clone());
                        fresh
                    }
                } else {
                    let Some((fresh_sig, fh)) = self.safe_fs.open_safe_regular_file(&file_path)
                    else {
                        continue;
                    };
                    let fresh = parse_session_file(&file_path, fh, fresh_sig);
                    res.files_reparsed += 1;
                    self.session_cache.insert(file_path.clone(), fresh.clone());
                    fresh
                };

                res.skipped_lines += cached.skipped_lines;

                let workspace = if !proj_root_ws.is_empty() {
                    proj_root_ws.clone()
                } else {
                    self.lookup_workspace_in_projects(&proj_dir_name, &cached.project_hash)
                };

                let title = if !cached.summary.is_empty() {
                    cached.summary.clone()
                } else if !cached.first_user_preview.is_empty() {
                    cached.first_user_preview.clone()
                } else {
                    cached.conversation_id.clone()
                };

                if !cached.conversation_id.is_empty() && (!title.is_empty() || !workspace.is_empty())
                {
                    let replace_hint = match fresh_hints.get(&cached.conversation_id) {
                        None => true,
                        Some((prev_t, prev_ws)) => {
                            (prev_t == &cached.conversation_id && title != cached.conversation_id)
                                || (prev_ws.is_empty() && !workspace.is_empty())
                        }
                    };
                    if replace_hint {
                        fresh_hints.insert(
                            cached.conversation_id.clone(),
                            (title.clone(), workspace.clone()),
                        );
                    }
                }

                if cached.turns.is_empty() {
                    res.skipped_empty += 1;
                    continue;
                }

                let last_file_mtime_utc = cached.sig.mtime_utc();
                let is_running_hint = compute_running_hint(
                    cached.last_step_running,
                    last_file_mtime_utc,
                    scan_now_utc,
                );

                res.sessions.push(ParsedSession {
                    conversation_id: cached.conversation_id,
                    title,
                    workspace,
                    source: SourceKind::GeminiCli,
                    is_subagent: cached.is_subagent,
                    parent_id: cached.parent_id,
                    parent_title: String::new(),
                    turns: cached.turns,
                    last_file_mtime_utc,
                    is_running_hint,
                });
            }
        }

        self.session_cache.retain(|k, _| seen_paths.contains(k));
        self.project_root_cache
            .retain(|k, _| seen_paths.contains(k));
        self.safe_fs.retain_verified(&seen_paths);
        self.session_hints = fresh_hints;
        res
    }
}

fn is_whitelisted_session_filename(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    lower.starts_with("session-") && (lower.ends_with(".json") || lower.ends_with(".jsonl"))
}

fn fallback_session_id(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("gemini-session")
        .to_string()
}

fn parse_session_file(path: &Path, fh: File, sig: FileSig) -> CachedSessionFile {
    let is_jsonl = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("jsonl"))
        .unwrap_or(false);
    if is_jsonl {
        parse_session_jsonl(path, fh, sig)
    } else {
        parse_session_json(path, fh, sig)
    }
}

#[derive(Default)]
struct SessionMetaState {
    session_id: String,
    project_hash: String,
    summary: String,
    default_model: String,
    is_subagent: bool,
    parent_id: Option<String>,
}

impl SessionMetaState {
    fn apply_fields(&mut self, map: &Map<String, Value>) {
        if let Some(id) = map
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.session_id = id.to_string();
        }
        if let Some(hash) = map
            .get("projectHash")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.project_hash = hash.to_string();
        }
        if let Some(val) = map.get("summary") {
            match val {
                Value::String(s) => self.summary = clean_single_line(s, 100),
                Value::Null => self.summary.clear(),
                _ => {}
            }
        }
        if let Some(m) = map
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.default_model = m.to_string();
        }
        if let Some(kind) = map.get("kind").and_then(Value::as_str) {
            self.is_subagent = kind.trim().eq_ignore_ascii_case("subagent");
        }
        if let Some(pid) = map
            .get("parentSessionId")
            .or_else(|| map.get("parentId"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.parent_id = Some(pid.to_string());
        }
    }
}

fn parse_session_json(path: &Path, fh: File, sig: FileSig) -> CachedSessionFile {
    let mut buf = Vec::with_capacity((sig.len as usize).min(MAX_SESSION_JSON_BYTES));
    if fh
        .take(MAX_SESSION_JSON_BYTES as u64)
        .read_to_end(&mut buf)
        .is_err()
    {
        return empty_cached_session(path, sig, 1);
    }
    let raw = String::from_utf8_lossy(&buf);
    let trimmed = raw.trim().trim_start_matches('\u{feff}');
    if trimmed.is_empty() {
        return empty_cached_session(path, sig, 0);
    }
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(trimmed) else {
        return empty_cached_session(path, sig, 1);
    };

    let mut meta = SessionMetaState::default();
    meta.apply_fields(&root);

    let messages: Vec<Map<String, Value>> = root
        .get("messages")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_object().cloned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    finalize_session(path, sig, meta, &messages, 0)
}

fn parse_session_jsonl(path: &Path, fh: File, sig: FileSig) -> CachedSessionFile {
    let mut reader = BufReader::with_capacity(64 * 1024, fh);
    let mut meta = SessionMetaState::default();
    let mut messages: Vec<Map<String, Value>> = Vec::new();
    let mut skipped_lines = 0usize;
    let mut line = String::new();
    let mut first_line = true;

    loop {
        line.clear();
        let Ok(n) = reader.read_line(&mut line) else {
            break;
        };
        if n == 0 {
            break;
        }
        let mut trimmed = line.trim();
        if first_line {
            trimmed = trimmed.trim_start_matches('\u{feff}').trim();
            first_line = false;
        }
        if trimmed.is_empty() {
            continue;
        }
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) else {
            skipped_lines += 1;
            continue;
        };

        let mut handled_mutation = false;

        // 1. `$rewindTo` mutation: exclusive truncation dropping target message and everything after it.
        if let Some(rewind_val) = map.get("$rewindTo") {
            apply_rewind_to(&mut messages, rewind_val);
            handled_mutation = true;
        }

        // 2. `$set` mutation: updates session metadata (or a specific message when outer `id`/`messageId` is given).
        if let Some(Value::Object(patch)) = map.get("$set") {
            let target_msg_id = map
                .get("id")
                .or_else(|| map.get("messageId"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if let Some(mid) = target_msg_id {
                if let Some(existing) = messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.get("id").and_then(Value::as_str).map(str::trim) == Some(mid))
                {
                    for (k, v) in patch {
                        existing.insert(k.clone(), v.clone());
                    }
                }
            } else {
                meta.apply_fields(patch);
                if let Some(arr) = patch.get("messages").and_then(Value::as_array) {
                    messages = arr.iter().filter_map(|v| v.as_object().cloned()).collect();
                }
            }
            handled_mutation = true;
        }

        if handled_mutation {
            continue;
        }

        // 3. Header/metadata record vs appended message record.
        let msg_type = map.get("type").and_then(Value::as_str).map(str::trim);
        let is_explicit_header = map.contains_key("sessionId")
            && matches!(msg_type, None | Some("session") | Some("header") | Some("metadata"));

        if is_explicit_header
            || (msg_type.is_none()
                && (map.contains_key("projectHash")
                    || map.contains_key("startTime")
                    || map.contains_key("summary")))
        {
            meta.apply_fields(&map);
            if let Some(arr) = map.get("messages").and_then(Value::as_array) {
                for item in arr {
                    if let Some(obj) = item.as_object() {
                        messages.push(obj.clone());
                    }
                }
            }
            continue;
        }

        if msg_type.is_some() {
            messages.push(map);
        }
    }

    finalize_session(path, sig, meta, &messages, skipped_lines)
}

fn apply_rewind_to(messages: &mut Vec<Map<String, Value>>, target: &Value) {
    match target {
        Value::String(id) => {
            let id = id.trim();
            if !id.is_empty() {
                if let Some(pos) = messages
                    .iter()
                    .position(|m| m.get("id").and_then(Value::as_str).map(str::trim) == Some(id))
                {
                    messages.truncate(pos);
                }
            }
        }
        Value::Object(obj) => {
            if let Some(id) = obj
                .get("messageId")
                .or_else(|| obj.get("id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if let Some(pos) = messages
                    .iter()
                    .position(|m| m.get("id").and_then(Value::as_str).map(str::trim) == Some(id))
                {
                    messages.truncate(pos);
                }
            }
        }
        Value::Number(n) => {
            if let Some(idx) = n.as_u64() {
                messages.truncate(idx as usize);
            }
        }
        _ => {}
    }
}

fn finalize_session(
    path: &Path,
    sig: FileSig,
    meta: SessionMetaState,
    messages: &[Map<String, Value>],
    skipped_lines: usize,
) -> CachedSessionFile {
    let conversation_id = if meta.session_id.is_empty() {
        fallback_session_id(path)
    } else {
        meta.session_id
    };

    let mut first_user_preview = String::new();
    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut id_to_slot: HashMap<String, usize> = HashMap::new();

    for msg in messages {
        let msg_type = msg
            .get("type")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");

        if msg_type == "user" {
            if first_user_preview.is_empty() {
                let raw = msg
                    .get("displayContent")
                    .map(extract_text_from_value)
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| msg.get("content").map(extract_text_from_value))
                    .unwrap_or_default();
                let cleaned = clean_single_line(&raw, 72);
                if !cleaned.is_empty() {
                    first_user_preview = cleaned;
                }
            }
            continue;
        }

        if msg_type != "gemini" {
            continue;
        }

        let Some(Value::Object(tok)) = msg.get("tokens") else {
            continue;
        };

        let raw_input = val_to_u64(tok.get("input"));
        let raw_output = val_to_u64(tok.get("output"));
        let raw_cached = val_to_u64(tok.get("cached"));
        let raw_thoughts = val_to_u64(tok.get("thoughts"));
        let raw_tool = val_to_u64(tok.get("tool"));

        // CRITICAL Gemini CLI token math (inclusive input, EXCLUSIVE output):
        // - `tokens.input` INCLUDES `tokens.cached` -> subtract cached, add `tool` (`toolUsePromptTokenCount`)
        // - `tokens.output` EXCLUDES `tokens.thoughts` -> add thoughts so billing/totals match output rate
        let cache_read_tokens = raw_cached.min(raw_input);
        let input_tokens = raw_input
            .saturating_sub(raw_cached)
            .saturating_add(raw_tool);
        let output_tokens = raw_output.saturating_add(raw_thoughts);
        let reasoning_tokens = raw_thoughts;

        if input_tokens
            .saturating_add(cache_read_tokens)
            .saturating_add(output_tokens)
            == 0
        {
            continue;
        }

        let (has_thoughts_arr, thoughts_arr_chars) = count_thoughts_chars(msg.get("thoughts"));
        let thinking_chars = if reasoning_tokens == 0 && has_thoughts_arr {
            thoughts_arr_chars.max(1)
        } else {
            0
        };

        let tool_names = extract_tool_names(msg.get("toolCalls"));
        let timestamp_utc = parse_timestamp_utc(msg.get("timestamp"));
        let model = msg
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| {
                if !meta.default_model.is_empty() {
                    Some(meta.default_model.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "unknown".to_string());

        let turn = TurnRecord {
            step_index: 0,
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
        };

        let msg_id = msg
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        if let Some(id) = msg_id {
            if let Some(&slot) = id_to_slot.get(id) {
                turns[slot] = turn;
            } else {
                let slot = turns.len();
                id_to_slot.insert(id.to_string(), slot);
                turns.push(turn);
            }
        } else {
            turns.push(turn);
        }
    }

    for (idx, t) in turns.iter_mut().enumerate() {
        t.step_index = idx as i64;
    }

    let last_step_running = messages.last().map(is_message_running).unwrap_or(false);
    let parent_id = meta.parent_id.filter(|p| p != &conversation_id);
    let is_subagent = meta.is_subagent || parent_id.is_some();

    CachedSessionFile {
        sig,
        conversation_id,
        project_hash: meta.project_hash,
        summary: meta.summary,
        first_user_preview,
        is_subagent,
        parent_id,
        turns,
        last_step_running,
        skipped_lines,
    }
}

fn empty_cached_session(path: &Path, sig: FileSig, skipped_lines: usize) -> CachedSessionFile {
    CachedSessionFile {
        sig,
        conversation_id: fallback_session_id(path),
        project_hash: String::new(),
        summary: String::new(),
        first_user_preview: String::new(),
        is_subagent: false,
        parent_id: None,
        turns: Vec::new(),
        last_step_running: false,
        skipped_lines,
    }
}

fn is_message_running(msg: &Map<String, Value>) -> bool {
    if msg.get("type").and_then(Value::as_str).map(str::trim) != Some("gemini") {
        return false;
    }
    if let Some(Value::Array(calls)) = msg.get("toolCalls") {
        for item in calls {
            if let Some(status) = item
                .as_object()
                .and_then(|o| o.get("status"))
                .and_then(Value::as_str)
            {
                let s = status.trim().to_ascii_lowercase();
                if matches!(
                    s.as_str(),
                    "executing" | "scheduled" | "validating" | "awaiting_approval" | "running" | "in_progress"
                ) {
                    return true;
                }
            }
        }
    }
    msg.get("tokens").map(Value::is_null).unwrap_or(true)
}

fn extract_tool_names(val: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(arr)) = val else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            item.as_object()
                .and_then(|o| o.get("name"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
        .collect()
}

fn count_thoughts_chars(val: Option<&Value>) -> (bool, u64) {
    let Some(Value::Array(arr)) = val else {
        return (false, 0);
    };
    if arr.is_empty() {
        return (false, 0);
    }
    let mut chars = 0u64;
    for item in arr {
        match item {
            Value::String(s) => chars = chars.saturating_add(s.chars().count() as u64),
            Value::Object(o) => {
                for key in ["subject", "description", "text"] {
                    if let Some(s) = o.get(key).and_then(Value::as_str) {
                        chars = chars.saturating_add(s.chars().count() as u64);
                    }
                }
            }
            _ => {}
        }
    }
    (true, chars)
}

fn extract_text_from_value(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Array(arr) => {
            let mut parts = Vec::new();
            for item in arr {
                let t = extract_text_from_value(item);
                let trimmed = t.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
            parts.join(" ")
        }
        Value::Object(o) => o
            .get("text")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn normalize_workspace_line(text: &str) -> String {
    let cleaned = text
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if cleaned.is_empty() {
        return String::new();
    }
    if cleaned.starts_with("file://") {
        decode_file_uri(cleaned)
    } else {
        cleaned.to_string()
    }
}

fn looks_like_path(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return true;
    }
    s.starts_with('/') || s.starts_with('\\') || s.starts_with("~/") || s.contains('/') || s.contains('\\')
}

fn insert_project_mapping(out: &mut HashMap<String, String>, id: &str, workspace: &str) {
    let id = id.trim();
    let ws = workspace.trim();
    if id.is_empty() || ws.is_empty() {
        return;
    }
    out.entry(id.to_string()).or_insert_with(|| ws.to_string());
    let lower = id.to_ascii_lowercase();
    out.entry(lower).or_insert_with(|| ws.to_string());
}

fn index_workspace_hashes(out: &mut HashMap<String, String>, workspace: &str) {
    let ws = workspace.trim();
    if ws.is_empty() {
        return;
    }
    let hash = sha256_hex(ws.as_bytes());
    out.entry(hash).or_insert_with(|| ws.to_string());
    let trimmed = ws.trim_end_matches(['/', '\\']);
    if !trimmed.is_empty() && trimmed != ws {
        let hash_trimmed = sha256_hex(trimmed.as_bytes());
        out.entry(hash_trimmed).or_insert_with(|| ws.to_string());
    }
}

/// Parses `~/.gemini/projects.json` into a reverse lookup map: slug / hash / sha256(path) -> workspace path.
fn parse_projects_json(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let trimmed = text.trim().trim_start_matches('\u{feff}');
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(trimmed) else {
        return out;
    };
    let entries = match root.get("projects") {
        Some(Value::Object(map)) => map,
        _ => &root,
    };

    for (k, v) in entries {
        match v {
            Value::String(val_str) => {
                let k_path = looks_like_path(k);
                let v_path = looks_like_path(val_str);
                if v_path && !k_path {
                    // Alternate shape: `{ "<hash_or_slug>": "/abs/path" }`
                    insert_project_mapping(&mut out, k, val_str);
                    index_workspace_hashes(&mut out, val_str);
                } else {
                    // Canonical Gemini CLI shape: `{ "/abs/path": "<slug_or_hash>" }`
                    insert_project_mapping(&mut out, val_str, k);
                    index_workspace_hashes(&mut out, k);
                }
            }
            Value::Object(obj) => {
                if looks_like_path(k) {
                    index_workspace_hashes(&mut out, k);
                    for field in ["slug", "id", "hash", "projectHash", "name"] {
                        if let Some(id) = obj.get(field).and_then(Value::as_str) {
                            insert_project_mapping(&mut out, id, k);
                        }
                    }
                } else if let Some(ws) = ["path", "root", "projectRoot", "project_root", "cwd", "workspace"]
                    .iter()
                    .find_map(|f| obj.get(*f).and_then(Value::as_str))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    insert_project_mapping(&mut out, k, ws);
                    index_workspace_hashes(&mut out, ws);
                    for field in ["slug", "id", "hash", "projectHash"] {
                        if let Some(id) = obj.get(field).and_then(Value::as_str) {
                            insert_project_mapping(&mut out, id, ws);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Pure-Rust zero-dependency SHA-256 hex digest (FIPS 180-4) for matching legacy `projectHash` dirs
/// against workspace paths listed in `projects.json`.
fn sha256_hex(input: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];

    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(((input.len() + 9 + 63) / 64) * 64);
    padded.extend_from_slice(input);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut w = [0u32; 64];
    for chunk in padded.chunks_exact(64) {
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let b = &chunk[i * 4..i * 4 + 4];
            *word = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for word in h {
        for byte in word.to_be_bytes() {
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn new(label: &str) -> Self {
            let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "agy_gauge_gemini_{label}_{}_{seq}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn test_sha256_nist_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn test_classic_json_token_math_tools_thinking_and_title_priority() {
        let dir = TempDirGuard::new("classic_json");
        let chats = dir.path().join("tmp").join("proj-a").join("chats");
        fs::create_dir_all(&chats).unwrap();

        // Session 1: summary present, thoughts > 0, tool > 0, cached subtracted from input,
        // plus a turn with thoughts == 0 & non-empty thoughts[] array, plus cached > input clamp.
        let sess1 = json!({
            "sessionId": "gem-sess-1",
            "projectHash": "hash-a",
            "startTime": "2026-11-24T08:00:00Z",
            "lastUpdated": "2026-11-24T08:05:00Z",
            "summary": "  Refactor  Gemini  Adapter \n",
            "messages": [
                {
                    "id": "u1",
                    "timestamp": "2026-11-24T08:00:01Z",
                    "type": "user",
                    "content": "First user prompt ignored when summary exists"
                },
                {
                    "id": "g1",
                    "timestamp": "2026-11-24T08:00:05Z",
                    "type": "gemini",
                    "model": "gemini-2.5-pro",
                    "thoughts": [
                        {"subject": "Plan", "description": "Inspecting token math"}
                    ],
                    "toolCalls": [
                        {"id": "tc1", "name": "read_file", "status": "success"},
                        {"id": "tc2", "name": "run_shell_command", "status": "success"}
                    ],
                    "tokens": {
                        "input": 10000,
                        "output": 500,
                        "cached": 6000,
                        "thoughts": 1200,
                        "tool": 300,
                        "total": 12000
                    }
                },
                {
                    "id": "g2",
                    "timestamp": "2026-11-24T08:01:00Z",
                    "type": "gemini",
                    "model": "gemini-2.5-flash",
                    "thoughts": [
                        {"subject": "Quick check", "description": "No thought token count reported"}
                    ],
                    "tokens": {
                        "input": 2000,
                        "output": 250,
                        "cached": 500,
                        "thoughts": 0,
                        "tool": 0
                    }
                },
                {
                    "id": "g3",
                    "timestamp": "2026-11-24T08:02:00Z",
                    "type": "gemini",
                    "model": "gemini-2.5-flash",
                    "tokens": {
                        "input": 100,
                        "output": 40,
                        "cached": 500,
                        "thoughts": 0,
                        "tool": 25
                    }
                }
            ]
        });
        fs::write(chats.join("session-2026-11-24-1.json"), sess1.to_string()).unwrap();

        // Session 2: no summary -> falls back to first `type == "user"` text (array parts)
        let sess2 = json!({
            "sessionId": "gem-sess-2",
            "messages": [
                {
                    "id": "u1",
                    "type": "user",
                    "content": [{"text": "Fix the"}, {"text": "build\nplease"}]
                },
                {
                    "id": "g1",
                    "timestamp": "2026-11-24T08:10:00Z",
                    "type": "gemini",
                    "model": "gemini-2.5-pro",
                    "tokens": {"input": 500, "output": 100, "cached": 0, "thoughts": 0, "tool": 0}
                }
            ]
        });
        fs::write(chats.join("session-2026-11-24-2.json"), sess2.to_string()).unwrap();

        // Session 3: no summary, no user message -> falls back to sessionId
        let sess3 = json!({
            "sessionId": "gem-sess-3",
            "messages": [
                {
                    "id": "g1",
                    "timestamp": "2026-11-24T08:20:00Z",
                    "type": "gemini",
                    "model": "gemini-2.5-pro",
                    "tokens": {"input": 100, "output": 50, "cached": 0, "thoughts": 0, "tool": 0}
                }
            ]
        });
        fs::write(chats.join("session-2026-11-24-3.json"), sess3.to_string()).unwrap();

        let mut adapter = GeminiCliAdapter::new(dir.path().to_path_buf());
        let res = adapter.scan(Utc::now());
        assert!(res.found_root);
        assert_eq!(res.files_reparsed, 3);
        assert_eq!(res.files_cached, 0);
        assert_eq!(res.sessions.len(), 3);

        let s1 = res
            .sessions
            .iter()
            .find(|s| s.conversation_id == "gem-sess-1")
            .unwrap();
        assert_eq!(s1.source, SourceKind::GeminiCli);
        assert_eq!(s1.title, "Refactor Gemini Adapter");
        assert_eq!(s1.turns.len(), 3);

        let t1 = &s1.turns[0];
        assert_eq!(t1.model, "gemini-2.5-pro");
        assert_eq!(t1.cache_read_tokens, 6000);
        assert_eq!(t1.input_tokens, 4300); // (10000 - 6000) + 300
        assert_eq!(t1.cache_write_tokens, 0);
        assert_eq!(t1.cache_write_1h_tokens, 0);
        assert_eq!(t1.output_tokens, 1700); // 500 + 1200
        assert_eq!(t1.reasoning_tokens, 1200);
        assert_eq!(t1.total_input(), 10300);
        assert_eq!(t1.total_tokens(), 12000);
        assert!(t1.has_thinking());
        assert_eq!(t1.tool_names, vec!["read_file", "run_shell_command"]);
        assert_eq!(t1.cost, CostBreakdown::default());

        let t2 = &s1.turns[1];
        assert_eq!(t2.cache_read_tokens, 500);
        assert_eq!(t2.input_tokens, 1500);
        assert_eq!(t2.output_tokens, 250);
        assert_eq!(t2.reasoning_tokens, 0);
        assert!(t2.thinking_chars > 0);
        assert!(t2.has_thinking());

        let t3 = &s1.turns[2];
        assert_eq!(t3.cache_read_tokens, 100); // clamped to input
        assert_eq!(t3.input_tokens, 25); // saturating_sub(500) + 25
        assert_eq!(t3.output_tokens, 40);
        assert_eq!(t3.thinking_chars, 0);
        assert!(!t3.has_thinking());

        let s2 = res
            .sessions
            .iter()
            .find(|s| s.conversation_id == "gem-sess-2")
            .unwrap();
        assert_eq!(s2.title, "Fix the build please");

        let s3 = res
            .sessions
            .iter()
            .find(|s| s.conversation_id == "gem-sess-3")
            .unwrap();
        assert_eq!(s3.title, "gem-sess-3");
    }

    #[test]
    fn test_jsonl_replay_set_rewind_and_dedup() {
        let dir = TempDirGuard::new("jsonl_replay");
        let chats = dir.path().join("tmp").join("proj-jsonl").join("chats");
        fs::create_dir_all(&chats).unwrap();

        let lines = [
            json!({
                "sessionId": "gem-jsonl-1",
                "projectHash": "hash-j",
                "startTime": "2026-11-24T09:00:00Z",
                "summary": "Initial Title"
            })
            .to_string(),
            json!({
                "id": "u1",
                "timestamp": "2026-11-24T09:00:01Z",
                "type": "user",
                "content": "Start task"
            })
            .to_string(),
            // Duplicate message.id = "g1": early partial snapshot
            json!({
                "id": "g1",
                "timestamp": "2026-11-24T09:00:02Z",
                "type": "gemini",
                "model": "gemini-2.5-pro",
                "tokens": {"input": 1000, "output": 10, "cached": 200, "thoughts": 50, "tool": 0}
            })
            .to_string(),
            // Duplicate message.id = "g1": finalized snapshot (must replace earlier)
            json!({
                "id": "g1",
                "timestamp": "2026-11-24T09:00:04Z",
                "type": "gemini",
                "model": "gemini-2.5-pro",
                "toolCalls": [{"id": "t1", "name": "grep_search", "status": "success"}],
                "tokens": {"input": 1000, "output": 120, "cached": 200, "thoughts": 300, "tool": 50}
            })
            .to_string(),
            // Rewound turn (u2 + g2) — must disappear after `$rewindTo: "u2"`
            json!({
                "id": "u2",
                "timestamp": "2026-11-24T09:01:00Z",
                "type": "user",
                "content": "Wrong direction"
            })
            .to_string(),
            json!({
                "id": "g2",
                "timestamp": "2026-11-24T09:01:05Z",
                "type": "gemini",
                "model": "gemini-2.5-pro",
                "tokens": {"input": 99999, "output": 5000, "cached": 0, "thoughts": 4000, "tool": 0}
            })
            .to_string(),
            json!({"$rewindTo": "u2"}).to_string(),
            // Replacement turn after rewind
            json!({
                "id": "u3",
                "timestamp": "2026-11-24T09:02:00Z",
                "type": "user",
                "content": "Right direction"
            })
            .to_string(),
            json!({
                "id": "g3",
                "timestamp": 1_795_510_925_000i64,
                "type": "gemini",
                "model": "gemini-2.5-pro",
                "tokens": {"input": 2000, "output": 80, "cached": 1200, "thoughts": 220, "tool": 10}
            })
            .to_string(),
            // `$set` metadata update
            json!({"$set": {"summary": "Updated Title Via Set"}}).to_string(),
            // Simulated partial/corrupted trailing line during live write
            "{\"id\":\"g4\",\"type\":\"gemini\"".to_string(),
        ];

        fs::write(
            chats.join("session-2026-11-24.jsonl"),
            lines.join("\n") + "\n",
        )
        .unwrap();

        let mut adapter = GeminiCliAdapter::new(dir.path().to_path_buf());
        let res = adapter.scan(Utc::now());
        assert!(res.found_root);
        assert_eq!(res.skipped_lines, 1);
        assert_eq!(res.sessions.len(), 1);

        let s = &res.sessions[0];
        assert_eq!(s.conversation_id, "gem-jsonl-1");
        assert_eq!(s.title, "Updated Title Via Set");
        assert_eq!(s.turns.len(), 2);

        // Turn 0: deduplicated latest `g1`
        assert_eq!(s.turns[0].input_tokens, 850); // (1000 - 200) + 50
        assert_eq!(s.turns[0].cache_read_tokens, 200);
        assert_eq!(s.turns[0].output_tokens, 420); // 120 + 300
        assert_eq!(s.turns[0].reasoning_tokens, 300);
        assert_eq!(s.turns[0].tool_names, vec!["grep_search"]);

        // Turn 1: `g3` (rewound `g2` with 99999 input tokens is gone)
        assert_eq!(s.turns[1].input_tokens, 810); // (2000 - 1200) + 10
        assert_eq!(s.turns[1].cache_read_tokens, 1200);
        assert_eq!(s.turns[1].output_tokens, 300); // 80 + 220
        assert_eq!(s.turns[1].reasoning_tokens, 220);
        assert!(s.turns[1].timestamp_utc.is_some());
    }

    #[test]
    fn test_classic_json_message_id_dedup_and_non_gemini_skips() {
        let dir = TempDirGuard::new("json_dedup");
        let chats = dir.path().join("tmp").join("proj-d").join("chats");
        fs::create_dir_all(&chats).unwrap();

        let payload = json!({
            "sessionId": "dedup-sess",
            "messages": [
                {"id": "i1", "type": "info", "content": "info line", "tokens": {"input": 500, "output": 50}},
                {"id": "e1", "type": "error", "content": "err line"},
                {"id": "g-null", "type": "gemini", "model": "gemini-2.5-pro", "tokens": null},
                {"id": "g-zero", "type": "gemini", "model": "gemini-2.5-pro", "tokens": {"input": 0, "output": 0, "cached": 0, "thoughts": 0, "tool": 0}},
                {"id": "dup-1", "type": "gemini", "model": "gemini-2.5-pro", "tokens": {"input": 400, "output": 20, "cached": 0, "thoughts": 0, "tool": 0}},
                {"id": "dup-1", "type": "gemini", "model": "gemini-2.5-pro", "tokens": {"input": 400, "output": 90, "cached": 100, "thoughts": 10, "tool": 0}}
            ]
        });
        fs::write(chats.join("session-dup.json"), payload.to_string()).unwrap();

        let mut adapter = GeminiCliAdapter::new(dir.path().to_path_buf());
        let res = adapter.scan(Utc::now());
        assert_eq!(res.sessions.len(), 1);
        let turns = &res.sessions[0].turns;
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].input_tokens, 300);
        assert_eq!(turns[0].cache_read_tokens, 100);
        assert_eq!(turns[0].output_tokens, 100);
        assert_eq!(turns[0].reasoning_tokens, 10);
    }

    #[test]
    fn test_workspace_resolution_project_root_projects_json_and_tmp_root() {
        let dir = TempDirGuard::new("workspaces");
        let tmp = dir.path().join("tmp");

        let ws_beta = "C:\\Users\\dev\\beta-service";
        let beta_hash = sha256_hex(ws_beta.as_bytes());

        // Write <gemini_root>/projects.json
        let projects_json = json!({
            "projects": {
                "C:\\Users\\dev\\alpha-app": "alpha-slug",
                "C:\\Users\\dev\\beta-service": "beta-slug",
                "C:\\Users\\dev\\override-ignored": "proj-with-root-file"
            }
        });
        fs::write(dir.path().join("projects.json"), projects_json.to_string()).unwrap();

        let mk_sess = |sid: &str, phash: &str| {
            json!({
                "sessionId": sid,
                "projectHash": phash,
                "messages": [{
                    "id": "g1",
                    "type": "gemini",
                    "model": "gemini-2.5-pro",
                    "tokens": {"input": 100, "output": 20, "cached": 0, "thoughts": 0, "tool": 0}
                }]
            })
            .to_string()
        };

        // 1. `.project_root` takes precedence over `projects.json`
        let p1 = tmp.join("proj-with-root-file");
        fs::create_dir_all(p1.join("chats")).unwrap();
        fs::write(p1.join(".project_root"), "C:\\Users\\dev\\from-project-root\n").unwrap();
        fs::write(p1.join("chats").join("session-1.json"), mk_sess("s-pr", "")).unwrap();

        // 2. `projects.json` lookup by slug dir name
        let p2 = tmp.join("alpha-slug");
        fs::create_dir_all(p2.join("chats")).unwrap();
        fs::write(p2.join("chats").join("session-2.json"), mk_sess("s-slug", "")).unwrap();

        // 3. `projects.json` reverse lookup by SHA-256 hash dir / projectHash
        let p3 = tmp.join(&beta_hash);
        fs::create_dir_all(p3.join("chats")).unwrap();
        fs::write(
            p3.join("chats").join("session-3.json"),
            mk_sess("s-hash", &beta_hash),
        )
        .unwrap();

        // 4. Unknown project -> empty workspace
        let p4 = tmp.join("mystery-proj");
        fs::create_dir_all(p4.join("chats")).unwrap();
        fs::write(p4.join("chats").join("session-4.json"), mk_sess("s-none", "")).unwrap();

        // Mode A: root is `<gemini_root>`
        let mut adapter_root = GeminiCliAdapter::new(dir.path().to_path_buf());
        let res_a = adapter_root.scan(Utc::now());
        assert!(res_a.found_root);
        let ws_of = |res: &SourceScanResult, id: &str| {
            res.sessions
                .iter()
                .find(|s| s.conversation_id == id)
                .unwrap()
                .workspace
                .clone()
        };
        assert_eq!(ws_of(&res_a, "s-pr"), "C:\\Users\\dev\\from-project-root");
        assert_eq!(ws_of(&res_a, "s-slug"), "C:\\Users\\dev\\alpha-app");
        assert_eq!(ws_of(&res_a, "s-hash"), ws_beta);
        assert_eq!(ws_of(&res_a, "s-none"), "");

        // Mode B: root is directly `<gemini_root>/tmp`
        let mut adapter_tmp = GeminiCliAdapter::new(tmp);
        let res_b = adapter_tmp.scan(Utc::now());
        assert!(res_b.found_root);
        assert_eq!(ws_of(&res_b, "s-pr"), "C:\\Users\\dev\\from-project-root");
        assert_eq!(ws_of(&res_b, "s-slug"), "C:\\Users\\dev\\alpha-app");
        assert_eq!(ws_of(&res_b, "s-hash"), ws_beta);
        assert_eq!(ws_of(&res_b, "s-none"), "");
    }

    #[test]
    fn test_incremental_cache_hit_reparse_eviction_and_session_hint() {
        let dir = TempDirGuard::new("inc_cache");
        let proj = dir.path().join("tmp").join("p-cache");
        let chats = proj.join("chats");
        fs::create_dir_all(&chats).unwrap();
        fs::write(proj.join(".project_root"), "/repo/cache-test").unwrap();

        let active_path = chats.join("session-active.json");
        let empty_path = chats.join("session-empty.json");

        let v1 = json!({
            "sessionId": "cache-sid",
            "summary": "Cached Session",
            "messages": [{
                "id": "g1",
                "type": "gemini",
                "model": "gemini-2.5-pro",
                "tokens": {"input": 200, "output": 50, "cached": 0, "thoughts": 0, "tool": 0}
            }]
        });
        let empty_sess = json!({
            "sessionId": "parent-no-turns",
            "summary": "Parent Hint Title",
            "messages": [{
                "id": "u1",
                "type": "user",
                "content": "Only user message so far"
            }]
        });
        fs::write(&active_path, v1.to_string()).unwrap();
        fs::write(&empty_path, empty_sess.to_string()).unwrap();

        let mut adapter = GeminiCliAdapter::new(dir.path().to_path_buf());

        // Scan 1: cold parse
        let r1 = adapter.scan(Utc::now());
        assert_eq!(r1.files_reparsed, 2);
        assert_eq!(r1.files_cached, 0);
        assert_eq!(r1.skipped_empty, 1);
        assert_eq!(r1.sessions.len(), 1);
        assert_eq!(r1.sessions[0].turns[0].output_tokens, 50);
        assert_eq!(
            adapter.lookup_session_hint("parent-no-turns"),
            Some(("Parent Hint Title".to_string(), "/repo/cache-test".to_string()))
        );

        // Scan 2: warm cache hit (zero reparses)
        let r2 = adapter.scan(Utc::now());
        assert_eq!(r2.files_reparsed, 0);
        assert_eq!(r2.files_cached, 2);
        assert_eq!(r2.sessions.len(), 1);

        // Scan 3: mutate file length so FileSig changes -> reparse active file only
        let v2 = json!({
            "sessionId": "cache-sid",
            "summary": "Cached Session Updated",
            "messages": [{
                "id": "g1",
                "type": "gemini",
                "model": "gemini-2.5-pro",
                "tokens": {"input": 200, "output": 150, "cached": 0, "thoughts": 25, "tool": 0}
            }]
        });
        fs::write(&active_path, v2.to_string()).unwrap();
        let r3 = adapter.scan(Utc::now());
        assert_eq!(r3.files_reparsed, 1);
        assert_eq!(r3.files_cached, 1);
        assert_eq!(r3.sessions[0].title, "Cached Session Updated");
        assert_eq!(r3.sessions[0].turns[0].output_tokens, 175);

        // Scan 4: delete active file -> evicted from cache & hints
        fs::remove_file(&active_path).unwrap();
        let r4 = adapter.scan(Utc::now());
        assert_eq!(r4.sessions.len(), 0);
        assert_eq!(r4.files_cached, 1);
        assert!(adapter.lookup_session_hint("cache-sid").is_none());
        assert!(adapter.lookup_session_hint("parent-no-turns").is_some());
    }

    #[test]
    fn test_strict_whitelist_ignores_secrets_and_non_session_files() {
        let dir = TempDirGuard::new("whitelist");
        // Decoy secret files & Antigravity dirs at root (must NEVER be read)
        fs::write(dir.path().join("oauth_creds.json"), "{invalid json!!!").unwrap();
        fs::write(dir.path().join("google_accounts.json"), "{invalid json!!!").unwrap();
        fs::write(dir.path().join("jetski-standalone-oauth-token"), "secret").unwrap();
        fs::write(dir.path().join("settings.json"), "{invalid json!!!").unwrap();
        fs::create_dir_all(dir.path().join("antigravity-cli").join("brain")).unwrap();

        // Missing tmp dir initially -> found_root == false
        let mut adapter = GeminiCliAdapter::new(dir.path().to_path_buf());
        let r0 = adapter.scan(Utc::now());
        assert!(!r0.found_root);
        assert_eq!(r0.skipped_lines, 0);

        // Create tmp/<proj>/chats with non-whitelisted filenames (ignored)
        let chats = dir.path().join("tmp").join("p1").join("chats");
        fs::create_dir_all(&chats).unwrap();
        fs::write(chats.join("notes.json"), "{not a session").unwrap();
        fs::write(chats.join("session-1.txt"), "{not json").unwrap();

        let r1 = adapter.scan(Utc::now());
        assert!(r1.found_root);
        assert_eq!(r1.sessions.len(), 0);
        assert_eq!(r1.files_reparsed, 0);
        assert_eq!(r1.skipped_lines, 0);
    }
}
