//! Strictly read-only incremental adapter for Antigravity (`agy` CLI / 2.0 App / IDE).
//!
//! Only opens whitelisted plain-text files with read-only handles:
//!   - `<data_dir>/brain/<id>/.system_generated/logs/transcript.jsonl`
//!     (fallback to `transcript_full.jsonl` only if absent)
//!   - `<data_dir>/annotations/<id>.pbtxt`
//!   - `<data_dir>/cache/conversation_metadata.json`
//!
//! Never writes to disk, never touches SQLite databases (`conversations/*.db`), never reads credentials.

use chrono::{DateTime, Duration, Local, TimeZone, Utc};
use memchr::memmem;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::pricing::{CostBreakdown, PricingCatalog};
use crate::sources::{
    clean_single_line, compute_running_hint, decode_file_uri, hex_val, is_uuid_like,
    non_symlink_dir, parse_rfc3339_utc, path_eq_platform, read_header_fingerprint,
    relative_components_under, stat_dir_fast, stat_regular_file_fast, try_seek_for_tail,
    AdapterPoll, CharCount, FileSig, FlexTimestamp, HeaderFingerprint, InternedStrSeed,
    OptFlexBool, OptFlexI64, OptFlexString, OptFlexU64, ParsedSession, SafeFs, SourceAdapter,
    SourceScanResult,
};
use crate::stats::{
    intern_str, is_compaction_model, SourceKind, TokenCounts, TurnRecord,
    COMPACTION_MODEL_LABEL,
};

const PARALLEL_MIN_FILES: usize = 2;
const PARALLEL_MIN_BYTES: u64 = 1_048_576;
const PARALLEL_MAX_THREADS: usize = 4;

#[derive(Debug, Clone)]
struct CachedTranscript {
    sig: FileSig,
    header: HeaderFingerprint,
    committed_offset: u64,
    turns: Arc<[TurnRecord]>,
    fallback_step: i64,
    seen_user_input: bool,
    first_user_preview: String,
    subagent_parent_id: Option<String>,
    subagent_preview: String,
    is_swarm_subagent: bool,
    subagent_meta_is_swarm_ping: bool,
    last_step_running: bool,
    committed_skipped_lines: usize,
    trailing_skipped_lines: usize,
    /// Indices into `turns` awaiting trailing GENERIC step timestamps for `gen_secs`.
    pending_gen_indices: Vec<usize>,
    /// Bounded recent GENERIC step timestamps needed only while `pending_gen_indices` is non-empty.
    recent_generic_ts: HashMap<i64, DateTime<Utc>>,
    max_seen_step: i64,
}

impl CachedTranscript {
    fn total_skipped_lines(&self) -> usize {
        self.committed_skipped_lines + self.trailing_skipped_lines
    }
}

#[derive(Debug, Clone)]
struct CachedMetaEntry {
    title: String,
    preview: String,
    workspace: String,
    is_internal: bool,
}

pub struct AntigravityAdapter {
    kind: SourceKind,
    safe_fs: SafeFs,
    transcript_cache: HashMap<PathBuf, CachedTranscript>,
    cid_transcript_path: HashMap<String, PathBuf>,
    /// Flat `annotations/<cid>.pbtxt` cache populated via a single `fs::read_dir` pass using
    /// `DirEntry` metadata (0 extra per-file stats on Windows -> ~0.4 ms vs 8.2 ms!).
    ann_by_cid: HashMap<String, (PathBuf, FileSig, String)>,
    meta_sig: Option<FileSig>,
    meta_cache: HashMap<String, CachedMetaEntry>,
    has_scanned: bool,
    last_found_root: bool,
    last_skipped_empty: usize,
    last_skipped_lines: usize,
}

impl AntigravityAdapter {
    pub fn new(kind: SourceKind, root: PathBuf) -> Self {
        Self {
            kind,
            safe_fs: SafeFs::new(root),
            transcript_cache: HashMap::new(),
            cid_transcript_path: HashMap::new(),
            ann_by_cid: HashMap::new(),
            meta_sig: None,
            meta_cache: HashMap::new(),
            has_scanned: false,
            last_found_root: false,
            last_skipped_empty: 0,
            last_skipped_lines: 0,
        }
    }

    /// Returns `true` if `conversation_metadata.json` changed.
    fn refresh_metadata_cache(&mut self, seen_paths: &mut HashSet<PathBuf>) -> bool {
        let path = self
            .safe_fs
            .root()
            .join("cache")
            .join("conversation_metadata.json");
        let Some((sig, freshly_canon)) = self.safe_fs.is_safe_regular_file_checked(&path) else {
            let had = self.meta_sig.is_some() || !self.meta_cache.is_empty();
            self.meta_sig = None;
            self.meta_cache.clear();
            return had;
        };
        seen_paths.insert(path.clone());
        if self.meta_sig == Some(sig) {
            return false;
        }
        if let Some(fh) = self.safe_fs.open_for_read(&path, freshly_canon) {
            self.meta_cache = parse_metadata_json_reader(fh);
            self.meta_cache.shrink_to_fit();
            self.meta_sig = Some(sig);
            return true;
        }
        false
    }

    /// Scans `annotations/` in a single `fs::read_dir` pass using `DirEntry` metadata on Windows
    /// (zero extra per-file syscalls for unchanged `.pbtxt` files) and only opens modified files.
    fn refresh_annotations_cache(
        &mut self,
        seen_paths: &mut HashSet<PathBuf>,
        seen_dirs: &mut HashSet<PathBuf>,
    ) -> bool {
        let ann_dir = self.safe_fs.root().join("annotations");
        if !self.safe_fs.is_safe_dir(&ann_dir) {
            let had = !self.ann_by_cid.is_empty();
            self.ann_by_cid.clear();
            return had;
        }
        seen_dirs.insert(ann_dir.clone());
        let Ok(entries) = fs::read_dir(&ann_dir) else {
            return false;
        };

        let mut changed = false;
        for entry in entries.flatten() {
            let fname = entry.file_name();
            let Some(name_str) = fname.to_str() else { continue };
            let Some(cid) = name_str.strip_suffix(".pbtxt") else {
                continue;
            };
            if cid.is_empty() {
                continue;
            }
            let path = entry.path();
            let Some((sig, freshly_canon)) =
                self.safe_fs.stat_verified_dir_entry_checked(&path, &entry)
            else {
                continue;
            };
            seen_paths.insert(path.clone());

            if let Some((_, cached_sig, _)) = self.ann_by_cid.get(cid) {
                if *cached_sig == sig {
                    continue;
                }
            }
            let mut title = String::new();
            if let Some(mut fh) = self.safe_fs.open_for_read(&path, freshly_canon) {
                let mut text = String::new();
                if fh.read_to_string(&mut text).is_ok() {
                    title = parse_pbtxt_title(&text);
                }
            }
            self.ann_by_cid.insert(cid.to_string(), (path, sig, title));
            changed = true;
        }

        let prev_len = self.ann_by_cid.len();
        self.ann_by_cid.retain(|_, (p, _, _)| seen_paths.contains(p));
        if self.ann_by_cid.len() != prev_len {
            changed = true;
        }
        changed
    }

    fn lookup_annotation_title(&mut self, cid: &str) -> String {
        if let Some((_, _, title)) = self.ann_by_cid.get(cid) {
            return title.clone();
        }
        let path = self
            .safe_fs
            .root()
            .join("annotations")
            .join(format!("{cid}.pbtxt"));
        let Some((sig, freshly_canon)) = self.safe_fs.is_safe_regular_file_checked(&path) else {
            return String::new();
        };
        let mut title = String::new();
        if let Some(mut fh) = self.safe_fs.open_for_read(&path, freshly_canon) {
            let mut text = String::new();
            if fh.read_to_string(&mut text).is_ok() {
                title = parse_pbtxt_title(&text);
            }
        }
        self.ann_by_cid.insert(cid.to_string(), (path, sig, title.clone()));
        title
    }

    fn build_parsed_session(
        &self,
        cid: String,
        cached: &CachedTranscript,
        scan_now_utc: DateTime<Utc>,
    ) -> ParsedSession {
        let ann_title = self
            .ann_by_cid
            .get(&cid)
            .map(|(_, _, t)| t.as_str())
            .unwrap_or("");
        let meta = self.meta_cache.get(&cid);
        let is_subagent = match meta {
            Some(m) if m.is_internal => true,
            Some(_) => false,
            None => cached.subagent_parent_id.is_some() || cached.is_swarm_subagent,
        };
        let parent_id = if is_subagent {
            cached
                .subagent_parent_id
                .as_deref()
                .filter(|p| *p != cid.as_str())
                .map(String::from)
        } else {
            None
        };
        let title = if !ann_title.is_empty() {
            ann_title.to_string()
        } else if let Some(m) = meta.filter(|m| !m.title.is_empty()) {
            m.title.clone()
        } else if let Some(m) = meta.filter(|m| !m.preview.is_empty()) {
            m.preview.clone()
        } else if !cached.first_user_preview.is_empty() {
            cached.first_user_preview.clone()
        } else if is_subagent && !cached.subagent_preview.is_empty() {
            cached.subagent_preview.clone()
        } else if is_subagent {
            "(子会话)".to_string()
        } else {
            "(未命名会话)".to_string()
        };
        let workspace = meta
            .filter(|m| !m.workspace.is_empty())
            .map(|m| m.workspace.clone())
            .unwrap_or_else(|| "(未知工作区)".to_string());

        let last_file_mtime_utc = cached.sig.mtime_utc();
        let is_running_hint =
            compute_running_hint(cached.last_step_running, last_file_mtime_utc, scan_now_utc);

        ParsedSession {
            conversation_id: cid,
            title,
            workspace,
            source: self.kind,
            is_subagent,
            parent_id,
            parent_title: String::new(),
            turns: Arc::clone(&cached.turns),
            last_file_mtime_utc,
            last_step_running: cached.last_step_running,
            is_running_hint,
        }
    }

