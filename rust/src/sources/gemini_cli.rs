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
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::pricing::CostBreakdown;
use crate::sources::{
    clean_single_line, compute_running_hint, decode_file_uri, non_symlink_dir,
    read_header_fingerprint, try_seek_for_tail, AdapterPoll, CharCount, FileSig, FlexTimestamp,
    FlexU64, HeaderFingerprint, InternedStrSeed, OptFlexString, ParsedSession, SafeFs,
    SourceAdapter, SourceScanResult,
};
use crate::stats::{intern_str, SourceKind, TurnRecord};

const MAX_PROJECT_ROOT_BYTES: usize = 4 * 1024;
const MAX_PROJECTS_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_SESSION_JSON_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone)]
struct CachedSessionFile {
    sig: FileSig,
    header: HeaderFingerprint,
    committed_offset: u64,
    is_jsonl: bool,
    conversation_id: String,
    project_hash: String,
    summary: String,
    default_model: Option<Arc<str>>,
    first_user_preview: String,
    is_subagent: bool,
    parent_id: Option<String>,
    turns: Arc<[TurnRecord]>,
    last_step_running: bool,
    committed_skipped_lines: usize,
    trailing_skipped_lines: usize,
}

impl CachedSessionFile {
    fn total_skipped_lines(&self) -> usize {
        self.committed_skipped_lines + self.trailing_skipped_lines
    }
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
    has_scanned: bool,
    last_found_root: bool,
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
            has_scanned: false,
            last_found_root: false,
        }
    }

    fn tmp_dir(&self) -> PathBuf {
        if self.root_is_tmp {
            self.safe_fs.root().to_path_buf()
        } else {
            self.safe_fs.root().join("tmp")
        }
    }

    /// Returns `true` if `projects.json` changed.
    fn refresh_projects_json(&mut self, seen_paths: &mut HashSet<PathBuf>) -> bool {
        if !self.root_is_tmp {
            let path = self.safe_fs.root().join("projects.json");
            let Some(sig) = self.safe_fs.is_safe_regular_file(&path) else {
                let had = self.projects_json_sig.is_some() || !self.projects_lookup.is_empty();
                self.projects_json_sig = None;
                self.projects_lookup.clear();
                return had;
            };
            seen_paths.insert(path.clone());
            if self.projects_json_sig == Some(sig) {
                return false;
            }
            if let Some(text) =
                self.safe_fs
                    .read_already_verified_limited(&path, sig, MAX_PROJECTS_JSON_BYTES)
            {
                self.projects_lookup = parse_projects_json(&text);
                self.projects_json_sig = Some(sig);
                return true;
            }
            return false;
        }

        let mut parent_seen = HashSet::new();
        let mut changed = false;
        if let Some(parent_fs) = self.parent_safe_fs.as_mut() {
            parent_fs.refresh_root();
            let path = parent_fs.root().join("projects.json");
            if let Some(sig) = parent_fs.is_safe_regular_file(&path) {
                parent_seen.insert(path.clone());
                if self.projects_json_sig != Some(sig) {
                    if let Some(text) =
                        parent_fs.read_already_verified_limited(&path, sig, MAX_PROJECTS_JSON_BYTES)
                    {
                        self.projects_lookup = parse_projects_json(&text);
                        self.projects_json_sig = Some(sig);
                        changed = true;
                    }
                }
            } else {
                if self.projects_json_sig.is_some() || !self.projects_lookup.is_empty() {
                    changed = true;
                }
                self.projects_json_sig = None;
                self.projects_lookup.clear();
            }
            parent_fs.retain_verified(&parent_seen);
        }
        changed
    }

    fn read_project_root(
        &mut self,
        proj_dir: &Path,
        seen_paths: &mut HashSet<PathBuf>,
    ) -> (String, bool) {
        let path = proj_dir.join(".project_root");
        let Some(sig) = self.safe_fs.is_safe_regular_file(&path) else {
            let removed = self.project_root_cache.remove(&path).is_some();
            return (String::new(), removed);
        };
        seen_paths.insert(path.clone());
        if let Some((cached_sig, val)) = self.project_root_cache.get(&path) {
            if *cached_sig == sig {
                return (val.clone(), false);
            }
        }
        let workspace = self
            .safe_fs
            .read_already_verified_limited(&path, sig, MAX_PROJECT_ROOT_BYTES)
            .map(|text| {
                let ws = normalize_workspace_line(&text);
                self.project_root_cache.insert(path, (sig, ws.clone()));
                ws
            })
            .unwrap_or_default();
        (workspace, true)
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

    fn scan_internal(&mut self, scan_now_utc: DateTime<Utc>, skip_if_unchanged: bool) -> AdapterPoll {
        self.safe_fs.refresh_root();
        let tmp_dir = self.tmp_dir();
        if !self.safe_fs.is_safe_dir(&tmp_dir) {
            let changed = !self.has_scanned
                || self.last_found_root
                || !self.session_cache.is_empty()
                || !self.project_root_cache.is_empty();
            self.session_cache.clear();
            self.project_root_cache.clear();
            self.projects_json_sig = None;
            self.projects_lookup.clear();
            self.session_hints.clear();
            self.safe_fs.clear_verified();
            if let Some(parent_fs) = self.parent_safe_fs.as_mut() {
                parent_fs.clear_verified();
            }
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
            HashSet::with_capacity(self.session_cache.len() + 8);
        let mut seen_dirs: HashSet<PathBuf> = HashSet::with_capacity(16);
        seen_dirs.insert(tmp_dir.clone());

        let mut any_changed = !self.has_scanned || !self.last_found_root;
        if self.refresh_projects_json(&mut seen_paths) {
            any_changed = true;
        }

        let Ok(proj_entries) = fs::read_dir(&tmp_dir) else {
            self.session_cache.clear();
            self.project_root_cache.clear();
            self.session_hints.clear();
            self.safe_fs.retain_verified(&seen_paths);
            self.safe_fs.retain_verified_dirs(&seen_dirs);
            self.has_scanned = true;
            self.last_found_root = true;
            return AdapterPoll::Changed(SourceScanResult {
                found_root: true,
                ..Default::default()
            });
        };

        let mut proj_dirs: Vec<PathBuf> = Vec::new();
        for e in proj_entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() || !ft.is_dir() {
                continue;
            }
            let p = e.path();
            if self.safe_fs.is_safe_dir(&p) {
                seen_dirs.insert(p.clone());
                proj_dirs.push(p);
            }
        }
        proj_dirs.sort();

        let mut files_cached = 0usize;
        let mut files_reparsed = 0usize;
        let mut skipped_empty = 0usize;
        let mut skipped_lines = 0usize;

        struct ProjFiles {
            proj_dir_name: String,
            proj_root_ws: String,
            files: Vec<PathBuf>,
        }
        let mut proj_plans: Vec<ProjFiles> = Vec::with_capacity(proj_dirs.len());

        for proj_dir in proj_dirs {
            let chats_dir = proj_dir.join("chats");
            if !self.safe_fs.is_safe_dir(&chats_dir) {
                continue;
            }
            seen_dirs.insert(chats_dir.clone());
            let Ok(chat_entries) = fs::read_dir(&chats_dir) else {
                continue;
            };

            let mut session_files: Vec<(PathBuf, FileSig, bool)> = Vec::new();
            for entry in chat_entries.flatten() {
                let path = entry.path();
                if !is_whitelisted_session_filename(&path) {
                    continue;
                }
                if let Some((sig, freshly_canon)) =
                    self.safe_fs.stat_verified_dir_entry_checked(&path, &entry)
                {
                    session_files.push((path, sig, freshly_canon));
                }
            }
            session_files.sort_by(|a, b| a.0.cmp(&b.0));
            if session_files.is_empty() {
                continue;
            }

            let proj_dir_name = proj_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let (proj_root_ws, pr_changed) = self.read_project_root(&proj_dir, &mut seen_paths);
            if pr_changed {
                any_changed = true;
            }

            let mut valid_files = Vec::with_capacity(session_files.len());
            for (file_path, sig, freshly_canon) in session_files {
                seen_paths.insert(file_path.clone());

                let need_reparse = match self.session_cache.get(&file_path) {
                    Some(entry) if entry.sig == sig => {
                        files_cached += 1;
                        false
                    }
                    _ => true,
                };

                if need_reparse {
                    let Some(fh) = self.safe_fs.open_for_read(&file_path, freshly_canon) else {
                        continue;
                    };
                    let prev = self.session_cache.remove(&file_path);
                    let fresh = parse_or_tail_session_file(&file_path, fh, sig, prev);
                    files_reparsed += 1;
                    any_changed = true;
                    self.session_cache.insert(file_path.clone(), fresh);
                }

                if let Some(entry) = self.session_cache.get(&file_path) {
                    skipped_lines += entry.total_skipped_lines();
                    if entry.turns.is_empty() {
                        skipped_empty += 1;
                    }
                    valid_files.push(file_path);
                }
            }

            proj_plans.push(ProjFiles {
                proj_dir_name,
                proj_root_ws,
                files: valid_files,
            });
        }

        let prev_sc = self.session_cache.len();
        let prev_pr = self.project_root_cache.len();
        self.session_cache.retain(|k, _| seen_paths.contains(k));
        self.project_root_cache.retain(|k, _| seen_paths.contains(k));
        self.safe_fs.retain_verified(&seen_paths);
        self.safe_fs.retain_verified_dirs(&seen_dirs);
        if self.session_cache.len() != prev_sc || self.project_root_cache.len() != prev_pr {
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

        let mut fresh_hints: HashMap<String, (String, String)> = HashMap::new();
        let mut sessions = Vec::new();

        for plan in proj_plans {
            for file_path in plan.files {
                let Some(cached) = self.session_cache.get(&file_path) else {
                    continue;
                };
                let workspace = if !plan.proj_root_ws.is_empty() {
                    plan.proj_root_ws.clone()
                } else {
                    self.lookup_workspace_in_projects(&plan.proj_dir_name, &cached.project_hash)
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
                    continue;
                }

                let last_file_mtime_utc = cached.sig.mtime_utc();
                let is_running_hint = compute_running_hint(
                    cached.last_step_running,
                    last_file_mtime_utc,
                    scan_now_utc,
                );

                sessions.push(ParsedSession {
                    conversation_id: cached.conversation_id.clone(),
                    title,
                    workspace,
                    source: SourceKind::GeminiCli,
                    is_subagent: cached.is_subagent,
                    parent_id: cached.parent_id.clone(),
                    parent_title: String::new(),
                    turns: Arc::clone(&cached.turns),
                    last_file_mtime_utc,
                    last_step_running: cached.last_step_running,
                    is_running_hint,
                });
            }
        }

        self.session_hints = fresh_hints;
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

impl SourceAdapter for GeminiCliAdapter {
    fn kind(&self) -> SourceKind {
        SourceKind::GeminiCli
    }

    fn root(&self) -> &Path {
        self.safe_fs.root()
    }

    fn watched_dirs(&self) -> Vec<PathBuf> {
        let p = self.tmp_dir();
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
        for entry in self.session_cache.values_mut() {
            if Arc::ptr_eq(&entry.turns, old_turns) {
                entry.turns = Arc::clone(priced_turns);
                break;
            }
        }
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

// ---------- Compact Zero-DOM Gemini Message Representation ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeminiMsgType {
    User,
    Gemini,
    Other,
}

#[derive(Debug, Clone, Copy, Default)]
struct GeminiTokens {
    input: u64,
    output: u64,
    cached: u64,
    thoughts: u64,
    tool: u64,
}

#[derive(Debug, Clone)]
struct CompactGeminiMsg {
    id: Option<String>,
    msg_type: GeminiMsgType,
    user_preview: Option<String>,
    model: Option<Arc<str>>,
    timestamp_utc: Option<DateTime<Utc>>,
    tokens: Option<GeminiTokens>,
    tool_names: Box<[Arc<str>]>,
    has_running_tool: bool,
    has_thoughts_arr: bool,
    thoughts_arr_chars: u64,
}

impl CompactGeminiMsg {
    fn is_running(&self) -> bool {
        if self.msg_type != GeminiMsgType::Gemini {
            return false;
        }
        self.has_running_tool || self.tokens.is_none()
    }

    fn to_turn(&self, default_model: Option<&Arc<str>>) -> Option<TurnRecord> {
        if self.msg_type != GeminiMsgType::Gemini {
            return None;
        }
        let tok = self.tokens?;
        // CRITICAL Gemini CLI token math (inclusive input, EXCLUSIVE output):
        let cache_read_tokens = tok.cached.min(tok.input);
        let input_tokens = tok.input.saturating_sub(tok.cached).saturating_add(tok.tool);
        let output_tokens = tok.output.saturating_add(tok.thoughts);
        let reasoning_tokens = tok.thoughts;
        if input_tokens
            .saturating_add(cache_read_tokens)
            .saturating_add(output_tokens)
            == 0
        {
            return None;
        }
        let thinking_chars = if reasoning_tokens == 0 && self.has_thoughts_arr {
            self.thoughts_arr_chars.max(1)
        } else {
            0
        };
        let model = self
            .model
            .clone()
            .or_else(|| default_model.cloned())
            .unwrap_or_else(|| intern_str("unknown"));
        Some(TurnRecord {
            step_index: 0,
            timestamp_utc: self.timestamp_utc,
            model,
            is_checkpoint: false,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens: 0,
            cache_write_1h_tokens: 0,
            output_tokens,
            reasoning_tokens,
            tool_names: self.tool_names.clone(),
            thinking_chars,
            gen_secs: None,
            cost: CostBreakdown::default(),
        })
    }
}

#[derive(Default)]
struct SessionMetaState {
    session_id: String,
    project_hash: String,
    summary: String,
    default_model: Option<Arc<str>>,
    is_subagent: bool,
    parent_id: Option<String>,
}

/// Zero-allocation map key when deserializing from `&str`, falling back to owned `String` when
/// streaming from `BufReader` (`from_reader`) or when a JSON key contains escape sequences.
struct KeyStr<'de>(std::borrow::Cow<'de, str>);

impl<'de> serde::Deserialize<'de> for KeyStr<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = KeyStr<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON object key")
            }
            fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(KeyStr(std::borrow::Cow::Borrowed(v)))
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                Ok(KeyStr(std::borrow::Cow::Owned(v.to_string())))
            }
            fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
                Ok(KeyStr(std::borrow::Cow::Owned(v)))
            }
        }
        deserializer.deserialize_str(V)
    }
}

