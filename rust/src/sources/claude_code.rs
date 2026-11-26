//! Strictly read-only incremental adapter for Claude Code JSONL transcripts.
//!
//! Whitelisted files under `<root>/projects/<encoded-proj>/`:
//!   - `<uuid>.jsonl` (main session transcript)
//!   - `<uuid>/subagents/agent-*.jsonl` (subagent transcript)
//!   - `sessions-index.json` (optional session metadata index)
//!   - `<uuid>/subagents/agent-*.meta.json` (optional subagent metadata)
//!
//! Strictly NEVER opens `settings*.json`, `.credentials.json`, `auth*`,
//! `history.jsonl`, `statsig/`, `debug/`, or any symlink / outside-root path.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::pricing::CostBreakdown;
use crate::sources::{
    clean_single_line, compute_running_hint, parse_timestamp_utc, val_to_u64, FileSig,
    ParsedSession, SafeFs, SourceAdapter, SourceScanResult,
};
use crate::stats::{SourceKind, TurnRecord};

const MAX_INDEX_BYTES: usize = 16 * 1024 * 1024;
const MAX_META_BYTES: usize = 1024 * 1024;
const UNKNOWN_WORKSPACE: &str = "(未知工作区)";

#[derive(Debug, Clone)]
struct CachedClaudeTranscript {
    sig: FileSig,
    turns: Vec<TurnRecord>,
    cwd: String,
    custom_title: String,
    summary: String,
    first_user_preview: String,
    last_step_running: bool,
    skipped_lines: usize,
}

#[derive(Debug, Clone, Default)]
struct IndexEntry {
    custom_title: String,
    summary: String,
    first_prompt: String,
    workspace: String,
}

#[derive(Debug, Clone)]
struct CachedProjectIndex {
    sig: FileSig,
    original_path: String,
    entries: HashMap<String, IndexEntry>,
}

#[derive(Debug, Clone)]
struct CachedSubagentMeta {
    sig: FileSig,
    description: String,
    agent_type: String,
}

pub struct ClaudeCodeAdapter {
    safe_fs: SafeFs,
    transcript_cache: HashMap<PathBuf, CachedClaudeTranscript>,
    index_cache: HashMap<PathBuf, CachedProjectIndex>,
    meta_cache: HashMap<PathBuf, CachedSubagentMeta>,
    /// Fallback `(title, workspace)` hints keyed by main session ID for zero-turn parent lookups.
    session_hints: HashMap<String, (String, String)>,
}

impl ClaudeCodeAdapter {
    pub fn new(root: PathBuf) -> Self {
        Self {
            safe_fs: SafeFs::new(root),
            transcript_cache: HashMap::new(),
            index_cache: HashMap::new(),
            meta_cache: HashMap::new(),
            session_hints: HashMap::new(),
        }
    }

    fn load_project_index(
        &mut self,
        proj_dir: &Path,
        seen_paths: &mut BTreeSet<PathBuf>,
    ) -> Option<CachedProjectIndex> {
        let index_path = proj_dir.join("sessions-index.json");
        let sig = self.safe_fs.is_safe_regular_file(&index_path)?;
        seen_paths.insert(index_path.clone());
        if let Some(cached) = self.index_cache.get(&index_path) {
            if cached.sig == sig {
                return Some(cached.clone());
            }
        }
        let (fresh_sig, text) = self
            .safe_fs
            .read_to_string_limited(&index_path, MAX_INDEX_BYTES)?;
        let parsed = parse_sessions_index(&text, fresh_sig);
        self.index_cache.insert(index_path, parsed.clone());
        Some(parsed)
    }

    fn load_subagent_meta(
        &mut self,
        subagents_dir: &Path,
        agent_id: &str,
        seen_paths: &mut BTreeSet<PathBuf>,
    ) -> Option<CachedSubagentMeta> {
        let meta_path = subagents_dir.join(format!("agent-{agent_id}.meta.json"));
        let sig = self.safe_fs.is_safe_regular_file(&meta_path)?;
        seen_paths.insert(meta_path.clone());
        if let Some(cached) = self.meta_cache.get(&meta_path) {
            if cached.sig == sig {
                return Some(cached.clone());
            }
        }
        let (fresh_sig, text) = self
            .safe_fs
            .read_to_string_limited(&meta_path, MAX_META_BYTES)?;
        let parsed = parse_subagent_meta(&text, fresh_sig);
        self.meta_cache.insert(meta_path, parsed.clone());
        Some(parsed)
    }

    fn load_transcript(
        &mut self,
        path: &Path,
        seen_paths: &mut BTreeSet<PathBuf>,
        res: &mut SourceScanResult,
    ) -> Option<CachedClaudeTranscript> {
        let sig = self.safe_fs.is_safe_regular_file(path)?;
        seen_paths.insert(path.to_path_buf());
        if let Some(cached) = self.transcript_cache.get(path) {
            if cached.sig == sig {
                res.files_cached += 1;
                return Some(cached.clone());
            }
        }
        let (fresh_sig, fh) = self.safe_fs.open_safe_regular_file(path)?;
        let parsed = parse_claude_jsonl(fh, fresh_sig);
        res.files_reparsed += 1;
        self.transcript_cache
            .insert(path.to_path_buf(), parsed.clone());
        Some(parsed)
    }

    fn record_hint(&mut self, cid: &str, title: &str, workspace: &str) {
        let clean_title = title.trim();
        let clean_ws = workspace.trim();
        let clean_ws = if clean_ws == UNKNOWN_WORKSPACE {
            ""
        } else {
            clean_ws
        };
        if clean_title.is_empty() && clean_ws.is_empty() {
            return;
        }
        let entry = self
            .session_hints
            .entry(cid.to_string())
            .or_insert_with(|| (String::new(), String::new()));
        if !clean_title.is_empty() {
            entry.0 = clean_title.to_string();
        }
        if !clean_ws.is_empty() {
            entry.1 = clean_ws.to_string();
        }
    }
}

