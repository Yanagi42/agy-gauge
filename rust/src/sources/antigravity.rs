//! Strictly read-only incremental adapter for Antigravity (`agy` CLI / 2.0 App / IDE).
//!
//! Only opens whitelisted plain-text files with read-only handles:
//!   - `<data_dir>/brain/<id>/.system_generated/logs/transcript.jsonl`
//!     (fallback to `transcript_full.jsonl` only if absent)
//!   - `<data_dir>/annotations/<id>.pbtxt`
//!   - `<data_dir>/cache/conversation_metadata.json`
//!
//! Never writes to disk, never touches SQLite databases (`conversations/*.db`), never reads credentials.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use crate::pricing::CostBreakdown;
use crate::sources::{
    clean_single_line, compute_running_hint, decode_file_uri, hex_val, is_uuid_like,
    parse_rfc3339_utc, val_to_u64, FileSig, ParsedSession, SafeFs, SourceAdapter,
    SourceScanResult,
};
use crate::stats::{SourceKind, TurnRecord, COMPACTION_MODEL_LABEL};

#[derive(Debug, Clone)]
struct CachedTranscript {
    sig: FileSig,
    turns: Vec<TurnRecord>,
    first_user_preview: String,
    subagent_parent_id: Option<String>,
    subagent_preview: String,
    is_swarm_subagent: bool,
    last_step_running: bool,
    skipped_lines: usize,
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
    ann_cache: HashMap<PathBuf, (FileSig, String)>,
    meta_sig: Option<FileSig>,
    meta_cache: HashMap<String, CachedMetaEntry>,
}

impl AntigravityAdapter {
    pub fn new(kind: SourceKind, root: PathBuf) -> Self {
        Self {
            kind,
            safe_fs: SafeFs::new(root),
            transcript_cache: HashMap::new(),
            ann_cache: HashMap::new(),
            meta_sig: None,
            meta_cache: HashMap::new(),
        }
    }

    fn refresh_metadata_cache(&mut self) {
        let path = self
            .safe_fs
            .root()
            .join("cache")
            .join("conversation_metadata.json");
        let Some(sig) = self.safe_fs.is_safe_regular_file(&path) else {
            self.meta_sig = None;
            self.meta_cache.clear();
            return;
        };
        if self.meta_sig == Some(sig) {
            return;
        }
        if let Ok(mut fh) = File::open(&path) {
            let mut text = String::new();
            if fh.read_to_string(&mut text).is_ok() {
                self.meta_cache = parse_metadata_json(&text);
                self.meta_sig = Some(sig);
            }
        }
    }

    fn get_annotation_title(&mut self, cid: &str) -> String {
        let path = self
            .safe_fs
            .root()
            .join("annotations")
            .join(format!("{cid}.pbtxt"));
        let Some(sig) = self.safe_fs.is_safe_regular_file(&path) else {
            self.ann_cache.remove(&path);
            return String::new();
        };
        if let Some((cached_sig, title)) = self.ann_cache.get(&path) {
            if *cached_sig == sig {
                return title.clone();
            }
        }
        let mut title = String::new();
        if let Ok(mut fh) = File::open(&path) {
            let mut text = String::new();
            if fh.read_to_string(&mut text).is_ok() {
                title = parse_pbtxt_title(&text);
            }
        }
        self.ann_cache.insert(path, (sig, title.clone()));
        title
    }
}

impl SourceAdapter for AntigravityAdapter {
    fn kind(&self) -> SourceKind {
        self.kind
    }

    fn root(&self) -> &Path {
        self.safe_fs.root()
    }

