//! Strictly read-only multi-agent incremental scanner & aggregator for `Gauge`.
//!
//! Supported Tier-1 pure-text agents (via `crate::sources` adapters):
//!   - Antigravity (`agy` CLI / 2.0 App / IDE)
//!   - Claude Code (`~/.claude/projects`, `$CLAUDE_CONFIG_DIR`)
//!   - OpenAI Codex CLI (`~/.codex/sessions`, `$CODEX_HOME`, pure JSONL rollouts only)
//!   - Gemini CLI (`~/.gemini/tmp`, `$GEMINI_CLI_HOME`)
//!
//! Never writes inside agent directories, never touches SQLite databases, never reads credentials.

use chrono::{DateTime, Datelike, FixedOffset, Local, NaiveDate, Timelike, Utc};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use crate::archive::UsageArchive;
use crate::pricing::{CostBreakdown, ModelRate, PricingCatalog, PricingTableRow};
use crate::quota::{BudgetConfig, BudgetStore, StatuslineReader, StatuslineSnapshot};
use crate::session_meta::{SessionMetaStore, SessionUserMeta};
use crate::sources::{
    build_adapter, default_antigravity_cli_dir, discover_antigravity_sources,
    discover_default_sources, path_eq_platform, relative_components_under, AdapterPoll,
    ParsedSession, SourceAdapter,
};

pub use crate::sources::antigravity::{parse_pbtxt_title, scan_antigravity_today_totals};
pub use crate::sources::{
    compute_running_hint, SourceDiscoveryFilter, ACTIVE_MTIME_WINDOW_SECS, RUNNING_STEP_STALE_SECS,
};

const INTERN_CAP: usize = 4096;
const ARCHIVE_PERSIST_DEBOUNCE: Duration = Duration::from_secs(120);

/// Bounded process-wide string interner for low-cardinality strings (model names, tool names).
/// If the table reaches `INTERN_CAP`, returns a fresh `Arc<str>` without unbounded memory growth.
pub fn intern_str(s: &str) -> Arc<str> {
    static INTERNER: OnceLock<RwLock<HashSet<Arc<str>>>> = OnceLock::new();
    let lock = INTERNER.get_or_init(|| RwLock::new(HashSet::with_capacity(64)));
    if let Ok(guard) = lock.read() {
        if let Some(existing) = guard.get(s) {
            return Arc::clone(existing);
        }
        if guard.len() >= INTERN_CAP {
            return Arc::from(s);
        }
    }
    if let Ok(mut guard) = lock.write() {
        if let Some(existing) = guard.get(s) {
            return Arc::clone(existing);
        }
        let arc: Arc<str> = Arc::from(s);
        if guard.len() < INTERN_CAP {
            guard.insert(Arc::clone(&arc));
        }
        return arc;
    }
    Arc::from(s)
}

/// Caches local timezone `FixedOffset` per 15-minute UTC bucket to eliminate thousands of
/// redundant Win32 timezone syscalls during session pricing, filtering, and aggregation while
/// remaining exact across DST transitions.
#[derive(Debug, Clone)]
pub struct LocalTzCache {
    last_bucket: i64,
    last_offset: FixedOffset,
    offsets: HashMap<i64, FixedOffset>,
}

impl Default for LocalTzCache {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalTzCache {
    pub fn new() -> Self {
        let now_utc = Utc::now();
        let bucket = now_utc.timestamp().div_euclid(900);
        let offset = *now_utc.with_timezone(&Local).offset();
        let mut offsets = HashMap::with_capacity(16);
        offsets.insert(bucket, offset);
        Self {
            last_bucket: bucket,
            last_offset: offset,
            offsets,
        }
    }

    #[inline]
    pub fn to_local(&mut self, utc: DateTime<Utc>) -> DateTime<Local> {
        let bucket = utc.timestamp().div_euclid(900);
        let offset = if bucket == self.last_bucket {
            self.last_offset
        } else {
            let off = *self
                .offsets
                .entry(bucket)
                .or_insert_with(|| *utc.with_timezone(&Local).offset());
            self.last_bucket = bucket;
            self.last_offset = off;
            off
        };
        DateTime::<Local>::from_naive_utc_and_offset(utc.naive_utc(), offset)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AgentFamily {
    Antigravity,
    ClaudeCode,
    Codex,
    GeminiCli,
}

impl AgentFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentFamily::Antigravity => "antigravity",
            AgentFamily::ClaudeCode => "claude-code",
            AgentFamily::Codex => "codex",
            AgentFamily::GeminiCli => "gemini-cli",
        }
    }

    pub fn label_short(self) -> &'static str {
        match self {
            AgentFamily::Antigravity => "AGY",
            AgentFamily::ClaudeCode => "Claude",
            AgentFamily::Codex => "Codex",
            AgentFamily::GeminiCli => "Gemini",
        }
    }

    pub fn label_full(self) -> &'static str {
        match self {
            AgentFamily::Antigravity => "Antigravity",
            AgentFamily::ClaudeCode => "Claude Code",
            AgentFamily::Codex => "Codex CLI",
            AgentFamily::GeminiCli => "Gemini CLI",
        }
    }

    pub fn search_aliases(self) -> &'static [&'static str] {
        match self {
            AgentFamily::Antigravity => &["agy", "antigravity", "cli", "app", "ide"],
            AgentFamily::ClaudeCode => &["claude", "claude-code", "claudecode"],
            AgentFamily::Codex => &["codex", "codex-cli", "openai-codex"],
            AgentFamily::GeminiCli => &["gemini", "gemini-cli", "geminicli"],
        }
    }

    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "agy" | "antigravity" | "antigravity-cli" | "antigravity-ide" | "cli" | "app"
            | "ide" => Some(AgentFamily::Antigravity),
            "claude" | "claude-code" | "claudecode" | "cc" => Some(AgentFamily::ClaudeCode),
            "codex" | "codex-cli" | "openai-codex" => Some(AgentFamily::Codex),
            "gemini" | "gemini-cli" | "geminicli" => Some(AgentFamily::GeminiCli),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceKind {
    Cli,
    App,
    Ide,
    ClaudeCode,
    Codex,
    GeminiCli,
    Custom,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Cli => "cli",
            SourceKind::App => "app",
            SourceKind::Ide => "ide",
            SourceKind::ClaudeCode => "claude-code",
            SourceKind::Codex => "codex",
            SourceKind::GeminiCli => "gemini-cli",
            SourceKind::Custom => "custom",
        }
    }

    pub fn label_short(self) -> &'static str {
        match self {
            SourceKind::Cli => "CLI",
            SourceKind::App => "App",
            SourceKind::Ide => "IDE",
            SourceKind::ClaudeCode => "Claude",
            SourceKind::Codex => "Codex",
            SourceKind::GeminiCli => "Gemini",
            SourceKind::Custom => "自定义",
        }
    }

    pub fn agent_family(self) -> AgentFamily {
        match self {
            SourceKind::Cli | SourceKind::App | SourceKind::Ide | SourceKind::Custom => {
                AgentFamily::Antigravity
            }
            SourceKind::ClaudeCode => AgentFamily::ClaudeCode,
            SourceKind::Codex => AgentFamily::Codex,
            SourceKind::GeminiCli => AgentFamily::GeminiCli,
        }
    }

    pub fn family_label(self) -> &'static str {
        self.agent_family().label_full()
    }

    pub fn from_str_loose(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "cli" | "antigravity-cli" | "agy-cli" | "agy" => SourceKind::Cli,
            "app" | "antigravity" | "agy-app" => SourceKind::App,
            "ide" | "antigravity-ide" | "agy-ide" => SourceKind::Ide,
            "claude-code" | "claude" | "claudecode" => SourceKind::ClaudeCode,
            "codex" | "codex-cli" | "openai-codex" => SourceKind::Codex,
            "gemini-cli" | "gemini" | "geminicli" => SourceKind::GeminiCli,
            _ => SourceKind::Custom,
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        Self::from_str_loose(s)
    }

    pub fn infer_from_path(path: &Path) -> Self {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match name.as_str() {
            "antigravity-cli" => return SourceKind::Cli,
            "antigravity" => return SourceKind::App,
            "antigravity-ide" => return SourceKind::Ide,
            ".claude" | "claude" => return SourceKind::ClaudeCode,
            ".codex" | "codex" => return SourceKind::Codex,
            "gemini-cli" => return SourceKind::GeminiCli,
            ".gemini" => {
                if path.join("tmp").is_dir() && !path.join("brain").is_dir() {
                    return SourceKind::GeminiCli;
                }
            }
            "tmp" => {
                let parent_is_gemini = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|s| s.to_str())
                    .map(|s| s.eq_ignore_ascii_case(".gemini"))
                    .unwrap_or(false);
                if parent_is_gemini {
                    return SourceKind::GeminiCli;
                }
            }
            _ => {}
        }
        // Directory layout fallback for non-default custom paths (e.g. `--data-dir <path>`).
        if path.join("brain").is_dir() {
            return SourceKind::Custom;
        }
        if path.join("projects").is_dir() {
            return SourceKind::ClaudeCode;
        }
        if path.join("sessions").is_dir() || path.join("archived_sessions").is_dir() {
            return SourceKind::Codex;
        }
        if path.join("tmp").is_dir() {
            return SourceKind::GeminiCli;
        }
        SourceKind::Custom
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
    pub model: Arc<str>,
    pub is_checkpoint: bool,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Optional subset of `cache_write_tokens` billed at 1h ephemeral TTL (2.0x base input; NOT additive).
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
    /// Optional informational subset of `output_tokens` representing reasoning/thoughts tokens (NOT additive).
    pub reasoning_tokens: u64,
    pub tool_names: Box<[Arc<str>]>,
    pub thinking_chars: u64,
    /// Estimated wall-clock generation seconds (request start -> last tool call streamed out).
    /// `None` when not measurable (pure text replies, checkpoints, missing tool steps, non-AGY logs).
    pub gen_secs: Option<u32>,
    pub cost: CostBreakdown,
}

/// Minimum window for a trustworthy speed estimate: timestamps have 1 s resolution, so shorter
/// windows (or tiny outputs) would swing wildly.
pub const TPS_MIN_GEN_SECS: u32 = 3;
pub const TPS_MIN_OUTPUT_TOKENS: u64 = 64;

impl TurnRecord {
    /// Whether this turn involved model thinking/reasoning (plaintext chars or token count).
    pub fn has_thinking(&self) -> bool {
        self.thinking_chars > 0 || self.reasoning_tokens > 0
    }

    /// End-to-end output speed (tokens/s, thinking included, TTFT included).
    pub fn output_tps(&self) -> Option<f64> {
        let g = self.gen_secs?;
        if g < TPS_MIN_GEN_SECS || self.output_tokens < TPS_MIN_OUTPUT_TOKENS {
            return None;
        }
        Some(self.output_tokens as f64 / g as f64)
    }

