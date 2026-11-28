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
    clean_single_line, compute_running_hint, non_symlink_dir, read_header_fingerprint,
    try_seek_for_tail, AdapterPoll, CharCount, FileSig, FlexTimestamp, FlexU64, HeaderFingerprint,
    InternedStrSeed, OptFlexBool, OptFlexString, ParsedSession, SafeFs, SourceAdapter,
    SourceScanResult,
};
use crate::stats::{intern_str, SourceKind, TurnRecord};

const MAX_INDEX_BYTES: usize = 16 * 1024 * 1024;
const MAX_META_BYTES: usize = 1024 * 1024;
const UNKNOWN_WORKSPACE: &str = "(未知工作区)";
/// Keep incremental dedup map in RAM only for files modified within the last 15 minutes
/// (cold files drop the map to save RAM and fall back to byte-0 if ever resumed later).
const ACTIVE_TAIL_RETENTION_SECS: i64 = 900;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DedupKey {
    MsgReq(String, String),
    MsgOnly(String),
    Uuid(String),
    Line(usize),
}

#[derive(Debug, Clone)]
struct DedupTurnAcc {
    first_seq: usize,
    timestamp_utc: Option<DateTime<Utc>>,
    model: Arc<str>,
    has_usage: bool,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    cache_write_1h_tokens: u64,
    output_tokens: u64,
    tool_names: Vec<Arc<str>>,
    seen_tool_ids: HashSet<String>,
    thinking_chars: u64,
}

#[derive(Debug, Clone)]
struct TailDedupState {
    by_key: HashMap<DedupKey, DedupTurnAcc>,
    next_seq: usize,
    line_no: usize,
}

#[derive(Debug, Clone)]
struct CachedClaudeTranscript {
    sig: FileSig,
    header: HeaderFingerprint,
    committed_offset: u64,
    turns: Arc<[TurnRecord]>,
    cwd: String,
    custom_title: String,
    summary: String,
    first_user_preview: String,
    last_step_running: bool,
    committed_skipped_lines: usize,
    trailing_skipped_lines: usize,
    tail_state: Option<TailDedupState>,
}

impl CachedClaudeTranscript {
    fn total_skipped_lines(&self) -> usize {
        self.committed_skipped_lines + self.trailing_skipped_lines
    }
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
    has_scanned: bool,
    last_found_root: bool,
}

impl ClaudeCodeAdapter {
    pub fn new(root: PathBuf) -> Self {
        Self {
            safe_fs: SafeFs::new(root),
            transcript_cache: HashMap::new(),
            index_cache: HashMap::new(),
            meta_cache: HashMap::new(),
            session_hints: HashMap::new(),
            has_scanned: false,
            last_found_root: false,
        }
    }

    /// Refreshes `<proj_dir>/sessions-index.json` in place without cloning the HashMap.
    /// Returns `(Some(index_path), changed)` when present.
    fn refresh_project_index(
        &mut self,
        proj_dir: &Path,
        seen_paths: &mut HashSet<PathBuf>,
    ) -> (Option<PathBuf>, bool) {
        let index_path = proj_dir.join("sessions-index.json");
        let Some((sig, freshly_canon)) = self.safe_fs.is_safe_regular_file_checked(&index_path)
        else {
            let removed = self.index_cache.remove(&index_path).is_some();
            return (None, removed);
        };
        seen_paths.insert(index_path.clone());
        if let Some(cached) = self.index_cache.get(&index_path) {
            if cached.sig == sig {
                return (Some(index_path), false);
            }
        }
        if let Some(text) = self.safe_fs.read_for_read_limited(
            &index_path,
            sig,
            MAX_INDEX_BYTES,
            freshly_canon,
        ) {
            let parsed = parse_sessions_index(&text, sig);
            self.index_cache.insert(index_path.clone(), parsed);
            (Some(index_path), true)
        } else {
            (None, false)
        }
    }

    /// Refreshes `agent-<id>.meta.json` in place without cloning. Returns `(Some(meta_path), changed)`.
    fn refresh_subagent_meta(
        &mut self,
        subagents_dir: &Path,
        agent_id: &str,
        seen_paths: &mut HashSet<PathBuf>,
    ) -> (Option<PathBuf>, bool) {
        let meta_path = subagents_dir.join(format!("agent-{agent_id}.meta.json"));
        let Some((sig, freshly_canon)) = self.safe_fs.is_safe_regular_file_checked(&meta_path)
        else {
            let removed = self.meta_cache.remove(&meta_path).is_some();
            return (None, removed);
        };
        seen_paths.insert(meta_path.clone());
        if let Some(cached) = self.meta_cache.get(&meta_path) {
            if cached.sig == sig {
                return (Some(meta_path), false);
            }
        }
        if let Some(text) =
            self.safe_fs
                .read_for_read_limited(&meta_path, sig, MAX_META_BYTES, freshly_canon)
        {
            let parsed = parse_subagent_meta(&text, sig);
            self.meta_cache.insert(meta_path.clone(), parsed);
            (Some(meta_path), true)
        } else {
            (None, false)
        }
    }