struct GeminiTokensSeed;
impl<'de> de::DeserializeSeed<'de> for GeminiTokensSeed {
    type Value = Option<GeminiTokens>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<GeminiTokens>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Gemini tokens object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut t = GeminiTokens::default();
                while let Some(KeyStr(k)) = map.next_key::<KeyStr<'de>>()? {
                    match k.as_ref() {
                        "input" => {
                            let FlexU64(v) = map.next_value()?;
                            t.input = v;
                        }
                        "output" => {
                            let FlexU64(v) = map.next_value()?;
                            t.output = v;
                        }
                        "cached" => {
                            let FlexU64(v) = map.next_value()?;
                            t.cached = v;
                        }
                        "thoughts" => {
                            let FlexU64(v) = map.next_value()?;
                            t.thoughts = v;
                        }
                        "tool" => {
                            let FlexU64(v) = map.next_value()?;
                            t.tool = v;
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(Some(t))
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

struct GeminiToolCallsSeed;
impl<'de> de::DeserializeSeed<'de> for GeminiToolCallsSeed {
    type Value = (Box<[Arc<str>]>, bool);
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = (Box<[Arc<str>]>, bool);
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Gemini toolCalls array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut names = Vec::new();
                let mut running = false;
                while let Some((n, is_run)) = seq.next_element_seed(GeminiToolCallItemSeed)? {
                    if let Some(name) = n {
                        names.push(name);
                    }
                    if is_run {
                        running = true;
                    }
                }
                Ok((names.into_boxed_slice(), running))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok((Box::new([]), false))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok((Box::new([]), false))
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct GeminiToolCallItemSeed;
impl<'de> de::DeserializeSeed<'de> for GeminiToolCallItemSeed {
    type Value = (Option<Arc<str>>, bool);
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = (Option<Arc<str>>, bool);
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Gemini toolCall object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut name = None;
                let mut running = false;
                while let Some(KeyStr(k)) = map.next_key::<KeyStr<'de>>()? {
                    match k.as_ref() {
                        "name" => {
                            name = map.next_value_seed(InternedStrSeed)?;
                        }
                        "status" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let low = s.trim().to_ascii_lowercase();
                                if matches!(
                                    low.as_str(),
                                    "executing"
                                        | "scheduled"
                                        | "validating"
                                        | "awaiting_approval"
                                        | "running"
                                        | "in_progress"
                                ) {
                                    running = true;
                                }
                            }
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok((name, running))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok((None, false))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok((None, false))
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct GeminiThoughtsSeed;
impl<'de> de::DeserializeSeed<'de> for GeminiThoughtsSeed {
    type Value = (bool, u64);
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = (bool, u64);
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Gemini thoughts array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut any = false;
                let mut chars = 0u64;
                while let Some(c) = seq.next_element_seed(GeminiThoughtItemSeed)? {
                    any = true;
                    chars = chars.saturating_add(c);
                }
                Ok((any, chars))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok((false, 0))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok((false, 0))
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct GeminiThoughtItemSeed;
impl<'de> de::DeserializeSeed<'de> for GeminiThoughtItemSeed {
    type Value = u64;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = u64;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("thought string or object")
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                Ok(v.chars().count() as u64)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut chars = 0u64;
                while let Some(KeyStr(k)) = map.next_key::<KeyStr<'de>>()? {
                    if matches!(k.as_ref(), "subject" | "description" | "text") {
                        let CharCount(c) = map.next_value()?;
                        chars = chars.saturating_add(c);
                    } else {
                        let _: IgnoredAny = map.next_value()?;
                    }
                }
                Ok(chars)
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

struct GeminiUserTextSeed;
impl<'de> de::DeserializeSeed<'de> for GeminiUserTextSeed {
    type Value = Option<String>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<String>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("user content text/array/object")
            }
            fn visit_str<E>(self, s: &str) -> Result<Self::Value, E> {
                let c = clean_single_line(s, 72);
                Ok((!c.is_empty()).then_some(c))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut joined = String::new();
                while let Some(opt) = seq.next_element_seed(GeminiUserTextSeed)? {
                    if let Some(t) = opt {
                        if joined.len() < 300 {
                            if !joined.is_empty() {
                                joined.push(' ');
                            }
                            joined.push_str(&t);
                        }
                    }
                }
                let c = clean_single_line(&joined, 72);
                Ok((!c.is_empty()).then_some(c))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut txt = None;
                while let Some(KeyStr(k)) = map.next_key::<KeyStr<'de>>()? {
                    if k.as_ref() == "text" {
                        if let OptFlexString(Some(s)) = map.next_value()? {
                            let c = clean_single_line(&s, 72);
                            if !c.is_empty() {
                                txt = Some(c);
                            }
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
        }
        deserializer.deserialize_any(V)
    }
}

struct CompactGeminiMsgSeed {
    capture_user_preview: bool,
}

impl<'de> de::DeserializeSeed<'de> for CompactGeminiMsgSeed {
    type Value = Option<CompactGeminiMsg>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V {
            capture_user_preview: bool,
        }
        impl<'de> Visitor<'de> for V {
            type Value = Option<CompactGeminiMsg>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Gemini message object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut id: Option<String> = None;
                let mut msg_type = GeminiMsgType::Other;
                let mut display_content: Option<String> = None;
                let mut content: Option<String> = None;
                let mut model: Option<Arc<str>> = None;
                let mut timestamp_utc: Option<DateTime<Utc>> = None;
                let mut tokens: Option<GeminiTokens> = None;
                let mut tool_names: Box<[Arc<str>]> = Box::new([]);
                let mut has_running_tool = false;
                let mut has_thoughts_arr = false;
                let mut thoughts_arr_chars = 0u64;

                while let Some(KeyStr(k)) = map.next_key::<KeyStr<'de>>()? {
                    match k.as_ref() {
                        "id" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let t = s.trim();
                                if !t.is_empty() {
                                    id = Some(t.to_string());
                                }
                            }
                        }
                        "type" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                msg_type = match s.trim() {
                                    "user" => GeminiMsgType::User,
                                    "gemini" => GeminiMsgType::Gemini,
                                    _ => GeminiMsgType::Other,
                                };
                            }
                        }
                        "displayContent" if self.capture_user_preview => {
                            display_content = map.next_value_seed(GeminiUserTextSeed)?;
                        }
                        "content" if self.capture_user_preview => {
                            content = map.next_value_seed(GeminiUserTextSeed)?;
                        }
                        "model" => {
                            model = map.next_value_seed(InternedStrSeed)?;
                        }
                        "timestamp" => {
                            let FlexTimestamp(ts) = map.next_value()?;
                            timestamp_utc = ts;
                        }
                        "tokens" => {
                            tokens = map.next_value_seed(GeminiTokensSeed)?;
                        }
                        "toolCalls" => {
                            let (tn, run) = map.next_value_seed(GeminiToolCallsSeed)?;
                            tool_names = tn;
                            has_running_tool = run;
                        }
                        "thoughts" => {
                            let (has_t, chars) = map.next_value_seed(GeminiThoughtsSeed)?;
                            has_thoughts_arr = has_t;
                            thoughts_arr_chars = chars;
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let user_preview = if msg_type == GeminiMsgType::User {
                    display_content.or(content)
                } else {
                    None
                };

                Ok(Some(CompactGeminiMsg {
                    id,
                    msg_type,
                    user_preview,
                    model,
                    timestamp_utc,
                    tokens,
                    tool_names,
                    has_running_tool,
                    has_thoughts_arr,
                    thoughts_arr_chars,
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
        deserializer.deserialize_any(V {
            capture_user_preview: self.capture_user_preview,
        })
    }
}

struct CompactGeminiMsgsSeqSeed<'a> {
    out: &'a mut Vec<CompactGeminiMsg>,
    has_user_preview: &'a mut bool,
}

impl<'de, 'a> de::DeserializeSeed<'de> for CompactGeminiMsgsSeqSeed<'a> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V<'a> {
            out: &'a mut Vec<CompactGeminiMsg>,
            has_user_preview: &'a mut bool,
        }
        impl<'de, 'a> Visitor<'de> for V<'a> {
            type Value = ();
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Gemini messages array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while let Some(opt_m) = seq.next_element_seed(CompactGeminiMsgSeed {
                    capture_user_preview: !*self.has_user_preview,
                })? {
                    if let Some(m) = opt_m {
                        if m.user_preview.is_some() {
                            *self.has_user_preview = true;
                        }
                        self.out.push(m);
                    }
                }
                Ok(())
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(())
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(())
            }
        }
        deserializer.deserialize_any(V {
            out: self.out,
            has_user_preview: self.has_user_preview,
        })
    }
}

/// Zero-DOM streaming deserializer for classic `session-*.json` files!
struct GeminiClassicJsonSeed<'a> {
    meta: &'a mut SessionMetaState,
    messages: &'a mut Vec<CompactGeminiMsg>,
}

impl<'de, 'a> de::DeserializeSeed<'de> for GeminiClassicJsonSeed<'a> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V<'a> {
            meta: &'a mut SessionMetaState,
            messages: &'a mut Vec<CompactGeminiMsg>,
        }
        impl<'de, 'a> Visitor<'de> for V<'a> {
            type Value = ();
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Gemini session JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut has_user_preview = false;
                while let Some(KeyStr(k)) = map.next_key::<KeyStr<'de>>()? {
                    match k.as_ref() {
                        "sessionId" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let t = s.trim();
                                if !t.is_empty() {
                                    self.meta.session_id = t.to_string();
                                }
                            }
                        }
                        "projectHash" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let t = s.trim();
                                if !t.is_empty() {
                                    self.meta.project_hash = t.to_string();
                                }
                            }
                        }
                        "summary" => {
                            let val: Option<OptFlexString> = map.next_value()?;
                            match val {
                                Some(OptFlexString(Some(s))) => {
                                    self.meta.summary = clean_single_line(&s, 100);
                                }
                                None => self.meta.summary.clear(),
                                _ => {}
                            }
                        }
                        "model" => {
                            if let Some(m) = map.next_value_seed(InternedStrSeed)? {
                                self.meta.default_model = Some(m);
                            }
                        }
                        "kind" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                self.meta.is_subagent = s.trim().eq_ignore_ascii_case("subagent");
                            }
                        }
                        "parentSessionId" | "parentId" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                let t = s.trim();
                                if !t.is_empty() {
                                    self.meta.parent_id = Some(t.to_string());
                                }
                            }
                        }
                        "messages" => {
                            map.next_value_seed(CompactGeminiMsgsSeqSeed {
                                out: self.messages,
                                has_user_preview: &mut has_user_preview,
                            })?;
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(())
            }
        }
        deserializer.deserialize_map(V {
            meta: self.meta,
            messages: self.messages,
        })
    }
}

