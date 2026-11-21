//! Strictly read-only incremental scanner & aggregator for Antigravity CLI (`agy`) transcripts.
//!
//! Only opens whitelisted plain-text files with read-only handles:
//!   - `<data_dir>/brain/<id>/.system_generated/logs/transcript.jsonl`
//!     (fallback to `transcript_full.jsonl` only if absent)
//!   - `<data_dir>/annotations/<id>.pbtxt`
//!   - `<data_dir>/cache/conversation_metadata.json`
//!
//! Never writes to disk, never touches SQLite databases, never reads credentials.

use chrono::{DateTime, Datelike, Local, NaiveDate, Timelike, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCounts {
    pub calls: u64,
    pub input_tokens: u64,       // Uncached new input tokens
    pub cache_read_tokens: u64,  // Prompt cache read tokens
    pub cache_write_tokens: u64, // Prompt cache creation tokens (if reported)
    pub output_tokens: u64,      // Output tokens (thinking + response + tool calls)
    pub peak_context: u64,       // Max single-call total input tokens
}

impl TokenCounts {
    pub fn total_input(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    pub fn total_tokens(&self) -> u64 {
        self.total_input().saturating_add(self.output_tokens)
    }

    pub fn non_cached_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_write_tokens)
            .saturating_add(self.output_tokens)
    }

    pub fn cache_hit_rate(&self) -> f64 {
        let denom = self.total_input();
        if denom == 0 {
            0.0
        } else {
            (self.cache_read_tokens as f64 / denom as f64) * 100.0
        }
    }

    pub fn add_call(&mut self, input: u64, cache_read: u64, cache_write: u64, output: u64) {
        self.calls += 1;
        self.input_tokens += input;
        self.cache_read_tokens += cache_read;
        self.cache_write_tokens += cache_write;
        self.output_tokens += output;
        let ctx = input + cache_read + cache_write;
        if ctx > self.peak_context {
            self.peak_context = ctx;
        }
    }

    pub fn merge(&mut self, other: &TokenCounts) {
        self.calls += other.calls;
        self.input_tokens += other.input_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.output_tokens += other.output_tokens;
        if other.peak_context > self.peak_context {
            self.peak_context = other.peak_context;
        }
    }
}

#[derive(Debug, Clone)]
pub struct TurnRecord {
    pub step_index: i64,
    pub timestamp_utc: Option<DateTime<Utc>>,
    pub model: String,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
}