    /// Ensures `path` is up-to-date in `self.transcript_cache` (via byte-offset tailing when possible).
    /// Uses `DirEntry` metadata with zero extra syscalls on Windows for already-verified paths.
    /// Returns `(present, reparsed)`.
    fn refresh_transcript_entry(
        &mut self,
        path: &Path,
        entry: &fs::DirEntry,
        scan_now_utc: DateTime<Utc>,
        seen_paths: &mut HashSet<PathBuf>,
        files_cached: &mut usize,
        files_reparsed: &mut usize,
    ) -> (bool, bool) {
        let Some((sig, freshly_canon)) = self.safe_fs.stat_verified_dir_entry_checked(path, entry)
        else {
            return (false, false);
        };
        seen_paths.insert(path.to_path_buf());
        if let Some(cached) = self.transcript_cache.get_mut(path) {
            if cached.sig == sig {
                *files_cached += 1;
                // Release active tail dedup state once file becomes cold to keep steady-state RAM minimal.
                if cached.tail_state.is_some() {
                    if let Some(mt) = sig.mtime_utc() {
                        if (scan_now_utc - mt).num_seconds() > ACTIVE_TAIL_RETENTION_SECS {
                            cached.tail_state = None;
                        }
                    }
                }
                return (true, false);
            }
        }
        let Some(fh) = self.safe_fs.open_for_read(path, freshly_canon) else {
            return (false, false);
        };
        let prev = self.transcript_cache.remove(path);
        let retain_tail = sig
            .mtime_utc()
            .map(|mt| (scan_now_utc - mt).num_seconds() <= ACTIVE_TAIL_RETENTION_SECS)
            .unwrap_or(false);
        let parsed = parse_or_tail_claude_jsonl(fh, sig, prev, retain_tail);
        *files_reparsed += 1;
        self.transcript_cache.insert(path.to_path_buf(), parsed);
        (true, true)
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

    fn scan_internal(&mut self, scan_now_utc: DateTime<Utc>, skip_if_unchanged: bool) -> AdapterPoll {
        self.safe_fs.refresh_root();
        let projects_dir = self.safe_fs.root().join("projects");
        if !self.safe_fs.is_safe_dir(&projects_dir) {
            let changed = !self.has_scanned
                || self.last_found_root
                || !self.transcript_cache.is_empty()
                || !self.index_cache.is_empty()
                || !self.meta_cache.is_empty();
            self.transcript_cache.clear();
            self.index_cache.clear();
            self.meta_cache.clear();
            self.session_hints.clear();
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
            HashSet::with_capacity(self.transcript_cache.len() + 8);
        let mut seen_dirs: HashSet<PathBuf> = HashSet::with_capacity(16);
        seen_dirs.insert(projects_dir.clone());

        let Ok(proj_entries) = fs::read_dir(&projects_dir) else {
            self.has_scanned = true;
            self.last_found_root = true;
            return AdapterPoll::Changed(SourceScanResult {
                found_root: true,
                ..Default::default()
            });
        };
        let mut proj_dirs: Vec<PathBuf> = proj_entries
            .flatten()
            .filter_map(|e| {
                let ft = e.file_type().ok()?;
                (!ft.is_symlink() && ft.is_dir()).then(|| e.path())
            })
            .collect();
        proj_dirs.sort();

        let mut any_changed = !self.has_scanned || !self.last_found_root;
        let mut files_cached = 0usize;
        let mut files_reparsed = 0usize;
        let mut skipped_empty = 0usize;
        let mut skipped_lines = 0usize;

        struct ProjScanPlan {
            index_path: Option<PathBuf>,
            main_files: Vec<(String, PathBuf)>,
            sub_files: Vec<(String, String, PathBuf, Option<PathBuf>)>,
        }
        let mut plans: Vec<ProjScanPlan> = Vec::with_capacity(proj_dirs.len());

        for proj_dir in proj_dirs {
            if !self.safe_fs.is_safe_dir(&proj_dir) {
                continue;
            }
            seen_dirs.insert(proj_dir.clone());

            let (index_path, idx_changed) = self.refresh_project_index(&proj_dir, &mut seen_paths);
            if idx_changed {
                any_changed = true;
            }

            let Ok(entries) = fs::read_dir(&proj_dir) else {
                continue;
            };
            let mut children: Vec<(PathBuf, fs::DirEntry)> =
                entries.flatten().map(|e| (e.path(), e)).collect();
            children.sort_by(|a, b| a.0.cmp(&b.0));

            let mut main_files: Vec<(String, PathBuf)> = Vec::new();
            let mut subagent_parents: Vec<(String, PathBuf)> = Vec::new();

            for (path, entry) in children {
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if name == "sessions-index.json" {
                    continue;
                }
                if let Some(stem) = name.strip_suffix(".jsonl") {
                    if is_allowed_session_stem(stem) {
                        let (ok, reparsed) = self.refresh_transcript_entry(
                            &path,
                            &entry,
                            scan_now_utc,
                            &mut seen_paths,
                            &mut files_cached,
                            &mut files_reparsed,
                        );
                        if reparsed {
                            any_changed = true;
                        }
                        if ok {
                            if let Some(c) = self.transcript_cache.get(&path) {
                                skipped_lines += c.total_skipped_lines();
                                if c.turns.is_empty() {
                                    skipped_empty += 1;
                                }
                            }
                            main_files.push((stem.to_string(), path));
                        }
                    }
                } else if is_allowed_session_stem(name)
                    && entry
                        .file_type()
                        .map(|ft| !ft.is_symlink() && ft.is_dir())
                        .unwrap_or(false)
                    && self.safe_fs.is_safe_dir(&path)
                {
                    seen_dirs.insert(path.clone());
                    subagent_parents.push((name.to_string(), path));
                }
            }

            let mut sub_files: Vec<(String, String, PathBuf, Option<PathBuf>)> = Vec::new();
            for (parent_uuid, session_dir) in subagent_parents {
                let subagents_dir = session_dir.join("subagents");
                if !self.safe_fs.is_safe_dir(&subagents_dir) {
                    continue;
                }
                seen_dirs.insert(subagents_dir.clone());
                let Ok(sub_entries) = fs::read_dir(&subagents_dir) else {
                    continue;
                };
                let mut sub_paths: Vec<(PathBuf, fs::DirEntry)> =
                    sub_entries.flatten().map(|e| (e.path(), e)).collect();
                sub_paths.sort_by(|a, b| a.0.cmp(&b.0));

                for (sub_path, sub_entry) in sub_paths {
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
                    let (ok, reparsed) = self.refresh_transcript_entry(
                        &sub_path,
                        &sub_entry,
                        scan_now_utc,
                        &mut seen_paths,
                        &mut files_cached,
                        &mut files_reparsed,
                    );
                    if reparsed {
                        any_changed = true;
                    }
                    if !ok {
                        continue;
                    }
                    let (meta_path, meta_changed) =
                        self.refresh_subagent_meta(&subagents_dir, agent_id, &mut seen_paths);
                    if meta_changed {
                        any_changed = true;
                    }
                    if let Some(c) = self.transcript_cache.get(&sub_path) {
                        skipped_lines += c.total_skipped_lines();
                        if c.turns.is_empty() {
                            skipped_empty += 1;
                            continue;
                        }
                    }
                    sub_files.push((parent_uuid.clone(), agent_id.to_string(), sub_path, meta_path));
                }
            }

            plans.push(ProjScanPlan {
                index_path,
                main_files,
                sub_files,
            });
        }

        let prev_t = self.transcript_cache.len();
        let prev_i = self.index_cache.len();
        let prev_m = self.meta_cache.len();
        self.transcript_cache.retain(|k, _| seen_paths.contains(k));
        self.index_cache.retain(|k, _| seen_paths.contains(k));
        self.meta_cache.retain(|k, _| seen_paths.contains(k));
        self.safe_fs.retain_verified(&seen_paths);
        self.safe_fs.retain_verified_dirs(&seen_dirs);
        if self.transcript_cache.len() != prev_t
            || self.index_cache.len() != prev_i
            || self.meta_cache.len() != prev_m
        {
            any_changed = true;
        }

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
        let mut sessions = Vec::new();

        for plan in plans {
            let proj_index = plan
                .index_path
                .as_ref()
                .and_then(|p| self.index_cache.get(p));
            // Collect hints from index without borrowing self mutably while reading index_cache.
            let mut pending_hints: Vec<(String, String, String)> = Vec::new();
            if let Some(idx) = proj_index {
                for (sid, entry) in &idx.entries {
                    let title = index_entry_title(entry);
                    let ws = if !entry.workspace.is_empty() {
                        entry.workspace.as_str()
                    } else {
                        idx.original_path.as_str()
                    };
                    if !title.is_empty() || !ws.is_empty() {
                        pending_hints.push((sid.clone(), title.to_string(), ws.to_string()));
                    }
                }
            }
            for (sid, t, w) in pending_hints {
                self.record_hint(&sid, &t, &w);
            }

            // Pass 1: Main sessions
            for (session_id, path) in plan.main_files {
                let (
                    raw_title,
                    raw_workspace,
                    turns_arc,
                    last_file_mtime_utc,
                    last_step_running,
                ) = {
                    let Some(cached) = self.transcript_cache.get(&path) else {
                        continue;
                    };
                    let idx = plan
                        .index_path
                        .as_ref()
                        .and_then(|p| self.index_cache.get(p));
                    let idx_entry = idx.and_then(|i| i.entries.get(&session_id));
                    let idx_orig_ws = idx.map(|i| i.original_path.as_str()).unwrap_or("");
                    (
                        resolve_main_title(cached, idx_entry).to_string(),
                        resolve_main_workspace(cached, idx_entry, idx_orig_ws).to_string(),
                        Arc::clone(&cached.turns),
                        cached.sig.mtime_utc(),
                        cached.last_step_running,
                    )
                };
                self.record_hint(&session_id, &raw_title, &raw_workspace);
                if turns_arc.is_empty() {
                    continue;
                }
                let title = if !raw_title.is_empty() {
                    raw_title
                } else {
                    session_id.clone()
                };
                let workspace = if !raw_workspace.is_empty() {
                    raw_workspace
                } else {
                    UNKNOWN_WORKSPACE.to_string()
                };
                let is_running_hint =
                    compute_running_hint(last_step_running, last_file_mtime_utc, scan_now_utc);
                sessions.push(ParsedSession {
                    conversation_id: session_id,
                    title,
                    workspace,
                    source: SourceKind::ClaudeCode,
                    is_subagent: false,
                    parent_id: None,
                    parent_title: String::new(),
                    turns: turns_arc,
                    last_file_mtime_utc,
                    last_step_running,
                    is_running_hint,
                });
            }

            // Pass 2: Subagent sessions
            for (parent_uuid, agent_id, sub_path, meta_path) in plan.sub_files {
                let Some(cached) = self.transcript_cache.get(&sub_path) else {
                    continue;
                };
                if cached.turns.is_empty() {
                    continue;
                }
                let meta = meta_path.as_ref().and_then(|p| self.meta_cache.get(p));
                let idx_orig = plan
                    .index_path
                    .as_ref()
                    .and_then(|p| self.index_cache.get(p))
                    .map(|i| i.original_path.as_str())
                    .unwrap_or("");
                let conversation_id = format!("{parent_uuid}__{agent_id}");
                let title = resolve_subagent_title(cached, meta, &conversation_id);
                let workspace = if !cached.cwd.is_empty() {
                    cached.cwd.clone()
                } else if let Some((_, pws)) = self.session_hints.get(&parent_uuid) {
                    if !pws.is_empty() {
                        pws.clone()
                    } else {
                        UNKNOWN_WORKSPACE.to_string()
                    }
                } else if !idx_orig.is_empty() {
                    idx_orig.to_string()
                } else {
                    UNKNOWN_WORKSPACE.to_string()
                };
                let last_file_mtime_utc = cached.sig.mtime_utc();
                let is_running_hint = compute_running_hint(
                    cached.last_step_running,
                    last_file_mtime_utc,
                    scan_now_utc,
                );
                sessions.push(ParsedSession {
                    conversation_id,
                    title,
                    workspace,
                    source: SourceKind::ClaudeCode,
                    is_subagent: true,
                    parent_id: Some(parent_uuid),
                    parent_title: String::new(),
                    turns: Arc::clone(&cached.turns),
                    last_file_mtime_utc,
                    last_step_running: cached.last_step_running,
                    is_running_hint,
                });
            }
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

impl SourceAdapter for ClaudeCodeAdapter {
    fn kind(&self) -> SourceKind {
        SourceKind::ClaudeCode
    }

    fn root(&self) -> &Path {
        self.safe_fs.root()
    }

    fn watched_dirs(&self) -> Vec<PathBuf> {
        let p = self.safe_fs.root().join("projects");
        if non_symlink_dir(&p) {
            vec![p]
        } else {
            Vec::new()
        }
    }

    fn lookup_session_hint(&mut self, cid: &str) -> Option<(String, String)> {
        self.session_hints.get(cid).cloned()
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
        for entry in self.transcript_cache.values_mut() {
            if Arc::ptr_eq(&entry.turns, old_turns) {
                entry.turns = Arc::clone(priced_turns);
                break;
            }
        }
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

// ---------- Zero-DOM Serde Streaming Parser + Byte-Offset Tailing for Claude Code JSONL ----------

struct RawClaudeUsage {
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    cache_write_1h_tokens: u64,
    output_tokens: u64,
}

struct RawClaudeMessage {
    id: Option<String>,
    model: Option<Arc<str>>,
    is_api_error: bool,
    stop_reason: Option<String>,
    usage: Option<RawClaudeUsage>,
    tools: Vec<(Option<String>, Arc<str>)>,
    thinking_chars: u64,
    user_text: Option<String>,
}

struct RawClaudeLine {
    rec_type: Option<String>,
    cwd: Option<String>,
    custom_title: Option<String>,
    summary: Option<String>,
    is_meta: bool,
    is_api_error: bool,
    request_id: Option<String>,
    uuid: Option<String>,
    timestamp_utc: Option<DateTime<Utc>>,
    message: Option<RawClaudeMessage>,
}

struct ClaudeLineSeed {
    need_cwd: bool,
    need_user_preview: bool,
    /// When false (e.g. a `"user"` tool_result line after `cwd` & `first_user_preview` are known),
    /// skips `message` completely via `IgnoredAny` (0 heap allocations!).
    parse_message: bool,
}

impl<'de> de::DeserializeSeed<'de> for ClaudeLineSeed {
    type Value = RawClaudeLine;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V {
            need_cwd: bool,
            need_user_preview: bool,
            parse_message: bool,
        }
        impl<'de> Visitor<'de> for V {
            type Value = RawClaudeLine;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Claude Code JSONL object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = RawClaudeLine {
                    rec_type: None,
                    cwd: None,
                    custom_title: None,
                    summary: None,
                    is_meta: false,
                    is_api_error: false,
                    request_id: None,
                    uuid: None,
                    timestamp_utc: None,
                    message: None,
                };
                while let Some(key) = map.next_key::<&str>()? {
                    match key {
                        "type" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                out.rec_type = Some(s);
                            }
                        }
                        "cwd" if self.need_cwd => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let t = s.trim();
                                if !t.is_empty() {
                                    out.cwd = Some(t.to_string());
                                }
                            }
                        }
                        "customTitle" | "custom_title" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                if out.custom_title.is_none() || key == "customTitle" {
                                    out.custom_title = Some(s);
                                }
                            }
                        }
                        "summary" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                out.summary = Some(s);
                            }
                        }
                        "isMeta" => {
                            if let OptFlexBool(Some(b)) = map.next_value()? {
                                out.is_meta = b;
                            }
                        }
                        "isApiErrorMessage" => {
                            if let OptFlexBool(Some(b)) = map.next_value()? {
                                out.is_api_error = b;
                            }
                        }
                        "requestId" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let t = s.trim();
                                if !t.is_empty() {
                                    out.request_id = Some(t.to_string());
                                }
                            }
                        }
                        "uuid" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let t = s.trim();
                                if !t.is_empty() {
                                    out.uuid = Some(t.to_string());
                                }
                            }
                        }
                        "timestamp" => {
                            let FlexTimestamp(ts) = map.next_value()?;
                            out.timestamp_utc = ts;
                        }
                        "message" if self.parse_message => {
                            out.message = map.next_value_seed(ClaudeMessageSeed {
                                capture_user_text: self.need_user_preview,
                            })?;
                        }
                        // Skip `toolUseResult` and all other unknown fields with zero heap allocs!
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(out)
            }
        }
        deserializer.deserialize_map(V {
            need_cwd: self.need_cwd,
            need_user_preview: self.need_user_preview,
            parse_message: self.parse_message,
        })
    }
}