    fn scan_internal(&mut self, scan_now_utc: DateTime<Utc>, skip_if_unchanged: bool) -> AdapterPoll {
        self.safe_fs.refresh_root();
        let brain_dir = self.safe_fs.root().join("brain");
        if !self.safe_fs.is_safe_dir(&brain_dir) {
            let changed = !self.has_scanned
                || self.last_found_root
                || !self.transcript_cache.is_empty()
                || !self.ann_by_cid.is_empty()
                || !self.meta_cache.is_empty();
            self.transcript_cache.clear();
            self.cid_transcript_path.clear();
            self.ann_by_cid.clear();
            self.meta_sig = None;
            self.meta_cache.clear();
            self.safe_fs.clear_verified();
            self.has_scanned = true;
            self.last_found_root = false;
            self.last_skipped_empty = 0;
            self.last_skipped_lines = 0;
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
            HashSet::with_capacity(self.transcript_cache.len() + self.ann_by_cid.len() + 8);
        let mut seen_dirs: HashSet<PathBuf> = HashSet::with_capacity(4);
        seen_dirs.insert(brain_dir.clone());

        let mut any_changed = !self.has_scanned || !self.last_found_root;
        if self.refresh_metadata_cache(&mut seen_paths) {
            any_changed = true;
        }
        if self.refresh_annotations_cache(&mut seen_paths, &mut seen_dirs) {
            any_changed = true;
        }

        let Ok(entries) = fs::read_dir(&brain_dir) else {
            self.has_scanned = true;
            self.last_found_root = true;
            return AdapterPoll::Changed(SourceScanResult {
                found_root: true,
                ..Default::default()
            });
        };

        // Use `DirEntry::file_type()` (populated by Win32 FindNextFileW with 0 extra syscalls)
        // to reject symlinks and non-directories without extra stat calls.
        let mut conv_dirs: Vec<(String, PathBuf)> =
            Vec::with_capacity(self.transcript_cache.len() + 8);
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() || !ft.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(cid) = name.to_str() else { continue };
            conv_dirs.push((cid.to_string(), entry.path()));
        }
        conv_dirs.sort_unstable_by(|a, b| a.1.cmp(&b.1));

        struct PendingTranscript {
            t_path: PathBuf,
            sig: FileSig,
            fh: File,
            prev: Option<CachedTranscript>,
            had_prev: bool,
        }

        let mut files_cached = 0usize;
        let mut files_reparsed = 0usize;
        let mut skipped_empty = 0usize;
        let mut skipped_lines = 0usize;
        let mut candidate_files: Vec<(String, PathBuf)> = Vec::with_capacity(conv_dirs.len());
        let mut pending: Vec<PendingTranscript> = Vec::new();
        let mut pending_bytes: u64 = 0;

        for (cid, conv_dir) in conv_dirs {
            // Fast 1-syscall path for already-verified cached transcript paths:
            // skips building `.system_generated/logs/{transcript,transcript_full}.jsonl` paths and
            // avoids `fs::canonicalize()` when the file is still present and non-symlink.
            let mut resolved: Option<(PathBuf, FileSig, bool)> = None;
            if let Some(cached_path) = self.cid_transcript_path.get(&cid).cloned() {
                if let Some(sig) = self.safe_fs.stat_verified_file(&cached_path) {
                    resolved = Some((cached_path, sig, false));
                } else {
                    self.cid_transcript_path.remove(&cid);
                }
            }
            let (t_path, sig, freshly_canon) = if let Some(triple) = resolved {
                triple
            } else {
                let logs_dir = conv_dir.join(".system_generated").join("logs");
                // 1-syscall short-circuit for empty brain/<cid> directories without a `logs/` dir.
                if stat_dir_fast(&logs_dir).is_none() {
                    skipped_empty += 1;
                    continue;
                }
                let primary = logs_dir.join("transcript.jsonl");
                let fallback = logs_dir.join("transcript_full.jsonl");
                let found = if let Some((s, fc)) = self.safe_fs.is_safe_regular_file_checked(&primary)
                {
                    Some((primary, s, fc))
                } else if let Some((s, fc)) = self.safe_fs.is_safe_regular_file_checked(&fallback) {
                    Some((fallback, s, fc))
                } else {
                    None
                };
                let Some((p, s, fc)) = found else {
                    skipped_empty += 1;
                    continue;
                };
                self.cid_transcript_path.insert(cid.clone(), p.clone());
                (p, s, fc)
            };

            seen_paths.insert(t_path.clone());

            let need_reparse = match self.transcript_cache.get(&t_path) {
                Some(entry) if entry.sig == sig => {
                    files_cached += 1;
                    false
                }
                _ => true,
            };

            if need_reparse {
                let prev = self.transcript_cache.remove(&t_path);
                let had_prev = prev.is_some();
                if let Some(fh) = self.safe_fs.open_for_read(&t_path, freshly_canon) {
                    pending_bytes = pending_bytes.saturating_add(sig.len);
                    pending.push(PendingTranscript {
                        t_path: t_path.clone(),
                        sig,
                        fh,
                        prev,
                        had_prev,
                    });
                    candidate_files.push((cid, t_path));
                } else if had_prev {
                    any_changed = true;
                }
            } else {
                candidate_files.push((cid, t_path));
            }
        }

        // Parse modified/new transcripts — bounded parallel `std::thread::scope` (<=4 threads,
        // 0 persistent threads) when cold-loading multiple large files, inline otherwise.
        if pending.len() >= PARALLEL_MIN_FILES && pending_bytes >= PARALLEL_MIN_BYTES {
            // Sort ascending by size so `.pop()` dispatches largest files first (LPT scheduling).
            pending.sort_unstable_by_key(|p| p.sig.len);
            let n_threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(2)
                .clamp(2, PARALLEL_MAX_THREADS)
                .min(pending.len());
            let queue = Mutex::new(pending);
            let mut results: Vec<(PathBuf, Option<CachedTranscript>, bool)> = Vec::new();
            std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(n_threads);
                for _ in 0..n_threads {
                    handles.push(scope.spawn(|| {
                        let mut local = Vec::new();
                        loop {
                            let task = {
                                let mut q = queue.lock().unwrap();
                                q.pop()
                            };
                            let Some(t) = task else { break };
                            let parsed = parse_or_tail_transcript(t.fh, t.sig, t.prev);
                            local.push((t.t_path, parsed, t.had_prev));
                        }
                        local
                    }));
                }
                for h in handles {
                    if let Ok(local) = h.join() {
                        results.extend(local);
                    }
                }
            });
            for (t_path, parsed, had_prev) in results {
                if let Some(fresh) = parsed {
                    files_reparsed += 1;
                    any_changed = true;
                    self.transcript_cache.insert(t_path, fresh);
                } else if had_prev {
                    any_changed = true;
                }
            }
        } else {
            for t in pending {
                if let Some(fresh) = parse_or_tail_transcript(t.fh, t.sig, t.prev) {
                    files_reparsed += 1;
                    any_changed = true;
                    self.transcript_cache.insert(t.t_path, fresh);
                } else if t.had_prev {
                    any_changed = true;
                }
            }
        }

        let mut active_files: Vec<(String, PathBuf)> = Vec::with_capacity(candidate_files.len());
        for (cid, t_path) in candidate_files {
            if let Some(entry) = self.transcript_cache.get(&t_path) {
                skipped_lines += entry.total_skipped_lines();
                if entry.turns.is_empty() {
                    skipped_empty += 1;
                    continue;
                }
                active_files.push((cid, t_path));
            }
        }

        let prev_cache_len = self.transcript_cache.len();
        self.transcript_cache.retain(|k, _| seen_paths.contains(k));
        self.cid_transcript_path.retain(|_, p| seen_paths.contains(p));
        if self.transcript_cache.len() != prev_cache_len {
            any_changed = true;
        }
        self.safe_fs.retain_verified(&seen_paths);
        self.safe_fs.retain_verified_dirs(&seen_dirs);

        self.has_scanned = true;
        self.last_found_root = true;
        self.last_skipped_empty = skipped_empty;
        self.last_skipped_lines = skipped_lines;

        if skip_if_unchanged && !any_changed {
            return AdapterPoll::Unchanged {
                found_root: true,
                files_cached,
                skipped_empty,
                skipped_lines,
            };
        }

        let mut sessions = Vec::with_capacity(active_files.len());
        for (cid, t_path) in active_files {
            let Some(cached) = self.transcript_cache.get(&t_path) else {
                continue;
            };
            sessions.push(self.build_parsed_session(cid, cached, scan_now_utc));
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

    fn poll_hinted_internal(
        &mut self,
        paths: &[PathBuf],
        scan_now_utc: DateTime<Utc>,
    ) -> Option<AdapterPoll> {
        if !self.has_scanned || !self.last_found_root || paths.is_empty() {
            return None;
        }
        self.safe_fs.refresh_root();

        let mut transcript_hints: Vec<(String, PathBuf)> = Vec::new();
        let mut ann_hints: Vec<(String, PathBuf)> = Vec::new();
        let mut meta_hinted = false;

        for p in paths {
            let comps = relative_components_under(self.safe_fs.root(), p)?;
            match comps.as_slice() {
                [b, cid, sg, logs, fname]
                    if b.eq_ignore_ascii_case("brain")
                        && sg.eq_ignore_ascii_case(".system_generated")
                        && logs.eq_ignore_ascii_case("logs")
                        && (fname.eq_ignore_ascii_case("transcript.jsonl")
                            || fname.eq_ignore_ascii_case("transcript_full.jsonl")) =>
                {
                    if !transcript_hints.iter().any(|(c, _)| c == cid) {
                        transcript_hints.push((cid.clone(), p.clone()));
                    }
                }
                [ann, fname] if ann.eq_ignore_ascii_case("annotations") => {
                    let stem = fname
                        .strip_suffix(".pbtxt")
                        .or_else(|| fname.strip_suffix(".PBTXT"))?;
                    if stem.is_empty() {
                        return None;
                    }
                    if !ann_hints.iter().any(|(c, _)| c == stem) {
                        ann_hints.push((stem.to_string(), p.clone()));
                    }
                }
                [cache, fname]
                    if cache.eq_ignore_ascii_case("cache")
                        && fname.eq_ignore_ascii_case("conversation_metadata.json") =>
                {
                    meta_hinted = true;
                }
                _ => return None,
            }
        }

        if meta_hinted {
            let meta_path = self
                .safe_fs
                .root()
                .join("cache")
                .join("conversation_metadata.json");
            let Some((sig, _)) = self.safe_fs.is_safe_regular_file_checked(&meta_path) else {
                return None;
            };
            if self.meta_sig != Some(sig) {
                // Metadata can update titles/workspaces across many sessions: run clean full sweep.
                return None;
            }
        }

        let mut affected_cids: HashSet<String> = HashSet::new();
        let mut files_reparsed = 0usize;

        for (cid, raw_ann_path) in ann_hints {
            let ann_path = self
                .ann_by_cid
                .get(&cid)
                .filter(|(p, _, _)| path_eq_platform(p, &raw_ann_path))
                .map(|(p, _, _)| p.clone())
                .unwrap_or(raw_ann_path);
            let Some((sig, freshly_canon)) = self.safe_fs.is_safe_regular_file_checked(&ann_path)
            else {
                // Deleted annotation: fall back cleanly to full sweep.
                return None;
            };
            if self
                .ann_by_cid
                .get(&cid)
                .is_some_and(|(_, prev_sig, _)| *prev_sig == sig)
            {
                continue;
            }
            let mut title = String::new();
            if let Some(mut fh) = self.safe_fs.open_for_read(&ann_path, freshly_canon) {
                let mut text = String::new();
                if fh.read_to_string(&mut text).is_ok() {
                    title = parse_pbtxt_title(&text);
                }
            }
            self.ann_by_cid
                .insert(cid.clone(), (ann_path, sig, title));
            if self.cid_transcript_path.contains_key(&cid) {
                affected_cids.insert(cid);
            }
        }

        for (cid, raw_t_path) in transcript_hints {
            let t_path = self
                .cid_transcript_path
                .get(&cid)
                .filter(|existing| path_eq_platform(existing, &raw_t_path))
                .cloned()
                .unwrap_or(raw_t_path);
            let (sig, freshly_canon) = if self.safe_fs.is_verified_file(&t_path) {
                let Some(s) = self.safe_fs.stat_verified_file(&t_path) else {
                    // Deleted or replaced with symlink: fall back cleanly to full sweep.
                    return None;
                };
                (s, false)
            } else {
                self.safe_fs.is_safe_regular_file_checked(&t_path)?
            };

            if self
                .transcript_cache
                .get(&t_path)
                .is_some_and(|c| c.sig == sig)
            {
                continue;
            }

            let fh = self.safe_fs.open_for_read(&t_path, freshly_canon)?;
            let prev = self.transcript_cache.remove(&t_path);
            let fresh = parse_or_tail_transcript(fh, sig, prev)?;
            self.transcript_cache.insert(t_path.clone(), fresh);
            self.cid_transcript_path.insert(cid.clone(), t_path);
            files_reparsed += 1;
            affected_cids.insert(cid);
        }

        if affected_cids.is_empty() {
            return Some(AdapterPoll::Unchanged {
                found_root: true,
                files_cached: self.transcript_cache.len(),
                skipped_empty: self.last_skipped_empty,
                skipped_lines: self.last_skipped_lines,
            });
        }

        let mut skipped_lines = 0usize;
        for entry in self.transcript_cache.values() {
            skipped_lines += entry.total_skipped_lines();
        }
        self.last_skipped_lines = skipped_lines;

        let mut updated_sessions = Vec::with_capacity(affected_cids.len());
        let mut removed_cids = Vec::new();
        for cid in affected_cids {
            let Some(t_path) = self.cid_transcript_path.get(&cid) else {
                removed_cids.push(cid);
                continue;
            };
            let Some(cached) = self.transcript_cache.get(t_path) else {
                removed_cids.push(cid);
                continue;
            };
            if cached.turns.is_empty() {
                removed_cids.push(cid);
            } else {
                updated_sessions.push(self.build_parsed_session(cid, cached, scan_now_utc));
            }
        }

        Some(AdapterPoll::Delta {
            updated_sessions,
            removed_cids,
            files_cached: self.transcript_cache.len().saturating_sub(files_reparsed),
            files_reparsed,
            skipped_empty: self.last_skipped_empty,
            skipped_lines,
        })
    }
}