fn parse_or_tail_session_file(
    path: &Path,
    mut fh: File,
    sig: FileSig,
    prev: Option<CachedSessionFile>,
) -> CachedSessionFile {
    let is_jsonl = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("jsonl"))
        .unwrap_or(false);
    if is_jsonl {
        if let Some(prev_state) = prev.filter(|p| p.is_jsonl) {
            if let Some(fresh_hdr) = try_seek_for_tail(
                &mut fh,
                prev_state.sig,
                sig,
                prev_state.header,
                prev_state.committed_offset,
            ) {
                if let Some(tailed) =
                    try_tail_session_jsonl(fh.try_clone().ok(), sig, fresh_hdr, &prev_state)
                {
                    return tailed;
                }
            }
            let _ = fh.seek(SeekFrom::Start(0));
        }
        let header = read_header_fingerprint(&mut fh).unwrap_or_default();
        let _ = fh.seek(SeekFrom::Start(0));
        parse_session_jsonl(path, fh, sig, header)
    } else {
        parse_session_json(path, fh, sig)
    }
}

/// Streams classic `session-*.json` directly from `BufReader` without reading the whole file into
/// a `Vec<u8>` or building a `serde_json::Value` DOM!
fn parse_session_json(path: &Path, fh: File, sig: FileSig) -> CachedSessionFile {
    if sig.len == 0 {
        return empty_cached_session(path, sig, 0, false);
    }
    let limited = fh.take(MAX_SESSION_JSON_BYTES as u64);
    let mut reader = BufReader::with_capacity(64 * 1024, limited);
    // Strip optional UTF-8 BOM cleanly before feeding serde_json::Deserializer.
    if let Ok(peek) = reader.fill_buf() {
        if peek.starts_with(&[0xEF, 0xBB, 0xBF]) {
            reader.consume(3);
        }
    }
    let mut meta = SessionMetaState::default();
    let mut messages: Vec<CompactGeminiMsg> = Vec::new();
    let mut de = serde_json::Deserializer::from_reader(reader);
    if de::DeserializeSeed::deserialize(
        GeminiClassicJsonSeed {
            meta: &mut meta,
            messages: &mut messages,
        },
        &mut de,
    )
    .is_err()
    {
        return empty_cached_session(path, sig, 1, false);
    }
    finalize_session(path, sig, HeaderFingerprint::default(), 0, false, meta, &messages, 0, 0)
}