    fn lookup_session_hint(&mut self, cid: &str) -> Option<(String, String)> {
        let p_ann = self.get_annotation_title(cid);
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
        self.safe_fs.refresh_root();
        let mut res = SourceScanResult::default();
        let brain_dir = self.safe_fs.root().join("brain");
        if !self.safe_fs.is_safe_dir(&brain_dir) {
            self.transcript_cache.clear();
            return res;
        }
        res.found_root = true;
        self.refresh_metadata_cache();

        let Ok(entries) = fs::read_dir(&brain_dir) else {
            return res;
        };
        let mut conv_dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                fs::symlink_metadata(p)
                    .map(|m| !m.file_type().is_symlink() && m.is_dir())
                    .unwrap_or(false)
            })
            .collect();
        conv_dirs.sort();

        let mut seen_paths = BTreeSet::new();

        for conv_dir in conv_dirs {
            let Some(cid) = conv_dir
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
            else {
                continue;
            };
            let logs_dir = conv_dir.join(".system_generated").join("logs");
            let primary = logs_dir.join("transcript.jsonl");
            let fallback = logs_dir.join("transcript_full.jsonl");

            let (t_path, sig) = if let Some(s) = self.safe_fs.is_safe_regular_file(&primary) {
                (primary, s)
            } else if let Some(s) = self.safe_fs.is_safe_regular_file(&fallback) {
                (fallback, s)
            } else {
                res.skipped_empty += 1;
                continue;
            };

            seen_paths.insert(t_path.clone());

            let cached = if let Some(entry) = self.transcript_cache.get(&t_path) {
                if entry.sig == sig {
                    res.files_cached += 1;
                    entry.clone()
                } else {
                    match parse_transcript_file(&t_path, sig) {
                        Some(fresh) => {
                            res.files_reparsed += 1;
                            self.transcript_cache.insert(t_path.clone(), fresh.clone());
                            fresh
                        }
                        None => continue,
                    }
                }
            } else {
                match parse_transcript_file(&t_path, sig) {
                    Some(fresh) => {
                        res.files_reparsed += 1;
                        self.transcript_cache.insert(t_path.clone(), fresh.clone());
                        fresh
                    }
                    None => continue,
                }
            };

            res.skipped_lines += cached.skipped_lines;
            if cached.turns.is_empty() {
                res.skipped_empty += 1;
                continue;
            }

            let ann_title = self.get_annotation_title(&cid);
            let meta = self.meta_cache.get(&cid);
            let is_subagent = match meta {
                Some(m) if m.is_internal => true,
                Some(_) => false,
                None => cached.subagent_parent_id.is_some() || cached.is_swarm_subagent,
            };
            let parent_id = if is_subagent {
                cached.subagent_parent_id.clone().filter(|p| p != &cid)
            } else {
                None
            };
            let title = if !ann_title.is_empty() {
                ann_title
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

            let last_file_mtime_utc = sig.mtime_utc();
            let is_running_hint =
                compute_running_hint(cached.last_step_running, last_file_mtime_utc, scan_now_utc);

            res.sessions.push(ParsedSession {
                conversation_id: cid,
                title,
                workspace,
                source: self.kind,
                is_subagent,
                parent_id,
                parent_title: String::new(),
                turns: cached.turns,
                last_file_mtime_utc,
                is_running_hint,
            });
        }

        self.transcript_cache.retain(|k, _| seen_paths.contains(k));
        res
    }
}