impl SourceAdapter for AntigravityAdapter {
    fn kind(&self) -> SourceKind {
        self.kind
    }

    fn root(&self) -> &Path {
        self.safe_fs.root()
    }

    fn watched_dirs(&self) -> Vec<PathBuf> {
        let root = self.safe_fs.root();
        let mut out = Vec::new();
        for sub in ["brain", "annotations", "cache"] {
            let p = root.join(sub);
            if non_symlink_dir(&p) {
                out.push(p);
            }
        }
        out
    }

    fn lookup_session_hint(&mut self, cid: &str) -> Option<(String, String)> {
        let p_ann = self.lookup_annotation_title(cid);
        let p_meta = self.meta_cache.get(cid);
        let ptitle = if !p_ann.is_empty() {
            p_ann
        } else if let Some(m) = p_meta.filter(|m| !m.title.is_empty()) {
            m.title.clone()
        } else if let Some(m) = p_meta.filter(|m| !m.preview.is_empty()) {
            m.preview.clone()
        } else {
            String::new()
        };
        let pws = p_meta
            .filter(|m| !m.workspace.is_empty())
            .map(|m| m.workspace.clone())
            .unwrap_or_default();
        if !ptitle.is_empty() || !pws.is_empty() {
            Some((ptitle, pws))
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

    fn poll_hinted(&mut self, paths: &[PathBuf], scan_now_utc: DateTime<Utc>) -> Option<AdapterPoll> {
        self.poll_hinted_internal(paths, scan_now_utc)
    }

    fn adopt_priced_turns(
        &mut self,
        cid: &str,
        old_turns: &Arc<[TurnRecord]>,
        priced_turns: &Arc<[TurnRecord]>,
    ) {
        if let Some(t_path) = self.cid_transcript_path.get(cid) {
            if let Some(entry) = self.transcript_cache.get_mut(t_path) {
                if Arc::ptr_eq(&entry.turns, old_turns) {
                    entry.turns = Arc::clone(priced_turns);
                }
            }
        }
    }
}

// ---------- Zero-DOM Streaming Transcript Line Parser + Incremental Byte-Offset Tailing ----------

struct RawAgyLine {
    step_type: Option<String>,
    step_index: Option<i64>,
    created_at: Option<DateTime<Utc>>,
    model: Option<Arc<str>>,
    has_tokens: bool,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: Option<u64>,
    cache_creation_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    output_tokens: u64,
    tool_names: Box<[Arc<str>]>,
    thinking_chars: u64,
    content: Option<String>,
}

struct AgyLineSeed {
    capture_content: bool,
    capture_tools: bool,
    capture_thinking: bool,
}

impl<'de> de::DeserializeSeed<'de> for AgyLineSeed {
    type Value = RawAgyLine;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V {
            capture_content: bool,
            capture_tools: bool,
            capture_thinking: bool,
        }
        impl<'de> Visitor<'de> for V {
            type Value = RawAgyLine;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Antigravity transcript JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = RawAgyLine {
                    step_type: None,
                    step_index: None,
                    created_at: None,
                    model: None,
                    has_tokens: false,
                    input_tokens: 0,
                    cache_read_tokens: 0,
                    cache_write_tokens: None,
                    cache_creation_tokens: None,
                    cache_creation_input_tokens: None,
                    output_tokens: 0,
                    tool_names: Box::new([]),
                    thinking_chars: 0,
                    content: None,
                };
                while let Some(key) = map.next_key::<&str>()? {
                    match key {
                        "type" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                out.step_type = Some(s);
                            }
                        }
                        "step_index" => {
                            if let OptFlexI64(Some(v)) = map.next_value()? {
                                out.step_index = Some(v);
                            }
                        }
                        "created_at" => {
                            let FlexTimestamp(ts) = map.next_value()?;
                            out.created_at = ts;
                        }
                        "model" => {
                            out.model = map.next_value_seed(InternedStrSeed)?;
                        }
                        "input_tokens" => {
                            if let OptFlexU64(Some(v)) = map.next_value()? {
                                out.has_tokens = true;
                                out.input_tokens = v;
                            }
                        }
                        "cache_read_tokens" => {
                            if let OptFlexU64(Some(v)) = map.next_value()? {
                                out.has_tokens = true;
                                out.cache_read_tokens = v;
                            }
                        }
                        "cache_write_tokens" => {
                            if let OptFlexU64(Some(v)) = map.next_value()? {
                                out.has_tokens = true;
                                out.cache_write_tokens = Some(v);
                            }
                        }
                        "cache_creation_tokens" => {
                            if let OptFlexU64(Some(v)) = map.next_value()? {
                                out.has_tokens = true;
                                out.cache_creation_tokens = Some(v);
                            }
                        }
                        "cache_creation_input_tokens" => {
                            if let OptFlexU64(Some(v)) = map.next_value()? {
                                out.has_tokens = true;
                                out.cache_creation_input_tokens = Some(v);
                            }
                        }
                        "output_tokens" => {
                            if let OptFlexU64(Some(v)) = map.next_value()? {
                                out.has_tokens = true;
                                out.output_tokens = v;
                            }
                        }
                        "tool_calls" if self.capture_tools => {
                            out.tool_names = map.next_value_seed(AgyToolCallsSeed)?;
                        }
                        "thinking" if self.capture_thinking => {
                            let CharCount(c) = map.next_value()?;
                            out.thinking_chars = c;
                        }
                        "content" if self.capture_content => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                out.content = Some(s);
                            }
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
            capture_content: self.capture_content,
            capture_tools: self.capture_tools,
            capture_thinking: self.capture_thinking,
        })
    }
}