    pub fn total_input(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    pub fn total_tokens(&self) -> u64 {
        self.total_input().saturating_add(self.output_tokens)
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

/// Prices a `TurnRecord`, honoring `cache_write_1h_tokens` at 2.0x base input when present
/// while pricing remaining 5m cache write tokens at `rate.cache_write`.
pub fn price_turn_record(
    pricing: &PricingCatalog,
    eff_model: &str,
    t: &TurnRecord,
    local_date: Option<NaiveDate>,
) -> CostBreakdown {
    let cw_1h = t.cache_write_1h_tokens.min(t.cache_write_tokens);
    if cw_1h == 0 {
        return pricing.price_turn(
            eff_model,
            t.input_tokens,
            t.cache_read_tokens,
            t.cache_write_tokens,
            t.output_tokens,
            local_date,
        );
    }
    let ctx = t.total_input();
    let Some(rate) = pricing.resolve_rate(eff_model, ctx, local_date) else {
        return CostBreakdown::unpriced();
    };
    let cw_5m = t.cache_write_tokens.saturating_sub(cw_1h);
    let mut cost = rate.calculate(t.input_tokens, t.cache_read_tokens, cw_5m, t.output_tokens);
    cost.cache_write_usd += (cw_1h as f64 / 1_000_000.0) * (rate.input * 2.0);
    cost
}

/// Dominant model by token volume, preferring real named models over compaction checkpoints.
pub fn compute_primary_model(turns: &[TurnRecord]) -> Arc<str> {
    let mut by_m: HashMap<&str, (bool, u64)> = HashMap::new();
    for t in turns {
        let entry = by_m.entry(t.model.as_ref()).or_insert((t.is_checkpoint, 0));
        entry.0 &= t.is_checkpoint;
        entry.1 = entry.1.saturating_add(t.total_input() + t.output_tokens);
    }
    by_m.into_iter()
        .max_by(|(name_a, (cp_a, tok_a)), (name_b, (cp_b, tok_b))| {
            // Non-checkpoint models always rank above pure checkpoint buckets
            cp_b.cmp(cp_a)
                .then_with(|| tok_a.cmp(tok_b))
                .then_with(|| name_a.cmp(name_b))
        })
        .map(|(m, _)| intern_str(m))
        .unwrap_or_else(|| intern_str("-"))
}

/// Effective active duration in seconds (sums consecutive turn intervals capped at 10 min idle).
/// Uses a zero-allocation single-pass fast path when timestamps are monotonic.
pub fn compute_active_duration_secs(turns: &[TurnRecord]) -> u64 {
    let mut count = 0usize;
    let mut prev: Option<DateTime<Utc>> = None;
    let mut secs: u64 = 0;
    let mut out_of_order = false;
    for t in turns {
        let Some(ts) = t.timestamp_utc else { continue };
        count += 1;
        if let Some(p) = prev {
            if ts < p {
                out_of_order = true;
                break;
            }
            let delta = (ts - p).num_seconds();
            if delta > 0 {
                secs = secs.saturating_add((delta as u64).min(600));
            }
        }
        prev = Some(ts);
    }
    if !out_of_order {
        return if count == 0 {
            0
        } else if count == 1 {
            15
        } else {
            secs.max(15)
        };
    }

    let mut times: Vec<DateTime<Utc>> = turns.iter().filter_map(|t| t.timestamp_utc).collect();
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
    pub turns: Arc<[TurnRecord]>,
    pub counts: TokenCounts,
    pub models: BTreeSet<String>,
    pub first_time: Option<DateTime<Local>>,
    pub last_time: Option<DateTime<Local>>,
    pub last_file_mtime_utc: Option<DateTime<Utc>>,
    pub is_running_hint: bool,
    pub primary_model: Arc<str>,
    pub active_duration_secs: u64,
    pub compaction_count: u32,
    pub thinking_turns: u32,
}

impl SessionInfo {
    /// Recomputes precomputed summary fields (`primary_model`, `active_duration_secs`,
    /// `compaction_count`, `thinking_turns`) from `self.turns`.
    pub fn recompute_summary(&mut self) {
        self.primary_model = compute_primary_model(&self.turns);
        self.active_duration_secs = compute_active_duration_secs(&self.turns);
        self.compaction_count = self.turns.iter().filter(|t| t.is_checkpoint).count() as u32;
        self.thinking_turns = self.turns.iter().filter(|t| t.has_thinking()).count() as u32;
    }

    /// Dominant model by token volume, preferring real named models over compaction checkpoints.
    #[inline]
    pub fn primary_model(&self) -> &str {
        &self.primary_model
    }

    #[allow(dead_code)]
    pub fn workspace_short(&self) -> String {
        workspace_short_name(&self.workspace)
    }

    #[inline]
    pub fn compaction_count(&self) -> usize {
        self.compaction_count as usize
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
    #[inline]
    pub fn active_duration_secs(&self) -> u64 {
        self.active_duration_secs
    }

    #[inline]
    pub fn thinking_turns(&self) -> usize {
        self.thinking_turns as usize
    }

    pub fn total_thinking_chars(&self) -> u64 {
        self.turns.iter().map(|t| t.thinking_chars).sum()
    }

    pub fn tool_counts(&self) -> BTreeMap<String, u64> {
        let mut map: BTreeMap<String, u64> = BTreeMap::new();
        for t in self.turns.iter() {
            for name in t.tool_names.iter() {
                if let Some(cnt) = map.get_mut(name.as_ref()) {
                    *cnt += 1;
                } else {
                    map.insert(name.to_string(), 1);
                }
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
    /// Output tokens / generation seconds over calls with a trustworthy speed estimate.
    pub speed_output_tokens: u64,
    pub speed_gen_secs: u64,
    pub speed_calls: u64,
}

impl ModelCacheStats {
    /// Token-weighted end-to-end output speed (tok/s), robust to 1 s timestamp rounding.
    pub fn output_tps(&self) -> Option<f64> {
        (self.speed_gen_secs > 0).then(|| self.speed_output_tokens as f64 / self.speed_gen_secs as f64)
    }

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
    pub by_agent: BTreeMap<AgentFamily, (usize, TokenCounts)>,
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
    pub statusline: Option<StatuslineSnapshot>,
    pub statusline_changed: bool,
    pub budget: BudgetConfig,
    pub budget_changed: bool,
    pub budget_path: Option<PathBuf>,
    pub session_meta: HashMap<String, SessionUserMeta>,
    pub session_meta_changed: bool,
    pub session_meta_path: Option<PathBuf>,
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

impl ScanSnapshot {
    /// Returns `true` if any scanned session or configured source belongs to a non-Antigravity agent.
    pub fn has_non_antigravity(&self) -> bool {
        self.sessions
            .iter()
            .any(|s| s.source.agent_family() != AgentFamily::Antigravity)
            || self
                .data_dirs
                .iter()
                .any(|(k, _)| k.agent_family() != AgentFamily::Antigravity)
    }
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
    TotalTokens,
    FreshInput,
    Output,
    HitRateLow,
}

#[derive(Debug, Default, Clone)]
pub struct ScanHints {
    pub paths: Vec<PathBuf>,
    pub overflow: bool,
    pub has_companion_change: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanDelta {
    pub changed: bool,
    pub running_changed: bool,
    pub files_reparsed: usize,
}

#[derive(Debug, Clone)]
struct CachedPricedSession {
    pricing_gen: u64,
    last_step_running: bool,
    session: SessionInfo,
}

#[derive(Debug, Default)]
struct AdapterScanCache {
    found_root: bool,
    skipped_empty: usize,
    skipped_lines: usize,
    files_cached: usize,
    sessions: HashMap<String, CachedPricedSession>,
}

#[derive(Debug, Clone)]
struct ActiveRunningEntry {
    family: AgentFamily,
    cid: String,
    last_step_running: bool,
    mtime_utc: Option<DateTime<Utc>>,
}

pub struct IncrementalScanner {
    sources: Vec<(SourceKind, PathBuf)>,
    adapters: Vec<Box<dyn SourceAdapter>>,
    adapter_caches: Vec<AdapterScanCache>,
    pricing: PricingCatalog,
    pricing_gen: u64,
    pricing_dirty: bool,
    statusline: StatuslineReader,
    budget: BudgetStore,
    budget_dirty: bool,
    archive: UsageArchive,
    archive_filter: SourceDiscoveryFilter,
    session_meta: SessionMetaStore,
    has_scanned: bool,
    has_persisted_archive: bool,
    last_archive_persist: Option<Instant>,
    last_live_sessions: Vec<SessionInfo>,
    last_full_sessions: Vec<SessionInfo>,
    last_agg: AggregatedStats,
    active_running: Vec<ActiveRunningEntry>,
    last_archived_count: usize,
    last_skipped_empty: usize,
    last_skipped_lines: usize,
    last_files_cached: usize,
    last_found_any_root: bool,
    last_warning: Option<String>,
    cached_model_rate_labels: HashMap<String, String>,
    cached_pricing_rows: Vec<PricingTableRow>,
    cached_pricing_date: Option<NaiveDate>,
}

impl Drop for IncrementalScanner {
    fn drop(&mut self) {
        self.flush_archive();
    }
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
        let adapters: Vec<Box<dyn SourceAdapter>> = sources
            .iter()
            .map(|(kind, path)| build_adapter(*kind, path.clone()))
            .collect();
        let adapter_caches = (0..adapters.len())
            .map(|_| AdapterScanCache::default())
            .collect();
        Self {
            sources,
            adapters,
            adapter_caches,
            pricing: PricingCatalog::new(pricing_override),
            pricing_gen: 1,
            pricing_dirty: false,
            statusline: StatuslineReader::new(statusline_path),
            budget: BudgetStore::new(budget_path),
            budget_dirty: false,
            archive: UsageArchive::new(archive_path, archive_enabled),
            archive_filter: SourceDiscoveryFilter::All,
            session_meta: SessionMetaStore::new(None),
            has_scanned: false,
            has_persisted_archive: false,
            last_archive_persist: None,
            last_live_sessions: Vec::new(),
            last_full_sessions: Vec::new(),
            last_agg: AggregatedStats::default(),
            active_running: Vec::new(),
            last_archived_count: 0,
            last_skipped_empty: 0,
            last_skipped_lines: 0,
            last_files_cached: 0,
            last_found_any_root: false,
            last_warning: None,
            cached_model_rate_labels: HashMap::new(),
            cached_pricing_rows: Vec::new(),
            cached_pricing_date: None,
        }
    }

    pub fn default_data_dir() -> PathBuf {
        default_antigravity_cli_dir()
    }

    /// Discovers all installed local Tier-1 pure-text agent directories (Antigravity + Claude Code + Codex + Gemini CLI).
    pub fn default_data_dirs() -> Vec<(SourceKind, PathBuf)> {
        discover_default_sources(&SourceDiscoveryFilter::All)
    }

    /// Discovers only Antigravity family directories (CLI, 2.0 App, IDE) — used by `agy-statusline.exe`.
    pub fn default_antigravity_dirs() -> Vec<(SourceKind, PathBuf)> {
        discover_antigravity_sources()
    }

    /// Discovers local Tier-1 agent directories matching `filter` (`All`, `AgyOnly`, or `Families(...)`).
    pub fn discover_data_dirs(filter: &SourceDiscoveryFilter) -> Vec<(SourceKind, PathBuf)> {
        discover_default_sources(filter)
    }

    #[allow(dead_code)]
    pub fn data_dir(&self) -> &Path {
        &self.sources[0].1
    }

    pub fn upsert_pricing_rule(&mut self, model: &str, rate: ModelRate) -> Result<(), String> {
        let res = self.pricing.upsert_custom_rule(model, rate);
        if res.is_ok() {
            self.pricing_dirty = true;
        }
        res
    }

    pub fn remove_pricing_rule(&mut self, model: &str) -> Result<bool, String> {
        let res = self.pricing.remove_custom_rule(model);
        if matches!(res, Ok(true)) {
            self.pricing_dirty = true;
        }
        res
    }

    pub fn save_budget(&mut self, cfg: BudgetConfig) -> Result<(), String> {
        let res = self.budget.save(cfg);
        if res.is_ok() {
            self.budget_dirty = true;
        }
        res
    }

    pub fn with_session_meta(mut self, session_meta_path: Option<PathBuf>) -> Self {
        self.session_meta = SessionMetaStore::new(session_meta_path);
        self
    }

    pub fn with_archive_filter(mut self, filter: SourceDiscoveryFilter) -> Self {
        self.archive_filter = filter;
        self.has_scanned = false;
        self
    }

    pub fn toggle_session_pin(&mut self, cid: &str) -> Result<bool, String> {
        self.session_meta.toggle_pin(cid)
    }

    pub fn set_session_pinned(&mut self, cid: &str, pinned: bool) -> Result<bool, String> {
        self.session_meta.set_pinned(cid, pinned)
    }

    pub fn clear_session_meta(&mut self, cid: &str) -> Result<bool, String> {
        self.session_meta.clear_entry(cid)
    }

    pub fn reload_session_meta(&mut self) -> bool {
        self.session_meta.force_reload()
    }

    /// Immediately persists any debounced archive changes to disk (with `sync_data`).
    pub fn flush_archive(&mut self) {
        if self.archive.flush(&self.last_live_sessions, true) {
            self.last_archive_persist = Some(Instant::now());
            self.has_persisted_archive = true;
        }
    }

    /// Returns deduplicated whitelisted directories to monitor with `ReadDirectoryChangesW`.
    /// Never includes SQLite WAL directories (such as `~/.gemini/.../conversations/`).
    pub fn watched_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut push_unique = |p: PathBuf| {
            let key = if cfg!(windows) {
                p.to_string_lossy().to_ascii_lowercase()
            } else {
                p.to_string_lossy().into_owned()
            };
            if seen.insert(key) {
                dirs.push(p);
            }
        };
        for adapter in &self.adapters {
            for d in adapter.watched_dirs() {
                push_unique(d);
            }
        }
        for opt_path in [
            self.pricing.override_path(),
            self.statusline.path(),
            self.budget.path(),
            self.session_meta.path(),
        ] {
            if let Some(parent) = opt_path.and_then(Path::parent) {
                if parent.is_dir() {
                    push_unique(parent.to_path_buf());
                }
            }
        }
        dirs
    }

    /// Zero-syscall O(active) check that clears expired `is_running_hint` badges in place.
    /// Returns `true` if any session's running status flipped `true -> false`.
    pub fn expire_running_hints_only(&mut self, current: &mut ScanSnapshot) -> bool {
        self.refresh_running_hints_in_place(Utc::now(), Some(current))
    }

    /// Targeted event-driven refresh driven by `ReadDirectoryChangesW` hints.
    /// - On `hints.overflow` or before initial scan: falls back cleanly to `scan_if_changed`.
    /// - On companion-only hints (`statusline_state.json`, `pricing.json`, `budget.json`,
    ///   `session_meta.json`): refreshes only companion configs in place (`< 0.1 ms`, 0 adapter calls).
    /// - On targeted agent paths: invokes `SourceAdapter::poll_hinted` so single-file appends only
    ///   stat & tail the 1 modified file instead of walking all 220+ sessions.
    pub fn scan_with_hints(&mut self, current: &mut ScanSnapshot, hints: &ScanHints) -> ScanDelta {
        let started = Instant::now();
        let scan_now_utc = Utc::now();
        if !self.has_scanned || hints.overflow {
            return self.scan_if_changed(current);
        }
        if !hints.has_companion_change
            && hints.paths.is_empty()
            && !self.pricing_dirty
            && !self.budget_dirty
        {
            self.maybe_flush_debounced_archive();
            let running_changed = self.refresh_running_hints_in_place(scan_now_utc, Some(current));
            current.files_reparsed = 0;
            current.scan_ms = started.elapsed().as_secs_f64() * 1000.0;
            return ScanDelta {
                changed: false,
                running_changed,
                files_reparsed: 0,
            };
        }

        let mut check_companion =
            hints.has_companion_change || self.pricing_dirty || self.budget_dirty;
        let mut per_adapter: Vec<Vec<PathBuf>> = vec![Vec::new(); self.adapters.len()];

        for p in &hints.paths {
            let is_companion = [
                self.pricing.override_path(),
                self.statusline.path(),
                self.budget.path(),
                self.session_meta.path(),
            ]
            .into_iter()
            .flatten()
            .any(|cp| path_eq_platform(cp, p));
            if is_companion {
                check_companion = true;
                continue;
            }
            let mut matched_adapter = false;
            for (idx, adapter) in self.adapters.iter().enumerate() {
                if relative_components_under(adapter.root(), p).is_some() {
                    per_adapter[idx].push(p.clone());
                    matched_adapter = true;
                    break;
                }
            }
            if !matched_adapter {
                return self.scan_if_changed(current);
            }
        }

        let (pricing_changed, statusline_changed, budget_changed, session_meta_changed) =
            if check_companion {
                let pc = self.pricing.refresh() || std::mem::take(&mut self.pricing_dirty);
                if pc {
                    self.pricing_gen = self.pricing_gen.wrapping_add(1);
                }
                let sc = self.statusline.refresh();
                let bc = self.budget.refresh() || std::mem::take(&mut self.budget_dirty);
                let smc = self.session_meta.refresh();
                (pc, sc, bc, smc)
            } else {
                (false, false, false, false)
            };

        let mut any_adapter_changed = false;
        let mut files_reparsed = 0usize;
        let mut tz_cache: Option<LocalTzCache> = None;

        for idx in 0..self.adapters.len() {
            if !per_adapter[idx].is_empty() {
                let poll = self.adapters[idx]
                    .poll_hinted(&per_adapter[idx], scan_now_utc)
                    .unwrap_or_else(|| self.adapters[idx].poll_changes(scan_now_utc));
                let (changed, reparsed) =
                    self.apply_adapter_poll(idx, poll, pricing_changed, &mut tz_cache);
                if changed {
                    any_adapter_changed = true;
                }
                files_reparsed += reparsed;
            } else if pricing_changed {
                self.reprice_adapter_cache(idx, &mut tz_cache);
            }
        }

        let files_cached: usize = self.adapter_caches.iter().map(|c| c.files_cached).sum();
        self.last_files_cached = files_cached;

        if any_adapter_changed || pricing_changed {
            let prev_running: HashSet<(AgentFamily, String)> = current
                .sessions
                .iter()
                .filter(|s| s.is_running_hint)
                .map(|s| (s.source.agent_family(), s.conversation_id.clone()))
                .collect();

            self.rebuild_sessions_and_agg(scan_now_utc);

            let new_running: HashSet<(AgentFamily, String)> = self
                .last_full_sessions
                .iter()
                .filter(|s| s.is_running_hint)
                .map(|s| (s.source.agent_family(), s.conversation_id.clone()))
                .collect();
            let running_changed = prev_running != new_running;

            *current = self.build_snapshot(
                started,
                pricing_changed,
                statusline_changed,
                budget_changed,
                session_meta_changed,
                files_reparsed,
            );
            return ScanDelta {
                changed: true,
                running_changed,
                files_reparsed,
            };
        }

        // Fast companion-only or unchanged-hint path: zero session/agg rebuilds!
        self.maybe_flush_debounced_archive();
        let running_changed = self.refresh_running_hints_in_place(scan_now_utc, Some(current));

        let today_local = Local::now().date_naive();
        let date_rolled = self.cached_pricing_date != Some(today_local);
        if date_rolled {
            self.refresh_pricing_tables(today_local);
            current.model_rate_labels = self.cached_model_rate_labels.clone();
            current.pricing_rows = self.cached_pricing_rows.clone();
        }

        current.statusline_changed = statusline_changed;
        if statusline_changed {
            current.statusline = self.statusline.snapshot().cloned();
        }
        current.budget_changed = budget_changed;
        if budget_changed {
            current.budget = self.budget.config();
        }
        current.session_meta_changed = session_meta_changed;
        if session_meta_changed {
            current.session_meta = self.session_meta.entries().clone();
        }
        current.pricing_changed = false;
        current.files_cached = files_cached;
        current.files_reparsed = 0;
        current.scan_ms = started.elapsed().as_secs_f64() * 1000.0;

        ScanDelta {
            changed: statusline_changed || budget_changed || session_meta_changed || date_rolled,
            running_changed,
            files_reparsed: 0,
        }
    }

    /// Polls all sources incrementally and updates `current` in place when anything changed.
    /// When no files or configs changed, performs zero `SessionInfo` / `TurnRecord` / `AggregatedStats`
    /// allocations and only expires active `is_running_hint` badges or flushes a due debounced archive.
    pub fn scan_if_changed(&mut self, current: &mut ScanSnapshot) -> ScanDelta {
        let started = Instant::now();
        let scan_now_utc = Utc::now();
        if !self.has_scanned {
            let snap = self.scan();
            let reparsed = snap.files_reparsed;
            *current = snap;
            return ScanDelta {
                changed: true,
                running_changed: true,
                files_reparsed: reparsed,
            };
        }

        let pricing_changed = self.pricing.refresh() || std::mem::take(&mut self.pricing_dirty);
        if pricing_changed {
            self.pricing_gen = self.pricing_gen.wrapping_add(1);
        }
        let statusline_changed = self.statusline.refresh();
        let budget_changed = self.budget.refresh() || std::mem::take(&mut self.budget_dirty);
        let session_meta_changed = self.session_meta.refresh();

        let (any_adapter_changed, files_reparsed, files_cached) =
            self.poll_adapters(scan_now_utc, pricing_changed);

        if any_adapter_changed || pricing_changed {
            let prev_running: HashSet<(AgentFamily, String)> = current
                .sessions
                .iter()
                .filter(|s| s.is_running_hint)
                .map(|s| (s.source.agent_family(), s.conversation_id.clone()))
                .collect();

            self.rebuild_sessions_and_agg(scan_now_utc);

            let new_running: HashSet<(AgentFamily, String)> = self
                .last_full_sessions
                .iter()
                .filter(|s| s.is_running_hint)
                .map(|s| (s.source.agent_family(), s.conversation_id.clone()))
                .collect();
            let running_changed = prev_running != new_running;

            *current = self.build_snapshot(
                started,
                pricing_changed,
                statusline_changed,
                budget_changed,
                session_meta_changed,
                files_reparsed,
            );
            return ScanDelta {
                changed: true,
                running_changed,
                files_reparsed,
            };
        }

        // Fast idle path: zero adapter changes and zero pricing changes.
        self.maybe_flush_debounced_archive();
        let running_changed = self.refresh_running_hints_in_place(scan_now_utc, Some(current));

        let today_local = Local::now().date_naive();
        let date_rolled = self.cached_pricing_date != Some(today_local);
        if date_rolled {
            self.refresh_pricing_tables(today_local);
            current.model_rate_labels = self.cached_model_rate_labels.clone();
            current.pricing_rows = self.cached_pricing_rows.clone();
        }

        current.statusline_changed = statusline_changed;
        if statusline_changed {
            current.statusline = self.statusline.snapshot().cloned();
        }
        current.budget_changed = budget_changed;
        if budget_changed {
            current.budget = self.budget.config();
        }
        current.session_meta_changed = session_meta_changed;
        if session_meta_changed {
            current.session_meta = self.session_meta.entries().clone();
        }
        current.pricing_changed = false;
        current.files_cached = files_cached;
        current.files_reparsed = 0;
        current.scan_ms = started.elapsed().as_secs_f64() * 1000.0;

        ScanDelta {
            changed: statusline_changed || budget_changed || session_meta_changed || date_rolled,
            running_changed,
            files_reparsed: 0,
        }
    }

    pub fn scan(&mut self) -> ScanSnapshot {
        let started = Instant::now();
        let scan_now_utc = Utc::now();
        let pricing_changed = self.pricing.refresh() || std::mem::take(&mut self.pricing_dirty);
        if pricing_changed {
            self.pricing_gen = self.pricing_gen.wrapping_add(1);
        }
        let statusline_changed = self.statusline.refresh();
        let budget_changed = self.budget.refresh() || std::mem::take(&mut self.budget_dirty);
        let session_meta_changed = self.session_meta.refresh();

        let (any_adapter_changed, files_reparsed, _files_cached) =
            self.poll_adapters(scan_now_utc, pricing_changed);

        if !self.has_scanned || any_adapter_changed || pricing_changed {
            self.rebuild_sessions_and_agg(scan_now_utc);
        } else {
            self.maybe_flush_debounced_archive();
            self.refresh_running_hints_in_place(scan_now_utc, None);
            let today_local = Local::now().date_naive();
            if self.cached_pricing_date != Some(today_local) {
                self.refresh_pricing_tables(today_local);
            }
        }

        self.build_snapshot(
            started,
            pricing_changed,
            statusline_changed,
            budget_changed,
            session_meta_changed,
            files_reparsed,
        )
    }

    fn reprice_adapter_cache(&mut self, idx: usize, tz_cache: &mut Option<LocalTzCache>) {
        if self.adapter_caches[idx].sessions.is_empty() {
            return;
        }
        let tz = tz_cache.get_or_insert_with(LocalTzCache::new);
        let pricing_gen = self.pricing_gen;
        for (cid, cached) in self.adapter_caches[idx].sessions.iter_mut() {
            if cached.pricing_gen != pricing_gen {
                let old_turns = Arc::clone(&cached.session.turns);
                reprice_session_info(&mut cached.session, &self.pricing, tz);
                cached.pricing_gen = pricing_gen;
                self.adapters[idx].adopt_priced_turns(cid, &old_turns, &cached.session.turns);
            }
        }
    }

    fn apply_adapter_poll(
        &mut self,
        idx: usize,
        poll: AdapterPoll,
        pricing_changed: bool,
        tz_cache: &mut Option<LocalTzCache>,
    ) -> (bool, usize) {
        match poll {
            AdapterPoll::Unchanged {
                found_root,
                files_cached,
                skipped_empty,
                skipped_lines,
            } => {
                {
                    let cache = &mut self.adapter_caches[idx];
                    cache.found_root = found_root;
                    cache.files_cached = files_cached;
                    cache.skipped_empty = skipped_empty;
                    cache.skipped_lines = skipped_lines;
                }
                if pricing_changed {
                    self.reprice_adapter_cache(idx, tz_cache);
                }
                (false, 0)
            }
            AdapterPoll::Delta {
                updated_sessions,
                removed_cids,
                files_cached,
                files_reparsed,
                skipped_empty,
                skipped_lines,
            } => {
                let tz = tz_cache.get_or_insert_with(LocalTzCache::new);
                let pricing_gen = self.pricing_gen;
                let mut touched_cids: HashSet<String> =
                    HashSet::with_capacity(updated_sessions.len());

                for cid in removed_cids {
                    self.adapter_caches[idx].sessions.remove(&cid);
                }
                for parsed in updated_sessions {
                    let cid = parsed.conversation_id.clone();
                    if parsed.turns.is_empty() {
                        self.adapter_caches[idx].sessions.remove(&cid);
                        continue;
                    }
                    touched_cids.insert(cid.clone());
                    let reuse = self.adapter_caches[idx].sessions.get(&cid).is_some_and(|prev| {
                        prev.pricing_gen == pricing_gen
                            && Arc::ptr_eq(&prev.session.turns, &parsed.turns)
                    });
                    if reuse {
                        if let Some(prev) = self.adapter_caches[idx].sessions.get_mut(&cid) {
                            prev.session.title = parsed.title;
                            prev.session.workspace = parsed.workspace;
                            prev.session.source = parsed.source;
                            prev.session.is_subagent = parsed.is_subagent;
                            prev.session.parent_id = parsed.parent_id;
                            prev.session.parent_title = parsed.parent_title;
                            prev.session.last_file_mtime_utc = parsed.last_file_mtime_utc;
                            prev.session.is_running_hint = parsed.is_running_hint;
                            prev.last_step_running = parsed.last_step_running;
                        }
                    } else {
                        let old_turns = Arc::clone(&parsed.turns);
                        let last_step_running = parsed.last_step_running;
                        let sess = build_priced_session(parsed, &self.pricing, tz);
                        // Hand single shared priced Arc<[TurnRecord]> back to adapter immediately.
                        self.adapters[idx].adopt_priced_turns(&cid, &old_turns, &sess.turns);
                        self.adapter_caches[idx].sessions.insert(
                            cid,
                            CachedPricedSession {
                                pricing_gen,
                                last_step_running,
                                session: sess,
                            },
                        );
                    }
                }
                if pricing_changed {
                    for (cid, cached) in self.adapter_caches[idx].sessions.iter_mut() {
                        if !touched_cids.contains(cid) && cached.pricing_gen != pricing_gen {
                            let old_turns = Arc::clone(&cached.session.turns);
                            reprice_session_info(&mut cached.session, &self.pricing, tz);
                            cached.pricing_gen = pricing_gen;
                            self.adapters[idx].adopt_priced_turns(
                                cid,
                                &old_turns,
                                &cached.session.turns,
                            );
                        }
                    }
                }
                let cache = &mut self.adapter_caches[idx];
                cache.found_root = true;
                cache.files_cached = files_cached;
                cache.skipped_empty = skipped_empty;
                cache.skipped_lines = skipped_lines;
                (true, files_reparsed)
            }
            AdapterPoll::Changed(res) => {
                let tz = tz_cache.get_or_insert_with(LocalTzCache::new);
                let pricing_gen = self.pricing_gen;
                let mut prev_map = std::mem::take(&mut self.adapter_caches[idx].sessions);
                let mut next_map: HashMap<String, CachedPricedSession> =
                    HashMap::with_capacity(res.sessions.len());
                let mut extra_empty = 0usize;

                for parsed in res.sessions {
                    if parsed.turns.is_empty() {
                        extra_empty += 1;
                        continue;
                    }
                    let cid = parsed.conversation_id.clone();
                    let priced_entry = match prev_map.remove(&cid) {
                        Some(mut prev)
                            if prev.pricing_gen == pricing_gen
                                && Arc::ptr_eq(&prev.session.turns, &parsed.turns) =>
                        {
                            // Turns and pricing unchanged: update metadata only, zero turn clones!
                            prev.session.title = parsed.title;
                            prev.session.workspace = parsed.workspace;
                            prev.session.source = parsed.source;
                            prev.session.is_subagent = parsed.is_subagent;
                            prev.session.parent_id = parsed.parent_id;
                            prev.session.parent_title = parsed.parent_title;
                            prev.session.last_file_mtime_utc = parsed.last_file_mtime_utc;
                            prev.session.is_running_hint = parsed.is_running_hint;
                            prev.last_step_running = parsed.last_step_running;
                            prev
                        }
                        _ => {
                            let old_turns = Arc::clone(&parsed.turns);
                            let last_step_running = parsed.last_step_running;
                            let sess = build_priced_session(parsed, &self.pricing, tz);
                            // Adopt priced Arc<[TurnRecord]> back into adapter so zero duplicate
                            // unpriced turn buffers remain in RAM (~4.5 MiB saved!).
                            self.adapters[idx].adopt_priced_turns(&cid, &old_turns, &sess.turns);
                            CachedPricedSession {
                                pricing_gen,
                                last_step_running,
                                session: sess,
                            }
                        }
                    };

                    let replace = match next_map.get(&cid) {
                        None => true,
                        Some(existing) => {
                            priced_entry.session.turns.len() > existing.session.turns.len()
                                || (priced_entry.session.turns.len()
                                    == existing.session.turns.len()
                                    && priced_entry.session.last_time
                                        > existing.session.last_time)
                        }
                    };
                    if replace {
                        next_map.insert(cid, priced_entry);
                    }
                }

                let cache = &mut self.adapter_caches[idx];
                cache.found_root = res.found_root;
                cache.skipped_empty = res.skipped_empty + extra_empty;
                cache.skipped_lines = res.skipped_lines;
                cache.files_cached = res.files_cached;
                cache.sessions = next_map;
                (true, res.files_reparsed)
            }
        }
    }

    fn poll_adapters(
        &mut self,
        scan_now_utc: DateTime<Utc>,
        pricing_changed: bool,
    ) -> (bool, usize, usize) {
        let mut any_changed = false;
        let mut total_reparsed = 0usize;
        let mut tz_cache: Option<LocalTzCache> = None;

        for idx in 0..self.adapters.len() {
            let poll = self.adapters[idx].poll_changes(scan_now_utc);
            let (changed, reparsed) =
                self.apply_adapter_poll(idx, poll, pricing_changed, &mut tz_cache);
            if changed {
                any_changed = true;
            }
            total_reparsed += reparsed;
        }

        let total_cached: usize = self.adapter_caches.iter().map(|c| c.files_cached).sum();
        self.last_files_cached = total_cached;
        (any_changed, total_reparsed, total_cached)
    }

    fn rebuild_sessions_and_agg(&mut self, scan_now_utc: DateTime<Utc>) {
        let mut sessions_by_key: HashMap<(AgentFamily, String), (SessionInfo, bool)> =
            HashMap::new();
        let mut skipped_empty = 0usize;
        let mut skipped_lines = 0usize;
        let mut files_cached = 0usize;
        let mut found_any_root = false;

        for cache in &self.adapter_caches {
            if cache.found_root {
                found_any_root = true;
            }
            skipped_empty += cache.skipped_empty;
            skipped_lines += cache.skipped_lines;
            files_cached += cache.files_cached;

            for cached in cache.sessions.values() {
                let mut sess = cached.session.clone();
                // Keep running hint accurate even for unchanged adapters whose files aged out.
                if sess.is_running_hint {
                    sess.is_running_hint = compute_running_hint(
                        cached.last_step_running,
                        sess.last_file_mtime_utc,
                        scan_now_utc,
                    );
                }
                let family = sess.source.agent_family();
                let key = (family, sess.conversation_id.clone());
                let replace = match sessions_by_key.get(&key) {
                    None => true,
                    Some((prev, _)) => {
                        sess.turns.len() > prev.turns.len()
                            || (sess.turns.len() == prev.turns.len()
                                && sess.last_time > prev.last_time)
                    }
                };
                if replace {
                    sessions_by_key.insert(key, (sess, cached.last_step_running));
                }
            }
        }

        let mut live_sessions: Vec<SessionInfo> = Vec::with_capacity(sessions_by_key.len());
        let mut active_running: Vec<ActiveRunningEntry> = Vec::new();
        for ((family, cid), (sess, last_step_running)) in sessions_by_key {
            if sess.is_running_hint {
                active_running.push(ActiveRunningEntry {
                    family,
                    cid,
                    last_step_running,
                    mtime_utc: sess.last_file_mtime_utc,
                });
            }
            live_sessions.push(sess);
        }

        // Link subagent sessions to their parent within the SAME AgentFamily so they inherit
        // workspace and parent title without ever cross-linking across different agents.
        if live_sessions.iter().any(|s| s.is_subagent) {
            let parent_lookup: HashMap<(AgentFamily, String), (String, String)> = live_sessions
                .iter()
                .map(|s| {
                    (
                        (s.source.agent_family(), s.conversation_id.clone()),
                        (s.title.clone(), s.workspace.clone()),
                    )
                })
                .collect();
            for s in &mut live_sessions {
                if !s.is_subagent {
                    continue;
                }
                let Some(pid) = s.parent_id.clone() else { continue };
                let family = s.source.agent_family();
                let resolved = if let Some((ptitle, pws)) = parent_lookup.get(&(family, pid.clone())) {
                    Some((ptitle.clone(), pws.clone()))
                } else {
                    let mut found = None;
                    for adapter in &mut self.adapters {
                        if adapter.kind().agent_family() != family {
                            continue;
                        }
                        if let Some(hint) = adapter.lookup_session_hint(&pid) {
                            found = Some(hint);
                            break;
                        }
                    }
                    found
                };
                if let Some((ptitle, pws)) = resolved {
                    if !ptitle.is_empty() && s.parent_title.is_empty() {
                        s.parent_title = ptitle.clone();
                    }
                    if (s.workspace.is_empty()
                        || s.workspace == "(未知工作区)"
                        || s.workspace == "(unknown)")
                        && !pws.is_empty()
                        && pws != "(未知工作区)"
                        && pws != "(unknown)"
                    {
                        s.workspace = pws;
                    }
                    if (s.title == "(子会话)" || s.title == "(未命名会话)") && !ptitle.is_empty()
                    {
                        s.title = format!("子任务 · {}", ptitle);
                    }
                }
            }
        }

        // If any previously-live session disappeared from disk while debounced archive changes
        // were pending in memory, flush the previous live set first so zero turns are lost.
        let any_vanished = if !self.last_live_sessions.is_empty() {
            let new_keys: HashSet<(AgentFamily, &str)> = live_sessions
                .iter()
                .map(|s| (s.source.agent_family(), s.conversation_id.as_str()))
                .collect();
            self.last_live_sessions
                .iter()
                .any(|old| !new_keys.contains(&(old.source.agent_family(), old.conversation_id.as_str())))
        } else {
            false
        };
        if any_vanished && self.archive.is_dirty() {
            if self.archive.flush(&self.last_live_sessions, false) {
                self.last_archive_persist = Some(Instant::now());
                self.has_persisted_archive = true;
            }
        }

        // Debounce periodic archive writes to 120s; persist immediately on first dirty scan or removal.
        let allow_persist = !self.has_persisted_archive
            || any_vanished
            || self
                .last_archive_persist
                .map(|t| t.elapsed() >= ARCHIVE_PERSIST_DEBOUNCE)
                .unwrap_or(true);

        let was_dirty_before = self.archive.is_dirty();
        let archived_backfill = self.archive.sync_and_backfill_cached(
            &live_sessions,
            &self.pricing,
            self.pricing_gen,
            allow_persist,
            false,
            &self.archive_filter,
        );
        if allow_persist && (was_dirty_before || !self.has_persisted_archive) && !self.archive.is_dirty() {
            self.last_archive_persist = Some(Instant::now());
            self.has_persisted_archive = true;
        }

        let archived_sessions_count = archived_backfill.len();
        self.last_live_sessions = live_sessions.clone();

        let mut sessions = live_sessions;
        sessions.extend(archived_backfill);
        sessions.sort_by(|a, b| b.last_time.cmp(&a.last_time));

        let agg = aggregate_sessions(&sessions);

        let warning = if !found_any_root && archived_sessions_count == 0 {
            if self.sources.len() == 1
                && self.sources[0].0.agent_family() == AgentFamily::Antigravity
            {
                Some(format!(
                    "未找到目录: {}",
                    self.sources[0].1.join("brain").display()
                ))
            } else {
                Some(format!(
                    "未找到可用的本地日志目录（已检查 {} 个来源）",
                    self.sources.len()
                ))
            }
        } else if skipped_lines > 0 {
            Some(format!(
                "已跳过 {skipped_lines} 行未写完/异常 JSONL（活跃写入时属正常现象）"
            ))
        } else {
            None
        };

        self.last_full_sessions = sessions;
        self.last_agg = agg;
        self.active_running = active_running;
        self.last_archived_count = archived_sessions_count;
        self.last_skipped_empty = skipped_empty;
        self.last_skipped_lines = skipped_lines;
        self.last_files_cached = files_cached;
        self.last_found_any_root = found_any_root;
        self.last_warning = warning;
        self.has_scanned = true;

        let today_local = Local::now().date_naive();
        self.refresh_pricing_tables(today_local);
    }

    fn refresh_pricing_tables(&mut self, today_local: NaiveDate) {
        let mut model_rate_labels = HashMap::new();
        let mut observed_ranked: Vec<(String, u64)> = self
            .last_agg
            .by_model
            .iter()
            .filter(|(name, _)| !is_compaction_model(name) && name.as_str() != "unknown")
            .map(|(name, c)| (name.clone(), c.total_tokens()))
            .collect();
        observed_ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let observed_names: Vec<String> = observed_ranked.into_iter().map(|(n, _)| n).collect();
        for name in self.last_agg.by_model.keys() {
            if let Some(lbl) = self.pricing.rate_label(name, Some(today_local)) {
                model_rate_labels.insert(name.clone(), lbl);
            }
        }
        self.cached_pricing_rows = self.pricing.build_table(&observed_names, today_local);
        self.cached_model_rate_labels = model_rate_labels;
        self.cached_pricing_date = Some(today_local);
    }

    fn maybe_flush_debounced_archive(&mut self) {
        if self.archive.is_dirty()
            && self
                .last_archive_persist
                .map(|t| t.elapsed() >= ARCHIVE_PERSIST_DEBOUNCE)
                .unwrap_or(true)
        {
            if self.archive.flush(&self.last_live_sessions, false) {
                self.last_archive_persist = Some(Instant::now());
                self.has_persisted_archive = true;
            }
        }
    }

    fn refresh_running_hints_in_place(
        &mut self,
        scan_now_utc: DateTime<Utc>,
        mut current: Option<&mut ScanSnapshot>,
    ) -> bool {
        if self.active_running.is_empty() {
            return false;
        }
        let mut expired: Vec<(AgentFamily, String)> = Vec::new();
        self.active_running.retain(|entry| {
            let still_running =
                compute_running_hint(entry.last_step_running, entry.mtime_utc, scan_now_utc);
            if !still_running {
                expired.push((entry.family, entry.cid.clone()));
            }
            still_running
        });
        if expired.is_empty() {
            return false;
        }
        for (family, cid) in &expired {
            for s in &mut self.last_full_sessions {
                if s.is_running_hint
                    && s.source.agent_family() == *family
                    && s.conversation_id == *cid
                {
                    s.is_running_hint = false;
                }
            }
            if let Some(snap) = current.as_deref_mut() {
                for s in &mut snap.sessions {
                    if s.is_running_hint
                        && s.source.agent_family() == *family
                        && s.conversation_id == *cid
                    {
                        s.is_running_hint = false;
                    }
                }
            }
        }
        true
    }

    fn build_snapshot(
        &self,
        started: Instant,
        pricing_changed: bool,
        statusline_changed: bool,
        budget_changed: bool,
        session_meta_changed: bool,
        files_reparsed: usize,
    ) -> ScanSnapshot {
        ScanSnapshot {
            data_dir: self.sources[0].1.clone(),
            data_dirs: self.sources.clone(),
            sessions: self.last_full_sessions.clone(),
            agg: self.last_agg.clone(),
            statusline: self.statusline.snapshot().cloned(),
            statusline_changed,
            budget: self.budget.config(),
            budget_changed,
            budget_path: self.budget.path().map(Path::to_path_buf),
            session_meta: self.session_meta.entries().clone(),
            session_meta_changed,
            session_meta_path: self.session_meta.path().map(Path::to_path_buf),
            archived_sessions_count: self.last_archived_count,
            archive_enabled: self.archive.is_enabled(),
            archive_path: self.archive.path().map(Path::to_path_buf),
            model_rate_labels: self.cached_model_rate_labels.clone(),
            pricing_rows: self.cached_pricing_rows.clone(),
            pricing_path: self.pricing.override_path().map(Path::to_path_buf),
            custom_pricing_count: self.pricing.custom_rules_count(),
            pricing_changed,
            skipped_empty: self.last_skipped_empty,
            skipped_lines: self.last_skipped_lines,
            files_cached: self.last_files_cached,
            files_reparsed,
            scan_ms: started.elapsed().as_secs_f64() * 1000.0,
            warning: self.last_warning.clone(),
        }
    }
}

fn build_priced_session(
    parsed: ParsedSession,
    pricing: &PricingCatalog,
    tz_cache: &mut LocalTzCache,
) -> SessionInfo {
    let mut counts = TokenCounts::default();
    let mut models = BTreeSet::new();
    let mut first_time: Option<DateTime<Local>> = None;
    let mut last_time: Option<DateTime<Local>> = None;
    let mut compaction_count: u32 = 0;
    let mut thinking_turns: u32 = 0;

    let fallback_model = dominant_real_model(&parsed.turns);
    let mut last_real_model: Option<Arc<str>> = None;
    let mut priced_turns: Vec<TurnRecord> = parsed.turns.to_vec();

    for t in &mut priced_turns {
        let eff_model: &str = if !t.is_checkpoint
            && !is_compaction_model(&t.model)
            && t.model.as_ref() != "unknown"
        {
            last_real_model = Some(Arc::clone(&t.model));
            &t.model
        } else if !is_compaction_model(&t.model) && t.model.as_ref() != "unknown" {
            &t.model
        } else if let Some(prev) = last_real_model.as_deref() {
            prev
        } else {
            fallback_model.as_deref().unwrap_or(&t.model)
        };

        let ts_local = t.timestamp_utc.map(|u| tz_cache.to_local(u));
        let local_date = ts_local.map(|dt| dt.date_naive());
        t.cost = price_turn_record(pricing, eff_model, t, local_date);

        counts.add_call_with_cost(
            t.input_tokens,
            t.cache_read_tokens,
            t.cache_write_tokens,
            t.output_tokens,
            t.cost,
        );
        if !models.contains(t.model.as_ref()) {
            models.insert(t.model.to_string());
        }
        if t.is_checkpoint {
            compaction_count = compaction_count.saturating_add(1);
        }
        if t.has_thinking() {
            thinking_turns = thinking_turns.saturating_add(1);
        }
        if let Some(ts_l) = ts_local {
            first_time = Some(match first_time {
                Some(prev) if prev <= ts_l => prev,
                _ => ts_l,
            });
            last_time = Some(match last_time {
                Some(prev) if prev >= ts_l => prev,
                _ => ts_l,
            });
        }
    }

    let primary_model = compute_primary_model(&priced_turns);
    let active_duration_secs = compute_active_duration_secs(&priced_turns);

    SessionInfo {
        conversation_id: parsed.conversation_id,
        title: parsed.title,
        workspace: parsed.workspace,
        source: parsed.source,
        is_archived: false,
        is_subagent: parsed.is_subagent,
        parent_id: parsed.parent_id,
        parent_title: parsed.parent_title,
        turns: priced_turns.into(),
        counts,
        models,
        first_time,
        last_time,
        last_file_mtime_utc: parsed.last_file_mtime_utc,
        is_running_hint: parsed.is_running_hint,
        primary_model,
        active_duration_secs,
        compaction_count,
        thinking_turns,
    }
}

fn reprice_session_info(
    sess: &mut SessionInfo,
    pricing: &PricingCatalog,
    tz_cache: &mut LocalTzCache,
) {
    let fallback_model = dominant_real_model(&sess.turns);
    let mut last_real_model: Option<Arc<str>> = None;
    let mut priced_turns: Vec<TurnRecord> = sess.turns.to_vec();
    let mut counts = TokenCounts::default();

    for t in &mut priced_turns {
        let eff_model: &str = if !t.is_checkpoint
            && !is_compaction_model(&t.model)
            && t.model.as_ref() != "unknown"
        {
            last_real_model = Some(Arc::clone(&t.model));
            &t.model
        } else if !is_compaction_model(&t.model) && t.model.as_ref() != "unknown" {
            &t.model
        } else if let Some(prev) = last_real_model.as_deref() {
            prev
        } else {
            fallback_model.as_deref().unwrap_or(&t.model)
        };
        let local_date = t.timestamp_utc.map(|u| tz_cache.to_local(u).date_naive());
        t.cost = price_turn_record(pricing, eff_model, t, local_date);
        counts.add_call_with_cost(
            t.input_tokens,
            t.cache_read_tokens,
            t.cache_write_tokens,
            t.output_tokens,
            t.cost,
        );
    }

    sess.turns = priced_turns.into();
    sess.counts = counts;
}

fn dominant_real_model(turns: &[TurnRecord]) -> Option<Arc<str>> {
    let mut by_m: HashMap<&str, u64> = HashMap::new();
    for t in turns {
        if t.is_checkpoint || is_compaction_model(&t.model) || t.model.as_ref() == "unknown" {
            continue;
        }
        let entry = by_m.entry(t.model.as_ref()).or_insert(0);
        *entry = entry.saturating_add(t.total_input() + t.output_tokens);
    }
    by_m.into_iter()
        .max_by(|(na, ta), (nb, tb)| ta.cmp(tb).then_with(|| na.cmp(nb)))
        .map(|(m, _)| intern_str(m))
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
    let mut tz_cache = LocalTzCache::new();
    let mut filtered = Vec::new();
    for s in &snapshot.sessions {
        // O(1) disjoint session prune via cached (first_time, last_time).
        let (Some(s_first), Some(s_last)) = (s.first_time, s.last_time) else {
            continue;
        };
        let first_d = s_first.date_naive();
        let last_d = s_last.date_naive();
        if since.is_some_and(|min_d| last_d < min_d) {
            continue;
        }
        if until.is_some_and(|max_d| first_d > max_d) {
            continue;
        }

        // O(1) whole-session Arc reuse when the entire session lies within [since, until].
        let wholly_in_range = since.map_or(true, |min_d| first_d >= min_d)
            && until.map_or(true, |max_d| last_d <= max_d);
        if wholly_in_range && s.turns.iter().all(|t| t.timestamp_utc.is_some()) {
            filtered.push(s.clone());
            continue;
        }

        let mut turns = Vec::new();
        let mut counts = TokenCounts::default();
        let mut models = BTreeSet::new();
        let mut first_time: Option<DateTime<Local>> = None;
        let mut last_time: Option<DateTime<Local>> = None;
        let mut compaction_count: u32 = 0;
        let mut thinking_turns: u32 = 0;

        for t in s.turns.iter() {
            let Some(ts_utc) = t.timestamp_utc else { continue };
            let ts_local = tz_cache.to_local(ts_utc);
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
            if !models.contains(t.model.as_ref()) {
                models.insert(t.model.to_string());
            }
            if t.is_checkpoint {
                compaction_count = compaction_count.saturating_add(1);
            }
            if t.has_thinking() {
                thinking_turns = thinking_turns.saturating_add(1);
            }
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
            let primary_model = compute_primary_model(&turns);
            let active_duration_secs = compute_active_duration_secs(&turns);
            filtered.push(SessionInfo {
                conversation_id: s.conversation_id.clone(),
                title: s.title.clone(),
                workspace: s.workspace.clone(),
                source: s.source,
                is_archived: s.is_archived,
                is_subagent: s.is_subagent,
                parent_id: s.parent_id.clone(),
                parent_title: s.parent_title.clone(),
                turns: turns.into(),
                counts,
                models,
                first_time,
                last_time,
                last_file_mtime_utc: s.last_file_mtime_utc,
                is_running_hint: s.is_running_hint,
                primary_model,
                active_duration_secs,
                compaction_count,
                thinking_turns,
            });
        }
    }
    filtered.sort_by(|a, b| b.last_time.cmp(&a.last_time));
    let agg = aggregate_sessions(&filtered);
    (filtered, agg)
}

pub fn aggregate_sessions(sessions: &[SessionInfo]) -> AggregatedStats {
    let mut agg = AggregatedStats::default();
    let mut tz_cache = LocalTzCache::new();
    let mut by_date: BTreeMap<NaiveDate, TokenCounts> = BTreeMap::new();

    for s in sessions {
        agg.total.merge(&s.counts);
        agg.total_active_secs = agg.total_active_secs.saturating_add(s.active_duration_secs());
        if let Some(ws) = agg.by_workspace.get_mut(s.workspace.as_str()) {
            ws.0 += 1;
            ws.1.merge(&s.counts);
        } else {
            agg.by_workspace.insert(s.workspace.clone(), (1, s.counts));
        }
        let src = agg
            .by_source
            .entry(s.source)
            .or_insert((0, TokenCounts::default()));
        src.0 += 1;
        src.1.merge(&s.counts);
        let ag = agg
            .by_agent
            .entry(s.source.agent_family())
            .or_insert((0, TokenCounts::default()));
        ag.0 += 1;
        ag.1.merge(&s.counts);

        for (idx, t) in s.turns.iter().enumerate() {
            if t.has_thinking() {
                agg.total_thinking_turns += 1;
            }
            for tool in t.tool_names.iter() {
                if let Some(cnt) = agg.by_tool.get_mut(tool.as_ref()) {
                    *cnt += 1;
                } else {
                    agg.by_tool.insert(tool.to_string(), 1);
                }
            }
            let model_str = t.model.as_ref();
            if let Some(mc) = agg.by_model.get_mut(model_str) {
                mc.add_call_with_cost(
                    t.input_tokens,
                    t.cache_read_tokens,
                    t.cache_write_tokens,
                    t.output_tokens,
                    t.cost,
                );
            } else {
                let mut tc = TokenCounts::default();
                tc.add_call_with_cost(
                    t.input_tokens,
                    t.cache_read_tokens,
                    t.cache_write_tokens,
                    t.output_tokens,
                    t.cost,
                );
                agg.by_model.insert(model_str.to_string(), tc);
            }

            let mcs = if let Some(m) = agg.by_model_cache.get_mut(model_str) {
                m
            } else {
                agg.by_model_cache
                    .entry(model_str.to_string())
                    .or_default()
            };
            if t.output_tps().is_some() {
                mcs.speed_calls += 1;
                mcs.speed_output_tokens = mcs.speed_output_tokens.saturating_add(t.output_tokens);
                mcs.speed_gen_secs =
                    mcs.speed_gen_secs.saturating_add(t.gen_secs.unwrap_or(0) as u64);
            }
            if idx > 0 && !t.is_checkpoint {
                let ctx = t.total_input();
                mcs.multi_turn_calls += 1;
                mcs.multi_turn_input = mcs.multi_turn_input.saturating_add(ctx);
                mcs.multi_turn_cache_read =
                    mcs.multi_turn_cache_read.saturating_add(t.cache_read_tokens);
                if ctx >= 30_000 && t.cache_hit_rate() < 50.0 {
                    mcs.miss_count += 1;
                    mcs.miss_uncached_tokens =
                        mcs.miss_uncached_tokens.saturating_add(t.input_tokens);
                }
            }

            if let Some(ts_utc) = t.timestamp_utc {
                let ts = tz_cache.to_local(ts_utc);
                let d = ts.date_naive();
                let hour = (ts.hour() as usize).min(23);

                by_date.entry(d).or_default().add_call_with_cost(
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

    // Format day / ISO-week / month strings once per unique NaiveDate (~30 vs 50,000+ allocs).
    for (d, day_tc) in by_date {
        let day_key = format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day());
        let wk_key = iso_week_key(d);
        let month_key = format!("{:04}-{:02}", d.year(), d.month());

        agg.by_day.insert(day_key, day_tc);
        if let Some(wk) = agg.by_week.get_mut(&wk_key) {
            wk.merge(&day_tc);
        } else {
            agg.by_week.insert(wk_key.clone(), day_tc);
        }
        *agg.week_active_days.entry(wk_key).or_insert(0) += 1;
        if let Some(mo) = agg.by_month.get_mut(&month_key) {
            mo.merge(&day_tc);
        } else {
            agg.by_month.insert(month_key, day_tc);
        }
    }

    agg
}

pub fn iso_week_key(d: NaiveDate) -> String {
    let iso = d.iso_week();
    format!("{:04}-W{:02}", iso.year(), iso.week())
}

pub fn iso_week_bounds(week_key: &str) -> Option<(NaiveDate, NaiveDate)> {
    let (y_str, w_str) = week_key.split_once("-W")?;
    let year = y_str.parse::<i32>().ok()?;
    let week = w_str.parse::<u32>().ok()?;
    let mon = NaiveDate::from_isoywd_opt(year, week, chrono::Weekday::Mon)?;
    let sun = NaiveDate::from_isoywd_opt(year, week, chrono::Weekday::Sun)?;
    Some((mon, sun))
}

pub fn iso_week_range_label(week_key: &str) -> String {
    match iso_week_bounds(week_key) {
        Some((m, s)) => format!("{}/{} - {}/{}", m.month(), m.day(), s.month(), s.day()),
        None => week_key.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::decode_file_uri;
    use std::fs;

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
    fn test_source_kind_agent_family_and_path_inference() {
        for (kind, s, fam, fam_lbl) in [
            (SourceKind::Cli, "cli", AgentFamily::Antigravity, "Antigravity"),
            (SourceKind::App, "app", AgentFamily::Antigravity, "Antigravity"),
            (SourceKind::Ide, "ide", AgentFamily::Antigravity, "Antigravity"),
            (SourceKind::ClaudeCode, "claude-code", AgentFamily::ClaudeCode, "Claude Code"),
            (SourceKind::Codex, "codex", AgentFamily::Codex, "Codex CLI"),
            (SourceKind::GeminiCli, "gemini-cli", AgentFamily::GeminiCli, "Gemini CLI"),
            (SourceKind::Custom, "custom", AgentFamily::Antigravity, "Antigravity"),
        ] {
            assert_eq!(kind.as_str(), s);
            assert_eq!(SourceKind::from_str_lossy(s), kind);
            assert_eq!(kind.agent_family(), fam);
            assert_eq!(kind.family_label(), fam_lbl);
        }
        assert_eq!(
            SourceKind::infer_from_path(Path::new(r"C:\Users\alice\.claude")),
            SourceKind::ClaudeCode
        );
        assert_eq!(
            SourceKind::infer_from_path(Path::new(r"C:\Users\alice\.codex")),
            SourceKind::Codex
        );
        assert_eq!(
            SourceKind::infer_from_path(Path::new(r"C:\Users\alice\.gemini\tmp")),
            SourceKind::GeminiCli
        );
        assert_eq!(
            SourceKind::infer_from_path(Path::new(r"C:\Users\alice\.gemini\antigravity-cli")),
            SourceKind::Cli
        );
    }

    #[test]
    fn test_reasoning_tokens_has_thinking_and_by_agent_aggregation() {
        let mk_turn = |model: &str, cw: u64, cw1h: u64, out: u64, rt: u64, th: u64| TurnRecord {
            step_index: 1,
            timestamp_utc: None,
            model: intern_str(model),
            is_checkpoint: false,
            input_tokens: 1_000,
            cache_read_tokens: 0,
            cache_write_tokens: cw,
            cache_write_1h_tokens: cw1h,
            output_tokens: out,
            reasoning_tokens: rt,
            tool_names: Box::new([]),
            thinking_chars: th,
            gen_secs: None,
            cost: CostBreakdown::default(),
        };

        let t_codex = mk_turn("gpt-5", 0, 0, 800, 500, 0);
        assert!(t_codex.has_thinking());
        let t_plain = mk_turn("gpt-5", 0, 0, 200, 0, 0);
        assert!(!t_plain.has_thinking());

        // Claude 1h cache write pricing: 1M 5m @ $5.00 + 1M 1h @ $8.00 (2x $4.00 base) = $13.00 cache_write
        let cat = PricingCatalog::new(None);
        let t_claude = mk_turn("claude-opus-5-5", 2_000_000, 1_000_000, 0, 0, 0);
        let c = price_turn_record(&cat, "claude-opus-5-5", &t_claude, None);
        assert!((c.cache_write_usd - 13.0).abs() < 1e-6);

        let mut s_agy = SessionInfo {
            conversation_id: "same-id".into(),
            title: "agy".into(),
            workspace: String::new(),
            source: SourceKind::Cli,
            is_archived: false,
            is_subagent: false,
            parent_id: None,
            parent_title: String::new(),
            turns: vec![t_plain].into(),
            counts: TokenCounts::default(),
            models: BTreeSet::new(),
            first_time: None,
            last_time: None,
            last_file_mtime_utc: None,
            is_running_hint: false,
            primary_model: intern_str("-"),
            active_duration_secs: 0,
            compaction_count: 0,
            thinking_turns: 0,
        };
        s_agy.recompute_summary();
        s_agy.counts.add_call(1_000, 0, 0, 200);

        let mut s_codex = SessionInfo {
            conversation_id: "same-id".into(),
            title: "codex".into(),
            workspace: String::new(),
            source: SourceKind::Codex,
            is_archived: false,
            is_subagent: false,
            parent_id: None,
            parent_title: String::new(),
            turns: vec![t_codex].into(),
            counts: TokenCounts::default(),
            models: BTreeSet::new(),
            first_time: None,
            last_time: None,
            last_file_mtime_utc: None,
            is_running_hint: false,
            primary_model: intern_str("-"),
            active_duration_secs: 0,
            compaction_count: 0,
            thinking_turns: 0,
        };
        s_codex.recompute_summary();
        s_codex.counts.add_call(1_000, 0, 0, 800);

        assert_eq!(s_codex.thinking_turns(), 1);
        let agg = aggregate_sessions(&[s_agy, s_codex]);
        assert_eq!(agg.total_thinking_turns, 1);
        assert_eq!(agg.by_agent.len(), 2);
        assert_eq!(agg.by_agent[&AgentFamily::Antigravity].0, 1);
        assert_eq!(agg.by_agent[&AgentFamily::Codex].0, 1);
        assert_eq!(agg.by_agent[&AgentFamily::Codex].1.output_tokens, 800);
    }

    #[test]
    fn test_primary_model_and_workspace_short() {
        let mut s = SessionInfo {
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
                    model: intern_str("(上下文压缩)"),
                    is_checkpoint: true,
                    input_tokens: 5_000,
                    cache_read_tokens: 500_000,
                    cache_write_tokens: 0,
                    cache_write_1h_tokens: 0,
                    output_tokens: 6_000,
                    reasoning_tokens: 0,
                    tool_names: Box::new([]),
                    thinking_chars: 0,
                    gen_secs: None,
                    cost: CostBreakdown::default(),
                },
                TurnRecord {
                    step_index: 2,
                    timestamp_utc: None,
                    model: intern_str("gemini-4-neon-xhigh"),
                    is_checkpoint: false,
                    input_tokens: 1_000,
                    cache_read_tokens: 100_000,
                    cache_write_tokens: 0,
                    cache_write_1h_tokens: 0,
                    output_tokens: 500,
                    reasoning_tokens: 0,
                    tool_names: vec![intern_str("read_file")].into_boxed_slice(),
                    thinking_chars: 80,
                    gen_secs: None,
                    cost: CostBreakdown::default(),
                },
                TurnRecord {
                    step_index: 3,
                    timestamp_utc: None,
                    model: intern_str("gemini-4-neon-high"),
                    is_checkpoint: false,
                    input_tokens: 500,
                    cache_read_tokens: 10_000,
                    cache_write_tokens: 0,
                    cache_write_1h_tokens: 0,
                    output_tokens: 200,
                    reasoning_tokens: 0,
                    tool_names: Box::new([]),
                    thinking_chars: 0,
                    gen_secs: None,
                    cost: CostBreakdown::default(),
                },
            ]
            .into(),
            counts: TokenCounts::default(),
            models: BTreeSet::new(),
            first_time: None,
            last_time: None,
            last_file_mtime_utc: None,
            is_running_hint: false,
            primary_model: intern_str("-"),
            active_duration_secs: 0,
            compaction_count: 0,
            thinking_turns: 0,
        };
        s.recompute_summary();
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
        let mut s = SessionInfo {
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
                    model: intern_str("m"),
                    is_checkpoint: false,
                    input_tokens: 40_000,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    cache_write_1h_tokens: 0,
                    output_tokens: 500,
                    reasoning_tokens: 0,
                    tool_names: Box::new([]),
                    thinking_chars: 0,
                    gen_secs: None,
                    cost: CostBreakdown::default(),
                },
                // Normal high hit
                TurnRecord {
                    step_index: 2,
                    timestamp_utc: None,
                    model: intern_str("m"),
                    is_checkpoint: false,
                    input_tokens: 2_000,
                    cache_read_tokens: 40_000,
                    cache_write_tokens: 0,
                    cache_write_1h_tokens: 0,
                    output_tokens: 500,
                    reasoning_tokens: 0,
                    tool_names: Box::new([]),
                    thinking_chars: 0,
                    gen_secs: None,
                    cost: CostBreakdown::default(),
                },
                // Big cache miss (>=30K ctx, <50% hit)
                TurnRecord {
                    step_index: 3,
                    timestamp_utc: None,
                    model: intern_str("m"),
                    is_checkpoint: false,
                    input_tokens: 180_000,
                    cache_read_tokens: 5_000,
                    cache_write_tokens: 0,
                    cache_write_1h_tokens: 0,
                    output_tokens: 800,
                    reasoning_tokens: 0,
                    tool_names: Box::new([]),
                    thinking_chars: 0,
                    gen_secs: None,
                    cost: CostBreakdown::default(),
                },
            ]
            .into(),
            counts: TokenCounts::default(),
            models: BTreeSet::new(),
            first_time: None,
            last_time: None,
            last_file_mtime_utc: None,
            is_running_hint: false,
            primary_model: intern_str("-"),
            active_duration_secs: 0,
            compaction_count: 0,
            thinking_turns: 0,
        };
        s.recompute_summary();
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
    fn test_generation_time_and_speed_from_tool_step_timestamps() {
        let base = std::env::temp_dir().join(format!("agy_gauge_speed_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let logs = base
            .join("brain")
            .join("sess-s")
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&logs).unwrap();
        let jsonl = [
            // Step 1: 2 tool calls; tools stream out at +4 s and +10 s -> gen 10 s, 1000 out -> 100 tok/s
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-22T01:00:00Z","model":"m-fast","input_tokens":10,"cache_read_tokens":0,"output_tokens":1000,"tool_calls":[{"name":"a"},{"name":"b"}]}"#,
            r#"{"step_index":2,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-11-22T01:00:04Z","content":"{\"step_index\":9,\"type\":\"GENERIC\",\"created_at\":\"2030-01-01T00:00:00Z\"}"}"#,
            r#"{"step_index":3,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-11-22T01:00:10Z","content":"ok"}"#,
            // Step 4: one tool, gen 2 s -> duration known but too short for a speed estimate
            r#"{"step_index":4,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-22T01:00:20Z","model":"m-fast","input_tokens":10,"cache_read_tokens":0,"output_tokens":500,"tool_calls":[{"name":"a"}]}"#,
            r#"{"step_index":5,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-11-22T01:00:22Z","content":"ok"}"#,
            // Step 6: tool step missing (interleaved error) -> unmeasurable
            r#"{"step_index":6,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-22T01:01:00Z","model":"m-fast","input_tokens":10,"cache_read_tokens":0,"output_tokens":800,"tool_calls":[{"name":"a"}]}"#,
            r#"{"step_index":7,"source":"SYSTEM","type":"ERROR_MESSAGE","status":"DONE","error":"503","created_at":"2026-11-22T01:01:30Z"}"#,
            // Step 8: pure text reply -> no end marker
            r#"{"step_index":8,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-22T01:02:00Z","model":"m-fast","input_tokens":10,"cache_read_tokens":0,"output_tokens":300,"content":"done"}"#,
        ]
        .join("\n");
        fs::write(logs.join("transcript.jsonl"), jsonl).unwrap();

        let mut scanner = IncrementalScanner::new_with_pricing(base.clone(), None);
        let snap = scanner.scan();
        let turns = &snap.sessions[0].turns;
        let by_step = |s: i64| turns.iter().find(|t| t.step_index == s).unwrap();
        assert_eq!(by_step(1).gen_secs, Some(10));
        assert_eq!(by_step(1).output_tps(), Some(100.0));
        assert_eq!(by_step(4).gen_secs, Some(2));
        assert_eq!(by_step(4).output_tps(), None);
        assert_eq!(by_step(6).gen_secs, None);
        assert_eq!(by_step(8).gen_secs, None);

        // Per-model speed is token-weighted over measurable calls only
        let mcs = snap.agg.by_model_cache.get("m-fast").unwrap();
        assert_eq!(mcs.speed_calls, 1);
        assert_eq!(mcs.output_tps(), Some(100.0));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_subagent_parent_linking_and_workspace_inheritance() {
        let base = std::env::temp_dir().join(format!("agy_gauge_subagent_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let parent_id = "8c1095e2-1558-4a06-bfde-446204f978aa";
        let child_id = "14ee4713-eb4d-4a3c-b6a9-6745e2e553e2";
        let p_logs = base
            .join("brain")
            .join(parent_id)
            .join(".system_generated")
            .join("logs");
        let c_logs = base
            .join("brain")
            .join(child_id)
            .join(".system_generated")
            .join("logs");
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
        let parent = snap
            .sessions
            .iter()
            .find(|s| s.conversation_id == parent_id)
            .unwrap();
        assert!(!parent.is_subagent);
        assert!(parent.parent_id.is_none());
        let child = snap
            .sessions
            .iter()
            .find(|s| s.conversation_id == child_id)
            .unwrap();
        assert!(child.is_subagent);
        assert_eq!(child.parent_id.as_deref(), Some(parent_id));
        assert_eq!(child.parent_title, "Subvault Review");
        assert_eq!(child.workspace, "C:/Users/qingz/agent-home");
        assert_eq!(child.title, "You are executing Wave 2 · Track A for SubVault.");
        // Child's tokens are aggregated under the inherited parent workspace, not "(未知工作区)"
        assert!(!snap.agg.by_workspace.contains_key("(未知工作区)"));
        assert_eq!(
            snap.agg
                .by_workspace
                .get("C:/Users/qingz/agent-home")
                .unwrap()
                .0,
            2
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_swarm_worker_board_ping_upgrades_to_step1_assignment_without_meta() {
        let base = std::env::temp_dir().join(format!(
            "agy_gauge_swarm_worker_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let parent_id = "ab159572-c4b8-4bd9-86e4-6b4bb34a5129";
        let worker_sys_id = "a6330233-a6c5-4e24-82dc-24e1795519fe";
        let worker_peer_id = "00b0f7b6-2eb8-40c6-ab17-8382ceef722e";
        let other_worker_id = "def33a67-8eec-4265-be5b-85ee403d56d4";
        let worker_early_id = "11e43752-b1f0-4710-9427-1c78a6b3b92b";

        for cid in [parent_id, worker_sys_id, worker_peer_id, worker_early_id] {
            fs::create_dir_all(
                base.join("brain")
                    .join(cid)
                    .join(".system_generated")
                    .join("logs"),
            )
            .unwrap();
        }
        let cache = base.join("cache");
        fs::create_dir_all(&cache).unwrap();
        // Deliberately omit all swarm workers from conversation_metadata.json to simulate
        // live runs before agy flushes metadata cache.
        fs::write(
            cache.join("conversation_metadata.json"),
            format!(
                r#"{{"conversations":{{"{parent_id}":{{"summary":{{"Title":"Swarm Coordinator","WorkspaceURIs":["file:///C:/Users/qingz/agent-home"]}}}}}}}}"#
            ),
        )
        .unwrap();

        fs::write(
            base.join("brain")
                .join(parent_id)
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl"),
            concat!(
                r#"{"step_index":0,"type":"USER_INPUT","source":"USER_EXPLICIT","status":"DONE","content":"<USER_REQUEST>\n/swarm audit repos\n</USER_REQUEST>"}"#,
                "\n",
                r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-25T16:38:00Z","model":"gemini-4-neon-xhigh","input_tokens":1000,"cache_read_tokens":0,"output_tokens":120}"#,
                "\n"
            ),
        )
        .unwrap();

        // Worker 1: step=0 sender=system board ping, step=1 real assignment with coordinator UUID & @verifier
        let w1 = [
            r#"{"step_index":0,"type":"SYSTEM_MESSAGE","source":"SYSTEM","status":"DONE","content":"The following is a <SYSTEM_MESSAGE>\n\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-25T16:38:11Z sender=system priority=MESSAGE_PRIORITY_HIGH content=[swarm] new board message #5\n</SYSTEM_MESSAGE>"}"#.to_string(),
            format!(
                r#"{{"step_index":1,"type":"SYSTEM_MESSAGE","source":"SYSTEM","status":"DONE","content":"The following is a <SYSTEM_MESSAGE>\n\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-25T16:38:11Z sender={parent_id} priority=MESSAGE_PRIORITY_HIGH content=You have just started. Your assignment (recorded on the board as #5 for @verifier):\n\nYou are the **independent** `verifier` for all repos.\n</SYSTEM_MESSAGE>"}}"#
            ),
            r#"{"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-25T16:38:16Z","model":"gemini-4-neon-xhigh","input_tokens":1500,"cache_read_tokens":2000,"output_tokens":200}"#.to_string(),
        ]
        .join("\n");
        fs::write(
            base.join("brain")
                .join(worker_sys_id)
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl"),
            format!("{w1}\n"),
        )
        .unwrap();

        // Worker 2: step=0 sender=<other-worker-uuid> board ping, step=1 upgrades to root coordinator UUID
        let w2 = [
            format!(
                r#"{{"step_index":0,"type":"SYSTEM_MESSAGE","source":"SYSTEM","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-23T18:40:58Z sender={other_worker_id} priority=MESSAGE_PRIORITY_HIGH content=[swarm] new board message #2\n</SYSTEM_MESSAGE>"}}"#
            ),
            format!(
                r#"{{"step_index":1,"type":"SYSTEM_MESSAGE","source":"SYSTEM","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-23T18:40:58Z sender={parent_id} priority=MESSAGE_PRIORITY_HIGH content=You have just started. Your assignment (recorded on the board as #2 for @stack-bench):\n\n# Benchmark Engineer\n</SYSTEM_MESSAGE>"}}"#
            ),
            r#"{"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-23T18:41:05Z","model":"gemini-4-neon-xhigh","input_tokens":800,"cache_read_tokens":1200,"output_tokens":100}"#.to_string(),
        ]
        .join("\n");
        fs::write(
            base.join("brain")
                .join(worker_peer_id)
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl"),
            format!("{w2}\n"),
        )
        .unwrap();

        // Worker 3: only step=0 board ping before first turn (no meta, no step=1) -> still subagent, never "(未命名会话)"
        let w3 = [
            r#"{"step_index":0,"type":"SYSTEM_MESSAGE","source":"SYSTEM","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-25T16:38:12Z sender=system priority=MESSAGE_PRIORITY_HIGH content=[swarm] new board message #6\n</SYSTEM_MESSAGE>"}"#,
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-25T16:38:19Z","model":"gemini-4-neon-xhigh","input_tokens":500,"cache_read_tokens":0,"output_tokens":60}"#,
            // Later runtime SYSTEM_MESSAGE after first turn must NOT hijack title or parent
            r#"{"step_index":2,"type":"SYSTEM_MESSAGE","source":"SYSTEM","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-25T16:41:00Z sender=00000000-0000-0000-0000-000000000000 priority=MESSAGE_PRIORITY_HIGH content=[monitor] tick\n</SYSTEM_MESSAGE>"}"#,
        ]
        .join("\n");
        fs::write(
            base.join("brain")
                .join(worker_early_id)
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl"),
            format!("{w3}\n"),
        )
        .unwrap();

        let mut scanner = IncrementalScanner::new_with_pricing(base.clone(), None);
        let snap = scanner.scan();
        assert_eq!(snap.sessions.len(), 4);

        let s1 = snap.sessions.iter().find(|s| s.conversation_id == worker_sys_id).unwrap();
        assert!(s1.is_subagent);
        assert_eq!(s1.parent_id.as_deref(), Some(parent_id));
        assert_eq!(s1.parent_title, "Swarm Coordinator");
        assert_eq!(s1.workspace, "C:/Users/qingz/agent-home");
        assert_eq!(s1.title, "@verifier · You are the independent verifier for all repos.");

        let s2 = snap.sessions.iter().find(|s| s.conversation_id == worker_peer_id).unwrap();
        assert!(s2.is_subagent);
        assert_eq!(s2.parent_id.as_deref(), Some(parent_id));
        assert_eq!(s2.workspace, "C:/Users/qingz/agent-home");
        assert_eq!(s2.title, "@stack-bench · Benchmark Engineer");

        let s3 = snap.sessions.iter().find(|s| s.conversation_id == worker_early_id).unwrap();
        assert!(s3.is_subagent);
        assert!(s3.parent_id.is_none());
        assert_eq!(s3.title, "[swarm] new board message #6");
        assert_ne!(s3.title, "(未命名会话)");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_multi_source_tools_thinking_and_custom_range() {
        let base = std::env::temp_dir().join(format!("agy_gauge_multi_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let cli_dir = base.join("antigravity-cli");
        let app_dir = base.join("antigravity");
        let s1_logs = cli_dir
            .join("brain")
            .join("sess-cli")
            .join(".system_generated")
            .join("logs");
        let s2_logs = app_dir
            .join("brain")
            .join("sess-app")
            .join(".system_generated")
            .join("logs");
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
        assert_eq!(snap.agg.by_agent.len(), 1);
        assert_eq!(snap.agg.by_agent[&AgentFamily::Antigravity].0, 2);
        assert!(!snap.has_non_antigravity());
        assert_eq!(snap.agg.by_tool.get("read_file"), Some(&2));
        assert_eq!(snap.agg.by_tool.get("run_command"), Some(&1));
        assert_eq!(snap.agg.total_thinking_turns, 1);

        let cli_s = snap
            .sessions
            .iter()
            .find(|s| s.conversation_id == "sess-cli")
            .unwrap();
        assert_eq!(cli_s.source, SourceKind::Cli);
        assert_eq!(cli_s.active_duration_secs(), 120);

        let d_only = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        let (filtered, f_agg) =
            filter_snapshot_range(&snap, DateScope::Custom, Some(d_only), Some(d_only));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].conversation_id, "sess-cli");
        assert_eq!(f_agg.by_tool.get("read_file"), Some(&2));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_compute_running_hint_thresholds() {
        let now = DateTime::parse_from_rfc3339("2026-11-24T05:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t_30s = now - chrono::Duration::seconds(30);
        let t_120s = now - chrono::Duration::seconds(120);
        let t_300s = now - chrono::Duration::seconds(300);

        assert!(compute_running_hint(true, Some(t_30s), now));
        assert!(compute_running_hint(false, Some(t_30s), now)); // within ACTIVE_MTIME_WINDOW_SECS
        assert!(compute_running_hint(true, Some(t_120s), now)); // RUNNING within RUNNING_STEP_STALE_SECS
        assert!(!compute_running_hint(false, Some(t_120s), now));
        assert!(!compute_running_hint(true, Some(t_300s), now)); // crashed/stale RUNNING > 180s
        assert!(!compute_running_hint(true, None, now));
    }

    #[test]
    fn test_scanner_running_hint_before_fast_path_and_session_meta_integration() {
        let base =
            std::env::temp_dir().join(format!("agy_gauge_meta_run_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let cid = "99999999-aaaa-bbbb-cccc-000000000001";
        let logs = base
            .join("brain")
            .join(cid)
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&logs).unwrap();

        // Trailing GENERIC line has NO `_tokens` (skipped by fast-path) and `status: RUNNING`
        let jsonl = [
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-24T04:00:00Z","model":"gemini-4-neon-xhigh","input_tokens":1000,"cache_read_tokens":2000,"output_tokens":300,"tool_calls":[{"name":"run_command"}]}"#,
            r#"{"step_index":2,"source":"MODEL","type":"GENERIC","status":"RUNNING","created_at":"2026-11-24T04:00:05Z","content":"running tool"}"#,
        ]
        .join("\n");
        let t_path = logs.join("transcript.jsonl");
        fs::write(&t_path, format!("{jsonl}\n")).unwrap();

        let meta_path = base.join("session_meta.json");
        let mut scanner = IncrementalScanner::new_with_pricing(base.clone(), None)
            .with_session_meta(Some(meta_path.clone()));

        let snap1 = scanner.scan();
        assert_eq!(snap1.sessions.len(), 1);
        assert!(snap1.sessions[0].last_file_mtime_utc.is_some());
        assert!(snap1.sessions[0].is_running_hint);
        assert!(snap1.session_meta.is_empty());
        assert!(!snap1.session_meta_changed);
        assert_eq!(snap1.session_meta_path.as_deref(), Some(meta_path.as_path()));

        // Second scan hits incremental file cache cleanly
        let snap_cached = scanner.scan();
        assert_eq!(snap_cached.files_cached, 1);
        assert_eq!(snap_cached.files_reparsed, 0);

        // Toggle pin via scanner helper -> next scan exposes updated session_meta & changed flag
        assert!(scanner.toggle_session_pin(cid).unwrap());
        let snap2 = scanner.scan();
        assert!(snap2.session_meta_changed);
        assert!(snap2.session_meta.get(cid).is_some_and(|m| m.pinned));

        // Subsequent scan with no changes reports session_meta_changed == false
        let snap3 = scanner.scan();
        assert!(!snap3.session_meta_changed);

        // Unpin auto-GCs the entry
        assert!(!scanner.toggle_session_pin(cid).unwrap());
        let snap4 = scanner.scan();
        assert!(snap4.session_meta_changed);
        assert!(snap4.session_meta.is_empty());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_local_tz_cache_and_intern_str() {
        let a = intern_str("gemini-4-neon-xhigh");
        let b = intern_str("gemini-4-neon-xhigh");
        assert!(Arc::ptr_eq(&a, &b));

        let mut tz = LocalTzCache::new();
        for ts_str in [
            "2026-01-15T00:00:00Z",
            "2026-03-08T09:59:59Z",
            "2026-03-08T10:00:00Z",
            "2026-07-01T18:30:45Z",
            "2026-11-01T08:30:00Z",
            "2026-11-27T04:15:12Z",
        ] {
            let utc = DateTime::parse_from_rfc3339(ts_str)
                .unwrap()
                .with_timezone(&Utc);
            assert_eq!(tz.to_local(utc), utc.with_timezone(&Local));
        }
    }

    #[test]
    fn test_scan_if_changed_noop_append_and_running_decay() {
        use std::io::Write;

        let base = std::env::temp_dir().join(format!(
            "agy_gauge_scan_delta_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let cid = "delta-sess-1";
        let logs = base
            .join("brain")
            .join(cid)
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&logs).unwrap();
        let t_path = logs.join("transcript.jsonl");
        let line1 = r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-26T04:00:00Z","model":"gemini-4-neon-xhigh","input_tokens":1000,"cache_read_tokens":2000,"output_tokens":300}"#;
        fs::write(&t_path, format!("{line1}\n")).unwrap();

        let meta_path = base.join("session_meta.json");
        let mut scanner = IncrementalScanner::new_with_pricing(base.clone(), None)
            .with_session_meta(Some(meta_path));

        let mut snap = scanner.scan();
        assert_eq!(snap.sessions.len(), 1);
        assert_eq!(snap.agg.total.calls, 1);
        let turns_ptr_before = Arc::as_ptr(&snap.sessions[0].turns);

        // Zero-change poll -> no-op delta, identical Arc pointer!
        let d_noop = scanner.scan_if_changed(&mut snap);
        assert_eq!(
            d_noop,
            ScanDelta {
                changed: false,
                running_changed: false,
                files_reparsed: 0,
            }
        );
        assert_eq!(Arc::as_ptr(&snap.sessions[0].turns), turns_ptr_before);

        // Pin session via session_meta -> updates in place without reparsing files!
        assert!(scanner.toggle_session_pin(cid).unwrap());
        let d_meta = scanner.scan_if_changed(&mut snap);
        assert!(d_meta.changed);
        assert_eq!(d_meta.files_reparsed, 0);
        assert!(snap.session_meta.get(cid).is_some_and(|m| m.pinned));
        assert_eq!(Arc::as_ptr(&snap.sessions[0].turns), turns_ptr_before);

        // Append turn 2 -> detected via byte-offset tailing, files_reparsed == 1.
        {
            let mut f = fs::OpenOptions::new().append(true).open(&t_path).unwrap();
            writeln!(
                f,
                r#"{{"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-26T04:01:00Z","model":"gemini-4-neon-xhigh","input_tokens":500,"cache_read_tokens":3000,"output_tokens":200}}"#
            )
            .unwrap();
        }
        let d_append = scanner.scan_if_changed(&mut snap);
        assert!(d_append.changed);
        assert_eq!(d_append.files_reparsed, 1);
        assert_eq!(snap.agg.total.calls, 2);
        assert_eq!(snap.sessions[0].active_duration_secs(), 60);

        // Simulate running-hint expiry in place without any file modification.
        assert!(snap.sessions[0].is_running_hint);
        scanner.active_running[0].mtime_utc =
            Some(Utc::now() - chrono::Duration::seconds(RUNNING_STEP_STALE_SECS + 60));
        let d_decay = scanner.scan_if_changed(&mut snap);
        assert!(!d_decay.changed);
        assert!(d_decay.running_changed);
        assert_eq!(d_decay.files_reparsed, 0);
        assert!(!snap.sessions[0].is_running_hint);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_archive_debounce_flush_and_watched_dirs() {
        use std::io::Write;

        let base = std::env::temp_dir().join(format!(
            "agy_gauge_debounce_watch_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("antigravity-cli");
        let logs = root
            .join("brain")
            .join("deb-sess")
            .join(".system_generated")
            .join("logs");
        let convs_sqlite_dir = root.join("conversations");
        let companion_dir = base.join("companion");
        fs::create_dir_all(&logs).unwrap();
        fs::create_dir_all(&convs_sqlite_dir).unwrap();
        fs::create_dir_all(&companion_dir).unwrap();

        let t_path = logs.join("transcript.jsonl");
        let arch_path = companion_dir.join("archive.json");
        let pricing_path = companion_dir.join("pricing.json");
        fs::write(
            &t_path,
            "{\"step_index\":1,\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"created_at\":\"2026-11-26T04:00:00Z\",\"model\":\"gemini-4-neon-xhigh\",\"input_tokens\":1000,\"cache_read_tokens\":0,\"output_tokens\":100}\n",
        )
        .unwrap();

        let mut scanner = IncrementalScanner::new_multi(
            vec![(SourceKind::Cli, root.clone())],
            Some(pricing_path),
            None,
            None,
            Some(arch_path.clone()),
            true,
        );

        // watched_dirs must include brain + companion dir, and NEVER conversations/ (SQLite WAL)!
        let watched = scanner.watched_dirs();
        assert!(watched.iter().any(|p| p == &root.join("brain")));
        assert!(watched.iter().any(|p| p == &companion_dir));
        assert!(!watched.iter().any(|p| p == &convs_sqlite_dir));

        // First dirty scan persists archive.json immediately.
        let mut snap = scanner.scan();
        assert_eq!(snap.agg.total.calls, 1);
        let raw1: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&arch_path).unwrap()).unwrap();
        assert_eq!(raw1["schema_version"], 1);
        assert_eq!(raw1["sessions"]["deb-sess"]["turns"].as_array().unwrap().len(), 1);

        // Append turn 2 immediately (<120s debounce): in-memory snapshot updates, disk write deferred!
        {
            let mut f = fs::OpenOptions::new().append(true).open(&t_path).unwrap();
            writeln!(
                f,
                r#"{{"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-26T04:02:00Z","model":"gemini-4-neon-xhigh","input_tokens":500,"cache_read_tokens":1000,"output_tokens":50}}"#
            )
            .unwrap();
        }
        let d = scanner.scan_if_changed(&mut snap);
        assert!(d.changed);
        assert_eq!(snap.agg.total.calls, 2);
        let raw2: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&arch_path).unwrap()).unwrap();
        assert_eq!(
            raw2["sessions"]["deb-sess"]["turns"].as_array().unwrap().len(),
            1,
            "second write within 120s should be debounced"
        );

        // Explicit flush_archive writes turn 2 immediately!
        scanner.flush_archive();
        let raw3: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&arch_path).unwrap()).unwrap();
        assert_eq!(raw3["sessions"]["deb-sess"]["turns"].as_array().unwrap().len(), 2);

        drop(scanner);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_filter_snapshot_range_pruning_and_arc_reuse() {
        let base = std::env::temp_dir().join(format!(
            "agy_gauge_filter_arc_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let s1_logs = base
            .join("brain")
            .join("sess-whole")
            .join(".system_generated")
            .join("logs");
        let s2_logs = base
            .join("brain")
            .join("sess-straddle")
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&s1_logs).unwrap();
        fs::create_dir_all(&s2_logs).unwrap();

        fs::write(
            s1_logs.join("transcript.jsonl"),
            "{\"step_index\":1,\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"created_at\":\"2026-11-20T06:00:00Z\",\"model\":\"gemini-4-neon-xhigh\",\"input_tokens\":1000,\"cache_read_tokens\":0,\"output_tokens\":200}\n",
        )
        .unwrap();
        fs::write(
            s2_logs.join("transcript.jsonl"),
            [
                r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-18T06:00:00Z","model":"claude-opus-5-5","input_tokens":500,"cache_read_tokens":0,"output_tokens":100}"#,
                r#"{"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-20T08:00:00Z","model":"gemini-4-neon-xhigh","input_tokens":800,"cache_read_tokens":0,"output_tokens":150,"thinking":"yes"}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        let mut scanner = IncrementalScanner::new_with_pricing(base.clone(), None);
        let snap = scanner.scan();
        let orig_whole = snap
            .sessions
            .iter()
            .find(|s| s.conversation_id == "sess-whole")
            .unwrap();

        let d20 = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        let (filtered, f_agg) =
            filter_snapshot_range(&snap, DateScope::Custom, Some(d20), Some(d20));
        assert_eq!(filtered.len(), 2);
        assert_eq!(f_agg.total.calls, 2);

        let f_whole = filtered
            .iter()
            .find(|s| s.conversation_id == "sess-whole")
            .unwrap();
        assert!(
            Arc::ptr_eq(&orig_whole.turns, &f_whole.turns),
            "wholly-in-range session should reuse Arc<[TurnRecord]> without cloning turns"
        );

        let f_straddle = filtered
            .iter()
            .find(|s| s.conversation_id == "sess-straddle")
            .unwrap();
        assert_eq!(f_straddle.turns.len(), 1);
        assert_eq!(f_straddle.primary_model(), "gemini-4-neon-xhigh");
        assert_eq!(f_straddle.thinking_turns(), 1);
        assert_eq!(f_straddle.active_duration_secs(), 15);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_scan_with_hints_single_file_companion_delete_overflow_and_expire_only() {
        use std::io::Write;

        let base = std::env::temp_dir().join(format!(
            "agy_gauge_hints_r2_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("antigravity-cli");
        let s1_logs = root
            .join("brain")
            .join("hint-s1")
            .join(".system_generated")
            .join("logs");
        let s2_logs = root
            .join("brain")
            .join("hint-s2")
            .join(".system_generated")
            .join("logs");
        let ann_dir = root.join("annotations");
        let comp_dir = base.join("companion");
        fs::create_dir_all(&s1_logs).unwrap();
        fs::create_dir_all(&s2_logs).unwrap();
        fs::create_dir_all(&ann_dir).unwrap();
        fs::create_dir_all(&comp_dir).unwrap();

        let t1 = s1_logs.join("transcript.jsonl");
        let t2 = s2_logs.join("transcript.jsonl");
        let sl_path = comp_dir.join("statusline_state.json");
        let meta_path = comp_dir.join("session_meta.json");

        fs::write(
            &t1,
            "{\"step_index\":1,\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"created_at\":\"2026-11-27T01:00:00Z\",\"model\":\"gemini-4-neon-xhigh\",\"input_tokens\":1000,\"cache_read_tokens\":0,\"output_tokens\":200}\n",
        )
        .unwrap();
        fs::write(
            &t2,
            "{\"step_index\":1,\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"created_at\":\"2026-11-27T01:05:00Z\",\"model\":\"claude-opus-5-5\",\"input_tokens\":500,\"cache_read_tokens\":0,\"output_tokens\":100}\n",
        )
        .unwrap();

        let mut scanner = IncrementalScanner::new_multi(
            vec![(SourceKind::Cli, root.clone())],
            None,
            Some(sl_path.clone()),
            None,
            None,
            false,
        )
        .with_session_meta(Some(meta_path));

        let mut snap = scanner.scan();
        assert_eq!(snap.sessions.len(), 2);
        assert_eq!(snap.agg.total.calls, 2);
        let s2_turns_ptr = Arc::as_ptr(
            &snap
                .sessions
                .iter()
                .find(|s| s.conversation_id == "hint-s2")
                .unwrap()
                .turns,
        );

        // 1) Companion-only hint (`has_companion_change: true`, empty `paths`):
        // refreshes statusline in place with 0 adapter scans!
        fs::write(
            &sl_path,
            r#"{"schema_version":1,"captured_at":"2026-11-27T01:10:00Z","payload":{"model":{"display_name":"Gemini 4 Neon"},"context_window":{"used_percentage":42.0},"quota":{}}}"#,
        )
        .unwrap();
        let d_comp = scanner.scan_with_hints(
            &mut snap,
            &ScanHints {
                paths: Vec::new(),
                overflow: false,
                has_companion_change: true,
            },
        );
        assert!(d_comp.changed);
        assert_eq!(d_comp.files_reparsed, 0);
        assert!(snap.statusline_changed);
        assert_eq!(
            snap.statusline.as_ref().map(|s| s.model_display.as_str()),
            Some("Gemini 4 Neon")
        );

        // 2) Targeted single-file append hint on s1 -> reparses ONLY s1 via AdapterPoll::Delta,
        // leaving s2's single shared priced Arc<[TurnRecord]> pointer untouched!
        {
            let mut f = fs::OpenOptions::new().append(true).open(&t1).unwrap();
            writeln!(
                f,
                r#"{{"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-27T01:12:00Z","model":"gemini-4-neon-xhigh","input_tokens":800,"cache_read_tokens":1000,"output_tokens":150}}"#
            )
            .unwrap();
        }
        let d_single = scanner.scan_with_hints(
            &mut snap,
            &ScanHints {
                paths: vec![t1.clone()],
                overflow: false,
                has_companion_change: false,
            },
        );
        assert!(d_single.changed);
        assert_eq!(d_single.files_reparsed, 1);
        assert_eq!(snap.agg.total.calls, 3);
        let s2_after_ptr = Arc::as_ptr(
            &snap
                .sessions
                .iter()
                .find(|s| s.conversation_id == "hint-s2")
                .unwrap()
                .turns,
        );
        assert_eq!(s2_turns_ptr, s2_after_ptr);

        // 3) Targeted annotation update on s1 -> updates title via Delta with 0 transcript reparses!
        let ann1 = ann_dir.join("hint-s1.pbtxt");
        fs::write(&ann1, "title:\"Hinted Title\"\n").unwrap();
        let d_ann = scanner.scan_with_hints(
            &mut snap,
            &ScanHints {
                paths: vec![ann1],
                overflow: false,
                has_companion_change: false,
            },
        );
        assert!(d_ann.changed);
        assert_eq!(d_ann.files_reparsed, 0);
        assert_eq!(
            snap.sessions
                .iter()
                .find(|s| s.conversation_id == "hint-s1")
                .unwrap()
                .title,
            "Hinted Title"
        );

        // 4) Zero-syscall expire_running_hints_only clears stale running hints in place!
        assert!(snap.sessions.iter().any(|s| s.is_running_hint));
        for e in &mut scanner.active_running {
            e.mtime_utc = Some(Utc::now() - chrono::Duration::seconds(RUNNING_STEP_STALE_SECS + 60));
        }
        assert!(scanner.expire_running_hints_only(&mut snap));
        assert!(snap.sessions.iter().all(|s| !s.is_running_hint));
        assert!(!scanner.expire_running_hints_only(&mut snap));

        // 5) Deletion hint falls back cleanly to full sweep and evicts deleted s2!
        fs::remove_file(&t2).unwrap();
        let d_del = scanner.scan_with_hints(
            &mut snap,
            &ScanHints {
                paths: vec![t2],
                overflow: false,
                has_companion_change: false,
            },
        );
        assert!(d_del.changed);
        assert_eq!(snap.sessions.len(), 1);
        assert_eq!(snap.sessions[0].conversation_id, "hint-s1");

        // 6) Overflow hint falls back cleanly to scan_if_changed (no-op when nothing changed).
        let d_ov = scanner.scan_with_hints(
            &mut snap,
            &ScanHints {
                paths: Vec::new(),
                overflow: true,
                has_companion_change: false,
            },
        );
        assert!(!d_ov.changed);
        assert_eq!(d_ov.files_reparsed, 0);

        let _ = fs::remove_dir_all(&base);
    }
}