impl SourceAdapter for ClaudeCodeAdapter {
    fn kind(&self) -> SourceKind {
        SourceKind::ClaudeCode
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
        self.session_hints.clear();

        let projects_dir = self.safe_fs.root().join("projects");
        if !self.safe_fs.is_safe_dir(&projects_dir) {
            self.transcript_cache.clear();
            self.index_cache.clear();
            self.meta_cache.clear();
            self.safe_fs.retain_verified(&BTreeSet::new());
            return res;
        }
        res.found_root = true;

        let Ok(proj_entries) = fs::read_dir(&projects_dir) else {
            return res;
        };
        let mut proj_dirs: Vec<PathBuf> = proj_entries.flatten().map(|e| e.path()).collect();
        proj_dirs.sort();

        let mut seen_paths = BTreeSet::new();

        for proj_dir in proj_dirs {
            if !self.safe_fs.is_safe_dir(&proj_dir) {
                continue;
            }

            let proj_index = self.load_project_index(&proj_dir, &mut seen_paths);
            if let Some(idx) = &proj_index {
                for (sid, entry) in &idx.entries {
                    let title = index_entry_title(entry);
                    let ws = if !entry.workspace.is_empty() {
                        entry.workspace.as_str()
                    } else {
                        idx.original_path.as_str()
                    };
                    self.record_hint(sid, title, ws);
                }
            }

            let Ok(entries) = fs::read_dir(&proj_dir) else {
                continue;
            };
            let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            children.sort();

            let mut main_files: Vec<(String, PathBuf)> = Vec::new();
            let mut subagent_parents: Vec<(String, PathBuf)> = Vec::new();

            for path in children {
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if name == "sessions-index.json" {
                    continue;
                }
                if let Some(stem) = name.strip_suffix(".jsonl") {
                    if is_allowed_session_stem(stem) {
                        main_files.push((stem.to_string(), path));
                    }
                } else if is_allowed_session_stem(name) && self.safe_fs.is_safe_dir(&path) {
                    subagent_parents.push((name.to_string(), path));
                }
            }

            // Pass 1: Main session transcripts (`<uuid>.jsonl`).
            for (session_id, path) in main_files {
                let Some(cached) = self.load_transcript(&path, &mut seen_paths, &mut res) else {
                    continue;
                };
                res.skipped_lines += cached.skipped_lines;

                let idx_entry = proj_index
                    .as_ref()
                    .and_then(|idx| idx.entries.get(&session_id));
                let idx_orig_ws = proj_index
                    .as_ref()
                    .map(|idx| idx.original_path.as_str())
                    .unwrap_or("");

                let raw_title = resolve_main_title(&cached, idx_entry);
                let raw_workspace = resolve_main_workspace(&cached, idx_entry, idx_orig_ws);
                self.record_hint(&session_id, raw_title, raw_workspace);

                if cached.turns.is_empty() {
                    res.skipped_empty += 1;
                    continue;
                }

                let title = if !raw_title.is_empty() {
                    raw_title.to_string()
                } else {
                    session_id.clone()
                };
                let workspace = if !raw_workspace.is_empty() {
                    raw_workspace.to_string()
                } else {
                    UNKNOWN_WORKSPACE.to_string()
                };
                let last_file_mtime_utc = cached.sig.mtime_utc();
                let is_running_hint = compute_running_hint(
                    cached.last_step_running,
                    last_file_mtime_utc,
                    scan_now_utc,
                );

                res.sessions.push(ParsedSession {
                    conversation_id: session_id,
                    title,
                    workspace,
                    source: SourceKind::ClaudeCode,
                    is_subagent: false,
                    parent_id: None,
                    parent_title: String::new(),
                    turns: cached.turns,
                    last_file_mtime_utc,
                    is_running_hint,
                });
            }

            // Pass 2: Subagent transcripts (`<uuid>/subagents/agent-*.jsonl`).
            for (parent_uuid, session_dir) in subagent_parents {
                let subagents_dir = session_dir.join("subagents");
                if !self.safe_fs.is_safe_dir(&subagents_dir) {
                    continue;
                }
                let Ok(sub_entries) = fs::read_dir(&subagents_dir) else {
                    continue;
                };
                let mut sub_paths: Vec<PathBuf> = sub_entries.flatten().map(|e| e.path()).collect();
                sub_paths.sort();

                for sub_path in sub_paths {
                    let Some(name) = sub_path.file_name().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    let Some(rest) = name.strip_prefix("agent-") else {
                        continue;
                    };
                    let Some(agent_id) = rest.strip_suffix(".jsonl") else {
                        continue;
                    };
                    if !is_allowed_agent_id(agent_id) {
                        continue;
                    }

                    let Some(cached) = self.load_transcript(&sub_path, &mut seen_paths, &mut res)
                    else {
                        continue;
                    };
                    res.skipped_lines += cached.skipped_lines;

                    let meta = self.load_subagent_meta(&subagents_dir, agent_id, &mut seen_paths);

                    if cached.turns.is_empty() {
                        res.skipped_empty += 1;
                        continue;
                    }

                    let conversation_id = format!("{parent_uuid}__{agent_id}");
                    let title = resolve_subagent_title(&cached, meta.as_ref(), &conversation_id);
                    let workspace = if !cached.cwd.is_empty() {
                        cached.cwd.clone()
                    } else if let Some((_, pws)) = self.session_hints.get(&parent_uuid) {
                        if !pws.is_empty() {
                            pws.clone()
                        } else {
                            UNKNOWN_WORKSPACE.to_string()
                        }
                    } else if let Some(idx) = proj_index
                        .as_ref()
                        .filter(|idx| !idx.original_path.is_empty())
                    {
                        idx.original_path.clone()
                    } else {
                        UNKNOWN_WORKSPACE.to_string()
                    };

                    let last_file_mtime_utc = cached.sig.mtime_utc();
                    let is_running_hint = compute_running_hint(
                        cached.last_step_running,
                        last_file_mtime_utc,
                        scan_now_utc,
                    );

                    res.sessions.push(ParsedSession {
                        conversation_id,
                        title,
                        workspace,
                        source: SourceKind::ClaudeCode,
                        is_subagent: true,
                        parent_id: Some(parent_uuid.clone()),
                        parent_title: String::new(),
                        turns: cached.turns,
                        last_file_mtime_utc,
                        is_running_hint,
                    });
                }
            }
        }

        self.transcript_cache.retain(|k, _| seen_paths.contains(k));
        self.index_cache.retain(|k, _| seen_paths.contains(k));
        self.meta_cache.retain(|k, _| seen_paths.contains(k));
        self.safe_fs.retain_verified(&seen_paths);
        res
    }
}

fn is_safe_id_chars(s: &str) -> bool {
    if s.is_empty() || s.len() > 128 {
        return false;
    }
    let bytes = s.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
}

fn is_allowed_session_stem(stem: &str) -> bool {
    if !is_safe_id_chars(stem) {
        return false;
    }
    let lower = stem.to_ascii_lowercase();
    if lower == "history"
        || lower == "sessions-index"
        || lower.starts_with("agent-")
        || lower.starts_with("settings")
        || lower.starts_with("auth")
        || lower.starts_with("credentials")
        || lower.contains("credentials")
        || lower.starts_with("statsig")
        || lower.starts_with("debug")
    {
        return false;
    }
    true
}

fn is_allowed_agent_id(agent_id: &str) -> bool {
    is_safe_id_chars(agent_id)
}

fn index_entry_title(entry: &IndexEntry) -> &str {
    if !entry.custom_title.is_empty() {
        &entry.custom_title
    } else if !entry.summary.is_empty() {
        &entry.summary
    } else if !entry.first_prompt.is_empty() {
        &entry.first_prompt
    } else {
        ""
    }
}

fn resolve_main_title<'a>(
    cached: &'a CachedClaudeTranscript,
    idx_entry: Option<&'a IndexEntry>,
) -> &'a str {
    if !cached.custom_title.is_empty() {
        return &cached.custom_title;
    }
    if !cached.summary.is_empty() {
        return &cached.summary;
    }
    if let Some(entry) = idx_entry {
        let t = index_entry_title(entry);
        if !t.is_empty() {
            return t;
        }
    }
    if !cached.first_user_preview.is_empty() {
        return &cached.first_user_preview;
    }
    ""
}

fn resolve_main_workspace<'a>(
    cached: &'a CachedClaudeTranscript,
    idx_entry: Option<&'a IndexEntry>,
    idx_orig_ws: &'a str,
) -> &'a str {
    if !cached.cwd.is_empty() {
        return &cached.cwd;
    }
    if let Some(entry) = idx_entry {
        if !entry.workspace.is_empty() {
            return &entry.workspace;
        }
    }
    if !idx_orig_ws.is_empty() {
        return idx_orig_ws;
    }
    ""
}