struct AgyToolCallsSeed;

impl<'de> de::DeserializeSeed<'de> for AgyToolCallsSeed {
    type Value = Box<[Arc<str>]>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Box<[Arc<str>]>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("tool_calls array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut names = Vec::new();
                while let Some(opt_name) = seq.next_element_seed(AgyToolCallItemSeed)? {
                    if let Some(n) = opt_name {
                        names.push(n);
                    }
                }
                Ok(names.into_boxed_slice())
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(Box::new([]))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Box::new([]))
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct AgyToolCallItemSeed;

impl<'de> de::DeserializeSeed<'de> for AgyToolCallItemSeed {
    type Value = Option<Arc<str>>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<Arc<str>>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("tool_call object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut name = None;
                while let Some(k) = map.next_key::<&str>()? {
                    if k == "name" {
                        name = map.next_value_seed(InternedStrSeed)?;
                    } else {
                        // Skip massive `args` payloads (35+ MB!) with zero heap allocations!
                        let _: IgnoredAny = map.next_value()?;
                    }
                }
                Ok(name)
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

fn parse_or_tail_transcript(
    mut fh: File,
    sig: FileSig,
    prev: Option<CachedTranscript>,
) -> Option<CachedTranscript> {
    if let Some(prev_state) = prev {
        if let Some(fresh_hdr) =
            try_seek_for_tail(&mut fh, prev_state.sig, sig, prev_state.header, prev_state.committed_offset)
        {
            return tail_transcript_from_offset(fh, sig, fresh_hdr, prev_state);
        }
    }
    let header = read_header_fingerprint(&mut fh)?;
    let _ = fh.seek(SeekFrom::Start(0));
    parse_transcript_full(fh, sig, header)
}

#[inline]
fn trim_ascii_ws(b: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = b.len();
    while start < end && b[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && b[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &b[start..end]
}

/// Borrows up to 320 leading valid UTF-8 bytes of a JSONL line without scanning the full line.
#[inline]
fn header_str_prefix(trimmed_bytes: &[u8]) -> &str {
    let slice = &trimmed_bytes[..trimmed_bytes.len().min(320)];
    match std::str::from_utf8(slice) {
        Ok(s) => s,
        Err(e) => std::str::from_utf8(&slice[..e.valid_up_to()]).unwrap_or(""),
    }
}

#[inline]
fn bytes_to_str_lossy(b: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(b) {
        Ok(s) => Cow::Borrowed(s),
        Err(_) => String::from_utf8_lossy(b),
    }
}

fn parse_transcript_full(
    fh: File,
    sig: FileSig,
    header: HeaderFingerprint,
) -> Option<CachedTranscript> {
    let mut reader = BufReader::with_capacity(64 * 1024, fh);
    let mut by_step: BTreeMap<i64, TurnRecord> = BTreeMap::new();
    let mut fallback_step: i64 = -1;
    let mut seen_user_input = false;
    let mut first_user_preview = String::new();
    let mut subagent_parent_id: Option<String> = None;
    let mut subagent_preview = String::new();
    let mut is_swarm_subagent = false;
    let mut subagent_meta_is_swarm_ping = false;
    let mut last_step_running = false;
    let mut committed_skipped_lines = 0usize;
    let mut trailing_skipped_lines = 0usize;
    let mut generic_ts: HashMap<i64, DateTime<Utc>> = HashMap::new();
    let mut max_seen_step: i64 = -1;
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut offset: u64 = 0;
    let mut committed_offset: u64 = 0;
    let tok_finder = memmem::Finder::new(b"_tokens");

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

        let trimmed_bytes = trim_ascii_ws(&buf);
        if trimmed_bytes.is_empty() {
            if has_newline {
                committed_offset = offset;
            }
            continue;
        }

        // Inspect ONLY the leading <=320-byte ASCII JSON header before any full-line UTF-8 check!
        let head = header_str_prefix(trimmed_bytes);
        let is_generic = if let Some((step, ts)) = parse_generic_step_header(head) {
            generic_ts.insert(step, ts);
            if step > max_seen_step {
                max_seen_step = step;
            }
            true
        } else {
            false
        };
        if let Some(running) = parse_step_status_running(head) {
            last_step_running = running;
        }

        let need_preview = !seen_user_input && memmem::find(trimmed_bytes, b"USER_INPUT").is_some();
        let want_subagent_meta = !seen_user_input
            && by_step.is_empty()
            && ((subagent_parent_id.is_none() && subagent_preview.is_empty())
                || subagent_meta_is_swarm_ping);
        let need_subagent_meta =
            want_subagent_meta && memmem::find(trimmed_bytes, b"SYSTEM_MESSAGE").is_some();

        // Fast-path skip complete `{...}` lines that cannot contain tokens or needed previews
        // (skipping ~70% of bytes with ZERO full-line UTF-8 validation or JSON parsing!).
        let has_tokens_substr = !is_generic && tok_finder.find(trimmed_bytes).is_some();
        if trimmed_bytes.first() == Some(&b'{')
            && trimmed_bytes.last() == Some(&b'}')
            && !need_preview
            && !need_subagent_meta
            && !has_tokens_substr
        {
            if has_newline {
                committed_offset = offset;
            }
            continue;
        }

        let line_cow = bytes_to_str_lossy(trimmed_bytes);
        let seed = AgyLineSeed {
            capture_content: need_preview || need_subagent_meta,
            capture_tools: has_tokens_substr,
            capture_thinking: has_tokens_substr,
        };
        let mut de = serde_json::Deserializer::from_str(&line_cow);
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

        if let Some(s) = parsed.step_index {
            if s > max_seen_step {
                max_seen_step = s;
            }
        }
        apply_parsed_agy_line(
            parsed,
            &mut by_step,
            &mut fallback_step,
            &mut seen_user_input,
            &mut first_user_preview,
            &mut subagent_parent_id,
            &mut subagent_preview,
            &mut is_swarm_subagent,
            &mut subagent_meta_is_swarm_ping,
            want_subagent_meta,
        );
    }

    let mut turns: Vec<TurnRecord> = by_step.into_values().collect();
    turns.sort_by_key(|t| {
        (
            if t.step_index >= 0 { t.step_index } else { i64::MAX },
            t.timestamp_utc,
        )
    });

    let mut pending_gen_indices = Vec::new();
    let mut min_pending_step = i64::MAX;
    for (idx, t) in turns.iter_mut().enumerate() {
        t.gen_secs = estimate_gen_secs(t, &generic_ts);
        if t.gen_secs.is_none()
            && !t.is_checkpoint
            && !t.tool_names.is_empty()
            && t.step_index >= 0
            && max_seen_step <= t.step_index + t.tool_names.len() as i64 + 4
        {
            pending_gen_indices.push(idx);
            min_pending_step = min_pending_step.min(t.step_index);
        }
    }

    // Retain ONLY generic timestamps needed for unresolved tail turns (empty for finished sessions!).
    if pending_gen_indices.is_empty() {
        generic_ts.clear();
        generic_ts.shrink_to_fit();
    } else {
        generic_ts.retain(|&s, _| s >= min_pending_step);
    }

    if seen_user_input {
        subagent_parent_id = None;
        subagent_preview.clear();
        is_swarm_subagent = false;
    }

    Some(CachedTranscript {
        sig,
        header,
        committed_offset,
        turns: turns.into(),
        fallback_step,
        seen_user_input,
        first_user_preview,
        subagent_parent_id,
        subagent_preview,
        is_swarm_subagent,
        subagent_meta_is_swarm_ping,
        last_step_running,
        committed_skipped_lines,
        trailing_skipped_lines,
        pending_gen_indices,
        recent_generic_ts: generic_ts,
        max_seen_step,
    })
}

fn tail_transcript_from_offset(
    fh: File,
    sig: FileSig,
    header: HeaderFingerprint,
    mut state: CachedTranscript,
) -> Option<CachedTranscript> {
    let mut reader = BufReader::with_capacity(64 * 1024, fh);
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut offset: u64 = state.committed_offset;
    let mut committed_offset: u64 = state.committed_offset;
    let mut trailing_skipped_lines = 0usize;
    let mut new_turns: BTreeMap<i64, TurnRecord> = BTreeMap::new();
    let mut any_new_generic = false;
    let tok_finder = memmem::Finder::new(b"_tokens");

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

        let trimmed_bytes = trim_ascii_ws(&buf);
        if trimmed_bytes.is_empty() {
            if has_newline {
                committed_offset = offset;
            }
            continue;
        }

        let head = header_str_prefix(trimmed_bytes);
        let is_generic = if let Some((step, ts)) = parse_generic_step_header(head) {
            state.recent_generic_ts.insert(step, ts);
            if step > state.max_seen_step {
                state.max_seen_step = step;
            }
            any_new_generic = true;
            true
        } else {
            false
        };
        if let Some(running) = parse_step_status_running(head) {
            state.last_step_running = running;
        }

        let need_preview =
            !state.seen_user_input && memmem::find(trimmed_bytes, b"USER_INPUT").is_some();
        let want_subagent_meta = !state.seen_user_input
            && state.turns.is_empty()
            && new_turns.is_empty()
            && ((state.subagent_parent_id.is_none() && state.subagent_preview.is_empty())
                || state.subagent_meta_is_swarm_ping);
        let need_subagent_meta =
            want_subagent_meta && memmem::find(trimmed_bytes, b"SYSTEM_MESSAGE").is_some();

        let has_tokens_substr = !is_generic && tok_finder.find(trimmed_bytes).is_some();
        if trimmed_bytes.first() == Some(&b'{')
            && trimmed_bytes.last() == Some(&b'}')
            && !need_preview
            && !need_subagent_meta
            && !has_tokens_substr
        {
            if has_newline {
                committed_offset = offset;
            }
            continue;
        }

        let line_cow = bytes_to_str_lossy(trimmed_bytes);
        let seed = AgyLineSeed {
            capture_content: need_preview || need_subagent_meta,
            capture_tools: has_tokens_substr,
            capture_thinking: has_tokens_substr,
        };
        let mut de = serde_json::Deserializer::from_str(&line_cow);
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

        if let Some(s) = parsed.step_index {
            if s > state.max_seen_step {
                state.max_seen_step = s;
            }
        }
        apply_parsed_agy_line(
            parsed,
            &mut new_turns,
            &mut state.fallback_step,
            &mut state.seen_user_input,
            &mut state.first_user_preview,
            &mut state.subagent_parent_id,
            &mut state.subagent_preview,
            &mut state.is_swarm_subagent,
            &mut state.subagent_meta_is_swarm_ping,
            want_subagent_meta,
        );
    }

    if state.seen_user_input {
        state.subagent_parent_id = None;
        state.subagent_preview.clear();
        state.is_swarm_subagent = false;
    }

    let need_resolve_pending = any_new_generic && !state.pending_gen_indices.is_empty();
    if !new_turns.is_empty() || need_resolve_pending {
        let mut turns_vec: Vec<TurnRecord> = state.turns.to_vec();
        let mut need_sort = false;

        for (step, turn) in new_turns {
            if step >= 0 {
                if let Some(pos) = turns_vec.iter().rposition(|t| t.step_index == step) {
                    turns_vec[pos] = turn;
                    if !state.pending_gen_indices.contains(&pos)
                        && !turns_vec[pos].is_checkpoint
                        && !turns_vec[pos].tool_names.is_empty()
                    {
                        state.pending_gen_indices.push(pos);
                    }
                    continue;
                }
                if let Some(last) = turns_vec.last() {
                    if last.step_index < 0 || step < last.step_index {
                        need_sort = true;
                    }
                }
            } else if turns_vec.last().is_some() {
                need_sort = true;
            }
            let new_idx = turns_vec.len();
            let track_pending = !turn.is_checkpoint && !turn.tool_names.is_empty() && turn.step_index >= 0;
            turns_vec.push(turn);
            if track_pending {
                state.pending_gen_indices.push(new_idx);
            }
        }

        if need_sort {
            turns_vec.sort_by_key(|t| {
                (
                    if t.step_index >= 0 { t.step_index } else { i64::MAX },
                    t.timestamp_utc,
                )
            });
            // Recompute pending indices after resort.
            state.pending_gen_indices.clear();
            for (idx, t) in turns_vec.iter().enumerate() {
                if t.gen_secs.is_none()
                    && !t.is_checkpoint
                    && !t.tool_names.is_empty()
                    && t.step_index >= 0
                    && state.max_seen_step <= t.step_index + t.tool_names.len() as i64 + 4
                {
                    state.pending_gen_indices.push(idx);
                }
            }
        }

        // Resolve gen_secs on pending tail turns and prune completed ones.
        let mut still_pending = Vec::new();
        let mut min_pending_step = i64::MAX;
        for idx in state.pending_gen_indices.drain(..) {
            if let Some(t) = turns_vec.get_mut(idx) {
                if t.gen_secs.is_none() {
                    t.gen_secs = estimate_gen_secs(t, &state.recent_generic_ts);
                }
                if t.gen_secs.is_none()
                    && state.max_seen_step <= t.step_index + t.tool_names.len() as i64 + 4
                {
                    still_pending.push(idx);
                    min_pending_step = min_pending_step.min(t.step_index);
                }
            }
        }
        state.pending_gen_indices = still_pending;
        if state.pending_gen_indices.is_empty() {
            state.recent_generic_ts.clear();
        } else {
            state.recent_generic_ts.retain(|&s, _| s >= min_pending_step);
        }
        state.turns = turns_vec.into();
    } else if state.pending_gen_indices.is_empty() && state.recent_generic_ts.len() > 64 {
        state.recent_generic_ts.clear();
    }

    state.sig = sig;
    state.header = header;
    state.committed_offset = committed_offset;
    state.trailing_skipped_lines = trailing_skipped_lines;
    Some(state)
}

#[allow(clippy::too_many_arguments)]
fn apply_parsed_agy_line(
    parsed: RawAgyLine,
    by_step: &mut BTreeMap<i64, TurnRecord>,
    fallback_step: &mut i64,
    seen_user_input: &mut bool,
    first_user_preview: &mut String,
    subagent_parent_id: &mut Option<String>,
    subagent_preview: &mut String,
    is_swarm_subagent: &mut bool,
    subagent_meta_is_swarm_ping: &mut bool,
    want_subagent_meta: bool,
) {
    let step_type = parsed.step_type.as_deref().unwrap_or("");
    if step_type == "USER_INPUT" {
        *seen_user_input = true;
        if first_user_preview.is_empty() {
            if let Some(content) = parsed.content.as_deref() {
                *first_user_preview = extract_user_preview(content);
            }
        }
    } else if want_subagent_meta && step_type == "SYSTEM_MESSAGE" {
        if let Some(content) = parsed.content.as_deref() {
            let meta = extract_subagent_meta(content);
            if meta.is_swarm {
                *is_swarm_subagent = true;
            }
            if *subagent_meta_is_swarm_ping {
                if meta.is_swarm_ping {
                    if subagent_parent_id.is_none() && meta.parent_id.is_some() {
                        *subagent_parent_id = meta.parent_id;
                    }
                } else {
                    if meta.parent_id.is_some() {
                        *subagent_parent_id = meta.parent_id;
                    }
                    if !meta.preview.is_empty() {
                        *subagent_preview = meta.preview;
                    }
                    *subagent_meta_is_swarm_ping = false;
                }
            } else {
                *subagent_parent_id = meta.parent_id;
                *subagent_preview = meta.preview;
                *subagent_meta_is_swarm_ping = meta.is_swarm_ping;
            }
        }
    }

    if !parsed.has_tokens {
        return;
    }

    let input_tokens = parsed.input_tokens;
    let cache_read_tokens = parsed.cache_read_tokens;
    let cache_write_tokens = parsed
        .cache_write_tokens
        .or(parsed.cache_creation_tokens)
        .or(parsed.cache_creation_input_tokens)
        .unwrap_or(0);
    let output_tokens = parsed.output_tokens;
    if input_tokens + cache_read_tokens + cache_write_tokens + output_tokens == 0 {
        return;
    }

    let is_checkpoint = step_type == "CHECKPOINT";
    let model = parsed.model.unwrap_or_else(|| {
        intern_str(if is_checkpoint {
            COMPACTION_MODEL_LABEL
        } else {
            "unknown"
        })
    });

    let step_index = match parsed.step_index {
        Some(s) => s,
        None => {
            let s = *fallback_step;
            *fallback_step -= 1;
            s
        }
    };

    by_step.insert(
        step_index,
        TurnRecord {
            step_index,
            timestamp_utc: parsed.created_at,
            model,
            is_checkpoint,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            cache_write_1h_tokens: 0,
            output_tokens,
            reasoning_tokens: 0,
            tool_names: parsed.tool_names,
            thinking_chars: parsed.thinking_chars,
            gen_secs: None,
            cost: CostBreakdown::default(),
        },
    );
}

// ---------- Ultra-Fast Today-Only Scanner for `agy-statusline.exe` ----------

struct TodayTurnLite {
    model: Arc<str>,
    is_checkpoint: bool,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    output_tokens: u64,
    is_today: bool,
}

/// Strictly read-only, mtime-pruned, zero-DOM streaming scanner computing today's Antigravity
/// total tokens, calls, and USD cost without reading `conversation_metadata.json`,
/// `annotations/*.pbtxt`, or historical files untouched today.
pub fn scan_antigravity_today_totals(
    sources: &[(SourceKind, PathBuf)],
    pricing: &PricingCatalog,
) -> TokenCounts {
    let today_local = Local::now().date_naive();
    scan_antigravity_date_totals(sources, pricing, today_local)
}

pub(crate) fn scan_antigravity_date_totals(
    sources: &[(SourceKind, PathBuf)],
    pricing: &PricingCatalog,
    target_local_date: chrono::NaiveDate,
) -> TokenCounts {
    let next_local_date = target_local_date
        .succ_opt()
        .unwrap_or(target_local_date);
    let start_naive = target_local_date
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let end_naive = next_local_date
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let start_utc = Local
        .from_local_datetime(&start_naive)
        .earliest()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.from_utc_datetime(&start_naive));
    let end_utc = Local
        .from_local_datetime(&end_naive)
        .latest()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.from_utc_datetime(&end_naive));
    // 60s skew margin; guard against synthetic future-dated test fixtures relative to host clock.
    let now_utc = Utc::now();
    let mtime_cutoff = if start_utc <= now_utc {
        Some(start_utc - Duration::seconds(60))
    } else {
        None
    };