impl TurnRecord {
    pub fn total_input(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    pub fn cache_hit_rate(&self) -> f64 {
        let d = self.total_input();
        if d == 0 {
            0.0
        } else {
            (self.cache_read_tokens as f64 / d as f64) * 100.0
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub conversation_id: String,
    pub title: String,
    pub workspace: String,
    pub turns: Vec<TurnRecord>,
    pub counts: TokenCounts,
    pub models: BTreeSet<String>,
    pub first_time: Option<DateTime<Local>>,
    pub last_time: Option<DateTime<Local>>,
}

#[derive(Debug, Clone, Default)]
pub struct AggregatedStats {
    pub total: TokenCounts,
    pub by_day: BTreeMap<String, TokenCounts>,
    pub by_week: BTreeMap<String, TokenCounts>,
    pub week_active_days: HashMap<String, usize>,
    pub by_month: BTreeMap<String, TokenCounts>,
    pub by_hour: [TokenCounts; 24],
    pub by_model: BTreeMap<String, TokenCounts>,
    pub by_workspace: BTreeMap<String, (usize, TokenCounts)>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ScanSnapshot {
    pub data_dir: PathBuf,
    pub sessions: Vec<SessionInfo>,
    pub agg: AggregatedStats,
    pub skipped_empty: usize,
    pub skipped_lines: usize,
    pub files_cached: usize,
    pub files_reparsed: usize,
    pub scan_ms: f64,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateScope {
    Today,
    Days7,
    Days30,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSort {
    Recent,
    TotalTokens,
    PeakContext,
    OutputTokens,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSig {
    mtime_secs: u64,
    mtime_nanos: u32,
    len: u64,
}

#[derive(Debug, Clone)]
struct CachedTranscript {
    sig: FileSig,
    turns: Vec<TurnRecord>,
    first_user_preview: String,
    skipped_lines: usize,
}

#[derive(Debug, Clone)]
struct CachedMetaEntry {
    title: String,
    preview: String,
    workspace: String,
}

pub struct IncrementalScanner {
    data_dir: PathBuf,
    canon_root: Option<PathBuf>,
    transcript_cache: HashMap<PathBuf, CachedTranscript>,
    ann_cache: HashMap<PathBuf, (FileSig, String)>,
    meta_sig: Option<FileSig>,
    meta_cache: HashMap<String, CachedMetaEntry>,
}

impl IncrementalScanner {
    pub fn new(data_dir: PathBuf) -> Self {
        let canon_root = fs::canonicalize(&data_dir).ok();
        Self {
            data_dir,
            canon_root,
            transcript_cache: HashMap::new(),
            ann_cache: HashMap::new(),
            meta_sig: None,
            meta_cache: HashMap::new(),
        }
    }

    pub fn default_data_dir() -> PathBuf {
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            PathBuf::from(home).join(".gemini").join("antigravity-cli")
        } else {
            PathBuf::from(".gemini").join("antigravity-cli")
        }
    }

    #[allow(dead_code)]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn is_safe_regular_file(&self, path: &Path) -> Option<FileSig> {
        let meta = fs::symlink_metadata(path).ok()?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            return None;
        }
        if let Some(root) = &self.canon_root {
            let canon = fs::canonicalize(path).ok()?;
            if !canon.starts_with(root) {
                return None;
            }
        }
        let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        Some(FileSig {
            mtime_secs: mtime.as_secs(),
            mtime_nanos: mtime.subsec_nanos(),
            len: meta.len(),
        })
    }

    fn refresh_metadata_cache(&mut self) {
        let path = self.data_dir.join("cache").join("conversation_metadata.json");
        let Some(sig) = self.is_safe_regular_file(&path) else {
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
        let path = self.data_dir.join("annotations").join(format!("{cid}.pbtxt"));
        let Some(sig) = self.is_safe_regular_file(&path) else {
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

    pub fn scan(&mut self) -> ScanSnapshot {
        let started = Instant::now();
        if self.canon_root.is_none() {
            self.canon_root = fs::canonicalize(&self.data_dir).ok();
        }
        let brain_dir = self.data_dir.join("brain");
        if !brain_dir.is_dir() {
            return ScanSnapshot {
                data_dir: self.data_dir.clone(),
                sessions: Vec::new(),
                agg: AggregatedStats::default(),
                skipped_empty: 0,
                skipped_lines: 0,
                files_cached: 0,
                files_reparsed: 0,
                scan_ms: started.elapsed().as_secs_f64() * 1000.0,
                warning: Some(format!("未找到目录: {}", brain_dir.display())),
            };
        }

        self.refresh_metadata_cache();

        let mut sessions = Vec::new();
        let mut skipped_empty = 0usize;
        let mut skipped_lines = 0usize;
        let mut files_cached = 0usize;
        let mut files_reparsed = 0usize;
        let mut seen_paths = BTreeSet::new();

        let Ok(entries) = fs::read_dir(&brain_dir) else {
            return ScanSnapshot {
                data_dir: self.data_dir.clone(),
                sessions,
                agg: AggregatedStats::default(),
                skipped_empty,
                skipped_lines,
                files_cached,
                files_reparsed,
                scan_ms: started.elapsed().as_secs_f64() * 1000.0,
                warning: Some(format!("无法读取目录: {}", brain_dir.display())),
            };
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

        for conv_dir in conv_dirs {
            let Some(cid) = conv_dir.file_name().and_then(|s| s.to_str()).map(|s| s.to_string()) else {
                continue;
            };
            let logs_dir = conv_dir.join(".system_generated").join("logs");
            let primary = logs_dir.join("transcript.jsonl");
            let fallback = logs_dir.join("transcript_full.jsonl");

            let (t_path, sig) = if let Some(s) = self.is_safe_regular_file(&primary) {
                (primary, s)
            } else if let Some(s) = self.is_safe_regular_file(&fallback) {
                (fallback, s)
            } else {
                skipped_empty += 1;
                continue;
            };

            seen_paths.insert(t_path.clone());

            let cached = if let Some(entry) = self.transcript_cache.get(&t_path) {
                if entry.sig == sig {
                    files_cached += 1;
                    entry.clone()
                } else {
                    match parse_transcript_file(&t_path, sig) {
                        Some(fresh) => {
                            files_reparsed += 1;
                            self.transcript_cache.insert(t_path.clone(), fresh.clone());
                            fresh
                        }
                        None => continue,
                    }
                }
            } else {
                match parse_transcript_file(&t_path, sig) {
                    Some(fresh) => {
                        files_reparsed += 1;
                        self.transcript_cache.insert(t_path.clone(), fresh.clone());
                        fresh
                    }
                    None => continue,
                }
            };

            skipped_lines += cached.skipped_lines;
            if cached.turns.is_empty() {
                skipped_empty += 1;
                continue;
            }

            let ann_title = self.get_annotation_title(&cid);
            let meta = self.meta_cache.get(&cid);
            let title = if !ann_title.is_empty() {
                ann_title
            } else if let Some(m) = meta.filter(|m| !m.title.is_empty()) {
                m.title.clone()
            } else if let Some(m) = meta.filter(|m| !m.preview.is_empty()) {
                m.preview.clone()
            } else if !cached.first_user_preview.is_empty() {
                cached.first_user_preview.clone()
            } else {
                "(未命名会话)".to_string()
            };
            let workspace = meta
                .filter(|m| !m.workspace.is_empty())
                .map(|m| m.workspace.clone())
                .unwrap_or_else(|| "(未知工作区)".to_string());

            let mut counts = TokenCounts::default();
            let mut models = BTreeSet::new();
            let mut first_time: Option<DateTime<Local>> = None;
            let mut last_time: Option<DateTime<Local>> = None;

            for t in &cached.turns {
                counts.add_call(t.input_tokens, t.cache_read_tokens, t.cache_write_tokens, t.output_tokens);
                models.insert(t.model.clone());
                if let Some(ts_utc) = t.timestamp_utc {
                    let ts_local = ts_utc.with_timezone(&Local);
                    first_time = Some(match first_time {
                        Some(prev) if prev <= ts_local => prev,
                        _ => ts_local,
                    });
                    last_time = Some(match last_time {
                        Some(prev) if prev >= ts_local => prev,
                        _ => ts_local,
                    });
                }
            }

            sessions.push(SessionInfo {
                conversation_id: cid,
                title,
                workspace,
                turns: cached.turns,
                counts,
                models,
                first_time,
                last_time,
            });
        }

        self.transcript_cache.retain(|k, _| seen_paths.contains(k));
        sessions.sort_by(|a, b| b.last_time.cmp(&a.last_time));

        let agg = aggregate_sessions(&sessions);
        let warning = if skipped_lines > 0 {
            Some(format!("已跳过 {skipped_lines} 行未写完/异常 JSONL（活跃写入时属正常现象）"))
        } else {
            None
        };

        ScanSnapshot {
            data_dir: self.data_dir.clone(),
            sessions,
            agg,
            skipped_empty,
            skipped_lines,
            files_cached,
            files_reparsed,
            scan_ms: started.elapsed().as_secs_f64() * 1000.0,
            warning,
        }
    }
}

pub fn filter_snapshot(snapshot: &ScanSnapshot, scope: DateScope) -> (Vec<SessionInfo>, AggregatedStats) {
    let today = Local::now().date_naive();
    let since: Option<NaiveDate> = match scope {
        DateScope::Today => Some(today),
        DateScope::Days7 => today.checked_sub_days(chrono::Days::new(6)),
        DateScope::Days30 => today.checked_sub_days(chrono::Days::new(29)),
        DateScope::All => None,
    };

    if since.is_none() {
        return (snapshot.sessions.clone(), snapshot.agg.clone());
    }
    let min_date = since.unwrap();
    let mut filtered = Vec::new();
    for s in &snapshot.sessions {
        let mut turns = Vec::new();
        let mut counts = TokenCounts::default();
        let mut models = BTreeSet::new();
        let mut first_time: Option<DateTime<Local>> = None;
        let mut last_time: Option<DateTime<Local>> = None;

        for t in &s.turns {
            let Some(ts_utc) = t.timestamp_utc else { continue };
            let ts_local = ts_utc.with_timezone(&Local);
            if ts_local.date_naive() < min_date || ts_local.date_naive() > today {
                continue;
            }
            counts.add_call(t.input_tokens, t.cache_read_tokens, t.cache_write_tokens, t.output_tokens);
            models.insert(t.model.clone());
            first_time = Some(match first_time {
                Some(prev) if prev <= ts_local => prev,
                _ => ts_local,
            });
            last_time = Some(match last_time {
                Some(prev) if prev >= ts_local => prev,
                _ => ts_local,
            });
            turns.push(t.clone());
        }
        if !turns.is_empty() {
            filtered.push(SessionInfo {
                conversation_id: s.conversation_id.clone(),
                title: s.title.clone(),
                workspace: s.workspace.clone(),
                turns,
                counts,
                models,
                first_time,
                last_time,
            });
        }
    }
    filtered.sort_by(|a, b| b.last_time.cmp(&a.last_time));
    let agg = aggregate_sessions(&filtered);
    (filtered, agg)
}

pub fn aggregate_sessions(sessions: &[SessionInfo]) -> AggregatedStats {
    let mut agg = AggregatedStats::default();
    let mut week_day_sets: HashMap<String, BTreeSet<String>> = HashMap::new();

    for s in sessions {
        agg.total.merge(&s.counts);
        let ws = agg.by_workspace.entry(s.workspace.clone()).or_insert((0, TokenCounts::default()));
        ws.0 += 1;
        ws.1.merge(&s.counts);

        for t in &s.turns {
            agg.by_model
                .entry(t.model.clone())
                .or_default()
                .add_call(t.input_tokens, t.cache_read_tokens, t.cache_write_tokens, t.output_tokens);

            if let Some(ts_utc) = t.timestamp_utc {
                let ts = ts_utc.with_timezone(&Local);
                let d = ts.date_naive();
                let day_key = d.format("%Y-%m-%d").to_string();
                let wk_key = iso_week_key(d);
                let month_key = d.format("%Y-%m").to_string();
                let hour = (ts.hour() as usize).min(23);

                agg.by_day
                    .entry(day_key.clone())
                    .or_default()
                    .add_call(t.input_tokens, t.cache_read_tokens, t.cache_write_tokens, t.output_tokens);
                agg.by_week
                    .entry(wk_key.clone())
                    .or_default()
                    .add_call(t.input_tokens, t.cache_read_tokens, t.cache_write_tokens, t.output_tokens);
                week_day_sets.entry(wk_key).or_default().insert(day_key);
                agg.by_month
                    .entry(month_key)
                    .or_default()
                    .add_call(t.input_tokens, t.cache_read_tokens, t.cache_write_tokens, t.output_tokens);
                agg.by_hour[hour].add_call(
                    t.input_tokens,
                    t.cache_read_tokens,
                    t.cache_write_tokens,
                    t.output_tokens,
                );
            }
        }
    }

    for (wk, set) in week_day_sets {
        agg.week_active_days.insert(wk, set.len());
    }
    agg
}

pub fn iso_week_key(d: NaiveDate) -> String {
    let iso = d.iso_week();
    format!("{:04}-W{:02}", iso.year(), iso.week())
}

pub fn iso_week_range_label(week_key: &str) -> String {
    let Some((y_str, w_str)) = week_key.split_once("-W") else {
        return week_key.to_string();
    };
    let (Ok(year), Ok(week)) = (y_str.parse::<i32>(), w_str.parse::<u32>()) else {
        return week_key.to_string();
    };
    let mon = NaiveDate::from_isoywd_opt(year, week, chrono::Weekday::Mon);
    let sun = NaiveDate::from_isoywd_opt(year, week, chrono::Weekday::Sun);
    match (mon, sun) {
        (Some(m), Some(s)) => format!("{}/{} - {}/{}", m.month(), m.day(), s.month(), s.day()),
        _ => week_key.to_string(),
    }
}

pub fn weekday_cn(d: NaiveDate) -> &'static str {
    match d.weekday() {
        chrono::Weekday::Mon => "周一",
        chrono::Weekday::Tue => "周二",
        chrono::Weekday::Wed => "周三",
        chrono::Weekday::Thu => "周四",
        chrono::Weekday::Fri => "周五",
        chrono::Weekday::Sat => "周六",
        chrono::Weekday::Sun => "周日",
    }
}

pub fn fmt_compact(v: u64) -> String {
    if v >= 1_000_000_000 {
        format!("{:.2}B", v as f64 / 1_000_000_000.0)
    } else if v >= 1_000_000 {
        format!("{:.2}M", v as f64 / 1_000_000.0)
    } else if v >= 10_000 {
        format!("{:.1}K", v as f64 / 1_000.0)
    } else if v >= 1_000 {
        format!("{:.2}K", v as f64 / 1_000.0)
    } else {
        v.to_string()
    }
}

pub fn fmt_commas(v: u64) -> String {
    let s = v.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

pub fn fmt_num(v: u64, human: bool) -> String {
    if human {
        fmt_compact(v)
    } else {
        fmt_commas(v)
    }
}

fn parse_transcript_file(path: &Path, sig: FileSig) -> Option<CachedTranscript> {
    let fh = File::open(path).ok()?;
    let reader = BufReader::new(fh);
    let mut by_step: BTreeMap<i64, TurnRecord> = BTreeMap::new();
    let mut fallback_step: i64 = -1;
    let mut first_user_preview = String::new();
    let mut skipped_lines = 0usize;

    for line_res in reader.lines() {
        let Ok(line) = line_res else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) else {
            skipped_lines += 1;
            continue;
        };

        if first_user_preview.is_empty()
            && map.get("type").and_then(Value::as_str) == Some("USER_INPUT")
        {
            if let Some(content) = map.get("content").and_then(Value::as_str) {
                first_user_preview = extract_user_preview(content);
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
            .and_then(|s| DateTime::parse_from_rfc3339(s.trim()).ok())
            .map(|dt| dt.with_timezone(&Utc));

        let model = map
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown")
            .to_string();

        let step_index = match map.get("step_index").and_then(Value::as_i64) {
            Some(s) => s,
            None => {
                let s = fallback_step;
                fallback_step -= 1;
                s
            }
        };

        by_step.insert(
            step_index,
            TurnRecord {
                step_index,
                timestamp_utc,
                model,
                input_tokens,
                cache_read_tokens,
                cache_write_tokens,
                output_tokens,
            },
        );
    }

    let mut turns: Vec<TurnRecord> = by_step.into_values().collect();
    turns.sort_by_key(|t| (if t.step_index >= 0 { t.step_index } else { i64::MAX }, t.timestamp_utc));

    Some(CachedTranscript {
        sig,
        turns,
        first_user_preview,
        skipped_lines,
    })
}

fn val_to_u64(val: Option<&Value>) -> u64 {
    match val {
        Some(Value::Number(n)) => n.as_u64().or_else(|| n.as_f64().map(|f| f.max(0.0) as u64)).unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<u64>().unwrap_or(0),
        _ => 0,
    }
}

fn extract_user_preview(content: &str) -> String {
    let snippet = if let (Some(a), Some(b)) = (content.find("<USER_REQUEST>"), content.find("</USER_REQUEST>")) {
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

fn clean_single_line(text: &str, max_chars: usize) -> String {
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

fn parse_metadata_json(text: &str) -> HashMap<String, CachedMetaEntry> {
    let mut out = HashMap::new();
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(text) else {
        return out;
    };
    let Some(Value::Object(convs)) = root.get("conversations") else {
        return out;
    };
    for (cid, entry) in convs {
        let Some(summary) = entry.get("summary").and_then(Value::as_object) else {
            continue;
        };
        let title = summary
            .get("Title")
            .and_then(Value::as_str)
            .map(|s| clean_single_line(s, 100))
            .unwrap_or_default();
        let preview = summary
            .get("Preview")
            .and_then(Value::as_str)
            .map(|s| clean_single_line(s, 100))
            .unwrap_or_default();
        let workspace = summary
            .get("WorkspaceURIs")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(Value::as_str)
            .map(decode_file_uri)
            .unwrap_or_default();
        out.insert(
            cid.clone(),
            CachedMetaEntry {
                title,
                preview,
                workspace,
            },
        );
    }
    out
}

fn decode_file_uri(uri: &str) -> String {
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

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
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
                            b'n' => { out.push(b' '); i += 1; }
                            b'r' | b't' => { out.push(b' '); i += 1; }
                            b'"' => { out.push(b'"'); i += 1; }
                            b'\\' => { out.push(b'\\'); i += 1; }
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
                                if let (Some(h1), Some(h2)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                                    out.push((h1 << 4) | h2);
                                    i += 3;
                                } else {
                                    i += 1;
                                }
                            }
                            other => { out.push(other); i += 1; }
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
    fn test_pbtxt_unescape_octal_and_utf8() {
        assert_eq!(parse_pbtxt_title("title:\"黎曼猜想通俗解释\""), "黎曼猜想通俗解释");
        assert_eq!(parse_pbtxt_title("title:\"\\346\\265\\213\\350\\257\\225\""), "测试");
        assert_eq!(parse_pbtxt_title("title:\"Hello \\\"World\\\"\""), "Hello \"World\"");
    }

    #[test]
    fn test_uri_and_formatting() {
        assert_eq!(decode_file_uri("file:///C:/Users/alice/my%20proj"), "C:/Users/alice/my proj");
        assert_eq!(fmt_compact(1_540_000), "1.54M");
        assert_eq!(fmt_commas(1_234_567), "1,234,567");
    }

    #[test]
    fn test_iso_week() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 21).unwrap();
        assert_eq!(iso_week_key(d), "2026-W47");
        assert_eq!(iso_week_range_label("2026-W47"), "11/16 - 11/22");
        assert_eq!(weekday_cn(d), "周六");
    }
}
