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
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use crate::archive::UsageArchive;
use crate::pricing::{CostBreakdown, ModelRate, PricingCatalog, PricingTableRow};
use crate::quota::{
    compute_billing_blocks, BillingBlock, BudgetConfig, BudgetStore, StatuslineReader,
    StatuslineSnapshot,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceKind {
    Cli,
    App,
    Ide,
    Custom,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Cli => "cli",
            SourceKind::App => "app",
            SourceKind::Ide => "ide",
            SourceKind::Custom => "custom",
        }
    }

    pub fn label_short(self) -> &'static str {
        match self {
            SourceKind::Cli => "CLI",
            SourceKind::App => "App",
            SourceKind::Ide => "IDE",
            SourceKind::Custom => "自定义",
        }
    }

    pub fn from_str_loose(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "cli" | "antigravity-cli" => SourceKind::Cli,
            "app" | "antigravity" => SourceKind::App,
            "ide" | "antigravity-ide" => SourceKind::Ide,
            _ => SourceKind::Custom,
        }
    }

    pub fn infer_from_path(path: &Path) -> Self {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match name.as_str() {
            "antigravity-cli" => SourceKind::Cli,
            "antigravity" => SourceKind::App,
            "antigravity-ide" => SourceKind::Ide,
            _ => SourceKind::Custom,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TokenCounts {
    pub calls: u64,
    pub input_tokens: u64,       // Uncached new input tokens
    pub cache_read_tokens: u64,  // Prompt cache read tokens
    pub cache_write_tokens: u64, // Prompt cache creation tokens (if reported)
    pub output_tokens: u64,      // Output tokens (thinking + response + tool calls)
    pub peak_context: u64,       // Max single-call total input tokens
    pub cost: CostBreakdown,     // Estimated API cost in USD
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

    #[allow(dead_code)]
    pub fn add_call(&mut self, input: u64, cache_read: u64, cache_write: u64, output: u64) {
        self.add_call_with_cost(input, cache_read, cache_write, output, CostBreakdown::default());
    }

    pub fn add_call_with_cost(
        &mut self,
        input: u64,
        cache_read: u64,
        cache_write: u64,
        output: u64,
        cost: CostBreakdown,
    ) {
        self.calls += 1;
        self.input_tokens += input;
        self.cache_read_tokens += cache_read;
        self.cache_write_tokens += cache_write;
        self.output_tokens += output;
        self.cost.merge(&cost);
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
        self.cost.merge(&other.cost);
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
    pub is_checkpoint: bool,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub tool_names: Vec<String>,
    pub thinking_chars: u64,
    pub cost: CostBreakdown,
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
    pub source: SourceKind,
    pub is_archived: bool,
    pub is_subagent: bool,
    pub parent_id: Option<String>,
    pub parent_title: String,
    pub turns: Vec<TurnRecord>,
    pub counts: TokenCounts,
    pub models: BTreeSet<String>,
    pub first_time: Option<DateTime<Local>>,
    pub last_time: Option<DateTime<Local>>,
}

impl SessionInfo {
    /// Dominant model by token volume, preferring real named models over compaction checkpoints.
    pub fn primary_model(&self) -> String {
        let mut by_m: HashMap<&str, (bool, u64)> = HashMap::new();
        for t in &self.turns {
            let entry = by_m.entry(t.model.as_str()).or_insert((t.is_checkpoint, 0));
            entry.0 &= t.is_checkpoint;
            entry.1 = entry.1.saturating_add(t.total_input() + t.output_tokens);
        }
        by_m.into_iter()
            .max_by(|(_, (cp_a, tok_a)), (_, (cp_b, tok_b))| {
                // Non-checkpoint models always rank above pure checkpoint buckets
                cp_b.cmp(cp_a).then_with(|| tok_a.cmp(tok_b))
            })
            .map(|(m, _)| m.to_string())
            .unwrap_or_else(|| "-".to_string())
    }

    #[allow(dead_code)]
    pub fn workspace_short(&self) -> String {
        workspace_short_name(&self.workspace)
    }

    pub fn compaction_count(&self) -> usize {
        self.turns.iter().filter(|t| t.is_checkpoint).count()
    }

    /// Count & uncached input volume of significant multi-turn prompt cache misses (>=30K ctx, <50% hit).
    pub fn cache_miss_stats(&self) -> (usize, u64) {
        let mut count = 0usize;
        let mut uncached = 0u64;
        for (idx, t) in self.turns.iter().enumerate() {
            if idx == 0 || t.is_checkpoint {
                continue;
            }
            let ctx = t.total_input();
            if ctx >= 30_000 && t.cache_hit_rate() < 50.0 {
                count += 1;
                uncached = uncached.saturating_add(t.input_tokens);
            }
        }
        (count, uncached)
    }

    /// Current live context size (most recent turn's total input tokens).
    pub fn latest_context(&self) -> u64 {
        self.turns.last().map(TurnRecord::total_input).unwrap_or(0)
    }

    /// Effective active duration in seconds (sums consecutive turn intervals capped at 10 min idle).
    pub fn active_duration_secs(&self) -> u64 {
        let mut times: Vec<DateTime<Utc>> = self.turns.iter().filter_map(|t| t.timestamp_utc).collect();
        if times.is_empty() {
            return 0;
        }
        times.sort();
        if times.len() == 1 {
            return 15;
        }
        let mut secs: u64 = 0;
        for w in times.windows(2) {
            let delta = (w[1] - w[0]).num_seconds();
            if delta > 0 {
                secs = secs.saturating_add((delta as u64).min(600));
            }
        }
        secs.max(15)
    }

    pub fn thinking_turns(&self) -> usize {
        self.turns.iter().filter(|t| t.thinking_chars > 0).count()
    }

    pub fn total_thinking_chars(&self) -> u64 {
        self.turns.iter().map(|t| t.thinking_chars).sum()
    }

    pub fn tool_counts(&self) -> BTreeMap<String, u64> {
        let mut map = BTreeMap::new();
        for t in &self.turns {
            for name in &t.tool_names {
                *map.entry(name.clone()).or_insert(0) += 1;
            }
        }
        map
    }
}

pub fn fmt_duration_short(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", (secs + 30) / 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h{m:02}m")
    }
}

pub fn fmt_gap(secs: i64) -> String {
    if secs < 0 {
        return String::new();
    }
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

pub fn workspace_short_name(workspace: &str) -> String {
    let trimmed = workspace.trim().trim_end_matches(['/', '\\']);
    if trimmed.is_empty() || trimmed == "(未知工作区)" || trimmed == "(unknown)" {
        return String::new();
    }
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

pub const COMPACTION_MODEL_LABEL: &str = "(上下文压缩)";

pub fn is_compaction_model(model: &str) -> bool {
    model == COMPACTION_MODEL_LABEL || model == "compaction"
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ModelCacheStats {
    pub miss_count: usize,
    pub miss_uncached_tokens: u64,
    pub multi_turn_calls: u64,
    pub multi_turn_input: u64,
    pub multi_turn_cache_read: u64,
}

impl ModelCacheStats {
    pub fn multi_turn_hit_rate(&self) -> Option<f64> {
        if self.multi_turn_calls == 0 || self.multi_turn_input == 0 {
            None
        } else {
            Some((self.multi_turn_cache_read as f64 / self.multi_turn_input as f64) * 100.0)
        }
    }
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
    pub by_model_cache: HashMap<String, ModelCacheStats>,
    pub by_workspace: BTreeMap<String, (usize, TokenCounts)>,
    pub by_tool: BTreeMap<String, u64>,
    pub by_source: BTreeMap<SourceKind, (usize, TokenCounts)>,
    pub total_thinking_turns: u64,
    pub total_active_secs: u64,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ScanSnapshot {
    pub data_dir: PathBuf,
    pub data_dirs: Vec<(SourceKind, PathBuf)>,
    pub sessions: Vec<SessionInfo>,
    pub agg: AggregatedStats,
    pub active_block: Option<BillingBlock>,
    pub recent_blocks: Vec<BillingBlock>,
    pub statusline: Option<StatuslineSnapshot>,
    pub statusline_changed: bool,
    pub budget: BudgetConfig,
    pub budget_changed: bool,
    pub budget_path: Option<PathBuf>,
    pub archived_sessions_count: usize,
    pub archive_enabled: bool,
    pub archive_path: Option<PathBuf>,
    pub model_rate_labels: HashMap<String, String>,
    pub pricing_rows: Vec<PricingTableRow>,
    pub pricing_path: Option<PathBuf>,
    pub custom_pricing_count: usize,
    pub pricing_changed: bool,
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
    Custom,
}

impl DateScope {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => DateScope::Today,
            1 => DateScope::Days7,
            2 => DateScope::Days30,
            3 => DateScope::All,
            4 => DateScope::Custom,
            _ => DateScope::Today,
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            DateScope::Today => 0,
            DateScope::Days7 => 1,
            DateScope::Days30 => 2,
            DateScope::All => 3,
            DateScope::Custom => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSort {
    Recent,
    Cost,
    NonCached,
    TotalTokens,
    HitRateLow,
    PeakContext,
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
    subagent_parent_id: Option<String>,
    subagent_preview: String,
    skipped_lines: usize,
}

#[derive(Debug, Clone)]
struct CachedMetaEntry {
    title: String,
    preview: String,
    workspace: String,
    is_internal: bool,
}

pub struct IncrementalScanner {
    sources: Vec<(SourceKind, PathBuf)>,
    canon_roots: Vec<Option<PathBuf>>,
    pricing: PricingCatalog,
    statusline: StatuslineReader,
    budget: BudgetStore,
    archive: UsageArchive,
    verified_paths: HashSet<PathBuf>,
    transcript_cache: HashMap<PathBuf, CachedTranscript>,
    ann_cache: HashMap<PathBuf, (FileSig, String)>,
    meta_sigs: HashMap<PathBuf, FileSig>,
    meta_caches: HashMap<PathBuf, HashMap<String, CachedMetaEntry>>,
}

impl IncrementalScanner {
    #[allow(dead_code)]
    pub fn new(data_dir: PathBuf) -> Self {
        Self::new_with_pricing(data_dir, PricingCatalog::default_override_path())
    }

    pub fn new_with_pricing(data_dir: PathBuf, pricing_override: Option<PathBuf>) -> Self {
        let kind = SourceKind::infer_from_path(&data_dir);
        Self::new_multi(
            vec![(kind, data_dir)],
            pricing_override,
            None,
            None,
            None,
            false,
        )
    }

    pub fn new_multi(
        sources: Vec<(SourceKind, PathBuf)>,
        pricing_override: Option<PathBuf>,
        statusline_path: Option<PathBuf>,
        budget_path: Option<PathBuf>,
        archive_path: Option<PathBuf>,
        archive_enabled: bool,
    ) -> Self {
        let sources = if sources.is_empty() {
            vec![(SourceKind::Cli, Self::default_data_dir())]
        } else {
            sources
        };
        let canon_roots = sources
            .iter()
            .map(|(_, p)| fs::canonicalize(p).ok())
            .collect();
        Self {
            sources,
            canon_roots,
            pricing: PricingCatalog::new(pricing_override),
            statusline: StatuslineReader::new(statusline_path),
            budget: BudgetStore::new(budget_path),
            archive: UsageArchive::new(archive_path, archive_enabled),
            verified_paths: HashSet::new(),
            transcript_cache: HashMap::new(),
            ann_cache: HashMap::new(),
            meta_sigs: HashMap::new(),
            meta_caches: HashMap::new(),
        }
    }

    pub fn default_data_dir() -> PathBuf {
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            PathBuf::from(home).join(".gemini").join("antigravity-cli")
        } else {
            PathBuf::from(".gemini").join("antigravity-cli")
        }
    }

    /// Discovers existing local Antigravity family directories (CLI, 2.0 App, IDE) strictly read-only.
    pub fn default_data_dirs() -> Vec<(SourceKind, PathBuf)> {
        let base = if let Some(home) =
            std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
        {
            PathBuf::from(home).join(".gemini")
        } else {
            PathBuf::from(".gemini")
        };
        let candidates = [
            (SourceKind::Cli, base.join("antigravity-cli")),
            (SourceKind::App, base.join("antigravity")),
            (SourceKind::Ide, base.join("antigravity-ide")),
        ];
        let mut out = Vec::new();
        for (idx, (kind, dir)) in candidates.into_iter().enumerate() {
            if idx == 0 || dir.join("brain").is_dir() {
                out.push((kind, dir));
            }
        }
        out
    }

    #[allow(dead_code)]
    pub fn data_dir(&self) -> &Path {
        &self.sources[0].1
    }

    pub fn upsert_pricing_rule(&mut self, model: &str, rate: ModelRate) -> Result<(), String> {
        self.pricing.upsert_custom_rule(model, rate)
    }

    pub fn remove_pricing_rule(&mut self, model: &str) -> Result<bool, String> {
        self.pricing.remove_custom_rule(model)
    }

    pub fn save_budget(&mut self, cfg: BudgetConfig) -> Result<(), String> {
        self.budget.save(cfg)
    }

    fn is_safe_regular_file(&mut self, src_idx: usize, path: &Path) -> Option<FileSig> {
        let meta = fs::symlink_metadata(path).ok()?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            self.verified_paths.remove(path);
            return None;
        }
        if !self.verified_paths.contains(path) {
            if let Some(Some(root)) = self.canon_roots.get(src_idx) {
                let canon = fs::canonicalize(path).ok()?;
                if !canon.starts_with(root) {
                    return None;
                }
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

    fn refresh_metadata_cache(&mut self, src_idx: usize) {
        let root_dir = self.sources[src_idx].1.clone();
        let path = root_dir.join("cache").join("conversation_metadata.json");
        let Some(sig) = self.is_safe_regular_file(src_idx, &path) else {
            self.meta_sigs.remove(&root_dir);
            self.meta_caches.remove(&root_dir);
            return;
        };
        if self.meta_sigs.get(&root_dir) == Some(&sig) {
            return;
        }
        if let Ok(mut fh) = File::open(&path) {
            let mut text = String::new();
            if fh.read_to_string(&mut text).is_ok() {
                self.meta_caches.insert(root_dir.clone(), parse_metadata_json(&text));
                self.meta_sigs.insert(root_dir, sig);
            }
        }
    }

    fn get_annotation_title(&mut self, src_idx: usize, cid: &str) -> String {
        let path = self.sources[src_idx]
            .1
            .join("annotations")
            .join(format!("{cid}.pbtxt"));
        let Some(sig) = self.is_safe_regular_file(src_idx, &path) else {
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
        let pricing_changed = self.pricing.refresh();
        let statusline_changed = self.statusline.refresh();
        let budget_changed = self.budget.refresh();

        for i in 0..self.sources.len() {
            if self.canon_roots[i].is_none() {
                self.canon_roots[i] = fs::canonicalize(&self.sources[i].1).ok();
            }
        }

        let mut sessions_by_id: HashMap<String, SessionInfo> = HashMap::new();
        let mut skipped_empty = 0usize;
        let mut skipped_lines = 0usize;
        let mut files_cached = 0usize;
        let mut files_reparsed = 0usize;
        let mut seen_paths = BTreeSet::new();
        let mut found_any_brain = false;

        for src_idx in 0..self.sources.len() {
            let (source_kind, root_dir) = self.sources[src_idx].clone();
            let brain_dir = root_dir.join("brain");
            if !brain_dir.is_dir() {
                continue;
            }
            found_any_brain = true;
            self.refresh_metadata_cache(src_idx);

            let Ok(entries) = fs::read_dir(&brain_dir) else {
                continue;
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

                let (t_path, sig) = if let Some(s) = self.is_safe_regular_file(src_idx, &primary) {
                    (primary, s)
                } else if let Some(s) = self.is_safe_regular_file(src_idx, &fallback) {
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

                let ann_title = self.get_annotation_title(src_idx, &cid);
                let meta = self.meta_caches.get(&root_dir).and_then(|m| m.get(&cid));
                let is_subagent = match meta {
                    Some(m) if m.is_internal => true,
                    Some(_) => false,
                    None => cached.subagent_parent_id.is_some(),
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

                let mut counts = TokenCounts::default();
                let mut models = BTreeSet::new();
                let mut first_time: Option<DateTime<Local>> = None;
                let mut last_time: Option<DateTime<Local>> = None;

                let fallback_model = dominant_real_model(&cached.turns);
                let mut last_real_model: Option<String> = None;
                let mut priced_turns = cached.turns;

                for t in &mut priced_turns {
                    let eff_model = if !t.is_checkpoint && !is_compaction_model(&t.model) && t.model != "unknown" {
                        last_real_model = Some(t.model.clone());
                        t.model.as_str()
                    } else if !is_compaction_model(&t.model) && t.model != "unknown" {
                        t.model.as_str()
                    } else if let Some(prev) = last_real_model.as_deref() {
                        prev
                    } else {
                        fallback_model.as_deref().unwrap_or(t.model.as_str())
                    };
                    let local_date = t.timestamp_utc.map(|u| u.with_timezone(&Local).date_naive());
                    t.cost = self.pricing.price_turn(
                        eff_model,
                        t.input_tokens,
                        t.cache_read_tokens,
                        t.cache_write_tokens,
                        t.output_tokens,
                        local_date,
                    );

                    counts.add_call_with_cost(
                        t.input_tokens,
                        t.cache_read_tokens,
                        t.cache_write_tokens,
                        t.output_tokens,
                        t.cost,
                    );
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

                let sess = SessionInfo {
                    conversation_id: cid.clone(),
                    title,
                    workspace,
                    source: source_kind,
                    is_archived: false,
                    is_subagent,
                    parent_id,
                    parent_title: String::new(),
                    turns: priced_turns,
                    counts,
                    models,
                    first_time,
                    last_time,
                };

                // Deduplicate across multiple Antigravity dirs: keep session with more turns (or newer last_time).
                let replace = match sessions_by_id.get(&cid) {
                    None => true,
                    Some(prev) => {
                        sess.turns.len() > prev.turns.len()
                            || (sess.turns.len() == prev.turns.len() && sess.last_time > prev.last_time)
                    }
                };
                if replace {
                    sessions_by_id.insert(cid, sess);
                }
            }
        }

        let mut sessions: Vec<SessionInfo> = sessions_by_id.into_values().collect();

        // Link subagent sessions to their parent session so they inherit workspace and parent title.
        let parent_lookup: HashMap<String, (String, String)> = sessions
            .iter()
            .map(|s| (s.conversation_id.clone(), (s.title.clone(), s.workspace.clone())))
            .collect();
        for s in &mut sessions {
            if !s.is_subagent {
                continue;
            }
            let Some(pid) = s.parent_id.clone() else { continue };
            let resolved = if let Some((ptitle, pws)) = parent_lookup.get(&pid) {
                Some((ptitle.clone(), pws.clone()))
            } else {
                let mut found = None;
                for src_idx in 0..self.sources.len() {
                    let root_dir = self.sources[src_idx].1.clone();
                    let p_ann = self.get_annotation_title(src_idx, &pid);
                    let p_meta = self.meta_caches.get(&root_dir).and_then(|m| m.get(&pid));
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
                        found = Some((ptitle, pws));
                        break;
                    }
                }
                found
            };
            if let Some((ptitle, pws)) = resolved {
                if !ptitle.is_empty() {
                    s.parent_title = ptitle.clone();
                }
                if (s.workspace.is_empty() || s.workspace == "(未知工作区)")
                    && !pws.is_empty()
                    && pws != "(未知工作区)"
                {
                    s.workspace = pws;
                }
                if (s.title == "(子会话)" || s.title == "(未命名会话)") && !ptitle.is_empty() {
                    s.title = format!("子任务 · {}", ptitle);
                }
            }
        }

        // Sync live sessions into local app-directory archive & backfill any pruned historical sessions.
        let archived_backfill = self.archive.sync_and_backfill(&sessions, &self.pricing, true);
        let archived_sessions_count = archived_backfill.len();
        sessions.extend(archived_backfill);

        self.transcript_cache.retain(|k, _| seen_paths.contains(k));
        sessions.sort_by(|a, b| b.last_time.cmp(&a.last_time));

        let agg = aggregate_sessions(&sessions);
        let (active_block, mut all_blocks) = compute_billing_blocks(&sessions, Utc::now());
        if all_blocks.len() > 24 {
            let drain_n = all_blocks.len() - 24;
            all_blocks.drain(0..drain_n);
        }

        let today_local = Local::now().date_naive();
        let mut model_rate_labels = HashMap::new();
        let mut observed_ranked: Vec<(String, u64)> = agg
            .by_model
            .iter()
            .filter(|(name, _)| !is_compaction_model(name) && name.as_str() != "unknown")
            .map(|(name, c)| (name.clone(), c.total_tokens()))
            .collect();
        observed_ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let observed_names: Vec<String> = observed_ranked.into_iter().map(|(n, _)| n).collect();
        for name in agg.by_model.keys() {
            if let Some(lbl) = self.pricing.rate_label(name, Some(today_local)) {
                model_rate_labels.insert(name.clone(), lbl);
            }
        }
        let pricing_rows = self.pricing.build_table(&observed_names, today_local);
        let warning = if !found_any_brain && archived_sessions_count == 0 {
            Some(format!("未找到目录: {}", self.sources[0].1.join("brain").display()))
        } else if skipped_lines > 0 {
            Some(format!("已跳过 {skipped_lines} 行未写完/异常 JSONL（活跃写入时属正常现象）"))
        } else {
            None
        };

        ScanSnapshot {
            data_dir: self.sources[0].1.clone(),
            data_dirs: self.sources.clone(),
            sessions,
            agg,
            active_block,
            recent_blocks: all_blocks,
            statusline: self.statusline.snapshot().cloned(),
            statusline_changed,
            budget: self.budget.config(),
            budget_changed,
            budget_path: self.budget.path().map(Path::to_path_buf),
            archived_sessions_count,
            archive_enabled: self.archive.is_enabled(),
            archive_path: self.archive.path().map(Path::to_path_buf),
            model_rate_labels,
            pricing_rows,
            pricing_path: self.pricing.override_path().map(Path::to_path_buf),
            custom_pricing_count: self.pricing.custom_rules_count(),
            pricing_changed,
            skipped_empty,
            skipped_lines,
            files_cached,
            files_reparsed,
            scan_ms: started.elapsed().as_secs_f64() * 1000.0,
            warning,
        }
    }
}

fn dominant_real_model(turns: &[TurnRecord]) -> Option<String> {
    let mut by_m: HashMap<&str, u64> = HashMap::new();
    for t in turns {
        if t.is_checkpoint || is_compaction_model(&t.model) || t.model == "unknown" {
            continue;
        }
        let entry = by_m.entry(t.model.as_str()).or_insert(0);
        *entry = entry.saturating_add(t.total_input() + t.output_tokens);
    }
    by_m.into_iter()
        .max_by_key(|&(_, tok)| tok)
        .map(|(m, _)| m.to_string())
}

pub fn filter_snapshot(snapshot: &ScanSnapshot, scope: DateScope) -> (Vec<SessionInfo>, AggregatedStats) {
    filter_snapshot_range(snapshot, scope, None, None)
}

pub fn filter_snapshot_range(
    snapshot: &ScanSnapshot,
    scope: DateScope,
    custom_since: Option<NaiveDate>,
    custom_until: Option<NaiveDate>,
) -> (Vec<SessionInfo>, AggregatedStats) {
    let today = Local::now().date_naive();
    let (since, until): (Option<NaiveDate>, Option<NaiveDate>) = match scope {
        DateScope::Today => (Some(today), Some(today)),
        DateScope::Days7 => (today.checked_sub_days(chrono::Days::new(6)), Some(today)),
        DateScope::Days30 => (today.checked_sub_days(chrono::Days::new(29)), Some(today)),
        DateScope::All => (None, None),
        DateScope::Custom => (custom_since, custom_until),
    };

    if since.is_none() && until.is_none() {
        return (snapshot.sessions.clone(), snapshot.agg.clone());
    }
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
            let d = ts_local.date_naive();
            if let Some(min_d) = since {
                if d < min_d {
                    continue;
                }
            }
            if let Some(max_d) = until {
                if d > max_d {
                    continue;
                }
            }
            counts.add_call_with_cost(
                t.input_tokens,
                t.cache_read_tokens,
                t.cache_write_tokens,
                t.output_tokens,
                t.cost,
            );
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
                source: s.source,
                is_archived: s.is_archived,
                is_subagent: s.is_subagent,
                parent_id: s.parent_id.clone(),
                parent_title: s.parent_title.clone(),
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
        agg.total_active_secs = agg.total_active_secs.saturating_add(s.active_duration_secs());
        let ws = agg.by_workspace.entry(s.workspace.clone()).or_insert((0, TokenCounts::default()));
        ws.0 += 1;
        ws.1.merge(&s.counts);
        let src = agg.by_source.entry(s.source).or_insert((0, TokenCounts::default()));
        src.0 += 1;
        src.1.merge(&s.counts);

        for (idx, t) in s.turns.iter().enumerate() {
            if t.thinking_chars > 0 {
                agg.total_thinking_turns += 1;
            }
            for tool in &t.tool_names {
                *agg.by_tool.entry(tool.clone()).or_insert(0) += 1;
            }
            agg.by_model
                .entry(t.model.clone())
                .or_default()
                .add_call_with_cost(
                    t.input_tokens,
                    t.cache_read_tokens,
                    t.cache_write_tokens,
                    t.output_tokens,
                    t.cost,
                );

            let mcs = agg.by_model_cache.entry(t.model.clone()).or_default();
            if idx > 0 && !t.is_checkpoint {
                let ctx = t.total_input();
                mcs.multi_turn_calls += 1;
                mcs.multi_turn_input = mcs.multi_turn_input.saturating_add(ctx);
                mcs.multi_turn_cache_read = mcs.multi_turn_cache_read.saturating_add(t.cache_read_tokens);
                if ctx >= 30_000 && t.cache_hit_rate() < 50.0 {
                    mcs.miss_count += 1;
                    mcs.miss_uncached_tokens = mcs.miss_uncached_tokens.saturating_add(t.input_tokens);
                }
            }

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
                    .add_call_with_cost(
                        t.input_tokens,
                        t.cache_read_tokens,
                        t.cache_write_tokens,
                        t.output_tokens,
                        t.cost,
                    );
                agg.by_week
                    .entry(wk_key.clone())
                    .or_default()
                    .add_call_with_cost(
                        t.input_tokens,
                        t.cache_read_tokens,
                        t.cache_write_tokens,
                        t.output_tokens,
                        t.cost,
                    );
                week_day_sets.entry(wk_key).or_default().insert(day_key);
                agg.by_month
                    .entry(month_key)
                    .or_default()
                    .add_call_with_cost(
                        t.input_tokens,
                        t.cache_read_tokens,
                        t.cache_write_tokens,
                        t.output_tokens,
                        t.cost,
                    );
                agg.by_hour[hour].add_call_with_cost(
                    t.input_tokens,
                    t.cache_read_tokens,
                    t.cache_write_tokens,
                    t.output_tokens,
                    t.cost,
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
    let mut reader = BufReader::with_capacity(64 * 1024, fh);
    let mut by_step: BTreeMap<i64, TurnRecord> = BTreeMap::new();
    let mut fallback_step: i64 = -1;
    let mut seen_user_input = false;
    let mut first_user_preview = String::new();
    let mut subagent_parent_id: Option<String> = None;
    let mut subagent_preview = String::new();
    let mut skipped_lines = 0usize;
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
        // Fast path: skip full JSON DOM parsing for bulky tool-result / non-token lines
        // that are syntactically framed as a JSON object and carry no token counters.
        let need_preview = !seen_user_input && trimmed.contains("USER_INPUT");
        let need_subagent_meta = !seen_user_input
            && subagent_parent_id.is_none()
            && subagent_preview.is_empty()
            && trimmed.contains("SYSTEM_MESSAGE");
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
        } else if !seen_user_input
            && subagent_parent_id.is_none()
            && subagent_preview.is_empty()
            && step_type == "SYSTEM_MESSAGE"
        {
            if let Some(content) = map.get("content").and_then(Value::as_str) {
                let (pid, prev) = extract_subagent_meta(content);
                subagent_parent_id = pid;
                subagent_preview = prev;
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
                output_tokens,
                tool_names,
                thinking_chars,
                cost: CostBreakdown::default(),
            },
        );
    }

    let mut turns: Vec<TurnRecord> = by_step.into_values().collect();
    turns.sort_by_key(|t| (if t.step_index >= 0 { t.step_index } else { i64::MAX }, t.timestamp_utc));
    if seen_user_input {
        subagent_parent_id = None;
        subagent_preview.clear();
    }

    Some(CachedTranscript {
        sig,
        turns,
        first_user_preview,
        subagent_parent_id,
        subagent_preview,
        skipped_lines,
    })
}

fn is_uuid_like(s: &str) -> bool {
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

/// Parses subagent launcher system message:
/// `[Message] timestamp=... sender=<parent-uuid> priority=... content=<task prompt>`
fn extract_subagent_meta(content: &str) -> (Option<String>, String) {
    let mut parent_id = None;
    if let Some(pos) = content.find("sender=") {
        let rest = &content[pos + "sender=".len()..];
        let token = rest.split_whitespace().next().unwrap_or("");
        if is_uuid_like(token) {
            parent_id = Some(token.to_string());
        }
    }
    let mut preview = String::new();
    if let Some(pos) = content.find("content=") {
        let body = &content[pos + "content=".len()..];
        // Pick first meaningful non-empty line, strip markdown markers for crisp display.
        for line in body.lines() {
            let trimmed = line.trim().trim_start_matches('#').trim();
            if trimmed.is_empty() || trimmed.starts_with("</SYSTEM_MESSAGE>") {
                continue;
            }
            let cleaned_md = trimmed.replace("**", "").replace('`', "");
            preview = clean_single_line(&cleaned_md, 72);
            if !preview.is_empty() {
                break;
            }
        }
    }
    (parent_id, preview)
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

    #[test]
    fn test_primary_model_and_workspace_short() {
        let s = SessionInfo {
            conversation_id: "abc".into(),
            title: "t".into(),
            workspace: "C:/Users/qingz/agent-home/".into(),
            source: SourceKind::Cli,
            is_archived: false,
            is_subagent: false,
            parent_id: None,
            parent_title: String::new(),
            turns: vec![
                TurnRecord {
                    step_index: 1,
                    timestamp_utc: None,
                    model: "(上下文压缩)".into(),
                    is_checkpoint: true,
                    input_tokens: 5_000,
                    cache_read_tokens: 500_000,
                    cache_write_tokens: 0,
                    output_tokens: 6_000,
                    tool_names: Vec::new(),
                    thinking_chars: 0,
                    cost: CostBreakdown::default(),
                },
                TurnRecord {
                    step_index: 2,
                    timestamp_utc: None,
                    model: "gemini-4-neon-xhigh".into(),
                    is_checkpoint: false,
                    input_tokens: 1_000,
                    cache_read_tokens: 100_000,
                    cache_write_tokens: 0,
                    output_tokens: 500,
                    tool_names: vec!["read_file".into()],
                    thinking_chars: 80,
                    cost: CostBreakdown::default(),
                },
                TurnRecord {
                    step_index: 3,
                    timestamp_utc: None,
                    model: "gemini-4-neon-high".into(),
                    is_checkpoint: false,
                    input_tokens: 500,
                    cache_read_tokens: 10_000,
                    cache_write_tokens: 0,
                    output_tokens: 200,
                    tool_names: Vec::new(),
                    thinking_chars: 0,
                    cost: CostBreakdown::default(),
                },
            ],
            counts: TokenCounts::default(),
            models: BTreeSet::new(),
            first_time: None,
            last_time: None,
        };
        assert_eq!(s.primary_model(), "gemini-4-neon-xhigh");
        assert_eq!(s.workspace_short(), "agent-home");
        assert_eq!(s.compaction_count(), 1);
        assert_eq!(workspace_short_name("(未知工作区)"), "");
        assert_eq!(fmt_gap(42), "42s");
        assert_eq!(fmt_gap(250), "4m10s");
        assert_eq!(fmt_gap(3725), "1h02m");
    }

    #[test]
    fn test_cache_miss_stats() {
        let s = SessionInfo {
            conversation_id: "m".into(),
            title: "m".into(),
            workspace: "".into(),
            source: SourceKind::Cli,
            is_archived: false,
            is_subagent: false,
            parent_id: None,
            parent_title: String::new(),
            turns: vec![
                // Turn 0 cold start is not counted as a multi-turn miss
                TurnRecord {
                    step_index: 1,
                    timestamp_utc: None,
                    model: "m".into(),
                    is_checkpoint: false,
                    input_tokens: 40_000,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    output_tokens: 500,
                    tool_names: Vec::new(),
                    thinking_chars: 0,
                    cost: CostBreakdown::default(),
                },
                // Normal high hit
                TurnRecord {
                    step_index: 2,
                    timestamp_utc: None,
                    model: "m".into(),
                    is_checkpoint: false,
                    input_tokens: 2_000,
                    cache_read_tokens: 40_000,
                    cache_write_tokens: 0,
                    output_tokens: 500,
                    tool_names: Vec::new(),
                    thinking_chars: 0,
                    cost: CostBreakdown::default(),
                },
                // Big cache miss (>=30K ctx, <50% hit)
                TurnRecord {
                    step_index: 3,
                    timestamp_utc: None,
                    model: "m".into(),
                    is_checkpoint: false,
                    input_tokens: 180_000,
                    cache_read_tokens: 5_000,
                    cache_write_tokens: 0,
                    output_tokens: 800,
                    tool_names: Vec::new(),
                    thinking_chars: 0,
                    cost: CostBreakdown::default(),
                },
            ],
            counts: TokenCounts::default(),
            models: BTreeSet::new(),
            first_time: None,
            last_time: None,
        };
        assert_eq!(s.cache_miss_stats(), (1, 180_000));
        assert_eq!(s.latest_context(), 185_000);

        let agg = aggregate_sessions(&[s]);
        let mcs = agg.by_model_cache.get("m").expect("model cache stats");
        assert_eq!(mcs.miss_count, 1);
        assert_eq!(mcs.miss_uncached_tokens, 180_000);
        assert_eq!(mcs.multi_turn_calls, 2);
        let mt_rate = mcs.multi_turn_hit_rate().expect("multi-turn rate");
        // (40_000 + 5_000) / (42_000 + 185_000) = 45_000 / 227_000 ≈ 19.82%
        assert!((mt_rate - 19.8237).abs() < 0.05);
        assert!(is_compaction_model(COMPACTION_MODEL_LABEL));
        assert!(is_compaction_model("compaction"));
        assert!(!is_compaction_model("gemini-4-neon-xhigh"));
    }

    #[test]
    fn test_scanner_pricing_compaction_and_override() {
        let base = std::env::temp_dir().join(format!("agy_gauge_price_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let logs = base
            .join("brain")
            .join("sess-1")
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&logs).unwrap();
        let jsonl = [
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-22T01:00:00Z","model":"gemini-4-neon-xhigh","input_tokens":1000000,"cache_read_tokens":0,"output_tokens":0}"#,
            r#"{"step_index":2,"type":"CHECKPOINT","status":"DONE","created_at":"2026-11-22T01:05:00Z","input_tokens":1000000,"cache_read_tokens":0,"output_tokens":0}"#,
        ]
        .join("\n");
        fs::write(logs.join("transcript.jsonl"), jsonl).unwrap();

        let pricing_path = base.join("pricing.json");
        let mut scanner = IncrementalScanner::new_with_pricing(base.clone(), Some(pricing_path.clone()));
        let snap1 = scanner.scan();
        // Built-in gemini-4-neon input = $3/M -> 2 turns * $3 = $6.00 (checkpoint inherited neon)
        assert!((snap1.agg.total.cost.total_usd() - 6.0).abs() < 1e-6);
        assert_eq!(snap1.agg.total.cost.unpriced_calls, 0);

        // Drop a pricing.json override and verify hot reload on next scan
        fs::write(
            &pricing_path,
            r#"{"models":{"gemini-4-neon":{"input":10.0,"cache_read":0.5,"output":20.0}}}"#,
        )
        .unwrap();
        let snap2 = scanner.scan();
        assert!(snap2.pricing_changed);
        assert_eq!(snap2.custom_pricing_count, 1);
        assert!((snap2.agg.total.cost.total_usd() - 20.0).abs() < 1e-6);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_subagent_parent_linking_and_workspace_inheritance() {
        let base = std::env::temp_dir().join(format!("agy_gauge_subagent_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let parent_id = "8c1095e2-1558-4a06-bfde-446204f978aa";
        let child_id = "14ee4713-eb4d-4a3c-b6a9-6745e2e553e2";
        let p_logs = base.join("brain").join(parent_id).join(".system_generated").join("logs");
        let c_logs = base.join("brain").join(child_id).join(".system_generated").join("logs");
        let cache = base.join("cache");
        fs::create_dir_all(&p_logs).unwrap();
        fs::create_dir_all(&c_logs).unwrap();
        fs::create_dir_all(&cache).unwrap();

        fs::write(
            cache.join("conversation_metadata.json"),
            format!(
                r#"{{"conversations":{{"{parent_id}":{{"summary":{{"Title":"Subvault Review","WorkspaceURIs":["file:///C:/Users/qingz/agent-home"]}}}},"{child_id}":{{"is_internal":true}}}}}}"#
            ),
        )
        .unwrap();
        let parent_lines = [
            r#"{"step_index":0,"type":"USER_INPUT","source":"USER_EXPLICIT","status":"DONE","content":"<USER_REQUEST>\nReview SubVault\n</USER_REQUEST>"}"#.to_string(),
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-22T14:48:00Z","model":"gemini-4-neon-xhigh","input_tokens":1000,"cache_read_tokens":0,"output_tokens":100}"#.to_string(),
            // Reply SYSTEM_MESSAGE sent from child back to parent must NEVER mark parent as a subagent.
            format!(
                r#"{{"step_index":2,"type":"SYSTEM_MESSAGE","source":"SYSTEM","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-22T14:50:00Z sender={child_id} priority=MESSAGE_PRIORITY_HIGH content=Subagent finished\n</SYSTEM_MESSAGE>"}}"#
            ),
        ]
        .join("\n");
        fs::write(p_logs.join("transcript.jsonl"), format!("{parent_lines}\n")).unwrap();
        let child_sys = format!(
            r#"{{"step_index":0,"type":"SYSTEM_MESSAGE","source":"SYSTEM","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-22T14:48:47Z sender={parent_id} priority=MESSAGE_PRIORITY_HIGH content=You are executing **Wave 2 · Track A** for `SubVault`.\n</SYSTEM_MESSAGE>"}}"#
        );
        let child_turn = r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-22T14:49:00Z","model":"gemini-4-hydrogen-tiered-xhigh","input_tokens":2000,"cache_read_tokens":5000,"output_tokens":300}"#;
        fs::write(c_logs.join("transcript.jsonl"), format!("{child_sys}\n{child_turn}\n")).unwrap();

        let mut scanner = IncrementalScanner::new_with_pricing(base.clone(), None);
        let snap = scanner.scan();
        assert_eq!(snap.sessions.len(), 2);
        let parent = snap.sessions.iter().find(|s| s.conversation_id == parent_id).unwrap();
        assert!(!parent.is_subagent);
        assert!(parent.parent_id.is_none());
        let child = snap.sessions.iter().find(|s| s.conversation_id == child_id).unwrap();
        assert!(child.is_subagent);
        assert_eq!(child.parent_id.as_deref(), Some(parent_id));
        assert_eq!(child.parent_title, "Subvault Review");
        assert_eq!(child.workspace, "C:/Users/qingz/agent-home");
        assert_eq!(child.title, "You are executing Wave 2 · Track A for SubVault.");
        // Child's tokens are aggregated under the inherited parent workspace, not "(未知工作区)"
        assert!(!snap.agg.by_workspace.contains_key("(未知工作区)"));
        assert_eq!(snap.agg.by_workspace.get("C:/Users/qingz/agent-home").unwrap().0, 2);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_multi_source_tools_thinking_and_custom_range() {
        let base = std::env::temp_dir().join(format!("agy_gauge_multi_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let cli_dir = base.join("antigravity-cli");
        let app_dir = base.join("antigravity");
        let s1_logs = cli_dir.join("brain").join("sess-cli").join(".system_generated").join("logs");
        let s2_logs = app_dir.join("brain").join("sess-app").join(".system_generated").join("logs");
        fs::create_dir_all(&s1_logs).unwrap();
        fs::create_dir_all(&s2_logs).unwrap();

        fs::write(
            s1_logs.join("transcript.jsonl"),
            [
                r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-20T10:00:00Z","model":"gemini-4-neon-xhigh","input_tokens":1000,"cache_read_tokens":2000,"output_tokens":300,"thinking":"step by step","tool_calls":[{"name":"read_file","args":{}},{"name":"run_command","args":{}}]}"#,
                r#"{"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-20T10:02:00Z","model":"gemini-4-neon-xhigh","input_tokens":500,"cache_read_tokens":3000,"output_tokens":200,"tool_calls":[{"name":"read_file","args":{}}]}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        fs::write(
            s2_logs.join("transcript.jsonl"),
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-22T10:00:00Z","model":"claude-opus-5-5","input_tokens":2000,"cache_read_tokens":0,"output_tokens":500}"#,
        )
        .unwrap();

        let mut scanner = IncrementalScanner::new_multi(
            vec![(SourceKind::Cli, cli_dir), (SourceKind::App, app_dir)],
            None,
            None,
            None,
            None,
            false,
        );
        let snap = scanner.scan();
        assert_eq!(snap.sessions.len(), 2);
        assert_eq!(snap.agg.by_source.len(), 2);
        assert_eq!(snap.agg.by_tool.get("read_file"), Some(&2));
        assert_eq!(snap.agg.by_tool.get("run_command"), Some(&1));
        assert_eq!(snap.agg.total_thinking_turns, 1);

        let cli_s = snap.sessions.iter().find(|s| s.conversation_id == "sess-cli").unwrap();
        assert_eq!(cli_s.source, SourceKind::Cli);
        assert_eq!(cli_s.active_duration_secs(), 120);

        let d_only = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        let (filtered, f_agg) = filter_snapshot_range(&snap, DateScope::Custom, Some(d_only), Some(d_only));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].conversation_id, "sess-cli");
        assert_eq!(f_agg.by_tool.get("read_file"), Some(&2));

        let _ = fs::remove_dir_all(&base);
    }
}