    // Deduplicate same conversation_id across CLI/App/IDE roots (more total turns wins, matching IncrementalScanner).
    struct TodayFileTask {
        cid: String,
        len: u64,
        fh: File,
    }
    let mut tasks: Vec<TodayFileTask> = Vec::new();
    let mut total_bytes: u64 = 0;

    for (kind, root) in sources {
        if kind.agent_family() != crate::stats::AgentFamily::Antigravity {
            continue;
        }
        let mut safe_fs = SafeFs::new(root.clone());
        let brain_dir = root.join("brain");
        if !safe_fs.is_safe_dir(&brain_dir) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&brain_dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() || !ft.is_dir() {
                continue;
            }
            let fname = entry.file_name();
            let Some(cid) = fname.to_str() else { continue };
            let logs_dir = entry.path().join(".system_generated").join("logs");
            let primary = logs_dir.join("transcript.jsonl");

            // 1-syscall fast mtime check BEFORE fs::canonicalize(): skips ~190 old sessions with
            // ZERO canonicalize() calls and ZERO File::open() handles!
            let (t_path, pre_sig) = if let Some(s) = stat_regular_file_fast(&primary) {
                (primary, s)
            } else if stat_dir_fast(&logs_dir).is_some() {
                let fallback = logs_dir.join("transcript_full.jsonl");
                let Some(s) = stat_regular_file_fast(&fallback) else {
                    continue;
                };
                (fallback, s)
            } else {
                continue;
            };

            if let Some(cutoff) = mtime_cutoff {
                if let Some(mtime) = pre_sig.mtime_utc() {
                    if mtime < cutoff {
                        continue;
                    }
                }
            }

            // Enforce single symlink + canonical root-containment check before opening kept files.
            let Some((sig, fh)) = safe_fs.open_safe_regular_file(&t_path) else {
                continue;
            };
            total_bytes = total_bytes.saturating_add(sig.len);
            tasks.push(TodayFileTask {
                cid: cid.to_string(),
                len: sig.len,
                fh,
            });
        }
    }