/// Attempts fast append-only tailing on a `.jsonl` session file; returns `None` if the appended
/// bytes contain a `$rewindTo` or `$set` mutation (falling back to full parse from byte 0).
fn try_tail_session_jsonl(
    fh_opt: Option<File>,
    sig: FileSig,
    header: HeaderFingerprint,
    prev: &CachedSessionFile,
) -> Option<CachedSessionFile> {
    let fh = fh_opt?;
    let mut reader = BufReader::with_capacity(64 * 1024, fh);
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut offset = prev.committed_offset;
    let mut committed_offset = prev.committed_offset;
    let mut committed_skipped_lines = prev.committed_skipped_lines;
    let mut trailing_skipped_lines = 0usize;

    let mut summary = prev.summary.clone();
    let mut first_user_preview = prev.first_user_preview.clone();
    let mut default_model = prev.default_model.clone();
    let mut last_step_running = prev.last_step_running;
    let mut new_msgs: Vec<CompactGeminiMsg> = Vec::new();

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

        // If any mutation record appears in the appended tail, bail out cleanly to full replay!
        if trimmed.contains("\"$rewindTo\"") || trimmed.contains("\"$set\"") {
            return None;
        }

        let seed = CompactGeminiMsgSeed {
            capture_user_preview: first_user_preview.is_empty(),
        };
        let mut de = serde_json::Deserializer::from_str(trimmed);
        let Ok(Some(msg)) = de::DeserializeSeed::deserialize(seed, &mut de) else {
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

        // If a duplicate message ID appears in the tail that might match an earlier turn, fall back.
        if msg.id.is_some() && !prev.turns.is_empty() {
            return None;
        }
        if msg.msg_type == GeminiMsgType::User && first_user_preview.is_empty() {
            if let Some(ref p) = msg.user_preview {
                first_user_preview = p.clone();
            }
        }
        if msg.msg_type != GeminiMsgType::Other {
            last_step_running = msg.is_running();
            new_msgs.push(msg);
        }
    }

    let mut turns_vec = prev.turns.to_vec();
    for msg in &new_msgs {
        if let Some(mut turn) = msg.to_turn(default_model.as_ref()) {
            turn.step_index = turns_vec.len() as i64;
            turns_vec.push(turn);
        }
    }
    let _ = (&mut summary, &mut default_model);

    Some(CachedSessionFile {
        sig,
        header,
        committed_offset,
        is_jsonl: true,
        conversation_id: prev.conversation_id.clone(),
        project_hash: prev.project_hash.clone(),
        summary,
        default_model,
        first_user_preview,
        is_subagent: prev.is_subagent,
        parent_id: prev.parent_id.clone(),
        turns: turns_vec.into(),
        last_step_running,
        committed_skipped_lines,
        trailing_skipped_lines,
    })
}