fn resolve_subagent_title(
    cached: &CachedClaudeTranscript,
    meta: Option<&CachedSubagentMeta>,
    fallback_id: &str,
) -> String {
    if let Some(m) = meta {
        if !m.description.is_empty() {
            return m.description.clone();
        }
        if !m.agent_type.is_empty() {
            return m.agent_type.clone();
        }
    }
    if !cached.custom_title.is_empty() {
        return cached.custom_title.clone();
    }
    if !cached.summary.is_empty() {
        return cached.summary.clone();
    }
    if !cached.first_user_preview.is_empty() {
        return cached.first_user_preview.clone();
    }
    fallback_id.to_string()
}

fn parse_sessions_index(text: &str, sig: FileSig) -> CachedProjectIndex {
    let mut out = CachedProjectIndex {
        sig,
        original_path: String::new(),
        entries: HashMap::new(),
    };
    let Ok(val) = serde_json::from_str::<Value>(text) else {
        return out;
    };

    let mut parse_entry_obj = |fallback_sid: Option<&str>, obj: &serde_json::Map<String, Value>| {
        let sid = obj
            .get("sessionId")
            .or_else(|| obj.get("session_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or(fallback_sid);
        let Some(sid) = sid else { return };

        let custom_title = obj
            .get("customTitle")
            .or_else(|| obj.get("custom_title"))
            .and_then(Value::as_str)
            .map(|s| clean_single_line(s, 100))
            .unwrap_or_default();
        let summary = obj
            .get("summary")
            .and_then(Value::as_str)
            .map(|s| clean_single_line(s, 100))
            .unwrap_or_default();
        let first_prompt = obj
            .get("firstPrompt")
            .or_else(|| obj.get("first_prompt"))
            .and_then(Value::as_str)
            .map(|s| clean_single_line(s, 100))
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("no prompt"))
            .unwrap_or_default();
        let workspace = obj
            .get("projectPath")
            .or_else(|| obj.get("project_path"))
            .or_else(|| obj.get("cwd"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_default();

        out.entries.insert(
            sid.to_string(),
            IndexEntry {
                custom_title,
                summary,
                first_prompt,
                workspace,
            },
        );
    };

    match &val {
        Value::Object(root) => {
            if let Some(orig) = root
                .get("originalPath")
                .or_else(|| root.get("original_path"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                out.original_path = orig.to_string();
            }
            if let Some(Value::Array(arr)) = root.get("entries") {
                for item in arr {
                    if let Some(obj) = item.as_object() {
                        parse_entry_obj(None, obj);
                    }
                }
            } else {
                // Fallback shape: map of `{ "<sessionId>": { ... } }`
                for (k, v) in root {
                    if let Some(obj) = v.as_object() {
                        parse_entry_obj(Some(k.as_str()), obj);
                    }
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                if let Some(obj) = item.as_object() {
                    parse_entry_obj(None, obj);
                }
            }
        }
        _ => {}
    }

    out
}

fn parse_subagent_meta(text: &str, sig: FileSig) -> CachedSubagentMeta {
    let mut out = CachedSubagentMeta {
        sig,
        description: String::new(),
        agent_type: String::new(),
    };
    let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(text) else {
        return out;
    };
    out.description = obj
        .get("description")
        .and_then(Value::as_str)
        .map(|s| clean_single_line(s, 100))
        .unwrap_or_default();
    out.agent_type = obj
        .get("agentType")
        .or_else(|| obj.get("agent_type"))
        .and_then(Value::as_str)
        .map(|s| clean_single_line(s, 100))
        .unwrap_or_default();
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DedupKey {
    MsgReq(String, String),
    MsgOnly(String),
    Uuid(String),
    Line(usize),
}

struct DedupTurnAcc {
    first_seq: usize,
    timestamp_utc: Option<DateTime<Utc>>,
    model: String,
    has_usage: bool,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    cache_write_1h_tokens: u64,
    output_tokens: u64,
    tool_names: Vec<String>,
    seen_tool_ids: HashSet<String>,
    thinking_chars: u64,
}

fn parse_claude_jsonl(fh: File, sig: FileSig) -> CachedClaudeTranscript {
    let mut reader = BufReader::with_capacity(64 * 1024, fh);
    let mut buf = Vec::with_capacity(8 * 1024);

    let mut by_key: HashMap<DedupKey, DedupTurnAcc> = HashMap::new();
    let mut next_seq: usize = 0;
    let mut line_no: usize = 0;

    let mut cwd = String::new();
    let mut custom_title = String::new();
    let mut summary = String::new();
    let mut first_user_preview = String::new();
    let mut last_step_running = false;
    let mut skipped_lines = 0usize;

    loop {
        buf.clear();
        let Ok(n) = reader.read_until(b'\n', &mut buf) else {
            break;
        };
        if n == 0 {
            break;
        }
        line_no += 1;

        let line_cow = String::from_utf8_lossy(&buf);
        let mut trimmed = line_cow.trim();
        if line_no == 1 {
            trimmed = trimmed.trim_start_matches('\u{feff}').trim();
        }
        if trimmed.is_empty() {
            continue;
        }

        // Fast-path skip for well-formed lines that cannot carry turns, titles, cwd, or user-turn transitions.
        if trimmed.starts_with('{') && trimmed.ends_with('}') {
            let need_cwd = cwd.is_empty() && trimmed.contains("\"cwd\"");
            let need_user = trimmed.contains("\"user\"");
            let interesting = trimmed.contains("\"assistant\"")
                || trimmed.contains("\"custom-title\"")
                || trimmed.contains("\"summary\"")
                || need_cwd
                || need_user;
            if !interesting {
                continue;
            }
        }

        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) else {
            skipped_lines += 1;
            continue;
        };

        if cwd.is_empty() {
            if let Some(c) = map
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                cwd = c.to_string();
            }
        }

        let rec_type = map.get("type").and_then(Value::as_str).unwrap_or("");
        match rec_type {
            "custom-title" => {
                if let Some(t) = map
                    .get("customTitle")
                    .or_else(|| map.get("custom_title"))
                    .and_then(Value::as_str)
                {
                    let clean = clean_single_line(t, 100);
                    if !clean.is_empty() {
                        custom_title = clean;
                    }
                }
                continue;
            }
            "summary" => {
                if let Some(s) = map.get("summary").and_then(Value::as_str) {
                    let clean = clean_single_line(s, 100);
                    if !clean.is_empty() {
                        summary = clean;
                    }
                }
                continue;
            }
            "user" => {
                last_step_running = true;
                if first_user_preview.is_empty() {
                    let is_meta = map
                        .get("isMeta")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if !is_meta {
                        if let Some(snippet) = extract_user_text(&map) {
                            first_user_preview = snippet;
                        }
                    }
                }
                continue;
            }
            "assistant" => {}
            _ => continue,
        }

        // Skip synthetic / API error assistant records.
        if map
            .get("isApiErrorMessage")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let Some(message) = map.get("message").and_then(Value::as_object) else {
            continue;
        };
        if message
            .get("isApiErrorMessage")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let model_raw = message
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if model_raw.eq_ignore_ascii_case("<synthetic>") {
            continue;
        }

        let stop_reason = message.get("stop_reason").and_then(Value::as_str);
        last_step_running = matches!(stop_reason, None | Some("tool_use"));

        let usage_opt = message.get("usage").and_then(Value::as_object);
        let (has_usage, input_tokens, cache_read_tokens, cache_write_tokens, cache_write_1h_tokens, output_tokens) =
            if let Some(usage) = usage_opt {
                let input = val_to_u64(usage.get("input_tokens"));
                let cache_read = val_to_u64(usage.get("cache_read_input_tokens"));
                let cache_write = val_to_u64(usage.get("cache_creation_input_tokens"));
                let raw_1h = usage
                    .get("cache_creation")
                    .and_then(Value::as_object)
                    .map(|cc| val_to_u64(cc.get("ephemeral_1h_input_tokens")))
                    .unwrap_or(0);
                let cache_write_1h = raw_1h.min(cache_write);
                let output = val_to_u64(usage.get("output_tokens"));
                (true, input, cache_read, cache_write, cache_write_1h, output)
            } else {
                (false, 0, 0, 0, 0, 0)
            };

        let msg_id = message
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let req_id = map
            .get("requestId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let uuid = map
            .get("uuid")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // If there is no usage AND no message.id to join with a sibling chunk, skip immediately.
        if !has_usage && msg_id.is_none() {
            continue;
        }

        let dedup_key = match (msg_id, req_id, uuid) {
            (Some(m), Some(r), _) => DedupKey::MsgReq(m.to_string(), r.to_string()),
            (Some(m), None, _) => DedupKey::MsgOnly(m.to_string()),
            (None, _, Some(u)) => DedupKey::Uuid(u.to_string()),
            (None, _, None) => DedupKey::Line(line_no),
        };

        let timestamp_utc = parse_timestamp_utc(map.get("timestamp"));
        let model = if model_raw.is_empty() {
            "unknown"
        } else {
            model_raw
        };

        let (line_tools, line_thinking_chars) = extract_assistant_content(message.get("content"));

        let acc = by_key.entry(dedup_key).or_insert_with(|| {
            let seq = next_seq;
            next_seq += 1;
            DedupTurnAcc {
                first_seq: seq,
                timestamp_utc: None,
                model: "unknown".to_string(),
                has_usage: false,
                input_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cache_write_1h_tokens: 0,
                output_tokens: 0,
                tool_names: Vec::new(),
                seen_tool_ids: HashSet::new(),
                thinking_chars: 0,
            }
        });

        if model != "unknown" {
            acc.model = model.to_string();
        }
        if let Some(ts) = timestamp_utc {
            acc.timestamp_utc = Some(match acc.timestamp_utc {
                Some(prev) => prev.max(ts),
                None => ts,
            });
        }

        if has_usage {
            let new_total_in = input_tokens
                .saturating_add(cache_read_tokens)
                .saturating_add(cache_write_tokens);
            let old_total_in = acc
                .input_tokens
                .saturating_add(acc.cache_read_tokens)
                .saturating_add(acc.cache_write_tokens);

            if !acc.has_usage
                || new_total_in > old_total_in
                || (new_total_in == old_total_in
                    && new_total_in > 0
                    && output_tokens >= acc.output_tokens)
            {
                acc.input_tokens = input_tokens;
                acc.cache_read_tokens = cache_read_tokens;
                acc.cache_write_tokens = cache_write_tokens;
                acc.cache_write_1h_tokens = acc
                    .cache_write_1h_tokens
                    .max(cache_write_1h_tokens)
                    .min(cache_write_tokens);
            }
            acc.output_tokens = acc.output_tokens.max(output_tokens);
            acc.has_usage = true;
        }

        for (tool_id, tool_name) in line_tools {
            if let Some(tid) = tool_id {
                if acc.seen_tool_ids.insert(tid) {
                    acc.tool_names.push(tool_name);
                }
            } else if !acc.tool_names.contains(&tool_name) {
                acc.tool_names.push(tool_name);
            }
        }
        acc.thinking_chars = acc.thinking_chars.saturating_add(line_thinking_chars);
    }

    let mut ordered: Vec<DedupTurnAcc> = by_key.into_values().collect();
    ordered.sort_by_key(|a| a.first_seq);

    let mut turns = Vec::with_capacity(ordered.len());
    for acc in ordered {
        if !acc.has_usage {
            continue;
        }
        let total = acc
            .input_tokens
            .saturating_add(acc.cache_read_tokens)
            .saturating_add(acc.cache_write_tokens)
            .saturating_add(acc.output_tokens);
        if total == 0 {
            continue;
        }
        let step_index = (turns.len() + 1) as i64;
        turns.push(TurnRecord {
            step_index,
            timestamp_utc: acc.timestamp_utc,
            model: acc.model,
            is_checkpoint: false,
            input_tokens: acc.input_tokens,
            cache_read_tokens: acc.cache_read_tokens,
            cache_write_tokens: acc.cache_write_tokens,
            cache_write_1h_tokens: acc.cache_write_1h_tokens.min(acc.cache_write_tokens),
            output_tokens: acc.output_tokens,
            reasoning_tokens: 0,
            tool_names: acc.tool_names,
            thinking_chars: acc.thinking_chars,
            gen_secs: None,
            cost: CostBreakdown::default(),
        });
    }

    CachedClaudeTranscript {
        sig,
        turns,
        cwd,
        custom_title,
        summary,
        first_user_preview,
        last_step_running,
        skipped_lines,
    }
}

fn extract_assistant_content(content: Option<&Value>) -> (Vec<(Option<String>, String)>, u64) {
    let mut tools = Vec::new();
    let mut thinking_chars: u64 = 0;

    let Some(Value::Array(blocks)) = content else {
        return (tools, thinking_chars);
    };
    for block in blocks {
        let Some(obj) = block.as_object() else {
            continue;
        };
        match obj.get("type").and_then(Value::as_str).unwrap_or("") {
            "tool_use" => {
                if let Some(name) = obj
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let tid = obj
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                    tools.push((tid, name.to_string()));
                }
            }
            "thinking" => {
                let text = obj
                    .get("thinking")
                    .or_else(|| obj.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let chars = (text.chars().count() as u64).max(1);
                thinking_chars = thinking_chars.saturating_add(chars);
            }
            "redacted_thinking" => {
                thinking_chars = thinking_chars.saturating_add(1);
            }
            _ => {}
        }
    }

    (tools, thinking_chars)
}

fn extract_user_text(map: &serde_json::Map<String, Value>) -> Option<String> {
    let content = map.get("message").and_then(|m| m.get("content"))?;
    match content {
        Value::String(s) => sanitize_user_snippet(s),
        Value::Array(blocks) => {
            for block in blocks {
                let Some(obj) = block.as_object() else {
                    continue;
                };
                if obj.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = obj.get("text").and_then(Value::as_str) {
                        if let Some(clean) = sanitize_user_snippet(t) {
                            return Some(clean);
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn sanitize_user_snippet(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Skip Claude Code internal wrapper messages that are not real user prompts.
    if trimmed.starts_with("<local-command-caveat>")
        || trimmed.starts_with("<local-command-stdout>")
        || trimmed.starts_with("<system-reminder>")
    {
        return None;
    }
    let clean = clean_single_line(trimmed, 80);
    if clean.is_empty() {
        None
    } else {
        Some(clean)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_test_dir(tag: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agy_gauge_claude_test_{}_{}_{}",
            std::process::id(),
            n,
            tag
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_lines(path: &Path, lines: &[&str]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut body = lines.join("\n");
        body.push('\n');
        fs::write(path, body).unwrap();
    }

    #[test]
    fn test_streaming_dedup_tool_union_and_thinking() {
        let root = temp_test_dir("streaming");
        let uuid = "11111111-2222-3333-4444-555555555555";
        let path = root
            .join("projects")
            .join("C--Users-dev-repo")
            .join(format!("{uuid}.jsonl"));

        write_lines(
            &path,
            &[
                r#"{"type":"user","cwd":"C:\\Users\\dev\\my-real-repo","message":{"role":"user","content":"Inspect parser bugs"}}"#,
                // Streaming chunk 1 (thinking block, partial output_tokens=4)
                r#"{"type":"assistant","uuid":"u1","requestId":"req_1","timestamp":"2026-11-24T10:00:01Z","message":{"id":"msg_1","model":"claude-sonnet-4-5-20250929","stop_reason":null,"content":[{"type":"thinking","thinking":"Checking parser"}],"usage":{"input_tokens":120,"cache_read_input_tokens":5000,"cache_creation_input_tokens":300,"output_tokens":4}}}"#,
                // Streaming chunk 2 (first tool_use, partial output_tokens=40)
                r#"{"type":"assistant","uuid":"u2","requestId":"req_1","timestamp":"2026-11-24T10:00:02Z","message":{"id":"msg_1","model":"claude-sonnet-4-5-20250929","stop_reason":null,"content":[{"type":"tool_use","id":"toolu_1","name":"Read"}],"usage":{"input_tokens":120,"cache_read_input_tokens":5000,"cache_creation_input_tokens":300,"output_tokens":40}}}"#,
                // Streaming chunk 3 (repeats toolu_1 + adds toolu_2, final output_tokens=180)
                r#"{"type":"assistant","uuid":"u3","requestId":"req_1","timestamp":"2026-11-24T10:00:04Z","message":{"id":"msg_1","model":"claude-sonnet-4-5-20250929","stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_1","name":"Read"},{"type":"tool_use","id":"toolu_2","name":"Edit"}],"usage":{"input_tokens":120,"cache_read_input_tokens":5000,"cache_creation_input_tokens":300,"output_tokens":180}}}"#,
                // Turn 2: fallback dedup by message.id only (no requestId) + redacted_thinking
                r#"{"type":"assistant","uuid":"u4","timestamp":"2026-11-24T10:01:00Z","message":{"id":"msg_2","model":"claude-sonnet-4-5-20250929","stop_reason":null,"content":[{"type":"redacted_thinking","data":"opaque-bytes"}],"usage":{"input_tokens":50,"cache_read_input_tokens":5400,"cache_creation_input_tokens":0,"output_tokens":5}}}"#,
                r#"{"type":"assistant","uuid":"u5","timestamp":"2026-11-24T10:01:05Z","message":{"id":"msg_2","model":"claude-sonnet-4-5-20250929","stop_reason":"end_turn","content":[{"type":"text","text":"All fixed!"}],"usage":{"input_tokens":50,"cache_read_input_tokens":5400,"cache_creation_input_tokens":0,"output_tokens":95}}}"#,
                // Turn 3: fallback dedup by uuid only (no message.id or requestId)
                r#"{"type":"assistant","uuid":"u6","timestamp":"2026-11-24T10:02:00Z","message":{"model":"claude-sonnet-4-5-20250929","stop_reason":null,"content":[{"type":"tool_use","name":"Bash"}],"usage":{"input_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":3}}}"#,
                r#"{"type":"assistant","uuid":"u6","timestamp":"2026-11-24T10:02:02Z","message":{"model":"claude-sonnet-4-5-20250929","stop_reason":"end_turn","content":[{"type":"tool_use","name":"Bash"}],"usage":{"input_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":25}}}"#,
            ],
        );

        let mut adapter = ClaudeCodeAdapter::new(root.clone());
        let res = adapter.scan(Utc::now());
        assert!(res.found_root);
        assert_eq!(res.files_reparsed, 1);
        assert_eq!(res.files_cached, 0);
        assert_eq!(res.sessions.len(), 1);

        let s = &res.sessions[0];
        assert_eq!(s.conversation_id, uuid);
        assert_eq!(s.source, SourceKind::ClaudeCode);
        assert_eq!(s.workspace, r"C:\Users\dev\my-real-repo");
        assert_eq!(s.title, "Inspect parser bugs");
        assert!(!s.is_subagent);
        assert_eq!(s.turns.len(), 3);

        // Turn 1
        let t1 = &s.turns[0];
        assert_eq!(t1.input_tokens, 120);
        assert_eq!(t1.cache_read_tokens, 5000);
        assert_eq!(t1.cache_write_tokens, 300);
        assert_eq!(t1.cache_write_1h_tokens, 0);
        assert_eq!(t1.output_tokens, 180);
        assert_eq!(t1.reasoning_tokens, 0);
        assert_eq!(t1.tool_names, vec!["Read".to_string(), "Edit".to_string()]);
        assert_eq!(t1.thinking_chars, "Checking parser".chars().count() as u64);
        assert!(t1.has_thinking());
        assert_eq!(t1.cost, CostBreakdown::default());

        // Turn 2 (redacted_thinking -> thinking_chars >= 1)
        let t2 = &s.turns[1];
        assert_eq!(t2.output_tokens, 95);
        assert!(t2.thinking_chars >= 1);
        assert!(t2.has_thinking());

        // Turn 3 (uuid fallback, tool_use without id deduplicated by name)
        let t3 = &s.turns[2];
        assert_eq!(t3.output_tokens, 25);
        assert_eq!(t3.tool_names, vec!["Bash".to_string()]);
        assert!(!t3.has_thinking());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_skip_synthetic_and_api_error_messages() {
        let root = temp_test_dir("synthetic");
        let uuid = "22222222-2222-3333-4444-555555555555";
        let path = root
            .join("projects")
            .join("proj-a")
            .join(format!("{uuid}.jsonl"));

        write_lines(
            &path,
            &[
                // Real host edge case: synthetic error with zeroed usage + cache_creation sub-object
                r#"{"type":"assistant","uuid":"e1","isApiErrorMessage":true,"message":{"id":"msg_e1","model":"<synthetic>","content":[{"type":"text","text":"API Error"}],"usage":{"input_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}}}}"#,
                // Synthetic model even with non-zero tokens & without isApiErrorMessage flag
                r#"{"type":"assistant","uuid":"e2","message":{"id":"msg_e2","model":"<synthetic>","usage":{"input_tokens":999,"output_tokens":999}}}"#,
                // Real model but marked isApiErrorMessage: true
                r#"{"type":"assistant","uuid":"e3","isApiErrorMessage":true,"message":{"id":"msg_e3","model":"claude-opus-4-5","usage":{"input_tokens":500,"output_tokens":200}}}"#,
                // Valid turn
                r#"{"type":"assistant","uuid":"ok1","requestId":"r1","timestamp":"2026-11-24T11:00:00Z","message":{"id":"msg_ok","model":"claude-opus-4-5","stop_reason":"end_turn","content":[{"type":"text","text":"Done"}],"usage":{"input_tokens":42,"cache_read_input_tokens":100,"cache_creation_input_tokens":0,"output_tokens":18}}}"#,
            ],
        );

        let mut adapter = ClaudeCodeAdapter::new(root.clone());
        let res = adapter.scan(Utc::now());
        assert_eq!(res.sessions.len(), 1);
        assert_eq!(res.sessions[0].turns.len(), 1);
        assert_eq!(res.sessions[0].turns[0].model, "claude-opus-4-5");
        assert_eq!(res.sessions[0].turns[0].input_tokens, 42);
        assert_eq!(res.sessions[0].turns[0].output_tokens, 18);
        // Workspace must NEVER lossy-decode "proj-a"
        assert_eq!(res.sessions[0].workspace, UNKNOWN_WORKSPACE);
        // Title falls back to conversation_id when no custom-title/summary/index/user prompt
        assert_eq!(res.sessions[0].title, uuid);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_ephemeral_1h_cache_write_subset_and_clamping() {
        let root = temp_test_dir("cw1h");
        let uuid = "33333333-2222-3333-4444-555555555555";
        let path = root
            .join("projects")
            .join("proj-cw")
            .join(format!("{uuid}.jsonl"));

        write_lines(
            &path,
            &[
                // Normal split: 1000 total cache_creation = 400 (5m) + 600 (1h) -> NOT additive!
                r#"{"type":"assistant","uuid":"u1","requestId":"r1","message":{"id":"m1","model":"claude-sonnet-4-5","usage":{"input_tokens":200,"cache_read_input_tokens":3000,"cache_creation_input_tokens":1000,"cache_creation":{"ephemeral_5m_input_tokens":400,"ephemeral_1h_input_tokens":600},"output_tokens":50}}}"#,
                // Malformed larger 1h value (1500 > 800): must clamp to 800
                r#"{"type":"assistant","uuid":"u2","requestId":"r2","message":{"id":"m2","model":"claude-sonnet-4-5","usage":{"input_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":800,"cache_creation":{"ephemeral_1h_input_tokens":1500},"output_tokens":20}}}"#,
            ],
        );

        let mut adapter = ClaudeCodeAdapter::new(root.clone());
        let res = adapter.scan(Utc::now());
        assert_eq!(res.sessions.len(), 1);
        let turns = &res.sessions[0].turns;
        assert_eq!(turns.len(), 2);

        assert_eq!(turns[0].cache_write_tokens, 1000);
        assert_eq!(turns[0].cache_write_1h_tokens, 600);
        assert_eq!(turns[0].total_input(), 200 + 3000 + 1000);
        assert_eq!(turns[0].total_tokens(), 200 + 3000 + 1000 + 50);

        assert_eq!(turns[1].cache_write_tokens, 800);
        assert_eq!(turns[1].cache_write_1h_tokens, 800);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_title_priority_and_sessions_index() {
        let root = temp_test_dir("titles");
        let proj = root.join("projects").join("proj-titles");
        let s1 = "44444444-1111-2222-3333-444444444441";
        let s2 = "44444444-1111-2222-3333-444444444442";
        let s3 = "44444444-1111-2222-3333-444444444443";
        let s4 = "44444444-1111-2222-3333-444444444444";

        fs::create_dir_all(&proj).unwrap();
        fs::write(
            proj.join("sessions-index.json"),
            format!(
                r#"{{
                    "version": 1,
                    "originalPath": "D:/work/app",
                    "entries": [
                        {{"sessionId":"{s1}","customTitle":"Index Custom","summary":"Index Sum","firstPrompt":"Index Prompt"}},
                        {{"sessionId":"{s2}","summary":"Index Sum","firstPrompt":"Index Prompt"}},
                        {{"sessionId":"{s3}","summary":"Index Summary Title","projectPath":"D:/work/subproj"}},
                        {{"sessionId":"{s4}","firstPrompt":"No prompt"}}
                    ]
                }}"#
            ),
        )
        .unwrap();

        // s1: custom-title wins over summary, index, and user prompt
        write_lines(
            &proj.join(format!("{s1}.jsonl")),
            &[
                r#"{"type":"user","cwd":"D:/work/s1","message":{"role":"user","content":"User prompt"}}"#,
                r#"{"type":"summary","summary":"JSONL Summary"}"#,
                r#"{"type":"custom-title","customTitle":"Renamed Custom Title"}"#,
                r#"{"type":"assistant","uuid":"a1","message":{"id":"m1","model":"claude-sonnet-4-5","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            ],
        );
        // s2: JSONL summary wins over index and user prompt
        write_lines(
            &proj.join(format!("{s2}.jsonl")),
            &[
                r#"{"type":"user","message":{"role":"user","content":"User prompt 2"}}"#,
                r#"{"type":"summary","summary":"JSONL Summary 2"}"#,
                r#"{"type":"assistant","uuid":"a2","message":{"id":"m2","model":"claude-sonnet-4-5","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            ],
        );
        // s3: sessions-index wins over user prompt & supplies projectPath when JSONL lacks cwd
        write_lines(
            &proj.join(format!("{s3}.jsonl")),
            &[
                r#"{"type":"user","message":{"role":"user","content":"User prompt 3"}}"#,
                r#"{"type":"assistant","uuid":"a3","message":{"id":"m3","model":"claude-sonnet-4-5","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            ],
        );
        // s4: index has sentinel "No prompt", falls back to real user prompt & originalPath
        write_lines(
            &proj.join(format!("{s4}.jsonl")),
            &[
                r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"Meta line ignored"}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Real user question\nwith newline"}]}}"#,
                r#"{"type":"assistant","uuid":"a4","message":{"id":"m4","model":"claude-sonnet-4-5","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            ],
        );

        let mut adapter = ClaudeCodeAdapter::new(root.clone());
        let res = adapter.scan(Utc::now());
        let by_id: HashMap<&str, &ParsedSession> = res
            .sessions
            .iter()
            .map(|s| (s.conversation_id.as_str(), s))
            .collect();

        assert_eq!(by_id[s1].title, "Renamed Custom Title");
        assert_eq!(by_id[s1].workspace, "D:/work/s1");

        assert_eq!(by_id[s2].title, "JSONL Summary 2");
        assert_eq!(by_id[s2].workspace, "D:/work/app");

        assert_eq!(by_id[s3].title, "Index Summary Title");
        assert_eq!(by_id[s3].workspace, "D:/work/subproj");

        assert_eq!(by_id[s4].title, "Real user question with newline");
        assert_eq!(by_id[s4].workspace, "D:/work/app");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_subagent_linkage_no_tool_use_result_double_count_and_parent_hint() {
        let root = temp_test_dir("subagents");
        let proj = root.join("projects").join("proj-sub");
        let parent_uuid = "55555555-2222-3333-4444-555555555555";
        let empty_parent = "66666666-2222-3333-4444-555555555555";

        // Main session has 1 assistant turn + 1 user line carrying polymorphic toolUseResult.usage
        // (which MUST NOT be counted as tokens!).
        write_lines(
            &proj.join(format!("{parent_uuid}.jsonl")),
            &[
                r#"{"type":"user","cwd":"C:/repo/main","message":{"role":"user","content":"Refactor auth"}}"#,
                r#"{"type":"assistant","uuid":"p1","requestId":"rp1","message":{"id":"mp1","model":"claude-sonnet-4-5","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"Agent"}],"usage":{"input_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":40}}}"#,
                r#"{"type":"user","toolUseResult":{"status":"completed","totalTokens":99999,"usage":{"input_tokens":80000,"output_tokens":19999}},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
                r#"{"type":"user","toolUseResult":"string-form-result","message":{"role":"user","content":"thanks"}}"#,
            ],
        );

        // Subagent 1 with meta.json (description)
        let sub_dir = proj.join(parent_uuid).join("subagents");
        write_lines(
            &sub_dir.join("agent-a1b2c3.jsonl"),
            &[
                r#"{"type":"user","isSidechain":true,"sessionId":"55555555-2222-3333-4444-555555555555","agentId":"a1b2c3","message":{"role":"user","content":"Find all auth files"}}"#,
                r#"{"type":"assistant","uuid":"s1","requestId":"rs1","isSidechain":true,"sessionId":"55555555-2222-3333-4444-555555555555","agentId":"a1b2c3","message":{"id":"ms1","model":"claude-haiku-4-5","stop_reason":"end_turn","content":[{"type":"text","text":"Found 3 files"}],"usage":{"input_tokens":200,"cache_read_input_tokens":500,"cache_creation_input_tokens":0,"output_tokens":60}}}"#,
            ],
        );
        fs::write(
            sub_dir.join("agent-a1b2c3.meta.json"),
            r#"{"agentType":"Explore","description":"Audit auth modules"}"#,
        )
        .unwrap();

        // Subagent 2 without meta.json under an empty parent (0 turns, only user prompt + cwd)
        write_lines(
            &proj.join(format!("{empty_parent}.jsonl")),
            &[
                r#"{"type":"user","cwd":"C:/repo/zero-turn-parent","message":{"role":"user","content":"Parent with zero assistant turns"}}"#,
            ],
        );
        let empty_sub_dir = proj.join(empty_parent).join("subagents");
        write_lines(
            &empty_sub_dir.join("agent-z9.jsonl"),
            &[
                r#"{"type":"user","isSidechain":true,"sessionId":"66666666-2222-3333-4444-555555555555","agentId":"z9","message":{"role":"user","content":"Subagent prompt title"}}"#,
                r#"{"type":"assistant","uuid":"sz1","requestId":"rz1","message":{"id":"msz1","model":"claude-haiku-4-5","usage":{"input_tokens":30,"output_tokens":15}}}"#,
            ],
        );

        let mut adapter = ClaudeCodeAdapter::new(root.clone());
        let res = adapter.scan(Utc::now());
        // 1 main + 2 subagents (empty_parent skipped as empty)
        assert_eq!(res.sessions.len(), 3);
        assert_eq!(res.skipped_empty, 1);

        let by_id: HashMap<&str, &ParsedSession> = res
            .sessions
            .iter()
            .map(|s| (s.conversation_id.as_str(), s))
            .collect();

        // Parent has ONLY its own turn (no 80000/19999 double count from toolUseResult)
        let parent = by_id[parent_uuid];
        assert!(!parent.is_subagent);
        assert_eq!(parent.turns.len(), 1);
        assert_eq!(parent.turns[0].input_tokens, 100);
        assert_eq!(parent.turns[0].output_tokens, 40);

        // Subagent 1 linked cleanly & titled from meta.json description
        let sub1_id = format!("{parent_uuid}__a1b2c3");
        let sub1 = by_id[sub1_id.as_str()];
        assert!(sub1.is_subagent);
        assert_eq!(sub1.parent_id.as_deref(), Some(parent_uuid));
        assert_eq!(sub1.title, "Audit auth modules");
        assert_eq!(sub1.workspace, "C:/repo/main");
        assert_eq!(sub1.turns.len(), 1);
        assert_eq!(sub1.turns[0].input_tokens, 200);
        assert_eq!(sub1.turns[0].output_tokens, 60);

        // Zero-turn parent hint lookup works & subagent 2 inherits workspace + falls back to first prompt
        assert_eq!(
            adapter.lookup_session_hint(empty_parent),
            Some((
                "Parent with zero assistant turns".to_string(),
                "C:/repo/zero-turn-parent".to_string()
            ))
        );
        let sub2_id = format!("{empty_parent}__z9");
        let sub2 = by_id[sub2_id.as_str()];
        assert!(sub2.is_subagent);
        assert_eq!(sub2.parent_id.as_deref(), Some(empty_parent));
        assert_eq!(sub2.title, "Subagent prompt title");
        assert_eq!(sub2.workspace, "C:/repo/zero-turn-parent");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_incremental_cache_hit_reparse_and_eviction() {
        let root = temp_test_dir("cache");
        let proj = root.join("projects").join("proj-cache");
        let u1 = "77777777-2222-3333-4444-555555555551";
        let u2 = "77777777-2222-3333-4444-555555555552";
        let p1 = proj.join(format!("{u1}.jsonl"));
        let p2 = proj.join(format!("{u2}.jsonl"));

        write_lines(
            &p1,
            &[r#"{"type":"assistant","uuid":"a1","requestId":"r1","message":{"id":"m1","model":"claude-sonnet-4-5","usage":{"input_tokens":10,"output_tokens":5}}}"#],
        );
        write_lines(
            &p2,
            &[r#"{"type":"assistant","uuid":"a2","requestId":"r2","message":{"id":"m2","model":"claude-sonnet-4-5","usage":{"input_tokens":20,"output_tokens":8}}}"#],
        );

        let mut adapter = ClaudeCodeAdapter::new(root.clone());

        // Scan 1: cold parse
        let r1 = adapter.scan(Utc::now());
        assert_eq!(r1.files_reparsed, 2);
        assert_eq!(r1.files_cached, 0);
        assert_eq!(r1.sessions.len(), 2);

        // Scan 2: warm cache hit
        let r2 = adapter.scan(Utc::now());
        assert_eq!(r2.files_reparsed, 0);
        assert_eq!(r2.files_cached, 2);
        assert_eq!(r2.sessions.len(), 2);

        // Modify p1 (append a second turn -> file size changes immediately)
        write_lines(
            &p1,
            &[
                r#"{"type":"assistant","uuid":"a1","requestId":"r1","message":{"id":"m1","model":"claude-sonnet-4-5","usage":{"input_tokens":10,"output_tokens":5}}}"#,
                r#"{"type":"assistant","uuid":"a1b","requestId":"r1b","message":{"id":"m1b","model":"claude-sonnet-4-5","usage":{"input_tokens":15,"output_tokens":12}}}"#,
            ],
        );
        let r3 = adapter.scan(Utc::now());
        assert_eq!(r3.files_reparsed, 1);
        assert_eq!(r3.files_cached, 1);
        let s1 = r3
            .sessions
            .iter()
            .find(|s| s.conversation_id == u1)
            .unwrap();
        assert_eq!(s1.turns.len(), 2);

        // Delete p2 -> evicted from transcript_cache & safe_fs verified paths
        fs::remove_file(&p2).unwrap();
        let r4 = adapter.scan(Utc::now());
        assert_eq!(r4.files_reparsed, 0);
        assert_eq!(r4.files_cached, 1);
        assert_eq!(r4.sessions.len(), 1);
        assert_eq!(adapter.transcript_cache.len(), 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_whitelist_red_lines_and_symlink_traversal_safety() {
        let root = temp_test_dir("safety");
        let outside = temp_test_dir("safety_outside");
        let proj = root.join("projects").join("proj-safe");
        let valid_uuid = "88888888-2222-3333-4444-555555555555";
        let decoy_line = r#"{"type":"assistant","uuid":"d1","requestId":"rd1","message":{"id":"md1","model":"claude-sonnet-4-5","usage":{"input_tokens":777,"output_tokens":777}}}"#;

        // Plant forbidden red-line files at root and inside projects/ (must NEVER be read)
        write_lines(&root.join("settings.json"), &[decoy_line]);
        write_lines(&root.join("settings.local.json"), &[decoy_line]);
        write_lines(&root.join(".credentials.json"), &[decoy_line]);
        write_lines(&root.join("auth.json"), &[decoy_line]);
        write_lines(&root.join("history.jsonl"), &[decoy_line]);
        write_lines(&root.join("statsig").join("eval.jsonl"), &[decoy_line]);
        write_lines(&root.join("debug").join("trace.jsonl"), &[decoy_line]);

        // Plant non-whitelisted files inside projects/<proj>/
        write_lines(&proj.join("history.jsonl"), &[decoy_line]);
        write_lines(&proj.join("settings.jsonl"), &[decoy_line]);
        write_lines(&proj.join("auth-token.jsonl"), &[decoy_line]);
        write_lines(&proj.join(".hidden.jsonl"), &[decoy_line]);
        write_lines(&proj.join("agent-orphan.jsonl"), &[decoy_line]);
        write_lines(
            &proj.join(valid_uuid).join("tool-results").join("r1.jsonl"),
            &[decoy_line],
        );
        write_lines(
            &proj.join(valid_uuid).join("subagents").join("not-agent.jsonl"),
            &[decoy_line],
        );

        // One legitimate session file
        write_lines(
            &proj.join(format!("{valid_uuid}.jsonl")),
            &[
                r#"{"type":"assistant","uuid":"v1","requestId":"rv1","message":{"id":"mv1","model":"claude-sonnet-4-5","usage":{"input_tokens":11,"output_tokens":7}}}"#,
                "{truncated-json-line",
            ],
        );

        // Outside-root secret JSONL + attempt symlink creation (if OS privileges permit)
        let outside_file = outside.join("99999999-2222-3333-4444-555555555555.jsonl");
        write_lines(&outside_file, &[decoy_line]);
        let outside_proj = outside.join("escaped-proj");
        write_lines(
            &outside_proj.join("99999999-2222-3333-4444-555555555556.jsonl"),
            &[decoy_line],
        );

        #[cfg(windows)]
        {
            let _ = std::os::windows::fs::symlink_file(
                &outside_file,
                proj.join("99999999-2222-3333-4444-555555555555.jsonl"),
            );
            let _ = std::os::windows::fs::symlink_dir(
                &outside_proj,
                root.join("projects").join("symlinked-proj"),
            );
        }
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(
                &outside_file,
                proj.join("99999999-2222-3333-4444-555555555555.jsonl"),
            );
            let _ = std::os::unix::fs::symlink(
                &outside_proj,
                root.join("projects").join("symlinked-proj"),
            );
        }

        let mut adapter = ClaudeCodeAdapter::new(root.clone());
        let res = adapter.scan(Utc::now());
        assert!(res.found_root);
        assert_eq!(res.sessions.len(), 1);
        assert_eq!(res.sessions[0].conversation_id, valid_uuid);
        assert_eq!(res.sessions[0].turns[0].input_tokens, 11);
        assert_eq!(res.sessions[0].turns[0].output_tokens, 7);
        assert_eq!(res.skipped_lines, 1);

        // Direct SafeFs traversal checks (including `..` escape attempt)
        assert!(adapter.safe_fs.is_safe_regular_file(&outside_file).is_none());
        assert!(!adapter.safe_fs.is_safe_dir(&outside_proj));
        let traversal = root
            .join("projects")
            .join("..")
            .join("..")
            .join(outside.file_name().unwrap())
            .join("99999999-2222-3333-4444-555555555555.jsonl");
        assert!(adapter.safe_fs.is_safe_regular_file(&traversal).is_none());

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn test_out_of_order_streaming_max_output_parallel_tools_and_missing_root() {
        let root = temp_test_dir("edge_cases");
        let mut adapter = ClaudeCodeAdapter::new(root.clone());

        // Missing `projects/` directory -> found_root == false
        let r0 = adapter.scan(Utc::now());
        assert!(!r0.found_root);
        assert!(r0.sessions.is_empty());

        let proj = root.join("projects").join("proj-edge");
        let uuid = "aaaaaaaa-2222-3333-4444-555555555555";
        write_lines(
            &proj.join(format!("{uuid}.jsonl")),
            &[
                // Chunk 1: larger output_tokens=250 + two distinct Read tool_use IDs
                r#"{"type":"assistant","uuid":"u1","requestId":"rq1","timestamp":"2026-11-24T12:00:05Z","message":{"id":"mg1","model":"claude-opus-4-5","stop_reason":"tool_use","content":[{"type":"thinking","thinking":""},{"type":"tool_use","id":"t_a","name":"Read"},{"type":"tool_use","id":"t_b","name":"Read"}],"usage":{"input_tokens":300,"cache_read_input_tokens":4000,"cache_creation_input_tokens":0,"output_tokens":250}}}"#,
                // Chunk 2 (out-of-order smaller duplicate): output_tokens=12, repeats t_a
                r#"{"type":"assistant","uuid":"u2","requestId":"rq1","timestamp":"2026-11-24T12:00:02Z","message":{"id":"mg1","model":"claude-opus-4-5","stop_reason":null,"content":[{"type":"tool_use","id":"t_a","name":"Read"}],"usage":{"input_tokens":300,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":12}}}"#,
            ],
        );

        // Subagent whose meta.json only provides agentType (no description)
        let sub_dir = proj.join(uuid).join("subagents");
        write_lines(
            &sub_dir.join("agent-plan1.jsonl"),
            &[
                r#"{"type":"assistant","uuid":"sp1","requestId":"rsp1","message":{"id":"msp1","model":"claude-haiku-4-5","usage":{"input_tokens":50,"output_tokens":20}}}"#,
            ],
        );
        fs::write(sub_dir.join("agent-plan1.meta.json"), r#"{"agentType":"Plan"}"#).unwrap();

        let r1 = adapter.scan(Utc::now());
        assert!(r1.found_root);
        assert_eq!(r1.sessions.len(), 2);

        let main = r1
            .sessions
            .iter()
            .find(|s| s.conversation_id == uuid)
            .unwrap();
        assert_eq!(main.turns.len(), 1);
        let t = &main.turns[0];
        assert_eq!(t.output_tokens, 250);
        assert_eq!(t.cache_read_tokens, 4000);
        assert_eq!(t.tool_names, vec!["Read".to_string(), "Read".to_string()]);
        assert_eq!(t.thinking_chars, 1);
        assert!(t.has_thinking());

        let sub = r1
            .sessions
            .iter()
            .find(|s| s.conversation_id == format!("{uuid}__plan1"))
            .unwrap();
        assert_eq!(sub.title, "Plan");

        // Deleting `projects/` clears caches and flips found_root back to false
        fs::remove_dir_all(root.join("projects")).unwrap();
        let r2 = adapter.scan(Utc::now());
        assert!(!r2.found_root);
        assert!(r2.sessions.is_empty());
        assert!(adapter.transcript_cache.is_empty());
        assert!(adapter.meta_cache.is_empty());

        let _ = fs::remove_dir_all(&root);
    }
}