    let mut by_cid: HashMap<String, (usize, Option<DateTime<Utc>>, TokenCounts)> =
        HashMap::with_capacity(tasks.len());
    let mut merge_result = |cid: String, total_turns: usize, last_ts: Option<DateTime<Utc>>, counts: TokenCounts| {
        if total_turns == 0 {
            return;
        }
        let replace = match by_cid.get(&cid) {
            None => true,
            Some((prev_n, prev_ts, _)) => {
                total_turns > *prev_n || (total_turns == *prev_n && last_ts > *prev_ts)
            }
        };
        if replace {
            by_cid.insert(cid, (total_turns, last_ts, counts));
        }
    };

    if tasks.len() >= PARALLEL_MIN_FILES && total_bytes >= PARALLEL_MIN_BYTES {
        // Sort ascending so `.pop()` dispatches largest 8-10 MiB files first (LPT load balance).
        tasks.sort_unstable_by_key(|t| t.len);
        let n_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .clamp(2, PARALLEL_MAX_THREADS)
            .min(tasks.len());
        let queue = Mutex::new(tasks);
        let queue_ref = &queue;
        let mut all_res: Vec<(String, usize, Option<DateTime<Utc>>, TokenCounts)> = Vec::new();
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(n_threads);
            for _ in 0..n_threads {
                let worker_pricing = pricing.clone();
                handles.push(scope.spawn(move || {
                    let tok_finder = memmem::Finder::new(b"_tokens");
                    let mut buf: Vec<u8> = Vec::with_capacity(4096);
                    let mut local = Vec::new();
                    loop {
                        let task = {
                            let mut q = queue_ref.lock().unwrap();
                            q.pop()
                        };
                        let Some(t) = task else { break };
                        let (total_turns, last_ts, counts) = scan_single_transcript_for_window(
                            t.fh,
                            &tok_finder,
                            &mut buf,
                            start_utc,
                            end_utc,
                            target_local_date,
                            &worker_pricing,
                        );
                        local.push((t.cid, total_turns, last_ts, counts));
                    }
                    local
                }));
            }
            for h in handles {
                if let Ok(local) = h.join() {
                    all_res.extend(local);
                }
            }
        });
        for (cid, total_turns, last_ts, counts) in all_res {
            merge_result(cid, total_turns, last_ts, counts);
        }
    } else {
        let tok_finder = memmem::Finder::new(b"_tokens");
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        for t in tasks {
            let (total_turns, last_ts, counts) = scan_single_transcript_for_window(
                t.fh,
                &tok_finder,
                &mut buf,
                start_utc,
                end_utc,
                target_local_date,
                pricing,
            );
            merge_result(t.cid, total_turns, last_ts, counts);
        }
    }

    let mut total = TokenCounts::default();
    for (_, _, c) in by_cid.into_values() {
        total.merge(&c);
    }
    total
}

fn scan_single_transcript_for_window(
    fh: File,
    tok_finder: &memmem::Finder<'_>,
    buf: &mut Vec<u8>,
    start_utc: DateTime<Utc>,
    end_utc: DateTime<Utc>,
    target_local_date: chrono::NaiveDate,
    pricing: &PricingCatalog,
) -> (usize, Option<DateTime<Utc>>, TokenCounts) {
    let mut reader = BufReader::with_capacity(64 * 1024, fh);
    let mut by_step: BTreeMap<i64, TodayTurnLite> = BTreeMap::new();
    let mut fallback_step: i64 = -1;
    let mut last_ts: Option<DateTime<Utc>> = None;
    let mut has_any_today = false;

    loop {
        buf.clear();
        let Ok(n) = reader.read_until(b'\n', buf) else {
            break;
        };
        if n == 0 {
            break;
        }
        let trimmed_bytes = trim_ascii_ws(buf);
        if trimmed_bytes.is_empty() {
            continue;
        }
        // Check GENERIC header on leading <=320 bytes BEFORE scanning multi-KB payloads for `_tokens`!
        if parse_generic_step_header(header_str_prefix(trimmed_bytes)).is_some() {
            continue;
        }
        if tok_finder.find(trimmed_bytes).is_none() {
            continue;
        }
        let line_cow = bytes_to_str_lossy(trimmed_bytes);

        let seed = AgyLineSeed {
            capture_content: false,
            capture_tools: false,
            capture_thinking: false,
        };
        let mut de = serde_json::Deserializer::from_str(&line_cow);
        let Ok(parsed) = de::DeserializeSeed::deserialize(seed, &mut de) else {
            continue;
        };
        if !parsed.has_tokens {
            continue;
        }
        let cw = parsed
            .cache_write_tokens
            .or(parsed.cache_creation_tokens)
            .or(parsed.cache_creation_input_tokens)
            .unwrap_or(0);
        if parsed.input_tokens + parsed.cache_read_tokens + cw + parsed.output_tokens == 0 {
            continue;
        }
        let is_checkpoint = parsed.step_type.as_deref() == Some("CHECKPOINT");
        let model = parsed.model.unwrap_or_else(|| {
            intern_str(if is_checkpoint {
                COMPACTION_MODEL_LABEL
            } else {
                "unknown"
            })
        });
        let step_index = match parsed.step_index {
            Some(s) => s,
            None => {
                let s = fallback_step;
                fallback_step -= 1;
                s
            }
        };
        let is_today = parsed
            .created_at
            .map(|ts| ts >= start_utc && ts < end_utc)
            .unwrap_or(false);
        if is_today {
            has_any_today = true;
        }
        if let Some(ts) = parsed.created_at {
            last_ts = Some(match last_ts {
                Some(prev) if prev >= ts => prev,
                _ => ts,
            });
        }
        by_step.insert(
            step_index,
            TodayTurnLite {
                model,
                is_checkpoint,
                input_tokens: parsed.input_tokens,
                cache_read_tokens: parsed.cache_read_tokens,
                cache_write_tokens: cw,
                output_tokens: parsed.output_tokens,
                is_today,
            },
        );
    }

    let total_turns = by_step.len();
    if !has_any_today {
        return (total_turns, last_ts, TokenCounts::default());
    }

    // Compute fallback dominant model only if any today turn is a checkpoint/unknown.
    let need_fallback = by_step
        .values()
        .any(|t| t.is_today && (t.is_checkpoint || is_compaction_model(&t.model) || &*t.model == "unknown"));
    let fallback_model: Option<Arc<str>> = if need_fallback {
        let mut by_m: HashMap<&str, u64> = HashMap::new();
        for t in by_step.values() {
            if t.is_checkpoint || is_compaction_model(&t.model) || &*t.model == "unknown" {
                continue;
            }
            let tot = t.input_tokens + t.cache_read_tokens + t.cache_write_tokens + t.output_tokens;
            *by_m.entry(&t.model).or_insert(0) += tot;
        }
        by_m.into_iter()
            .max_by_key(|&(_, tok)| tok)
            .map(|(m, _)| intern_str(m))
    } else {
        None
    };

    let mut counts = TokenCounts::default();
    let mut last_real: Option<Arc<str>> = None;
    for t in by_step.values() {
        let eff_model: &str =
            if !t.is_checkpoint && !is_compaction_model(&t.model) && &*t.model != "unknown" {
                last_real = Some(Arc::clone(&t.model));
                &t.model
            } else if !is_compaction_model(&t.model) && &*t.model != "unknown" {
                &t.model
            } else if let Some(prev) = last_real.as_deref() {
                prev
            } else {
                fallback_model.as_deref().unwrap_or(&t.model)
            };

        if t.is_today {
            let cost = pricing.price_turn(
                eff_model,
                t.input_tokens,
                t.cache_read_tokens,
                t.cache_write_tokens,
                t.output_tokens,
                Some(target_local_date),
            );
            counts.add_call_with_cost(
                t.input_tokens,
                t.cache_read_tokens,
                t.cache_write_tokens,
                t.output_tokens,
                cost,
            );
        }
    }
    (total_turns, last_ts, counts)
}