struct ClaudeMessageSeed {
    capture_user_text: bool,
}

impl<'de> de::DeserializeSeed<'de> for ClaudeMessageSeed {
    type Value = Option<RawClaudeMessage>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V {
            capture_user_text: bool,
        }
        impl<'de> Visitor<'de> for V {
            type Value = Option<RawClaudeMessage>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Claude message object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut msg = RawClaudeMessage {
                    id: None,
                    model: None,
                    is_api_error: false,
                    stop_reason: None,
                    usage: None,
                    tools: Vec::new(),
                    thinking_chars: 0,
                    user_text: None,
                };
                while let Some(key) = map.next_key::<&str>()? {
                    match key {
                        "id" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let t = s.trim();
                                if !t.is_empty() {
                                    msg.id = Some(t.to_string());
                                }
                            }
                        }
                        "model" => {
                            msg.model = map.next_value_seed(InternedStrSeed)?;
                        }
                        "isApiErrorMessage" => {
                            if let OptFlexBool(Some(b)) = map.next_value()? {
                                msg.is_api_error = b;
                            }
                        }
                        "stop_reason" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                msg.stop_reason = Some(s);
                            }
                        }
                        "usage" => {
                            msg.usage = map.next_value_seed(ClaudeUsageSeed)?;
                        }
                        "content" => {
                            let (tools, th_chars, u_txt) =
                                map.next_value_seed(ClaudeContentSeed {
                                    capture_user_text: self.capture_user_text,
                                })?;
                            msg.tools = tools;
                            msg.thinking_chars = th_chars;
                            msg.user_text = u_txt;
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(Some(msg))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }
        deserializer.deserialize_any(V {
            capture_user_text: self.capture_user_text,
        })
    }
}

