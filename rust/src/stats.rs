//! Strictly read-only multi-agent incremental scanner & aggregator for `Gauge`.
//!
//! Supported Tier-1 pure-text agents (via `crate::sources` adapters):
//!   - Antigravity (`agy` CLI / 2.0 App / IDE)
//!   - Claude Code (`~/.claude/projects`, `$CLAUDE_CONFIG_DIR`)
//!   - OpenAI Codex CLI (`~/.codex/sessions`, `$CODEX_HOME`, pure JSONL rollouts only)
//!   - Gemini CLI (`~/.gemini/tmp`, `$GEMINI_CLI_HOME`)
//!
//! Never writes inside agent directories, never touches SQLite databases, never reads credentials.

use chrono::{DateTime, Datelike, Local, NaiveDate, Timelike, Utc};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::archive::UsageArchive;
use crate::pricing::{CostBreakdown, ModelRate, PricingCatalog, PricingTableRow};
use crate::quota::{BudgetConfig, BudgetStore, StatuslineReader, StatuslineSnapshot};
use crate::session_meta::{SessionMetaStore, SessionUserMeta};
use crate::sources::{
    build_adapter, default_antigravity_cli_dir, discover_antigravity_sources,
    discover_default_sources, SourceAdapter,
};

pub use crate::sources::antigravity::parse_pbtxt_title;
pub use crate::sources::{
    compute_running_hint, SourceDiscoveryFilter, ACTIVE_MTIME_WINDOW_SECS, RUNNING_STEP_STALE_SECS,
};

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
    pub model: String,
    pub is_checkpoint: bool,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Optional subset of `cache_write_tokens` billed at 1h ephemeral TTL (2.0x base input; NOT additive).
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
    /// Optional informational subset of `output_tokens` representing reasoning/thoughts tokens (NOT additive).
    pub reasoning_tokens: u64,
    pub tool_names: Vec<String>,
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
    pub last_file_mtime_utc: Option<DateTime<Utc>>,
    pub is_running_hint: bool,
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
        self.turns.iter().filter(|t| t.has_thinking()).count()
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

pub struct IncrementalScanner {
    sources: Vec<(SourceKind, PathBuf)>,
    adapters: Vec<Box<dyn SourceAdapter>>,
    pricing: PricingCatalog,
    statusline: StatuslineReader,
    budget: BudgetStore,
    archive: UsageArchive,
    session_meta: SessionMetaStore,
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
        let adapters = sources
            .iter()
            .map(|(kind, path)| build_adapter(*kind, path.clone()))
            .collect();
        Self {
            sources,
            adapters,
            pricing: PricingCatalog::new(pricing_override),
            statusline: StatuslineReader::new(statusline_path),
            budget: BudgetStore::new(budget_path),
            archive: UsageArchive::new(archive_path, archive_enabled),
            session_meta: SessionMetaStore::new(None),
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
        self.pricing.upsert_custom_rule(model, rate)
    }

    pub fn remove_pricing_rule(&mut self, model: &str) -> Result<bool, String> {
        self.pricing.remove_custom_rule(model)
    }

    pub fn save_budget(&mut self, cfg: BudgetConfig) -> Result<(), String> {
        self.budget.save(cfg)
    }