fn parse_session_jsonl(
    path: &Path,
    fh: File,
    sig: FileSig,
    header: HeaderFingerprint,
) -> CachedSessionFile {
    let mut reader = BufReader::with_capacity(64 * 1024, fh);
    let mut meta = SessionMetaState::default();
    let mut messages: Vec<CompactGeminiMsg> = Vec::new();
    let mut committed_skipped_lines = 0usize;
    let mut trailing_skipped_lines = 0usize;
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut first_line = true;
    let mut offset: u64 = 0;
    let mut committed_offset: u64 = 0;
    let mut has_user_preview = false;

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
        let mut trimmed = line_cow.trim();
        if first_line {
            trimmed = trimmed.trim_start_matches('\u{feff}').trim();
            first_line = false;
        }
        if trimmed.is_empty() {
            if has_newline {
                committed_offset = offset;
            }
            continue;
        }

        // Fast zero-DOM path for regular message lines (99%+ of JSONL lines):
        let maybe_meta_or_mutation = trimmed.contains("\"$rewindTo\"")
            || trimmed.contains("\"$set\"")
            || trimmed.contains("\"sessionId\"")
            || trimmed.contains("\"projectHash\"")
            || trimmed.contains("\"startTime\"")
            || trimmed.contains("\"summary\"");

        if !maybe_meta_or_mutation {
            let seed = CompactGeminiMsgSeed {
                capture_user_preview: !has_user_preview,
            };
            let mut de = serde_json::Deserializer::from_str(trimmed);
            let Ok(Some(msg)) = de::DeserializeSeed::deserialize(seed, &mut de) else {
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
            if msg.user_preview.is_some() {
                has_user_preview = true;
            }
            if msg.msg_type != GeminiMsgType::Other || msg.id.is_some() {
                messages.push(msg);
            }
            continue;
        }

        // Rare header / `$set` / `$rewindTo` lines (~1-3 per file): parse into Value and project immediately.
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) else {
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

        let mut handled_mutation = false;

        if let Some(rewind_val) = map.get("$rewindTo") {
            apply_rewind_to_compact(&mut messages, rewind_val);
            has_user_preview = messages.iter().any(|m| m.user_preview.is_some());
            handled_mutation = true;
        }

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
                    .find(|m| m.id.as_deref() == Some(mid))
                {
                    apply_msg_patch(existing, patch);
                }
            } else {
                apply_meta_fields_value(&mut meta, patch);
                if let Some(arr) = patch.get("messages").and_then(Value::as_array) {
                    messages.clear();
                    has_user_preview = false;
                    for item in arr {
                        if let Some(obj) = item.as_object() {
                            let cm = compact_msg_from_value_map(obj, !has_user_preview);
                            if cm.user_preview.is_some() {
                                has_user_preview = true;
                            }
                            messages.push(cm);
                        }
                    }
                }
            }
            handled_mutation = true;
        }

        if handled_mutation {
            continue;
        }

        let msg_type = map.get("type").and_then(Value::as_str).map(str::trim);
        let is_explicit_header = map.contains_key("sessionId")
            && matches!(
                msg_type,
                None | Some("session")
                    | Some("header")
                    | Some("metadata")
                    | Some("session_metadata")
            );

        if is_explicit_header
            || (msg_type.is_none()
                && (map.contains_key("projectHash")
                    || map.contains_key("startTime")
                    || map.contains_key("summary")))
        {
            apply_meta_fields_value(&mut meta, &map);
            if let Some(arr) = map.get("messages").and_then(Value::as_array) {
                for item in arr {
                    if let Some(obj) = item.as_object() {
                        let cm = compact_msg_from_value_map(obj, !has_user_preview);
                        if cm.user_preview.is_some() {
                            has_user_preview = true;
                        }
                        messages.push(cm);
                    }
                }
            }
            continue;
        }

        if msg_type.is_some() {
            let cm = compact_msg_from_value_map(&map, !has_user_preview);
            if cm.user_preview.is_some() {
                has_user_preview = true;
            }
            messages.push(cm);
        }
    }

    finalize_session(
        path,
        sig,
        header,
        committed_offset,
        true,
        meta,
        &messages,
        committed_skipped_lines,
        trailing_skipped_lines,
    )
}