fn parse_step_status_running(line: &str) -> Option<bool> {
    if !line.starts_with('{') {
        return None;
    }
    let mut end = line.len().min(256);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let mut head = &line[..end];
    for stop_key in ["\"content\"", "\"thinking\"", "\"tool_calls\"", "\"error\""] {
        if let Some(pos) = head.find(stop_key) {
            head = &head[..pos];
        }
    }
    let key = "\"status\"";
    let key_pos = head.find(key)?;
    let rest = head[key_pos + key.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let close = rest.find('"')?;
    let val = &rest[..close];
    if val.is_empty() {
        return None;
    }
    Some(val == "RUNNING")
}

/// Zero-allocation header parser for Antigravity `GENERIC` tool steps.
fn parse_generic_step_header(line: &str) -> Option<(i64, DateTime<Utc>)> {
    let mut end = line.len().min(256);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let head = &line[..end];
    let head = match head.find("\"content\"") {
        Some(p) => &head[..p],
        None => head,
    };
    const PREFIX: &str = "{\"step_index\":";
    if !head.starts_with(PREFIX) || !head.contains("\"type\":\"GENERIC\"") {
        return None;
    }
    let after = &head[PREFIX.len()..];
    let num_len = after
        .as_bytes()
        .iter()
        .take_while(|&&b| b.is_ascii_digit() || b == b'-')
        .count();
    let step = after[..num_len].parse::<i64>().ok()?;
    let key = "\"created_at\":\"";
    let start = head.find(key)? + key.len();
    let rest = &head[start..];
    let ts_str = &rest[..rest.find('"')?];
    let ts = parse_rfc3339_utc(ts_str)?;
    Some((step, ts))
}

fn estimate_gen_secs(t: &TurnRecord, generic_ts: &HashMap<i64, DateTime<Utc>>) -> Option<u32> {
    if t.is_checkpoint || t.tool_names.is_empty() || t.step_index < 0 {
        return None;
    }
    let start = t.timestamp_utc?;
    let mut end = start;
    for k in 1..=t.tool_names.len() as i64 {
        let ts = *generic_ts.get(&(t.step_index + k))?;
        end = end.max(ts);
    }
    let secs = (end - start).num_seconds();
    (0..=3600).contains(&secs).then_some(secs as u32)
}

struct SubagentMeta {
    parent_id: Option<String>,
    preview: String,
    is_swarm: bool,
    is_swarm_ping: bool,
}

fn extract_swarm_role(header_line: &str) -> Option<&str> {
    let pos = header_line.find(" for @")?;
    let rest = &header_line[pos + " for ".len()..];
    let end = rest
        .find(|c: char| matches!(c, ')' | ':' | ',') || c.is_whitespace())
        .unwrap_or(rest.len());
    let role = &rest[..end];
    (role.len() > 1).then_some(role)
}

fn extract_subagent_meta(content: &str) -> SubagentMeta {
    let mut parent_id = None;
    if let Some(pos) = content.find("sender=") {
        let rest = &content[pos + "sender=".len()..];
        let token = rest.split_whitespace().next().unwrap_or("");
        if is_uuid_like(token) {
            parent_id = Some(token.to_string());
        }
    }
    let mut preview = String::new();
    let mut is_swarm = false;
    let mut is_swarm_ping = false;
    if let Some(pos) = content.find("content=") {
        let body = &content[pos + "content=".len()..];
        let mut lines: Vec<&str> = Vec::new();
        for raw_line in body.lines() {
            let mut trimmed = raw_line.trim();
            if trimmed.is_empty() || trimmed.starts_with("</SYSTEM_MESSAGE>") {
                continue;
            }
            if let Some(stripped) = trimmed.strip_suffix("</SYSTEM_MESSAGE>") {
                trimmed = stripped.trim();
                if trimmed.is_empty() {
                    continue;
                }
            }
            lines.push(trimmed);
        }
        if let Some(&first_raw) = lines.first() {
            if first_raw.starts_with("[swarm] new board message") {
                is_swarm = true;
                is_swarm_ping = true;
                preview = clean_single_line(first_raw, 72);
            } else if first_raw.starts_with("You have just started. Your assignment") {
                is_swarm = true;
                let role = extract_swarm_role(first_raw);
                let mut brief = String::new();
                for candidate in lines.iter().skip(1) {
                    let cleaned = candidate
                        .trim_start_matches('#')
                        .trim()
                        .replace("**", "")
                        .replace('`', "");
                    if !cleaned.is_empty() {
                        brief = cleaned;
                        break;
                    }
                }
                preview = match (role, brief.is_empty()) {
                    (Some(r), false) => clean_single_line(&format!("{r} · {brief}"), 72),
                    (Some(r), true) => clean_single_line(r, 72),
                    (None, false) => clean_single_line(&brief, 72),
                    (None, true) => clean_single_line(first_raw, 72),
                };
            } else {
                for candidate in lines {
                    let cleaned_md = candidate
                        .trim_start_matches('#')
                        .trim()
                        .replace("**", "")
                        .replace('`', "");
                    preview = clean_single_line(&cleaned_md, 72);
                    if !preview.is_empty() {
                        break;
                    }
                }
            }
        }
    }
    SubagentMeta {
        parent_id,
        preview,
        is_swarm,
        is_swarm_ping,
    }
}

fn extract_user_preview(content: &str) -> String {
    let snippet = if let (Some(a), Some(b)) =
        (content.find("<USER_REQUEST>"), content.find("</USER_REQUEST>"))
    {
        let start = a + "<USER_REQUEST>".len();
        if start < b {
            &content[start..b]
        } else {
            content
        }
    } else {
        content
    };
    clean_single_line(snippet, 72)
}

/// Zero-DOM streaming parser for `cache/conversation_metadata.json`.
fn parse_metadata_json_reader(fh: File) -> HashMap<String, CachedMetaEntry> {
    let mut out = HashMap::new();
    let reader = BufReader::with_capacity(32 * 1024, fh);
    let mut de = serde_json::Deserializer::from_reader(reader);
    let _ = de::DeserializeSeed::deserialize(MetaRootSeed { out: &mut out }, &mut de);
    out
}

struct MetaRootSeed<'a> {
    out: &'a mut HashMap<String, CachedMetaEntry>,
}

impl<'de, 'a> de::DeserializeSeed<'de> for MetaRootSeed<'a> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V<'a> {
            out: &'a mut HashMap<String, CachedMetaEntry>,
        }
        impl<'de, 'a> Visitor<'de> for V<'a> {
            type Value = ();
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("conversation_metadata root")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while let Some(k) = map.next_key::<String>()? {
                    if k == "conversations" {
                        map.next_value_seed(MetaConvsSeed { out: self.out })?;
                    } else {
                        let _: IgnoredAny = map.next_value()?;
                    }
                }
                Ok(())
            }
        }
        deserializer.deserialize_any(V { out: self.out })
    }
}

struct MetaConvsSeed<'a> {
    out: &'a mut HashMap<String, CachedMetaEntry>,
}

impl<'de, 'a> de::DeserializeSeed<'de> for MetaConvsSeed<'a> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V<'a> {
            out: &'a mut HashMap<String, CachedMetaEntry>,
        }
        impl<'de, 'a> Visitor<'de> for V<'a> {
            type Value = ();
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("conversations map")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while let Some(cid) = map.next_key::<String>()? {
                    if let Some(entry) = map.next_value_seed(MetaEntrySeed)? {
                        self.out.insert(cid, entry);
                    }
                }
                Ok(())
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(())
            }
        }
        deserializer.deserialize_any(V { out: self.out })
    }
}

struct MetaEntrySeed;