    pub fn with_session_meta(mut self, session_meta_path: Option<PathBuf>) -> Self {
        self.session_meta = SessionMetaStore::new(session_meta_path);
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

    pub fn scan(&mut self) -> ScanSnapshot {
        let started = Instant::now();
        let scan_now_utc = Utc::now();
        let pricing_changed = self.pricing.refresh();
        let statusline_changed = self.statusline.refresh();
        let budget_changed = self.budget.refresh();
        let session_meta_changed = self.session_meta.refresh();

        let mut sessions_by_key: HashMap<(AgentFamily, String), SessionInfo> = HashMap::new();
        let mut skipped_empty = 0usize;
        let mut skipped_lines = 0usize;
        let mut files_cached = 0usize;
        let mut files_reparsed = 0usize;
        let mut found_any_root = false;

        for adapter in &mut self.adapters {
            let res = adapter.scan(scan_now_utc);
            if res.found_root {
                found_any_root = true;
            }
            skipped_empty += res.skipped_empty;
            skipped_lines += res.skipped_lines;
            files_cached += res.files_cached;
            files_reparsed += res.files_reparsed;

            for parsed in res.sessions {
                if parsed.turns.is_empty() {
                    skipped_empty += 1;
                    continue;
                }

                let mut counts = TokenCounts::default();
                let mut models = BTreeSet::new();
                let mut first_time: Option<DateTime<Local>> = None;
                let mut last_time: Option<DateTime<Local>> = None;

                let fallback_model = dominant_real_model(&parsed.turns);
                let mut last_real_model: Option<String> = None;
                let mut priced_turns = parsed.turns;

                for t in &mut priced_turns {
                    let eff_model = if !t.is_checkpoint
                        && !is_compaction_model(&t.model)
                        && t.model != "unknown"
                    {
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
                    t.cost = price_turn_record(&self.pricing, eff_model, t, local_date);

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

                let family = parsed.source.agent_family();
                let key = (family, parsed.conversation_id.clone());
                let sess = SessionInfo {
                    conversation_id: parsed.conversation_id,
                    title: parsed.title,
                    workspace: parsed.workspace,
                    source: parsed.source,
                    is_archived: false,
                    is_subagent: parsed.is_subagent,
                    parent_id: parsed.parent_id,
                    parent_title: parsed.parent_title,
                    turns: priced_turns,
                    counts,
                    models,
                    first_time,
                    last_time,
                    last_file_mtime_utc: parsed.last_file_mtime_utc,
                    is_running_hint: parsed.is_running_hint,
                };

                // Deduplicate within the same AgentFamily (e.g. Antigravity CLI/App/IDE sharing UUIDs):
                // keep session with more turns (or newer last_time).
                let replace = match sessions_by_key.get(&key) {
                    None => true,
                    Some(prev) => {
                        sess.turns.len() > prev.turns.len()
                            || (sess.turns.len() == prev.turns.len() && sess.last_time > prev.last_time)
                    }
                };
                if replace {
                    sessions_by_key.insert(key, sess);
                }
            }
        }

        let mut sessions: Vec<SessionInfo> = sessions_by_key.into_values().collect();

        // Link subagent sessions to their parent within the SAME AgentFamily so they inherit
        // workspace and parent title without ever cross-linking across different agents.
        let parent_lookup: HashMap<(AgentFamily, String), (String, String)> = sessions
            .iter()
            .map(|s| {
                (
                    (s.source.agent_family(), s.conversation_id.clone()),
                    (s.title.clone(), s.workspace.clone()),
                )
            })
            .collect();
        for s in &mut sessions {
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
                if (s.title == "(子会话)" || s.title == "(未命名会话)") && !ptitle.is_empty() {
                    s.title = format!("子任务 · {}", ptitle);
                }
            }
        }

        // Sync live sessions into local app-directory archive & backfill any pruned historical sessions.
        let archived_backfill = self.archive.sync_and_backfill(&sessions, &self.pricing, true);
        let archived_sessions_count = archived_backfill.len();
        sessions.extend(archived_backfill);

        sessions.sort_by(|a, b| b.last_time.cmp(&a.last_time));

        let agg = aggregate_sessions(&sessions);

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

        ScanSnapshot {
            data_dir: self.sources[0].1.clone(),
            data_dirs: self.sources.clone(),
            sessions,
            agg,
            statusline: self.statusline.snapshot().cloned(),
            statusline_changed,
            budget: self.budget.config(),
            budget_changed,
            budget_path: self.budget.path().map(Path::to_path_buf),
            session_meta: self.session_meta.entries().clone(),
            session_meta_changed,
            session_meta_path: self.session_meta.path().map(Path::to_path_buf),
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
                last_file_mtime_utc: s.last_file_mtime_utc,
                is_running_hint: s.is_running_hint,
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
        let ws = agg
            .by_workspace
            .entry(s.workspace.clone())
            .or_insert((0, TokenCounts::default()));
        ws.0 += 1;
        ws.1.merge(&s.counts);
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
            model: model.into(),
            is_checkpoint: false,
            input_tokens: 1_000,
            cache_read_tokens: 0,
            cache_write_tokens: cw,
            cache_write_1h_tokens: cw1h,
            output_tokens: out,
            reasoning_tokens: rt,
            tool_names: Vec::new(),
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
            turns: vec![t_plain],
            counts: TokenCounts::default(),
            models: BTreeSet::new(),
            first_time: None,
            last_time: None,
            last_file_mtime_utc: None,
            is_running_hint: false,
        };
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
            turns: vec![t_codex],
            counts: TokenCounts::default(),
            models: BTreeSet::new(),
            first_time: None,
            last_time: None,
            last_file_mtime_utc: None,
            is_running_hint: false,
        };
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
                    cache_write_1h_tokens: 0,
                    output_tokens: 6_000,
                    reasoning_tokens: 0,
                    tool_names: Vec::new(),
                    thinking_chars: 0,
                    gen_secs: None,
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
                    cache_write_1h_tokens: 0,
                    output_tokens: 500,
                    reasoning_tokens: 0,
                    tool_names: vec!["read_file".into()],
                    thinking_chars: 80,
                    gen_secs: None,
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
                    cache_write_1h_tokens: 0,
                    output_tokens: 200,
                    reasoning_tokens: 0,
                    tool_names: Vec::new(),
                    thinking_chars: 0,
                    gen_secs: None,
                    cost: CostBreakdown::default(),
                },
            ],
            counts: TokenCounts::default(),
            models: BTreeSet::new(),
            first_time: None,
            last_time: None,
            last_file_mtime_utc: None,
            is_running_hint: false,
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
                    cache_write_1h_tokens: 0,
                    output_tokens: 500,
                    reasoning_tokens: 0,
                    tool_names: Vec::new(),
                    thinking_chars: 0,
                    gen_secs: None,
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
                    cache_write_1h_tokens: 0,
                    output_tokens: 500,
                    reasoning_tokens: 0,
                    tool_names: Vec::new(),
                    thinking_chars: 0,
                    gen_secs: None,
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
                    cache_write_1h_tokens: 0,
                    output_tokens: 800,
                    reasoning_tokens: 0,
                    tool_names: Vec::new(),
                    thinking_chars: 0,
                    gen_secs: None,
                    cost: CostBreakdown::default(),
                },
            ],
            counts: TokenCounts::default(),
            models: BTreeSet::new(),
            first_time: None,
            last_time: None,
            last_file_mtime_utc: None,
            is_running_hint: false,
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
}