fn apply_meta_fields_value(meta: &mut SessionMetaState, map: &serde_json::Map<String, Value>) {
    if let Some(id) = map
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        meta.session_id = id.to_string();
    }
    if let Some(hash) = map
        .get("projectHash")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        meta.project_hash = hash.to_string();
    }
    if let Some(val) = map.get("summary") {
        match val {
            Value::String(s) => meta.summary = clean_single_line(s, 100),
            Value::Null => meta.summary.clear(),
            _ => {}
        }
    }
    if let Some(m) = map
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        meta.default_model = Some(intern_str(m));
    }
    if let Some(kind) = map.get("kind").and_then(Value::as_str) {
        meta.is_subagent = kind.trim().eq_ignore_ascii_case("subagent");
    }
    if let Some(pid) = map
        .get("parentSessionId")
        .or_else(|| map.get("parentId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        meta.parent_id = Some(pid.to_string());
    }
}

fn compact_msg_from_value_map(
    msg: &serde_json::Map<String, Value>,
    capture_user_preview: bool,
) -> CompactGeminiMsg {
    let id = msg
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let msg_type = match msg.get("type").and_then(Value::as_str).map(str::trim) {
        Some("user") => GeminiMsgType::User,
        Some("gemini") => GeminiMsgType::Gemini,
        _ => GeminiMsgType::Other,
    };
    let user_preview = if msg_type == GeminiMsgType::User && capture_user_preview {
        let raw = msg
            .get("displayContent")
            .map(extract_text_from_value)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| msg.get("content").map(extract_text_from_value))
            .unwrap_or_default();
        let cleaned = clean_single_line(&raw, 72);
        (!cleaned.is_empty()).then_some(cleaned)
    } else {
        None
    };
    let model = msg
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(intern_str);
    let timestamp_utc = crate::sources::parse_timestamp_utc(msg.get("timestamp"));
    let tokens = msg.get("tokens").and_then(Value::as_object).map(|tok| GeminiTokens {
        input: crate::sources::val_to_u64(tok.get("input")),
        output: crate::sources::val_to_u64(tok.get("output")),
        cached: crate::sources::val_to_u64(tok.get("cached")),
        thoughts: crate::sources::val_to_u64(tok.get("thoughts")),
        tool: crate::sources::val_to_u64(tok.get("tool")),
    });
    let (tool_names, has_running_tool) = extract_tools_from_value(msg.get("toolCalls"));
    let (has_thoughts_arr, thoughts_arr_chars) = count_thoughts_chars(msg.get("thoughts"));

    CompactGeminiMsg {
        id,
        msg_type,
        user_preview,
        model,
        timestamp_utc,
        tokens,
        tool_names,
        has_running_tool,
        has_thoughts_arr,
        thoughts_arr_chars,
    }
}