impl<'de> de::DeserializeSeed<'de> for MetaEntrySeed {
    type Value = Option<CachedMetaEntry>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<CachedMetaEntry>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("conversation metadata entry")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut is_internal = false;
                let mut summary: Option<(String, String, String)> = None;
                while let Some(k) = map.next_key::<String>()? {
                    match k.as_str() {
                        "is_internal" => {
                            if let OptFlexBool(Some(b)) = map.next_value()? {
                                is_internal = b;
                            }
                        }
                        "summary" => {
                            summary = map.next_value_seed(MetaSummarySeed)?;
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                if !is_internal && summary.is_none() {
                    return Ok(None);
                }
                let (title, preview, workspace) = summary.unwrap_or_default();
                Ok(Some(CachedMetaEntry {
                    title,
                    preview,
                    workspace,
                    is_internal,
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

struct MetaSummarySeed;

impl<'de> de::DeserializeSeed<'de> for MetaSummarySeed {
    type Value = Option<(String, String, String)>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<(String, String, String)>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("conversation summary object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut title = String::new();
                let mut preview = String::new();
                let mut workspace = String::new();
                while let Some(k) = map.next_key::<String>()? {
                    match k.as_str() {
                        "Title" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                title = clean_single_line(&s, 100);
                            }
                        }
                        "Preview" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                preview = clean_single_line(&s, 100);
                            }
                        }
                        "WorkspaceURIs" => {
                            if let Some(first) = map.next_value_seed(FirstStringInSeqSeed)? {
                                workspace = decode_file_uri(&first);
                            }
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(Some((title, preview, workspace)))
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

struct FirstStringInSeqSeed;

impl<'de> de::DeserializeSeed<'de> for FirstStringInSeqSeed {
    type Value = Option<String>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<String>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("string array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let first = match seq.next_element::<OptFlexString>()? {
                    Some(OptFlexString(s)) => s,
                    None => None,
                };
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(first)
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

pub fn parse_pbtxt_title(text: &str) -> String {
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("title") else { continue };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix(':') else { continue };
        let rest = rest.trim_start();
        if let Some(quoted) = rest.strip_prefix('"') {
            let mut out = Vec::new();
            let b = quoted.as_bytes();
            let mut i = 0;
            while i < b.len() {
                match b[i] {
                    b'"' => break,
                    b'\\' if i + 1 < b.len() => {
                        i += 1;
                        match b[i] {
                            b'n' => {
                                out.push(b' ');
                                i += 1;
                            }
                            b'r' | b't' => {
                                out.push(b' ');
                                i += 1;
                            }
                            b'"' => {
                                out.push(b'"');
                                i += 1;
                            }
                            b'\\' => {
                                out.push(b'\\');
                                i += 1;
                            }
                            b'0'..=b'7' => {
                                let mut val: u16 = 0;
                                let mut cnt = 0;
                                while i < b.len() && cnt < 3 && (b'0'..=b'7').contains(&b[i]) {
                                    val = (val << 3) | (b[i] - b'0') as u16;
                                    i += 1;
                                    cnt += 1;
                                }
                                out.push((val & 0xFF) as u8);
                            }
                            b'x' if i + 2 < b.len() => {
                                if let (Some(h1), Some(h2)) = (hex_val(b[i + 1]), hex_val(b[i + 2]))
                                {
                                    out.push((h1 << 4) | h2);
                                    i += 3;
                                } else {
                                    i += 1;
                                }
                            }
                            other => {
                                out.push(other);
                                i += 1;
                            }
                        }
                    }
                    other => {
                        out.push(other);
                        i += 1;
                    }
                }
            }
            return clean_single_line(&String::from_utf8_lossy(&out), 100);
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn test_generic_header_and_step_status_spoof_resistance() {
        assert!(parse_generic_step_header(
            r#"{"step_index":3,"type":"USER_INPUT","created_at":"2026-11-22T01:00:10Z"}"#
        )
        .is_none());
        assert!(parse_generic_step_header(
            r#"{"step_index":3,"source":"MODEL","content":"\"type\":\"GENERIC\",\"created_at\":\"2026-11-22T01:00:10Z\""}"#
        )
        .is_none());

        assert_eq!(
            parse_step_status_running(
                r#"{"step_index":2,"source":"MODEL","type":"GENERIC","status":"RUNNING","created_at":"2026-11-24T01:00:00Z","content":"ok"}"#
            ),
            Some(true)
        );
        assert_eq!(
            parse_step_status_running(
                r#"{"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "content": "status\":\"RUNNING\""}"#
            ),
            Some(false)
        );
        assert_eq!(
            parse_step_status_running(
                r#"{"step_index":3,"type":"GENERIC","content":"{\"status\":\"RUNNING\"}"}"#
            ),
            None
        );
        assert_eq!(
            parse_step_status_running(
                r#"{"step_index":3,"thinking":"check \"status\":\"RUNNING\"","status":"DONE"}"#
            ),
            None
        );
        assert_eq!(parse_step_status_running("not json"), None);
    }

    #[test]
    fn test_byte_offset_tailing_partial_line_and_truncation_fallback() {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "agy_gauge_tail_test_{}_{}",
            std::process::id(),
            seq
        ));
        let _ = fs::remove_dir_all(&base);
        let logs = base
            .join("brain")
            .join("tail-sess")
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&logs).unwrap();
        let t_path = logs.join("transcript.jsonl");

        // Write turn 1 + partial truncated line without newline (live writer mid-flush)
        let line1 = r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-25T10:00:00Z","model":"gemini-4-neon-xhigh","input_tokens":1000,"cache_read_tokens":2000,"output_tokens":300,"thinking":"plan","tool_calls":[{"name":"read_file","args":{"big":"payload"}}]}"#;
        let partial = r#"{"step_index":2,"type":"GENER"#;
        fs::write(&t_path, format!("{line1}\n{partial}")).unwrap();

        let mut adapter = AntigravityAdapter::new(SourceKind::Cli, base.clone());
        let r1 = adapter.scan(Utc::now());
        assert_eq!(r1.files_reparsed, 1);
        assert_eq!(r1.skipped_lines, 1);
        assert_eq!(r1.sessions.len(), 1);
        assert_eq!(r1.sessions[0].turns.len(), 1);
        assert_eq!(r1.sessions[0].turns[0].gen_secs, None);
        assert_eq!(r1.sessions[0].turns[0].thinking_chars, 4);

        // Warm poll with no change returns AdapterPoll::Unchanged!
        assert!(matches!(adapter.poll_changes(Utc::now()), AdapterPoll::Unchanged { .. }));

        // Finish flushing step 2 (GENERIC tool timestamp -> resolves gen_secs = 8s!) + append step 3 turn
        let rest_of_line2 = r#"IC","status":"DONE","created_at":"2026-11-25T10:00:08Z","content":"_tokens mentioned in file"}"#;
        let line3 = r#"{"step_index":3,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-25T10:00:20Z","model":"gemini-4-neon-xhigh","input_tokens":500,"cache_read_tokens":3000,"output_tokens":200}"#;
        {
            let mut f = fs::OpenOptions::new().append(true).open(&t_path).unwrap();
            writeln!(f, "{rest_of_line2}").unwrap();
            writeln!(f, "{line3}").unwrap();
        }
        let r2 = match adapter.poll_changes(Utc::now()) {
            AdapterPoll::Changed(res) => res,
            _ => panic!("expected Changed after append"),
        };
        assert_eq!(r2.files_reparsed, 1);
        assert_eq!(r2.skipped_lines, 0); // partial line resolved cleanly!
        assert_eq!(r2.sessions[0].turns.len(), 2);
        assert_eq!(r2.sessions[0].turns[0].gen_secs, Some(8));
        assert_eq!(r2.sessions[0].turns[1].input_tokens, 500);

        // Truncate/rewrite file with completely different smaller content -> fallback to byte 0!
        let rewritten = r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-25T11:00:00Z","model":"claude-opus-5-5","input_tokens":77,"cache_read_tokens":0,"output_tokens":33}"#;
        fs::write(&t_path, format!("{rewritten}\n")).unwrap();
        let r3 = adapter.scan(Utc::now());
        assert_eq!(r3.files_reparsed, 1);
        assert_eq!(r3.sessions[0].turns.len(), 1);
        assert_eq!(&*r3.sessions[0].turns[0].model, "claude-opus-5-5");
        assert_eq!(r3.sessions[0].turns[0].input_tokens, 77);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_parallel_cold_and_today_scan_parity() {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "agy_gauge_parallel_parity_{}_{}",
            std::process::id(),
            seq
        ));
        let _ = fs::remove_dir_all(&base);
        let pad = "x".repeat(350_000);
        let now_ts = Utc::now().to_rfc3339();
        let today = Local::now().date_naive();

        // Create 4 sessions (~1.4 MiB total > PARALLEL_MIN_BYTES) so parallel `std::thread::scope`
        // runs on both `AntigravityAdapter::scan()` and `scan_antigravity_date_totals()`.
        for i in 0..4u64 {
            let cid = format!("par-sess-{i}");
            let logs = base
                .join("brain")
                .join(&cid)
                .join(".system_generated")
                .join("logs");
            fs::create_dir_all(&logs).unwrap();
            let line1 = format!(
                r#"{{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"{now_ts}","model":"gemini-4-neon-xhigh","input_tokens":{},"cache_read_tokens":2000,"output_tokens":300,"tool_calls":[{{"name":"read_file","args":{{"blob":"{pad}"}}}}]}}"#,
                1000 + i * 100
            );
            let line2 = format!(
                r#"{{"step_index":2,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"{now_ts}","content":"_tokens {pad}"}}"#
            );
            fs::write(logs.join("transcript.jsonl"), format!("{line1}\n{line2}\n")).unwrap();
        }

        let mut adapter = AntigravityAdapter::new(SourceKind::Cli, base.clone());
        let res = adapter.scan(Utc::now());
        assert_eq!(res.files_reparsed, 4);
        assert_eq!(res.sessions.len(), 4);
        let sum_in: u64 = res
            .sessions
            .iter()
            .flat_map(|s| s.turns.iter())
            .map(|t| t.input_tokens)
            .sum();
        assert_eq!(sum_in, 1000 + 1100 + 1200 + 1300);

        let pricing = PricingCatalog::new(None);
        let today_tc = scan_antigravity_date_totals(&[(SourceKind::Cli, base.clone())], &pricing, today);
        assert_eq!(today_tc.calls, 4);
        assert_eq!(today_tc.input_tokens, sum_in);
        assert_eq!(today_tc.cache_read_tokens, 8000);
        assert_eq!(today_tc.output_tokens, 1200);

        let _ = fs::remove_dir_all(&base);
    }
}