struct ClaudeUsageSeed;

impl<'de> de::DeserializeSeed<'de> for ClaudeUsageSeed {
    type Value = Option<RawClaudeUsage>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<RawClaudeUsage>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Claude usage object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut input = 0u64;
                let mut cache_read = 0u64;
                let mut cache_write = 0u64;
                let mut raw_1h = 0u64;
                let mut output = 0u64;
                while let Some(k) = map.next_key::<&str>()? {
                    match k {
                        "input_tokens" => {
                            let FlexU64(v) = map.next_value()?;
                            input = v;
                        }
                        "cache_read_input_tokens" => {
                            let FlexU64(v) = map.next_value()?;
                            cache_read = v;
                        }
                        "cache_creation_input_tokens" => {
                            let FlexU64(v) = map.next_value()?;
                            cache_write = v;
                        }
                        "cache_creation" => {
                            raw_1h = map.next_value_seed(ClaudeCacheCreationSeed)?;
                        }
                        "output_tokens" => {
                            let FlexU64(v) = map.next_value()?;
                            output = v;
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(Some(RawClaudeUsage {
                    input_tokens: input,
                    cache_read_tokens: cache_read,
                    cache_write_tokens: cache_write,
                    cache_write_1h_tokens: raw_1h.min(cache_write),
                    output_tokens: output,
                }))
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

struct ClaudeCacheCreationSeed;

impl<'de> de::DeserializeSeed<'de> for ClaudeCacheCreationSeed {
    type Value = u64;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = u64;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("cache_creation object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut h1 = 0u64;
                while let Some(k) = map.next_key::<&str>()? {
                    if k == "ephemeral_1h_input_tokens" {
                        let FlexU64(v) = map.next_value()?;
                        h1 = v;
                    } else {
                        let _: IgnoredAny = map.next_value()?;
                    }
                }
                Ok(h1)
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(0)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(0)
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct ClaudeContentSeed {
    capture_user_text: bool,
}

impl<'de> de::DeserializeSeed<'de> for ClaudeContentSeed {
    type Value = (Vec<(Option<String>, Arc<str>)>, u64, Option<String>);
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V {
            capture_user_text: bool,
        }
        impl<'de> Visitor<'de> for V {
            type Value = (Vec<(Option<String>, Arc<str>)>, u64, Option<String>);
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Claude message content")
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                let txt = if self.capture_user_text {
                    sanitize_user_snippet(v)
                } else {
                    None
                };
                Ok((Vec::new(), 0, txt))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut tools = Vec::new();
                let mut thinking_chars = 0u64;
                let mut user_text: Option<String> = None;
                while let Some(block) = seq.next_element_seed(ClaudeBlockSeed {
                    capture_user_text: self.capture_user_text && user_text.is_none(),
                })? {
                    match block {
                        ClaudeBlockResult::Tool(tid, name) => tools.push((tid, name)),
                        ClaudeBlockResult::Thinking(c) => {
                            thinking_chars = thinking_chars.saturating_add(c);
                        }
                        ClaudeBlockResult::UserText(t) => {
                            if user_text.is_none() {
                                user_text = Some(t);
                            }
                        }
                        ClaudeBlockResult::None => {}
                    }
                }
                Ok((tools, thinking_chars, user_text))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok((Vec::new(), 0, None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok((Vec::new(), 0, None))
            }
        }
        deserializer.deserialize_any(V {
            capture_user_text: self.capture_user_text,
        })
    }
}

enum ClaudeBlockResult {
    Tool(Option<String>, Arc<str>),
    Thinking(u64),
    UserText(String),
    None,
}

struct ClaudeBlockSeed {
    capture_user_text: bool,
}

impl<'de> de::DeserializeSeed<'de> for ClaudeBlockSeed {
    type Value = ClaudeBlockResult;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V {
            capture_user_text: bool,
        }
        impl<'de> Visitor<'de> for V {
            type Value = ClaudeBlockResult;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Claude content block")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut btype: Option<String> = None;
                let mut id: Option<String> = None;
                let mut name: Option<Arc<str>> = None;
                let mut thinking_chars: Option<u64> = None;
                let mut text_chars: Option<u64> = None;
                let mut user_text: Option<String> = None;

                while let Some(k) = map.next_key::<&str>()? {
                    match k {
                        "type" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                btype = Some(s);
                            }
                        }
                        "id" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let t = s.trim();
                                if !t.is_empty() {
                                    id = Some(t.to_string());
                                }
                            }
                        }
                        "name" => {
                            name = map.next_value_seed(InternedStrSeed)?;
                        }
                        "thinking" => {
                            let CharCount(c) = map.next_value()?;
                            thinking_chars = Some(c);
                        }
                        "text" => {
                            if self.capture_user_text
                                && !matches!(btype.as_deref(), Some("thinking" | "tool_use"))
                            {
                                if let OptFlexString(Some(s)) = map.next_value()? {
                                    text_chars = Some(s.chars().count() as u64);
                                    user_text = sanitize_user_snippet(&s);
                                }
                            } else {
                                let CharCount(c) = map.next_value()?;
                                text_chars = Some(c);
                            }
                        }
                        // Skip `input`, `content`, `data`, `signature`, etc. with 0 allocs!
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                match btype.as_deref().unwrap_or("") {
                    "tool_use" => match name {
                        Some(n) => Ok(ClaudeBlockResult::Tool(id, n)),
                        None => Ok(ClaudeBlockResult::None),
                    },
                    "thinking" => {
                        let chars = thinking_chars.or(text_chars).unwrap_or(0).max(1);
                        Ok(ClaudeBlockResult::Thinking(chars))
                    }
                    "redacted_thinking" => Ok(ClaudeBlockResult::Thinking(1)),
                    "text" if self.capture_user_text => match user_text {
                        Some(t) => Ok(ClaudeBlockResult::UserText(t)),
                        None => Ok(ClaudeBlockResult::None),
                    },
                    _ => Ok(ClaudeBlockResult::None),
                }
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(ClaudeBlockResult::None)
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(ClaudeBlockResult::None)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(ClaudeBlockResult::None)
            }
        }
        deserializer.deserialize_any(V {
            capture_user_text: self.capture_user_text,
        })
    }
}

fn parse_or_tail_claude_jsonl(
    mut fh: File,
    sig: FileSig,
    prev: Option<CachedClaudeTranscript>,
    retain_tail: bool,
) -> CachedClaudeTranscript {
    if let Some(mut prev_state) = prev {
        if let Some(tail_dedup) = prev_state.tail_state.take() {
            if let Some(fresh_hdr) = try_seek_for_tail(
                &mut fh,
                prev_state.sig,
                sig,
                prev_state.header,
                prev_state.committed_offset,
            ) {
                return parse_claude_from_offset(
                    fh,
                    sig,
                    fresh_hdr,
                    prev_state.committed_offset,
                    tail_dedup,
                    prev_state.cwd,
                    prev_state.custom_title,
                    prev_state.summary,
                    prev_state.first_user_preview,
                    prev_state.last_step_running,
                    prev_state.committed_skipped_lines,
                    retain_tail,
                );
            }
        }
    }
    let header = read_header_fingerprint(&mut fh).unwrap_or_default();
    let _ = fh.seek(SeekFrom::Start(0));
    parse_claude_from_offset(
        fh,
        sig,
        header,
        0,
        TailDedupState {
            by_key: HashMap::new(),
            next_seq: 0,
            line_no: 0,
        },
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        false,
        0,
        retain_tail,
    )
}

#[allow(clippy::too_many_arguments)]
fn parse_claude_from_offset(
    fh: File,
    sig: FileSig,
    header: HeaderFingerprint,
    start_offset: u64,
    mut dedup: TailDedupState,
    mut cwd: String,
    mut custom_title: String,
    mut summary: String,
    mut first_user_preview: String,
    mut last_step_running: bool,
    mut committed_skipped_lines: usize,
    retain_tail: bool,
) -> CachedClaudeTranscript {
    let mut reader = BufReader::with_capacity(64 * 1024, fh);
    let mut buf = Vec::with_capacity(8 * 1024);
    let mut offset = start_offset;
    let mut committed_offset = start_offset;
    let mut trailing_skipped_lines = 0usize;

    let f_asst = memmem::Finder::new(b"\"assistant\"");
    let f_user = memmem::Finder::new(b"\"user\"");
    let f_title = memmem::Finder::new(b"\"custom-title\"");
    let f_sum = memmem::Finder::new(b"\"summary\"");
    let f_cwd = memmem::Finder::new(b"\"cwd\"");

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
        dedup.line_no += 1;

        let line_cow = String::from_utf8_lossy(&buf);
        let mut trimmed = line_cow.trim();
        if dedup.line_no == 1 {
            trimmed = trimmed.trim_start_matches('\u{feff}').trim();
        }
        if trimmed.is_empty() {
            if has_newline {
                committed_offset = offset;
            }
            continue;
        }

        let b = trimmed.as_bytes();
        let well_formed = b.first() == Some(&b'{') && b.last() == Some(&b'}');
        let has_asst = f_asst.find(b).is_some();
        let has_user = f_user.find(b).is_some();
        let has_title = f_title.find(b).is_some();
        let has_sum = f_sum.find(b).is_some();
        let need_cwd = cwd.is_empty() && f_cwd.find(b).is_some();

        if well_formed && !(has_asst || has_user || has_title || has_sum || need_cwd) {
            if has_newline {
                committed_offset = offset;
            }
            continue;
        }

        // Massive win on Claude Code `tool_result` `"user"` lines once `cwd` & `first_user_preview`
        // are already known: skip deserializing `message` (and `toolUseResult`) completely!
        let parse_message = has_asst || (has_user && first_user_preview.is_empty());
        let seed = ClaudeLineSeed {
            need_cwd: cwd.is_empty(),
            need_user_preview: first_user_preview.is_empty(),
            parse_message,
        };
        let mut de = serde_json::Deserializer::from_str(trimmed);
        let Ok(parsed) = de::DeserializeSeed::deserialize(seed, &mut de) else {
            if has_newline && offset < sig.len {
                committed_skipped_lines += 1;
                committed_offset = offset;
            } else {
                trailing_skipped_lines += 1;
            }
            continue;
        };
        if has_newline {
            committed_offset = offset;
        }

        if cwd.is_empty() {
            if let Some(c) = parsed.cwd {
                cwd = c;
            }
        }

        let rec_type = parsed.rec_type.as_deref().unwrap_or("");
        match rec_type {
            "custom-title" => {
                if let Some(t) = parsed.custom_title {
                    let clean = clean_single_line(&t, 100);
                    if !clean.is_empty() {
                        custom_title = clean;
                    }
                }
                continue;
            }
            "summary" => {
                if let Some(s) = parsed.summary {
                    let clean = clean_single_line(&s, 100);
                    if !clean.is_empty() {
                        summary = clean;
                    }
                }
                continue;
            }
            "user" => {
                last_step_running = true;
                if first_user_preview.is_empty() && !parsed.is_meta {
                    if let Some(snippet) = parsed.message.and_then(|m| m.user_text) {
                        first_user_preview = snippet;
                    }
                }
                continue;
            }
            "assistant" => {}
            _ => continue,
        }

        if parsed.is_api_error {
            continue;
        }
        let Some(message) = parsed.message else {
            continue;
        };
        if message.is_api_error {
            continue;
        }
        if let Some(m) = message.model.as_deref() {
            if m.eq_ignore_ascii_case("<synthetic>") {
                continue;
            }
        }

        last_step_running = matches!(message.stop_reason.as_deref(), None | Some("tool_use"));

        let (
            has_usage,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            cache_write_1h_tokens,
            output_tokens,
        ) = match message.usage {
            Some(u) => (
                true,
                u.input_tokens,
                u.cache_read_tokens,
                u.cache_write_tokens,
                u.cache_write_1h_tokens,
                u.output_tokens,
            ),
            None => (false, 0, 0, 0, 0, 0),
        };

        if !has_usage && message.id.is_none() {
            continue;
        }

        let dedup_key = match (message.id, parsed.request_id, parsed.uuid) {
            (Some(m), Some(r), _) => DedupKey::MsgReq(m, r),
            (Some(m), None, _) => DedupKey::MsgOnly(m),
            (None, _, Some(u)) => DedupKey::Uuid(u),
            (None, _, None) => DedupKey::Line(dedup.line_no),
        };

        let acc = dedup.by_key.entry(dedup_key).or_insert_with(|| {
            let seq = dedup.next_seq;
            dedup.next_seq += 1;
            DedupTurnAcc {
                first_seq: seq,
                timestamp_utc: None,
                model: intern_str("unknown"),
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

        if let Some(m) = message.model {
            if &*m != "unknown" {
                acc.model = m;
            }
        }
        if let Some(ts) = parsed.timestamp_utc {
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

        for (tool_id, tool_name) in message.tools {
            if let Some(tid) = tool_id {
                if acc.seen_tool_ids.insert(tid) {
                    acc.tool_names.push(tool_name);
                }
            } else if !acc.tool_names.iter().any(|existing| existing.as_ref() == tool_name.as_ref()) {
                acc.tool_names.push(tool_name);
            }
        }
        acc.thinking_chars = acc.thinking_chars.saturating_add(message.thinking_chars);
    }

    let turns: Arc<[TurnRecord]> = if retain_tail {
        let mut refs: Vec<&DedupTurnAcc> = dedup.by_key.values().collect();
        refs.sort_by_key(|a| a.first_seq);
        let mut out = Vec::with_capacity(refs.len());
        for acc in refs {
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
            let step_index = (out.len() + 1) as i64;
            out.push(TurnRecord {
                step_index,
                timestamp_utc: acc.timestamp_utc,
                model: Arc::clone(&acc.model),
                is_checkpoint: false,
                input_tokens: acc.input_tokens,
                cache_read_tokens: acc.cache_read_tokens,
                cache_write_tokens: acc.cache_write_tokens,
                cache_write_1h_tokens: acc.cache_write_1h_tokens.min(acc.cache_write_tokens),
                output_tokens: acc.output_tokens,
                reasoning_tokens: 0,
                tool_names: acc.tool_names.clone().into_boxed_slice(),
                thinking_chars: acc.thinking_chars,
                gen_secs: None,
                cost: CostBreakdown::default(),
            });
        }
        out.into()
    } else {
        let mut ordered: Vec<DedupTurnAcc> = dedup.by_key.into_values().collect();
        ordered.sort_by_key(|a| a.first_seq);
        let mut out = Vec::with_capacity(ordered.len());
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
            let step_index = (out.len() + 1) as i64;
            out.push(TurnRecord {
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
                tool_names: acc.tool_names.into_boxed_slice(),
                thinking_chars: acc.thinking_chars,
                gen_secs: None,
                cost: CostBreakdown::default(),
            });
        }
        dedup.by_key = HashMap::new();
        out.into()
    };

    CachedClaudeTranscript {
        sig,
        header,
        committed_offset,
        turns,
        cwd,
        custom_title,
        summary,
        first_user_preview,
        last_step_running,
        committed_skipped_lines,
        trailing_skipped_lines,
        tail_state: if retain_tail { Some(dedup) } else { None },
    }
}

fn sanitize_user_snippet(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
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
                r#"{"type":"assistant","uuid":"u1","requestId":"req_1","timestamp":"2026-11-24T10:00:01Z","message":{"id":"msg_1","model":"claude-sonnet-4-5-20250929","stop_reason":null,"content":[{"type":"thinking","thinking":"Checking parser"}],"usage":{"input_tokens":120,"cache_read_input_tokens":5000,"cache_creation_input_tokens":300,"output_tokens":4}}}"#,
                r#"{"type":"assistant","uuid":"u2","requestId":"req_1","timestamp":"2026-11-24T10:00:02Z","message":{"id":"msg_1","model":"claude-sonnet-4-5-20250929","stop_reason":null,"content":[{"type":"tool_use","id":"toolu_1","name":"Read"}],"usage":{"input_tokens":120,"cache_read_input_tokens":5000,"cache_creation_input_tokens":300,"output_tokens":40}}}"#,
                r#"{"type":"assistant","uuid":"u3","requestId":"req_1","timestamp":"2026-11-24T10:00:04Z","message":{"id":"msg_1","model":"claude-sonnet-4-5-20250929","stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_1","name":"Read"},{"type":"tool_use","id":"toolu_2","name":"Edit"}],"usage":{"input_tokens":120,"cache_read_input_tokens":5000,"cache_creation_input_tokens":300,"output_tokens":180}}}"#,
                r#"{"type":"assistant","uuid":"u4","timestamp":"2026-11-24T10:01:00Z","message":{"id":"msg_2","model":"claude-sonnet-4-5-20250929","stop_reason":null,"content":[{"type":"redacted_thinking","data":"opaque-bytes"}],"usage":{"input_tokens":50,"cache_read_input_tokens":5400,"cache_creation_input_tokens":0,"output_tokens":5}}}"#,
                r#"{"type":"assistant","uuid":"u5","timestamp":"2026-11-24T10:01:05Z","message":{"id":"msg_2","model":"claude-sonnet-4-5-20250929","stop_reason":"end_turn","content":[{"type":"text","text":"All fixed!"}],"usage":{"input_tokens":50,"cache_read_input_tokens":5400,"cache_creation_input_tokens":0,"output_tokens":95}}}"#,
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

        let t1 = &s.turns[0];
        assert_eq!(t1.input_tokens, 120);
        assert_eq!(t1.cache_read_tokens, 5000);
        assert_eq!(t1.cache_write_tokens, 300);
        assert_eq!(t1.cache_write_1h_tokens, 0);
        assert_eq!(t1.output_tokens, 180);
        assert_eq!(t1.reasoning_tokens, 0);
        let t1_tools: Vec<&str> = t1.tool_names.iter().map(AsRef::as_ref).collect();
        assert_eq!(t1_tools, vec!["Read", "Edit"]);
        assert_eq!(t1.thinking_chars, "Checking parser".chars().count() as u64);
        assert!(t1.has_thinking());
        assert_eq!(t1.cost, CostBreakdown::default());

        let t2 = &s.turns[1];
        assert_eq!(t2.output_tokens, 95);
        assert!(t2.thinking_chars >= 1);
        assert!(t2.has_thinking());

        let t3 = &s.turns[2];
        assert_eq!(t3.output_tokens, 25);
        let t3_tools: Vec<&str> = t3.tool_names.iter().map(AsRef::as_ref).collect();
        assert_eq!(t3_tools, vec!["Bash"]);
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
                r#"{"type":"assistant","uuid":"e1","isApiErrorMessage":true,"message":{"id":"msg_e1","model":"<synthetic>","content":[{"type":"text","text":"API Error"}],"usage":{"input_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}}}}"#,
                r#"{"type":"assistant","uuid":"e2","message":{"id":"msg_e2","model":"<synthetic>","usage":{"input_tokens":999,"output_tokens":999}}}"#,
                r#"{"type":"assistant","uuid":"e3","isApiErrorMessage":true,"message":{"id":"msg_e3","model":"claude-opus-4-5","usage":{"input_tokens":500,"output_tokens":200}}}"#,
                r#"{"type":"assistant","uuid":"ok1","requestId":"r1","timestamp":"2026-11-24T11:00:00Z","message":{"id":"msg_ok","model":"claude-opus-4-5","stop_reason":"end_turn","content":[{"type":"text","text":"Done"}],"usage":{"input_tokens":42,"cache_read_input_tokens":100,"cache_creation_input_tokens":0,"output_tokens":18}}}"#,
            ],
        );

        let mut adapter = ClaudeCodeAdapter::new(root.clone());
        let res = adapter.scan(Utc::now());
        assert_eq!(res.sessions.len(), 1);
        assert_eq!(res.sessions[0].turns.len(), 1);
        assert_eq!(&*res.sessions[0].turns[0].model, "claude-opus-4-5");
        assert_eq!(res.sessions[0].turns[0].input_tokens, 42);
        assert_eq!(res.sessions[0].turns[0].output_tokens, 18);
        assert_eq!(res.sessions[0].workspace, UNKNOWN_WORKSPACE);
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
                r#"{"type":"assistant","uuid":"u1","requestId":"r1","message":{"id":"m1","model":"claude-sonnet-4-5","usage":{"input_tokens":200,"cache_read_input_tokens":3000,"cache_creation_input_tokens":1000,"cache_creation":{"ephemeral_5m_input_tokens":400,"ephemeral_1h_input_tokens":600},"output_tokens":50}}}"#,
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

        write_lines(
            &proj.join(format!("{s1}.jsonl")),
            &[
                r#"{"type":"user","cwd":"D:/work/s1","message":{"role":"user","content":"User prompt"}}"#,
                r#"{"type":"summary","summary":"JSONL Summary"}"#,
                r#"{"type":"custom-title","customTitle":"Renamed Custom Title"}"#,
                r#"{"type":"assistant","uuid":"a1","message":{"id":"m1","model":"claude-sonnet-4-5","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            ],
        );
        write_lines(
            &proj.join(format!("{s2}.jsonl")),
            &[
                r#"{"type":"user","message":{"role":"user","content":"User prompt 2"}}"#,
                r#"{"type":"summary","summary":"JSONL Summary 2"}"#,
                r#"{"type":"assistant","uuid":"a2","message":{"id":"m2","model":"claude-sonnet-4-5","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            ],
        );
        write_lines(
            &proj.join(format!("{s3}.jsonl")),
            &[
                r#"{"type":"user","message":{"role":"user","content":"User prompt 3"}}"#,
                r#"{"type":"assistant","uuid":"a3","message":{"id":"m3","model":"claude-sonnet-4-5","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            ],
        );
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

        write_lines(
            &proj.join(format!("{parent_uuid}.jsonl")),
            &[
                r#"{"type":"user","cwd":"C:/repo/main","message":{"role":"user","content":"Refactor auth"}}"#,
                r#"{"type":"assistant","uuid":"p1","requestId":"rp1","message":{"id":"mp1","model":"claude-sonnet-4-5","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"Agent"}],"usage":{"input_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":40}}}"#,
                r#"{"type":"user","toolUseResult":{"status":"completed","totalTokens":99999,"usage":{"input_tokens":80000,"output_tokens":19999}},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
                r#"{"type":"user","toolUseResult":"string-form-result","message":{"role":"user","content":"thanks"}}"#,
            ],
        );

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
        assert_eq!(res.sessions.len(), 3);
        assert_eq!(res.skipped_empty, 1);

        let by_id: HashMap<&str, &ParsedSession> = res
            .sessions
            .iter()
            .map(|s| (s.conversation_id.as_str(), s))
            .collect();

        let parent = by_id[parent_uuid];
        assert!(!parent.is_subagent);
        assert_eq!(parent.turns.len(), 1);
        assert_eq!(parent.turns[0].input_tokens, 100);
        assert_eq!(parent.turns[0].output_tokens, 40);

        let sub1_id = format!("{parent_uuid}__a1b2c3");
        let sub1 = by_id[sub1_id.as_str()];
        assert!(sub1.is_subagent);
        assert_eq!(sub1.parent_id.as_deref(), Some(parent_uuid));
        assert_eq!(sub1.title, "Audit auth modules");
        assert_eq!(sub1.workspace, "C:/repo/main");
        assert_eq!(sub1.turns.len(), 1);
        assert_eq!(sub1.turns[0].input_tokens, 200);
        assert_eq!(sub1.turns[0].output_tokens, 60);

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

        let r1 = adapter.scan(Utc::now());
        assert_eq!(r1.files_reparsed, 2);
        assert_eq!(r1.files_cached, 0);
        assert_eq!(r1.sessions.len(), 2);

        let r2 = adapter.scan(Utc::now());
        assert_eq!(r2.files_reparsed, 0);
        assert_eq!(r2.files_cached, 2);
        assert_eq!(r2.sessions.len(), 2);

        // Append a second turn to p1 -> exercises byte-offset tailing!
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

        write_lines(&root.join("settings.json"), &[decoy_line]);
        write_lines(&root.join("settings.local.json"), &[decoy_line]);
        write_lines(&root.join(".credentials.json"), &[decoy_line]);
        write_lines(&root.join("auth.json"), &[decoy_line]);
        write_lines(&root.join("history.jsonl"), &[decoy_line]);
        write_lines(&root.join("statsig").join("eval.jsonl"), &[decoy_line]);
        write_lines(&root.join("debug").join("trace.jsonl"), &[decoy_line]);

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

        write_lines(
            &proj.join(format!("{valid_uuid}.jsonl")),
            &[
                r#"{"type":"assistant","uuid":"v1","requestId":"rv1","message":{"id":"mv1","model":"claude-sonnet-4-5","usage":{"input_tokens":11,"output_tokens":7}}}"#,
                "{truncated-json-line",
            ],
        );

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

        let r0 = adapter.scan(Utc::now());
        assert!(!r0.found_root);
        assert!(r0.sessions.is_empty());

        let proj = root.join("projects").join("proj-edge");
        let uuid = "aaaaaaaa-2222-3333-4444-555555555555";
        write_lines(
            &proj.join(format!("{uuid}.jsonl")),
            &[
                r#"{"type":"assistant","uuid":"u1","requestId":"rq1","timestamp":"2026-11-24T12:00:05Z","message":{"id":"mg1","model":"claude-opus-4-5","stop_reason":"tool_use","content":[{"type":"thinking","thinking":""},{"type":"tool_use","id":"t_a","name":"Read"},{"type":"tool_use","id":"t_b","name":"Read"}],"usage":{"input_tokens":300,"cache_read_input_tokens":4000,"cache_creation_input_tokens":0,"output_tokens":250}}}"#,
                r#"{"type":"assistant","uuid":"u2","requestId":"rq1","timestamp":"2026-11-24T12:00:02Z","message":{"id":"mg1","model":"claude-opus-4-5","stop_reason":null,"content":[{"type":"tool_use","id":"t_a","name":"Read"}],"usage":{"input_tokens":300,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":12}}}"#,
            ],
        );

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
        let main_s = r1
            .sessions
            .iter()
            .find(|s| s.conversation_id == uuid)
            .unwrap();
        assert_eq!(main_s.turns.len(), 1);
        assert_eq!(main_s.turns[0].cache_read_tokens, 4000);
        assert_eq!(main_s.turns[0].output_tokens, 250);
        assert!(main_s.turns[0].has_thinking());
        let tools: Vec<&str> = main_s.turns[0].tool_names.iter().map(AsRef::as_ref).collect();
        assert_eq!(tools, vec!["Read", "Read"]);

        let sub_s = r1
            .sessions
            .iter()
            .find(|s| s.is_subagent)
            .unwrap();
        assert_eq!(sub_s.title, "Plan");

        let _ = fs::remove_dir_all(&root);
    }
}