fn apply_msg_patch(target: &mut CompactGeminiMsg, patch: &serde_json::Map<String, Value>) {
    if let Some(m) = patch
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        target.model = Some(intern_str(m));
    }
    if let Some(ts) = patch.get("timestamp") {
        target.timestamp_utc = crate::sources::parse_timestamp_utc(Some(ts));
    }
    if let Some(tv) = patch.get("tokens") {
        target.tokens = tv.as_object().map(|tok| GeminiTokens {
            input: crate::sources::val_to_u64(tok.get("input")),
            output: crate::sources::val_to_u64(tok.get("output")),
            cached: crate::sources::val_to_u64(tok.get("cached")),
            thoughts: crate::sources::val_to_u64(tok.get("thoughts")),
            tool: crate::sources::val_to_u64(tok.get("tool")),
        });
    }
    if let Some(tc) = patch.get("toolCalls") {
        let (tn, run) = extract_tools_from_value(Some(tc));
        target.tool_names = tn;
        target.has_running_tool = run;
    }
    if let Some(th) = patch.get("thoughts") {
        let (has_t, chars) = count_thoughts_chars(Some(th));
        target.has_thoughts_arr = has_t;
        target.thoughts_arr_chars = chars;
    }
}

fn apply_rewind_to_compact(messages: &mut Vec<CompactGeminiMsg>, target: &Value) {
    match target {
        Value::String(id) => {
            let id = id.trim();
            if !id.is_empty() {
                if let Some(pos) = messages.iter().position(|m| m.id.as_deref() == Some(id)) {
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
                if let Some(pos) = messages.iter().position(|m| m.id.as_deref() == Some(id)) {
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

#[allow(clippy::too_many_arguments)]
fn finalize_session(
    path: &Path,
    sig: FileSig,
    header: HeaderFingerprint,
    committed_offset: u64,
    is_jsonl: bool,
    meta: SessionMetaState,
    messages: &[CompactGeminiMsg],
    committed_skipped_lines: usize,
    trailing_skipped_lines: usize,
) -> CachedSessionFile {
    let conversation_id = if meta.session_id.is_empty() {
        fallback_session_id(path)
    } else {
        meta.session_id
    };

    let mut first_user_preview = String::new();
    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut id_to_slot: HashMap<&str, usize> = HashMap::new();

    for msg in messages {
        if msg.msg_type == GeminiMsgType::User {
            if first_user_preview.is_empty() {
                if let Some(ref p) = msg.user_preview {
                    first_user_preview = p.clone();
                }
            }
            continue;
        }
        let Some(turn) = msg.to_turn(meta.default_model.as_ref()) else {
            continue;
        };
        if let Some(id) = msg.id.as_deref() {
            if let Some(&slot) = id_to_slot.get(id) {
                turns[slot] = turn;
            } else {
                let slot = turns.len();
                id_to_slot.insert(id, slot);
                turns.push(turn);
            }
        } else {
            turns.push(turn);
        }
    }

    for (idx, t) in turns.iter_mut().enumerate() {
        t.step_index = idx as i64;
    }

    let last_step_running = messages.last().map(CompactGeminiMsg::is_running).unwrap_or(false);
    let parent_id = meta.parent_id.filter(|p| p != &conversation_id);
    let is_subagent = meta.is_subagent || parent_id.is_some();

    CachedSessionFile {
        sig,
        header,
        committed_offset,
        is_jsonl,
        conversation_id,
        project_hash: meta.project_hash,
        summary: meta.summary,
        default_model: meta.default_model,
        first_user_preview,
        is_subagent,
        parent_id,
        turns: turns.into(),
        last_step_running,
        committed_skipped_lines,
        trailing_skipped_lines,
    }
}

fn empty_cached_session(
    path: &Path,
    sig: FileSig,
    skipped_lines: usize,
    is_jsonl: bool,
) -> CachedSessionFile {
    CachedSessionFile {
        sig,
        header: HeaderFingerprint::default(),
        committed_offset: 0,
        is_jsonl,
        conversation_id: fallback_session_id(path),
        project_hash: String::new(),
        summary: String::new(),
        default_model: None,
        first_user_preview: String::new(),
        is_subagent: false,
        parent_id: None,
        turns: Arc::from(Vec::new()),
        last_step_running: false,
        committed_skipped_lines: skipped_lines,
        trailing_skipped_lines: 0,
    }
}

fn extract_tools_from_value(val: Option<&Value>) -> (Box<[Arc<str>]>, bool) {
    let Some(Value::Array(arr)) = val else {
        return (Box::new([]), false);
    };
    let mut names = Vec::new();
    let mut running = false;
    for item in arr {
        if let Some(o) = item.as_object() {
            if let Some(name) = o
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                names.push(intern_str(name));
            }
            if let Some(status) = o.get("status").and_then(Value::as_str) {
                let s = status.trim().to_ascii_lowercase();
                if matches!(
                    s.as_str(),
                    "executing"
                        | "scheduled"
                        | "validating"
                        | "awaiting_approval"
                        | "running"
                        | "in_progress"
                ) {
                    running = true;
                }
            }
        }
    }
    (names.into_boxed_slice(), running)
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
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return true;
    }
    s.starts_with('/')
        || s.starts_with('\\')
        || s.starts_with("~/")
        || s.contains('/')
        || s.contains('\\')
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
                    insert_project_mapping(&mut out, k, val_str);
                    index_workspace_hashes(&mut out, val_str);
                } else {
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
                } else if let Some(ws) =
                    ["path", "root", "projectRoot", "project_root", "cwd", "workspace"]
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
        assert_eq!(&*t1.model, "gemini-2.5-pro");
        assert_eq!(t1.cache_read_tokens, 6000);
        assert_eq!(t1.input_tokens, 4300);
        assert_eq!(t1.cache_write_tokens, 0);
        assert_eq!(t1.cache_write_1h_tokens, 0);
        assert_eq!(t1.output_tokens, 1700);
        assert_eq!(t1.reasoning_tokens, 1200);
        assert_eq!(t1.total_input(), 10300);
        assert_eq!(t1.total_tokens(), 12000);
        assert!(t1.has_thinking());
        let t1_tools: Vec<&str> = t1.tool_names.iter().map(AsRef::as_ref).collect();
        assert_eq!(t1_tools, vec!["read_file", "run_shell_command"]);
        assert_eq!(t1.cost, CostBreakdown::default());

        let t2 = &s1.turns[1];
        assert_eq!(t2.cache_read_tokens, 500);
        assert_eq!(t2.input_tokens, 1500);
        assert_eq!(t2.output_tokens, 250);
        assert_eq!(t2.reasoning_tokens, 0);
        assert!(t2.thinking_chars > 0);
        assert!(t2.has_thinking());

        let t3 = &s1.turns[2];
        assert_eq!(t3.cache_read_tokens, 100);
        assert_eq!(t3.input_tokens, 25);
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
            json!({
                "id": "g1",
                "timestamp": "2026-11-24T09:00:02Z",
                "type": "gemini",
                "model": "gemini-2.5-pro",
                "tokens": {"input": 1000, "output": 10, "cached": 200, "thoughts": 50, "tool": 0}
            })
            .to_string(),
            json!({
                "id": "g1",
                "timestamp": "2026-11-24T09:00:04Z",
                "type": "gemini",
                "model": "gemini-2.5-pro",
                "toolCalls": [{"id": "t1", "name": "grep_search", "status": "success"}],
                "tokens": {"input": 1000, "output": 120, "cached": 200, "thoughts": 300, "tool": 50}
            })
            .to_string(),
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
            json!({"$set": {"summary": "Updated Title Via Set"}}).to_string(),
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

        assert_eq!(s.turns[0].input_tokens, 850);
        assert_eq!(s.turns[0].cache_read_tokens, 200);
        assert_eq!(s.turns[0].output_tokens, 420);
        assert_eq!(s.turns[0].reasoning_tokens, 300);
        let t0_tools: Vec<&str> = s.turns[0].tool_names.iter().map(AsRef::as_ref).collect();
        assert_eq!(t0_tools, vec!["grep_search"]);

        assert_eq!(s.turns[1].input_tokens, 810);
        assert_eq!(s.turns[1].cache_read_tokens, 1200);
        assert_eq!(s.turns[1].output_tokens, 300);
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

        let p1 = tmp.join("proj-with-root-file");
        fs::create_dir_all(p1.join("chats")).unwrap();
        fs::write(p1.join(".project_root"), "C:\\Users\\dev\\from-project-root\n").unwrap();
        fs::write(p1.join("chats").join("session-1.json"), mk_sess("s-pr", "")).unwrap();

        let p2 = tmp.join("alpha-slug");
        fs::create_dir_all(p2.join("chats")).unwrap();
        fs::write(p2.join("chats").join("session-2.json"), mk_sess("s-slug", "")).unwrap();

        let p3 = tmp.join(&beta_hash);
        fs::create_dir_all(p3.join("chats")).unwrap();
        fs::write(
            p3.join("chats").join("session-3.json"),
            mk_sess("s-hash", &beta_hash),
        )
        .unwrap();

        let p4 = tmp.join("mystery-proj");
        fs::create_dir_all(p4.join("chats")).unwrap();
        fs::write(p4.join("chats").join("session-4.json"), mk_sess("s-none", "")).unwrap();

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

        let r2 = adapter.scan(Utc::now());
        assert_eq!(r2.files_reparsed, 0);
        assert_eq!(r2.files_cached, 2);
        assert_eq!(r2.sessions.len(), 1);

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
        fs::write(dir.path().join("oauth_creds.json"), "{invalid json!!!").unwrap();
        fs::write(dir.path().join("google_accounts.json"), "{invalid json!!!").unwrap();
        fs::write(dir.path().join("jetski-standalone-oauth-token"), "secret").unwrap();
        fs::write(dir.path().join("settings.json"), "{invalid json!!!").unwrap();
        fs::create_dir_all(dir.path().join("antigravity-cli").join("brain")).unwrap();

        let mut adapter = GeminiCliAdapter::new(dir.path().to_path_buf());
        let r0 = adapter.scan(Utc::now());
        assert!(!r0.found_root);
        assert!(r0.sessions.is_empty());

        let chats = dir.path().join("tmp").join("p").join("chats");
        fs::create_dir_all(&chats).unwrap();
        fs::write(chats.join("other.json"), "{invalid json").unwrap();
        fs::write(
            chats.join("session-valid.json"),
            r#"{"sessionId":"ok-1","messages":[{"id":"g1","type":"gemini","model":"gemini-2.5-pro","tokens":{"input":100,"output":20,"cached":0,"thoughts":0,"tool":0}}]}"#,
        )
        .unwrap();
        let r1 = adapter.scan(Utc::now());
        assert!(r1.found_root);
        assert_eq!(r1.skipped_lines, 0);
        assert_eq!(r1.sessions.len(), 1);
        assert_eq!(r1.sessions[0].conversation_id, "ok-1");
    }
}