fn parse_transcript_file(path: &Path, sig: FileSig) -> Option<CachedTranscript> {
    let fh = File::open(path).ok()?;
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
    let mut skipped_lines = 0usize;
    let mut generic_ts: HashMap<i64, DateTime<Utc>> = HashMap::new();
    let mut line = String::new();

    loop {
        line.clear();
        let Ok(n) = reader.read_line(&mut line) else { break };
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((step, ts)) = parse_generic_step_header(trimmed) {
            generic_ts.insert(step, ts);
        }
        if let Some(running) = parse_step_status_running(trimmed) {
            last_step_running = running;
        }
        let need_preview = !seen_user_input && trimmed.contains("USER_INPUT");
        let want_subagent_meta = !seen_user_input
            && by_step.is_empty()
            && ((subagent_parent_id.is_none() && subagent_preview.is_empty())
                || subagent_meta_is_swarm_ping);
        let need_subagent_meta = want_subagent_meta && trimmed.contains("SYSTEM_MESSAGE");
        if trimmed.starts_with('{')
            && trimmed.ends_with('}')
            && !need_preview
            && !need_subagent_meta
            && !trimmed.contains("_tokens")
        {
            continue;
        }
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) else {
            skipped_lines += 1;
            continue;
        };

        let step_type = map.get("type").and_then(Value::as_str).unwrap_or("");
        if step_type == "USER_INPUT" {
            seen_user_input = true;
            if first_user_preview.is_empty() {
                if let Some(content) = map.get("content").and_then(Value::as_str) {
                    first_user_preview = extract_user_preview(content);
                }
            }
        } else if want_subagent_meta && step_type == "SYSTEM_MESSAGE" {
            if let Some(content) = map.get("content").and_then(Value::as_str) {
                let meta = extract_subagent_meta(content);
                if meta.is_swarm {
                    is_swarm_subagent = true;
                }
                if subagent_meta_is_swarm_ping {
                    if meta.is_swarm_ping {
                        if subagent_parent_id.is_none() && meta.parent_id.is_some() {
                            subagent_parent_id = meta.parent_id;
                        }
                    } else {
                        if meta.parent_id.is_some() {
                            subagent_parent_id = meta.parent_id;
                        }
                        if !meta.preview.is_empty() {
                            subagent_preview = meta.preview;
                        }
                        subagent_meta_is_swarm_ping = false;
                    }
                } else {
                    subagent_parent_id = meta.parent_id;
                    subagent_preview = meta.preview;
                    subagent_meta_is_swarm_ping = meta.is_swarm_ping;
                }
            }
        }

        let has_tokens = [
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "cache_creation_tokens",
            "cache_creation_input_tokens",
        ]
        .iter()
        .any(|k| map.get(*k).map(|v| !v.is_null()).unwrap_or(false));

        if !has_tokens {
            continue;
        }

        let input_tokens = val_to_u64(map.get("input_tokens"));
        let cache_read_tokens = val_to_u64(map.get("cache_read_tokens"));
        let cache_write_tokens = val_to_u64(
            map.get("cache_write_tokens")
                .or_else(|| map.get("cache_creation_tokens"))
                .or_else(|| map.get("cache_creation_input_tokens")),
        );
        let output_tokens = val_to_u64(map.get("output_tokens"));
        if input_tokens + cache_read_tokens + cache_write_tokens + output_tokens == 0 {
            continue;
        }

        let timestamp_utc = map
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_utc);

        let is_checkpoint = step_type == "CHECKPOINT";
        let model = map
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(if is_checkpoint { COMPACTION_MODEL_LABEL } else { "unknown" })
            .to_string();

        let step_index = match map.get("step_index").and_then(Value::as_i64) {
            Some(s) => s,
            None => {
                let s = fallback_step;
                fallback_step -= 1;
                s
            }
        };

        let tool_names: Vec<String> = map
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|arr| {
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
            })
            .unwrap_or_default();
        let thinking_chars = map
            .get("thinking")
            .and_then(Value::as_str)
            .map(|s| s.chars().count() as u64)
            .unwrap_or(0);

        by_step.insert(
            step_index,
            TurnRecord {
                step_index,
                timestamp_utc,
                model,
                is_checkpoint,
                input_tokens,
                cache_read_tokens,
                cache_write_tokens,
                cache_write_1h_tokens: 0,
                output_tokens,
                reasoning_tokens: 0,
                tool_names,
                thinking_chars,
                gen_secs: None,
                cost: CostBreakdown::default(),
            },
        );
    }

    let mut turns: Vec<TurnRecord> = by_step.into_values().collect();
    turns.sort_by_key(|t| {
        (
            if t.step_index >= 0 { t.step_index } else { i64::MAX },
            t.timestamp_utc,
        )
    });
    for t in &mut turns {
        t.gen_secs = estimate_gen_secs(t, &generic_ts);
    }
    if seen_user_input {
        subagent_parent_id = None;
        subagent_preview.clear();
        is_swarm_subagent = false;
    }

    Some(CachedTranscript {
        sig,
        turns,
        first_user_preview,
        subagent_parent_id,
        subagent_preview,
        is_swarm_subagent,
        last_step_running,
        skipped_lines,
    })
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
    if !head.starts_with("{\"step_index\":") || !head.contains("\"type\":\"GENERIC\"") {
        return None;
    }
    let num: String = head["{\"step_index\":".len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    let step = num.parse::<i64>().ok()?;
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

fn parse_metadata_json(text: &str) -> HashMap<String, CachedMetaEntry> {
    let mut out = HashMap::new();
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(text) else {
        return out;
    };
    let Some(Value::Object(convs)) = root.get("conversations") else {
        return out;
    };
    for (cid, entry) in convs {
        let is_internal = entry
            .get("is_internal")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let summary = entry.get("summary").and_then(Value::as_object);
        let title = summary
            .and_then(|s| s.get("Title"))
            .and_then(Value::as_str)
            .map(|s| clean_single_line(s, 100))
            .unwrap_or_default();
        let preview = summary
            .and_then(|s| s.get("Preview"))
            .and_then(Value::as_str)
            .map(|s| clean_single_line(s, 100))
            .unwrap_or_default();
        let workspace = summary
            .and_then(|s| s.get("WorkspaceURIs"))
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(Value::as_str)
            .map(decode_file_uri)
            .unwrap_or_default();
        if !is_internal && summary.is_none() {
            continue;
        }
        out.insert(
            cid.clone(),
            CachedMetaEntry {
                title,
                preview,
                workspace,
                is_internal,
            },
        );
    }
    out
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
}
