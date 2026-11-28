//! Editorial-grade Direct2D + DirectWrite analytics dashboard.
//!
//! Design principles:
//!   - Warm neutral Obsidian Charcoal / Warm Paper themes (zero neon AI blue/purple/mint).
//!   - Flattened 4-tab underline navigation (`概览` | `按天` | `按周` | `会话`);
//!     Hero strip right-side CPU/RAM rings click-toggle the 90s System Monitor view.
//!   - Asymmetric 3-panel Overview: smart Trend + ISO Week/Model breakdown + Full-Width Recent Sessions
//!     with per-session cache hit rate.
//!   - Interactive "总计 / 仅增量" toggle and Registry-persisted DateScope (defaults to Today).

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike, Utc};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use windows::core::Result;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F};
use windows::Win32::Graphics::Direct2D::ID2D1RenderTarget;
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

use crate::d2d::{
    draw_activity_heatmap, draw_area_series, draw_brand_dial_icon, draw_editorial_bars, draw_line,
    draw_pace_bar, draw_ring_gauge, draw_text, editorial_bar_label_stride, fill_circle, fill_rect,
    fill_round_rect, hit_rect, load_color, rect_f, stroke_circle, stroke_round_rect, theme_for,
    with_alpha, BarItem, BarUnit, D2DFactories, DashFonts, Fonts, HeatmapDay, HeatmapGridGeom,
    Theme, WindowSurface,
};
use crate::launcher::{
    classify_resume_capability, find_resumable_parent, normalize_workspace_path_str,
    validate_conversation_id, BlockedReason,
};
use crate::pricing::{
    fmt_cost, fmt_cost_compact, fmt_rate_plain, fmt_usd, fmt_usd_compact, fmt_usd_precise,
    PriceSource, PricingTableRow,
};
use crate::quota::{evaluate_alerts, fmt_countdown, AlertSeverity, BudgetConfig, QuotaProvenance};
use crate::stats::{
    compute_active_duration_secs, compute_primary_model, filter_snapshot_range, fmt_commas,
    fmt_compact, fmt_duration_short, fmt_gap, fmt_num, is_compaction_model, iso_week_bounds,
    iso_week_key, iso_week_range_label, weekday_cn, workspace_short_name, AgentFamily,
    AggregatedStats, DateScope, ModelCacheStats, ScanSnapshot, SessionInfo, SessionSort,
    SourceKind, TokenCounts, TurnRecord,
};
use crate::telemetry::{apply_win11_frame, copy_text_to_clipboard, SystemTelemetry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Daily,
    Weekly,
    Models,
    Sessions,
    Calls,
    System,
}

/// Sort order for the per-call log (`调用` tab).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallSort {
    Recent,
    Cost,
    Total,
    Input,
    Output,
    Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DailyViewMode {
    Bars,
    Heatmap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverviewTopMode {
    QuotaPace,
    WeeklyCompare,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeeklyMode {
    Weekly,
    Monthly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakdownMode {
    Model,
    Workspace,
    Tools,
    Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCardMetric {
    Volume,
    CacheHit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelViewMode {
    CacheCompare,
    Pricing,
    Budget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSort {
    TotalDesc,
    CostDesc,
    HitRateAsc,
    HitRateDesc,
}

/// Zero-allocation stack formatter for short hot-path labels (`"24%"`, `"12轮"`, `"16.3/32.0 GB"`).
struct FmtBuf {
    buf: [u8; 64],
    len: usize,
}

impl FmtBuf {
    #[inline]
    fn new() -> Self {
        Self {
            buf: [0u8; 64],
            len: 0,
        }
    }

    #[inline]
    fn clear(&mut self) {
        self.len = 0;
    }

    #[inline]
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl std::fmt::Write for FmtBuf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let bytes = s.as_bytes();
        let avail = self.buf.len().saturating_sub(self.len);
        if bytes.len() <= avail {
            self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
            self.len += bytes.len();
        } else {
            let mut take = avail;
            while take > 0 && !s.is_char_boundary(take) {
                take -= 1;
            }
            self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
            self.len += take;
        }
        Ok(())
    }
}

fn is_unknown_workspace(ws: &str) -> bool {
    let t = ws.trim();
    t.is_empty() || t == "(未知工作区)" || t == "(unknown)"
}

fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn alpha_label(mut idx: usize) -> String {
    let mut chars = Vec::new();
    loop {
        chars.push((b'A' + (idx % 26) as u8) as char);
        if idx < 26 {
            break;
        }
        idx = idx / 26 - 1;
    }
    chars.into_iter().rev().collect()
}

pub fn redact_data_dir(path: &Path) -> String {
    let norm = path.to_string_lossy().replace('\\', "/");
    let trimmed = norm.trim_end_matches('/');
    let lower = trimmed.to_ascii_lowercase();
    for suffix in ["antigravity-cli", "antigravity", "antigravity-ide", "tmp"] {
        if lower == format!(".gemini/{suffix}") || lower.ends_with(&format!("/.gemini/{suffix}")) {
            return format!("~/.gemini/{suffix}");
        }
    }
    for suffix in [".config/claude", ".claude", ".codex"] {
        if lower == suffix || lower.ends_with(&format!("/{suffix}")) {
            return format!("~/{suffix}");
        }
    }
    "<已脱敏目录>".to_string()
}

/// Returns the quiet monochrome badge text (without brackets) for a session's `SourceKind`.
/// When `has_non_antigravity` is `false`, preserves legacy behavior (`Cli` has no prefix;
/// `App` / `IDE` / `自定义` stay unchanged). When non-AGY agents exist, labels all sources
/// uniformly as `AGY` / `App` / `IDE` / `Claude` / `Codex` / `Gemini`.
pub fn format_source_tag(source: SourceKind, has_non_antigravity: bool) -> Option<&'static str> {
    match source {
        SourceKind::Cli => has_non_antigravity.then_some("AGY"),
        SourceKind::App => Some("App"),
        SourceKind::Ide => Some("IDE"),
        SourceKind::ClaudeCode => Some("Claude"),
        SourceKind::Codex => Some("Codex"),
        SourceKind::GeminiCli => Some("Gemini"),
        SourceKind::Custom => Some(if has_non_antigravity { "AGY" } else { "自定义" }),
    }
}

/// Formats turn thinking/reasoning metadata: prefers exact `reasoning_tokens` (`X tok`, used by
/// Codex & Gemini CLI) over plaintext `thinking_chars` (`X 字`, used by Antigravity & Claude Code).
pub fn format_turn_thinking(t: &TurnRecord) -> Option<String> {
    if !t.has_thinking() {
        return None;
    }
    if t.reasoning_tokens > 0 {
        Some(format!("{} tok", fmt_commas(t.reasoning_tokens)))
    } else if t.thinking_chars > 0 {
        Some(format!("{} 字", fmt_commas(t.thinking_chars)))
    } else {
        None
    }
}

/// Formats all active read-only source directories for the System tab footer (privacy-safe when redacted).
pub fn format_active_sources_line(
    data_dirs: &[(SourceKind, std::path::PathBuf)],
    fallback_dir: &Path,
    privacy_mode: bool,
) -> String {
    let fmt_one = |kind: SourceKind, dir: &Path| -> String {
        let eff_path: std::borrow::Cow<'_, Path> = if kind == SourceKind::GeminiCli
            && dir
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case(".gemini"))
        {
            std::borrow::Cow::Owned(dir.join("tmp"))
        } else {
            std::borrow::Cow::Borrowed(dir)
        };
        let path_disp = if privacy_mode {
            redact_data_dir(&eff_path)
        } else {
            eff_path.display().to_string()
        };
        format!("[{}] {}", kind.label_short(), path_disp)
    };
    if data_dirs.is_empty() {
        let disp = if privacy_mode {
            redact_data_dir(fallback_dir)
        } else {
            fallback_dir.display().to_string()
        };
        return format!("活跃来源: {disp}");
    }
    let parts: Vec<String> = data_dirs.iter().map(|(k, p)| fmt_one(*k, p)).collect();
    format!("活跃来源({}): {}", data_dirs.len(), parts.join(" · "))
}

#[inline]
fn contains_ascii_icase(haystack: &str, needle_lower: &str) -> bool {
    let nb = needle_lower.as_bytes();
    if nb.is_empty() {
        return true;
    }
    let hb = haystack.as_bytes();
    if nb.len() > hb.len() {
        return false;
    }
    let first = nb[0];
    for i in 0..=(hb.len() - nb.len()) {
        if hb[i].to_ascii_lowercase() == first
            && hb[i + 1..i + nb.len()]
                .iter()
                .zip(&nb[1..])
                .all(|(h, n)| h.to_ascii_lowercase() == *n)
        {
            return true;
        }
    }
    false
}

fn source_matches_query(source: SourceKind, q: &str) -> bool {
    if contains_ascii_icase(source.label_short(), q) || contains_ascii_icase(source.as_str(), q) {
        return true;
    }
    let fam = source.agent_family();
    if contains_ascii_icase(fam.label_short(), q) || contains_ascii_icase(fam.label_full(), q) {
        return true;
    }
    fam.search_aliases()
        .iter()
        .any(|alias| contains_ascii_icase(alias, q))
}

pub fn redact_pricing_path(path: Option<&Path>) -> String {
    let fname = path
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("pricing.json");
    format!("./{fname}")
}

#[derive(Debug, Clone, Default)]
pub struct Redactor {
    session_ids: HashMap<String, String>,
    session_titles: HashMap<String, String>,
    parent_titles: HashMap<String, String>,
    workspaces: HashMap<String, String>,
}

impl Redactor {
    pub fn build(sessions: &[SessionInfo], agg: &AggregatedStats) -> Self {
        Self::build_slices(sessions, &[], agg)
    }

    /// Zero-clone builder over primary in-scope sessions + optional out-of-scope pinned sessions.
    pub fn build_slices(
        primary: &[SessionInfo],
        extra: &[SessionInfo],
        agg: &AggregatedStats,
    ) -> Self {
        let mut out = Self::default();

        // 1) Deterministic workspace aliases ranked by total token volume descending.
        let mut ws_ranked: Vec<(&str, u64)> = agg
            .by_workspace
            .iter()
            .filter(|(ws, _)| !is_unknown_workspace(ws))
            .map(|(ws, (_, c))| (ws.as_str(), c.total_tokens()))
            .collect();
        ws_ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        for (idx, (ws, _)) in ws_ranked.into_iter().enumerate() {
            out.workspaces
                .insert(ws.to_string(), format!("项目 {}", alpha_label(idx)));
        }
        let mut next_ws = out.workspaces.len();
        let mut extra_ws: Vec<&str> = primary
            .iter()
            .chain(extra.iter())
            .map(|s| s.workspace.as_str())
            .filter(|ws| !is_unknown_workspace(ws) && !out.workspaces.contains_key(*ws))
            .collect();
        extra_ws.sort_unstable();
        extra_ws.dedup();
        for ws in extra_ws {
            out.workspaces
                .insert(ws.to_string(), format!("项目 {}", alpha_label(next_ws)));
            next_ws += 1;
        }

        // 2) Deterministic session & subagent aliases (preserves parent-child linkage).
        let present: HashSet<&str> = primary
            .iter()
            .chain(extra.iter())
            .map(|s| s.conversation_id.as_str())
            .collect();
        let mut root_nums: HashMap<&str, usize> = HashMap::new();
        let mut root_idx = 0usize;
        for s in primary.iter().chain(extra.iter()) {
            let attached_child = s.is_subagent
                && s.parent_id
                    .as_deref()
                    .map(|pid| present.contains(pid))
                    .unwrap_or(false);
            if attached_child {
                continue;
            }
            root_idx += 1;
            root_nums.insert(s.conversation_id.as_str(), root_idx);
            let sid = format!("S-{root_idx:03}");
            let title = if s.is_subagent {
                format!("子会话 #{root_idx}")
            } else {
                format!("会话 #{root_idx}")
            };
            out.session_ids.insert(s.conversation_id.clone(), sid);
            out.session_titles.insert(s.conversation_id.clone(), title.clone());
            out.parent_titles.insert(s.conversation_id.clone(), title);
        }

        let mut sub_counts: HashMap<&str, usize> = HashMap::new();
        let mut orphan_parent_idx = 0usize;
        for s in primary.iter().chain(extra.iter()) {
            if let Some(pid) = s.parent_id.as_deref() {
                if s.is_subagent {
                    if let Some(&p_num) = root_nums.get(pid) {
                        let c = sub_counts.entry(pid).or_insert(0);
                        *c += 1;
                        let sub_n = *c;
                        out.session_ids
                            .insert(s.conversation_id.clone(), format!("S-{p_num:03}.{sub_n}"));
                        out.session_titles
                            .insert(s.conversation_id.clone(), format!("子任务 #{p_num}.{sub_n}"));
                    } else if !out.parent_titles.contains_key(pid) {
                        orphan_parent_idx += 1;
                        out.parent_titles
                            .insert(pid.to_string(), format!("父任务 #{orphan_parent_idx}"));
                        out.session_ids
                            .entry(pid.to_string())
                            .or_insert_with(|| format!("P-{orphan_parent_idx:03}"));
                    }
                }
            }
        }

        out
    }

    pub fn session_id(&self, cid: &str) -> String {
        self.session_ids
            .get(cid)
            .cloned()
            .unwrap_or_else(|| "S-???".to_string())
    }

    pub fn session_title(&self, s: &SessionInfo) -> String {
        self.session_titles
            .get(&s.conversation_id)
            .cloned()
            .unwrap_or_else(|| {
                if s.is_subagent {
                    "子会话".to_string()
                } else {
                    "会话".to_string()
                }
            })
    }

    pub fn parent_title(&self, pid: &str) -> String {
        self.parent_titles
            .get(pid)
            .or_else(|| self.session_titles.get(pid))
            .cloned()
            .unwrap_or_else(|| "父任务".to_string())
    }

    pub fn workspace_label(&self, ws: &str) -> String {
        if is_unknown_workspace(ws) {
            "(未知工作区)".to_string()
        } else {
            self.workspaces
                .get(ws)
                .cloned()
                .unwrap_or_else(|| "项目 ?".to_string())
        }
    }

    pub fn workspace_short(&self, ws: &str) -> String {
        if workspace_short_name(ws).is_empty() {
            String::new()
        } else {
            self.workspace_label(ws)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionFilter {
    Model(String),
    Workspace(String),
    Agent(AgentFamily),
    Date(String),
    Week(String),
    Parent(String, String), // (parent_conversation_id, parent_title)
}

fn local_date_range_to_utc(
    start_date: NaiveDate,
    end_inclusive: NaiveDate,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let next_day = end_inclusive.succ_opt()?;
    let start_naive = start_date.and_hms_opt(0, 0, 0)?;
    let end_naive = next_day.and_hms_opt(0, 0, 0)?;
    let start_utc = Local
        .from_local_datetime(&start_naive)
        .earliest()?
        .with_timezone(&Utc);
    let end_utc = Local
        .from_local_datetime(&end_naive)
        .earliest()?
        .with_timezone(&Utc);
    if start_utc <= end_utc {
        Some((start_utc, end_utc))
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy)]
enum CompiledTurnFilter<'a> {
    Model(&'a str),
    UtcRange(DateTime<Utc>, DateTime<Utc>),
    DateFallback(NaiveDate),
    WeekFallback((NaiveDate, NaiveDate)),
    Never,
    All,
}

impl CompiledTurnFilter<'_> {
    #[inline]
    fn matches_turn(&self, t: &TurnRecord) -> bool {
        match *self {
            CompiledTurnFilter::Model(m) => t.model.as_ref() == m,
            CompiledTurnFilter::UtcRange(start_utc, end_utc) => t
                .timestamp_utc
                .is_some_and(|u| u >= start_utc && u < end_utc),
            CompiledTurnFilter::DateFallback(target) => t
                .timestamp_utc
                .is_some_and(|u| u.with_timezone(&Local).date_naive() == target),
            CompiledTurnFilter::WeekFallback((mon, sun)) => t.timestamp_utc.is_some_and(|u| {
                let d = u.with_timezone(&Local).date_naive();
                d >= mon && d <= sun
            }),
            CompiledTurnFilter::Never => false,
            CompiledTurnFilter::All => true,
        }
    }
}

impl SessionFilter {
    fn compile(&self) -> CompiledTurnFilter<'_> {
        match self {
            SessionFilter::Model(m) => CompiledTurnFilter::Model(m.as_str()),
            SessionFilter::Date(day) => match NaiveDate::parse_from_str(day, "%Y-%m-%d").ok() {
                Some(d) => match local_date_range_to_utc(d, d) {
                    Some((s, e)) => CompiledTurnFilter::UtcRange(s, e),
                    None => CompiledTurnFilter::DateFallback(d),
                },
                None => CompiledTurnFilter::Never,
            },
            SessionFilter::Week(wk) => match iso_week_bounds(wk) {
                Some((mon, sun)) => match local_date_range_to_utc(mon, sun) {
                    Some((s, e)) => CompiledTurnFilter::UtcRange(s, e),
                    None => CompiledTurnFilter::WeekFallback((mon, sun)),
                },
                None => CompiledTurnFilter::Never,
            },
            SessionFilter::Workspace(_) | SessionFilter::Agent(_) | SessionFilter::Parent(..) => {
                CompiledTurnFilter::All
            }
        }
    }

    fn label(&self, redactor: Option<&Redactor>) -> String {
        match self {
            SessionFilter::Model(m) => format!("模型: {}  ×", m),
            SessionFilter::Workspace(ws) => {
                let disp = match redactor {
                    Some(r) => r.workspace_label(ws),
                    None => {
                        let s = workspace_short_name(ws);
                        if s.is_empty() { ws.clone() } else { s }
                    }
                };
                format!("项目: {}  ×", disp)
            }
            SessionFilter::Agent(fam) => format!("来源: {}  ×", fam.label_short()),
            SessionFilter::Date(d) => format!("日期: {}  ×", d),
            SessionFilter::Week(w) => format!("周: {}  ×", w),
            SessionFilter::Parent(pid, title) => {
                let disp = match redactor {
                    Some(r) => r.parent_title(pid),
                    None => title.clone(),
                };
                let short: String = disp.chars().take(18).collect();
                let ell = if disp.chars().count() > 18 { "…" } else { "" };
                format!("任务组: {}{}  ×", short, ell)
            }
        }
    }

    fn matches(&self, s: &SessionInfo) -> bool {
        match self {
            SessionFilter::Model(_) | SessionFilter::Date(_) | SessionFilter::Week(_) => {
                let compiled = self.compile();
                s.turns.iter().any(|t| compiled.matches_turn(t))
            }
            SessionFilter::Workspace(ws) => s.workspace == *ws,
            SessionFilter::Agent(fam) => s.source.agent_family() == *fam,
            SessionFilter::Parent(pid, _) => {
                s.conversation_id == *pid || s.parent_id.as_deref() == Some(pid.as_str())
            }
        }
    }

    /// Model / day / week filters slice *inside* a session: only the matching turns should be
    /// counted. Workspace / agent / parent filters select whole sessions.
    fn slices_turns(&self) -> bool {
        matches!(self, SessionFilter::Model(_) | SessionFilter::Date(_) | SessionFilter::Week(_))
    }
}

/// Returns a copy of `s` keeping only turns matching a slicing filter (with counts, models,
/// summary stats, and time range recomputed), or `None` when no turn matches.
fn slice_session(s: &SessionInfo, f: &SessionFilter) -> Option<SessionInfo> {
    let compiled = f.compile();
    // Fast-path: if all turns match, reuse `Arc<[TurnRecord]>` with zero turn clones!
    if !s.turns.is_empty() && s.turns.iter().all(|t| compiled.matches_turn(t)) {
        return Some(s.clone());
    }
    let turns_vec: Vec<TurnRecord> = s
        .turns
        .iter()
        .filter(|t| compiled.matches_turn(t))
        .cloned()
        .collect();
    if turns_vec.is_empty() {
        return None;
    }
    let mut counts = TokenCounts::default();
    let mut models = std::collections::BTreeSet::new();
    let mut first_time: Option<chrono::DateTime<Local>> = None;
    let mut last_time: Option<chrono::DateTime<Local>> = None;
    let mut compaction_count = 0u32;
    let mut thinking_turns = 0u32;
    for t in &turns_vec {
        counts.add_call_with_cost(
            t.input_tokens,
            t.cache_read_tokens,
            t.cache_write_tokens,
            t.output_tokens,
            t.cost,
        );
        models.insert(t.model.to_string());
        if t.is_checkpoint {
            compaction_count = compaction_count.saturating_add(1);
        }
        if t.has_thinking() {
            thinking_turns = thinking_turns.saturating_add(1);
        }
        if let Some(ts) = t.timestamp_utc.map(|u| u.with_timezone(&Local)) {
            first_time = Some(first_time.map_or(ts, |p| p.min(ts)));
            last_time = Some(last_time.map_or(ts, |p| p.max(ts)));
        }
    }
    let primary_model = compute_primary_model(&turns_vec);
    let active_duration_secs = compute_active_duration_secs(&turns_vec);
    let turns: Arc<[TurnRecord]> = Arc::from(turns_vec);
    Some(SessionInfo {
        turns,
        counts,
        models,
        first_time,
        last_time,
        primary_model,
        active_duration_secs,
        compaction_count,
        thinking_turns,
        ..s.clone()
    })
}

#[derive(Debug, Clone)]
struct DisplaySessionRow {
    sess_idx: usize,
    out_of_scope: bool,
    sliced: Option<Box<SessionInfo>>,
    counts: TokenCounts,
    primary_model: Arc<str>,
    last_time: Option<chrono::DateTime<Local>>,
    subagent_count: usize,
    is_child_row: bool,
    is_pinned: bool,
}

/// One model call (= one transcript step carrying token counters) in the call log.
/// Indexes into `cached_sessions` (or `pinned_out_of_scope` when `out_of_scope` is true),
/// so building the log never copies turn data.
#[derive(Debug, Clone, Copy)]
struct CallRow {
    out_of_scope: bool,
    sess_idx: usize,
    turn_idx: usize,
    /// Seconds since the previous timestamped step of the same session (includes tool execution
    /// and user think time, so it is an interval rather than pure model latency).
    gap_secs: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickAction {
    SelectTab(Tab),
    ToggleSystemView,
    SelectDailyView(DailyViewMode),
    SelectOverviewTop(OverviewTopMode),
    SelectWeeklyMode(WeeklyMode),
    SelectBreakdown(BreakdownMode),
    SelectModelCardMetric(ModelCardMetric),
    SelectModelView(ModelViewMode),
    OpenPricingTable,
    OpenBudgetView,
    PickPricingRow(usize),
    NewPricingEntry,
    SavePricingEntry,
    ResetPricingEntry,
    SaveBudgetEntry,
    ClearBudgetEntry,
    CopyStatuslineConfig,
    ExportMenu,
    ApplyCustomDate,
    ClearSearch,
    SelectModelSort(ModelSort),
    SelectScope(DateScope),
    SetCostMode(bool),
    ToggleCostMode,
    TogglePrivacyMode,
    ToggleGroupSubagents,
    TogglePinnedOnly,
    TogglePinSession(String),
    ResumeSession(String),
    CopyResumeCmd(String),
    OpenWorkspace(String),
    SessionMoreMenu(String),
    ShowBlockedReason(String),
    ToggleFreshOnly,
    ToggleLightTheme,
    ToggleHuman,
    CopySummary,
    CopySessionDetail,
    Refresh,
    SelectSort(SessionSort),
    SelectSession(String),
    InspectSession(String),
    FilterSessions(SessionFilter),
    ClearSessionFilter,
    SelectCallSort(CallSort),
    SelectCall(String, i64),
    ShowSessionCalls(String),
    ClearCallSessionFilter,
    CopyCallDetail,
}

/// Minimal repaint mode returned by `DashboardView::on_telemetry_tick`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryRepaintMode {
    Skip,
    PartialHero,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeroTelemetrySig {
    cpu_pct_round: i32,
    mem_pct_round: i32,
    mem_used_tenth: i32,
    mem_total_tenth: i32,
    cpu_cores: u32,
}

impl HeroTelemetrySig {
    fn from_telemetry(telemetry: &SystemTelemetry) -> Self {
        let t = telemetry.latest;
        Self {
            cpu_pct_round: t.cpu_pct.round() as i32,
            mem_pct_round: t.mem_pct.round() as i32,
            mem_used_tenth: (t.mem_used_gb * 10.0).round() as i32,
            mem_total_tenth: (t.mem_total_gb * 10.0).round() as i32,
            cpu_cores: t.cpu_cores,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct HeroBlock4Layout {
    band: D2D_RECT_F,
    x3: f32,
    y: f32,
    h: f32,
    b4: D2D_RECT_F,
}

enum DiscretePlot {
    VerticalBars {
        plot: D2D_RECT_F,
        n: usize,
    },
    Heatmap(HeatmapGridGeom),
    RowRects {
        bounds: D2D_RECT_F,
        rows: Vec<D2D_RECT_F>,
    },
    TimeSeries {
        x0: f32,
        pr: f32,
        pt_y: f32,
        pb_y: f32,
        dx: f32,
        n: usize,
    },
}

impl DiscretePlot {
    fn hit_slot(&self, x: f32, y: f32) -> Option<usize> {
        match self {
            DiscretePlot::VerticalBars { plot, n } => {
                if *n == 0 || x < plot.left || x > plot.right || y < plot.top || y > plot.bottom {
                    return None;
                }
                let w = (plot.right - plot.left).max(1.0);
                let slot_w = w / *n as f32;
                let idx = ((x - plot.left) / slot_w).floor() as isize;
                if idx >= 0 && (idx as usize) < *n {
                    Some(idx as usize)
                } else {
                    None
                }
            }
            DiscretePlot::Heatmap(g) => {
                if g.n_weeks == 0 {
                    return None;
                }
                let step = g.cell + g.gap;
                let dx = x - g.gx0;
                let dy = y - g.gy0;
                if dx < 0.0 || dy < 0.0 {
                    return None;
                }
                let col = (dx / step).floor() as usize;
                let row = (dy / step).floor() as usize;
                if col >= g.n_weeks || row >= 7 {
                    return None;
                }
                let rem_x = dx - col as f32 * step;
                let rem_y = dy - row as f32 * step;
                if rem_x <= g.cell && rem_y <= g.cell {
                    Some(col * 7 + row)
                } else {
                    None
                }
            }
            DiscretePlot::RowRects { bounds, rows } => {
                if !hit_rect(bounds, x, y) {
                    return None;
                }
                rows.iter().position(|r| hit_rect(r, x, y))
            }
            DiscretePlot::TimeSeries {
                x0,
                pr,
                pt_y,
                pb_y,
                dx,
                n,
            } => {
                if *n < 2 || *dx <= 0.0 || x < *x0 || x > *pr || y < *pt_y || y > *pb_y {
                    return None;
                }
                let idx = (((x - *x0) / *dx).round() as isize).clamp(0, (*n - 1) as isize) as usize;
                Some(idx)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct HoverSig {
    hit_box: Option<usize>,
    continuous_plot: bool,
    discrete_slot: Option<(usize, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionRowsKey {
    data_gen: u64,
    sess_ptr: usize,
    sess_len: usize,
    oos_len: usize,
    pinned_len: usize,
    apply_filter: bool,
    include_oos: bool,
    sort: SessionSort,
    group_subagents: bool,
    pinned_only: bool,
    privacy_mode: bool,
    session_filter: Option<SessionFilter>,
    search_query: String,
}

struct SessionRowsCache {
    key: SessionRowsKey,
    rows: Vec<DisplaySessionRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallRowsKey {
    data_gen: u64,
    sess_ptr: usize,
    sess_len: usize,
    oos_len: usize,
    call_sort: CallSort,
    privacy_mode: bool,
    call_session_filter: Option<String>,
    search_query: String,
}

struct CallRowsCache {
    key: CallRowsKey,
    rows: Vec<CallRow>,
    sum: TokenCounts,
    speed_out: u64,
    speed_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionDetailKey {
    data_gen: u64,
    sess_ptr: usize,
    sess_len: usize,
    snap_sess_len: usize,
    selected_cid: String,
    session_filter: Option<SessionFilter>,
}

struct SessionDetailCache {
    key: SessionDetailKey,
    sliced_override: Option<SessionInfo>,
    sub_cnt: usize,
    sub_agg: TokenCounts,
    direct_ok: bool,
    blocked_msg: &'static str,
    resumable_parent_id: Option<String>,
    has_ws: bool,
    compaction_count: usize,
    cache_miss_count: usize,
    active_duration_secs: u64,
    thinking_turns: usize,
    top_tools_str: String,
}

struct HitBox {
    rect: D2D_RECT_F,
    action: ClickAction,
}

pub struct DashboardView {
    /// Isolated dashboard-only Direct2D + DirectWrite factory (lazily created on first show,
    /// dropped on hide AFTER `surface` so D3D10Warp tile pools are genuinely released).
    dash_factories: Option<D2DFactories>,
    surface: Option<WindowSurface>,
    pub active_tab: Tab,
    prev_token_tab: Tab,
    pub daily_view: DailyViewMode,
    pub overview_top: OverviewTopMode,
    overview_top_chosen: bool,
    pub weekly_mode: WeeklyMode,
    pub breakdown_mode: BreakdownMode,
    pub model_card_metric: ModelCardMetric,
    pub model_view: ModelViewMode,
    pub model_sort: ModelSort,
    pub scope: DateScope,
    pub custom_since: Option<NaiveDate>,
    pub custom_until: Option<NaiveDate>,
    pub custom_date_rects: Option<[D2D_RECT_F; 2]>,
    pub custom_date_focused: Option<usize>,
    pub search_query: String,
    pub search_field_rect: Option<D2D_RECT_F>,
    pub search_focused: bool,
    pub sort: SessionSort,
    pub session_filter: Option<SessionFilter>,
    pub cost_mode: bool,
    pub group_subagents: bool,
    pub pinned_only: bool,
    pub privacy_mode: bool,
    pub fresh_only: bool,
    pub selected_session: Option<String>,
    pub call_sort: CallSort,
    /// Selected call as `(conversation_id, step_index)`.
    pub selected_call: Option<(String, i64)>,
    /// Restricts the call log to one session (drill-down from the Sessions tab).
    pub call_session_filter: Option<String>,
    call_copied_until: Option<Instant>,
    pub pricing_selected_model: Option<String>,
    pub pricing_editing_custom_key: Option<String>,
    pub pricing_status: Option<(String, bool, Instant)>,
    pub pricing_field_rects: Option<[D2D_RECT_F; 5]>,
    pub budget_field_rects: Option<[D2D_RECT_F; 4]>,
    pub pricing_focused_field: Option<usize>,
    pub export_btn_rect: Option<D2D_RECT_F>,
    pub detail_more_btn_rect: Option<D2D_RECT_F>,
    pub mouse_dip: Option<(f32, f32)>,
    pub scroll_rows: usize,
    max_scroll: usize,
    sessions_vis_rows: usize,
    hit_boxes: Vec<HitBox>,
    session_row_rects: Vec<(D2D_RECT_F, String)>,
    plot_rects: Vec<D2D_RECT_F>,
    discrete_plots: Vec<DiscretePlot>,
    last_hover_sig: HoverSig,
    copied_until: Option<Instant>,
    session_copied_until: Option<Instant>,
    pub resume_cmd_copied_until: Option<Instant>,
    banner_status: Option<(String, bool, Instant)>,
    cached_sessions: Vec<SessionInfo>,
    pinned_out_of_scope: Vec<SessionInfo>,
    pinned_ids: HashSet<String>,
    cached_agg: AggregatedStats,
    all_total: TokenCounts,
    all_sessions_count: usize,
    all_root_sessions: usize,
    all_sub_sessions: usize,
    cached_root_sessions: usize,
    cached_sub_sessions: usize,
    has_non_antigravity: bool,
    redactor: RefCell<Redactor>,
    redactor_built: Cell<bool>,
    cached_date: NaiveDate,
    dirty_filter: bool,
    data_gen: u64,
    session_rows_cache: RefCell<Option<SessionRowsCache>>,
    overview_rows_cache: RefCell<Option<SessionRowsCache>>,
    call_rows_cache: RefCell<Option<CallRowsCache>>,
    session_detail_cache: RefCell<Option<SessionDetailCache>>,
    cached_alert_level: Option<(u64, i64, u8)>,
    last_hero_telemetry_sig: Option<HeroTelemetrySig>,
    hero_block4_layout: Option<HeroBlock4Layout>,
    last_full_paint: Option<Instant>,
    had_active_toast: bool,
}

/// Compact human-scannable Chinese relative timestamp for session rows (`刚刚`/`3m前`/`2h前`/`昨天`/`04/12`).
pub fn fmt_relative_time_cn(ts: Option<DateTime<Local>>, now: DateTime<Local>) -> String {
    let Some(t) = ts else {
        return "-".to_string();
    };
    let delta = now.signed_duration_since(t).num_seconds();
    if delta < 60 {
        return "刚刚".to_string();
    }
    if delta < 3600 {
        return format!("{}m前", delta / 60);
    }
    let today = now.date_naive();
    let d = t.date_naive();
    if d == today && delta < 86_400 {
        return format!("{}h前", delta / 3600);
    }
    if today.signed_duration_since(d).num_days() == 1 {
        return "昨天".to_string();
    }
    if d.year() == today.year() {
        t.format("%m/%d").to_string()
    } else {
        t.format("%y/%m/%d").to_string()
    }
}

fn draw_session_status_dot(
    rt: &ID2D1RenderTarget,
    theme: &Theme,
    cx: f32,
    cy: f32,
    s: &SessionInfo,
    last_time: Option<DateTime<Local>>,
    now: DateTime<Local>,
) {
    if s.is_running_hint && !s.is_archived {
        fill_circle(rt, cx, cy, 4.6, with_alpha(theme.accent, 0.24));
        fill_circle(rt, cx, cy, 2.5, theme.accent);
    } else if s.is_archived {
        stroke_circle(rt, cx, cy, 2.1, theme.muted, 0.9);
    } else if let Some(lt) = last_time {
        let age = now.signed_duration_since(lt).num_seconds();
        if age < 3600 {
            fill_circle(rt, cx, cy, 2.2, theme.sage);
        }
    }
}

fn hit_rate_color(theme: &Theme, rate: f64) -> D2D1_COLOR_F {
    if rate >= 85.0 {
        theme.sage
    } else if rate >= 60.0 {
        theme.warn
    } else {
        theme.danger
    }
}

fn cache_hit_color(theme: &Theme, c: &TokenCounts) -> D2D1_COLOR_F {
    if c.calls <= 1 && c.cache_read_tokens == 0 {
        theme.muted
    } else {
        hit_rate_color(theme, c.cache_hit_rate())
    }
}

/// Unified 3-chunk Gestalt metric columns for Overview Recent Sessions and Sessions List:
///   [轮数]  —10px—  [总Token | 新输入 | 输出]  —10px—  [命中率 | 费用]
const SESSION_METRIC_LABELS: [&str; 6] = ["轮数", "总Token", "新输入", "输出", "命中率", "费用"];

fn session_metric_columns(rx: f32) -> ([f32; 6], [f32; 6], f32) {
    let widths = [38.0_f32, 64.0, 56.0, 52.0, 50.0, 62.0];
    // Gap after each column (index 0 -> 10px before token trio; index 3 -> 10px before hit/cost)
    let gaps = [10.0_f32, 4.0, 4.0, 10.0, 4.0, 0.0];
    let mut xs = [0.0_f32; 6];
    let mut cx = rx;
    for i in (0..6).rev() {
        cx -= widths[i];
        xs[i] = cx;
        if i > 0 {
            cx -= gaps[i - 1];
        }
    }
    let total_w = rx - xs[0] + 6.0;
    (xs, widths, total_w)
}

#[allow(clippy::too_many_arguments)]
fn draw_session_metric_cells(
    rt: &ID2D1RenderTarget,
    fonts: &DashFonts,
    theme: &Theme,
    xs: &[f32; 6],
    ws: &[f32; 6],
    ry: f32,
    cell_h: f32,
    counts: &TokenCounts,
    hit_str: &str,
    hit_col: D2D1_COLOR_F,
    cost_str: &str,
    cost_mode: bool,
    human: bool,
) {
    let r = |i: usize| rect_f(xs[i], ry, ws[i], cell_h);
    // Tier 4: quiet count (stack-formatted, 0 heap allocs)
    let mut fbuf = FmtBuf::new();
    let _ = write!(fbuf, "{}轮", counts.calls);
    draw_text(rt, fbuf.as_str(), &fonts.mono_small_right, r(0), theme.muted);
    // Tier 1 (Tok mode) / Tier 3 ($ mode): 总Token
    draw_text(
        rt,
        &fmt_num(counts.total_tokens(), human),
        if cost_mode { &fonts.mono_small_right } else { &fonts.mono_med_right },
        r(1),
        if cost_mode { theme.sub } else { theme.text },
    );
    // Tier 2: fresh input & output (crisp ink in Tok mode, subdued in $ mode)
    let driver_col = if cost_mode { theme.sub } else { theme.text };
    draw_text(
        rt,
        &fmt_num(counts.input_tokens + counts.cache_write_tokens, human),
        &fonts.mono_small_right,
        r(2),
        driver_col,
    );
    draw_text(rt, &fmt_num(counts.output_tokens, human), &fonts.mono_small_right, r(3), driver_col);
    // Tier 2: semantic cache hit rate
    draw_text(rt, hit_str, &fonts.mono_small_right, r(4), hit_col);
    // Tier 1 ($ mode) / Tier 2 accent (Tok mode): cost
    draw_text(
        rt,
        cost_str,
        if cost_mode { &fonts.mono_med_right } else { &fonts.mono_small_right },
        r(5),
        if cost_mode { theme.text } else { theme.output },
    );
}

impl DashboardView {
    pub fn new(
        fresh_only: bool,
        scope: DateScope,
        cost_mode: bool,
        group_subagents: bool,
        privacy_mode: bool,
    ) -> Self {
        Self {
            dash_factories: None,
            surface: None,
            active_tab: Tab::Overview,
            prev_token_tab: Tab::Overview,
            daily_view: DailyViewMode::Bars,
            overview_top: OverviewTopMode::WeeklyCompare,
            overview_top_chosen: false,
            weekly_mode: WeeklyMode::Weekly,
            breakdown_mode: BreakdownMode::Model,
            model_card_metric: ModelCardMetric::Volume,
            model_view: ModelViewMode::CacheCompare,
            model_sort: ModelSort::TotalDesc,
            scope,
            custom_since: Local::now().date_naive().checked_sub_days(chrono::Days::new(6)),
            custom_until: Some(Local::now().date_naive()),
            custom_date_rects: None,
            custom_date_focused: None,
            search_query: String::new(),
            search_field_rect: None,
            search_focused: false,
            sort: SessionSort::Recent,
            session_filter: None,
            cost_mode,
            group_subagents,
            pinned_only: false,
            privacy_mode,
            fresh_only,
            selected_session: None,
            call_sort: CallSort::Recent,
            selected_call: None,
            call_session_filter: None,
            call_copied_until: None,
            pricing_selected_model: None,
            pricing_editing_custom_key: None,
            pricing_status: None,
            pricing_field_rects: None,
            budget_field_rects: None,
            pricing_focused_field: None,
            export_btn_rect: None,
            detail_more_btn_rect: None,
            mouse_dip: None,
            scroll_rows: 0,
            max_scroll: 0,
            sessions_vis_rows: 6,
            hit_boxes: Vec::new(),
            session_row_rects: Vec::new(),
            plot_rects: Vec::new(),
            discrete_plots: Vec::new(),
            last_hover_sig: HoverSig::default(),
            copied_until: None,
            session_copied_until: None,
            resume_cmd_copied_until: None,
            banner_status: None,
            cached_sessions: Vec::new(),
            pinned_out_of_scope: Vec::new(),
            pinned_ids: HashSet::new(),
            cached_agg: AggregatedStats::default(),
            all_total: TokenCounts::default(),
            all_sessions_count: 0,
            all_root_sessions: 0,
            all_sub_sessions: 0,
            cached_root_sessions: 0,
            cached_sub_sessions: 0,
            has_non_antigravity: false,
            redactor: RefCell::new(Redactor::default()),
            redactor_built: Cell::new(false),
            cached_date: Local::now().date_naive(),
            dirty_filter: true,
            data_gen: 0,
            session_rows_cache: RefCell::new(None),
            overview_rows_cache: RefCell::new(None),
            call_rows_cache: RefCell::new(None),
            session_detail_cache: RefCell::new(None),
            cached_alert_level: None,
            last_hero_telemetry_sig: None,
            hero_block4_layout: None,
            last_full_paint: None,
            had_active_toast: false,
        }
    }

    fn with_redactor<R>(&self, f: impl FnOnce(&Redactor) -> R) -> R {
        if !self.redactor_built.get() {
            let built = Redactor::build_slices(
                &self.cached_sessions,
                &self.pinned_out_of_scope,
                &self.cached_agg,
            );
            *self.redactor.borrow_mut() = built;
            self.redactor_built.set(true);
        }
        f(&self.redactor.borrow())
    }

    #[cfg(test)]
    fn set_redactor_for_test(&mut self, redactor: Redactor) {
        *self.redactor.get_mut() = redactor;
        self.redactor_built.set(true);
    }

    fn clear_view_caches(&self) {
        *self.session_rows_cache.borrow_mut() = None;
        *self.overview_rows_cache.borrow_mut() = None;
        *self.call_rows_cache.borrow_mut() = None;
        *self.session_detail_cache.borrow_mut() = None;
    }

    fn format_session_id(&self, cid: &str) -> String {
        if self.privacy_mode {
            self.with_redactor(|r| r.session_id(cid))
        } else {
            cid.chars().take(8).collect()
        }
    }

    fn format_session_title(&self, s: &SessionInfo) -> String {
        if self.privacy_mode {
            self.with_redactor(|r| r.session_title(s))
        } else {
            s.title.clone()
        }
    }

    fn format_parent_title(&self, pid: &str, raw_title: &str) -> String {
        if self.privacy_mode {
            self.with_redactor(|r| r.parent_title(pid))
        } else {
            raw_title.to_string()
        }
    }

    fn format_workspace_label(&self, ws: &str) -> String {
        if self.privacy_mode {
            self.with_redactor(|r| r.workspace_label(ws))
        } else {
            let short = workspace_short_name(ws);
            if short.is_empty() {
                "(未知工作区)".to_string()
            } else {
                short
            }
        }
    }

    fn format_workspace_short(&self, ws: &str) -> String {
        if self.privacy_mode {
            self.with_redactor(|r| r.workspace_short(ws))
        } else {
            workspace_short_name(ws)
        }
    }

    fn format_session_detail_clipboard(&self, s: &SessionInfo) -> String {
        let ws = self.format_workspace_short(&s.workspace);
        let title = self.format_session_title(s);
        let sid = self.format_session_id(&s.conversation_id);
        let cp = s.compaction_count as usize;
        let (misses, miss_tok) = s.cache_miss_stats();
        let range = match (s.first_time, s.last_time) {
            (Some(a), Some(b)) => format!("{} -> {}", a.format("%m/%d %H:%M"), b.format("%H:%M")),
            _ => "-".to_string(),
        };
        let mut events = String::new();
        if cp > 0 {
            events.push_str(&format!(" · {}次压缩", cp));
        }
        if misses > 0 {
            events.push_str(&format!(" · {}次缓存未命中(重灌{})", misses, fmt_compact(miss_tok)));
        }
        let source_disp = match s.source {
            SourceKind::App | SourceKind::Ide => {
                format!("{} ({})", s.source.family_label(), s.source.label_short())
            }
            _ => s.source.family_label().to_string(),
        };
        format!(
            "会话明细 · {}\n- ID: {} | 来源: {} | 时间: {} | 模型: {}{}\n- 轮数: {}{} | 缓存命中: {:.1}%\n- 估算 API 费用: {} (新输入 {} / 缓存读 {} / 输出 {} · 缓存已省 {})\n- 新增输入: {} | 缓存读取: {} | 缓存写入: {} | 输出: {}\n- 总 Token: {} (非缓存增量: {}) | 峰值Ctx: {}",
            title,
            sid,
            source_disp,
            range,
            s.primary_model(),
            if ws.is_empty() { String::new() } else { format!(" | 项目: {}", ws) },
            s.counts.calls,
            events,
            s.counts.cache_hit_rate(),
            fmt_cost(&s.counts.cost, s.counts.calls, false),
            fmt_usd_precise(s.counts.cost.input_plus_write_usd()),
            fmt_usd_precise(s.counts.cost.cache_read_usd),
            fmt_usd_precise(s.counts.cost.output_usd),
            fmt_usd_precise(s.counts.cost.cache_saved_usd),
            fmt_commas(s.counts.input_tokens),
            fmt_commas(s.counts.cache_read_tokens),
            fmt_commas(s.counts.cache_write_tokens),
            fmt_commas(s.counts.output_tokens),
            fmt_commas(s.counts.total_tokens()),
            fmt_commas(s.counts.non_cached_tokens()),
            fmt_commas(s.counts.peak_context),
        )
    }

    fn bar_unit(&self) -> BarUnit {
        if self.cost_mode {
            BarUnit::Usd
        } else {
            BarUnit::Tokens
        }
    }

    fn make_bar(&self, label: impl Into<Cow<'static, str>>, c: &TokenCounts, marker: bool) -> BarItem {
        let label = label.into();
        if self.cost_mode {
            BarItem {
                label,
                cache: c.cost.cache_read_usd,
                input: c.cost.input_plus_write_usd(),
                output: c.cost.output_usd,
                marker,
            }
        } else {
            BarItem {
                label,
                cache: c.cache_read_tokens as f64,
                input: (c.input_tokens + c.cache_write_tokens) as f64,
                output: c.output_tokens as f64,
                marker,
            }
        }
    }

    fn make_turn_bar(&self, label: impl Into<Cow<'static, str>>, t: &TurnRecord) -> BarItem {
        let label = label.into();
        if self.cost_mode {
            BarItem {
                label,
                cache: t.cost.cache_read_usd,
                input: t.cost.input_plus_write_usd(),
                output: t.cost.output_usd,
                marker: t.is_checkpoint,
            }
        } else {
            BarItem {
                label,
                cache: t.cache_read_tokens as f64,
                input: (t.input_tokens + t.cache_write_tokens) as f64,
                output: t.output_tokens as f64,
                marker: t.is_checkpoint,
            }
        }
    }

    pub fn step_session(&mut self, delta: isize) -> bool {
        if self.active_tab == Tab::Calls {
            return self.step_call(delta);
        }
        if self.active_tab != Tab::Sessions {
            return false;
        }
        let next_sel = self.with_display_sessions_for_tab(|sorted| {
            if sorted.is_empty() {
                return None;
            }
            let cur_idx = self
                .selected_session
                .as_deref()
                .and_then(|id| sorted.iter().position(|r| self.row_session(r).conversation_id == id))
                .unwrap_or(0);
            let next_idx = (cur_idx as isize + delta).clamp(0, (sorted.len() - 1) as isize) as usize;
            if next_idx == cur_idx && self.selected_session.is_some() {
                return None;
            }
            Some((next_idx, self.row_session(&sorted[next_idx]).conversation_id.clone()))
        });
        let Some((next_idx, cid)) = next_sel else {
            return false;
        };
        self.selected_session = Some(cid);
        let vis = self.sessions_vis_rows.max(1);
        if next_idx < self.scroll_rows {
            self.scroll_rows = next_idx;
        } else if next_idx >= self.scroll_rows + vis {
            self.scroll_rows = (next_idx + 1).saturating_sub(vis);
        }
        true
    }

    /// The active slicing filter (model / day / week), if any.
    fn slicing_filter(&self) -> Option<&SessionFilter> {
        self.session_filter.as_ref().filter(|f| f.slices_turns())
    }

    fn session_ref_for_detail<'a>(&'a self, cid: &str) -> Option<Cow<'a, SessionInfo>> {
        if let Some(s) = self.cached_sessions.iter().find(|s| s.conversation_id == cid) {
            return match self.slicing_filter() {
                Some(f) => Some(
                    slice_session(s, f)
                        .map(Cow::Owned)
                        .unwrap_or(Cow::Borrowed(s)),
                ),
                None => Some(Cow::Borrowed(s)),
            };
        }
        self.pinned_out_of_scope
            .iter()
            .find(|s| s.conversation_id == cid)
            .map(Cow::Borrowed)
    }

    /// Looks up a session by id (including out-of-scope pinned sessions), sliced to the active
    /// model / day / week filter when applicable so the detail card and clipboard match the row.
    #[allow(dead_code)]
    pub fn session_for_detail(&self, cid: &str) -> Option<SessionInfo> {
        self.session_ref_for_detail(cid).map(Cow::into_owned)
    }

    pub fn count_subagents_of(&self, parent_id: &str) -> (usize, TokenCounts) {
        let mut cnt = 0usize;
        let mut agg = TokenCounts::default();
        let slicer = self.slicing_filter();
        for s in &self.cached_sessions {
            if s.parent_id.as_deref() == Some(parent_id) {
                match slicer {
                    Some(f) => {
                        if let Some(sl) = slice_session(s, f) {
                            cnt += 1;
                            agg.merge(&sl.counts);
                        }
                    }
                    None => {
                        cnt += 1;
                        agg.merge(&s.counts);
                    }
                }
            }
        }
        (cnt, agg)
    }

    fn session_split_counts_of(sessions: &[SessionInfo]) -> (usize, usize) {
        let sub = sessions.iter().filter(|s| s.is_subagent).count();
        (sessions.len().saturating_sub(sub), sub)
    }

    fn session_split_counts(&self) -> (usize, usize) {
        if self.cached_root_sessions + self.cached_sub_sessions == self.cached_sessions.len() {
            (self.cached_root_sessions, self.cached_sub_sessions)
        } else {
            Self::session_split_counts_of(&self.cached_sessions)
        }
    }

    fn row_session<'a>(&'a self, r: &'a DisplaySessionRow) -> &'a SessionInfo {
        if let Some(ref s) = r.sliced {
            s.as_ref()
        } else if r.out_of_scope {
            &self.pinned_out_of_scope[r.sess_idx]
        } else {
            &self.cached_sessions[r.sess_idx]
        }
    }

    fn sort_display_rows_by(
        &self,
        rows: &mut [DisplaySessionRow],
        sort: SessionSort,
        keep_parent_first: Option<&str>,
    ) {
        let ord_row = |a: &DisplaySessionRow, b: &DisplaySessionRow| -> std::cmp::Ordering {
            match sort {
                SessionSort::Recent => b.last_time.cmp(&a.last_time),
                SessionSort::Cost => b
                    .counts
                    .cost
                    .total_usd()
                    .partial_cmp(&a.counts.cost.total_usd())
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.counts.total_tokens().cmp(&a.counts.total_tokens())),
                SessionSort::TotalTokens => b.counts.total_tokens().cmp(&a.counts.total_tokens()),
                SessionSort::FreshInput => (b.counts.input_tokens + b.counts.cache_write_tokens)
                    .cmp(&(a.counts.input_tokens + a.counts.cache_write_tokens)),
                SessionSort::Output => b.counts.output_tokens.cmp(&a.counts.output_tokens),
                SessionSort::HitRateLow => {
                    let a_single = a.counts.calls <= 1;
                    let b_single = b.counts.calls <= 1;
                    a_single
                        .cmp(&b_single)
                        .then_with(|| {
                            a.counts
                                .cache_hit_rate()
                                .partial_cmp(&b.counts.cache_hit_rate())
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .then_with(|| b.counts.total_tokens().cmp(&a.counts.total_tokens()))
                }
            }
        };
        if let Some(pid) = keep_parent_first {
            rows.sort_by(|a, b| {
                let a_root = self.row_session(a).conversation_id == pid;
                let b_root = self.row_session(b).conversation_id == pid;
                b_root
                    .cmp(&a_root)
                    .then_with(|| b.is_pinned.cmp(&a.is_pinned))
                    .then_with(|| ord_row(a, b))
            });
        } else {
            rows.sort_by(|a, b| b.is_pinned.cmp(&a.is_pinned).then_with(|| ord_row(a, b)));
        }
    }

    fn session_matches_search_lower(&self, s: &SessionInfo, q: &str) -> bool {
        if q.is_empty() {
            return true;
        }
        let title = self.format_session_title(s);
        if contains_ascii_icase(&title, q) {
            return true;
        }
        let sid = self.format_session_id(&s.conversation_id);
        if contains_ascii_icase(&sid, q) {
            return true;
        }
        let ws = self.format_workspace_label(&s.workspace);
        if contains_ascii_icase(&ws, q) {
            return true;
        }
        if source_matches_query(s.source, q) {
            return true;
        }
        if s.models.iter().any(|m| contains_ascii_icase(m, q)) {
            return true;
        }
        if s.turns
            .iter()
            .any(|t| t.tool_names.iter().any(|tn| contains_ascii_icase(tn, q)))
        {
            return true;
        }
        false
    }

    fn make_session_rows_key(
        &self,
        apply_filter: bool,
        include_oos: bool,
        sort: SessionSort,
    ) -> SessionRowsKey {
        SessionRowsKey {
            data_gen: self.data_gen,
            sess_ptr: self.cached_sessions.as_ptr() as usize,
            sess_len: self.cached_sessions.len(),
            oos_len: self.pinned_out_of_scope.len(),
            pinned_len: self.pinned_ids.len(),
            apply_filter,
            include_oos,
            sort,
            group_subagents: self.group_subagents,
            pinned_only: apply_filter && include_oos && self.pinned_only,
            privacy_mode: self.privacy_mode,
            session_filter: if apply_filter {
                self.session_filter.clone()
            } else {
                None
            },
            search_query: if apply_filter {
                self.search_query.clone()
            } else {
                String::new()
            },
        }
    }

    /// Borrows cached rows for the `会话` tab (zero rebuild/sort on hover, scroll, or 1s ticks).
    fn with_display_sessions_for_tab<R>(&self, f: impl FnOnce(&[DisplaySessionRow]) -> R) -> R {
        let key = self.make_session_rows_key(true, true, self.sort);
        let hit = self
            .session_rows_cache
            .borrow()
            .as_ref()
            .is_some_and(|c| c.key == key);
        if !hit {
            let rows = self.display_sessions_inner(true, true, self.sort, true);
            *self.session_rows_cache.borrow_mut() = Some(SessionRowsCache { key, rows });
        }
        let borrow = self.session_rows_cache.borrow();
        f(&borrow.as_ref().expect("session_rows_cache populated").rows)
    }

    /// Borrows cached recent-sorted rows for the Overview tab (zero rebuild/sort on hover/ticks).
    fn with_overview_recent_sessions<R>(&self, f: impl FnOnce(&[DisplaySessionRow]) -> R) -> R {
        let key = self.make_session_rows_key(false, false, SessionSort::Recent);
        let hit = self
            .overview_rows_cache
            .borrow()
            .as_ref()
            .is_some_and(|c| c.key == key);
        if !hit {
            // Overview sorts strictly by recency (not pin-first) matching original behavior.
            let rows = self.display_sessions_inner(false, false, SessionSort::Recent, false);
            *self.overview_rows_cache.borrow_mut() = Some(SessionRowsCache { key, rows });
        }
        let borrow = self.overview_rows_cache.borrow();
        f(&borrow.as_ref().expect("overview_rows_cache populated").rows)
    }

    /// Sessions list for the `会话` tab (includes cross-`DateScope` pinned sessions and honors `pinned_only`).
    #[cfg(test)]
    fn display_sessions_for_tab(&self) -> Vec<DisplaySessionRow> {
        self.with_display_sessions_for_tab(|r| r.to_vec())
    }

    /// Sessions list strictly inside the active `DateScope` (for CSV/JSON exports & overview fidelity).
    fn display_sessions(&self, apply_filter: bool) -> Vec<DisplaySessionRow> {
        self.display_sessions_inner(apply_filter, false, self.sort, true)
    }

    /// Builds session list rows, rolling up subagents into their parent session when `group_subagents`
    /// is enabled (unless drilling into a Parent group or filtering specifically to subagent-only models).
    fn display_sessions_inner(
        &self,
        apply_filter: bool,
        include_out_of_scope_pins: bool,
        sort: SessionSort,
        pin_first: bool,
    ) -> Vec<DisplaySessionRow> {
        let filter = if apply_filter { self.session_filter.as_ref() } else { None };
        let q = if apply_filter {
            self.search_query.trim().to_ascii_lowercase()
        } else {
            String::new()
        };
        let apply_search = !q.is_empty();
        let apply_pinned_only = apply_filter && include_out_of_scope_pins && self.pinned_only;

        // Parent drill-down always expands parent + all its subagents flat for inspection.
        if let Some(SessionFilter::Parent(pid, _)) = filter {
            let mut rows: Vec<DisplaySessionRow> = self
                .cached_sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| s.conversation_id == *pid || s.parent_id.as_deref() == Some(pid.as_str()))
                .filter(|(_, s)| !apply_search || self.session_matches_search_lower(s, &q))
                .map(|(idx, s)| DisplaySessionRow {
                    sess_idx: idx,
                    out_of_scope: false,
                    sliced: None,
                    counts: s.counts,
                    primary_model: s.primary_model.clone(),
                    last_time: s.last_time,
                    subagent_count: 0,
                    is_child_row: s.parent_id.as_deref() == Some(pid.as_str()),
                    is_pinned: self.pinned_ids.contains(&s.conversation_id),
                })
                .collect();
            if include_out_of_scope_pins && !self.pinned_out_of_scope.is_empty() {
                for (idx, s) in self.pinned_out_of_scope.iter().enumerate() {
                    if (s.conversation_id == *pid || s.parent_id.as_deref() == Some(pid.as_str()))
                        && (!apply_search || self.session_matches_search_lower(s, &q))
                        && !rows
                            .iter()
                            .any(|r| self.row_session(r).conversation_id == s.conversation_id)
                    {
                        rows.push(DisplaySessionRow {
                            sess_idx: idx,
                            out_of_scope: true,
                            sliced: None,
                            counts: s.counts,
                            primary_model: s.primary_model.clone(),
                            last_time: s.last_time,
                            subagent_count: 0,
                            is_child_row: s.parent_id.as_deref() == Some(pid.as_str()),
                            is_pinned: true,
                        });
                    }
                }
            }
            if apply_pinned_only {
                rows.retain(|r| r.is_pinned);
            }
            self.sort_display_rows_by(&mut rows, sort, Some(pid.as_str()));
            return rows;
        }

        // Model filter or active search shows exact matching sessions (including subagents that match).
        let force_flat = matches!(filter, Some(SessionFilter::Model(_))) || apply_search;
        let use_grouping = self.group_subagents && !force_flat;

        // Model / day / week filters: rows count only the matching turns of each session.
        let slicer = filter.filter(|f| f.slices_turns());
        let mut sliced_storage: Vec<(usize, Option<SessionInfo>)> = Vec::new();
        // Each item is `(storage_or_cached_idx, is_sliced)`
        let matched_indices: Vec<(usize, bool)> = match slicer {
            Some(f) => {
                for (idx, s) in self.cached_sessions.iter().enumerate() {
                    if let Some(sl) = slice_session(s, f) {
                        if !apply_search || self.session_matches_search_lower(&sl, &q) {
                            sliced_storage.push((idx, Some(sl)));
                        }
                    }
                }
                (0..sliced_storage.len()).map(|i| (i, true)).collect()
            }
            None => self
                .cached_sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| filter.map(|f| f.matches(s)).unwrap_or(true))
                .filter(|(_, s)| !apply_search || self.session_matches_search_lower(s, &q))
                .map(|(idx, _)| (idx, false))
                .collect(),
        };

        let mut rows = if !use_grouping {
            matched_indices
                .into_iter()
                .map(|(idx, is_sliced)| {
                    if is_sliced {
                        let (sess_idx, sl_opt) = &mut sliced_storage[idx];
                        let sl = sl_opt.take().expect("sliced item taken once");
                        let counts = sl.counts;
                        let primary_model = sl.primary_model.clone();
                        let last_time = sl.last_time;
                        let is_child_row = sl.is_subagent;
                        let is_pinned = self.pinned_ids.contains(&sl.conversation_id);
                        DisplaySessionRow {
                            sess_idx: *sess_idx,
                            out_of_scope: false,
                            sliced: Some(Box::new(sl)),
                            counts,
                            primary_model,
                            last_time,
                            subagent_count: 0,
                            is_child_row,
                            is_pinned,
                        }
                    } else {
                        let s = &self.cached_sessions[idx];
                        DisplaySessionRow {
                            sess_idx: idx,
                            out_of_scope: false,
                            sliced: None,
                            counts: s.counts,
                            primary_model: s.primary_model.clone(),
                            last_time: s.last_time,
                            subagent_count: 0,
                            is_child_row: s.is_subagent,
                            is_pinned: self.pinned_ids.contains(&s.conversation_id),
                        }
                    }
                })
                .collect()
        } else {
            // Borrow `conversation_id` / `parent_id` directly from `self.cached_sessions` (unchanged by slicing)
            // so grouping allocates ZERO UUID `String` clones.
            let orig_idx_of = |idx: usize, is_sliced: bool| -> usize {
                if is_sliced {
                    sliced_storage[idx].0
                } else {
                    idx
                }
            };
            let mut present_ids: HashSet<&str> = HashSet::with_capacity(matched_indices.len());
            for &(idx, is_sliced) in &matched_indices {
                present_ids.insert(self.cached_sessions[orig_idx_of(idx, is_sliced)].conversation_id.as_str());
            }
            let mut rollup: HashMap<&str, (usize, TokenCounts, Option<DateTime<Local>>)> =
                HashMap::new();
            for &(idx, is_sliced) in &matched_indices {
                let base = &self.cached_sessions[orig_idx_of(idx, is_sliced)];
                if let Some(pid) = base.parent_id.as_deref() {
                    if present_ids.contains(pid) {
                        let (counts, last_time) = if is_sliced {
                            let sl = sliced_storage[idx].1.as_ref().expect("sliced present");
                            (&sl.counts, sl.last_time)
                        } else {
                            (&base.counts, base.last_time)
                        };
                        let e = rollup
                            .entry(pid)
                            .or_insert((0, TokenCounts::default(), None));
                        e.0 += 1;
                        e.1.merge(counts);
                        if let Some(kt) = last_time {
                            if e.2.map(|pt| kt > pt).unwrap_or(true) {
                                e.2 = Some(kt);
                            }
                        }
                    }
                }
            }

            let mut out = Vec::with_capacity(matched_indices.len());
            for (idx, is_sliced) in matched_indices {
                let orig_idx = if is_sliced { sliced_storage[idx].0 } else { idx };
                let base = &self.cached_sessions[orig_idx];
                if base
                    .parent_id
                    .as_deref()
                    .is_some_and(|pid| present_ids.contains(pid))
                {
                    continue;
                }
                if is_sliced {
                    let (sess_idx, sl_opt) = &mut sliced_storage[idx];
                    let sl = sl_opt.take().expect("sliced item taken once");
                    let mut counts = sl.counts;
                    let mut last_time = sl.last_time;
                    let mut sub_cnt = 0usize;
                    if let Some((sc, rc, rlt)) = rollup.get(base.conversation_id.as_str()) {
                        sub_cnt = *sc;
                        counts.merge(rc);
                        if let Some(kt) = *rlt {
                            if last_time.map(|pt| kt > pt).unwrap_or(true) {
                                last_time = Some(kt);
                            }
                        }
                    }
                    let primary_model = sl.primary_model.clone();
                    let is_child_row = sl.is_subagent;
                    let is_pinned = self.pinned_ids.contains(&sl.conversation_id);
                    out.push(DisplaySessionRow {
                        sess_idx: *sess_idx,
                        out_of_scope: false,
                        sliced: Some(Box::new(sl)),
                        counts,
                        primary_model,
                        last_time,
                        subagent_count: sub_cnt,
                        is_child_row,
                        is_pinned,
                    });
                } else {
                    let mut counts = base.counts;
                    let mut last_time = base.last_time;
                    let mut sub_cnt = 0usize;
                    if let Some((sc, rc, rlt)) = rollup.get(base.conversation_id.as_str()) {
                        sub_cnt = *sc;
                        counts.merge(rc);
                        if let Some(kt) = *rlt {
                            if last_time.map(|pt| kt > pt).unwrap_or(true) {
                                last_time = Some(kt);
                            }
                        }
                    }
                    out.push(DisplaySessionRow {
                        sess_idx: idx,
                        out_of_scope: false,
                        sliced: None,
                        counts,
                        primary_model: base.primary_model.clone(),
                        last_time,
                        subagent_count: sub_cnt,
                        is_child_row: base.is_subagent,
                        is_pinned: self.pinned_ids.contains(&base.conversation_id),
                    });
                }
            }
            out
        };

        // Append out-of-scope pinned sessions for the Sessions tab (Scheme A: always reachable
        // without polluting DateScope aggregates / Hero stats / exports). Skip when user explicitly
        // drilled into a specific Date or Week bar/cell.
        let allow_oos = include_out_of_scope_pins
            && !self.pinned_out_of_scope.is_empty()
            && !matches!(filter, Some(SessionFilter::Date(_) | SessionFilter::Week(_)));
        if allow_oos {
            let existing: HashSet<&str> = rows
                .iter()
                .map(|r| {
                    if let Some(ref s) = r.sliced {
                        s.conversation_id.as_str()
                    } else if r.out_of_scope {
                        self.pinned_out_of_scope[r.sess_idx].conversation_id.as_str()
                    } else {
                        self.cached_sessions[r.sess_idx].conversation_id.as_str()
                    }
                })
                .collect();
            let mut oos_to_push = Vec::new();
            for (idx, s) in self.pinned_out_of_scope.iter().enumerate() {
                if existing.contains(s.conversation_id.as_str()) {
                    continue;
                }
                if let Some(f) = filter {
                    if !f.matches(s) {
                        continue;
                    }
                }
                if apply_search && !self.session_matches_search_lower(s, &q) {
                    continue;
                }
                let sliced = slicer.and_then(|f| slice_session(s, f)).map(Box::new);
                let eff = sliced.as_deref().unwrap_or(s);
                oos_to_push.push(DisplaySessionRow {
                    sess_idx: idx,
                    out_of_scope: true,
                    counts: eff.counts,
                    primary_model: eff.primary_model.clone(),
                    last_time: eff.last_time,
                    subagent_count: 0,
                    is_child_row: eff.is_subagent,
                    is_pinned: true,
                    sliced,
                });
            }
            drop(existing);
            rows.extend(oos_to_push);
        }

        if apply_pinned_only {
            rows.retain(|r| r.is_pinned);
        }
        if pin_first {
            self.sort_display_rows_by(&mut rows, sort, None);
        } else {
            rows.sort_by(|a, b| b.last_time.cmp(&a.last_time));
        }
        rows
    }

    // ---------- Call Log (per-step records derived from already-scanned turns; no extra file access) ----------

    fn call_session(&self, r: &CallRow) -> &SessionInfo {
        if r.out_of_scope {
            &self.pinned_out_of_scope[r.sess_idx]
        } else {
            &self.cached_sessions[r.sess_idx]
        }
    }

    fn make_call_rows_key(&self) -> CallRowsKey {
        CallRowsKey {
            data_gen: self.data_gen,
            sess_ptr: self.cached_sessions.as_ptr() as usize,
            sess_len: self.cached_sessions.len(),
            oos_len: self.pinned_out_of_scope.len(),
            call_sort: self.call_sort,
            privacy_mode: self.privacy_mode,
            call_session_filter: self.call_session_filter.clone(),
            search_query: self.search_query.clone(),
        }
    }

    /// Borrows cached sorted call rows + precomputed totals/speed summary.
    fn with_call_rows<R>(
        &self,
        f: impl FnOnce(&[CallRow], TokenCounts, u64, u64) -> R,
    ) -> R {
        let key = self.make_call_rows_key();
        let hit = self
            .call_rows_cache
            .borrow()
            .as_ref()
            .is_some_and(|c| c.key == key);
        if !hit {
            let (rows, sum, speed_out, speed_secs) = self.build_call_rows_and_summary();
            *self.call_rows_cache.borrow_mut() = Some(CallRowsCache {
                key,
                rows,
                sum,
                speed_out,
                speed_secs,
            });
        }
        let borrow = self.call_rows_cache.borrow();
        let c = borrow.as_ref().expect("call_rows_cache populated");
        f(&c.rows, c.sum, c.speed_out, c.speed_secs)
    }

    /// Builds the call log for the current date scope, honoring the shared search box and the
    /// optional single-session drill-down (falling back to `pinned_out_of_scope` when inspecting
    /// an out-of-scope pinned session's calls). Search matches redacted session labels under
    /// privacy mode (same no-oracle rule as the Sessions tab), plus model and tool names.
    fn build_call_rows_and_summary(&self) -> (Vec<CallRow>, TokenCounts, u64, u64) {
        let q = self.search_query.trim().to_ascii_lowercase();
        let mut rows = Vec::new();
        let mut sum = TokenCounts::default();
        let mut speed_out = 0u64;
        let mut speed_secs = 0u64;

        let mut push_session_calls = |out_of_scope: bool, si: usize, s: &SessionInfo| {
            let session_hit = q.is_empty() || {
                let title = self.format_session_title(s);
                contains_ascii_icase(&title, &q)
                    || contains_ascii_icase(&self.format_session_id(&s.conversation_id), &q)
                    || contains_ascii_icase(&self.format_workspace_label(&s.workspace), &q)
                    || source_matches_query(s.source, &q)
            };
            let mut prev_ts: Option<chrono::DateTime<Utc>> = None;
            for (ti, t) in s.turns.iter().enumerate() {
                let gap_secs = match (prev_ts, t.timestamp_utc) {
                    (Some(p), Some(cur)) => Some((cur - p).num_seconds()).filter(|g| *g >= 0),
                    _ => None,
                };
                if t.timestamp_utc.is_some() {
                    prev_ts = t.timestamp_utc;
                }
                let hit = session_hit
                    || contains_ascii_icase(&t.model, &q)
                    || t.tool_names.iter().any(|n| contains_ascii_icase(n, &q));
                if hit {
                    rows.push(CallRow {
                        out_of_scope,
                        sess_idx: si,
                        turn_idx: ti,
                        gap_secs,
                    });
                    sum.add_call_with_cost(
                        t.input_tokens,
                        t.cache_read_tokens,
                        t.cache_write_tokens,
                        t.output_tokens,
                        t.cost,
                    );
                    if t.output_tps().is_some() {
                        speed_out += t.output_tokens;
                        speed_secs += t.gen_secs.unwrap_or(0) as u64;
                    }
                }
            }
        };

        let mut matched_filtered_session = false;
        for (si, s) in self.cached_sessions.iter().enumerate() {
            if let Some(cid) = self.call_session_filter.as_deref() {
                if s.conversation_id != cid {
                    continue;
                }
                matched_filtered_session = true;
            }
            push_session_calls(false, si, s);
        }
        // Fallback for `ShowSessionCalls` triggered on an out-of-scope pinned session.
        if !matched_filtered_session {
            if let Some(cid) = self.call_session_filter.as_deref() {
                if let Some((si, s)) = self
                    .pinned_out_of_scope
                    .iter()
                    .enumerate()
                    .find(|(_, s)| s.conversation_id == cid)
                {
                    push_session_calls(true, si, s);
                }
            }
        }
        let turn = |r: &CallRow| &self.call_session(r).turns[r.turn_idx];
        let recent = |a: &CallRow, b: &CallRow| {
            let (ta, tb) = (turn(a), turn(b));
            tb.timestamp_utc
                .cmp(&ta.timestamp_utc)
                .then_with(|| tb.step_index.cmp(&ta.step_index))
        };
        match self.call_sort {
            CallSort::Recent => rows.sort_by(recent),
            CallSort::Cost => rows.sort_by(|a, b| {
                turn(b)
                    .cost
                    .total_usd()
                    .partial_cmp(&turn(a).cost.total_usd())
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| recent(a, b))
            }),
            CallSort::Total => rows.sort_by(|a, b| {
                turn(b).total_tokens().cmp(&turn(a).total_tokens()).then_with(|| recent(a, b))
            }),
            CallSort::Duration => rows.sort_by(|a, b| {
                turn(b).gen_secs.cmp(&turn(a).gen_secs).then_with(|| recent(a, b))
            }),
            CallSort::Input => rows.sort_by(|a, b| {
                let fresh = |r: &CallRow| turn(r).input_tokens + turn(r).cache_write_tokens;
                fresh(b).cmp(&fresh(a)).then_with(|| recent(a, b))
            }),
            CallSort::Output => rows.sort_by(|a, b| {
                turn(b).output_tokens.cmp(&turn(a).output_tokens).then_with(|| recent(a, b))
            }),
        }
        (rows, sum, speed_out, speed_secs)
    }

    #[cfg(test)]
    fn call_rows(&self) -> Vec<CallRow> {
        self.with_call_rows(|rows, _, _, _| rows.to_vec())
    }

    fn call_key(&self, r: &CallRow) -> (String, i64) {
        let s = self.call_session(r);
        (s.conversation_id.clone(), s.turns[r.turn_idx].step_index)
    }

    fn selected_call_index(&self, rows: &[CallRow]) -> Option<usize> {
        let (cid, step) = self.selected_call.as_ref()?;
        rows.iter().position(|r| {
            let s = self.call_session(r);
            s.conversation_id == *cid && s.turns[r.turn_idx].step_index == *step
        })
    }

    fn step_call(&mut self, delta: isize) -> bool {
        let next_sel = self.with_call_rows(|rows, _, _, _| {
            if rows.is_empty() {
                return None;
            }
            let cur_idx = self.selected_call_index(rows);
            let cur = cur_idx.unwrap_or(0);
            let next = (cur as isize + delta).clamp(0, (rows.len() - 1) as isize) as usize;
            if next == cur && cur_idx.is_some() {
                return None;
            }
            Some((next, self.call_key(&rows[next])))
        });
        let Some((next, key)) = next_sel else {
            return false;
        };
        self.selected_call = Some(key);
        let vis = self.sessions_vis_rows.max(1);
        if next < self.scroll_rows {
            self.scroll_rows = next;
        } else if next >= self.scroll_rows + vis {
            self.scroll_rows = (next + 1).saturating_sub(vis);
        }
        true
    }

    fn call_tools_label(t: &TurnRecord, max_shown: usize) -> String {
        if t.is_checkpoint {
            return "上下文压缩".to_string();
        }
        if t.tool_names.is_empty() {
            return "—".to_string();
        }
        // Collapse repeats (parallel calls of the same tool) into `name×N`, keeping first-seen order.
        let mut groups: Vec<(&str, usize)> = Vec::new();
        for name in t.tool_names.iter() {
            match groups.iter_mut().find(|g| g.0 == name.as_ref()) {
                Some(g) => g.1 += 1,
                None => groups.push((name.as_ref(), 1)),
            }
        }
        let shown: Vec<String> = groups
            .iter()
            .take(max_shown)
            .map(|&(n, c)| if c > 1 { format!("{n}×{c}") } else { n.to_string() })
            .collect();
        let more = groups.len().saturating_sub(max_shown);
        if more > 0 {
            format!("{} +{}", shown.join(", "), more)
        } else {
            shown.join(", ")
        }
    }

    fn format_call_detail_clipboard(&self, s: &SessionInfo, t: &TurnRecord, gap_secs: Option<i64>) -> String {
        let ws = self.format_workspace_short(&s.workspace);
        let time = t
            .timestamp_utc
            .map(|u| u.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "-".into());
        let cost = if t.cost.is_fully_unpriced(1) {
            "- (未定价)".to_string()
        } else {
            format!(
                "{} (新输入 {} / 缓存读 {} / 输出 {})",
                fmt_usd_precise(t.cost.total_usd()),
                fmt_usd_precise(t.cost.input_plus_write_usd()),
                fmt_usd_precise(t.cost.cache_read_usd),
                fmt_usd_precise(t.cost.output_usd),
            )
        };
        let tools = if t.tool_names.is_empty() {
            "无".to_string()
        } else {
            t.tool_names.join(", ")
        };
        let speed = match (t.gen_secs, t.output_tps()) {
            (Some(g), Some(v)) => format!(
                "{} · {:.0} tok/s (含首字延迟与思考，秒级时间戳估算)",
                fmt_duration_short(g as u64),
                v
            ),
            (Some(g), None) => format!(
                "{} (过短，不计速度)",
                fmt_duration_short(g as u64)
            ),
            (None, _) if s.source.agent_family() != AgentFamily::Antigravity => {
                "- (当前日志无生成耗时记录)".to_string()
            }
            (None, _) => "- (纯回复 / 压缩步骤无结束时间)".to_string(),
        };
        let thinking_disp = format_turn_thinking(t).unwrap_or_else(|| "无".to_string());
        format!(
            "调用明细 · {} · Step #{}\n- 时间: {} | 来源: {} | 会话: {} | 模型: {}{}\n- 估算 API 费用: {}\n- 总 Token: {} | 新输入: {} | 输出: {} | 缓存读: {} (命中 {:.1}%) | 缓存写: {}\n- 生成耗时: {}\n- 工具({}): {} | 思考: {} | 距上一步: {}",
            self.format_session_title(s),
            t.step_index,
            time,
            s.source.family_label(),
            self.format_session_id(&s.conversation_id),
            t.model,
            if ws.is_empty() { String::new() } else { format!(" | 项目: {}", ws) },
            cost,
            fmt_commas(t.total_tokens()),
            fmt_commas(t.input_tokens),
            fmt_commas(t.output_tokens),
            fmt_commas(t.cache_read_tokens),
            t.cache_hit_rate(),
            fmt_commas(t.cache_write_tokens),
            speed,
            t.tool_names.len(),
            tools,
            thinking_disp,
            gap_secs.map(fmt_gap).unwrap_or_else(|| "-".into()),
        )
    }

    /// Per-call CSV (UTF-8 BOM). Only token metadata and tool *names* are exported — never
    /// prompts, tool arguments, or outputs; session labels follow privacy mode.
    pub fn export_calls_csv(&self) -> String {
        let mut out = String::from("\u{FEFF}time,session_id,source,step,model,kind,tools,tool_count,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,reasoning_tokens,total_tokens,context_tokens,cache_hit_pct,thinking_chars,gen_secs,output_tps,gap_secs,cost_usd,workspace,session_title\n");
        self.with_call_rows(|rows, _, _, _| {
            for r in rows {
                let s = self.call_session(r);
                let t = &s.turns[r.turn_idx];
                let time = t
                    .timestamp_utc
                    .map(|u| u.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default();
                let kind = if t.is_checkpoint {
                    "compaction"
                } else if t.tool_names.is_empty() {
                    "reply"
                } else {
                    "tool_call"
                };
                let cost = if t.cost.is_fully_unpriced(1) {
                    String::new()
                } else {
                    format!("{:.6}", t.cost.total_usd())
                };
                out.push_str(&format!(
                    "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{:.2},{},{},{},{},{},{},{}\n",
                    time,
                    csv_escape(&self.format_session_id(&s.conversation_id)),
                    s.source.as_str(),
                    t.step_index,
                    csv_escape(&t.model),
                    kind,
                    csv_escape(&t.tool_names.join(";")),
                    t.tool_names.len(),
                    t.input_tokens,
                    t.cache_read_tokens,
                    t.cache_write_tokens,
                    t.output_tokens,
                    t.reasoning_tokens,
                    t.total_tokens(),
                    t.total_input(),
                    t.cache_hit_rate(),
                    t.thinking_chars,
                    t.gen_secs.map(|g| g.to_string()).unwrap_or_default(),
                    t.output_tps().map(|v| format!("{v:.1}")).unwrap_or_default(),
                    r.gap_secs.map(|g| g.to_string()).unwrap_or_default(),
                    cost,
                    csv_escape(&self.format_workspace_label(&s.workspace)),
                    csv_escape(&self.format_session_title(s)),
                ));
            }
        });
        out
    }

    fn ordered_models_of<'a>(
        by_model: &'a BTreeMap<String, TokenCounts>,
        sort: ModelSort,
    ) -> (Vec<(&'a str, TokenCounts)>, Vec<(&'a str, TokenCounts)>) {
        let mut primary: Vec<(&'a str, TokenCounts)> = Vec::with_capacity(by_model.len());
        let mut secondary: Vec<(&'a str, TokenCounts)> = Vec::new();
        for (name, c) in by_model {
            if is_compaction_model(name) {
                secondary.push((name.as_str(), *c));
            } else {
                primary.push((name.as_str(), *c));
            }
        }
        match sort {
            ModelSort::TotalDesc => {
                primary.sort_by(|a, b| b.1.total_tokens().cmp(&a.1.total_tokens()));
            }
            ModelSort::CostDesc => {
                primary.sort_by(|a, b| {
                    b.1.cost
                        .total_usd()
                        .partial_cmp(&a.1.cost.total_usd())
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| b.1.total_tokens().cmp(&a.1.total_tokens()))
                });
            }
            ModelSort::HitRateAsc => {
                primary.sort_by(|a, b| {
                    a.1.cache_hit_rate()
                        .partial_cmp(&b.1.cache_hit_rate())
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| b.1.total_tokens().cmp(&a.1.total_tokens()))
                });
            }
            ModelSort::HitRateDesc => {
                primary.sort_by(|a, b| {
                    b.1.cache_hit_rate()
                        .partial_cmp(&a.1.cache_hit_rate())
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| b.1.total_tokens().cmp(&a.1.total_tokens()))
                });
            }
        }
        secondary.sort_by(|a, b| b.1.total_tokens().cmp(&a.1.total_tokens()));
        (primary, secondary)
    }

    fn ordered_models(&self, sort: ModelSort) -> (Vec<(&str, TokenCounts)>, Vec<(&str, TokenCounts)>) {
        Self::ordered_models_of(&self.cached_agg.by_model, sort)
    }

    pub fn set_pricing_status(&mut self, msg: impl Into<String>, is_error: bool) {
        let dur = if is_error { 4500 } else { 2600 };
        self.pricing_status = Some((
            msg.into(),
            is_error,
            Instant::now() + std::time::Duration::from_millis(dur),
        ));
    }

    pub fn set_banner_status(&mut self, msg: impl Into<String>, is_error: bool) {
        let dur = if is_error { 4200 } else { 2400 };
        self.banner_status = Some((
            msg.into(),
            is_error,
            Instant::now() + std::time::Duration::from_millis(dur),
        ));
    }

    pub fn is_pricing_editor_active(&self) -> bool {
        self.active_tab == Tab::Models && self.model_view == ModelViewMode::Pricing
    }

    pub fn is_budget_editor_active(&self) -> bool {
        self.active_tab == Tab::Models && self.model_view == ModelViewMode::Budget
    }

    pub fn is_search_active(&self) -> bool {
        matches!(self.active_tab, Tab::Sessions | Tab::Calls)
    }

    /// Whether the active tab is filtered by the top-bar `DateScope` selector.
    /// `按天` and `按周` (incl. `按月`) are full-history time-series views and stay independent of narrow scopes.
    pub fn tab_uses_scope(&self) -> bool {
        !matches!(self.active_tab, Tab::Daily | Tab::Weekly)
    }

    pub fn is_custom_date_active(&self) -> bool {
        self.tab_uses_scope() && self.scope == DateScope::Custom
    }

    fn scope_covers_filter(
        scope: DateScope,
        custom_since: Option<NaiveDate>,
        custom_until: Option<NaiveDate>,
        filter: &SessionFilter,
        today: NaiveDate,
    ) -> bool {
        let (since, until): (Option<NaiveDate>, Option<NaiveDate>) = match scope {
            DateScope::Today => (Some(today), Some(today)),
            DateScope::Days7 => (today.checked_sub_days(chrono::Days::new(6)), Some(today)),
            DateScope::Days30 => (today.checked_sub_days(chrono::Days::new(29)), Some(today)),
            DateScope::All => return true,
            DateScope::Custom => (custom_since, custom_until),
        };
        match filter {
            SessionFilter::Date(day_str) => {
                let Ok(d) = NaiveDate::parse_from_str(day_str, "%Y-%m-%d") else {
                    return true;
                };
                since.map(|s| d >= s).unwrap_or(true) && until.map(|u| d <= u).unwrap_or(true)
            }
            SessionFilter::Week(wk_str) => {
                let Some((mon, sun)) = iso_week_bounds(wk_str) else {
                    return true;
                };
                let eff_end = sun.min(today).max(mon);
                since.map(|s| mon >= s).unwrap_or(true) && until.map(|u| eff_end <= u).unwrap_or(true)
            }
            _ => true,
        }
    }

    pub fn format_budget_fields(cfg: &BudgetConfig) -> [String; 4] {
        let fmt_b = |v: f64| if v > 0.0 { format!("{v:.2}") } else { String::new() };
        [
            fmt_b(cfg.daily_usd),
            fmt_b(cfg.weekly_usd),
            format!("{:.0}", cfg.warn_pct),
            format!("{:.0}", cfg.critical_pct),
        ]
    }

    pub fn format_custom_date_fields(&self) -> [String; 2] {
        [
            self.custom_since
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_default(),
            self.custom_until
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_default(),
        ]
    }

    pub fn format_statusline_config_snippet() -> String {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("agy-statusline.exe")));
        let path_str = exe_dir
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "agy-statusline.exe".to_string());
        let escaped = path_str.replace('\\', "\\\\");
        format!(
            "{{\n  \"statusLine\": {{\n    \"type\": \"command\",\n    \"command\": \"\\\"{}\\\"\",\n    \"stack_with_default\": false\n  }}\n}}",
            escaped
        )
    }

    pub fn export_sessions_csv(&self) -> String {
        let mut out = String::from("\u{FEFF}id,source,archived,last_active,calls,active_secs,thinking_turns,model,cache_hit_pct,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,total_tokens,cost_usd,peak_context,workspace,title\n");
        let csv_esc = |s: &str| {
            if s.contains([',', '"', '\n', '\r']) {
                format!("\"{}\"", s.replace('"', "\"\""))
            } else {
                s.to_string()
            }
        };
        for row in self.display_sessions(true) {
            let s = self.row_session(&row);
            let sid = self.format_session_id(&s.conversation_id);
            let title = self.format_session_title(s);
            let ws = self.format_workspace_label(&s.workspace);
            let last = row
                .last_time
                .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default();
            let c = row.counts;
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{},{:.2},{},{},{},{},{},{:.6},{},{},{}\n",
                csv_esc(&sid),
                s.source.as_str(),
                s.is_archived,
                csv_esc(&last),
                c.calls,
                s.active_duration_secs(),
                s.thinking_turns,
                csv_esc(&row.primary_model),
                c.cache_hit_rate(),
                c.input_tokens,
                c.cache_read_tokens,
                c.cache_write_tokens,
                c.output_tokens,
                c.total_tokens(),
                c.cost.total_usd(),
                c.peak_context,
                csv_esc(&ws),
                csv_esc(&title),
            ));
        }
        out
    }

    pub fn export_daily_csv(&self, snapshot: &ScanSnapshot) -> String {
        let by_day = if self.tab_uses_scope() {
            &self.cached_agg.by_day
        } else {
            &snapshot.agg.by_day
        };
        let mut out = String::from("\u{FEFF}date,calls,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,total_tokens,non_cached_tokens,cache_hit_pct,cost_usd,cache_saved_usd,peak_context\n");
        for (day, c) in by_day {
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{},{:.2},{:.6},{:.6},{}\n",
                day,
                c.calls,
                c.input_tokens,
                c.cache_read_tokens,
                c.cache_write_tokens,
                c.output_tokens,
                c.total_tokens(),
                c.non_cached_tokens(),
                c.cache_hit_rate(),
                c.cost.total_usd(),
                c.cost.cache_saved_usd,
                c.peak_context,
            ));
        }
        out
    }

    pub fn export_snapshot_json(&self, snapshot: &ScanSnapshot) -> String {
        let tc_json = |c: &TokenCounts| {
            json!({
                "calls": c.calls,
                "input_tokens": c.input_tokens,
                "cache_read_tokens": c.cache_read_tokens,
                "cache_write_tokens": c.cache_write_tokens,
                "output_tokens": c.output_tokens,
                "total_tokens": c.total_tokens(),
                "non_cached_tokens": c.non_cached_tokens(),
                "cache_hit_rate_pct": (c.cache_hit_rate() * 100.0).round() / 100.0,
                "peak_context": c.peak_context,
                "cost_usd": (c.cost.total_usd() * 1e6).round() / 1e6,
                "cache_saved_usd": (c.cost.cache_saved_usd * 1e6).round() / 1e6,
            })
        };
        let use_full = !self.tab_uses_scope() && self.scope != DateScope::All;
        let full_redactor = if use_full && self.privacy_mode {
            Some(Redactor::build(&snapshot.sessions, &snapshot.agg))
        } else {
            None
        };
        let agg_ref = if use_full { &snapshot.agg } else { &self.cached_agg };
        let scope_str = if use_full {
            format!("{:?}", DateScope::All)
        } else {
            format!("{:?}", self.scope)
        };
        let sessions_v: Vec<Value> = if use_full {
            snapshot
                .sessions
                .iter()
                .map(|s| {
                    let (sid, title, ws) = if let Some(red) = full_redactor.as_ref() {
                        (
                            red.session_id(&s.conversation_id),
                            red.session_title(s),
                            red.workspace_label(&s.workspace),
                        )
                    } else {
                        let short_ws = workspace_short_name(&s.workspace);
                        (
                            s.conversation_id.chars().take(8).collect(),
                            s.title.clone(),
                            if short_ws.is_empty() {
                                "(未知工作区)".to_string()
                            } else {
                                short_ws
                            },
                        )
                    };
                    json!({
                        "id": sid,
                        "title": title,
                        "workspace": ws,
                        "source": s.source.as_str(),
                        "is_archived": s.is_archived,
                        "primary_model": s.primary_model(),
                        "active_duration_secs": s.active_duration_secs(),
                        "thinking_turns": s.thinking_turns,
                        "tools": s.tool_counts(),
                        "stats": tc_json(&s.counts),
                    })
                })
                .collect()
        } else {
            self.display_sessions(true)
                .into_iter()
                .map(|r| {
                    let s = self.row_session(&r);
                    json!({
                        "id": self.format_session_id(&s.conversation_id),
                        "title": self.format_session_title(s),
                        "workspace": self.format_workspace_label(&s.workspace),
                        "source": s.source.as_str(),
                        "is_archived": s.is_archived,
                        "primary_model": r.primary_model.as_ref(),
                        "active_duration_secs": s.active_duration_secs(),
                        "thinking_turns": s.thinking_turns,
                        "tools": s.tool_counts(),
                        "stats": tc_json(&r.counts),
                    })
                })
                .collect()
        };
        let by_day: BTreeMap<&String, Value> = agg_ref
            .by_day
            .iter()
            .map(|(k, c)| (k, tc_json(c)))
            .collect();
        let by_model: BTreeMap<&String, Value> = agg_ref
            .by_model
            .iter()
            .map(|(k, c)| (k, tc_json(c)))
            .collect();
        let by_agent: BTreeMap<&str, Value> = agg_ref
            .by_agent
            .iter()
            .map(|(fam, (cnt, c))| {
                (
                    fam.as_str(),
                    json!({
                        "label": fam.label_full(),
                        "sessions": cnt,
                        "stats": tc_json(c),
                    }),
                )
            })
            .collect();
        let payload = json!({
            "exported_at": Utc::now().to_rfc3339(),
            "redacted": self.privacy_mode,
            "scope": scope_str,
            "overview": tc_json(&agg_ref.total),
            "active_duration_secs": agg_ref.total_active_secs,
            "thinking_turns": agg_ref.total_thinking_turns,
            "by_agent": by_agent,
            "by_tool": agg_ref.by_tool,
            "by_day": by_day,
            "by_model": by_model,
            "sessions": sessions_v,
            "archived_sessions_count": snapshot.archived_sessions_count,
        });
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into())
    }

    pub fn format_row_fields(row: &PricingTableRow) -> [String; 5] {
        let model = row.model.clone();
        if let Some(r) = row.rate {
            [
                model,
                fmt_rate_plain(r.input),
                fmt_rate_plain(r.cache_read),
                fmt_rate_plain(r.cache_write),
                fmt_rate_plain(r.output),
            ]
        } else {
            [model, String::new(), "0.00".to_string(), String::new(), String::new()]
        }
    }

    pub fn mark_data_dirty(&mut self) {
        self.dirty_filter = true;
    }

    pub fn invalidate_surface(&mut self) {
        self.surface = None;
        self.last_hero_telemetry_sig = None;
    }

    /// Releases ALL heavy rendering & view-state resources when the dashboard hides:
    ///   - Drops `WindowSurface` (DIB + `ID2D1DCRenderTarget` + brushes) FIRST
    ///   - Drops isolated `dash_factories` (`ID2D1Factory` + `IDWriteFactory` + D3D10Warp pools + measure cache)
    ///   - Frees cloned `cached_sessions`, `pinned_out_of_scope`, `cached_agg`, `redactor`,
    ///     hit-test vectors, and derived row/detail caches
    /// Preserves user navigation/filter/theme preferences for instant reopen fidelity.
    pub fn on_hide(&mut self) {
        self.mouse_dip = None;
        self.pricing_focused_field = None;
        self.search_focused = false;
        self.custom_date_focused = None;
        self.pricing_field_rects = None;
        self.budget_field_rects = None;
        self.search_field_rect = None;
        self.custom_date_rects = None;
        self.export_btn_rect = None;
        self.detail_more_btn_rect = None;
        self.last_hover_sig = HoverSig::default();
        self.last_hero_telemetry_sig = None;
        self.hero_block4_layout = None;
        self.last_full_paint = None;
        self.had_active_toast = false;

        // Clear transient banners/toast timers and shrink empty search buffer capacity.
        self.banner_status = None;
        self.pricing_status = None;
        self.copied_until = None;
        self.session_copied_until = None;
        self.resume_cmd_copied_until = None;
        self.call_copied_until = None;
        if self.search_query.is_empty() {
            self.search_query.shrink_to_fit();
        }

        // Release surface strictly BEFORE dropping the isolated dashboard D2D factory.
        self.surface = None;
        self.dash_factories = None;

        self.cached_sessions.clear();
        self.cached_sessions.shrink_to_fit();
        self.pinned_out_of_scope.clear();
        self.pinned_out_of_scope.shrink_to_fit();
        self.pinned_ids.clear();
        self.pinned_ids.shrink_to_fit();
        self.cached_agg = AggregatedStats::default();
        *self.redactor.get_mut() = Redactor::default();
        self.redactor_built.set(false);
        self.cached_root_sessions = 0;
        self.cached_sub_sessions = 0;

        self.hit_boxes.clear();
        self.hit_boxes.shrink_to_fit();
        self.session_row_rects.clear();
        self.session_row_rects.shrink_to_fit();
        self.plot_rects.clear();
        self.plot_rects.shrink_to_fit();
        self.discrete_plots.clear();
        self.discrete_plots.shrink_to_fit();

        self.clear_view_caches();
        self.cached_alert_level = None;
        self.data_gen = self.data_gen.wrapping_add(1);
        self.dirty_filter = true;
    }

    fn has_active_toast(&self, now: Instant) -> bool {
        self.copied_until.is_some_and(|t| now < t)
            || self.session_copied_until.is_some_and(|t| now < t)
            || self.resume_cmd_copied_until.is_some_and(|t| now < t)
            || self.call_copied_until.is_some_and(|t| now < t)
            || self.banner_status.as_ref().is_some_and(|(_, _, t)| now < *t)
            || self.pricing_status.as_ref().is_some_and(|(_, _, t)| now < *t)
    }

    /// Decides the minimal repaint required for a 1s telemetry tick while the dashboard is visible:
    ///   - `Full` on `Tab::System`, date rollover, dirty data, toast expiry, or periodic countdown refresh
    ///   - `Skip` when Hero Block 4 displayed CPU%/RAM%/GB values are unchanged
    ///   - `PartialHero` when only Hero Block 4 changed on non-System tabs (~2.5% of pixels)
    pub fn on_telemetry_tick(&mut self, telemetry: &SystemTelemetry) -> TelemetryRepaintMode {
        if self.active_tab == Tab::System || self.dirty_filter {
            return TelemetryRepaintMode::Full;
        }
        let now = Instant::now();
        if self.cached_date != Local::now().date_naive() {
            return TelemetryRepaintMode::Full;
        }
        let toast_now = self.has_active_toast(now);
        if self.had_active_toast && !toast_now {
            return TelemetryRepaintMode::Full;
        }
        let has_live_countdown = (self.active_tab == Tab::Overview
            && self.overview_top == OverviewTopMode::QuotaPace)
            || (self.active_tab == Tab::Models && self.model_view == ModelViewMode::Budget);
        if has_live_countdown
            && self
                .last_full_paint
                .map(|t| now.duration_since(t) >= std::time::Duration::from_secs(15))
                .unwrap_or(true)
        {
            return TelemetryRepaintMode::Full;
        }
        let sig = HeroTelemetrySig::from_telemetry(telemetry);
        if self.last_hero_telemetry_sig == Some(sig) {
            return TelemetryRepaintMode::Skip;
        }
        if self.surface.is_some()
            && self.dash_factories.is_some()
            && self.hero_block4_layout.is_some()
        {
            TelemetryRepaintMode::PartialHero
        } else {
            TelemetryRepaintMode::Full
        }
    }

    /// Repaints ONLY Hero Strip Block 4 (CPU/RAM rings + GB text) into the retained surface DIB
    /// and blits only that dirty rectangle to `target_hdc`. Returns `Err` if the retained surface
    /// or layout is unavailable/stale so the caller falls back to a full `InvalidateRect`.
    #[allow(clippy::too_many_arguments)]
    pub fn repaint_hero_telemetry_only(
        &mut self,
        _shared_factories: &D2DFactories,
        fonts: &Fonts,
        hwnd: HWND,
        target_hdc: HDC,
        dpi: f32,
        telemetry: &SystemTelemetry,
        light_theme: bool,
    ) -> Result<()> {
        let Some(layout) = self.hero_block4_layout else {
            return Err(windows::core::Error::empty());
        };
        if target_hdc.0.is_null() {
            return Err(windows::core::Error::empty());
        }
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut rc);
        }
        let px_w = (rc.right - rc.left).max(1);
        let px_h = (rc.bottom - rc.top).max(1);
        let matches = self
            .surface
            .as_ref()
            .is_some_and(|s| s.matches(px_w, px_h, dpi));
        if !matches || self.dash_factories.is_none() {
            return Err(windows::core::Error::empty());
        }

        let theme = theme_for(light_theme);
        let sys_act = self.active_tab == Tab::System;
        let sys_hov = self
            .mouse_dip
            .map(|(mx, my)| hit_rect(&layout.b4, mx, my))
            .unwrap_or(false);
        let clip_dip = rect_f(
            layout.x3 + 1.0,
            layout.y + 1.0,
            (layout.band.right - layout.x3 - 2.0).max(1.0),
            (layout.h - 2.0).max(1.0),
        );

        let dash_f = self.dash_factories.take().expect("checked dash_factories");
        let surface = self.surface.take().expect("checked surface");
        let res = fonts.with_dash(&dash_f, |df| {
            surface.render_clip_and_blit(Some(target_hdc), clip_dip, |rt| {
                fill_rect(rt, clip_dip, theme.surface);
                Self::draw_hero_block4(
                    &dash_f,
                    rt,
                    df,
                    theme,
                    layout,
                    telemetry,
                    sys_act,
                    sys_hov,
                );
            })
        });
        self.dash_factories = Some(dash_f);
        match res {
            Ok(Ok(())) => {
                self.surface = Some(surface);
                self.last_hero_telemetry_sig = Some(HeroTelemetrySig::from_telemetry(telemetry));
                Ok(())
            }
            Ok(Err(e)) => {
                // Surface lost/invalid; drop it so fallback full paint recreates cleanly.
                self.last_hero_telemetry_sig = None;
                Err(e)
            }
            Err(e) => {
                self.last_hero_telemetry_sig = None;
                Err(e)
            }
        }
    }

    pub fn on_scroll(&mut self, delta: i16) {
        if delta > 0 {
            self.scroll_rows = self.scroll_rows.saturating_sub(2);
        } else if delta < 0 {
            self.scroll_rows = (self.scroll_rows + 2).min(self.max_scroll);
        }
    }

    pub fn is_hovering_interactive(&self) -> bool {
        let Some((mx, my)) = self.mouse_dip else {
            return false;
        };
        self.hit_boxes.iter().any(|hb| hit_rect(&hb.rect, mx, my))
    }

    /// Updates cursor position and returns `true` only when hover visuals actually change:
    /// entering/leaving a hitbox, changing discrete chart slot (bar/cell/row/sample),
    /// or moving inside/leaving a continuous plot rect.
    pub fn on_mouse_move(&mut self, x: f32, y: f32) -> bool {
        self.mouse_dip = Some((x, y));
        let hb_idx = self.hit_boxes.iter().rposition(|hb| hit_rect(&hb.rect, x, y));
        let continuous_plot = self.plot_rects.iter().any(|r| hit_rect(r, x, y));
        let discrete_slot = self
            .discrete_plots
            .iter()
            .enumerate()
            .find_map(|(pi, dp)| dp.hit_slot(x, y).map(|si| (pi, si)));
        let sig = HoverSig {
            hit_box: hb_idx,
            continuous_plot,
            discrete_slot,
        };
        let changed = continuous_plot
            || self.last_hover_sig.continuous_plot
            || sig != self.last_hover_sig;
        self.last_hover_sig = sig;
        changed
    }

    pub fn on_mouse_leave(&mut self) -> bool {
        self.mouse_dip = None;
        let had_hover = self.last_hover_sig.hit_box.is_some()
            || self.last_hover_sig.continuous_plot
            || self.last_hover_sig.discrete_slot.is_some();
        self.last_hover_sig = HoverSig::default();
        had_hover
    }

    pub fn open_sessions_tab(&mut self) {
        self.active_tab = Tab::Sessions;
        self.prev_token_tab = Tab::Sessions;
        self.scroll_rows = 0;
    }

    pub fn hit_test_click(&self, x: f32, y: f32) -> Option<ClickAction> {
        for hb in self.hit_boxes.iter().rev() {
            if hit_rect(&hb.rect, x, y) {
                return Some(hb.action.clone());
            }
        }
        None
    }

    /// Independent session-row hit-tester for right-click context menus (`WM_RBUTTONUP`) on both
    /// the Overview recent list and the Sessions list (immune to nested star button hitboxes).
    pub fn hit_test_session_row(&self, x: f32, y: f32) -> Option<String> {
        for (rect, cid) in self.session_row_rects.iter().rev() {
            if hit_rect(rect, x, y) {
                return Some(cid.clone());
            }
        }
        None
    }

    /// Returns `(need_data_refresh, prefs_changed)`.
    pub fn apply_action(
        &mut self,
        hwnd: HWND,
        action: ClickAction,
        human: &mut bool,
        light_theme: &mut bool,
    ) -> (bool, bool) {
        match action {
            ClickAction::SelectTab(t) => {
                if t != Tab::System {
                    self.prev_token_tab = t;
                }
                self.active_tab = t;
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::ToggleSystemView => {
                if self.active_tab == Tab::System {
                    self.active_tab = self.prev_token_tab;
                } else {
                    self.prev_token_tab = self.active_tab;
                    self.active_tab = Tab::System;
                }
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::SelectDailyView(mode) => {
                self.daily_view = mode;
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::SelectOverviewTop(mode) => {
                self.overview_top = mode;
                self.overview_top_chosen = true;
                (false, false)
            }
            ClickAction::SelectWeeklyMode(m) => {
                self.weekly_mode = m;
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::SelectBreakdown(mode) => {
                self.breakdown_mode = mode;
                (false, false)
            }
            ClickAction::SelectModelCardMetric(metric) => {
                self.model_card_metric = metric;
                (false, false)
            }
            ClickAction::SelectModelView(mode) => {
                self.model_view = mode;
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::OpenPricingTable => {
                self.active_tab = Tab::Models;
                self.prev_token_tab = Tab::Models;
                self.model_view = ModelViewMode::Pricing;
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::OpenBudgetView => {
                self.active_tab = Tab::Models;
                self.prev_token_tab = Tab::Models;
                self.model_view = ModelViewMode::Budget;
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::CopyStatuslineConfig => {
                let snippet = Self::format_statusline_config_snippet();
                if copy_text_to_clipboard(hwnd, &snippet) {
                    self.set_banner_status(
                        "已复制 /statusline 配置（粘入 ~/.gemini/antigravity-cli/settings.json 接入 agy 官方配额）",
                        false,
                    );
                    self.set_pricing_status("已复制 Antigravity CLI /statusline 配置片段到剪贴板", false);
                }
                (false, false)
            }
            ClickAction::ClearSearch => {
                self.search_query.clear();
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::PickPricingRow(_)
            | ClickAction::NewPricingEntry
            | ClickAction::SavePricingEntry
            | ClickAction::ResetPricingEntry
            | ClickAction::SaveBudgetEntry
            | ClickAction::ClearBudgetEntry
            | ClickAction::ExportMenu
            | ClickAction::ApplyCustomDate
            | ClickAction::TogglePinSession(_)
            | ClickAction::ResumeSession(_)
            | ClickAction::CopyResumeCmd(_)
            | ClickAction::OpenWorkspace(_)
            | ClickAction::SessionMoreMenu(_) => (false, false),
            ClickAction::ShowBlockedReason(msg) => {
                self.set_banner_status(msg, true);
                (false, false)
            }
            ClickAction::TogglePinnedOnly => {
                self.pinned_only = !self.pinned_only;
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::SelectModelSort(sort) => {
                self.model_sort = sort;
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::SelectScope(s) => {
                self.scope = s;
                self.scroll_rows = 0;
                self.dirty_filter = true;
                (false, true)
            }
            ClickAction::SetCostMode(v) => {
                let changed = self.cost_mode != v;
                self.cost_mode = v;
                (false, changed)
            }
            ClickAction::ToggleCostMode => {
                self.cost_mode = !self.cost_mode;
                (false, true)
            }
            ClickAction::TogglePrivacyMode => {
                self.privacy_mode = !self.privacy_mode;
                if !self.privacy_mode {
                    *self.redactor.get_mut() = Redactor::default();
                    self.redactor_built.set(false);
                }
                (false, true)
            }
            ClickAction::ToggleGroupSubagents => {
                self.group_subagents = !self.group_subagents;
                self.scroll_rows = 0;
                (false, true)
            }
            ClickAction::ToggleFreshOnly => {
                self.fresh_only = !self.fresh_only;
                (false, true)
            }
            ClickAction::ToggleLightTheme => {
                *light_theme = !*light_theme;
                let th = theme_for(*light_theme);
                apply_win11_frame(hwnd, th.is_dark, th.caption_colorref);
                (false, true)
            }
            ClickAction::ToggleHuman => {
                *human = !*human;
                (false, true)
            }
            ClickAction::CopySummary => {
                let use_full = !self.tab_uses_scope();
                let t = if use_full { self.all_total } else { self.cached_agg.total };
                let sess_cnt = if use_full { self.all_sessions_count } else { self.cached_sessions.len() };
                let scope_lbl = if use_full {
                    "全量历史"
                } else {
                    match self.scope {
                        DateScope::Today => "今日",
                        DateScope::Days7 => "近7天",
                        DateScope::Days30 => "近30天",
                        DateScope::All => "全部",
                        DateScope::Custom => "自定义范围",
                    }
                };
                let brand_hdr = "Gauge ";
                let text = if self.active_tab == Tab::Models {
                    let (primary, secondary) = self.ordered_models(self.model_sort);
                    let mut lines = vec![
                        format!("{brand_hdr}按模型用量与估算费用 ({})", scope_lbl),
                        "| 模型 | 调用 | 命中率 | 稳定命中(第2轮起) | 未命中 | 输出速度(tok/s) | 新增输入 | 缓存读取 | 缓存写入 | 输出 | 总 Token | 估算费用(USD) | 峰值Ctx |".to_string(),
                        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |".to_string(),
                    ];
                    let fmt_row = |name: &str, c: &TokenCounts, mcs: Option<&ModelCacheStats>, tag: &str| {
                        let mt = mcs
                            .and_then(ModelCacheStats::multi_turn_hit_rate)
                            .map(|r| format!("{r:.1}%"))
                            .unwrap_or_else(|| "-".into());
                        let miss = mcs
                            .filter(|m| m.miss_count > 0)
                            .map(|m| format!("{}次", m.miss_count))
                            .unwrap_or_else(|| "-".into());
                        let speed = mcs
                            .and_then(ModelCacheStats::output_tps)
                            .map(|v| format!("{v:.0}"))
                            .unwrap_or_else(|| "-".into());
                        format!(
                            "| {}{} | {} | {:.1}% | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                            name,
                            tag,
                            c.calls,
                            c.cache_hit_rate(),
                            mt,
                            miss,
                            speed,
                            fmt_compact(c.input_tokens),
                            fmt_compact(c.cache_read_tokens),
                            fmt_compact(c.cache_write_tokens),
                            fmt_compact(c.output_tokens),
                            fmt_compact(c.total_tokens()),
                            fmt_cost(&c.cost, c.calls, false),
                            fmt_compact(c.peak_context),
                        )
                    };
                    for &(name, ref c) in &primary {
                        lines.push(fmt_row(name, c, self.cached_agg.by_model_cache.get(name), ""));
                    }
                    for &(name, ref c) in &secondary {
                        lines.push(fmt_row(name, c, self.cached_agg.by_model_cache.get(name), " [次要]"));
                    }
                    lines.push(format!(
                        "\n总计: {} 次调用 · 总 Token {} · 估算费用 {} (缓存已省 {}) · 综合缓存命中 {:.1}% · 峰值Ctx {}",
                        t.calls,
                        fmt_commas(t.total_tokens()),
                        fmt_cost(&t.cost, t.calls, false),
                        fmt_usd_precise(t.cost.cache_saved_usd),
                        t.cache_hit_rate(),
                        fmt_commas(t.peak_context),
                    ));
                    lines.join("\n")
                } else {
                    format!(
                        "{brand_hdr}Token & API 费用统计 ({})\n- 会话: {} | 调用: {}\n- 估算 API 费用: {} (新输入 {} / 缓存读 {} / 输出 {} · 缓存已省 {})\n- 新增输入: {} | 缓存读取: {} (命中 {:.1}%) | 缓存写入: {}\n- 输出: {} | 总 Token: {} (非缓存增量: {})\n- 峰值Ctx: {}",
                        scope_lbl,
                        sess_cnt,
                        t.calls,
                        fmt_cost(&t.cost, t.calls, false),
                        fmt_usd_precise(t.cost.input_plus_write_usd()),
                        fmt_usd_precise(t.cost.cache_read_usd),
                        fmt_usd_precise(t.cost.output_usd),
                        fmt_usd_precise(t.cost.cache_saved_usd),
                        fmt_commas(t.input_tokens),
                        fmt_commas(t.cache_read_tokens),
                        t.cache_hit_rate(),
                        fmt_commas(t.cache_write_tokens),
                        fmt_commas(t.output_tokens),
                        fmt_commas(t.total_tokens()),
                        fmt_commas(t.non_cached_tokens()),
                        fmt_commas(t.peak_context),
                    )
                };
                if copy_text_to_clipboard(hwnd, &text) {
                    self.copied_until = Some(Instant::now() + std::time::Duration::from_millis(1600));
                }
                (false, false)
            }
            ClickAction::CopySessionDetail => {
                let text = self
                    .selected_session
                    .as_deref()
                    .and_then(|id| self.session_ref_for_detail(id))
                    .map(|s| self.format_session_detail_clipboard(s.as_ref()));
                if let Some(text) = text {
                    if copy_text_to_clipboard(hwnd, &text) {
                        self.session_copied_until =
                            Some(Instant::now() + std::time::Duration::from_millis(1600));
                    }
                }
                (false, false)
            }
            ClickAction::Refresh => (true, false),
            ClickAction::SelectSort(sort) => {
                self.sort = sort;
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::SelectSession(cid) => {
                self.selected_session = Some(cid);
                (false, false)
            }
            ClickAction::InspectSession(cid) => {
                self.selected_session = Some(cid);
                self.session_filter = None;
                self.pinned_only = false;
                self.active_tab = Tab::Sessions;
                self.prev_token_tab = Tab::Sessions;
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::FilterSessions(filter) => {
                let expanded_scope = !Self::scope_covers_filter(
                    self.scope,
                    self.custom_since,
                    self.custom_until,
                    &filter,
                    Local::now().date_naive(),
                );
                if expanded_scope {
                    let msg = match &filter {
                        SessionFilter::Week(_) => "已切换至「全部」以查看整周会话",
                        _ => "已切换至「全部」以查看所选日期的会话",
                    };
                    self.scope = DateScope::All;
                    self.dirty_filter = true;
                    self.selected_session = None;
                    self.set_banner_status(msg, false);
                }
                self.session_filter = Some(filter);
                self.pinned_only = false;
                self.active_tab = Tab::Sessions;
                self.prev_token_tab = Tab::Sessions;
                self.scroll_rows = 0;
                if !expanded_scope {
                    let replacement = self.with_display_sessions_for_tab(|matches| {
                        let keep = self
                            .selected_session
                            .as_deref()
                            .map(|id| matches.iter().any(|r| self.row_session(r).conversation_id == id))
                            .unwrap_or(false);
                        if keep {
                            None
                        } else {
                            Some(matches.first().map(|r| self.row_session(r).conversation_id.clone()))
                        }
                    });
                    if let Some(new_sel) = replacement {
                        self.selected_session = new_sel;
                    }
                }
                (false, false)
            }
            ClickAction::ClearSessionFilter => {
                if self.active_tab == Tab::Calls {
                    self.call_session_filter = None;
                } else if self.session_filter.is_some() {
                    self.session_filter = None;
                } else {
                    self.pinned_only = false;
                }
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::SelectCallSort(sort) => {
                self.call_sort = sort;
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::SelectCall(cid, step) => {
                self.selected_call = Some((cid, step));
                (false, false)
            }
            ClickAction::ShowSessionCalls(cid) => {
                self.call_session_filter = Some(cid);
                self.selected_call = None;
                self.active_tab = Tab::Calls;
                self.prev_token_tab = Tab::Calls;
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::ClearCallSessionFilter => {
                self.call_session_filter = None;
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::CopyCallDetail => {
                let text = self.with_call_rows(|rows, _, _, _| {
                    self.selected_call_index(rows).map(|i| {
                        let r = rows[i];
                        let s = self.call_session(&r);
                        self.format_call_detail_clipboard(s, &s.turns[r.turn_idx], r.gap_secs)
                    })
                });
                if let Some(text) = text {
                    if copy_text_to_clipboard(hwnd, &text) {
                        self.call_copied_until = Some(Instant::now() + std::time::Duration::from_millis(1600));
                    }
                }
                (false, false)
            }
        }
    }

    fn ensure_filtered(&mut self, snapshot: &ScanSnapshot) {
        let today = Local::now().date_naive();
        if !self.dirty_filter && self.cached_date == today {
            return;
        }
        self.cached_date = today;
        let (sessions, agg) =
            filter_snapshot_range(snapshot, self.scope, self.custom_since, self.custom_until);
        let has_any_pinned = snapshot.session_meta.values().any(|m| m.pinned);
        let pinned_out_of_scope: Vec<SessionInfo> = if has_any_pinned {
            let all_ids: HashSet<&str> = snapshot
                .sessions
                .iter()
                .map(|s| s.conversation_id.as_str())
                .collect();
            self.pinned_ids = snapshot
                .session_meta
                .iter()
                .filter(|(k, m)| m.pinned && all_ids.contains(k.as_str()))
                .map(|(k, _)| k.clone())
                .collect();
            if self.pinned_ids.is_empty() {
                Vec::new()
            } else {
                let in_scope_ids: HashSet<&str> = sessions
                    .iter()
                    .map(|s| s.conversation_id.as_str())
                    .collect();
                snapshot
                    .sessions
                    .iter()
                    .filter(|s| {
                        self.pinned_ids.contains(&s.conversation_id)
                            && !in_scope_ids.contains(s.conversation_id.as_str())
                    })
                    .cloned()
                    .collect()
            }
        } else {
            self.pinned_ids.clear();
            Vec::new()
        };

        // Invalidate Redactor lazily (zero allocation when privacy_mode == false!).
        *self.redactor.get_mut() = Redactor::default();
        self.redactor_built.set(false);

        if self.pinned_ids.is_empty() {
            self.pinned_only = false;
        }
        if !self.overview_top_chosen {
            let has_official = snapshot
                .statusline
                .as_ref()
                .map(|s| s.provenance(Utc::now()) != QuotaProvenance::LocalEstimate)
                .unwrap_or(false);
            self.overview_top = if has_official {
                OverviewTopMode::QuotaPace
            } else {
                OverviewTopMode::WeeklyCompare
            };
        }
        let (all_root, all_sub) = Self::session_split_counts_of(&snapshot.sessions);
        let (cached_root, cached_sub) = Self::session_split_counts_of(&sessions);
        self.cached_sessions = sessions;
        self.pinned_out_of_scope = pinned_out_of_scope;
        self.cached_agg = agg;
        self.all_total = snapshot.agg.total;
        self.all_sessions_count = snapshot.sessions.len();
        self.all_root_sessions = all_root;
        self.all_sub_sessions = all_sub;
        self.cached_root_sessions = cached_root;
        self.cached_sub_sessions = cached_sub;
        self.has_non_antigravity = snapshot.has_non_antigravity();
        self.dirty_filter = false;
        self.data_gen = self.data_gen.wrapping_add(1);
        self.cached_alert_level = None;
        self.clear_view_caches();

        // Only eagerly build `session_rows_cache` when already on `Tab::Sessions`; on all other
        // tabs validate `selected_session` via a zero-alloc scan and defer row-cache building.
        if self.active_tab == Tab::Sessions {
            let replacement = self.with_display_sessions_for_tab(|visible| {
                let keep = self
                    .selected_session
                    .as_ref()
                    .map(|id| visible.iter().any(|r| &self.row_session(r).conversation_id == id))
                    .unwrap_or(false);
                if keep {
                    None
                } else {
                    Some(visible.first().map(|r| self.row_session(r).conversation_id.clone()))
                }
            });
            if let Some(new_sel) = replacement {
                self.selected_session = new_sel;
            }
        } else if let Some(ref sel_id) = self.selected_session {
            let exists = self
                .cached_sessions
                .iter()
                .chain(self.pinned_out_of_scope.iter())
                .any(|s| &s.conversation_id == sel_id);
            if !exists {
                self.selected_session = None;
            }
        }
    }

    fn compute_alert_level(&mut self, snapshot: &ScanSnapshot) -> u8 {
        let now_utc = Utc::now();
        let minute_bucket = now_utc.timestamp() / 60;
        if let Some((gen, bucket, lvl)) = self.cached_alert_level {
            if gen == self.data_gen && bucket == minute_bucket {
                return lvl;
            }
        }
        let today_date = Local::now().date_naive();
        let today_key = today_date.format("%Y-%m-%d").to_string();
        let week_key = iso_week_key(today_date);
        let today_usd = snapshot
            .agg
            .by_day
            .get(&today_key)
            .map(|c| c.cost.total_usd())
            .unwrap_or(0.0);
        let week_usd = snapshot
            .agg
            .by_week
            .get(&week_key)
            .map(|c| c.cost.total_usd())
            .unwrap_or(0.0);
        let alerts = evaluate_alerts(
            &snapshot.budget,
            &today_key,
            today_usd,
            &week_key,
            week_usd,
            snapshot.statusline.as_ref(),
            now_utc,
        );
        let lvl = match alerts
            .iter()
            .map(|a| a.severity)
            .max()
            .unwrap_or(AlertSeverity::Normal)
        {
            AlertSeverity::Critical => 2u8,
            AlertSeverity::Warn => 1u8,
            AlertSeverity::Normal => 0u8,
        };
        self.cached_alert_level = Some((self.data_gen, minute_bucket, lvl));
        lvl
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_to_hdc(
        &mut self,
        _shared_factories: &D2DFactories,
        fonts: &Fonts,
        hwnd: HWND,
        target_hdc: Option<HDC>,
        dpi: f32,
        snapshot: &ScanSnapshot,
        telemetry: &SystemTelemetry,
        human: bool,
        light_theme: bool,
    ) -> Result<()> {
        self.ensure_filtered(snapshot);

        // Lazily create an isolated dashboard-only Direct2D + DirectWrite factory so D3D10Warp
        // software rasterizer pools are genuinely torn down when `on_hide()` drops it.
        if self.dash_factories.is_none() {
            self.dash_factories = Some(D2DFactories::new()?);
        }

        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut rc);
        }
        let px_w = (rc.right - rc.left).max(1);
        let px_h = (rc.bottom - rc.top).max(1);
        let scale = dpi / 96.0;
        let w = px_w as f32 / scale;
        let h = px_h as f32 / scale;

        let dash_f = self.dash_factories.take().expect("dash_factories initialized");
        let surface_res: Result<WindowSurface> = match self.surface.take() {
            Some(mut existing) => {
                if !existing.matches(px_w, px_h, dpi) {
                    match existing.resize(px_w, px_h, dpi) {
                        Ok(()) => Ok(existing),
                        Err(e) => Err(e),
                    }
                } else {
                    Ok(existing)
                }
            }
            None => WindowSurface::new(&dash_f, px_w, px_h, dpi),
        };
        let surface = match surface_res {
            Ok(s) => s,
            Err(e) => {
                self.dash_factories = Some(dash_f);
                return Err(e);
            }
        };

        self.hit_boxes.clear();
        self.session_row_rects.clear();
        self.plot_rects.clear();
        self.discrete_plots.clear();
        self.pricing_field_rects = None;
        self.budget_field_rects = None;
        self.search_field_rect = None;
        self.custom_date_rects = None;
        self.export_btn_rect = None;
        self.detail_more_btn_rect = None;
        self.hero_block4_layout = None;
        let theme = theme_for(light_theme);
        let alert_level = self.compute_alert_level(snapshot);

        let res = fonts.with_dash(&dash_f, |df| {
            surface.render_and_blit(target_hdc, |rt| {
                unsafe {
                    rt.Clear(Some(&theme.bg));
                }
                self.draw_header(&dash_f, rt, df, theme, w, human, light_theme, alert_level);
                let y_off = if self.is_custom_date_active() {
                    self.draw_custom_date_bar(rt, df, theme, w);
                    32.0
                } else {
                    0.0
                };
                self.draw_hero_strip(&dash_f, rt, df, theme, w, 58.0 + y_off, snapshot, telemetry, human);

                let body = rect_f(
                    20.0,
                    148.0 + y_off,
                    (w - 40.0).max(200.0),
                    (h - 166.0 - y_off).max(180.0),
                );
                match self.active_tab {
                    Tab::Overview => self.draw_overview_tab(&dash_f, rt, df, theme, body, snapshot, human),
                    Tab::Daily => self.draw_daily_tab(rt, df, theme, body, snapshot, human),
                    Tab::Weekly => self.draw_weekly_tab(rt, df, theme, body, snapshot, human),
                    Tab::Models => self.draw_models_tab(&dash_f, rt, df, theme, body, snapshot, human),
                    Tab::Sessions => self.draw_sessions_tab(&dash_f, rt, df, theme, body, snapshot, human),
                    Tab::Calls => self.draw_calls_tab(&dash_f, rt, df, theme, body, human),
                    Tab::System => self.draw_system_tab(&dash_f, rt, df, theme, body, snapshot, telemetry),
                }

                // Quiet bottom-center ephemeral confirmation toast (export / statusline copy)
                if let Some((ref msg, is_err, until)) = self.banner_status {
                    if Instant::now() < until {
                        let tw = (dash_f.measure_text_width(msg, &df.ui_small) + 28.0).clamp(180.0, w - 60.0);
                        let th = 28.0;
                        let tr = rect_f((w - tw) * 0.5, h - th - 14.0, tw, th);
                        fill_round_rect(rt, tr, 7.0, theme.tooltip_bg);
                        stroke_round_rect(rt, tr, 7.0, if is_err { theme.danger } else { theme.sage }, 1.0);
                        draw_text(rt, msg, &df.ui_small_center, tr, if is_err { theme.danger } else { theme.text });
                    }
                }
            })
        });

        self.dash_factories = Some(dash_f);
        match res {
            Ok(Ok(())) => {
                self.surface = Some(surface);
                let now = Instant::now();
                self.last_full_paint = Some(now);
                self.had_active_toast = self.has_active_toast(now);
                self.last_hero_telemetry_sig = Some(HeroTelemetrySig::from_telemetry(telemetry));
                Ok(())
            }
            Ok(Err(e)) => {
                self.last_hero_telemetry_sig = None;
                Err(e)
            }
            Err(e) => {
                self.last_hero_telemetry_sig = None;
                Err(e)
            }
        }
    }

    fn draw_custom_date_bar(
        &mut self,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        w: f32,
    ) {
        let y = 52.0;
        let h = 28.0;
        let bar = rect_f(20.0, y, (w - 40.0).max(200.0), h);
        fill_round_rect(rt, bar, 7.0, theme.surface);
        stroke_round_rect(rt, bar, 7.0, theme.border, 1.0);

        draw_text(rt, "自定义区间", &fonts.ui_small, rect_f(bar.left + 12.0, y, 66.0, h), theme.sub);
        let input_bg = if theme.is_dark {
            crate::d2d::rgb(20, 20, 22)
        } else {
            crate::d2d::rgb(246, 244, 241)
        };
        let box_w = 96.0;
        let box_h = 20.0;
        let box_y = y + 4.0;
        let x1 = bar.left + 84.0;
        let r1 = rect_f(x1, box_y, box_w, box_h);
        fill_round_rect(rt, r1, 5.0, input_bg);
        stroke_round_rect(
            rt,
            r1,
            5.0,
            if self.custom_date_focused == Some(0) { theme.accent } else { theme.border },
            1.0,
        );
        draw_text(rt, "至", &fonts.ui_small_center, rect_f(x1 + box_w + 4.0, y, 18.0, h), theme.muted);
        let x2 = x1 + box_w + 26.0;
        let r2 = rect_f(x2, box_y, box_w, box_h);
        fill_round_rect(rt, r2, 5.0, input_bg);
        stroke_round_rect(
            rt,
            r2,
            5.0,
            if self.custom_date_focused == Some(1) { theme.accent } else { theme.border },
            1.0,
        );
        self.custom_date_rects = Some([
            rect_f(r1.left + 6.0, r1.top + 2.0, box_w - 12.0, box_h - 4.0),
            rect_f(r2.left + 6.0, r2.top + 2.0, box_w - 12.0, box_h - 4.0),
        ]);

        let apply_r = rect_f(x2 + box_w + 10.0, box_y, 44.0, box_h);
        self.draw_ghost_btn(rt, fonts, theme, apply_r, "应用", true, ClickAction::ApplyCustomDate);

        draw_text(
            rt,
            "格式 YYYY-MM-DD（留空不限 · 按 Enter 生效）",
            &fonts.ui_micro,
            rect_f(apply_r.right + 12.0, y, (bar.right - apply_r.right - 22.0).max(80.0), h),
            theme.muted,
        );
    }

    // ---------- Minimalist Editorial Header (4-Tab Flat Underline Nav) ----------

    #[allow(clippy::too_many_arguments)]
    fn draw_header(
        &mut self,
        factories: &D2DFactories,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        w: f32,
        human: bool,
        light_theme: bool,
        alert_level: u8,
    ) {
        let h = 46.0;
        draw_line(rt, 0.0, h, w, h, theme.border, 1.0);

        // 14 DIP Concept C Braun Paper Dial brand micro-icon
        draw_brand_dial_icon(factories, rt, 20.0, h * 0.5, theme.is_dark, alert_level);
        draw_text(rt, "GAUGE", &fonts.ui_brand, rect_f(31.0, 0.0, 56.0, h), theme.text);
        draw_text(
            rt,
            if self.cost_mode { "/ $" } else { "/ 额" },
            &fonts.ui_small,
            rect_f(87.0, 1.0, 27.0, h),
            theme.muted,
        );

        // Flattened 6 direct token tabs (System monitor lives on Hero CPU/RAM rings)
        let tabs = [
            (Tab::Overview, "概览", 36.0),
            (Tab::Daily, "按天", 36.0),
            (Tab::Weekly, "按周", 36.0),
            (Tab::Models, "模型", 36.0),
            (Tab::Sessions, "会话", 36.0),
            (Tab::Calls, "调用", 36.0),
        ];
        let mut tx = 118.0;
        for (tab, lbl, tw) in tabs {
            let r = rect_f(tx, 0.0, tw, h);
            let act = self.active_tab == tab;
            let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&r, mx, my)).unwrap_or(false);
            draw_text(
                rt,
                lbl,
                &fonts.ui_tab,
                r,
                if act { theme.text } else if hov { theme.sub } else { theme.muted },
            );
            if act {
                fill_round_rect(rt, rect_f(tx + 5.0, h - 2.5, tw - 10.0, 2.5), 1.25, theme.accent);
            }
            self.hit_boxes.push(HitBox { rect: r, action: ClickAction::SelectTab(tab) });
            tx += tw + 3.0;
        }

        // Right minimalist utility actions
        let mut rx = w - 20.0;
        let btn_y = 10.0;
        let btn_h = 26.0;

        let r_ref = rect_f(rx - 34.0, btn_y, 34.0, btn_h);
        self.draw_ghost_btn(rt, fonts, theme, r_ref, "刷新", false, ClickAction::Refresh);
        rx -= 37.0;

        let r_exp = rect_f(rx - 36.0, btn_y, 36.0, btn_h);
        self.export_btn_rect = Some(r_exp);
        self.draw_ghost_btn(rt, fonts, theme, r_exp, "导出", false, ClickAction::ExportMenu);
        rx -= 39.0;

        let copied = self.copied_until.map(|t| Instant::now() < t).unwrap_or(false);
        let r_copy = rect_f(rx - 40.0, btn_y, 40.0, btn_h);
        self.draw_ghost_btn(
            rt,
            fonts,
            theme,
            r_copy,
            if copied { "已复" } else { "复制" },
            copied,
            ClickAction::CopySummary,
        );
        rx -= 43.0;

        let r_km = rect_f(rx - 34.0, btn_y, 34.0, btn_h);
        self.draw_ghost_btn(rt, fonts, theme, r_km, if human { "K/M" } else { "精确" }, false, ClickAction::ToggleHuman);
        rx -= 37.0;

        let r_th = rect_f(rx - 34.0, btn_y, 34.0, btn_h);
        self.draw_ghost_btn(
            rt,
            fonts,
            theme,
            r_th,
            if light_theme { "暗色" } else { "浅色" },
            false,
            ClickAction::ToggleLightTheme,
        );
        rx -= 37.0;

        // One-click Privacy / Redaction toggle (persisted in Registry, hotkey: P)
        let priv_w = if self.privacy_mode { 44.0 } else { 36.0 };
        let r_priv = rect_f(rx - priv_w, btn_y, priv_w, btn_h);
        self.draw_ghost_btn(
            rt,
            fonts,
            theme,
            r_priv,
            if self.privacy_mode { "已脱敏" } else { "脱敏" },
            self.privacy_mode,
            ClickAction::TogglePrivacyMode,
        );
        if self.privacy_mode {
            stroke_round_rect(rt, r_priv, 6.0, with_alpha(theme.accent, 0.72), 0.9);
        }
        rx -= priv_w + 4.0;

        // Tactile Tok | $ display mode switch (persisted in Registry, hotkey: D)
        let sw_w = 60.0;
        let r_mode = rect_f(rx - sw_w, btn_y + 1.0, sw_w, btn_h - 2.0);
        let mode_hov = self.mouse_dip.map(|(mx, my)| hit_rect(&r_mode, mx, my)).unwrap_or(false);
        fill_round_rect(rt, r_mode, 6.0, theme.surface_subtle);
        stroke_round_rect(
            rt,
            r_mode,
            6.0,
            if mode_hov { theme.accent } else { theme.border },
            0.8,
        );
        let r_tok = rect_f(r_mode.left + 2.0, r_mode.top + 2.0, 30.0, r_mode.bottom - r_mode.top - 4.0);
        let r_usd = rect_f(r_mode.left + 33.0, r_mode.top + 2.0, 25.0, r_mode.bottom - r_mode.top - 4.0);
        if !self.cost_mode {
            fill_round_rect(rt, r_tok, 4.5, theme.hover);
        } else {
            fill_round_rect(rt, r_usd, 4.5, theme.accent_soft);
        }
        draw_text(rt, "Tok", &fonts.ui_small_center, r_tok, if !self.cost_mode { theme.text } else { theme.muted });
        draw_text(rt, "$", &fonts.ui_small_center, r_usd, if self.cost_mode { theme.accent } else { theme.muted });
        self.hit_boxes.push(HitBox {
            rect: r_tok,
            action: ClickAction::SetCostMode(false),
        });
        self.hit_boxes.push(HitBox {
            rect: r_usd,
            action: ClickAction::SetCostMode(true),
        });
        rx -= sw_w + 6.0;

        draw_line(rt, rx, 14.0, rx, h - 14.0, theme.border, 1.0);
        rx -= 6.0;

        // Scope filter (borderless editorial text toggles; persisted in Registry).
        // Hidden on full-history time-series tabs (`按天` / `按周`) to prevent truncated periods.
        if self.tab_uses_scope() {
            let scopes = [
                (DateScope::Custom, "自定义", 42.0),
                (DateScope::All, "全部", 32.0),
                (DateScope::Days30, "30天", 34.0),
                (DateScope::Days7, "7天", 28.0),
                (DateScope::Today, "今日", 32.0),
            ];
            for (sc, lbl, sw) in scopes {
                let r = rect_f(rx - sw, btn_y, sw, btn_h);
                if r.left > tx + 4.0 {
                    let act = self.scope == sc;
                    let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&r, mx, my)).unwrap_or(false);
                    if act {
                        fill_round_rect(rt, r, 6.0, theme.hover);
                    } else if hov {
                        fill_round_rect(rt, r, 6.0, with_alpha(theme.text, 0.04));
                    }
                    draw_text(
                        rt,
                        lbl,
                        &fonts.ui_small_center,
                        r,
                        if act { theme.text } else if hov { theme.sub } else { theme.muted },
                    );
                    self.hit_boxes.push(HitBox { rect: r, action: ClickAction::SelectScope(sc) });
                }
                rx -= sw + 2.0;
            }
        } else {
            let badge_w = 62.0;
            let r = rect_f(rx - badge_w, btn_y + 1.0, badge_w, btn_h - 2.0);
            if r.left > tx + 4.0 {
                fill_round_rect(rt, r, 6.0, theme.surface_subtle);
                stroke_round_rect(rt, r, 6.0, theme.border, 0.8);
                draw_text(rt, "全量历史", &fonts.ui_small_center, r, theme.muted);
            }
        }
    }

    fn draw_ghost_btn(
        &mut self,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        r: D2D_RECT_F,
        label: &str,
        accent: bool,
        action: ClickAction,
    ) {
        let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&r, mx, my)).unwrap_or(false);
        if accent {
            fill_round_rect(rt, r, 6.0, theme.accent_soft);
        } else if hov {
            fill_round_rect(rt, r, 6.0, theme.hover);
        }
        draw_text(
            rt,
            label,
            &fonts.ui_small_center,
            r,
            if accent { theme.accent } else if hov { theme.text } else { theme.sub },
        );
        self.hit_boxes.push(HitBox { rect: r, action });
    }

    // ---------- Typographic Hero Strip (Clickable CPU/RAM Ring Block) ----------

    #[allow(clippy::too_many_arguments)]
    fn draw_hero_block4(
        factories: &D2DFactories,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        layout: HeroBlock4Layout,
        telemetry: &SystemTelemetry,
        sys_act: bool,
        sys_hov: bool,
    ) {
        let HeroBlock4Layout { band, x3, y, h, b4 } = layout;
        if sys_act {
            fill_round_rect(rt, b4, 7.0, with_alpha(theme.accent, 0.08));
            fill_round_rect(
                rt,
                rect_f(b4.left + 14.0, band.bottom - 2.5, (b4.right - b4.left - 28.0).max(20.0), 2.5),
                1.25,
                theme.accent,
            );
        } else if sys_hov {
            fill_round_rect(rt, b4, 7.0, theme.hover);
        }

        let tel = telemetry.latest;
        let g_cy = y + h * 0.5;
        let c_cx = x3 + 32.0;
        let mut fbuf = FmtBuf::new();
        draw_ring_gauge(factories, rt, c_cx, g_cy, 17.0, 3.0, tel.cpu_pct, theme.grid, theme.accent);
        let _ = write!(fbuf, "{:.0}%", tel.cpu_pct);
        draw_text(rt, fbuf.as_str(), &fonts.mono_small_center, rect_f(c_cx - 18.0, g_cy - 8.0, 36.0, 16.0), theme.text);

        let r_cx = x3 + 80.0;
        draw_ring_gauge(factories, rt, r_cx, g_cy, 17.0, 3.0, tel.mem_pct, theme.grid, theme.ram);
        fbuf.clear();
        let _ = write!(fbuf, "{:.0}%", tel.mem_pct);
        draw_text(rt, fbuf.as_str(), &fonts.mono_small_center, rect_f(r_cx - 18.0, g_cy - 8.0, 36.0, 16.0), theme.text);

        let tx_l = x3 + 106.0;
        let tx_w = (band.right - tx_l - 10.0).max(60.0);
        draw_text(
            rt,
            if sys_act { "系统监控  ‹ 返回" } else { "CPU / 内存  ›" },
            &fonts.ui_micro,
            rect_f(tx_l, y + 14.0, tx_w, 14.0),
            if sys_act { theme.accent } else if sys_hov { theme.text } else { theme.muted },
        );
        fbuf.clear();
        if tx_w >= 104.0 {
            let _ = write!(fbuf, "{:.1}/{:.1} GB", tel.mem_used_gb, tel.mem_total_gb);
        } else {
            let _ = write!(fbuf, "{:.1}/{:.0}G", tel.mem_used_gb, tel.mem_total_gb);
        }
        draw_text(
            rt,
            fbuf.as_str(),
            &fonts.mono_med,
            rect_f(tx_l, y + 31.0, tx_w, 18.0),
            theme.text,
        );
        fbuf.clear();
        let _ = write!(fbuf, "{} 核实时", tel.cpu_cores);
        draw_text(rt, fbuf.as_str(), &fonts.ui_micro, rect_f(tx_l, y + 50.0, tx_w, 14.0), theme.sub);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_hero_strip(
        &mut self,
        factories: &D2DFactories,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        w: f32,
        y: f32,
        snapshot: &ScanSnapshot,
        telemetry: &SystemTelemetry,
        human: bool,
    ) {
        let h = 76.0;
        let band = rect_f(20.0, y, (w - 40.0).max(200.0), h);
        fill_round_rect(rt, band, 10.0, theme.surface);
        stroke_round_rect(rt, band, 10.0, theme.border, 1.0);

        let bw = band.right - band.left;
        let c1 = bw * 0.30;
        let c2 = bw * 0.24;
        let c3 = bw * 0.20;
        let x0 = band.left;
        let x1 = x0 + c1;
        let x2 = x1 + c2;
        let x3 = x2 + c3;

        for dx in [x1, x2, x3] {
            draw_line(rt, dx, y + 14.0, dx, y + h - 14.0, theme.border, 1.0);
        }

        let use_full = !self.tab_uses_scope();
        let (t, days_cnt, root_sess, sub_sess, total_sess) = if use_full {
            (
                snapshot.agg.total,
                snapshot.agg.by_day.len(),
                self.all_root_sessions,
                self.all_sub_sessions,
                snapshot.sessions.len(),
            )
        } else {
            let (r, s) = self.session_split_counts();
            (self.cached_agg.total, self.cached_agg.by_day.len(), r, s, self.cached_sessions.len())
        };

        // Block 1: Hero Total (Token or USD) + subtle ratio ribbon
        let bar_l = x0 + 18.0;
        let bar_w = c1 - 36.0;
        let bar_y = y + h - 6.0;
        fill_round_rect(rt, rect_f(bar_l, bar_y, bar_w, 2.5), 1.25, theme.grid);

        if self.cost_mode {
            let hdr_lbl = if t.cost.has_unpriced() {
                "估算费用 (USD · 含未定价)"
            } else {
                "估算 API 费用 (USD)"
            };
            draw_text(rt, hdr_lbl, &fonts.ui_micro, rect_f(x0 + 18.0, y + 9.0, (c1 - 102.0).max(80.0), 14.0), theme.muted);
            let pr_r = rect_f(x0 + c1 - 76.0, y + 6.0, 60.0, 18.0);
            self.draw_ghost_btn(rt, fonts, theme, pr_r, "单价表 ›", false, ClickAction::OpenPricingTable);
            draw_text(
                rt,
                &fmt_cost(&t.cost, t.calls, human),
                &fonts.mono_hero,
                rect_f(x0 + 18.0, y + 23.0, c1 - 28.0, 30.0),
                theme.text,
            );
            let sub1 = format!(
                "缓存已省 {}  ·  {} tok",
                fmt_usd_compact(t.cost.cache_saved_usd),
                fmt_compact(t.total_tokens()),
            );
            draw_text(rt, &sub1, &fonts.ui_micro, rect_f(x0 + 18.0, y + 53.0, c1 - 28.0, 14.0), theme.sage);

            let tot_usd = t.cost.total_usd();
            if tot_usd > 0.0 {
                let mut bx = bar_l;
                for (v, col) in [
                    (t.cost.cache_read_usd, theme.cache),
                    (t.cost.input_plus_write_usd(), theme.accent),
                    (t.cost.output_usd, theme.output),
                ] {
                    if v <= 0.0 {
                        continue;
                    }
                    let sw = ((v / tot_usd) as f32 * bar_w).max(2.0);
                    fill_rect(rt, rect_f(bx, bar_y, sw.min(bar_l + bar_w - bx), 2.5), col);
                    bx += sw;
                }
            }
        } else {
            draw_text(rt, "总 Token 消耗", &fonts.ui_micro, rect_f(x0 + 18.0, y + 9.0, c1 - 110.0, 14.0), theme.muted);
            if t.calls > 0 {
                let cost_r = rect_f(x0 + c1 - 108.0, y + 6.0, 92.0, 18.0);
                let cost_hov = self.mouse_dip.map(|(mx, my)| hit_rect(&cost_r, mx, my)).unwrap_or(false);
                if cost_hov {
                    fill_round_rect(rt, cost_r, 5.0, theme.hover);
                }
                draw_text(
                    rt,
                    &format!("≈ {} ›", fmt_cost(&t.cost, t.calls, true)),
                    &fonts.mono_small_right,
                    rect_f(cost_r.left, cost_r.top + 1.0, cost_r.right - cost_r.left - 4.0, 16.0),
                    if cost_hov { theme.accent } else { theme.output },
                );
                self.hit_boxes.push(HitBox {
                    rect: cost_r,
                    action: ClickAction::OpenPricingTable,
                });
            }
            draw_text(rt, &fmt_num(t.total_tokens(), human), &fonts.mono_hero, rect_f(x0 + 18.0, y + 23.0, c1 - 28.0, 30.0), theme.text);
            let sub1 = format!("非缓存增量 {}  ·  缓存命中 {:.1}%", fmt_compact(t.non_cached_tokens()), t.cache_hit_rate());
            draw_text(rt, &sub1, &fonts.ui_micro, rect_f(x0 + 18.0, y + 53.0, c1 - 28.0, 14.0), theme.sub);

            if t.total_tokens() > 0 {
                let tot = t.total_tokens() as f32;
                let mut bx = bar_l;
                for (v, col) in [
                    (t.cache_read_tokens, theme.cache),
                    (t.input_tokens + t.cache_write_tokens, theme.accent),
                    (t.output_tokens, theme.output),
                ] {
                    if v == 0 {
                        continue;
                    }
                    let sw = ((v as f32 / tot) * bar_w).max(2.0);
                    fill_rect(rt, rect_f(bx, bar_y, sw.min(bar_l + bar_w - bx), 2.5), col);
                    bx += sw;
                }
            }
        }

        // Block 2: Fresh Input & Output (Tokens or USD)
        if self.cost_mode {
            draw_text(rt, "新输入 / 输出 费用", &fonts.ui_micro, rect_f(x1 + 18.0, y + 9.0, c2 - 28.0, 14.0), theme.muted);
            let io_str = format!(
                "{}  /  {}",
                fmt_usd_compact(t.cost.input_plus_write_usd()),
                fmt_usd_compact(t.cost.output_usd)
            );
            draw_text(rt, &io_str, &fonts.mono_kpi, rect_f(x1 + 18.0, y + 26.0, c2 - 28.0, 24.0), theme.text);
            let sub2 = format!(
                "缓存读 {}  ·  非缓存 {}",
                fmt_usd_compact(t.cost.cache_read_usd),
                fmt_usd_compact(t.cost.non_cached_usd())
            );
            draw_text(rt, &sub2, &fonts.ui_micro, rect_f(x1 + 18.0, y + 53.0, c2 - 28.0, 14.0), theme.sub);
        } else {
            draw_text(rt, "新输入 / 输出", &fonts.ui_micro, rect_f(x1 + 18.0, y + 9.0, c2 - 28.0, 14.0), theme.muted);
            let io_str = format!(
                "{}  /  {}",
                fmt_compact(t.input_tokens + t.cache_write_tokens),
                fmt_compact(t.output_tokens)
            );
            draw_text(rt, &io_str, &fonts.mono_kpi, rect_f(x1 + 18.0, y + 26.0, c2 - 28.0, 24.0), theme.text);
            let sub2 = format!(
                "缓存读 {}  ·  缓存已省 {}",
                fmt_compact(t.cache_read_tokens),
                fmt_usd_compact(t.cost.cache_saved_usd)
            );
            draw_text(rt, &sub2, &fonts.ui_micro, rect_f(x1 + 18.0, y + 53.0, c2 - 28.0, 14.0), theme.sub);
        }

        // Block 3: Sessions & Calls
        let sess_lbl = if sub_sess > 0 {
            format!("活跃规模 (含{}子会话)", sub_sess)
        } else {
            "活跃规模".to_string()
        };
        draw_text(rt, &sess_lbl, &fonts.ui_micro, rect_f(x2 + 16.0, y + 9.0, c3 - 22.0, 14.0), theme.muted);
        let disp_sess = if self.group_subagents && root_sess > 0 { root_sess } else { total_sess };
        let act_str = if c3 >= 195.0 {
            format!("{} 会话 · {} 次", disp_sess, t.calls)
        } else {
            format!("{}会话·{}次", disp_sess, t.calls)
        };
        draw_text(rt, &act_str, &fonts.mono_kpi, rect_f(x2 + 16.0, y + 26.0, c3 - 22.0, 24.0), theme.text);
        let sub3 = if self.cost_mode {
            let avg_call = if t.calls > 0 { t.cost.total_usd() / t.calls as f64 } else { 0.0 };
            let avg_sess = if disp_sess > 0 {
                t.cost.total_usd() / disp_sess as f64
            } else {
                0.0
            };
            format!("均单次 {}  ·  均会话 {}", fmt_usd_compact(avg_call), fmt_usd_compact(avg_sess))
        } else {
            let avg = if t.calls > 0 { t.total_tokens() / t.calls } else { 0 };
            format!("均单次 {}  ·  {} 天", fmt_compact(avg), days_cnt)
        };
        draw_text(rt, &sub3, &fonts.ui_micro, rect_f(x2 + 16.0, y + 53.0, c3 - 22.0, 14.0), theme.sub);

        // Block 4: Clickable Radial Ring Gauges for CPU & RAM (toggles System Monitor view!)
        let b4 = rect_f(x3 + 4.0, y + 4.0, (band.right - x3 - 8.0).max(80.0), h - 8.0);
        let layout = HeroBlock4Layout { band, x3, y, h, b4 };
        self.hero_block4_layout = Some(layout);
        let sys_act = self.active_tab == Tab::System;
        let sys_hov = self.mouse_dip.map(|(mx, my)| hit_rect(&b4, mx, my)).unwrap_or(false);
        self.hit_boxes.push(HitBox {
            rect: b4,
            action: ClickAction::ToggleSystemView,
        });
        Self::draw_hero_block4(factories, rt, fonts, theme, layout, telemetry, sys_act, sys_hov);
    }

    // ---------- Tab 1: Redesigned 3-Panel Editorial Overview ----------

    #[allow(clippy::too_many_arguments)]
    fn draw_overview_tab(
        &mut self,
        factories: &D2DFactories,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        body: D2D_RECT_F,
        snapshot: &ScanSnapshot,
        human: bool,
    ) {
        enum OverviewBarMeta {
            Hour(usize, TokenCounts),
            Day(NaiveDate, String, TokenCounts),
        }

        let gap = 14.0;
        let bw = body.right - body.left;
        let bh = body.bottom - body.top;
        let left_w = ((bw - gap) * 0.59).round();
        let right_w = bw - gap - left_w;
        let top_h = ((bh - gap) * 0.50).round();
        let bot_h = bh - gap - top_h;
        let today = Local::now().date_naive();

        // 1) Top-Left: Smart Trend Chart (Hourly on Today scope, smart 7..14 day window otherwise)
        let tl = rect_f(body.left, body.top, left_w, top_h);
        fill_round_rect(rt, tl, 10.0, theme.surface);
        stroke_round_rect(rt, tl, 10.0, theme.border, 1.0);

        let (chart_title, bars, bar_meta, bar_days, empty_msg) = if self.scope == DateScope::Today {
            let active_hours: Vec<usize> = self
                .cached_agg
                .by_hour
                .iter()
                .enumerate()
                .filter_map(|(h, c)| if c.calls > 0 { Some(h) } else { None })
                .collect();
            let mut meta = Vec::new();
            let items: Vec<BarItem> = if let (Some(&first_h), Some(&last_h)) =
                (active_hours.first(), active_hours.last())
            {
                let now_h = (Local::now().hour() as usize).min(23);
                let end_h = last_h.max(now_h).min(23);
                let mut start_h = first_h.min(end_h.saturating_sub(7));
                if end_h.saturating_sub(start_h) < 7 {
                    start_h = end_h.saturating_sub(7);
                }
                const HOUR_LABELS: [&str; 24] = [
                    "00h", "01h", "02h", "03h", "04h", "05h", "06h", "07h",
                    "08h", "09h", "10h", "11h", "12h", "13h", "14h", "15h",
                    "16h", "17h", "18h", "19h", "20h", "21h", "22h", "23h",
                ];
                (start_h..=end_h)
                    .map(|h| {
                        let c = self.cached_agg.by_hour[h];
                        meta.push(OverviewBarMeta::Hour(h, c));
                        self.make_bar(HOUR_LABELS[h], &c, false)
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let title = if self.cost_mode { "今日分时费用 ($)" } else { "今日分时趋势" };
            (title, items, meta, Vec::new(), "今日暂无用量 · 右上角可切换 7天 / 全部")
        } else {
            let (end_day, n_days): (NaiveDate, u64) = match self.scope {
                DateScope::Days7 => (today, 7),
                DateScope::Custom => {
                    let end = self
                        .custom_until
                        .or_else(|| {
                            self.cached_agg
                                .by_day
                                .keys()
                                .next_back()
                                .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
                        })
                        .unwrap_or(today);
                    let start = self
                        .custom_since
                        .or_else(|| {
                            self.cached_agg
                                .by_day
                                .keys()
                                .next()
                                .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
                        })
                        .unwrap_or(end);
                    let span = ((end - start).num_days() + 1).clamp(1, 14) as u64;
                    (end, span)
                }
                _ => {
                    let earliest = self
                        .cached_agg
                        .by_day
                        .keys()
                        .next()
                        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
                    let span = match earliest {
                        Some(d) => ((today - d).num_days() + 1).clamp(7, 14) as u64,
                        None => 7,
                    };
                    (today, span)
                }
            };
            let mut days_keys: Vec<Option<String>> = Vec::with_capacity(n_days as usize);
            let mut meta: Vec<OverviewBarMeta> = Vec::with_capacity(n_days as usize);
            let items: Vec<BarItem> = (0..n_days)
                .rev()
                .filter_map(|off| end_day.checked_sub_days(chrono::Days::new(off)))
                .map(|d| {
                    let k = d.format("%Y-%m-%d").to_string();
                    let c = self.cached_agg.by_day.get(&k).copied().unwrap_or_default();
                    days_keys.push(if c.calls > 0 { Some(k.clone()) } else { None });
                    meta.push(OverviewBarMeta::Day(d, k, c));
                    self.make_bar(d.format("%m/%d").to_string(), &c, false)
                })
                .collect();
            let title = if self.cost_mode {
                if self.scope == DateScope::Days7 { "近 7 天费用趋势 ($)" } else { "每日费用趋势 ($)" }
            } else if self.scope == DateScope::Days7 {
                "近 7 天用量趋势"
            } else {
                "每日用量趋势"
            };
            (title, items, meta, days_keys, "暂无用量数据")
        };

        draw_text(rt, chart_title, &fonts.ui_section, rect_f(tl.left + 16.0, tl.top + 11.0, 146.0, 20.0), theme.text);
        self.draw_chart_legend_and_toggle(rt, fonts, theme, tl);
        let tl_plot = rect_f(tl.left + 10.0, tl.top + 38.0, left_w - 20.0, top_h - 46.0);
        let cost_mode = self.cost_mode;
        let (tl_slots, tl_inner) = draw_editorial_bars(
            rt,
            fonts,
            theme,
            tl_plot,
            &bars,
            self.fresh_only,
            self.bar_unit(),
            self.mouse_dip,
            empty_msg,
            |idx| match &bar_meta[idx] {
                OverviewBarMeta::Hour(h, c) => {
                    if cost_mode {
                        vec![
                            format!("今日 {:02}:00 - {:02}:59 · {} 次", h, h, c.calls),
                            format!("估算费用: {} (省 {})", fmt_cost(&c.cost, c.calls, false), fmt_usd_compact(c.cost.cache_saved_usd)),
                            format!("新输入  : {} ({})", fmt_usd_precise(c.cost.input_plus_write_usd()), fmt_compact(c.input_tokens)),
                            format!("缓存读  : {} ({:.1}%)", fmt_usd_precise(c.cost.cache_read_usd), c.cache_hit_rate()),
                            format!("输出    : {} ({})", fmt_usd_precise(c.cost.output_usd), fmt_compact(c.output_tokens)),
                            format!("总Token : {}", fmt_commas(c.total_tokens())),
                        ]
                    } else {
                        vec![
                            format!("今日 {:02}:00 - {:02}:59 · {} 次", h, h, c.calls),
                            format!("总计   : {}", fmt_commas(c.total_tokens())),
                            format!("估算费 : {}", fmt_cost(&c.cost, c.calls, false)),
                            format!("新输入 : {}", fmt_commas(c.input_tokens)),
                            format!("缓存读 : {} ({:.1}%)", fmt_commas(c.cache_read_tokens), c.cache_hit_rate()),
                            format!("输出   : {}", fmt_commas(c.output_tokens)),
                        ]
                    }
                }
                OverviewBarMeta::Day(d, k, c) => {
                    if cost_mode {
                        vec![
                            format!("{} ({}) · {} 次", k, weekday_cn(*d), c.calls),
                            format!("估算费用: {} (省 {})", fmt_cost(&c.cost, c.calls, false), fmt_usd_compact(c.cost.cache_saved_usd)),
                            format!("新输入  : {} ({})", fmt_usd_precise(c.cost.input_plus_write_usd()), fmt_compact(c.input_tokens)),
                            format!("缓存读  : {} ({:.1}%)", fmt_usd_precise(c.cost.cache_read_usd), c.cache_hit_rate()),
                            format!("输出    : {} ({})", fmt_usd_precise(c.cost.output_usd), fmt_compact(c.output_tokens)),
                            format!("总Token : {}", fmt_commas(c.total_tokens())),
                        ]
                    } else {
                        vec![
                            format!("{} ({}) · {} 次", k, weekday_cn(*d), c.calls),
                            format!("总计   : {}", fmt_commas(c.total_tokens())),
                            format!("估算费 : {}", fmt_cost(&c.cost, c.calls, false)),
                            format!("新输入 : {}", fmt_commas(c.input_tokens)),
                            format!("缓存读 : {} ({:.1}%)", fmt_commas(c.cache_read_tokens), c.cache_hit_rate()),
                            format!("输出   : {}", fmt_commas(c.output_tokens)),
                        ]
                    }
                }
            },
        );
        if let Some(inner) = tl_inner {
            self.discrete_plots.push(DiscretePlot::VerticalBars {
                plot: inner,
                n: bars.len(),
            });
        }
        for (rect, maybe_day) in tl_slots.into_iter().zip(bar_days.into_iter()) {
            if let Some(day) = maybe_day {
                self.hit_boxes.push(HitBox {
                    rect,
                    action: ClickAction::FilterSessions(SessionFilter::Date(day)),
                });
            }
        }

        // 2) Top-Right: Official Quota / Daily-Weekly Budget (or ISO Week Compare) + Breakdown
        let tr = rect_f(tl.right + gap, body.top, right_w, top_h);
        fill_round_rect(rt, tr, 10.0, theme.surface);
        stroke_round_rect(rt, tr, 10.0, theme.border, 1.0);

        let split_y = tr.top + (top_h * 0.44).clamp(100.0, 114.0);
        let mut ox = tr.left + 14.0;
        for (mode, lbl, ow) in [
            (OverviewTopMode::QuotaPace, "官方额度", 60.0),
            (OverviewTopMode::WeeklyCompare, "本周同比", 60.0),
        ] {
            let r = rect_f(ox, tr.top + 8.0, ow, 20.0);
            let act = self.overview_top == mode;
            let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&r, mx, my)).unwrap_or(false);
            if act {
                fill_round_rect(rt, r, 5.0, theme.hover);
            } else if hov {
                fill_round_rect(rt, r, 5.0, with_alpha(theme.text, 0.04));
            }
            draw_text(
                rt,
                lbl,
                &fonts.ui_small_center,
                r,
                if act { theme.text } else if hov { theme.sub } else { theme.muted },
            );
            self.hit_boxes.push(HitBox {
                rect: r,
                action: ClickAction::SelectOverviewTop(mode),
            });
            ox += ow + 4.0;
        }

        match self.overview_top {
            OverviewTopMode::QuotaPace => {
                let now_utc = Utc::now();
                let prov = snapshot
                    .statusline
                    .as_ref()
                    .map(|s| s.provenance(now_utc))
                    .unwrap_or(QuotaProvenance::LocalEstimate);
                let prov_col = match prov {
                    QuotaProvenance::OfficialLive => theme.sage,
                    QuotaProvenance::OfficialStale => theme.warn,
                    QuotaProvenance::LocalEstimate => theme.muted,
                };
                let b_btn = rect_f(tr.right - 56.0, tr.top + 8.0, 44.0, 20.0);
                self.draw_ghost_btn(rt, fonts, theme, b_btn, "预算 ›", false, ClickAction::OpenBudgetView);
                let prov_lbl = if prov == QuotaProvenance::LocalEstimate {
                    "未接入"
                } else {
                    prov.label_cn()
                };
                draw_text(
                    rt,
                    prov_lbl,
                    &fonts.ui_small_right,
                    rect_f(ox + 2.0, tr.top + 10.0, (b_btn.left - ox - 8.0).max(40.0), 16.0),
                    prov_col,
                );

                let inner_l = tr.left + 16.0;
                let inner_w = right_w - 32.0;
                let mut cy = tr.top + 32.0;

                let has_official = snapshot
                    .statusline
                    .as_ref()
                    .map(|s| s.provenance(now_utc) != QuotaProvenance::LocalEstimate)
                    .unwrap_or(false);

                let today_key = today.format("%Y-%m-%d").to_string();
                let cur_wk = iso_week_key(today);
                let today_usd = snapshot
                    .agg
                    .by_day
                    .get(&today_key)
                    .map(|c| c.cost.total_usd())
                    .unwrap_or(0.0);
                let week_usd = snapshot
                    .agg
                    .by_week
                    .get(&cur_wk)
                    .map(|c| c.cost.total_usd())
                    .unwrap_or(0.0);

                if has_official {
                    if let Some(sl) = snapshot.statusline.as_ref() {
                        let show_buckets: Vec<_> = sl
                            .buckets
                            .iter()
                            .filter(|b| !b.disabled && b.remaining_pct().is_some())
                            .take(2)
                            .collect();
                        for b in show_buckets {
                            let rem_pct = b.remaining_pct().unwrap_or(0.0);
                            let used_pct = b.used_pct().unwrap_or(0.0);
                            let time_pct = b.time_elapsed_pct(sl.captured_at_utc, now_utc).map(|v| v as f32);
                            let rem_str = b
                                .live_remaining_secs(sl.captured_at_utc, now_utc)
                                .map(fmt_countdown)
                                .unwrap_or_else(|| "-".into());
                            let sev = snapshot.budget.classify_quota_remaining(rem_pct);
                            let bar_col = match sev {
                                AlertSeverity::Critical => theme.danger,
                                AlertSeverity::Warn => theme.warn,
                                AlertSeverity::Normal => theme.sage,
                            };
                            draw_text(rt, &b.label, &fonts.mono_small, rect_f(inner_l, cy, inner_w * 0.45, 14.0), theme.text);
                            draw_text(
                                rt,
                                &format!("剩 {:.0}% · {}后重置", rem_pct, rem_str),
                                &fonts.mono_small_right,
                                rect_f(inner_l + inner_w * 0.42, cy, inner_w * 0.58, 14.0),
                                bar_col,
                            );
                            draw_pace_bar(rt, theme, rect_f(inner_l, cy + 16.0, inner_w, 5.0), used_pct as f32, time_pct, bar_col);
                            cy += 27.0;
                        }
                        let foot = if snapshot.budget.daily_usd > 0.0 || snapshot.budget.weekly_usd > 0.0 {
                            let mut parts = Vec::new();
                            if snapshot.budget.daily_usd > 0.0 {
                                parts.push(format!(
                                    "今日 {}/${:.0}",
                                    fmt_usd_compact(today_usd),
                                    snapshot.budget.daily_usd
                                ));
                            }
                            if snapshot.budget.weekly_usd > 0.0 {
                                parts.push(format!(
                                    "本周 {}/${:.0}",
                                    fmt_usd_compact(week_usd),
                                    snapshot.budget.weekly_usd
                                ));
                            }
                            format!("全源预算 · {}", parts.join(" · "))
                        } else {
                            let age = fmt_duration_short(sl.age_secs(now_utc).max(0) as u64);
                            format!("Antigravity CLI /statusline 已同步 · {}前更新", age)
                        };
                        draw_text(rt, &foot, &fonts.ui_micro, rect_f(inner_l, (split_y - 17.0).max(cy), inner_w, 14.0), theme.sub);
                    }
                } else if snapshot.budget.daily_usd > 0.0 || snapshot.budget.weekly_usd > 0.0 {
                    let (lbl, spent, cap) = if snapshot.budget.daily_usd > 0.0 {
                        ("今日预算 (全源)", today_usd, snapshot.budget.daily_usd)
                    } else {
                        ("本周预算 (全源)", week_usd, snapshot.budget.weekly_usd)
                    };
                    let used_p = (spent / cap) * 100.0;
                    let sev = snapshot.budget.classify_usage_pct(used_p);
                    let bar_col = match sev {
                        AlertSeverity::Critical => theme.danger,
                        AlertSeverity::Warn => theme.warn,
                        AlertSeverity::Normal => theme.accent,
                    };
                    draw_text(rt, lbl, &fonts.mono_small, rect_f(inner_l, cy, inner_w * 0.44, 15.0), theme.text);
                    draw_text(
                        rt,
                        &format!("{} / ${:.0} ({:.0}%)", fmt_usd_compact(spent), cap, used_p),
                        &fonts.mono_small_right,
                        rect_f(inner_l + inner_w * 0.42, cy, inner_w * 0.58, 15.0),
                        bar_col,
                    );
                    draw_pace_bar(rt, theme, rect_f(inner_l, cy + 19.0, inner_w, 6.0), used_p as f32, None, bar_col);
                    let sl_btn = rect_f(inner_l, (split_y - 21.0).max(cy + 34.0), inner_w, 18.0);
                    let sl_hov = self.mouse_dip.map(|(mx, my)| hit_rect(&sl_btn, mx, my)).unwrap_or(false);
                    draw_text(
                        rt,
                        "复制 /statusline 配置，接入 Antigravity CLI 官方配额 ›",
                        &fonts.ui_micro,
                        sl_btn,
                        if sl_hov { theme.accent } else { theme.muted },
                    );
                    self.hit_boxes.push(HitBox {
                        rect: sl_btn,
                        action: ClickAction::CopyStatuslineConfig,
                    });
                } else {
                    draw_text(
                        rt,
                        "Antigravity CLI /statusline 配额桥接未接入",
                        &fonts.ui_small,
                        rect_f(inner_l, cy + 2.0, inner_w, 16.0),
                        theme.text,
                    );
                    draw_text(
                        rt,
                        "仅桥接 agy 官方余量；每日/每周美元预算汇总全部已发现 Agent",
                        &fonts.ui_micro,
                        rect_f(inner_l, cy + 21.0, inner_w, 14.0),
                        theme.muted,
                    );
                    let btn_w = 136.0_f32.min(inner_w);
                    let sl_btn = rect_f(inner_l, (split_y - 25.0).max(cy + 40.0), btn_w, 21.0);
                    self.draw_ghost_btn(
                        rt,
                        fonts,
                        theme,
                        sl_btn,
                        "复制 /statusline 配置",
                        true,
                        ClickAction::CopyStatuslineConfig,
                    );
                    stroke_round_rect(rt, sl_btn, 6.0, with_alpha(theme.accent, 0.55), 0.8);
                }
            }
            OverviewTopMode::WeeklyCompare => {
                let wk_hit = rect_f(tr.left + 8.0, tr.top + 30.0, right_w - 16.0, (split_y - tr.top - 34.0).max(52.0));
                let wk_hov = self.mouse_dip.map(|(mx, my)| hit_rect(&wk_hit, mx, my)).unwrap_or(false);
                if wk_hov {
                    fill_round_rect(rt, wk_hit, 7.0, with_alpha(theme.text, 0.035));
                }
                self.hit_boxes.push(HitBox {
                    rect: wk_hit,
                    action: ClickAction::SelectTab(Tab::Weekly),
                });

                let cur_wk = iso_week_key(today);
                let prev_wk = today
                    .checked_sub_days(chrono::Days::new(7))
                    .map(iso_week_key)
                    .unwrap_or_default();
                let wk_c = snapshot.agg.by_week.get(&cur_wk).copied().unwrap_or_default();
                let prev_c = snapshot.agg.by_week.get(&prev_wk).copied().unwrap_or_default();
                let wk_days = snapshot.agg.week_active_days.get(&cur_wk).copied().unwrap_or(0);

                draw_text(
                    rt,
                    &format!("{} ({}) ›", cur_wk, iso_week_range_label(&cur_wk)),
                    &fonts.mono_small_right,
                    rect_f(ox + 4.0, tr.top + 10.0, (tr.right - 16.0 - ox).max(80.0), 16.0),
                    if wk_hov { theme.accent } else { theme.muted },
                );

                if self.cost_mode {
                    draw_text(
                        rt,
                        &fmt_cost(&wk_c.cost, wk_c.calls, human),
                        &fonts.mono_kpi,
                        rect_f(tr.left + 16.0, tr.top + 33.0, (right_w - 150.0).max(90.0), 24.0),
                        theme.text,
                    );
                    let (wow_str, wow_col) = if prev_c.cost.total_usd() > 0.0 {
                        let pct = ((wk_c.cost.total_usd() - prev_c.cost.total_usd()) / prev_c.cost.total_usd()) * 100.0;
                        (format!("较上周 {:+.1}%", pct), if pct >= 0.0 { theme.accent } else { theme.sage })
                    } else {
                        ("上周无记录".to_string(), theme.muted)
                    };
                    draw_text(rt, &wow_str, &fonts.mono_small_right, rect_f(tr.right - 134.0, tr.top + 37.0, 118.0, 18.0), wow_col);
                    let wk_avg_usd = if wk_days > 0 { wk_c.cost.total_usd() / wk_days as f64 } else { 0.0 };
                    draw_text(
                        rt,
                        &format!(
                            "日均 {}  ·  活跃 {} 天  ·  非缓存 {}",
                            fmt_usd_compact(wk_avg_usd),
                            wk_days,
                            fmt_usd_compact(wk_c.cost.non_cached_usd())
                        ),
                        &fonts.ui_micro,
                        rect_f(tr.left + 16.0, tr.top + 61.0, right_w - 32.0, 15.0),
                        theme.sub,
                    );
                    let prev_lbl = if prev_c.calls > 0 {
                        fmt_cost(&prev_c.cost, prev_c.calls, true)
                    } else {
                        "-".to_string()
                    };
                    draw_text(
                        rt,
                        &format!(
                            "本周缓存已省 {}  ·  上周 {}",
                            fmt_usd_compact(wk_c.cost.cache_saved_usd),
                            prev_lbl
                        ),
                        &fonts.ui_micro,
                        rect_f(tr.left + 16.0, tr.top + 79.0, right_w - 32.0, 15.0),
                        theme.muted,
                    );
                } else {
                    let wk_avg = if wk_days > 0 { wk_c.total_tokens() / wk_days as u64 } else { 0 };
                    draw_text(rt, &fmt_num(wk_c.total_tokens(), human), &fonts.mono_kpi, rect_f(tr.left + 16.0, tr.top + 33.0, (right_w - 150.0).max(90.0), 24.0), theme.text);

                    let (wow_str, wow_col) = if prev_c.total_tokens() > 0 {
                        let pct = ((wk_c.total_tokens() as f64 - prev_c.total_tokens() as f64)
                            / prev_c.total_tokens() as f64)
                            * 100.0;
                        (format!("较上周 {:+.1}%", pct), if pct >= 0.0 { theme.accent } else { theme.sage })
                    } else {
                        ("上周无记录".to_string(), theme.muted)
                    };
                    draw_text(rt, &wow_str, &fonts.mono_small_right, rect_f(tr.right - 134.0, tr.top + 37.0, 118.0, 18.0), wow_col);

                    draw_text(
                        rt,
                        &format!("日均 {}  ·  活跃 {} 天  ·  非缓存 {}", fmt_compact(wk_avg), wk_days, fmt_compact(wk_c.non_cached_tokens())),
                        &fonts.ui_micro,
                        rect_f(tr.left + 16.0, tr.top + 61.0, right_w - 32.0, 15.0),
                        theme.sub,
                    );
                    let prev_lbl = if prev_c.total_tokens() > 0 {
                        fmt_compact(prev_c.total_tokens())
                    } else {
                        "-".to_string()
                    };
                    draw_text(
                        rt,
                        &format!(
                            "命中 {:.1}%  ·  本周约 {}  ·  上周 {}",
                            wk_c.cache_hit_rate(),
                            fmt_cost_compact(&wk_c.cost, wk_c.calls),
                            prev_lbl
                        ),
                        &fonts.ui_micro,
                        rect_f(tr.left + 16.0, tr.top + 79.0, right_w - 32.0, 15.0),
                        theme.muted,
                    );
                }
            }
        }

        draw_line(rt, tr.left + 16.0, split_y, tr.right - 16.0, split_y, theme.border, 1.0);

        // Quiet "模型 | 项目 | 工具 | 来源" breakdown toggle inside lower right section
        let mut bx = tr.left + 14.0;
        for (mode, lbl, bw_btn) in [
            (BreakdownMode::Model, "模型", 36.0),
            (BreakdownMode::Workspace, "项目", 36.0),
            (BreakdownMode::Tools, "工具", 36.0),
            (BreakdownMode::Source, "来源", 36.0),
        ] {
            let r = rect_f(bx, split_y + 7.0, bw_btn, 20.0);
            let act = self.breakdown_mode == mode;
            let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&r, mx, my)).unwrap_or(false);
            if act {
                fill_round_rect(rt, r, 5.0, theme.hover);
            } else if hov {
                fill_round_rect(rt, r, 5.0, with_alpha(theme.text, 0.04));
            }
            draw_text(
                rt,
                lbl,
                &fonts.ui_small_center,
                r,
                if act { theme.text } else if hov { theme.sub } else { theme.muted },
            );
            self.hit_boxes.push(HitBox {
                rect: r,
                action: ClickAction::SelectBreakdown(mode),
            });
            bx += bw_btn + 2.0;
        }
        if self.breakdown_mode == BreakdownMode::Model {
            let mut mx = tr.right - 98.0;
            for (metric, lbl, mw) in [
                (ModelCardMetric::Volume, if self.cost_mode { "费用" } else { "用量" }, 36.0),
                (ModelCardMetric::CacheHit, "缓存率", 44.0),
            ] {
                let r = rect_f(mx, split_y + 7.0, mw, 20.0);
                let act = self.model_card_metric == metric;
                let hov = self.mouse_dip.map(|(px, py)| hit_rect(&r, px, py)).unwrap_or(false);
                if act {
                    fill_round_rect(rt, r, 5.0, theme.hover);
                } else if hov {
                    fill_round_rect(rt, r, 5.0, with_alpha(theme.text, 0.04));
                }
                draw_text(
                    rt,
                    lbl,
                    &fonts.ui_small_center,
                    r,
                    if act { theme.text } else if hov { theme.sub } else { theme.muted },
                );
                self.hit_boxes.push(HitBox {
                    rect: r,
                    action: ClickAction::SelectModelCardMetric(metric),
                });
                mx += mw + 3.0;
            }
        } else if matches!(self.breakdown_mode, BreakdownMode::Workspace | BreakdownMode::Source) {
            draw_text(
                rt,
                "占比 · 会话数",
                &fonts.ui_small_right,
                rect_f(tr.right - 130.0, split_y + 9.0, 114.0, 16.0),
                theme.muted,
            );
        } else {
            draw_text(
                rt,
                "调用频次",
                &fonts.ui_small_right,
                rect_f(tr.right - 100.0, split_y + 9.0, 84.0, 16.0),
                theme.muted,
            );
        }
        let bd_bounds = rect_f(tr.left + 16.0, split_y + 31.0, right_w - 32.0, (tr.bottom - split_y - 37.0).max(24.0));
        match self.breakdown_mode {
            BreakdownMode::Model => self.draw_compact_models(factories, rt, fonts, theme, bd_bounds, human),
            BreakdownMode::Workspace => self.draw_compact_workspaces(rt, fonts, theme, bd_bounds, human),
            BreakdownMode::Tools => self.draw_compact_tools(rt, fonts, theme, bd_bounds),
            BreakdownMode::Source => self.draw_compact_sources(rt, fonts, theme, bd_bounds, human),
        }

        // 3) Bottom Full-Width: Recent Active Sessions (grouped by parent session when enabled)
        let bot = rect_f(body.left, tl.bottom + gap, bw, bot_h);
        fill_round_rect(rt, bot, 10.0, theme.surface);
        stroke_round_rect(rt, bot, 10.0, theme.border, 1.0);

        let (_, sub_total) = self.session_split_counts();
        let show_multi = self.has_non_antigravity;
        let (new_row_rects, new_hits) = self.with_overview_recent_sessions(|recent_rows| {
            let mut row_rects_out = Vec::with_capacity(6);
            let mut hits_out = Vec::with_capacity(6);

            draw_text(rt, "最近活跃会话", &fonts.ui_section, rect_f(bot.left + 16.0, bot.top + 9.0, 96.0, 20.0), theme.text);
            let cnt_txt = if self.group_subagents && sub_total > 0 {
                format!("{} 个主会话 · 含 {} 子会话", recent_rows.len(), sub_total)
            } else {
                format!("共 {} 个", recent_rows.len())
            };
            draw_text(
                rt,
                &cnt_txt,
                &fonts.ui_micro,
                rect_f(bot.left + 108.0, bot.top + 11.0, 200.0, 16.0),
                theme.muted,
            );
            draw_text(
                rt,
                "左键剖析 · 右键管理 →",
                &fonts.ui_small_right,
                rect_f(bot.right - 170.0, bot.top + 10.0, 154.0, 18.0),
                theme.muted,
            );

            let rr_l = bot.left + 10.0;
            let rr_w = bw - 20.0;
            let rx = rr_l + rr_w - 10.0;
            let list_y = bot.top + 54.0;
            let avail_h = (bot.bottom - 8.0 - list_y).max(28.0);
            let max_vis = ((avail_h / 28.0).floor() as usize).clamp(1, 6);
            let show_n = recent_rows.len().min(max_vis);

            // Measure actual model names in visible rows so long names (e.g. 30-char models) fit without ellipsis.
            let max_model_px = recent_rows
                .iter()
                .take(show_n)
                .map(|r| factories.measure_text_width(&r.primary_model, &fonts.mono_small_right))
                .fold(132.0_f32, f32::max);
            let model_cap = (rr_w * 0.28).clamp(156.0, 228.0);
            let model_w = (max_model_px + 8.0).clamp(132.0, model_cap);
            let (sm_x, sm_w, metrics_w) = session_metric_columns(rx);
            let sm_rect = |i: usize, y: f32, h: f32| rect_f(sm_x[i], y, sm_w[i], h);
            let model_x = rx - metrics_w - model_w;
            let time_x = rr_l + 34.0;
            let title_x = rr_l + 94.0;
            let title_w = (model_x - 10.0 - title_x).max(80.0);

            // Subtle column header row
            let hdr_y = bot.top + 31.0;
            let hdr_h = 20.0;
            draw_text(rt, "时间", &fonts.ui_micro, rect_f(time_x, hdr_y, 56.0, hdr_h), theme.muted);
            draw_text(rt, "会话", &fonts.ui_micro, rect_f(title_x, hdr_y, title_w, hdr_h), theme.muted);
            draw_text(rt, "模型", &fonts.ui_small_right, rect_f(model_x, hdr_y, model_w, hdr_h), theme.muted);
            for (i, lbl) in SESSION_METRIC_LABELS.iter().enumerate() {
                draw_text(rt, lbl, &fonts.ui_small_right, sm_rect(i, hdr_y, hdr_h), theme.muted);
            }
            draw_line(rt, bot.left + 14.0, bot.top + 51.0, bot.right - 14.0, bot.top + 51.0, theme.border, 1.0);

            if recent_rows.is_empty() {
                draw_text(
                    rt,
                    "当前范围暂无会话 · 点击右上角可切换 7天 / 30天 / 全部",
                    &fonts.ui_reg_center,
                    rect_f(bot.left, list_y, bw, avail_h),
                    theme.muted,
                );
            } else {
                let now_local = Local::now();
                let row_h = (avail_h / max_vis as f32).clamp(26.0, 32.0);
                for (i, row) in recent_rows.iter().take(show_n).enumerate() {
                    let s = self.row_session(row);
                    let ry = list_y + i as f32 * row_h;
                    let cell_h = row_h - 2.0;
                    let rr = rect_f(rr_l, ry, rr_w, cell_h);
                    let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&rr, mx, my)).unwrap_or(false);
                    if hov {
                        fill_round_rect(rt, rr, 6.0, theme.hover);
                    }
                    if i > 0 {
                        draw_line(rt, bot.left + 16.0, ry - 1.0, bot.right - 16.0, ry - 1.0, theme.grid, 0.8);
                    }
                    let cy = ry + cell_h * 0.5;
                    draw_session_status_dot(rt, theme, rr_l + 10.5, cy, s, row.last_time, now_local);
                    if row.is_pinned {
                        draw_text(
                            rt,
                            "★",
                            &fonts.ui_small_center,
                            rect_f(rr_l + 16.0, ry, 16.0, cell_h),
                            theme.accent,
                        );
                    }

                    let t_str = fmt_relative_time_cn(row.last_time, now_local);
                    let mut hit_buf = FmtBuf::new();
                    let _ = write!(hit_buf, "{:.1}%", row.counts.cache_hit_rate());
                    let hit_col = cache_hit_color(theme, &row.counts);
                    let cost_str = fmt_cost(&row.counts.cost, row.counts.calls, human);
                    let base_title = self.format_session_title(s);
                    let mut prefix = String::new();
                    if row.is_child_row {
                        prefix.push_str("↳ ");
                    }
                    if let Some(tag) = format_source_tag(s.source, show_multi) {
                        prefix.push_str(&format!("[{tag}] "));
                    }
                    if s.is_archived {
                        prefix.push_str("[归档] ");
                    }
                    let title_disp = format!("{prefix}{base_title}");
                    let mut sub_buf = FmtBuf::new();
                    let has_sub_badge = !row.is_child_row && row.subagent_count > 0;
                    if has_sub_badge {
                        let _ = write!(sub_buf, "+{}子", row.subagent_count);
                    }

                    draw_text(rt, &t_str, &fonts.mono_small, rect_f(time_x, ry, 56.0, cell_h), theme.muted);
                    let (actual_title_w, badge_rect) = if has_sub_badge {
                        let bw = (factories.measure_text_width(sub_buf.as_str(), &fonts.ui_small_center) + 10.0).ceil();
                        if title_w > bw + 40.0 {
                            let tw = (title_w - bw - 6.0).max(32.0);
                            let nat = factories.measure_text_width(&title_disp, &fonts.ui_reg);
                            let bx = title_x + nat.min(tw) + 6.0;
                            (tw, Some(rect_f(bx, ry + (cell_h - 16.0) * 0.5, bw, 16.0)))
                        } else {
                            (title_w, None)
                        }
                    } else {
                        (title_w, None)
                    };
                    draw_text(
                        rt,
                        &title_disp,
                        &fonts.ui_reg,
                        rect_f(title_x, ry, actual_title_w, cell_h),
                        if row.is_child_row { theme.sub } else { theme.text },
                    );
                    if let Some(br) = badge_rect {
                        fill_round_rect(rt, br, 8.0, with_alpha(theme.accent, 0.14));
                        draw_text(rt, sub_buf.as_str(), &fonts.ui_small_center, br, theme.accent);
                    }
                    draw_text(rt, &row.primary_model, &fonts.mono_small_right, rect_f(model_x, ry, model_w, cell_h), theme.muted);
                    draw_session_metric_cells(
                        rt,
                        fonts,
                        theme,
                        &sm_x,
                        &sm_w,
                        ry,
                        cell_h,
                        &row.counts,
                        hit_buf.as_str(),
                        hit_col,
                        &cost_str,
                        self.cost_mode,
                        human,
                    );

                    let cid = s.conversation_id.clone();
                    row_rects_out.push((rr, cid.clone()));
                    hits_out.push(HitBox {
                        rect: rr,
                        action: ClickAction::InspectSession(cid),
                    });
                }
            }
            (row_rects_out, hits_out)
        });
        self.session_row_rects.extend(new_row_rects);
        self.hit_boxes.extend(new_hits);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_compact_models(
        &mut self,
        factories: &D2DFactories,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        bounds: D2D_RECT_F,
        human: bool,
    ) {
        let sort = match self.model_card_metric {
            ModelCardMetric::Volume => {
                if self.cost_mode { ModelSort::CostDesc } else { ModelSort::TotalDesc }
            }
            ModelCardMetric::CacheHit => ModelSort::HitRateDesc,
        };
        let (primary, secondary) = Self::ordered_models_of(&self.cached_agg.by_model, sort);
        if primary.is_empty() && secondary.is_empty() {
            draw_text(rt, "当前范围暂无模型数据", &fonts.ui_small, bounds, theme.muted);
            return;
        }
        let avail_h = (bounds.bottom - bounds.top).max(24.0);
        let max_show: usize = if avail_h >= 94.0 { 4 } else if avail_h >= 66.0 { 3 } else { 2 };
        let show_secondary = !secondary.is_empty()
            && (primary.is_empty() || primary.len() < max_show || max_show >= 3);
        let max_primary = if show_secondary { max_show.saturating_sub(1) } else { max_show };

        let mut rows: Vec<(&str, TokenCounts, bool)> = primary
            .into_iter()
            .take(max_primary)
            .map(|(n, c)| (n, c, false))
            .collect();
        if show_secondary {
            if let Some((n, c)) = secondary.into_iter().next() {
                rows.push((n, c, true));
            }
        }

        let show_n = rows.len().max(1);
        let total_all_tok = self.cached_agg.total.total_tokens().max(1);
        let total_all_usd = self.cached_agg.total.cost.total_usd();
        let row_h = (avail_h / show_n as f32).clamp(21.5, 34.0);
        let w = bounds.right - bounds.left;
        let palette = [theme.accent, theme.output, theme.ram];

        let formatted: Vec<(&str, TokenCounts, bool, f32, String, String)> = rows
            .into_iter()
            .map(|(name, c, is_sec)| {
                let (share, stat) = if self.cost_mode {
                    let sh = if total_all_usd > 0.0 {
                        (c.cost.total_usd() / total_all_usd) as f32
                    } else {
                        0.0
                    };
                    (sh, format!("{} ({:.0}%)", fmt_cost(&c.cost, c.calls, human), sh * 100.0))
                } else {
                    let sh = (c.total_tokens() as f64 / total_all_tok as f64) as f32;
                    (sh, format!("{} ({:.0}%)", fmt_num(c.total_tokens(), human), sh * 100.0))
                };
                let hit_s = format!("{:.1}%", c.cache_hit_rate());
                (name, c, is_sec, share, stat, hit_s)
            })
            .collect();

        let hit_w = formatted
            .iter()
            .map(|(_, _, _, _, _, h)| factories.measure_text_width(h, &fonts.mono_med_right))
            .fold(40.0_f32, f32::max)
            .min(w * 0.22)
            + 4.0;
        let stat_w = formatted
            .iter()
            .map(|(_, _, _, _, s, _)| factories.measure_text_width(s, &fonts.mono_small_right))
            .fold(74.0_f32, f32::max)
            .min(w * 0.36)
            + 6.0;
        let name_w = (w - hit_w - stat_w - 8.0).max(96.0);
        let stat_l = bounds.left + name_w + 4.0;

        let mut hover_tip: Option<(f32, &str, String)> = None;
        let mut row_rects = Vec::with_capacity(formatted.len());

        for (i, (name, c, is_sec, share, stat, hit_s)) in formatted.into_iter().enumerate() {
            let y = bounds.top + i as f32 * row_h;
            if is_sec && i > 0 {
                draw_line(rt, bounds.left, y - 2.0, bounds.right, y - 2.0, theme.border, 0.8);
            }
            let row_r = rect_f(bounds.left - 4.0, y - 1.5, w + 8.0, row_h.min(24.0));
            row_rects.push(row_r);
            let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&row_r, mx, my)).unwrap_or(false);
            if hov {
                fill_round_rect(rt, row_r, 5.0, with_alpha(theme.text, 0.04));
                let detail = format!(
                    "{} 次 · {} · {} · 命中 {}",
                    c.calls,
                    fmt_compact(c.total_tokens()),
                    fmt_cost_compact(&c.cost, c.calls),
                    hit_s
                );
                hover_tip = Some((y, name, detail));
            }
            let hit = c.cache_hit_rate();
            let hit_col = if is_sec { theme.muted } else { cache_hit_color(theme, &c) };
            let name_col = if hov {
                theme.accent
            } else if is_sec {
                theme.muted
            } else {
                theme.text
            };
            let stat_col = if is_sec { theme.muted } else { theme.sub };

            draw_text(rt, name, &fonts.mono_small, rect_f(bounds.left, y, name_w, 15.0), name_col);
            draw_text(rt, &stat, &fonts.mono_small_right, rect_f(stat_l, y, stat_w, 15.0), stat_col);
            draw_text(
                rt,
                &hit_s,
                if is_sec { &fonts.mono_small_right } else { &fonts.mono_med_right },
                rect_f(bounds.right - hit_w, y, hit_w, 15.0),
                hit_col,
            );

            let (bar_frac, bar_col) = match self.model_card_metric {
                ModelCardMetric::Volume => (
                    share,
                    if is_sec { theme.cache } else { palette[i % palette.len()] },
                ),
                ModelCardMetric::CacheHit => (
                    (hit as f32 / 100.0).clamp(0.0, 1.0),
                    if is_sec { theme.cache } else { hit_col },
                ),
            };
            fill_round_rect(rt, rect_f(bounds.left, y + 16.5, w, 3.0), 1.5, theme.grid);
            if bar_frac > 0.0 {
                fill_round_rect(rt, rect_f(bounds.left, y + 16.5, (w * bar_frac).max(3.0), 3.0), 1.5, bar_col);
            }
            self.hit_boxes.push(HitBox {
                rect: row_r,
                action: ClickAction::FilterSessions(SessionFilter::Model(name.to_string())),
            });
        }
        self.discrete_plots.push(DiscretePlot::RowRects {
            bounds: rect_f(bounds.left - 4.0, bounds.top - 2.0, w + 8.0, avail_h + 4.0),
            rows: row_rects,
        });

        // Compact floating tooltip when hovering a row whose model name is truncated (narrow window)
        if let Some((ry, full_name, detail)) = hover_tip {
            let full_w = factories.measure_text_width(full_name, &fonts.mono_small);
            if full_w > name_w - 2.0 {
                let tw = full_w.max(factories.measure_text_width(&detail, &fonts.mono_small)) + 20.0;
                let tw = tw.clamp(180.0, (w + 16.0).max(180.0));
                let th = 40.0;
                let tx = (bounds.right - tw).max(bounds.left - 4.0);
                let ty = if ry - th - 4.0 >= bounds.top - 26.0 {
                    ry - th - 4.0
                } else {
                    (ry + 22.0).min(bounds.bottom - th)
                };
                let tr = rect_f(tx, ty, tw, th);
                fill_round_rect(rt, tr, 6.0, theme.tooltip_bg);
                stroke_round_rect(rt, tr, 6.0, theme.border, 1.0);
                draw_text(rt, full_name, &fonts.mono_small, rect_f(tx + 9.0, ty + 5.0, tw - 18.0, 15.0), theme.text);
                draw_text(rt, &detail, &fonts.mono_small, rect_f(tx + 9.0, ty + 20.0, tw - 18.0, 15.0), theme.sub);
            }
        }
    }

    fn draw_compact_workspaces(&mut self, rt: &ID2D1RenderTarget, fonts: &DashFonts, theme: &Theme, bounds: D2D_RECT_F, human: bool) {
        let mut refs: Vec<(&str, usize, TokenCounts)> = self
            .cached_agg
            .by_workspace
            .iter()
            .map(|(ws, &(cnt, c))| (ws.as_str(), cnt, c))
            .collect();
        if self.cost_mode {
            refs.sort_by(|a, b| {
                b.2.cost
                    .total_usd()
                    .partial_cmp(&a.2.cost.total_usd())
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.2.total_tokens().cmp(&a.2.total_tokens()))
            });
        } else {
            refs.sort_by(|a, b| b.2.total_tokens().cmp(&a.2.total_tokens()));
        }
        if refs.is_empty() {
            draw_text(rt, "当前范围暂无项目数据", &fonts.ui_small, bounds, theme.muted);
            return;
        }
        let avail_h = (bounds.bottom - bounds.top).max(24.0);
        let max_show = if avail_h >= 82.0 { 3 } else { 2 };
        let show_n = refs.len().min(max_show);
        let items: Vec<(String, String, usize, TokenCounts)> = refs
            .into_iter()
            .take(show_n)
            .map(|(ws, cnt, c)| (ws.to_string(), self.format_workspace_label(ws), cnt, c))
            .collect();
        let total_all_tok = self.cached_agg.total.total_tokens().max(1);
        let total_all_usd = self.cached_agg.total.cost.total_usd();
        let row_h = (avail_h / show_n as f32).clamp(24.0, 34.0);
        let w = bounds.right - bounds.left;
        let palette = [theme.accent, theme.output, theme.ram];

        for (i, (full_ws, name, sess_cnt, c)) in items.into_iter().enumerate() {
            let y = bounds.top + i as f32 * row_h;
            let row_r = rect_f(bounds.left - 4.0, y - 2.0, w + 8.0, row_h.min(27.0));
            let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&row_r, mx, my)).unwrap_or(false);
            if hov {
                fill_round_rect(rt, row_r, 5.0, with_alpha(theme.text, 0.04));
            }
            let (share, stat) = if self.cost_mode {
                let sh = if total_all_usd > 0.0 {
                    (c.cost.total_usd() / total_all_usd) as f32
                } else {
                    0.0
                };
                (
                    sh,
                    format!("{} ({:.0}%) · {}会话", fmt_cost(&c.cost, c.calls, human), sh * 100.0, sess_cnt),
                )
            } else {
                let sh = (c.total_tokens() as f64 / total_all_tok as f64) as f32;
                (
                    sh,
                    format!("{} ({:.0}%) · {}会话", fmt_num(c.total_tokens(), human), sh * 100.0, sess_cnt),
                )
            };
            draw_text(rt, &name, &fonts.mono_small, rect_f(bounds.left, y, w * 0.48, 16.0), if hov { theme.accent } else { theme.text });
            draw_text(rt, &stat, &fonts.mono_small_right, rect_f(bounds.left + w * 0.46, y, w * 0.54, 16.0), theme.sub);
            fill_round_rect(rt, rect_f(bounds.left, y + 18.0, w, 3.5), 1.75, theme.grid);
            if share > 0.0 {
                fill_round_rect(rt, rect_f(bounds.left, y + 18.0, (w * share).max(3.0), 3.5), 1.75, palette[i % palette.len()]);
            }
            self.hit_boxes.push(HitBox {
                rect: row_r,
                action: ClickAction::FilterSessions(SessionFilter::Workspace(full_ws)),
            });
        }
    }

    fn draw_compact_tools(&mut self, rt: &ID2D1RenderTarget, fonts: &DashFonts, theme: &Theme, bounds: D2D_RECT_F) {
        let mut items: Vec<(&str, u64)> = self
            .cached_agg
            .by_tool
            .iter()
            .map(|(k, &v)| (k.as_str(), v))
            .collect();
        items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        if items.is_empty() {
            draw_text(rt, "当前范围暂无工具调用记录", &fonts.ui_small, bounds, theme.muted);
            return;
        }
        let total: u64 = items.iter().map(|(_, v)| *v).sum::<u64>().max(1);
        let avail_h = (bounds.bottom - bounds.top).max(24.0);
        let max_show = if avail_h >= 94.0 { 4 } else if avail_h >= 66.0 { 3 } else { 2 };
        let show_n = items.len().min(max_show);
        let row_h = (avail_h / show_n as f32).clamp(21.5, 32.0);
        let w = bounds.right - bounds.left;
        let palette = [theme.accent, theme.output, theme.ram, theme.sage];

        for (i, (name, cnt)) in items.into_iter().take(show_n).enumerate() {
            let y = bounds.top + i as f32 * row_h;
            let share = (cnt as f64 / total as f64) as f32;
            draw_text(rt, name, &fonts.mono_small, rect_f(bounds.left, y, w * 0.60, 15.0), theme.text);
            draw_text(
                rt,
                &format!("{} 次 ({:.0}%)", cnt, share * 100.0),
                &fonts.mono_small_right,
                rect_f(bounds.left + w * 0.56, y, w * 0.44, 15.0),
                theme.sub,
            );
            fill_round_rect(rt, rect_f(bounds.left, y + 16.5, w, 3.0), 1.5, theme.grid);
            if share > 0.0 {
                fill_round_rect(rt, rect_f(bounds.left, y + 16.5, (w * share).max(3.0), 3.0), 1.5, palette[i % palette.len()]);
            }
        }
    }

    fn draw_compact_sources(
        &mut self,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        bounds: D2D_RECT_F,
        human: bool,
    ) {
        let mut items: Vec<(AgentFamily, usize, TokenCounts)> = self
            .cached_agg
            .by_agent
            .iter()
            .map(|(&fam, &(cnt, c))| (fam, cnt, c))
            .collect();
        if self.cost_mode {
            items.sort_by(|a, b| {
                b.2.cost
                    .total_usd()
                    .partial_cmp(&a.2.cost.total_usd())
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.2.total_tokens().cmp(&a.2.total_tokens()))
                    .then_with(|| a.0.cmp(&b.0))
            });
        } else {
            items.sort_by(|a, b| {
                b.2.total_tokens()
                    .cmp(&a.2.total_tokens())
                    .then_with(|| a.0.cmp(&b.0))
            });
        }
        if items.is_empty() {
            draw_text(rt, "当前范围暂无来源数据", &fonts.ui_small, bounds, theme.muted);
            return;
        }
        let avail_h = (bounds.bottom - bounds.top).max(24.0);
        let max_show = if avail_h >= 94.0 { 4 } else if avail_h >= 66.0 { 3 } else { 2 };
        let show_n = items.len().min(max_show);
        let total_all_tok = self.cached_agg.total.total_tokens().max(1);
        let total_all_usd = self.cached_agg.total.cost.total_usd();
        let row_h = (avail_h / show_n as f32).clamp(21.5, 34.0);
        let w = bounds.right - bounds.left;
        let palette = [theme.accent, theme.output, theme.ram, theme.sage];

        for (i, (fam, sess_cnt, c)) in items.into_iter().take(show_n).enumerate() {
            let y = bounds.top + i as f32 * row_h;
            let row_r = rect_f(bounds.left - 4.0, y - 1.5, w + 8.0, row_h.min(25.0));
            let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&row_r, mx, my)).unwrap_or(false);
            if hov {
                fill_round_rect(rt, row_r, 5.0, with_alpha(theme.text, 0.04));
            }
            let (share, stat) = if self.cost_mode {
                let sh = if total_all_usd > 0.0 {
                    (c.cost.total_usd() / total_all_usd) as f32
                } else {
                    0.0
                };
                (
                    sh,
                    format!(
                        "{} ({:.0}%) · {}会话",
                        fmt_cost(&c.cost, c.calls, human),
                        sh * 100.0,
                        sess_cnt
                    ),
                )
            } else {
                let sh = (c.total_tokens() as f64 / total_all_tok as f64) as f32;
                (
                    sh,
                    format!(
                        "{} ({:.0}%) · {}会话",
                        fmt_num(c.total_tokens(), human),
                        sh * 100.0,
                        sess_cnt
                    ),
                )
            };
            draw_text(
                rt,
                fam.label_full(),
                &fonts.mono_small,
                rect_f(bounds.left, y, w * 0.44, 15.0),
                if hov { theme.accent } else { theme.text },
            );
            draw_text(
                rt,
                &stat,
                &fonts.mono_small_right,
                rect_f(bounds.left + w * 0.42, y, w * 0.58, 15.0),
                theme.sub,
            );
            fill_round_rect(rt, rect_f(bounds.left, y + 16.5, w, 3.0), 1.5, theme.grid);
            if share > 0.0 {
                fill_round_rect(
                    rt,
                    rect_f(bounds.left, y + 16.5, (w * share).max(3.0), 3.0),
                    1.5,
                    palette[i % palette.len()],
                );
            }
            self.hit_boxes.push(HitBox {
                rect: row_r,
                action: ClickAction::FilterSessions(SessionFilter::Agent(fam)),
            });
        }
    }

    fn draw_chart_legend_and_toggle(&mut self, rt: &ID2D1RenderTarget, fonts: &DashFonts, theme: &Theme, card: D2D_RECT_F) {
        let _ = self.draw_chart_legend_and_toggle_at(rt, fonts, theme, card, card.top + 9.0);
    }

    /// Draws the stacked-bar color legend + `总计 | 仅增量` switch at a caller-specified `y_top`
    /// and returns the left-most X coordinate occupied so caller text never collides on narrow windows.
    fn draw_chart_legend_and_toggle_at(
        &mut self,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        card: D2D_RECT_F,
        y_top: f32,
    ) -> f32 {
        let tw = 112.0;
        let tr = rect_f(card.right - tw - 14.0, y_top, tw, 22.0);
        fill_round_rect(rt, tr, 6.0, theme.surface_subtle);
        let r_tot = rect_f(tr.left + 2.0, tr.top + 2.0, 52.0, 18.0);
        let r_inc = rect_f(tr.left + 56.0, tr.top + 2.0, 54.0, 18.0);
        if !self.fresh_only {
            fill_round_rect(rt, r_tot, 4.5, theme.hover);
        } else {
            fill_round_rect(rt, r_inc, 4.5, theme.accent_soft);
        }
        draw_text(
            rt,
            if self.cost_mode { "总费用" } else { "总计" },
            &fonts.ui_small_center,
            r_tot,
            if !self.fresh_only { theme.text } else { theme.muted },
        );
        draw_text(
            rt,
            if self.cost_mode { "非缓存" } else { "仅增量" },
            &fonts.ui_small_center,
            r_inc,
            if self.fresh_only { theme.accent } else { theme.muted },
        );
        self.hit_boxes.push(HitBox { rect: tr, action: ClickAction::ToggleFreshOnly });

        let mut lx = tr.left - 168.0;
        let ly = y_top + 11.0;
        let mut left_edge = tr.left;
        if lx > card.left + 152.0 {
            left_edge = lx - 6.0;
            if !self.fresh_only {
                fill_circle(rt, lx, ly, 3.0, theme.cache);
                draw_text(rt, "缓存", &fonts.ui_micro, rect_f(lx + 6.0, ly - 7.0, 34.0, 14.0), theme.muted);
                lx += 48.0;
            } else {
                lx += 48.0;
            }
            fill_circle(rt, lx, ly, 3.0, theme.accent);
            draw_text(rt, "新入", &fonts.ui_micro, rect_f(lx + 6.0, ly - 7.0, 34.0, 14.0), theme.muted);
            lx += 48.0;
            fill_circle(rt, lx, ly, 3.0, theme.output);
            draw_text(rt, "输出", &fonts.ui_micro, rect_f(lx + 6.0, ly - 7.0, 34.0, 14.0), theme.muted);
        }
        left_edge
    }

    // ---------- Tab 2: Direct Daily Analytics ----------

    fn draw_daily_tab(
        &mut self,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        body: D2D_RECT_F,
        snapshot: &ScanSnapshot,
        human: bool,
    ) {
        let bw = body.right - body.left;
        let bh = body.bottom - body.top;
        let chart_h = (bh * 0.50).clamp(190.0, 275.0);
        let chart_c = rect_f(body.left, body.top, bw, chart_h);
        fill_round_rect(rt, chart_c, 10.0, theme.surface);
        stroke_round_rect(rt, chart_c, 10.0, theme.border, 1.0);

        let mut dx = chart_c.left + 14.0;
        for (mode, lbl, dw) in [
            (DailyViewMode::Bars, "按天柱图", 64.0),
            (DailyViewMode::Heatmap, "活跃热力图", 74.0),
        ] {
            let r = rect_f(dx, chart_c.top + 9.0, dw, 22.0);
            let act = self.daily_view == mode;
            let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&r, mx, my)).unwrap_or(false);
            if act {
                fill_round_rect(rt, r, 6.0, theme.hover);
            } else if hov {
                fill_round_rect(rt, r, 6.0, with_alpha(theme.text, 0.04));
            }
            draw_text(rt, lbl, &fonts.ui_small_center, r, if act { theme.text } else if hov { theme.sub } else { theme.muted });
            self.hit_boxes.push(HitBox { rect: r, action: ClickAction::SelectDailyView(mode) });
            dx += dw + 4.0;
        }
        if self.daily_view == DailyViewMode::Bars {
            self.draw_chart_legend_and_toggle(rt, fonts, theme, chart_c);
        } else {
            draw_text(
                rt,
                if self.cost_mode { "近 20 周每日估算费用热度" } else { "近 20 周每日 Token 热度" },
                &fonts.ui_small_right,
                rect_f(chart_c.right - 190.0, chart_c.top + 11.0, 174.0, 18.0),
                theme.muted,
            );
        }

        let plot = rect_f(chart_c.left + 12.0, chart_c.top + 40.0, bw - 24.0, chart_h - 48.0);
        let tbl_r = rect_f(body.left, chart_c.bottom + 12.0, bw, (body.bottom - chart_c.bottom - 12.0).max(110.0));

        let days_asc: Vec<(&String, &TokenCounts)> = snapshot.agg.by_day.iter().collect();

        if self.daily_view == DailyViewMode::Heatmap {
            let today = Local::now().date_naive();
            let n_weeks: usize = 20;
            let wd_today = today.weekday().num_days_from_monday() as i64;
            let end_sun = today + chrono::Duration::days(6 - wd_today);
            let start_mon = end_sun - chrono::Duration::days((n_weeks as i64) * 7 - 1);
            let mut h_days = Vec::with_capacity(n_weeks * 7);
            let mut h_meta: Vec<(NaiveDate, TokenCounts)> = Vec::with_capacity(n_weeks * 7);
            let mut prev_month = 0u32;
            let mut ymd_buf = [0u8; 10];
            for offset in 0..(n_weeks * 7) {
                let d = start_mon + chrono::Duration::days(offset as i64);
                if d > today {
                    continue;
                }
                let y = d.year().unsigned_abs();
                let m = d.month();
                let day_num = d.day();
                ymd_buf[0] = b'0' + ((y / 1000) % 10) as u8;
                ymd_buf[1] = b'0' + ((y / 100) % 10) as u8;
                ymd_buf[2] = b'0' + ((y / 10) % 10) as u8;
                ymd_buf[3] = b'0' + (y % 10) as u8;
                ymd_buf[4] = b'-';
                ymd_buf[5] = b'0' + (m / 10) as u8;
                ymd_buf[6] = b'0' + (m % 10) as u8;
                ymd_buf[7] = b'-';
                ymd_buf[8] = b'0' + (day_num / 10) as u8;
                ymd_buf[9] = b'0' + (day_num % 10) as u8;
                let k_str = std::str::from_utf8(&ymd_buf).unwrap_or("");
                let c = snapshot.agg.by_day.get(k_str).copied().unwrap_or_default();
                let val = if self.cost_mode {
                    c.cost.total_usd()
                } else {
                    c.total_tokens() as f64
                };
                let month_label = if m != prev_month && d.weekday() == chrono::Weekday::Mon {
                    prev_month = m;
                    Some(format!("{m}月"))
                } else if offset == 0 {
                    prev_month = m;
                    Some(format!("{m}月"))
                } else {
                    None
                };
                h_meta.push((d, c));
                h_days.push(HeatmapDay {
                    date_key: if c.calls > 0 { k_str.to_string() } else { String::new() },
                    weekday_idx: d.weekday().num_days_from_monday() as usize,
                    week_col: offset / 7,
                    month_label,
                    value: val,
                    calls: c.calls,
                });
            }
            let (cells, geom) = draw_activity_heatmap(
                rt,
                fonts,
                theme,
                plot,
                n_weeks,
                &h_days,
                self.mouse_dip,
                |idx| {
                    let (d, c) = h_meta[idx];
                    let k = d.format("%Y-%m-%d");
                    if c.calls > 0 {
                        vec![
                            format!("{} ({}) · {} 次调用", k, weekday_cn(d), c.calls),
                            format!("总计   : {}", fmt_commas(c.total_tokens())),
                            format!("估算费 : {} (省 {})", fmt_cost(&c.cost, c.calls, false), fmt_usd_compact(c.cost.cache_saved_usd)),
                            format!("命中率 : {:.1}%  |  峰值Ctx: {}", c.cache_hit_rate(), fmt_compact(c.peak_context)),
                        ]
                    } else {
                        vec![format!("{} ({}) · 无使用记录", k, weekday_cn(d))]
                    }
                },
            );
            if let Some(g) = geom {
                self.discrete_plots.push(DiscretePlot::Heatmap(g));
            }
            for (rect, day_key) in cells {
                self.hit_boxes.push(HitBox {
                    rect,
                    action: ClickAction::FilterSessions(SessionFilter::Date(day_key)),
                });
            }
        } else {
            let chart_days: Vec<(String, TokenCounts)> = if days_asc.len() < 7 && !days_asc.is_empty() {
                let today = Local::now().date_naive();
                (0..7u64)
                    .rev()
                    .filter_map(|off| today.checked_sub_days(chrono::Days::new(off)))
                    .map(|d| {
                        let k = d.format("%Y-%m-%d").to_string();
                        (k.clone(), snapshot.agg.by_day.get(&k).copied().unwrap_or_default())
                    })
                    .collect()
            } else {
                let s = days_asc.len().saturating_sub(30);
                days_asc[s..].iter().map(|(k, c)| ((*k).clone(), **c)).collect()
            };
            let bars: Vec<BarItem> = chart_days
                .iter()
                .map(|(day, c)| {
                    let lbl = if day.len() >= 10 { day[5..].to_string() } else { day.clone() };
                    self.make_bar(lbl, c, false)
                })
                .collect();
            let cost_mode = self.cost_mode;
            let (day_slots, inner) = draw_editorial_bars(
                rt,
                fonts,
                theme,
                plot,
                &bars,
                self.fresh_only,
                self.bar_unit(),
                self.mouse_dip,
                "暂无按天数据",
                |idx| {
                    let (day, c) = &chart_days[idx];
                    if cost_mode {
                        vec![
                            format!("{} · {} 次调用", day, c.calls),
                            format!("估算费用: {} (省 {})", fmt_cost(&c.cost, c.calls, false), fmt_usd_compact(c.cost.cache_saved_usd)),
                            format!("新输入  : {} ({})", fmt_usd_precise(c.cost.input_plus_write_usd()), fmt_compact(c.input_tokens)),
                            format!("缓存读  : {} ({:.1}%)", fmt_usd_precise(c.cost.cache_read_usd), c.cache_hit_rate()),
                            format!("输出    : {} ({})", fmt_usd_precise(c.cost.output_usd), fmt_compact(c.output_tokens)),
                            format!("总Token : {}", fmt_commas(c.total_tokens())),
                        ]
                    } else {
                        vec![
                            format!("{} · {} 次调用", day, c.calls),
                            format!("总计   : {}", fmt_commas(c.total_tokens())),
                            format!("估算费 : {}", fmt_cost(&c.cost, c.calls, false)),
                            format!("新输入 : {}", fmt_commas(c.input_tokens)),
                            format!("缓存读 : {} ({:.1}%)", fmt_commas(c.cache_read_tokens), c.cache_hit_rate()),
                            format!("输出   : {}", fmt_commas(c.output_tokens)),
                            format!("峰值Ctx: {}", fmt_commas(c.peak_context)),
                        ]
                    }
                },
            );
            if let Some(p) = inner {
                self.discrete_plots.push(DiscretePlot::VerticalBars {
                    plot: p,
                    n: bars.len(),
                });
            }
            for (rect, (day, c)) in day_slots.into_iter().zip(chart_days.iter()) {
                if c.calls > 0 {
                    self.hit_boxes.push(HitBox {
                        rect,
                        action: ClickAction::FilterSessions(SessionFilter::Date(day.clone())),
                    });
                }
            }
        }

        let cols: &[(&str, f32, bool)] = if self.cost_mode {
            &[
                ("日期", 0.11, false),
                ("星期", 0.055, false),
                ("调用", 0.055, true),
                ("估算总费", 0.12, true),
                ("新入费用", 0.11, true),
                ("输出费用", 0.10, true),
                ("缓存费用", 0.10, true),
                ("缓存已省", 0.11, true),
                ("命中率", 0.10, true),
                ("总 Token", 0.14, true),
            ]
        } else {
            &[
                ("日期", 0.12, false),
                ("星期", 0.06, false),
                ("调用", 0.06, true),
                ("总 Token", 0.14, true),
                ("新输入", 0.13, true),
                ("输出", 0.11, true),
                ("缓存读", 0.13, true),
                ("命中率", 0.11, true),
                ("估算费用", 0.14, true),
            ]
        };
        let cost_mode = self.cost_mode;
        let total_days = days_asc.len();
        self.draw_table_virtual(rt, fonts, theme, tbl_r, cols, total_days, None, None, |abs_idx, cells| {
            let (day, c) = days_asc[total_days - 1 - abs_idx];
            let wd = NaiveDate::parse_from_str(day, "%Y-%m-%d").map(weekday_cn).unwrap_or("-");
            if cost_mode {
                cells.extend([
                    day.clone(),
                    wd.into(),
                    c.calls.to_string(),
                    fmt_cost(&c.cost, c.calls, human),
                    fmt_usd(c.cost.input_plus_write_usd(), human),
                    fmt_usd(c.cost.output_usd, human),
                    fmt_usd(c.cost.cache_read_usd, human),
                    fmt_usd(c.cost.cache_saved_usd, human),
                    format!("{:.1}%", c.cache_hit_rate()),
                    fmt_num(c.total_tokens(), human),
                ]);
            } else {
                cells.extend([
                    day.clone(),
                    wd.into(),
                    c.calls.to_string(),
                    fmt_num(c.total_tokens(), human),
                    fmt_num(c.input_tokens + c.cache_write_tokens, human),
                    fmt_num(c.output_tokens, human),
                    fmt_num(c.cache_read_tokens, human),
                    format!("{:.1}%", c.cache_hit_rate()),
                    fmt_cost(&c.cost, c.calls, human),
                ]);
            }
            Some(ClickAction::FilterSessions(SessionFilter::Date(day.clone())))
        });
    }

    // ---------- Tab 3: Direct ISO Weekly (with quiet Monthly sub-toggle) ----------

    fn draw_weekly_tab(
        &mut self,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        body: D2D_RECT_F,
        snapshot: &ScanSnapshot,
        human: bool,
    ) {
        let bw = body.right - body.left;
        let bh = body.bottom - body.top;
        let chart_h = (bh * 0.50).clamp(190.0, 275.0);
        let chart_c = rect_f(body.left, body.top, bw, chart_h);
        fill_round_rect(rt, chart_c, 10.0, theme.surface);
        stroke_round_rect(rt, chart_c, 10.0, theme.border, 1.0);

        // Quiet Weekly / Monthly sub-toggle inside card header
        let mut gx = chart_c.left + 14.0;
        for (mode, lbl, gw) in [
            (WeeklyMode::Weekly, "按周 (ISO)", 76.0),
            (WeeklyMode::Monthly, "按月", 46.0),
        ] {
            let r = rect_f(gx, chart_c.top + 9.0, gw, 22.0);
            let act = self.weekly_mode == mode;
            if act {
                fill_round_rect(rt, r, 6.0, theme.hover);
            }
            draw_text(rt, lbl, &fonts.ui_small_center, r, if act { theme.text } else { theme.muted });
            self.hit_boxes.push(HitBox { rect: r, action: ClickAction::SelectWeeklyMode(mode) });
            gx += gw + 4.0;
        }
        self.draw_chart_legend_and_toggle(rt, fonts, theme, chart_c);

        let plot = rect_f(chart_c.left + 12.0, chart_c.top + 40.0, bw - 24.0, chart_h - 48.0);
        let tbl_r = rect_f(body.left, chart_c.bottom + 12.0, bw, (body.bottom - chart_c.bottom - 12.0).max(110.0));

        match self.weekly_mode {
            WeeklyMode::Weekly => {
                let weeks_asc: Vec<(&String, &TokenCounts)> = snapshot.agg.by_week.iter().collect();
                let cost_mode = self.cost_mode;
                let wow_at = |asc_idx: usize| -> String {
                    if asc_idx == 0 {
                        return "-".into();
                    }
                    let prev_c = weeks_asc[asc_idx - 1].1;
                    let cur_c = weeks_asc[asc_idx].1;
                    let prev_v = if cost_mode { prev_c.cost.total_usd() } else { prev_c.total_tokens() as f64 };
                    let cur_v = if cost_mode { cur_c.cost.total_usd() } else { cur_c.total_tokens() as f64 };
                    if prev_v > 0.0 {
                        format!("{:+.1}%", ((cur_v - prev_v) / prev_v) * 100.0)
                    } else {
                        "-".into()
                    }
                };
                let start = weeks_asc.len().saturating_sub(16);
                let bars: Vec<BarItem> = weeks_asc[start..]
                    .iter()
                    .map(|(wk, c)| {
                        let lbl = wk.split('-').last().unwrap_or(wk).to_string();
                        self.make_bar(lbl, c, false)
                    })
                    .collect();
                let (wk_slots, inner) = draw_editorial_bars(
                    rt,
                    fonts,
                    theme,
                    plot,
                    &bars,
                    self.fresh_only,
                    self.bar_unit(),
                    self.mouse_dip,
                    "暂无按周数据",
                    |rel_idx| {
                        let asc_idx = start + rel_idx;
                        let (wk, c) = weeks_asc[asc_idx];
                        let ad = snapshot.agg.week_active_days.get(wk).copied().unwrap_or(1).max(1) as u64;
                        let w_str = wow_at(asc_idx);
                        if cost_mode {
                            vec![
                                format!("{} ({})", wk, iso_week_range_label(wk)),
                                format!("估算费用: {} (环比 {})", fmt_cost(&c.cost, c.calls, false), w_str),
                                format!("日均费用: {} ({} 活跃天)", fmt_usd_compact(c.cost.total_usd() / ad as f64), ad),
                                format!("新输入  : {}  |  输出: {}", fmt_usd_precise(c.cost.input_plus_write_usd()), fmt_usd_precise(c.cost.output_usd)),
                                format!("缓存读  : {} (省 {})", fmt_usd_precise(c.cost.cache_read_usd), fmt_usd_compact(c.cost.cache_saved_usd)),
                                format!("总Token : {}", fmt_commas(c.total_tokens())),
                            ]
                        } else {
                            vec![
                                format!("{} ({})", wk, iso_week_range_label(wk)),
                                format!("总计   : {} (环比 {})", fmt_commas(c.total_tokens()), w_str),
                                format!("估算费 : {} (省 {})", fmt_cost(&c.cost, c.calls, false), fmt_usd_compact(c.cost.cache_saved_usd)),
                                format!("日均   : {} ({} 活跃天)", fmt_commas(c.total_tokens() / ad), ad),
                                format!("新输入 : {}", fmt_commas(c.input_tokens)),
                                format!("缓存读 : {} ({:.1}%)", fmt_commas(c.cache_read_tokens), c.cache_hit_rate()),
                                format!("输出   : {}", fmt_commas(c.output_tokens)),
                            ]
                        }
                    },
                );
                if let Some(p) = inner {
                    self.discrete_plots.push(DiscretePlot::VerticalBars {
                        plot: p,
                        n: bars.len(),
                    });
                }
                for (rect, (wk, c)) in wk_slots.into_iter().zip(weeks_asc[start..].iter()) {
                    if c.calls > 0 {
                        self.hit_boxes.push(HitBox {
                            rect,
                            action: ClickAction::FilterSessions(SessionFilter::Week((*wk).clone())),
                        });
                    }
                }

                let cols: &[(&str, f32, bool)] = if self.cost_mode {
                    &[
                        ("ISO 周", 0.09, false),
                        ("周一至周日", 0.095, false),
                        ("活跃", 0.05, true),
                        ("调用", 0.05, true),
                        ("估算总费", 0.105, true),
                        ("日均费用", 0.09, true),
                        ("新入费用", 0.085, true),
                        ("输出费用", 0.085, true),
                        ("缓存已省", 0.085, true),
                        ("命中率", 0.08, true),
                        ("周环比", 0.085, true),
                        ("总 Token", 0.10, true),
                    ]
                } else {
                    &[
                        ("ISO 周", 0.095, false),
                        ("周一至周日", 0.11, false),
                        ("活跃", 0.05, true),
                        ("调用", 0.055, true),
                        ("总 Token", 0.11, true),
                        ("日均", 0.095, true),
                        ("新输入", 0.10, true),
                        ("输出", 0.09, true),
                        ("命中率", 0.09, true),
                        ("周环比", 0.095, true),
                        ("估算费用", 0.11, true),
                    ]
                };
                let total_weeks = weeks_asc.len();
                self.draw_table_virtual(rt, fonts, theme, tbl_r, cols, total_weeks, None, None, |abs_idx, cells| {
                    let asc_idx = total_weeks - 1 - abs_idx;
                    let (wk, c) = weeks_asc[asc_idx];
                    let ad = snapshot.agg.week_active_days.get(wk).copied().unwrap_or(0);
                    let w_str = wow_at(asc_idx);
                    if cost_mode {
                        let avg_usd = if ad > 0 { c.cost.total_usd() / ad as f64 } else { 0.0 };
                        cells.extend([
                            wk.clone(),
                            iso_week_range_label(wk),
                            format!("{}天", ad),
                            c.calls.to_string(),
                            fmt_cost(&c.cost, c.calls, human),
                            fmt_usd(avg_usd, human),
                            fmt_usd(c.cost.input_plus_write_usd(), human),
                            fmt_usd(c.cost.output_usd, human),
                            fmt_usd(c.cost.cache_saved_usd, human),
                            format!("{:.1}%", c.cache_hit_rate()),
                            w_str,
                            fmt_num(c.total_tokens(), human),
                        ]);
                    } else {
                        let avg = if ad > 0 { c.total_tokens() / ad as u64 } else { 0 };
                        cells.extend([
                            wk.clone(),
                            iso_week_range_label(wk),
                            format!("{}天", ad),
                            c.calls.to_string(),
                            fmt_num(c.total_tokens(), human),
                            fmt_num(avg, human),
                            fmt_num(c.input_tokens + c.cache_write_tokens, human),
                            fmt_num(c.output_tokens, human),
                            format!("{:.1}%", c.cache_hit_rate()),
                            w_str,
                            fmt_cost(&c.cost, c.calls, human),
                        ]);
                    }
                    Some(ClickAction::FilterSessions(SessionFilter::Week(wk.clone())))
                });
            }
            WeeklyMode::Monthly => {
                let m_asc: Vec<(&String, &TokenCounts)> = snapshot.agg.by_month.iter().collect();
                let cost_mode = self.cost_mode;
                let bars: Vec<BarItem> = m_asc
                    .iter()
                    .map(|(m, c)| self.make_bar((*m).clone(), c, false))
                    .collect();
                let (_, inner) = draw_editorial_bars(
                    rt,
                    fonts,
                    theme,
                    plot,
                    &bars,
                    self.fresh_only,
                    self.bar_unit(),
                    self.mouse_dip,
                    "暂无按月数据",
                    |idx| {
                        let (m, c) = m_asc[idx];
                        if cost_mode {
                            vec![
                                format!("{} · {} 次调用", m, c.calls),
                                format!("估算费用: {} (省 {})", fmt_cost(&c.cost, c.calls, false), fmt_usd_compact(c.cost.cache_saved_usd)),
                                format!("新输入  : {}  |  输出: {}", fmt_usd_precise(c.cost.input_plus_write_usd()), fmt_usd_precise(c.cost.output_usd)),
                                format!("缓存读  : {} ({:.1}%)", fmt_usd_precise(c.cost.cache_read_usd), c.cache_hit_rate()),
                                format!("总Token : {}", fmt_commas(c.total_tokens())),
                            ]
                        } else {
                            vec![
                                format!("{} · {} 次调用", m, c.calls),
                                format!("总计   : {}", fmt_commas(c.total_tokens())),
                                format!("估算费 : {}", fmt_cost(&c.cost, c.calls, false)),
                                format!("新输入 : {}", fmt_commas(c.input_tokens)),
                                format!("缓存读 : {} ({:.1}%)", fmt_commas(c.cache_read_tokens), c.cache_hit_rate()),
                                format!("输出   : {}", fmt_commas(c.output_tokens)),
                            ]
                        }
                    },
                );
                if let Some(p) = inner {
                    self.discrete_plots.push(DiscretePlot::VerticalBars {
                        plot: p,
                        n: bars.len(),
                    });
                }

                let cols: &[(&str, f32, bool)] = if self.cost_mode {
                    &[
                        ("月份", 0.10, false),
                        ("调用", 0.07, true),
                        ("估算总费", 0.13, true),
                        ("新入费用", 0.115, true),
                        ("输出费用", 0.105, true),
                        ("缓存费用", 0.11, true),
                        ("缓存已省", 0.115, true),
                        ("命中率", 0.115, true),
                        ("总 Token", 0.14, true),
                    ]
                } else {
                    &[
                        ("月份", 0.10, false),
                        ("调用", 0.07, true),
                        ("总 Token", 0.13, true),
                        ("新输入", 0.125, true),
                        ("输出", 0.11, true),
                        ("缓存读", 0.125, true),
                        ("命中率", 0.10, true),
                        ("缓存已省", 0.115, true),
                        ("估算费用", 0.125, true),
                    ]
                };
                let total_months = m_asc.len();
                self.draw_table_virtual(rt, fonts, theme, tbl_r, cols, total_months, None, None, |abs_idx, cells| {
                    let (m, c) = m_asc[total_months - 1 - abs_idx];
                    if cost_mode {
                        cells.extend([
                            m.clone(),
                            c.calls.to_string(),
                            fmt_cost(&c.cost, c.calls, human),
                            fmt_usd(c.cost.input_plus_write_usd(), human),
                            fmt_usd(c.cost.output_usd, human),
                            fmt_usd(c.cost.cache_read_usd, human),
                            fmt_usd(c.cost.cache_saved_usd, human),
                            format!("{:.1}%", c.cache_hit_rate()),
                            fmt_num(c.total_tokens(), human),
                        ]);
                    } else {
                        cells.extend([
                            m.clone(),
                            c.calls.to_string(),
                            fmt_num(c.total_tokens(), human),
                            fmt_num(c.input_tokens + c.cache_write_tokens, human),
                            fmt_num(c.output_tokens, human),
                            fmt_num(c.cache_read_tokens, human),
                            format!("{:.1}%", c.cache_hit_rate()),
                            fmt_usd(c.cost.cache_saved_usd, human),
                            fmt_cost(&c.cost, c.calls, human),
                        ]);
                    }
                    None
                });
            }
        }
    }

    // ---------- Tab 4: Per-Model Cache Hit Rate Comparison & Full Breakdown ----------

    fn draw_model_view_toggle(
        &mut self,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        card_left: f32,
        card_top: f32,
    ) -> f32 {
        let mut gx = card_left + 14.0;
        for (mode, lbl, gw) in [
            (ModelViewMode::CacheCompare, "缓存对比", 64.0),
            (ModelViewMode::Pricing, "单价表 ($)", 72.0),
            (ModelViewMode::Budget, "预算 / 桥接", 78.0),
        ] {
            let r = rect_f(gx, card_top + 9.0, gw, 22.0);
            let act = self.model_view == mode;
            let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&r, mx, my)).unwrap_or(false);
            if act {
                fill_round_rect(rt, r, 6.0, theme.hover);
            } else if hov {
                fill_round_rect(rt, r, 6.0, with_alpha(theme.text, 0.04));
            }
            draw_text(
                rt,
                lbl,
                &fonts.ui_small_center,
                r,
                if act { theme.text } else if hov { theme.sub } else { theme.muted },
            );
            self.hit_boxes.push(HitBox {
                rect: r,
                action: ClickAction::SelectModelView(mode),
            });
            gx += gw + 4.0;
        }
        gx
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_models_tab(
        &mut self,
        factories: &D2DFactories,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        body: D2D_RECT_F,
        snapshot: &ScanSnapshot,
        human: bool,
    ) {
        if self.model_view == ModelViewMode::Pricing {
            self.draw_pricing_subview(rt, fonts, theme, body, snapshot);
            return;
        }
        if self.model_view == ModelViewMode::Budget {
            self.draw_budget_subview(rt, fonts, theme, body, snapshot, human);
            return;
        }

        let bw = body.right - body.left;
        let bh = body.bottom - body.top;
        let chart_h = (bh * 0.45).clamp(176.0, 245.0);
        let chart_c = rect_f(body.left, body.top, bw, chart_h);
        fill_round_rect(rt, chart_c, 10.0, theme.surface);
        stroke_round_rect(rt, chart_c, 10.0, theme.border, 1.0);

        let toggle_end = self.draw_model_view_toggle(rt, fonts, theme, chart_c.left, chart_c.top);
        if self.scope == DateScope::Today {
            let hint_r = rect_f(toggle_end + 6.0, chart_c.top + 9.0, 108.0, 22.0);
            self.draw_ghost_btn(
                rt,
                fonts,
                theme,
                hint_r,
                "仅今日 · 展开全部",
                true,
                ClickAction::SelectScope(DateScope::All),
            );
        }

        // Right header sort controls
        let sorts = [
            (ModelSort::TotalDesc, "按总量", 50.0),
            (ModelSort::CostDesc, "按费用", 50.0),
            (ModelSort::HitRateAsc, "命中低→高", 68.0),
            (ModelSort::HitRateDesc, "命中高→低", 68.0),
        ];
        let mut sx = chart_c.right - 14.0 - (50.0 + 50.0 + 68.0 + 68.0 + 9.0);
        for (sort_k, lbl, sw) in sorts {
            let r = rect_f(sx, chart_c.top + 9.0, sw, 22.0);
            let act = self.model_sort == sort_k;
            let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&r, mx, my)).unwrap_or(false);
            if act {
                fill_round_rect(rt, r, 6.0, theme.hover);
            } else if hov {
                fill_round_rect(rt, r, 6.0, with_alpha(theme.text, 0.04));
            }
            draw_text(
                rt,
                lbl,
                &fonts.ui_small_center,
                r,
                if act { theme.text } else if hov { theme.sub } else { theme.muted },
            );
            self.hit_boxes.push(HitBox {
                rect: r,
                action: ClickAction::SelectModelSort(sort_k),
            });
            sx += sw + 3.0;
        }

        let (primary, secondary) = Self::ordered_models_of(&self.cached_agg.by_model, self.model_sort);
        let plot = rect_f(chart_c.left + 14.0, chart_c.top + 38.0, bw - 28.0, chart_h - 46.0);

        if primary.is_empty() && secondary.is_empty() {
            draw_text(
                rt,
                "当前范围暂无模型用量 · 可切换右上角 7天 / 30天 / 全部",
                &fonts.ui_reg_center,
                plot,
                theme.muted,
            );
        } else {
            let plot_w = plot.right - plot.left;
            let axis_h = 18.0;
            let rows_top = plot.top + 4.0;
            let rows_bot = (plot.bottom - axis_h).max(rows_top + 28.0);
            let max_chart_rows = (((rows_bot - rows_top) / 26.0).floor() as usize).clamp(2, 6);
            let show_sec_in_chart = !secondary.is_empty() && (primary.is_empty() || max_chart_rows >= 2);
            let max_pri = if show_sec_in_chart { max_chart_rows - 1 } else { max_chart_rows };
            let mut chart_items: Vec<(&str, TokenCounts, bool)> = primary
                .iter()
                .take(max_pri)
                .map(|&(n, c)| (n, c, false))
                .collect();
            if show_sec_in_chart {
                if let Some(&(n, c)) = secondary.first() {
                    chart_items.push((n, c, true));
                }
            }

            let max_name_px = chart_items
                .iter()
                .map(|(n, _, _)| factories.measure_text_width(n, &fonts.mono_small))
                .fold(140.0_f32, f32::max);
            let label_w = (max_name_px + 10.0).clamp(146.0, (plot_w * 0.34).max(232.0));
            let right_w = 182.0_f32.min(plot_w * 0.28);
            let track_l = plot.left + label_w + 10.0;
            let track_r = (plot.right - right_w - 10.0).max(track_l + 80.0);
            let track_w = track_r - track_l;

            // Vertical percentage gridlines + 85% healthy guide
            let mut fbuf = FmtBuf::new();
            for pct in [0, 25, 50, 75, 100] {
                let gx = track_l + (pct as f32 / 100.0) * track_w;
                let col = if pct == 0 || pct == 100 { theme.border } else { theme.grid };
                draw_line(rt, gx, rows_top, gx, rows_bot, col, 1.0);
                if pct == 0 || pct == 50 || pct == 100 {
                    fbuf.clear();
                    let _ = write!(fbuf, "{pct}%");
                    draw_text(
                        rt,
                        fbuf.as_str(),
                        &fonts.mono_small_center,
                        rect_f(gx - 20.0, rows_bot + 2.0, 40.0, 14.0),
                        theme.muted,
                    );
                }
            }
            let gx85 = track_l + 0.85 * track_w;
            draw_line(rt, gx85, rows_top, gx85, rows_bot, with_alpha(theme.sage, 0.30), 1.0);
            draw_text(
                rt,
                "85%",
                &fonts.mono_small_center,
                rect_f(gx85 - 16.0, rows_bot + 2.0, 32.0, 14.0),
                with_alpha(theme.sage, 0.85),
            );

            let n_rows = chart_items.len().max(1);
            let row_h = ((rows_bot - rows_top) / n_rows as f32).clamp(24.0, 36.0);
            let mut hovered_tip: Option<(f32, f32, Vec<String>)> = None;
            let mut row_rects = Vec::with_capacity(chart_items.len());

            for (i, &(name, c, is_sec)) in chart_items.iter().enumerate() {
                let ry = rows_top + i as f32 * row_h;
                if is_sec && i > 0 {
                    draw_line(rt, plot.left, ry - 1.5, plot.right, ry - 1.5, theme.border, 0.9);
                }
                let rr = rect_f(plot.left - 4.0, ry + 1.0, (plot.right - plot.left) + 8.0, row_h - 3.0);
                row_rects.push(rr);
                let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&rr, mx, my)).unwrap_or(false);
                if hov {
                    fill_round_rect(rt, rr, 6.0, with_alpha(theme.text, 0.04));
                }

                let hit = c.cache_hit_rate();
                let col = if is_sec { theme.cache } else { cache_hit_color(theme, &c) };
                let name_col = if hov {
                    theme.accent
                } else if is_sec {
                    theme.muted
                } else {
                    theme.text
                };
                draw_text(rt, name, &fonts.mono_small, rect_f(plot.left, ry, label_w, row_h), name_col);

                let bar_h = (row_h * 0.34).clamp(7.0, 10.0);
                let bar_y = ry + (row_h - bar_h) * 0.5;
                fill_round_rect(rt, rect_f(track_l, bar_y, track_w, bar_h), bar_h * 0.5, theme.grid);
                let fill_w = ((hit as f32 / 100.0).clamp(0.0, 1.0) * track_w).max(if c.cache_read_tokens > 0 { 4.0 } else { 0.0 });
                if fill_w > 0.0 {
                    fill_round_rect(rt, rect_f(track_l, bar_y, fill_w, bar_h), bar_h * 0.5, col);
                }

                fbuf.clear();
                let _ = write!(fbuf, "{:.1}%", hit);
                let meta_txt = if is_sec {
                    format!("{}次 · {}", c.calls, fmt_cost(&c.cost, c.calls, true))
                } else if self.cost_mode {
                    format!("{}次 · {}", c.calls, fmt_cost(&c.cost, c.calls, human))
                } else {
                    format!("{}次 · {}", c.calls, fmt_num(c.total_tokens(), human))
                };
                draw_text(
                    rt,
                    fbuf.as_str(),
                    if is_sec { &fonts.mono_small_right } else { &fonts.mono_med_right },
                    rect_f(track_r + 8.0, ry, 50.0, row_h),
                    if is_sec { theme.muted } else { col },
                );
                draw_text(
                    rt,
                    &meta_txt,
                    &fonts.mono_small_right,
                    rect_f(track_r + 62.0, ry, (plot.right - (track_r + 62.0)).max(50.0), row_h),
                    if self.cost_mode && !is_sec { theme.output } else { theme.muted },
                );

                self.hit_boxes.push(HitBox {
                    rect: rr,
                    action: ClickAction::FilterSessions(SessionFilter::Model(name.to_string())),
                });

                if hov {
                    let mcs = self.cached_agg.by_model_cache.get(name);
                    let mt_str = mcs
                        .and_then(ModelCacheStats::multi_turn_hit_rate)
                        .map(|r| format!("{r:.1}% (第2轮起 {} 次)", mcs.map(|m| m.multi_turn_calls).unwrap_or(0)))
                        .unwrap_or_else(|| "-".into());
                    let mut tip = vec![
                        if is_sec {
                            format!("{} · {} 次调用 (按会话主模型计价)", name, c.calls)
                        } else {
                            format!("{} · {} 次调用 (点击筛选会话)", name, c.calls)
                        },
                    ];
                    if let Some(rate_lbl) = snapshot.model_rate_labels.get(name) {
                        tip.push(format!("API单价  : {}", rate_lbl));
                    }
                    tip.push(format!(
                        "估算费用 : {} (入{} / 缓{} / 出{})",
                        fmt_cost(&c.cost, c.calls, false),
                        fmt_usd_compact(c.cost.input_plus_write_usd()),
                        fmt_usd_compact(c.cost.cache_read_usd),
                        fmt_usd_compact(c.cost.output_usd),
                    ));
                    if c.cost.cache_saved_usd > 0.0 {
                        tip.push(format!("缓存已省 : {}", fmt_usd_precise(c.cost.cache_saved_usd)));
                    }
                    tip.push(format!("总命中率 : {:.1}%  |  稳定命中: {}", hit, mt_str));
                    if !is_sec {
                        let (mc, mu) = mcs.map(|m| (m.miss_count, m.miss_uncached_tokens)).unwrap_or((0, 0));
                        tip.push(if mc > 0 {
                            format!("缓存未命 : {} 次 (重传 {})", mc, fmt_commas(mu))
                        } else {
                            "缓存未命 : 0 次".to_string()
                        });
                    }
                    tip.push(format!("新增输入 : {}  |  缓存读 : {}", fmt_commas(c.input_tokens), fmt_commas(c.cache_read_tokens)));
                    tip.push(format!("输出     : {}  |  总计   : {}", fmt_commas(c.output_tokens), fmt_commas(c.total_tokens())));
                    tip.push(format!("峰值Ctx  : {}", fmt_commas(c.peak_context)));
                    // Anchor tooltip deterministically to bar geometry (enables discrete row hover gating)
                    let anchor_x = track_l + (fill_w * 0.65).clamp(16.0, (track_w - 20.0).max(16.0));
                    hovered_tip = Some((anchor_x, ry, tip));
                }
            }
            self.discrete_plots.push(DiscretePlot::RowRects {
                bounds: rect_f(plot.left - 4.0, rows_top, plot_w + 8.0, rows_bot - rows_top),
                rows: row_rects,
            });

            if let Some((anchor_x, ry, lines)) = hovered_tip {
                let max_units = lines
                    .iter()
                    .map(|l| {
                        l.chars()
                            .map(|ch| if ch.is_ascii() { 1.0_f32 } else { 1.82_f32 })
                            .sum::<f32>()
                    })
                    .fold(0.0_f32, f32::max);
                let tw = (max_units * 6.4 + 24.0).clamp(240.0, (plot.right - plot.left - 16.0).max(240.0));
                let th = lines.len() as f32 * 16.0 + 14.0;
                let tx = (anchor_x + 12.0).min(plot.right - tw - 4.0).max(plot.left + 4.0);
                let ty = (ry - 4.0).clamp(plot.top, (plot.bottom - th).max(plot.top));
                let tr = rect_f(tx, ty, tw, th);
                fill_round_rect(rt, tr, 8.0, theme.tooltip_bg);
                stroke_round_rect(rt, tr, 8.0, theme.border, 1.0);
                for (li, line) in lines.iter().enumerate() {
                    draw_text(
                        rt,
                        line,
                        &fonts.mono_small,
                        rect_f(tx + 10.0, ty + 7.0 + li as f32 * 16.0, tw - 20.0, 16.0),
                        if li == 0 { theme.text } else { theme.sub },
                    );
                }
            }
        }

        // Bottom full-width breakdown table (compaction pinned to bottom secondary rows)
        let tbl_r = rect_f(body.left, chart_c.bottom + 12.0, bw, (body.bottom - chart_c.bottom - 12.0).max(110.0));
        let pri_len = primary.len();
        let total_models = pri_len + secondary.len();
        let secondary_from = if secondary.is_empty() { None } else { Some(pri_len) };

        let cols: &[(&str, f32, bool)] = if self.cost_mode {
            &[
                ("模型", 0.24, false),
                ("调用", 0.05, true),
                ("估算总费", 0.095, true),
                ("新入费用", 0.08, true),
                ("输出费用", 0.08, true),
                ("缓存费用", 0.075, true),
                ("缓存已省", 0.08, true),
                ("命中率", 0.07, true),
                ("稳定命中", 0.07, true),
                ("输出速度", 0.07, true),
                ("总 Token", 0.09, true),
            ]
        } else {
            &[
                ("模型", 0.245, false),
                ("调用", 0.05, true),
                ("总 Token", 0.10, true),
                ("新输入", 0.085, true),
                ("输出", 0.08, true),
                ("命中率", 0.07, true),
                ("稳定命中", 0.075, true),
                ("未命中", 0.06, true),
                ("输出速度", 0.07, true),
                ("峰值Ctx", 0.075, true),
                ("估算费用", 0.09, true),
            ]
        };
        let cost_mode = self.cost_mode;
        let by_model_cache = &self.cached_agg.by_model_cache;
        Self::draw_table_virtual_state(
            rt,
            fonts,
            theme,
            tbl_r,
            cols,
            total_models,
            secondary_from,
            None,
            &mut self.scroll_rows,
            &mut self.max_scroll,
            &mut self.hit_boxes,
            self.mouse_dip,
            cost_mode,
            |abs_idx, cells| {
                let (name, c, is_sec) = if abs_idx < pri_len {
                    let &(n, c) = &primary[abs_idx];
                    (n, c, false)
                } else {
                    let &(n, c) = &secondary[abs_idx - pri_len];
                    (n, c, true)
                };
                let mcs = by_model_cache.get(name);
                let speed_str = mcs
                    .and_then(ModelCacheStats::output_tps)
                    .map(|v| format!("{v:.0}/s"))
                    .unwrap_or_else(|| "-".into());
                let mt_str = if is_sec {
                    "-".to_string()
                } else {
                    mcs.and_then(ModelCacheStats::multi_turn_hit_rate)
                        .map(|r| format!("{r:.1}%"))
                        .unwrap_or_else(|| "-".into())
                };
                let miss_str = if is_sec {
                    "-".to_string()
                } else {
                    match mcs.map(|m| m.miss_count).unwrap_or(0) {
                        0 => "-".to_string(),
                        n => format!("{}次", n),
                    }
                };
                if cost_mode {
                    cells.extend([
                        name.to_string(),
                        c.calls.to_string(),
                        fmt_cost(&c.cost, c.calls, human),
                        fmt_usd(c.cost.input_plus_write_usd(), human),
                        fmt_usd(c.cost.output_usd, human),
                        fmt_usd(c.cost.cache_read_usd, human),
                        fmt_usd(c.cost.cache_saved_usd, human),
                        format!("{:.1}%", c.cache_hit_rate()),
                        mt_str,
                        speed_str,
                        fmt_num(c.total_tokens(), human),
                    ]);
                } else {
                    cells.extend([
                        name.to_string(),
                        c.calls.to_string(),
                        fmt_num(c.total_tokens(), human),
                        fmt_num(c.input_tokens + c.cache_write_tokens, human),
                        fmt_num(c.output_tokens, human),
                        format!("{:.1}%", c.cache_hit_rate()),
                        mt_str,
                        miss_str,
                        speed_str,
                        fmt_num(c.peak_context, human),
                        fmt_cost(&c.cost, c.calls, human),
                    ]);
                }
                Some(ClickAction::FilterSessions(SessionFilter::Model(name.to_string())))
            },
        );
    }

    fn draw_pricing_subview(
        &mut self,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        body: D2D_RECT_F,
        snapshot: &ScanSnapshot,
    ) {
        let bw = body.right - body.left;
        let edit_h = 138.0;
        let edit_c = rect_f(body.left, body.top, bw, edit_h);
        fill_round_rect(rt, edit_c, 10.0, theme.surface);
        stroke_round_rect(rt, edit_c, 10.0, theme.border, 1.0);

        let toggle_end = self.draw_model_view_toggle(rt, fonts, theme, edit_c.left, edit_c.top);

        // Right "+ 新增模型" button in top bar
        let new_r = rect_f(edit_c.right - 90.0, edit_c.top + 9.0, 76.0, 22.0);
        self.draw_ghost_btn(rt, fonts, theme, new_r, "+ 新增模型", false, ClickAction::NewPricingEntry);

        // Inline status banner (validation error / save confirmation / quiet help)
        let active_status = self
            .pricing_status
            .as_ref()
            .filter(|(_, _, until)| Instant::now() < *until)
            .map(|(m, err, _)| (m.as_str(), *err));
        let (st_text, st_col) = match active_status {
            Some((msg, true)) => (msg.to_string(), theme.danger),
            Some((msg, false)) => (msg.to_string(), theme.sage),
            None => (
                "点击下方任意模型即可载入编辑 · 保存立即重算全盘费用".to_string(),
                theme.muted,
            ),
        };
        let st_l = toggle_end + 10.0;
        let st_w = (new_r.left - 10.0 - st_l).max(80.0);
        draw_text(rt, &st_text, &fonts.ui_small, rect_f(st_l, edit_c.top + 10.0, st_w, 20.0), st_col);

        // 5 input boxes + right Save / Reset buttons
        let inner_l = edit_c.left + 16.0;
        let inner_r = edit_c.right - 16.0;
        let btn_save_w = 56.0;
        let btn_reset_w = 68.0;
        let btns_w = btn_save_w + 6.0 + btn_reset_w;
        let fields_r = inner_r - btns_w - 12.0;
        let fields_w = (fields_r - inner_l).max(260.0);
        let gap = 8.0;
        let net_w = (fields_w - gap * 4.0).max(220.0);
        let fracs = [0.36_f32, 0.16, 0.16, 0.16, 0.16];
        let labels = [
            "模型名 / 前缀",
            "输入 ($/1M)",
            "缓存读 ($/1M)",
            "缓存写 (空=入)",
            "输出 ($/1M)",
        ];

        let lbl_y = edit_c.top + 38.0;
        let box_y = edit_c.top + 53.0;
        let box_h = 28.0;
        let input_bg = if theme.is_dark {
            crate::d2d::rgb(20, 20, 22)
        } else {
            crate::d2d::rgb(246, 244, 241)
        };

        let mut fx = inner_l;
        let mut child_rects = [rect_f(0.0, 0.0, 10.0, 10.0); 5];
        for i in 0..5 {
            let fw = (net_w * fracs[i]).floor();
            draw_text(rt, labels[i], &fonts.ui_micro, rect_f(fx + 2.0, lbl_y, fw, 14.0), theme.muted);
            let outer = rect_f(fx, box_y, fw, box_h);
            fill_round_rect(rt, outer, 6.0, input_bg);
            let is_foc = self.pricing_focused_field == Some(i);
            stroke_round_rect(
                rt,
                outer,
                6.0,
                if is_foc { theme.accent } else { theme.border },
                if is_foc { 1.3 } else { 1.0 },
            );
            child_rects[i] = rect_f(fx + 7.0, box_y + 5.0, (fw - 14.0).max(16.0), (box_h - 9.0).max(14.0));
            fx += fw + gap;
        }
        self.pricing_field_rects = Some(child_rects);

        // Save button (primary accent)
        let r_save = rect_f(inner_r - btns_w, box_y, btn_save_w, box_h);
        let save_hov = self.mouse_dip.map(|(mx, my)| hit_rect(&r_save, mx, my)).unwrap_or(false);
        fill_round_rect(
            rt,
            r_save,
            6.0,
            if save_hov { theme.accent } else { theme.accent_soft },
        );
        stroke_round_rect(rt, r_save, 6.0, theme.accent, 1.0);
        draw_text(
            rt,
            "保存",
            &fonts.ui_small_center,
            r_save,
            if save_hov { theme.bg } else { theme.accent },
        );
        self.hit_boxes.push(HitBox {
            rect: r_save,
            action: ClickAction::SavePricingEntry,
        });

        // Reset / Delete custom override button
        let r_reset = rect_f(inner_r - btn_reset_w, box_y, btn_reset_w, box_h);
        let can_reset = self.pricing_editing_custom_key.is_some();
        let reset_hov = can_reset && self.mouse_dip.map(|(mx, my)| hit_rect(&r_reset, mx, my)).unwrap_or(false);
        fill_round_rect(
            rt,
            r_reset,
            6.0,
            if reset_hov { theme.hover } else { theme.surface_subtle },
        );
        stroke_round_rect(rt, r_reset, 6.0, theme.border, 1.0);
        draw_text(
            rt,
            "恢复默认",
            &fonts.ui_small_center,
            r_reset,
            if !can_reset { theme.muted } else if reset_hov { theme.text } else { theme.sub },
        );
        if can_reset {
            self.hit_boxes.push(HitBox {
                rect: r_reset,
                action: ClickAction::ResetPricingEntry,
            });
        }

        // Bottom metadata hints
        let path_hint = if self.privacy_mode {
            redact_pricing_path(snapshot.pricing_path.as_deref())
        } else {
            snapshot
                .pricing_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "pricing.json".to_string())
        };
        let line1 = "优先级：精确模型名 > 前缀匹配（如 gemini-4-neon 匹配全档位） > 内置默认  ·  Tab 切框 / Enter 保存";
        let line2 = format!(
            "共 {} 条单价 · 已自定义 {} 项 · 写入本地 {}（对所有 Agent 日志目录保持绝对只读）",
            snapshot.pricing_rows.len(),
            snapshot.custom_pricing_count,
            path_hint,
        );
        draw_text(rt, line1, &fonts.ui_micro, rect_f(inner_l, edit_c.top + 89.0, inner_r - inner_l, 15.0), theme.sub);
        draw_text(rt, &line2, &fonts.ui_micro, rect_f(inner_l, edit_c.top + 109.0, inner_r - inner_l, 15.0), theme.muted);

        // Full-width scrollable API pricing table (virtualized row formatting)
        let tbl_r = rect_f(body.left, edit_c.bottom + 12.0, bw, (body.bottom - edit_c.bottom - 12.0).max(110.0));
        let secondary_from = snapshot.pricing_rows.iter().position(|r| r.is_reference);
        let selected_idx = self.pricing_selected_model.as_deref().and_then(|sel| {
            snapshot
                .pricing_rows
                .iter()
                .position(|r| r.model.eq_ignore_ascii_case(sel))
        });

        self.draw_table_virtual(
            rt,
            fonts,
            theme,
            tbl_r,
            &[
                ("模型名 / 规则", 0.29, false),
                ("来源", 0.11, false),
                ("匹配规则 / 说明", 0.21, false),
                ("输入 $/M", 0.0975, true),
                ("缓存读 $/M", 0.0975, true),
                ("缓存写 $/M", 0.0975, true),
                ("输出 $/M", 0.0975, true),
            ],
            snapshot.pricing_rows.len(),
            secondary_from,
            selected_idx,
            |abs_idx, cells| {
                let r = &snapshot.pricing_rows[abs_idx];
                let (in_s, cr_s, cw_s, out_s) = match r.rate {
                    Some(rate) => (
                        format!("${}", fmt_rate_plain(rate.input)),
                        format!("${}", fmt_rate_plain(rate.cache_read)),
                        format!("${}", fmt_rate_plain(rate.cache_write)),
                        format!("${}", fmt_rate_plain(rate.output)),
                    ),
                    None => ("-".into(), "-".into(), "-".into(), "-".into()),
                };
                let rule_col = if !r.note.is_empty() && r.source == PriceSource::Builtin {
                    format!("{} ({})", r.matched_rule, r.note)
                } else if !r.note.is_empty() && r.source == PriceSource::Unpriced {
                    r.note.clone()
                } else {
                    r.matched_rule.clone()
                };
                cells.extend([
                    r.model.clone(),
                    r.source.label_cn().to_string(),
                    rule_col,
                    in_s,
                    cr_s,
                    cw_s,
                    out_s,
                ]);
                Some(ClickAction::PickPricingRow(abs_idx))
            },
        );
    }

    fn draw_budget_subview(
        &mut self,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        body: D2D_RECT_F,
        snapshot: &ScanSnapshot,
        human: bool,
    ) {
        let bw = body.right - body.left;
        let edit_h = 136.0;
        let edit_c = rect_f(body.left, body.top, bw, edit_h);
        fill_round_rect(rt, edit_c, 10.0, theme.surface);
        stroke_round_rect(rt, edit_c, 10.0, theme.border, 1.0);

        let toggle_end = self.draw_model_view_toggle(rt, fonts, theme, edit_c.left, edit_c.top);

        let copy_sl_r = rect_f(edit_c.right - 140.0, edit_c.top + 9.0, 126.0, 22.0);
        self.draw_ghost_btn(rt, fonts, theme, copy_sl_r, "复制 /statusline 配置", true, ClickAction::CopyStatuslineConfig);

        let active_status = self
            .pricing_status
            .as_ref()
            .filter(|(_, _, until)| Instant::now() < *until)
            .map(|(m, err, _)| (m.as_str(), *err));
        let (st_text, st_col) = match active_status {
            Some((msg, true)) => (msg.to_string(), theme.danger),
            Some((msg, false)) => (msg.to_string(), theme.sage),
            None => (
                "每日/每周美元预算汇总全部已发现 Agent · 留空或填 0 不限，超阈值自动提醒".to_string(),
                theme.muted,
            ),
        };
        let st_l = toggle_end + 10.0;
        let st_w = (copy_sl_r.left - 10.0 - st_l).max(80.0);
        draw_text(rt, &st_text, &fonts.ui_small, rect_f(st_l, edit_c.top + 10.0, st_w, 20.0), st_col);

        let inner_l = edit_c.left + 16.0;
        let inner_r = edit_c.right - 16.0;
        let btn_save_w = 56.0;
        let btn_clear_w = 68.0;
        let btns_w = btn_save_w + 6.0 + btn_clear_w;
        let fields_r = inner_r - btns_w - 12.0;
        let fields_w = (fields_r - inner_l).max(260.0);
        let gap = 8.0;
        let net_w = (fields_w - gap * 3.0).max(220.0);
        let fracs = [0.28_f32, 0.28, 0.22, 0.22];
        let labels = [
            "每日预算 ($)",
            "每周预算 ($)",
            "预警比例 (%)",
            "临界比例 (%)",
        ];

        let lbl_y = edit_c.top + 38.0;
        let box_y = edit_c.top + 53.0;
        let box_h = 28.0;
        let input_bg = if theme.is_dark {
            crate::d2d::rgb(20, 20, 22)
        } else {
            crate::d2d::rgb(246, 244, 241)
        };

        let mut fx = inner_l;
        let mut child_rects = [rect_f(0.0, 0.0, 10.0, 10.0); 4];
        for i in 0..4 {
            let fw = (net_w * fracs[i]).floor();
            draw_text(rt, labels[i], &fonts.ui_micro, rect_f(fx + 2.0, lbl_y, fw, 14.0), theme.muted);
            let outer = rect_f(fx, box_y, fw, box_h);
            fill_round_rect(rt, outer, 6.0, input_bg);
            let is_foc = self.pricing_focused_field == Some(i);
            stroke_round_rect(
                rt,
                outer,
                6.0,
                if is_foc { theme.accent } else { theme.border },
                if is_foc { 1.3 } else { 1.0 },
            );
            child_rects[i] = rect_f(fx + 7.0, box_y + 5.0, (fw - 14.0).max(16.0), (box_h - 9.0).max(14.0));
            fx += fw + gap;
        }
        self.budget_field_rects = Some(child_rects);

        let r_save = rect_f(inner_r - btns_w, box_y, btn_save_w, box_h);
        let save_hov = self.mouse_dip.map(|(mx, my)| hit_rect(&r_save, mx, my)).unwrap_or(false);
        fill_round_rect(rt, r_save, 6.0, if save_hov { theme.accent } else { theme.accent_soft });
        stroke_round_rect(rt, r_save, 6.0, theme.accent, 1.0);
        draw_text(rt, "保存", &fonts.ui_small_center, r_save, if save_hov { theme.bg } else { theme.accent });
        self.hit_boxes.push(HitBox { rect: r_save, action: ClickAction::SaveBudgetEntry });

        let r_clear = rect_f(inner_r - btn_clear_w, box_y, btn_clear_w, box_h);
        let clear_hov = self.mouse_dip.map(|(mx, my)| hit_rect(&r_clear, mx, my)).unwrap_or(false);
        fill_round_rect(rt, r_clear, 6.0, if clear_hov { theme.hover } else { theme.surface_subtle });
        stroke_round_rect(rt, r_clear, 6.0, theme.border, 1.0);
        draw_text(rt, "清空预算", &fonts.ui_small_center, r_clear, if clear_hov { theme.text } else { theme.sub });
        self.hit_boxes.push(HitBox { rect: r_clear, action: ClickAction::ClearBudgetEntry });

        let now_utc = Utc::now();
        let sl_summary = match snapshot.statusline.as_ref() {
            Some(sl) => {
                let prov = sl.provenance(now_utc);
                let age = fmt_duration_short(sl.age_secs(now_utc).max(0) as u64);
                let b_txt: Vec<String> = sl
                    .buckets
                    .iter()
                    .filter(|b| !b.disabled)
                    .take(3)
                    .filter_map(|b| b.remaining_pct().map(|p| format!("{} {:.0}%", b.label, p)))
                    .collect();
                format!(
                    "Antigravity CLI /statusline 桥接（仅 agy 官方配额）：{}（{}前更新） · {}",
                    prov.label_cn(),
                    age,
                    if b_txt.is_empty() { "尚无配额池字段".into() } else { b_txt.join(" · ") }
                )
            }
            None => "Antigravity CLI /statusline 桥接（仅 agy 官方配额）：未接入 · 粘入 settings.json 即可同步真实余量".into(),
        };
        let bpath = if self.privacy_mode {
            "./budget.json".to_string()
        } else {
            snapshot
                .budget_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "budget.json".to_string())
        };
        draw_text(rt, &sl_summary, &fonts.ui_micro, rect_f(inner_l, edit_c.top + 90.0, inner_r - inner_l, 15.0), theme.sub);
        draw_text(
            rt,
            &format!(
                "每日/每周美元预算汇总全部 Agent（保存至 {}）· agy 配额提醒 ≤{:.0}% / ≤{:.0}% · Tab 切框 / Enter 保存",
                bpath, snapshot.budget.quota_warn_rem_pct, snapshot.budget.quota_crit_rem_pct
            ),
            &fonts.ui_micro,
            rect_f(inner_l, edit_c.top + 110.0, inner_r - inner_l, 15.0),
            theme.muted,
        );

        let tbl_r = rect_f(body.left, edit_c.bottom + 12.0, bw, (body.bottom - edit_c.bottom - 12.0).max(110.0));
        let mut rows: Vec<Vec<String>> = Vec::new();

        // 1) Official /statusline quota buckets (when bridged)
        if let Some(sl) = snapshot.statusline.as_ref() {
            let prov = sl.provenance(now_utc);
            for b in sl.buckets.iter().filter(|b| !b.disabled) {
                let rem_s = b
                    .remaining_pct()
                    .map(|p| format!("剩 {:.0}%", p))
                    .unwrap_or_else(|| "-".into());
                let used_s = b
                    .used_pct()
                    .map(|p| format!("已用 {:.0}%", p))
                    .unwrap_or_else(|| "-".into());
                let reset_s = b
                    .live_remaining_secs(sl.captured_at_utc, now_utc)
                    .map(|s| format!("{}后重置", fmt_countdown(s)))
                    .unwrap_or_else(|| "-".into());
                let st_s = match b.remaining_pct().map(|p| snapshot.budget.classify_quota_remaining(p)) {
                    Some(AlertSeverity::Critical) => "临界",
                    Some(AlertSeverity::Warn) => "预警",
                    Some(AlertSeverity::Normal) => "正常",
                    None => "-",
                };
                rows.push(vec![
                    b.label.clone(),
                    prov.label_cn().to_string(),
                    rem_s,
                    used_s,
                    reset_s,
                    st_s.to_string(),
                ]);
            }
        }

        // 2) Daily & Weekly USD budget utilization (always visible so user can benchmark caps against real spend)
        let today_date = Local::now().date_naive();
        let today_key = today_date.format("%Y-%m-%d").to_string();
        let week_key = iso_week_key(today_date);
        let today_c = snapshot.agg.by_day.get(&today_key).copied().unwrap_or_default();
        let week_c = snapshot.agg.by_week.get(&week_key).copied().unwrap_or_default();

        for (label, period, counts, cap) in [
            ("今日预算", today_key.clone(), today_c, snapshot.budget.daily_usd),
            ("本周预算", week_key.clone(), week_c, snapshot.budget.weekly_usd),
        ] {
            let spent = counts.cost.total_usd();
            let spent_s = format!(
                "{} ({})",
                fmt_cost(&counts.cost, counts.calls, human),
                fmt_num(counts.total_tokens(), human)
            );
            let (cap_s, pct_s, st_s) = if cap > 0.0 {
                let pct = (spent / cap) * 100.0;
                let sev = snapshot.budget.classify_usage_pct(pct);
                let st = match sev {
                    AlertSeverity::Critical => "临界",
                    AlertSeverity::Warn => "预警",
                    AlertSeverity::Normal => "正常",
                };
                (format!("${:.2}", cap), format!("{:.1}%", pct), st)
            } else {
                ("未限制".to_string(), "-".to_string(), "未设限")
            };
            rows.push(vec![
                format!("{label} ({period})"),
                "本地汇总 (全源)".to_string(),
                spent_s,
                cap_s,
                pct_s,
                st_s.to_string(),
            ]);
        }

        self.draw_table_virtual(
            rt,
            fonts,
            theme,
            tbl_r,
            &[
                ("配额 / 预算项", 0.28, false),
                ("来源", 0.14, false),
                ("当前状态 / 消耗", 0.22, true),
                ("已用 / 上限", 0.14, true),
                ("重置 / 进度", 0.13, true),
                ("状态", 0.09, true),
            ],
            rows.len(),
            None,
            None,
            |i, cells| {
                cells.extend(std::mem::take(&mut rows[i]));
                None
            },
        );
    }

    // ---------- Tab 4: Session Explorer + Per-Session Cache Hit Rate & Turn Anatomy ----------

    #[allow(clippy::too_many_arguments)]
    fn draw_sessions_tab(
        &mut self,
        factories: &D2DFactories,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        body: D2D_RECT_F,
        snapshot: &ScanSnapshot,
        human: bool,
    ) {
        let bw = body.right - body.left;
        let bh = body.bottom - body.top;
        let top_h = (bh * 0.48).clamp(160.0, 265.0);
        let top_c = rect_f(body.left, body.top, bw, top_h);
        fill_round_rect(rt, top_c, 10.0, theme.surface);
        stroke_round_rect(rt, top_c, 10.0, theme.border, 1.0);

        let (sorted_len, auto_sel) = self.with_display_sessions_for_tab(|sorted| {
            let keep = self
                .selected_session
                .as_deref()
                .is_some_and(|id| sorted.iter().any(|r| self.row_session(r).conversation_id == id));
            let next = if keep {
                None
            } else {
                Some(sorted.first().map(|r| self.row_session(r).conversation_id.clone()))
            };
            (sorted.len(), next)
        });
        if let Some(new_sel) = auto_sel {
            self.selected_session = new_sel;
        }
        let (root_total, sub_total) = self.session_split_counts();
        let pinned_total = self.pinned_ids.len();
        let now_local = Local::now();

        draw_text(rt, "会话记录", &fonts.ui_section, rect_f(top_c.left + 16.0, top_c.top + 9.0, 66.0, 20.0), theme.text);
        let cnt_str = if self.session_filter.is_some() || self.pinned_only {
            format!("{} 条", sorted_len)
        } else if self.group_subagents && sub_total > 0 {
            format!("{} 主 (+{}子)", root_total, sub_total)
        } else if sub_total > 0 {
            format!("{} 个 ({}子)", sorted_len, sub_total)
        } else {
            format!("{} 个", sorted_len)
        };
        let cnt_w = (factories.measure_text_width(&cnt_str, &fonts.ui_micro) + 6.0).clamp(42.0, 102.0);
        draw_text(
            rt,
            &cnt_str,
            &fonts.ui_micro,
            rect_f(top_c.left + 80.0, top_c.top + 11.0, cnt_w, 16.0),
            theme.muted,
        );

        let sorts = [
            (SessionSort::Recent, "最近", 38.0),
            (SessionSort::Cost, "费用", 38.0),
            (SessionSort::TotalTokens, "总Token", 54.0),
            (SessionSort::FreshInput, "新输入", 46.0),
            (SessionSort::Output, "输出", 38.0),
            (SessionSort::HitRateLow, "命中低→高", 64.0),
        ];
        let mut sx = top_c.right - 14.0;
        for (so, lbl, sw) in sorts.iter().rev() {
            sx -= *sw + 4.0;
            let r = rect_f(sx, top_c.top + 8.0, *sw, 22.0);
            let act = self.sort == *so;
            let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&r, mx, my)).unwrap_or(false);
            if act {
                fill_round_rect(rt, r, 5.0, theme.hover);
            } else if hov {
                fill_round_rect(rt, r, 5.0, with_alpha(theme.text, 0.04));
            }
            draw_text(rt, lbl, &fonts.ui_small_center, r, if act { theme.text } else if hov { theme.sub } else { theme.muted });
            self.hit_boxes.push(HitBox { rect: r, action: ClickAction::SelectSort(*so) });
        }

        let mut left_cursor = top_c.left + 84.0 + cnt_w;

        // Pinned-only quick filter chip (shown whenever at least 1 pinned session exists)
        if pinned_total > 0 {
            let pin_lbl = format!("★置顶 {}", pinned_total);
            let pin_w = (factories.measure_text_width(&pin_lbl, &fonts.ui_small_center) + 14.0).clamp(56.0, 78.0);
            if left_cursor + pin_w + 8.0 < sx {
                let pin_r = rect_f(left_cursor, top_c.top + 8.0, pin_w, 22.0);
                self.draw_ghost_btn(
                    rt,
                    fonts,
                    theme,
                    pin_r,
                    &pin_lbl,
                    self.pinned_only,
                    ClickAction::TogglePinnedOnly,
                );
                if self.pinned_only {
                    stroke_round_rect(rt, pin_r, 6.0, with_alpha(theme.accent, 0.65), 0.9);
                }
                left_cursor += pin_w + 6.0;
            }
        }

        // Subagent grouping toggle button (shown when enough space remains for search box)
        if sub_total > 0 || !self.group_subagents {
            let grp_lbl = if self.group_subagents { "子会话:归组" } else { "子会话:独立" };
            let grp_w = 76.0;
            let reserve_for_search = if self.session_filter.is_some() { 184.0 } else { 102.0 };
            if left_cursor + grp_w + reserve_for_search < sx {
                let grp_r = rect_f(left_cursor, top_c.top + 8.0, grp_w, 22.0);
                self.draw_ghost_btn(
                    rt,
                    fonts,
                    theme,
                    grp_r,
                    grp_lbl,
                    !self.group_subagents,
                    ClickAction::ToggleGroupSubagents,
                );
                left_cursor += grp_w + 6.0;
            }
        }

        // Dismissible drill-down filter chip (precisely measured so long model names don't clip!)
        if let Some(f) = self.session_filter.clone() {
            let chip_lbl = if self.privacy_mode {
                self.with_redactor(|r| f.label(Some(r)))
            } else {
                f.label(None)
            };
            let max_w = (sx - 8.0 - left_cursor).max(0.0);
            if max_w >= 72.0 {
                let measured = factories.measure_text_width(&chip_lbl, &fonts.ui_small_center) + 18.0;
                let est_w = measured.clamp(84.0, max_w.min(168.0));
                let chip_r = rect_f(left_cursor, top_c.top + 8.0, est_w, 22.0);
                self.draw_ghost_btn(rt, fonts, theme, chip_r, &chip_lbl, true, ClickAction::ClearSessionFilter);
                left_cursor += est_w + 6.0;
            }
        }

        // Inline search box (shortcut `/`, filters title / ID / workspace / model / tools)
        let avail_search = sx - 8.0 - left_cursor;
        if avail_search >= 92.0 {
            let sw = avail_search.clamp(92.0, 210.0);
            let sr = rect_f(left_cursor, top_c.top + 8.0, sw, 22.0);
            let input_bg = if theme.is_dark {
                crate::d2d::rgb(20, 20, 22)
            } else {
                crate::d2d::rgb(246, 244, 241)
            };
            fill_round_rect(rt, sr, 5.5, input_bg);
            stroke_round_rect(
                rt,
                sr,
                5.5,
                if self.search_focused { theme.accent } else { theme.border },
                if self.search_focused { 1.2 } else { 0.9 },
            );
            let clear_w = if self.search_query.is_empty() { 0.0 } else { 18.0 };
            self.search_field_rect = Some(rect_f(sr.left + 6.0, sr.top + 3.0, (sw - 12.0 - clear_w).max(30.0), 16.0));
            if !self.search_query.is_empty() {
                let clr = rect_f(sr.right - 18.0, sr.top + 2.0, 16.0, 18.0);
                self.draw_ghost_btn(rt, fonts, theme, clr, "×", false, ClickAction::ClearSearch);
            }
        }

        let rr_l = top_c.left + 10.0;
        let rr_w = bw - 24.0;
        let rx = rr_l + rr_w - 8.0;
        let list_y = top_c.top + 55.0;
        let row_h = 28.0;
        let vis = (((top_c.bottom - 8.0 - list_y) / row_h).floor() as usize).max(1);
        self.sessions_vis_rows = vis;
        self.max_scroll = sorted_len.saturating_sub(vis);
        if self.scroll_rows > self.max_scroll {
            self.scroll_rows = self.max_scroll;
        }
        let scroll_rows = self.scroll_rows;
        let max_scroll = self.max_scroll;
        let show_multi = self.has_non_antigravity;

        let (new_row_rects, new_hits) = self.with_display_sessions_for_tab(|sorted| {
            let mut row_rects_out = Vec::with_capacity(vis);
            let mut hits_out = Vec::with_capacity(vis * 2);

            // Dynamically size model column to fit long model names cleanly
            let max_model_px = sorted
                .iter()
                .skip(scroll_rows)
                .take(vis)
                .map(|r| factories.measure_text_width(&r.primary_model, &fonts.mono_small_right))
                .fold(124.0_f32, f32::max);
            let model_cap = (rr_w * 0.24).clamp(138.0, 206.0);
            let model_w = (max_model_px + 8.0).clamp(124.0, model_cap);
            let (sm_x, sm_w, metrics_w) = session_metric_columns(rx);
            let sm_rect = |i: usize, y: f32, h: f32| rect_f(sm_x[i], y, sm_w[i], h);
            let model_x = rx - metrics_w - model_w;
            let time_x = rr_l + 36.0;
            let title_x = rr_l + 96.0;
            let title_w = (model_x - 10.0 - title_x).max(96.0);

            // Column headers
            let hdr_y = top_c.top + 32.0;
            let hdr_h = 19.0;
            draw_text(rt, "时间", &fonts.ui_micro, rect_f(time_x, hdr_y, 56.0, hdr_h), theme.muted);
            draw_text(rt, "会话 / 项目", &fonts.ui_micro, rect_f(title_x, hdr_y, title_w, hdr_h), theme.muted);
            draw_text(rt, "模型", &fonts.ui_small_right, rect_f(model_x, hdr_y, model_w, hdr_h), theme.muted);
            for (i, lbl) in SESSION_METRIC_LABELS.iter().enumerate() {
                draw_text(rt, lbl, &fonts.ui_small_right, sm_rect(i, hdr_y, hdr_h), theme.muted);
            }
            draw_line(rt, top_c.left + 14.0, top_c.top + 52.0, top_c.right - 14.0, top_c.top + 52.0, theme.border, 1.0);

            if sorted.is_empty() {
                let empty_txt = if self.pinned_only {
                    "当前筛选条件下无置顶会话 · 点击上方「★置顶」或清除搜索查看全部"
                } else if self.session_filter.is_some() {
                    "当前筛选条件下无匹配会话 · 点击上方筛选标签或按 Backspace 清除"
                } else {
                    "当前范围暂无会话 · 可在右上角切换 7天 / 30天 / 全部"
                };
                draw_text(
                    rt,
                    empty_txt,
                    &fonts.ui_reg_center,
                    rect_f(top_c.left, list_y, bw, (top_c.bottom - list_y).max(30.0)),
                    theme.muted,
                );
            } else {
                let mut prev_was_pinned = false;
                for (vi, row) in sorted.iter().skip(scroll_rows).take(vis).enumerate() {
                    let s = self.row_session(row);
                    let ry = list_y + vi as f32 * row_h;
                    let cell_h = row_h - 2.0;
                    let rr = rect_f(rr_l, ry, rr_w, cell_h);
                    let sel = self.selected_session.as_deref() == Some(s.conversation_id.as_str());
                    let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&rr, mx, my)).unwrap_or(false);

                    // Soft divider separating pinned block from regular rows
                    if vi > 0 && prev_was_pinned && !row.is_pinned {
                        draw_line(
                            rt,
                            top_c.left + 16.0,
                            ry - 1.0,
                            top_c.right - 16.0,
                            ry - 1.0,
                            theme.border,
                            1.0,
                        );
                    }
                    prev_was_pinned = row.is_pinned;

                    if sel {
                        fill_round_rect(rt, rr, 6.0, theme.hover);
                        fill_round_rect(rt, rect_f(rr.left, rr.top + 5.0, 2.5, cell_h - 10.0), 1.25, theme.accent);
                    } else if hov {
                        fill_round_rect(rt, rr, 6.0, with_alpha(theme.text, 0.04));
                    }

                    // 28 DIP left gutter: live/recent status dot + clickable pin star
                    let cy = ry + cell_h * 0.5;
                    draw_session_status_dot(rt, theme, rr_l + 10.5, cy, s, row.last_time, now_local);
                    let star_r = rect_f(rr_l + 16.0, ry + 3.0, 18.0, cell_h - 6.0);
                    let star_hov = self.mouse_dip.map(|(mx, my)| hit_rect(&star_r, mx, my)).unwrap_or(false);
                    if row.is_pinned || hov || sel {
                        if star_hov {
                            fill_round_rect(rt, star_r, 4.5, theme.accent_soft);
                        }
                        let (glyph, col) = if row.is_pinned {
                            ("★", theme.accent)
                        } else if star_hov {
                            ("☆", theme.accent)
                        } else {
                            ("☆", theme.muted)
                        };
                        draw_text(rt, glyph, &fonts.ui_small_center, star_r, col);
                    }

                    let t_str = fmt_relative_time_cn(row.last_time, now_local);
                    let mut hit_buf = FmtBuf::new();
                    let _ = write!(hit_buf, "{:.1}%", row.counts.cache_hit_rate());
                    let hit_col = cache_hit_color(theme, &row.counts);
                    let cost_str = fmt_cost(&row.counts.cost, row.counts.calls, human);
                    let base_title = self.format_session_title(s);
                    let mut prefix = String::new();
                    if row.is_child_row {
                        prefix.push_str("↳ ");
                    }
                    if let Some(tag) = format_source_tag(s.source, show_multi) {
                        prefix.push_str(&format!("[{tag}] "));
                    }
                    if s.is_archived {
                        prefix.push_str("[归档] ");
                    }
                    let title_disp = format!("{prefix}{base_title}");
                    let mut sub_buf = FmtBuf::new();
                    let has_sub_badge = !row.is_child_row && row.subagent_count > 0;
                    if has_sub_badge {
                        let _ = write!(sub_buf, "+{}子", row.subagent_count);
                    }
                    let ws_badge = {
                        let ws = self.format_workspace_short(&s.workspace);
                        if ws.is_empty() { None } else { Some(ws) }
                    };

                    draw_text(
                        rt,
                        &t_str,
                        &fonts.mono_small,
                        rect_f(time_x, ry, 56.0, cell_h),
                        if row.out_of_scope { theme.muted } else { theme.sub },
                    );

                    // Compute trailing pill budgets cleanly: Title [workspace] [+N子]
                    let mut reserved = 0.0_f32;
                    let sub_w = if has_sub_badge {
                        Some((factories.measure_text_width(sub_buf.as_str(), &fonts.ui_small_center) + 10.0).ceil())
                    } else {
                        None
                    };
                    if let Some(sw) = sub_w {
                        if title_w > sw + 56.0 {
                            reserved += sw + 5.0;
                        }
                    }
                    let ws_w = ws_badge.as_deref().and_then(|w| {
                        let nat = (factories.measure_text_width(w, &fonts.ui_micro) + 10.0).ceil();
                        let capped = nat.min(104.0);
                        if title_w - reserved > capped + 76.0 {
                            Some(capped)
                        } else {
                            None
                        }
                    });
                    if let Some(ww) = ws_w {
                        reserved += ww + 5.0;
                    }
                    let actual_title_w = (title_w - reserved).max(40.0);
                    draw_text(
                        rt,
                        &title_disp,
                        &fonts.ui_reg,
                        rect_f(title_x, ry, actual_title_w, cell_h),
                        if row.is_child_row { theme.sub } else { theme.text },
                    );

                    let nat_title = factories.measure_text_width(&title_disp, &fonts.ui_reg);
                    let mut pill_x = title_x + nat_title.min(actual_title_w) + 6.0;
                    let pill_y = ry + (cell_h - 16.0) * 0.5;
                    if let (Some(ws), Some(ww)) = (ws_badge.as_deref(), ws_w) {
                        let wr = rect_f(pill_x, pill_y, ww, 16.0);
                        fill_round_rect(rt, wr, 5.0, theme.surface_subtle);
                        stroke_round_rect(rt, wr, 5.0, theme.border, 0.7);
                        draw_text(rt, ws, &fonts.ui_micro, rect_f(wr.left + 5.0, wr.top, ww - 10.0, 16.0), theme.sub);
                        pill_x += ww + 5.0;
                    }
                    if let Some(sw) = sub_w {
                        if pill_x + sw <= title_x + title_w + 2.0 {
                            let br = rect_f(pill_x, pill_y, sw, 16.0);
                            fill_round_rect(rt, br, 8.0, with_alpha(theme.accent, 0.14));
                            draw_text(rt, sub_buf.as_str(), &fonts.ui_small_center, br, theme.accent);
                        }
                    }

                    draw_text(rt, &row.primary_model, &fonts.mono_small_right, rect_f(model_x, ry, model_w, cell_h), theme.muted);
                    draw_session_metric_cells(
                        rt,
                        fonts,
                        theme,
                        &sm_x,
                        &sm_w,
                        ry,
                        cell_h,
                        &row.counts,
                        hit_buf.as_str(),
                        hit_col,
                        &cost_str,
                        self.cost_mode,
                        human,
                    );

                    let cid = s.conversation_id.clone();
                    row_rects_out.push((rr, cid.clone()));
                    hits_out.push(HitBox {
                        rect: rr,
                        action: ClickAction::SelectSession(cid.clone()),
                    });
                    // Push pin star hitbox AFTER row hitbox so reverse-order hit_test_click hits star first.
                    hits_out.push(HitBox {
                        rect: star_r,
                        action: ClickAction::TogglePinSession(cid),
                    });
                }

                if max_scroll > 0 {
                    let track_h = (top_c.bottom - 10.0 - list_y).max(20.0);
                    let thumb_h = ((vis as f32 / sorted.len() as f32) * track_h).clamp(24.0, track_h);
                    let ratio = scroll_rows as f32 / max_scroll as f32;
                    let thumb_y = list_y + ratio * (track_h - thumb_h);
                    fill_round_rect(rt, rect_f(top_c.right - 7.0, thumb_y, 3.0, thumb_h), 1.5, theme.muted);
                }
            }
            (row_rects_out, hits_out)
        });
        self.session_row_rects.extend(new_row_rects);
        self.hit_boxes.extend(new_hits);

        // Bottom: Session Action Header + Meta/Legend + Turn-by-Turn Context Anatomy
        let bot_c = rect_f(body.left, top_c.bottom + 12.0, bw, (body.bottom - top_c.bottom - 12.0).max(140.0));
        fill_round_rect(rt, bot_c, 10.0, theme.surface);
        stroke_round_rect(rt, bot_c, 10.0, theme.border, 1.0);

        let sel_cid = self.selected_session.clone();
        let found_loc = sel_cid.as_deref().and_then(|id| {
            if let Some(idx) = self.cached_sessions.iter().position(|s| s.conversation_id == id) {
                Some((false, idx))
            } else {
                self.pinned_out_of_scope
                    .iter()
                    .position(|s| s.conversation_id == id)
                    .map(|idx| (true, idx))
            }
        });

        if let (Some(sel_id), Some((is_oos, sess_idx))) = (sel_cid, found_loc) {
            let detail_key = SessionDetailKey {
                data_gen: self.data_gen,
                sess_ptr: self.cached_sessions.as_ptr() as usize,
                sess_len: self.cached_sessions.len(),
                snap_sess_len: snapshot.sessions.len(),
                selected_cid: sel_id,
                session_filter: self.session_filter.clone(),
            };
            let need_rebuild = self
                .session_detail_cache
                .borrow()
                .as_ref()
                .is_none_or(|c| c.key != detail_key);
            if need_rebuild {
                let raw_s = if is_oos {
                    &self.pinned_out_of_scope[sess_idx]
                } else {
                    &self.cached_sessions[sess_idx]
                };
                let sliced_override = if !is_oos {
                    self.slicing_filter().and_then(|f| slice_session(raw_s, f))
                } else {
                    None
                };
                let eff = sliced_override.as_ref().unwrap_or(raw_s);
                let (sub_cnt, sub_agg) = self.count_subagents_of(&eff.conversation_id);
                let cap = classify_resume_capability(eff, &snapshot.sessions);
                let valid_id = validate_conversation_id(&eff.conversation_id);
                let direct_ok = cap.is_direct_resumable() && valid_id;
                let blocked_msg: &'static str = if !valid_id {
                    BlockedReason::InvalidConversationId.message_cn()
                } else {
                    cap.disabled_reason_cn()
                        .or_else(|| cap.blocked_reason().map(BlockedReason::message_cn))
                        .unwrap_or("该会话不支持终端恢复")
                };
                let resumable_parent_id = find_resumable_parent(eff, &snapshot.sessions)
                    .map(|p| p.conversation_id.clone());
                let has_ws = normalize_workspace_path_str(&eff.workspace).is_some();
                let compaction_count = eff.compaction_count as usize;
                let (cache_miss_count, _) = eff.cache_miss_stats();
                let active_duration_secs = eff.active_duration_secs();
                let thinking_turns = eff.thinking_turns as usize;
                let mut tools_vec: Vec<(String, u64)> = eff.tool_counts().into_iter().collect();
                tools_vec.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                let top_tools_str = if !tools_vec.is_empty() {
                    let top_t: Vec<String> = tools_vec
                        .into_iter()
                        .take(2)
                        .map(|(n, c)| format!("{n}×{c}"))
                        .collect();
                    top_t.join("/")
                } else {
                    String::new()
                };
                *self.session_detail_cache.borrow_mut() = Some(SessionDetailCache {
                    key: detail_key,
                    sliced_override,
                    sub_cnt,
                    sub_agg,
                    direct_ok,
                    blocked_msg,
                    resumable_parent_id,
                    has_ws,
                    compaction_count,
                    cache_miss_count,
                    active_duration_secs,
                    thinking_turns,
                    top_tools_str,
                });
            }

            let detail_cache = self.session_detail_cache.borrow();
            let det = detail_cache.as_ref().expect("detail cache populated");
            let s: &SessionInfo = det.sliced_override.as_ref().unwrap_or_else(|| {
                if is_oos {
                    &self.pinned_out_of_scope[sess_idx]
                } else {
                    &self.cached_sessions[sess_idx]
                }
            });

            let cid = s.conversation_id.clone();
            let raw_title = s.title.clone();
            let is_pinned = self.pinned_ids.contains(&cid);
            let sub_cnt = det.sub_cnt;
            let sub_agg = det.sub_agg;
            let direct_ok = det.direct_ok;
            let blocked_msg = det.blocked_msg;
            let resumable_parent_id = det.resumable_parent_id.clone();
            let has_ws = det.has_ws;

            let slice_note = match self.slicing_filter() {
                Some(SessionFilter::Model(m)) => format!(" [仅 {m}]"),
                Some(SessionFilter::Date(d)) => format!(" [仅 {d}]"),
                Some(SessionFilter::Week(w)) => format!(" [仅 {w}]"),
                _ => String::new(),
            };
            let mut title_prefix = String::new();
            if let Some(tag) = format_source_tag(s.source, show_multi) {
                title_prefix.push_str(&format!("[{tag}] "));
            }
            if s.is_subagent {
                title_prefix.push_str("[子会话] ");
            }
            let title_str = format!("{}{}{}", title_prefix, self.format_session_title(s), slice_note);
            let sid_short = self.format_session_id(&cid);
            let ws_short = self.format_workspace_short(&s.workspace);
            let id_ws_suffix = if ws_short.is_empty() {
                format!("#{sid_short}")
            } else {
                format!("#{sid_short} · {ws_short}")
            };

            let mut meta = if self.cost_mode {
                format!(
                    "{} · 费用 {} (新入{} / 缓{} / 出{}) · 总 {} · {}轮 · 命中 {:.1}%",
                    s.primary_model(),
                    fmt_cost(&s.counts.cost, s.counts.calls, false),
                    fmt_usd_compact(s.counts.cost.input_plus_write_usd()),
                    fmt_usd_compact(s.counts.cost.cache_read_usd),
                    fmt_usd_compact(s.counts.cost.output_usd),
                    fmt_compact(s.counts.total_tokens()),
                    s.turns.len(),
                    s.counts.cache_hit_rate(),
                )
            } else {
                format!(
                    "{} · {}轮 · 总 {} (新入 {} / 出 {} / 命中 {:.1}%) · 费用 {}",
                    s.primary_model(),
                    s.turns.len(),
                    fmt_compact(s.counts.total_tokens()),
                    fmt_compact(s.counts.input_tokens + s.counts.cache_write_tokens),
                    fmt_compact(s.counts.output_tokens),
                    s.counts.cache_hit_rate(),
                    fmt_cost(&s.counts.cost, s.counts.calls, false),
                )
            };
            if let Some(lt) = s.last_time {
                meta.push_str(&format!(" · {}", lt.format("%m/%d %H:%M")));
            }
            if sub_cnt > 0 {
                meta.push_str(&format!(
                    " · +{}子({})",
                    sub_cnt,
                    fmt_cost_compact(&sub_agg.cost, sub_agg.calls)
                ));
            } else if s.is_subagent && (!s.parent_title.is_empty() || s.parent_id.is_some()) {
                let p_disp = match s.parent_id.as_deref() {
                    Some(pid) => self.format_parent_title(pid, &s.parent_title),
                    None if self.privacy_mode => "父任务".to_string(),
                    None => s.parent_title.clone(),
                };
                if !p_disp.is_empty() {
                    meta.push_str(&format!(" · 所属: {}", p_disp));
                }
            }
            if det.compaction_count > 0 {
                meta.push_str(&format!(" · {}压缩", det.compaction_count));
            }
            if det.cache_miss_count > 0 {
                meta.push_str(&format!(" · {}未命中", det.cache_miss_count));
            }
            if det.active_duration_secs > 0 {
                meta.push_str(&format!(" · 活跃{}", fmt_duration_short(det.active_duration_secs)));
            }
            if det.thinking_turns > 0 {
                meta.push_str(&format!(" · 思考{}/{}", det.thinking_turns, s.turns.len()));
            }
            if !det.top_tools_str.is_empty() {
                meta.push_str(&format!(" · 工具 {}", det.top_tools_str));
            }
            if let Some(tag) = format_source_tag(s.source, show_multi) {
                meta.push_str(&format!(" · [{tag}]"));
            }
            if s.is_archived {
                meta.push_str(" · [归档]");
            }
            let already_in_parent_group = matches!(
                &self.session_filter,
                Some(SessionFilter::Parent(pid, _))
                    if *pid == cid || s.parent_id.as_deref() == Some(pid.as_str())
            );
            // Cheap O(1) Arc refcount clone of turns slice so we can drop `detail_cache` borrow before `&mut self` draws!
            let turns_arc: Arc<[TurnRecord]> = Arc::clone(&s.turns);
            drop(detail_cache);

            // ---- Row 1 (bot_c.top + 8, h = 22): Pin + Title + #id · workspace  |  [▶ 继续] [复制命令] [目录] [···] ----
            let r1_y = bot_c.top + 8.0;
            let r1_h = 22.0;

            let mut act_rx = bot_c.right - 14.0;
            let more_w = 28.0;
            let more_r = rect_f(act_rx - more_w, r1_y, more_w, r1_h);
            self.detail_more_btn_rect = Some(more_r);
            self.draw_ghost_btn(
                rt,
                fonts,
                theme,
                more_r,
                "···",
                false,
                ClickAction::SessionMoreMenu(cid.clone()),
            );
            stroke_round_rect(rt, more_r, 6.0, theme.border, 0.8);
            act_rx = more_r.left - 5.0;

            let dir_w = 40.0;
            let dir_r = rect_f(act_rx - dir_w, r1_y, dir_w, r1_h);
            if has_ws {
                self.draw_ghost_btn(
                    rt,
                    fonts,
                    theme,
                    dir_r,
                    "目录",
                    false,
                    ClickAction::OpenWorkspace(cid.clone()),
                );
                stroke_round_rect(rt, dir_r, 6.0, theme.border, 0.8);
            } else {
                fill_round_rect(rt, dir_r, 6.0, theme.surface_subtle);
                draw_text(rt, "目录", &fonts.ui_small_center, dir_r, theme.muted);
                self.hit_boxes.push(HitBox {
                    rect: dir_r,
                    action: ClickAction::ShowBlockedReason("该会话无可用本地工作区路径".to_string()),
                });
            }
            act_rx = dir_r.left - 5.0;

            let cmd_copied = self
                .resume_cmd_copied_until
                .map(|t| Instant::now() < t)
                .unwrap_or(false);
            let cmd_w = 62.0;
            let cmd_r = rect_f(act_rx - cmd_w, r1_y, cmd_w, r1_h);
            if direct_ok {
                self.draw_ghost_btn(
                    rt,
                    fonts,
                    theme,
                    cmd_r,
                    if cmd_copied { "已复制" } else { "复制命令" },
                    cmd_copied,
                    ClickAction::CopyResumeCmd(cid.clone()),
                );
                stroke_round_rect(
                    rt,
                    cmd_r,
                    6.0,
                    if cmd_copied { theme.accent } else { theme.border },
                    0.8,
                );
            } else {
                fill_round_rect(rt, cmd_r, 6.0, theme.surface_subtle);
                draw_text(rt, "复制命令", &fonts.ui_small_center, cmd_r, theme.muted);
                self.hit_boxes.push(HitBox {
                    rect: cmd_r,
                    action: ClickAction::ShowBlockedReason(blocked_msg.to_string()),
                });
            }
            act_rx = cmd_r.left - 5.0;

            if direct_ok {
                let res_w = 64.0;
                let res_r = rect_f(act_rx - res_w, r1_y, res_w, r1_h);
                let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&res_r, mx, my)).unwrap_or(false);
                fill_round_rect(
                    rt,
                    res_r,
                    6.0,
                    if hov { theme.accent } else { theme.accent_soft },
                );
                stroke_round_rect(rt, res_r, 6.0, with_alpha(theme.accent, 0.8), 0.9);
                draw_text(
                    rt,
                    "▶ 继续",
                    &fonts.ui_small_center,
                    res_r,
                    if hov { theme.bg } else { theme.accent },
                );
                self.hit_boxes.push(HitBox {
                    rect: res_r,
                    action: ClickAction::ResumeSession(cid.clone()),
                });
                act_rx = res_r.left - 8.0;
            } else if let Some(pid) = resumable_parent_id {
                let res_w = 78.0;
                let res_r = rect_f(act_rx - res_w, r1_y, res_w, r1_h);
                self.draw_ghost_btn(
                    rt,
                    fonts,
                    theme,
                    res_r,
                    "定位父会话 ›",
                    true,
                    ClickAction::InspectSession(pid),
                );
                stroke_round_rect(rt, res_r, 6.0, with_alpha(theme.accent, 0.65), 0.8);
                act_rx = res_r.left - 8.0;
            } else {
                let res_w = 64.0;
                let res_r = rect_f(act_rx - res_w, r1_y, res_w, r1_h);
                fill_round_rect(rt, res_r, 6.0, theme.surface_subtle);
                stroke_round_rect(rt, res_r, 6.0, theme.border, 0.7);
                draw_text(rt, "▶ 继续", &fonts.ui_small_center, res_r, theme.muted);
                self.hit_boxes.push(HitBox {
                    rect: res_r,
                    action: ClickAction::ShowBlockedReason(blocked_msg.to_string()),
                });
                act_rx = res_r.left - 8.0;
            }

            if sub_cnt > 0 && !already_in_parent_group {
                let lbl = format!("+{}子 ›", sub_cnt);
                let sub_w = (factories.measure_text_width(&lbl, &fonts.ui_small_center) + 12.0).clamp(48.0, 74.0);
                if act_rx - sub_w > bot_c.left + 200.0 {
                    let sub_r = rect_f(act_rx - sub_w, r1_y + 1.0, sub_w, r1_h - 2.0);
                    self.draw_ghost_btn(
                        rt,
                        fonts,
                        theme,
                        sub_r,
                        &lbl,
                        true,
                        ClickAction::FilterSessions(SessionFilter::Parent(cid.clone(), raw_title)),
                    );
                    act_rx = sub_r.left - 6.0;
                }
            }

            let pin_r = rect_f(bot_c.left + 12.0, r1_y, 22.0, r1_h);
            self.draw_ghost_btn(
                rt,
                fonts,
                theme,
                pin_r,
                if is_pinned { "★" } else { "☆" },
                is_pinned,
                ClickAction::TogglePinSession(cid),
            );

            let id_left = bot_c.left + 38.0;
            let id_avail = (act_rx - id_left).max(100.0);
            let suffix_w = (factories.measure_text_width(&id_ws_suffix, &fonts.mono_small) + 6.0).min(210.0);
            let show_suffix = id_avail > suffix_w + 110.0;
            let title_max_w = if show_suffix {
                (id_avail - suffix_w - 8.0).max(80.0)
            } else {
                id_avail
            };
            draw_text(
                rt,
                &title_str,
                &fonts.ui_section,
                rect_f(id_left, r1_y, title_max_w, r1_h),
                theme.text,
            );
            if show_suffix {
                let nat_t = factories.measure_text_width(&title_str, &fonts.ui_section);
                let sx_pos = id_left + nat_t.min(title_max_w) + 8.0;
                let rem_w = (act_rx - sx_pos).max(0.0);
                if rem_w >= 48.0 {
                    draw_text(
                        rt,
                        &id_ws_suffix,
                        &fonts.mono_small,
                        rect_f(sx_pos, r1_y + 1.0, rem_w, r1_h - 2.0),
                        theme.muted,
                    );
                }
            }

            // ---- Row 2 (bot_c.top + 33, h = 22): Meta summary on left | Chart legend + 总计/仅增量 on right ----
            let legend_left = self.draw_chart_legend_and_toggle_at(rt, fonts, theme, bot_c, bot_c.top + 32.0);
            let meta_w = (legend_left - 10.0 - (bot_c.left + 16.0)).max(120.0);
            draw_text(
                rt,
                &meta,
                &fonts.mono_small,
                rect_f(bot_c.left + 16.0, bot_c.top + 35.0, meta_w, 16.0),
                theme.sub,
            );

            // Lightweight bars: ZERO tooltip allocations unless hovered, and only format X-axis
            // labels on indices actually rendered by `draw_editorial_bars` (500 allocs -> ~15).
            let plot = rect_f(bot_c.left + 12.0, bot_c.top + 56.0, bw - 24.0, (bot_c.bottom - bot_c.top - 64.0).max(60.0));
            let n_turns = turns_arc.len();
            let stride = editorial_bar_label_stride(plot, n_turns);
            let bars: Vec<BarItem> = turns_arc
                .iter()
                .enumerate()
                .map(|(idx, t)| {
                    let need_lbl = idx + 1 == n_turns || idx % stride == 0;
                    let lbl: Cow<'static, str> = if !need_lbl {
                        Cow::Borrowed("")
                    } else if t.step_index >= 0 {
                        Cow::Owned(format!("#{}", t.step_index))
                    } else {
                        Cow::Borrowed("-")
                    };
                    self.make_turn_bar(lbl, t)
                })
                .collect();
            let (_, inner) = draw_editorial_bars(
                rt,
                fonts,
                theme,
                plot,
                &bars,
                self.fresh_only,
                self.bar_unit(),
                self.mouse_dip,
                "无逐轮记录",
                |idx| {
                    let t = &turns_arc[idx];
                    let lbl = if t.step_index >= 0 { format!("#{}", t.step_index) } else { "-".into() };
                    let ts = t.timestamp_utc.map(|u| u.with_timezone(&Local).format("%H:%M:%S").to_string()).unwrap_or_else(|| "-".into());
                    let prev_ts = turns_arc[..idx].iter().rev().find_map(|p| p.timestamp_utc);
                    let gap_s = match (prev_ts, t.timestamp_utc) {
                        (Some(p), Some(cur)) => {
                            let g = fmt_gap((cur - p).num_seconds());
                            if g.is_empty() { String::new() } else { format!(" · 隔 {}", g) }
                        }
                        _ => String::new(),
                    };
                    let is_miss = idx > 0
                        && !t.is_checkpoint
                        && t.total_input() >= 30_000
                        && t.cache_hit_rate() < 50.0;
                    let step_hdr = if t.is_checkpoint {
                        format!("Step {} · 上下文压缩 ({}{})", lbl, ts, gap_s)
                    } else if is_miss {
                        format!("Step {} · 缓存未命中 ({}{})", lbl, ts, gap_s)
                    } else {
                        format!("Step {} ({}{})", lbl, ts, gap_s)
                    };
                    let cost_line = if t.cost.is_fully_unpriced(1) {
                        "费用   : - (未定价)".to_string()
                    } else {
                        format!(
                            "费用   : {} (入{} / 缓{} / 出{})",
                            fmt_usd_precise(t.cost.total_usd()),
                            fmt_usd_compact(t.cost.input_plus_write_usd()),
                            fmt_usd_compact(t.cost.cache_read_usd),
                            fmt_usd_compact(t.cost.output_usd),
                        )
                    };
                    let mut tip = vec![
                        step_hdr,
                        format!("模型   : {}", t.model),
                        cost_line,
                        format!("总Token: {}", fmt_commas(t.total_tokens())),
                        format!("新输入 : {}", fmt_commas(t.input_tokens + t.cache_write_tokens)),
                        format!("输出   : {}", fmt_commas(t.output_tokens)),
                        format!("缓存读 : {} ({:.1}%)", fmt_commas(t.cache_read_tokens), t.cache_hit_rate()),
                    ];
                    match (t.gen_secs, t.output_tps()) {
                        (Some(g), Some(v)) => tip.push(format!("生成   : {} ({:.0} tok/s)", fmt_duration_short(g as u64), v)),
                        (Some(g), None) => tip.push(format!("生成   : {}", fmt_duration_short(g as u64))),
                        (None, _) => {}
                    }
                    if !t.tool_names.is_empty() {
                        let shown_t: Vec<&str> = t.tool_names.iter().take(4).map(Arc::as_ref).collect();
                        let more = if t.tool_names.len() > 4 {
                            format!(" +{}", t.tool_names.len() - 4)
                        } else {
                            String::new()
                        };
                        tip.push(format!("工具   : {}{}", shown_t.join(", "), more));
                    }
                    if let Some(th) = format_turn_thinking(t) {
                        tip.push(format!("思考   : {th}"));
                    }
                    tip
                },
            );
            if let Some(p) = inner {
                self.discrete_plots.push(DiscretePlot::VerticalBars {
                    plot: p,
                    n: bars.len(),
                });
            }
        } else {
            draw_text(rt, "点击上方会话查看逐轮上下文增长趋势", &fonts.ui_reg_center, bot_c, theme.muted);
        }
    }

    // ---------- Call Log Tab (one row per model call, newest first) ----------

    fn draw_calls_tab(
        &mut self,
        factories: &D2DFactories,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        body: D2D_RECT_F,
        human: bool,
    ) {
        struct CallDisp {
            key: (String, i64),
            time: String,
            title: String,
            is_child: bool,
            model: Arc<str>,
            tools: String,
            tools_col: D2D1_COLOR_F,
            gen: String,
            tps: String,
            total_tok: u64,
            fresh_in: u64,
            out: u64,
            hit: String,
            hit_col: D2D1_COLOR_F,
            cost: String,
        }

        let bw = body.right - body.left;
        let bh = body.bottom - body.top;
        let detail_h = (bh * 0.30).clamp(88.0, 104.0);
        let top_c = rect_f(body.left, body.top, bw, (bh - detail_h - 12.0).max(140.0));
        fill_round_rect(rt, top_c, 10.0, theme.surface);
        stroke_round_rect(rt, top_c, 10.0, theme.border, 1.0);

        let (total_rows, init_sel_idx, first_key, sum, speed_out, speed_secs) =
            self.with_call_rows(|rows, sum, speed_out, speed_secs| {
                let sel_idx = self.selected_call_index(rows);
                let first_k = if sel_idx.is_none() && !rows.is_empty() {
                    Some(self.call_key(&rows[0]))
                } else {
                    None
                };
                (rows.len(), sel_idx, first_k, sum, speed_out, speed_secs)
            });
        let sel_idx = match init_sel_idx {
            Some(i) => Some(i),
            None => {
                if let Some(fk) = first_key {
                    self.selected_call = Some(fk);
                    Some(0)
                } else {
                    None
                }
            }
        };

        // ---- Header: title, count/cost summary, session chip, search, sort toggles ----
        draw_text(rt, "调用记录", &fonts.ui_section, rect_f(top_c.left + 16.0, top_c.top + 9.0, 66.0, 20.0), theme.text);
        let cnt_str = if total_rows == 0 {
            "0 次".to_string()
        } else if speed_secs > 0 {
            format!(
                "{} 次 · 总 {} · {} · 均速 {:.0} tok/s",
                total_rows,
                fmt_compact(sum.total_tokens()),
                fmt_cost(&sum.cost, sum.calls, true),
                speed_out as f64 / speed_secs as f64
            )
        } else {
            format!(
                "{} 次 · 总 {} · {}",
                total_rows,
                fmt_compact(sum.total_tokens()),
                fmt_cost(&sum.cost, sum.calls, true)
            )
        };
        let cnt_w = (factories.measure_text_width(&cnt_str, &fonts.ui_micro) + 6.0).clamp(44.0, 290.0);
        draw_text(rt, &cnt_str, &fonts.ui_micro, rect_f(top_c.left + 80.0, top_c.top + 11.0, cnt_w, 16.0), theme.muted);

        let sorts = [
            (CallSort::Recent, "最新", 38.0),
            (CallSort::Cost, "费用", 38.0),
            (CallSort::Total, "总Token", 54.0),
            (CallSort::Input, "新输入", 46.0),
            (CallSort::Output, "输出", 38.0),
            (CallSort::Duration, "耗时", 38.0),
        ];
        let mut sx = top_c.right - 14.0;
        for (so, lbl, sw) in sorts.iter().rev() {
            sx -= *sw + 4.0;
            let r = rect_f(sx, top_c.top + 8.0, *sw, 22.0);
            let act = self.call_sort == *so;
            let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&r, mx, my)).unwrap_or(false);
            if act {
                fill_round_rect(rt, r, 5.0, theme.hover);
            } else if hov {
                fill_round_rect(rt, r, 5.0, with_alpha(theme.text, 0.04));
            }
            draw_text(rt, lbl, &fonts.ui_small_center, r, if act { theme.text } else if hov { theme.sub } else { theme.muted });
            self.hit_boxes.push(HitBox { rect: r, action: ClickAction::SelectCallSort(*so) });
        }

        let mut left_cursor = top_c.left + 84.0 + cnt_w;
        if let Some(cid) = self.call_session_filter.clone() {
            let disp = self
                .cached_sessions
                .iter()
                .chain(self.pinned_out_of_scope.iter())
                .find(|s| s.conversation_id == cid)
                .map(|s| self.format_session_title(s))
                .unwrap_or_else(|| self.format_session_id(&cid));
            let short: String = disp.chars().take(16).collect();
            let ell = if disp.chars().count() > 16 { "…" } else { "" };
            let chip_lbl = format!("会话: {short}{ell}  ×");
            let max_w = (sx - 8.0 - left_cursor).max(0.0);
            if max_w >= 72.0 {
                let measured = factories.measure_text_width(&chip_lbl, &fonts.ui_small_center) + 18.0;
                let chip_w = measured.clamp(84.0, max_w.min(190.0));
                let chip_r = rect_f(left_cursor, top_c.top + 8.0, chip_w, 22.0);
                self.draw_ghost_btn(rt, fonts, theme, chip_r, &chip_lbl, true, ClickAction::ClearCallSessionFilter);
                left_cursor += chip_w + 6.0;
            }
        }

        let avail_search = sx - 8.0 - left_cursor;
        if avail_search >= 92.0 {
            let sw = avail_search.clamp(92.0, 210.0);
            let sr = rect_f(left_cursor, top_c.top + 8.0, sw, 22.0);
            let input_bg = if theme.is_dark {
                crate::d2d::rgb(20, 20, 22)
            } else {
                crate::d2d::rgb(246, 244, 241)
            };
            fill_round_rect(rt, sr, 5.5, input_bg);
            stroke_round_rect(
                rt,
                sr,
                5.5,
                if self.search_focused { theme.accent } else { theme.border },
                if self.search_focused { 1.2 } else { 0.9 },
            );
            let clear_w = if self.search_query.is_empty() { 0.0 } else { 18.0 };
            self.search_field_rect = Some(rect_f(sr.left + 6.0, sr.top + 3.0, (sw - 12.0 - clear_w).max(30.0), 16.0));
            if !self.search_query.is_empty() {
                let clr = rect_f(sr.right - 18.0, sr.top + 2.0, 16.0, 18.0);
                self.draw_ghost_btn(rt, fonts, theme, clr, "×", false, ClickAction::ClearSearch);
            }
        }

        // ---- Column geometry ----
        let rr_l = top_c.left + 10.0;
        let rr_w = bw - 24.0;
        let rx = rr_l + rr_w - 8.0;
        let list_y = top_c.top + 55.0;
        let row_h = 25.0;
        let vis = (((top_c.bottom - 8.0 - list_y) / row_h).floor() as usize).max(1);
        self.sessions_vis_rows = vis;
        self.max_scroll = total_rows.saturating_sub(vis);
        if self.scroll_rows > self.max_scroll {
            self.scroll_rows = self.max_scroll;
        }
        let scroll_rows = self.scroll_rows;

        const MCOLS: [(&str, f32, f32); 7] = [
            ("耗时", 40.0, 4.0),
            ("tok/s", 42.0, 10.0),
            ("总Token", 62.0, 4.0),
            ("新输入", 56.0, 4.0),
            ("输出", 50.0, 10.0),
            ("命中率", 50.0, 4.0),
            ("费用", 60.0, 0.0),
        ];
        let mut mx = [0.0_f32; 7];
        let mut cx = rx;
        for i in (0..MCOLS.len()).rev() {
            cx -= MCOLS[i].1;
            mx[i] = cx;
            if i > 0 {
                cx -= MCOLS[i - 1].2;
            }
        }
        let mrect = |i: usize, y: f32, h: f32| rect_f(mx[i], y, MCOLS[i].1, h);
        let metrics_w = rx - mx[0] + 6.0;
        let time_x = rr_l + 8.0;
        let title_x = rr_l + 112.0;
        let avail = (rx - metrics_w - title_x - 12.0).max(160.0);
        let show_multi = self.has_non_antigravity;

        let (max_model_px, disp, sel_detail) = self.with_call_rows(|rows, _, _, _| {
            let max_model_px = rows
                .iter()
                .skip(scroll_rows)
                .take(vis)
                .map(|r| {
                    factories.measure_text_width(&self.call_session(r).turns[r.turn_idx].model, &fonts.mono_small)
                })
                .fold(96.0_f32, f32::max);

            let disp: Vec<CallDisp> = rows
                .iter()
                .skip(scroll_rows)
                .take(vis)
                .map(|r| {
                    let s = self.call_session(r);
                    let t = &s.turns[r.turn_idx];
                    let mut prefix = String::new();
                    if s.is_subagent {
                        prefix.push_str("↳ ");
                    }
                    if let Some(tag) = format_source_tag(s.source, show_multi) {
                        prefix.push_str(&format!("[{tag}] "));
                    }
                    let ctx = t.total_input();
                    let rate = t.cache_hit_rate();
                    let is_miss = r.turn_idx > 0 && !t.is_checkpoint && ctx >= 30_000 && rate < 50.0;
                    let hit_col = if t.is_checkpoint || r.turn_idx == 0 {
                        theme.muted
                    } else if is_miss {
                        theme.danger
                    } else if rate >= 85.0 {
                        theme.sage
                    } else {
                        theme.sub
                    };
                    let tools_col = if t.is_checkpoint {
                        theme.warn
                    } else if t.tool_names.is_empty() {
                        theme.muted
                    } else {
                        theme.sub
                    };
                    CallDisp {
                        key: (s.conversation_id.clone(), t.step_index),
                        time: t
                            .timestamp_utc
                            .map(|u| u.with_timezone(&Local).format("%m/%d %H:%M:%S").to_string())
                            .unwrap_or_else(|| "-".into()),
                        title: format!("{prefix}{}", self.format_session_title(s)),
                        is_child: s.is_subagent,
                        model: t.model.clone(),
                        tools: Self::call_tools_label(t, 3),
                        tools_col,
                        gen: t.gen_secs.map(|g| fmt_duration_short(g as u64)).unwrap_or_else(|| "-".into()),
                        tps: t.output_tps().map(|v| format!("{v:.0}")).unwrap_or_else(|| "-".into()),
                        total_tok: t.total_tokens(),
                        fresh_in: t.input_tokens + t.cache_write_tokens,
                        out: t.output_tokens,
                        hit: if t.is_checkpoint { "-".into() } else { format!("{rate:.1}%") },
                        hit_col,
                        cost: fmt_cost(&t.cost, 1, human),
                    }
                })
                .collect();

            let sel_detail = sel_idx.and_then(|i| rows.get(i)).map(|r| {
                let s = self.call_session(r);
                let t = &s.turns[r.turn_idx];
                let time = t
                    .timestamp_utc
                    .map(|u| u.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_else(|| "-".into());
                let kind = if t.is_checkpoint { " · 上下文压缩" } else { "" };
                let src_prefix = format_source_tag(s.source, show_multi)
                    .map(|tag| format!("[{tag}] "))
                    .unwrap_or_default();
                let hdr = format!("调用详情 · {src_prefix}{} · Step #{}{}", self.format_session_title(s), t.step_index, kind);
                let cost = if t.cost.is_fully_unpriced(1) {
                    "费用 - (未定价)".to_string()
                } else {
                    format!(
                        "费用 {} (入{} / 缓{} / 出{})",
                        fmt_usd_precise(t.cost.total_usd()),
                        fmt_usd_compact(t.cost.input_plus_write_usd()),
                        fmt_usd_compact(t.cost.cache_read_usd),
                        fmt_usd_compact(t.cost.output_usd),
                    )
                };
                let mut line_cost = format!("{} · {} · {}", time, t.model, cost);
                match (t.gen_secs, t.output_tps()) {
                    (Some(g), Some(v)) => {
                        line_cost.push_str(&format!(" · 生成 {} · {:.0} tok/s", fmt_duration_short(g as u64), v))
                    }
                    (Some(g), None) => line_cost.push_str(&format!(" · 生成 {}", fmt_duration_short(g as u64))),
                    (None, _) => {}
                }
                if let Some(g) = r.gap_secs.map(fmt_gap).filter(|g| !g.is_empty()) {
                    line_cost.push_str(&format!(" · 距上一步 {g}"));
                }
                if let Some(th) = format_turn_thinking(t) {
                    line_cost.push_str(&format!(" · 思考 {th}"));
                }
                let mut line_tok = format!(
                    "总 Token {} · 新输入 {} · 输出 {} · 缓存读 {} ({:.1}%)",
                    fmt_commas(t.total_tokens()),
                    fmt_commas(t.input_tokens),
                    fmt_commas(t.output_tokens),
                    fmt_commas(t.cache_read_tokens),
                    t.cache_hit_rate(),
                );
                if t.cache_write_tokens > 0 {
                    line_tok.push_str(&format!(" · 缓存写 {}", fmt_commas(t.cache_write_tokens)));
                }
                let ws = self.format_workspace_short(&s.workspace);
                if !ws.is_empty() {
                    line_tok.push_str(&format!(" · {ws}"));
                }
                let line_tools = if t.tool_names.is_empty() {
                    if t.is_checkpoint { "工具   : 无（上下文压缩步骤）".to_string() } else { "工具   : 无（纯回复 / 思考）".to_string() }
                } else {
                    format!("工具({}): {}", t.tool_names.len(), Self::call_tools_label(t, usize::MAX))
                };
                (s.conversation_id.clone(), hdr, line_cost, line_tok, line_tools)
            });

            (max_model_px, disp, sel_detail)
        });

        let model_w = (max_model_px + 8.0).clamp(96.0, (avail * 0.34).max(96.0).min(176.0));
        let flex = (avail - model_w - 12.0).max(80.0);
        let title_w = flex * 0.52;
        let model_x = title_x + title_w + 6.0;
        let tools_x = model_x + model_w + 6.0;
        let tools_w = (rx - metrics_w - 6.0 - tools_x).max(40.0);

        let hdr_y = top_c.top + 32.0;
        let hdr_h = 19.0;
        draw_text(rt, "时间", &fonts.ui_micro, rect_f(time_x, hdr_y, 100.0, hdr_h), theme.muted);
        draw_text(rt, "会话", &fonts.ui_micro, rect_f(title_x, hdr_y, title_w, hdr_h), theme.muted);
        draw_text(rt, "模型", &fonts.ui_micro, rect_f(model_x, hdr_y, model_w, hdr_h), theme.muted);
        draw_text(rt, "工具", &fonts.ui_micro, rect_f(tools_x, hdr_y, tools_w, hdr_h), theme.muted);
        for (i, (lbl, _, _)) in MCOLS.iter().enumerate() {
            draw_text(rt, lbl, &fonts.ui_small_right, mrect(i, hdr_y, hdr_h), theme.muted);
        }
        draw_line(rt, top_c.left + 14.0, top_c.top + 52.0, top_c.right - 14.0, top_c.top + 52.0, theme.border, 1.0);

        if disp.is_empty() {
            let empty_txt = if !self.search_query.trim().is_empty() || self.call_session_filter.is_some() {
                "无匹配调用 · 清空搜索或点击会话标签取消筛选"
            } else {
                "当前范围暂无调用记录 · 可在右上角切换 7天 / 30天 / 全部"
            };
            draw_text(
                rt,
                empty_txt,
                &fonts.ui_reg_center,
                rect_f(top_c.left, list_y, bw, (top_c.bottom - list_y).max(30.0)),
                theme.muted,
            );
        } else {
            for (vi, d) in disp.into_iter().enumerate() {
                let ry = list_y + vi as f32 * row_h;
                let rr = rect_f(rr_l, ry, rr_w, row_h - 2.0);
                let cell_h = row_h - 2.0;
                let sel = sel_idx == Some(scroll_rows + vi);
                let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&rr, mx, my)).unwrap_or(false);
                if sel {
                    fill_round_rect(rt, rr, 6.0, theme.hover);
                    fill_round_rect(rt, rect_f(rr.left, rr.top + 5.0, 2.5, rr.bottom - rr.top - 10.0), 1.25, theme.accent);
                } else if hov {
                    fill_round_rect(rt, rr, 6.0, with_alpha(theme.text, 0.04));
                }
                draw_text(rt, &d.time, &fonts.mono_small, rect_f(time_x, ry, 100.0, cell_h), theme.sub);
                draw_text(
                    rt,
                    &d.title,
                    &fonts.ui_reg,
                    rect_f(title_x, ry, title_w, cell_h),
                    if d.is_child { theme.sub } else { theme.text },
                );
                draw_text(rt, &d.model, &fonts.mono_small, rect_f(model_x, ry, model_w, cell_h), theme.muted);
                draw_text(rt, &d.tools, &fonts.mono_small, rect_f(tools_x, ry, tools_w, cell_h), d.tools_col);
                draw_text(rt, &d.gen, &fonts.mono_small_right, mrect(0, ry, cell_h), theme.muted);
                draw_text(rt, &d.tps, &fonts.mono_small_right, mrect(1, ry, cell_h), theme.sub);
                draw_text(
                    rt,
                    &fmt_num(d.total_tok, human),
                    if self.cost_mode { &fonts.mono_small_right } else { &fonts.mono_med_right },
                    mrect(2, ry, cell_h),
                    if self.cost_mode { theme.sub } else { theme.text },
                );
                let driver_col = if self.cost_mode { theme.sub } else { theme.text };
                draw_text(rt, &fmt_num(d.fresh_in, human), &fonts.mono_small_right, mrect(3, ry, cell_h), driver_col);
                draw_text(rt, &fmt_num(d.out, human), &fonts.mono_small_right, mrect(4, ry, cell_h), driver_col);
                draw_text(rt, &d.hit, &fonts.mono_small_right, mrect(5, ry, cell_h), d.hit_col);
                draw_text(
                    rt,
                    &d.cost,
                    if self.cost_mode { &fonts.mono_med_right } else { &fonts.mono_small_right },
                    mrect(6, ry, cell_h),
                    if self.cost_mode { theme.text } else { theme.output },
                );
                self.hit_boxes.push(HitBox {
                    rect: rr,
                    action: ClickAction::SelectCall(d.key.0, d.key.1),
                });
            }

            if self.max_scroll > 0 {
                let track_h = (top_c.bottom - 10.0 - list_y).max(20.0);
                let thumb_h = ((vis as f32 / total_rows as f32) * track_h).clamp(24.0, track_h);
                let ratio = scroll_rows as f32 / self.max_scroll as f32;
                let thumb_y = list_y + ratio * (track_h - thumb_h);
                fill_round_rect(rt, rect_f(top_c.right - 7.0, thumb_y, 3.0, thumb_h), 1.5, theme.muted);
            }
        }

        // ---- Bottom: selected call detail ----
        let bot_c = rect_f(body.left, top_c.bottom + 12.0, bw, (body.bottom - top_c.bottom - 12.0).max(80.0));
        fill_round_rect(rt, bot_c, 10.0, theme.surface);
        stroke_round_rect(rt, bot_c, 10.0, theme.border, 1.0);

        let Some((cid, hdr, line_cost, line_tok, line_tools)) = sel_detail else {
            draw_text(rt, "点击上方任意一次调用查看 Token、费用与工具明细", &fonts.ui_reg_center, bot_c, theme.muted);
            return;
        };

        let copied = self.call_copied_until.map(|t| Instant::now() < t).unwrap_or(false);
        let copy_r = rect_f(bot_c.right - 72.0, bot_c.top + 9.0, 58.0, 18.0);
        self.draw_ghost_btn(rt, fonts, theme, copy_r, if copied { "已复制" } else { "复制明细" }, copied, ClickAction::CopyCallDetail);
        let sess_r = rect_f(copy_r.left - 78.0, bot_c.top + 9.0, 72.0, 18.0);
        self.draw_ghost_btn(rt, fonts, theme, sess_r, "所属会话 ›", true, ClickAction::InspectSession(cid));

        let text_w = (sess_r.left - 8.0 - bot_c.left - 16.0).max(120.0);
        draw_text(rt, &hdr, &fonts.ui_section, rect_f(bot_c.left + 16.0, bot_c.top + 8.0, text_w, 20.0), theme.text);
        let full_w = (bw - 32.0).max(120.0);
        draw_text(rt, &line_cost, &fonts.mono_small, rect_f(bot_c.left + 16.0, bot_c.top + 32.0, full_w, 16.0), theme.sub);
        draw_text(rt, &line_tok, &fonts.mono_small, rect_f(bot_c.left + 16.0, bot_c.top + 50.0, full_w, 16.0), theme.sub);
        draw_text(rt, &line_tools, &fonts.mono_small, rect_f(bot_c.left + 16.0, bot_c.top + 68.0, full_w, 16.0), theme.muted);
    }

    // ---------- System Telemetry Studio (Opened via Hero CPU/RAM Ring Block) ----------

    #[allow(clippy::too_many_arguments)]
    fn draw_system_tab(
        &mut self,
        factories: &D2DFactories,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        body: D2D_RECT_F,
        snapshot: &ScanSnapshot,
        telemetry: &SystemTelemetry,
    ) {
        let bw = body.right - body.left;
        let bh = body.bottom - body.top;
        let footer_h = 38.0;
        let cards_h = 128.0;
        let chart_h = (bh - cards_h - footer_h - 20.0).clamp(160.0, 320.0);

        let chart_c = rect_f(body.left, body.top, bw, chart_h);
        fill_round_rect(rt, chart_c, 10.0, theme.surface);
        stroke_round_rect(rt, chart_c, 10.0, theme.border, 1.0);

        draw_text(rt, "实时系统负载 (90 秒采样 · 悬停查看历史时刻)", &fonts.ui_section, rect_f(chart_c.left + 16.0, chart_c.top + 11.0, 320.0, 20.0), theme.text);

        let leg_x = chart_c.right - 160.0;
        fill_circle(rt, leg_x, chart_c.top + 21.0, 3.0, theme.accent);
        draw_text(rt, "CPU", &fonts.ui_small, rect_f(leg_x + 8.0, chart_c.top + 13.0, 36.0, 16.0), theme.sub);
        fill_circle(rt, leg_x + 56.0, chart_c.top + 21.0, 3.0, theme.ram);
        draw_text(rt, "物理内存", &fonts.ui_small, rect_f(leg_x + 64.0, chart_c.top + 13.0, 64.0, 16.0), theme.sub);

        let pl = chart_c.left + 44.0;
        let pr = chart_c.right - 18.0;
        let pt_y = chart_c.top + 42.0;
        let pb_y = chart_c.bottom - 18.0;
        let pw = (pr - pl).max(40.0);
        let ph = (pb_y - pt_y).max(40.0);

        let mut fbuf = FmtBuf::new();
        for pct in [0, 50, 100] {
            let y = pb_y - (pct as f32 / 100.0) * ph;
            draw_line(rt, pl, y, pr, y, if pct == 0 { theme.border } else { theme.grid }, 1.0);
            fbuf.clear();
            let _ = write!(fbuf, "{pct}%");
            draw_text(rt, fbuf.as_str(), &fonts.mono_small_right, rect_f(chart_c.left + 4.0, y - 7.0, 34.0, 14.0), theme.muted);
        }

        if telemetry.history.len() >= 2 {
            let max_pts = 90usize;
            let n = telemetry.history.len();
            let dx = pw / (max_pts - 1) as f32;
            let x0 = pl + (max_pts.saturating_sub(n)) as f32 * dx;
            self.discrete_plots.push(DiscretePlot::TimeSeries {
                x0,
                pr,
                pt_y,
                pb_y,
                dx,
                n,
            });
            let mut cpu_pts = Vec::with_capacity(n);
            let mut mem_pts = Vec::with_capacity(n);
            for (i, s) in telemetry.history.iter().enumerate() {
                let x = x0 + i as f32 * dx;
                cpu_pts.push((x, pb_y - (s.cpu_pct.clamp(0.0, 100.0) / 100.0) * ph));
                mem_pts.push((x, pb_y - (s.mem_pct.clamp(0.0, 100.0) / 100.0) * ph));
            }
            draw_area_series(factories, rt, &mem_pts, pb_y, pt_y, theme.ram, 1.8, 0.12);
            draw_area_series(factories, rt, &cpu_pts, pb_y, pt_y, theme.accent, 2.0, 0.18);
            if let Some(&(lx, ly)) = mem_pts.last() {
                fill_circle(rt, lx, ly, 3.0, theme.ram);
            }
            if let Some(&(lx, ly)) = cpu_pts.last() {
                fill_circle(rt, lx, ly, 3.0, theme.accent);
            }

            if let Some((mx, my)) = self.mouse_dip {
                if mx >= x0 && mx <= pr && my >= pt_y && my <= pb_y {
                    let idx = (((mx - x0) / dx).round() as isize).clamp(0, (n - 1) as isize) as usize;
                    let s = telemetry.history[idx];
                    let cx = cpu_pts[idx].0;
                    draw_line(rt, cx, pt_y, cx, pb_y, theme.border, 1.0);
                    fill_circle(rt, cx, cpu_pts[idx].1, 4.0, theme.accent);
                    fill_circle(rt, cx, mem_pts[idx].1, 4.0, theme.ram);

                    let secs_ago = n - 1 - idx;
                    let tip = format!(
                        "{}  ·  CPU {:.1}%  ·  RAM {:.1}% ({:.1} GB)",
                        if secs_ago == 0 { "刚刚".into() } else { format!("-{}s", secs_ago) },
                        s.cpu_pct,
                        s.mem_pct,
                        s.mem_used_gb
                    );
                    let tw = 248.0;
                    let tx = (cx - tw * 0.5).clamp(pl, pr - tw);
                    let tr = rect_f(tx, pt_y + 4.0, tw, 24.0);
                    fill_round_rect(rt, tr, 6.0, theme.tooltip_bg);
                    stroke_round_rect(rt, tr, 6.0, theme.border, 1.0);
                    draw_text(rt, &tip, &fonts.mono_small_center, tr, theme.text);
                }
            }
        }

        let cy_top = chart_c.bottom + 12.0;
        let half_w = (bw - 12.0) * 0.5;
        let cur = telemetry.latest;

        let c_left = rect_f(body.left, cy_top, half_w, cards_h);
        fill_round_rect(rt, c_left, 10.0, theme.surface);
        stroke_round_rect(rt, c_left, 10.0, theme.border, 1.0);
        let g1_x = c_left.left + 58.0;
        let g1_y = cy_top + cards_h * 0.5;
        draw_ring_gauge(factories, rt, g1_x, g1_y, 34.0, 5.5, cur.cpu_pct, theme.grid, theme.accent);
        fbuf.clear();
        let _ = write!(fbuf, "{:.0}%", cur.cpu_pct);
        draw_text(rt, fbuf.as_str(), &fonts.mono_kpi_center, rect_f(g1_x - 32.0, g1_y - 12.0, 64.0, 24.0), load_color(theme, cur.cpu_pct, 60.0, 85.0));
        draw_text(rt, "处理器负载", &fonts.ui_section, rect_f(c_left.left + 114.0, cy_top + 26.0, half_w - 130.0, 20.0), theme.text);
        draw_text(rt, &format!("{} 个逻辑核心  ·  1 秒原生 Win32 采样", cur.cpu_cores), &fonts.ui_small, rect_f(c_left.left + 114.0, cy_top + 52.0, half_w - 130.0, 18.0), theme.sub);
        draw_text(rt, "直接调用 GetSystemTimes，无命令行子进程", &fonts.ui_micro, rect_f(c_left.left + 114.0, cy_top + 76.0, half_w - 130.0, 16.0), theme.muted);

        let c_right = rect_f(c_left.right + 12.0, cy_top, half_w, cards_h);
        fill_round_rect(rt, c_right, 10.0, theme.surface);
        stroke_round_rect(rt, c_right, 10.0, theme.border, 1.0);
        let g2_x = c_right.left + 58.0;
        let g2_y = cy_top + cards_h * 0.5;
        draw_ring_gauge(factories, rt, g2_x, g2_y, 34.0, 5.5, cur.mem_pct, theme.grid, theme.ram);
        fbuf.clear();
        let _ = write!(fbuf, "{:.0}%", cur.mem_pct);
        draw_text(rt, fbuf.as_str(), &fonts.mono_kpi_center, rect_f(g2_x - 32.0, g2_y - 12.0, 64.0, 24.0), load_color(theme, cur.mem_pct, 75.0, 88.0));
        draw_text(rt, "物理内存占用", &fonts.ui_section, rect_f(c_right.left + 114.0, cy_top + 26.0, half_w - 130.0, 20.0), theme.text);
        draw_text(
            rt,
            &format!("已用 {:.2} GB  /  总计 {:.2} GB  (可用 {:.2} GB)", cur.mem_used_gb, cur.mem_total_gb, cur.mem_avail_gb),
            &fonts.mono_small,
            rect_f(c_right.left + 114.0, cy_top + 52.0, half_w - 130.0, 18.0),
            theme.sub,
        );
        draw_text(
            rt,
            &format!("本进程内存 {:.1} MB · 隐藏自动释放画布并回收工作集", cur.self_mb),
            &fonts.ui_micro,
            rect_f(c_right.left + 114.0, cy_top + 76.0, half_w - 130.0, 16.0),
            theme.muted,
        );

        let pricing_note = if snapshot.custom_pricing_count > 0 {
            format!(" · 自定义单价 {} 项", snapshot.custom_pricing_count)
        } else {
            String::new()
        };
        let arch_note = if snapshot.archived_sessions_count > 0 {
            format!(" · 归档回填 {} 会话", snapshot.archived_sessions_count)
        } else {
            String::new()
        };
        let foot_line1 = format!(
            "严格只读 · 增量缓存 {}/{} ({:.1} ms){}{}",
            snapshot.files_cached,
            snapshot.files_cached + snapshot.files_reparsed,
            snapshot.scan_ms,
            pricing_note,
            arch_note,
        );
        let foot_line2 = format_active_sources_line(&snapshot.data_dirs, &snapshot.data_dir, self.privacy_mode);
        draw_text(
            rt,
            &foot_line1,
            &fonts.mono_small,
            rect_f(body.left + 4.0, c_left.bottom + 6.0, bw - 8.0, 16.0),
            theme.muted,
        );
        draw_text(
            rt,
            &foot_line2,
            &fonts.mono_small,
            rect_f(body.left + 4.0, c_left.bottom + 22.0, bw - 8.0, 16.0),
            theme.muted,
        );
    }

    // ---------- Minimalist Editorial Table (Virtualized Row Formatting) ----------

    #[allow(clippy::too_many_arguments)]
    fn draw_table_virtual<F>(
        &mut self,
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        bounds: D2D_RECT_F,
        cols: &[(&str, f32, bool)],
        total_rows: usize,
        secondary_from: Option<usize>,
        selected_row: Option<usize>,
        row_at: F,
    ) where
        F: FnMut(usize, &mut Vec<String>) -> Option<ClickAction>,
    {
        Self::draw_table_virtual_state(
            rt,
            fonts,
            theme,
            bounds,
            cols,
            total_rows,
            secondary_from,
            selected_row,
            &mut self.scroll_rows,
            &mut self.max_scroll,
            &mut self.hit_boxes,
            self.mouse_dip,
            self.cost_mode,
            row_at,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_table_virtual_state<F>(
        rt: &ID2D1RenderTarget,
        fonts: &DashFonts,
        theme: &Theme,
        bounds: D2D_RECT_F,
        cols: &[(&str, f32, bool)],
        total_rows: usize,
        secondary_from: Option<usize>,
        selected_row: Option<usize>,
        scroll_rows: &mut usize,
        max_scroll: &mut usize,
        hit_boxes: &mut Vec<HitBox>,
        mouse_dip: Option<(f32, f32)>,
        cost_mode: bool,
        mut row_at: F,
    ) where
        F: FnMut(usize, &mut Vec<String>) -> Option<ClickAction>,
    {
        fill_round_rect(rt, bounds, 10.0, theme.surface);
        stroke_round_rect(rt, bounds, 10.0, theme.border, 1.0);

        let pad_x = 16.0;
        let inner_w = (bounds.right - bounds.left - pad_x * 2.0 - 8.0).max(100.0);
        let hdr_h = 30.0;
        draw_line(rt, bounds.left + 12.0, bounds.top + hdr_h, bounds.right - 12.0, bounds.top + hdr_h, theme.border, 1.0);

        let mut cx = bounds.left + pad_x;
        for &(title, frac, right_align) in cols {
            let cw = inner_w * frac;
            draw_text(
                rt,
                title,
                if right_align { &fonts.ui_small_right } else { &fonts.ui_small },
                rect_f(cx, bounds.top, cw - 6.0, hdr_h),
                theme.muted,
            );
            cx += cw;
        }

        if total_rows == 0 {
            draw_text(rt, "暂无明细数据", &fonts.ui_reg_center, rect_f(bounds.left, bounds.top + hdr_h, bounds.right - bounds.left, bounds.bottom - bounds.top - hdr_h), theme.muted);
            *max_scroll = 0;
            return;
        }

        let row_h = 28.0;
        let body_top = bounds.top + hdr_h + 2.0;
        let vis_rows = (((bounds.bottom - 6.0 - body_top) / row_h).floor() as usize).max(1);
        *max_scroll = total_rows.saturating_sub(vis_rows);
        if *scroll_rows > *max_scroll {
            *scroll_rows = *max_scroll;
        }

        let has_cost_total_col = cols.iter().any(|(t, _, _)| *t == "估算总费");
        let end_idx = (*scroll_rows + vis_rows).min(total_rows);
        let mut cells_buf: Vec<String> = Vec::with_capacity(cols.len());
        for (vi, abs_idx) in (*scroll_rows..end_idx).enumerate() {
            cells_buf.clear();
            let maybe_act = row_at(abs_idx, &mut cells_buf);
            let row = &cells_buf;
            let is_secondary = secondary_from.map(|s| abs_idx >= s).unwrap_or(false);
            let is_sec_boundary = secondary_from == Some(abs_idx) && vi > 0;
            let is_selected = selected_row == Some(abs_idx);
            let ry = body_top + vi as f32 * row_h;
            let rr = rect_f(bounds.left + 8.0, ry, bounds.right - bounds.left - 20.0, row_h - 1.0);
            let hovered = mouse_dip.map(|(mx, my)| hit_rect(&rr, mx, my)).unwrap_or(false);
            if is_selected {
                fill_round_rect(rt, rr, 6.0, theme.hover);
                fill_round_rect(rt, rect_f(rr.left, rr.top + 5.0, 2.5, rr.bottom - rr.top - 10.0), 1.25, theme.accent);
            } else if hovered {
                fill_round_rect(rt, rr, 6.0, theme.hover);
            }
            if let Some(act) = maybe_act {
                hit_boxes.push(HitBox {
                    rect: rr,
                    action: act,
                });
            }
            if is_sec_boundary {
                draw_line(rt, bounds.left + 12.0, ry, bounds.right - 12.0, ry, theme.border, 1.0);
            } else if vi > 0 {
                draw_line(rt, bounds.left + 14.0, ry, bounds.right - 14.0, ry, theme.grid, 0.8);
            }

            let mut rx = bounds.left + pad_x;
            for (ci, &(col_title, frac, right_align)) in cols.iter().enumerate() {
                let cw = inner_w * frac;
                let cell = row.get(ci).map(String::as_str).unwrap_or("");
                let is_headline = !is_secondary
                    && right_align
                    && if cost_mode {
                        col_title == "估算总费" || (!has_cost_total_col && col_title == "估算费用")
                    } else {
                        col_title == "总 Token"
                    };
                let color = if col_title == "来源" {
                    if cell.starts_with("自定义") {
                        theme.accent
                    } else if cell == "未定价" {
                        theme.warn
                    } else {
                        theme.muted
                    }
                } else if is_secondary && !is_selected {
                    theme.muted
                } else if cell.starts_with('+') && cell.ends_with('%') {
                    theme.accent
                } else if cell.starts_with('-') && cell.ends_with('%') {
                    theme.sage
                } else if col_title == "命中率" || col_title == "稳定命中" {
                    match cell.trim_end_matches('%').parse::<f64>() {
                        Ok(v) if cell.ends_with('%') => hit_rate_color(theme, v),
                        _ => theme.muted,
                    }
                } else if col_title == "未命中" {
                    if cell == "-" { theme.muted } else { theme.warn }
                } else if col_title == "缓存已省" {
                    theme.sage
                } else if is_headline {
                    theme.text
                } else if col_title == "估算费用" || col_title == "输出 $/M" {
                    theme.output
                } else if matches!(
                    col_title,
                    "缓存读" | "缓存读取" | "缓存费用" | "峰值Ctx" | "调用" | "活跃" | "星期"
                ) {
                    theme.muted
                } else if ci == 0
                    || (!cost_mode && matches!(col_title, "新输入" | "新增输入" | "输出"))
                    || (cost_mode && matches!(col_title, "新入费用" | "输出费用"))
                {
                    theme.text
                } else {
                    theme.sub
                };
                let font = if is_headline {
                    &fonts.mono_med_right
                } else if right_align {
                    &fonts.mono_small_right
                } else {
                    &fonts.mono_small
                };
                draw_text(
                    rt,
                    cell,
                    font,
                    rect_f(rx, ry, cw - 6.0, row_h),
                    color,
                );
                rx += cw;
            }
        }

        if *max_scroll > 0 {
            let track_h = (bounds.bottom - 8.0 - body_top).max(20.0);
            let thumb_h = ((vis_rows as f32 / total_rows as f32) * track_h).clamp(24.0, track_h);
            let ratio = *scroll_rows as f32 / *max_scroll as f32;
            let thumb_y = body_top + ratio * (track_h - thumb_h);
            fill_round_rect(rt, rect_f(bounds.right - 7.0, thumb_y, 3.0, thumb_h), 1.5, theme.muted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::CostBreakdown;
    use crate::stats::TurnRecord;
    use std::collections::BTreeSet;

    fn make_session(id: &str, ws: &str, model: &str, ts_ymd: (i32, u32, u32), calls: u64) -> SessionInfo {
        let ts = Utc.with_ymd_and_hms(ts_ymd.0, ts_ymd.1, ts_ymd.2, 10, 0, 0).single();
        let mut counts = TokenCounts::default();
        let mut turns = Vec::new();
        let model_arc: Arc<str> = Arc::from(model);
        for i in 0..calls {
            counts.add_call(1000, 9000, 0, 500);
            turns.push(TurnRecord {
                step_index: i as i64,
                timestamp_utc: ts,
                model: Arc::clone(&model_arc),
                is_checkpoint: false,
                input_tokens: 1000,
                cache_read_tokens: 9000,
                cache_write_tokens: 0,
                cache_write_1h_tokens: 0,
                output_tokens: 500,
                reasoning_tokens: 0,
                tool_names: Box::new([]),
                thinking_chars: 0,
                gen_secs: None,
                cost: CostBreakdown::default(),
            });
        }
        let mut models = BTreeSet::new();
        models.insert(model.to_string());
        let primary_model = compute_primary_model(&turns);
        let active_duration_secs = compute_active_duration_secs(&turns);
        SessionInfo {
            conversation_id: id.to_string(),
            title: id.to_string(),
            workspace: ws.to_string(),
            source: crate::stats::SourceKind::Cli,
            is_archived: false,
            is_subagent: false,
            parent_id: None,
            parent_title: String::new(),
            turns: Arc::from(turns),
            counts,
            models,
            first_time: ts.map(|u| u.with_timezone(&Local)),
            last_time: ts.map(|u| u.with_timezone(&Local)),
            last_file_mtime_utc: ts,
            is_running_hint: false,
            primary_model,
            active_duration_secs,
            compaction_count: 0,
            thinking_turns: 0,
        }
    }

    fn mutate_turns(s: &mut SessionInfo, f: impl FnOnce(&mut Vec<TurnRecord>)) {
        let mut turns = s.turns.to_vec();
        f(&mut turns);
        s.turns = Arc::from(turns);
        s.recompute_summary();
    }

    fn tools_box(names: &[&str]) -> Box<[Arc<str>]> {
        names.iter().map(|s| Arc::from(*s)).collect::<Vec<_>>().into_boxed_slice()
    }

    #[test]
    fn test_session_filter_and_keyboard_step() {
        let s1 = make_session("s1", "C:/Users/a/agent-home", "gemini-4-neon-xhigh", (2026, 11, 21), 5);
        let s2 = make_session("s2", "C:/Users/a/subvault", "gemini-4-neon-high", (2026, 11, 20), 2);

        assert!(SessionFilter::Workspace("C:/Users/a/agent-home".into()).matches(&s1));
        assert!(!SessionFilter::Workspace("C:/Users/a/agent-home".into()).matches(&s2));
        assert!(SessionFilter::Model("gemini-4-neon-high".into()).matches(&s2));
        assert!(SessionFilter::Week("2026-W47".into()).matches(&s1));

        let mut view = DashboardView::new(false, DateScope::All, false, true, false);
        view.active_tab = Tab::Sessions;
        view.cached_sessions = vec![s1, s2];
        view.selected_session = Some("s1".into());
        assert!(view.step_session(1));
        assert_eq!(view.selected_session.as_deref(), Some("s2"));
        assert!(!view.step_session(1)); // clamped at end
        assert!(view.step_session(-1));
        assert_eq!(view.selected_session.as_deref(), Some("s1"));
    }

    #[test]
    fn test_subagent_grouping_and_model_drilldown() {
        let parent = make_session("p1", "C:/Users/a/agent-home", "gemini-4-neon-xhigh", (2026, 11, 22), 3);
        let mut child = make_session("c1", "C:/Users/a/agent-home", "gemini-4-hydrogen-tiered-xhigh", (2026, 11, 22), 2);
        child.is_subagent = true;
        child.parent_id = Some("p1".into());
        child.parent_title = "p1".into();

        let mut view = DashboardView::new(false, DateScope::All, false, true, false);
        view.cached_sessions = vec![parent, child];

        // Grouped mode rolls child into parent (1 row, 5 calls total, subagent_count=1)
        let grouped = view.display_sessions(true);
        assert_eq!(grouped.len(), 1);
        assert_eq!(view.row_session(&grouped[0]).conversation_id, "p1");
        assert_eq!(grouped[0].subagent_count, 1);
        assert_eq!(grouped[0].counts.calls, 5);

        // Filtering by subagent-only model reveals the matching subagent directly
        view.session_filter = Some(SessionFilter::Model("gemini-4-hydrogen-tiered-xhigh".into()));
        let by_model = view.display_sessions(true);
        assert_eq!(by_model.len(), 1);
        assert_eq!(view.row_session(&by_model[0]).conversation_id, "c1");
        assert!(by_model[0].is_child_row);

        // Parent drill-down expands parent first, then its subagent
        view.session_filter = Some(SessionFilter::Parent("p1".into(), "p1".into()));
        let expanded = view.display_sessions(true);
        assert_eq!(expanded.len(), 2);
        assert_eq!(view.row_session(&expanded[0]).conversation_id, "p1");
        assert!(!expanded[0].is_child_row);
        assert_eq!(view.row_session(&expanded[1]).conversation_id, "c1");
        assert!(expanded[1].is_child_row);
    }

    #[test]
    fn test_model_and_date_filters_count_only_matching_turns() {
        // One session: 3 turns on neon (11/22) + 2 turns on opus (11/21)
        let mut s = make_session("mix", "C:/Users/a/agent-home", "gemini-4-neon-xhigh", (2026, 11, 22), 5);
        mutate_turns(&mut s, |turns| {
            for t in turns.iter_mut().skip(3) {
                t.model = Arc::from("claude-opus-5-5");
                t.output_tokens = 2_000;
                t.timestamp_utc = Utc.with_ymd_and_hms(2026, 11, 21, 10, 0, 0).single();
            }
        });
        s.models.insert("claude-opus-5-5".into());
        let mut view = DashboardView::new(false, DateScope::All, false, true, false);
        view.cached_sessions = vec![s];
        view.selected_session = Some("mix".into());

        view.session_filter = Some(SessionFilter::Model("claude-opus-5-5".into()));
        let rows = view.display_sessions(true);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].counts.calls, 2);
        assert_eq!(rows[0].counts.output_tokens, 4_000);
        assert_eq!(rows[0].primary_model.as_ref(), "claude-opus-5-5");
        let detail = view.session_for_detail("mix").unwrap();
        assert_eq!(detail.turns.len(), 2);
        assert!(view.export_sessions_csv().contains(",2,"));

        view.session_filter = Some(SessionFilter::Date("2026-11-22".into()));
        let rows = view.display_sessions(true);
        assert_eq!(rows[0].counts.calls, 3);
        assert_eq!(rows[0].primary_model.as_ref(), "gemini-4-neon-xhigh");

        // Whole-session filters keep every turn
        view.session_filter = Some(SessionFilter::Workspace("C:/Users/a/agent-home".into()));
        assert_eq!(view.display_sessions(true)[0].counts.calls, 5);
        assert_eq!(view.session_for_detail("mix").unwrap().turns.len(), 5);
    }

    #[test]
    fn test_date_scope_roundtrip() {
        for sc in [
            DateScope::Today,
            DateScope::Days7,
            DateScope::Days30,
            DateScope::All,
            DateScope::Custom,
        ] {
            assert_eq!(DateScope::from_u8(sc.to_u8()), sc);
        }
        assert_eq!(DateScope::from_u8(99), DateScope::Today);
    }

    #[test]
    fn test_session_search_and_exports_respect_privacy_mode() {
        let mut s1 = make_session(
            "secret-uuid-1111",
            "C:/Users/alice/secret-client",
            "gemini-4-neon-xhigh",
            (2026, 11, 22),
            2,
        );
        s1.title = "机密支付网关排查".into();
        mutate_turns(&mut s1, |turns| {
            turns[0].tool_names = tools_box(&["read_file", "run_command"]);
            turns[0].thinking_chars = 420;
        });

        let mut s2 = make_session(
            "public-uuid-2222",
            "C:/Users/alice/notes",
            "claude-opus-5-5",
            (2026, 11, 21),
            1,
        );
        s2.title = "普通周报整理".into();

        let sessions = vec![s1.clone(), s2.clone()];
        let agg = crate::stats::aggregate_sessions(&sessions);
        let redactor = Redactor::build(&sessions, &agg);

        let mut view = DashboardView::new(false, DateScope::All, false, true, true);
        view.cached_sessions = sessions;
        view.cached_agg = agg.clone();
        view.set_redactor_for_test(redactor);

        // Under privacy_mode, raw secret title/workspace cannot be queried (no oracle leak)
        view.search_query = "机密支付".into();
        assert_eq!(view.display_sessions(true).len(), 0);
        view.search_query = "secret-client".into();
        assert_eq!(view.display_sessions(true).len(), 0);
        // Redacted label, model name, and tool name still match cleanly
        view.search_query = "run_command".into();
        assert_eq!(view.display_sessions(true).len(), 1);
        view.search_query = "项目 A".into();
        assert_eq!(view.display_sessions(true).len(), 1);
        view.search_query.clear();

        // CSV & JSON exports under privacy_mode must never leak raw title, UUID, or username
        let csv = view.export_sessions_csv();
        assert!(csv.starts_with('\u{FEFF}'));
        assert!(csv.contains("S-001"));
        assert!(csv.contains("项目 A"));
        assert!(!csv.contains("机密支付"));
        assert!(!csv.contains("secret-uuid"));
        assert!(!csv.contains("alice"));

        let snap = ScanSnapshot {
            data_dir: std::path::PathBuf::from(r"C:\Users\alice\.gemini\antigravity-cli"),
            data_dirs: Vec::new(),
            sessions: view.cached_sessions.clone(),
            agg,
            statusline: None,
            statusline_changed: false,
            budget: BudgetConfig::default(),
            budget_changed: false,
            budget_path: None,
            session_meta: HashMap::new(),
            session_meta_changed: false,
            session_meta_path: None,
            archived_sessions_count: 1,
            archive_enabled: true,
            archive_path: None,
            model_rate_labels: HashMap::new(),
            pricing_rows: Vec::new(),
            pricing_path: None,
            custom_pricing_count: 0,
            pricing_changed: false,
            skipped_empty: 0,
            skipped_lines: 0,
            files_cached: 0,
            files_reparsed: 0,
            scan_ms: 1.0,
            warning: None,
        };
        let daily_csv = view.export_daily_csv(&snap);
        assert!(daily_csv.starts_with("\u{FEFF}date,calls"));
        assert!(daily_csv.contains("2026-11-22"));
        let js = view.export_snapshot_json(&snap);
        let parsed: serde_json::Value = serde_json::from_str(&js).expect("valid export json");
        assert_eq!(parsed["redacted"], serde_json::Value::Bool(true));
        assert_eq!(parsed["archived_sessions_count"], serde_json::json!(1));
        assert!(!js.contains("机密支付"));
        assert!(!js.contains("secret-client"));
        assert!(!js.contains("alice"));

        // Turning off privacy_mode allows searching raw titles again
        view.privacy_mode = false;
        view.search_query = "机密支付".into();
        assert_eq!(view.display_sessions(true).len(), 1);
    }

    #[test]
    fn test_call_log_order_gap_search_and_privacy_export() {
        let mut s1 = make_session("secret-uuid-1111", "C:/Users/alice/secret-client", "gemini-4-neon-xhigh", (2026, 11, 22), 3);
        s1.title = "机密支付网关排查".into();
        mutate_turns(&mut s1, |turns| {
            for (i, t) in turns.iter_mut().enumerate() {
                t.timestamp_utc = Utc.with_ymd_and_hms(2026, 11, 22, 10, 0, (i as u32) * 20).single();
            }
            turns[1].tool_names = tools_box(&["run_command", "read_file"]);
            turns[2].output_tokens = 9_000;
        });
        let mut s2 = make_session("public-uuid-2222", "C:/Users/alice/notes", "claude-opus-5-5", (2026, 11, 21), 1);
        s2.title = "普通周报整理".into();

        let sessions = vec![s1, s2];
        let agg = crate::stats::aggregate_sessions(&sessions);
        let mut view = DashboardView::new(false, DateScope::All, false, true, false);
        view.set_redactor_for_test(Redactor::build(&sessions, &agg));
        view.cached_sessions = sessions;
        view.cached_agg = agg;
        view.active_tab = Tab::Calls;

        // Newest first across sessions; gap measured within the same session only
        let rows = view.call_rows();
        assert_eq!(rows.len(), 4);
        assert_eq!(view.call_key(&rows[0]), ("secret-uuid-1111".to_string(), 2));
        assert_eq!(rows[0].gap_secs, Some(20));
        assert_eq!(view.call_key(&rows[3]).0, "public-uuid-2222");
        assert_eq!(rows[3].gap_secs, None);

        // Total & Output sorts surface the heavy step; search matches per-call tool names
        view.call_sort = CallSort::Total;
        assert_eq!(view.call_key(&view.call_rows()[0]), ("secret-uuid-1111".to_string(), 2));
        view.call_sort = CallSort::Output;
        assert_eq!(view.call_key(&view.call_rows()[0]), ("secret-uuid-1111".to_string(), 2));
        view.call_sort = CallSort::Recent;
        view.search_query = "run_command".into();
        let hits = view.call_rows();
        assert_eq!(hits.len(), 1);
        assert_eq!(view.call_key(&hits[0]), ("secret-uuid-1111".to_string(), 1));
        view.search_query.clear();

        // Keyboard stepping and single-session drill-down
        view.selected_call = None;
        assert!(view.step_call(1));
        assert_eq!(view.selected_call, Some(("secret-uuid-1111".to_string(), 1)));
        view.call_session_filter = Some("public-uuid-2222".into());
        assert_eq!(view.call_rows().len(), 1);
        view.call_session_filter = None;

        // Privacy mode: raw titles are not searchable and never exported
        view.privacy_mode = true;
        view.search_query = "机密支付".into();
        assert!(view.call_rows().is_empty());
        view.search_query.clear();
        let csv = view.export_calls_csv();
        assert!(csv.starts_with("\u{FEFF}time,session_id"));
        assert!(csv.lines().next().unwrap().contains(",total_tokens,context_tokens,"));
        assert_eq!(csv.lines().count(), 5);
        assert!(csv.contains("run_command;read_file"));
        assert!(csv.contains("S-00"));
        assert!(!csv.contains("机密支付"));
        assert!(!csv.contains("secret-uuid"));
        assert!(!csv.contains("alice"));

        let s = &view.cached_sessions[0];
        let clip = view.format_call_detail_clipboard(s, &s.turns[1], Some(20));
        assert!(clip.contains("Step #1"));
        assert!(clip.contains("总 Token: 10,500"));
        assert!(clip.contains("run_command, read_file"));
        assert!(!clip.contains("机密支付"));

        let mut t = view.cached_sessions[0].turns[1].clone();
        t.tool_names = tools_box(&["edit_file", "edit_file", "read_file", "edit_file"]);
        assert_eq!(DashboardView::call_tools_label(&t, 3), "edit_file×3, read_file");
        assert_eq!(DashboardView::call_tools_label(&t, 1), "edit_file×3 +1");
    }

    #[test]
    fn test_smart_hover_repaint_gating() {
        let mut view = DashboardView::new(false, DateScope::Today, false, true, false);
        view.hit_boxes.push(HitBox {
            rect: rect_f(10.0, 10.0, 50.0, 20.0),
            action: ClickAction::Refresh,
        });
        view.plot_rects.push(rect_f(100.0, 100.0, 200.0, 100.0));

        // Moving over blank space does not repaint
        assert!(!view.on_mouse_move(2.0, 2.0));
        assert!(!view.on_mouse_move(5.0, 5.0));
        // Entering hitbox repaints once; sliding inside same hitbox does NOT repaint
        assert!(view.on_mouse_move(20.0, 15.0));
        assert!(!view.on_mouse_move(30.0, 15.0));
        // Leaving hitbox repaints once
        assert!(view.on_mouse_move(2.0, 2.0));
        assert!(!view.on_mouse_move(3.0, 3.0));
        // Moving inside chart plot always repaints for smooth tooltips/crosshair
        assert!(view.on_mouse_move(150.0, 150.0));
        assert!(view.on_mouse_move(160.0, 150.0));
        // Leaving chart plot repaints once to clear tooltip
        assert!(view.on_mouse_move(5.0, 5.0));
        assert!(!view.on_mouse_leave());
    }

    #[test]
    fn test_ordered_models_pins_compaction_to_secondary() {
        let mut view = DashboardView::new(false, DateScope::All, false, true, false);
        let mut hi = TokenCounts::default();
        hi.add_call_with_cost(
            1_000,
            9_000,
            0,
            200,
            CostBreakdown {
                input_usd: 0.01,
                output_usd: 0.02,
                ..Default::default()
            },
        ); // 90% hit, 10.2K total, $0.03
        let mut lo = TokenCounts::default();
        lo.add_call_with_cost(
            4_000,
            1_000,
            0,
            200,
            CostBreakdown {
                input_usd: 0.20,
                output_usd: 0.10,
                ..Default::default()
            },
        ); // 20% hit, 5.2K total, $0.30
        let mut cp = TokenCounts::default();
        cp.add_call_with_cost(
            1_000,
            99_000,
            0,
            500,
            CostBreakdown {
                input_usd: 0.50,
                output_usd: 0.50,
                ..Default::default()
            },
        ); // 99% hit, 100.5K total, $1.00 (compaction!)

        view.cached_agg.by_model.insert("model-hi".into(), hi);
        view.cached_agg.by_model.insert("model-lo".into(), lo);
        view.cached_agg.by_model.insert("(上下文压缩)".into(), cp);

        for sort in [ModelSort::TotalDesc, ModelSort::CostDesc, ModelSort::HitRateAsc, ModelSort::HitRateDesc] {
            let (pri, sec) = view.ordered_models(sort);
            assert_eq!(pri.len(), 2);
            assert_eq!(sec.len(), 1);
            assert_eq!(sec[0].0, "(上下文压缩)");
            if matches!(sort, ModelSort::HitRateAsc | ModelSort::CostDesc) {
                assert_eq!(pri[0].0, "model-lo");
                assert_eq!(pri[1].0, "model-hi");
            } else {
                assert_eq!(pri[0].0, "model-hi");
                assert_eq!(pri[1].0, "model-lo");
            }
        }
    }

    #[test]
    fn test_privacy_redactor_filters_paths_and_clipboard() {
        let mut parent = make_session(
            "a1b2c3d4-0000-0000-0000-000000000001",
            "C:/Users/alice/secret-client",
            "gemini-4-neon-xhigh",
            (2026, 11, 22),
            5,
        );
        parent.title = "修复 SecretProject 登录秘钥".into();

        let mut child = make_session(
            "e5f60708-0000-0000-0000-000000000002",
            "C:/Users/alice/secret-client",
            "gemini-4-hydrogen-tiered-xhigh",
            (2026, 11, 22),
            2,
        );
        child.title = "子任务分析 alice 内部文档".into();
        child.is_subagent = true;
        child.parent_id = Some(parent.conversation_id.clone());
        child.parent_title = parent.title.clone();

        let mut other = make_session(
            "99998888-0000-0000-0000-000000000003",
            "C:/Users/alice/hobby-blog",
            "claude-opus-5-5",
            (2026, 11, 21),
            1,
        );
        other.title = "个人博客草稿".into();

        let mut orphan = make_session(
            "77776666-0000-0000-0000-000000000004",
            "(未知工作区)",
            "gemini-4-carbon-high",
            (2026, 11, 20),
            1,
        );
        orphan.title = "孤立子代理提示词".into();
        orphan.is_subagent = true;
        orphan.parent_id = Some("deadbeef-0000-0000-0000-000000000099".into());
        orphan.parent_title = "未加载的涉密父会话".into();

        let sessions = vec![parent.clone(), child.clone(), other.clone(), orphan.clone()];
        let agg = crate::stats::aggregate_sessions(&sessions);
        let redactor = Redactor::build(&sessions, &agg);

        // Workspaces ranked by token volume desc: secret-client (7 calls) -> 项目 A, hobby-blog (1 call) -> 项目 B
        assert_eq!(redactor.workspace_label("C:/Users/alice/secret-client"), "项目 A");
        assert_eq!(redactor.workspace_label("C:/Users/alice/hobby-blog"), "项目 B");
        assert_eq!(redactor.workspace_label("(未知工作区)"), "(未知工作区)");
        assert_eq!(redactor.workspace_short("(未知工作区)"), "");

        // Session IDs and titles preserve parent-child linkage without leaking raw values
        assert_eq!(redactor.session_id(&parent.conversation_id), "S-001");
        assert_eq!(redactor.session_title(&parent), "会话 #1");
        assert_eq!(redactor.session_id(&child.conversation_id), "S-001.1");
        assert_eq!(redactor.session_title(&child), "子任务 #1.1");
        assert_eq!(redactor.parent_title(&parent.conversation_id), "会话 #1");
        assert_eq!(redactor.session_id(&other.conversation_id), "S-002");
        assert_eq!(redactor.session_title(&other), "会话 #2");
        assert_eq!(redactor.session_id(&orphan.conversation_id), "S-003");
        assert_eq!(redactor.session_title(&orphan), "子会话 #3");
        assert_eq!(
            redactor.parent_title("deadbeef-0000-0000-0000-000000000099"),
            "父任务 #1"
        );

        // Alpha label rollover past Z -> AA, AB
        assert_eq!(alpha_label(0), "A");
        assert_eq!(alpha_label(25), "Z");
        assert_eq!(alpha_label(26), "AA");
        assert_eq!(alpha_label(27), "AB");

        // Filter chip labels sanitize workspace & parent title while keeping model names intact
        let f_ws = SessionFilter::Workspace("C:/Users/alice/secret-client".into());
        assert_eq!(f_ws.label(Some(&redactor)), "项目: 项目 A  ×");
        assert_eq!(f_ws.label(None), "项目: secret-client  ×");
        let f_parent = SessionFilter::Parent(parent.conversation_id.clone(), parent.title.clone());
        assert_eq!(f_parent.label(Some(&redactor)), "任务组: 会话 #1  ×");
        let f_model = SessionFilter::Model("gemini-4-neon-xhigh".into());
        assert_eq!(f_model.label(Some(&redactor)), "模型: gemini-4-neon-xhigh  ×");

        // Clipboard output under privacy mode strips title, UUID, workspace, username while preserving model/tokens/cost
        let mut view = DashboardView::new(false, DateScope::All, false, true, true);
        view.set_redactor_for_test(redactor);
        let clip_redacted = view.format_session_detail_clipboard(&parent);
        assert!(clip_redacted.contains("会话 #1"));
        assert!(clip_redacted.contains("S-001"));
        assert!(clip_redacted.contains("项目: 项目 A"));
        assert!(clip_redacted.contains("gemini-4-neon-xhigh"));
        assert!(!clip_redacted.contains("SecretProject"));
        assert!(!clip_redacted.contains("a1b2c3d4"));
        assert!(!clip_redacted.contains("secret-client"));
        assert!(!clip_redacted.contains("alice"));

        // Toggling privacy mode off restores real details and marks prefs dirty
        let mut human = true;
        let mut light = false;
        let (_, prefs_changed) = view.apply_action(HWND::default(), ClickAction::TogglePrivacyMode, &mut human, &mut light);
        assert!(prefs_changed);
        assert!(!view.privacy_mode);
        let clip_raw = view.format_session_detail_clipboard(&parent);
        assert!(clip_raw.contains("SecretProject"));
        assert!(clip_raw.contains("a1b2c3d4"));
        assert!(clip_raw.contains("secret-client"));

        // Path redaction strips Windows usernames & custom directories
        assert_eq!(
            redact_data_dir(Path::new(r"C:\Users\alice\.gemini\antigravity-cli")),
            "~/.gemini/antigravity-cli"
        );
        assert_eq!(
            redact_data_dir(Path::new("/home/alice/.gemini/antigravity-cli/")),
            "~/.gemini/antigravity-cli"
        );
        assert_eq!(
            redact_data_dir(Path::new(r"D:\Work\PrivateClient\agy-data")),
            "<已脱敏目录>"
        );
        assert_eq!(
            redact_pricing_path(Some(Path::new(r"C:\Users\alice\gauge\pricing.json"))),
            "./pricing.json"
        );
        assert_eq!(redact_pricing_path(None), "./pricing.json");
    }

    #[test]
    fn test_cross_scope_pinned_session_isolation_and_couplings() {
        use crate::session_meta::SessionUserMeta;
        use crate::stats::aggregate_sessions;
        use std::path::PathBuf;

        let today = Local::now().date_naive();
        let old_day = today.checked_sub_days(chrono::Days::new(10)).unwrap();

        let s_today = make_session(
            "today-sess-0001",
            "C:/Users/alice/repo-today",
            "gemini-4-neon-xhigh",
            (today.year(), today.month(), today.day()),
            2,
        );
        let s_old_pinned = make_session(
            "old-pinned-0002",
            "C:/Users/alice/repo-pinned-old",
            "claude-opus-4-6",
            (old_day.year(), old_day.month(), old_day.day()),
            4,
        );
        let s_old_unpinned = make_session(
            "old-plain-0003",
            "C:/Users/alice/repo-other",
            "claude-sonnet-4-5",
            (old_day.year(), old_day.month(), old_day.day()),
            3,
        );

        let all_sessions = vec![s_today.clone(), s_old_pinned.clone(), s_old_unpinned.clone()];
        let all_agg = aggregate_sessions(&all_sessions);
        let mut session_meta = HashMap::new();
        session_meta.insert(
            s_old_pinned.conversation_id.clone(),
            SessionUserMeta { pinned: true, updated_at: None },
        );

        let snap = ScanSnapshot {
            data_dir: PathBuf::from("."),
            data_dirs: Vec::new(),
            sessions: all_sessions,
            agg: all_agg,
            statusline: None,
            statusline_changed: false,
            budget: BudgetConfig::default(),
            budget_changed: false,
            budget_path: None,
            session_meta,
            session_meta_changed: false,
            session_meta_path: None,
            archived_sessions_count: 0,
            archive_enabled: false,
            archive_path: None,
            model_rate_labels: HashMap::new(),
            pricing_rows: Vec::new(),
            pricing_path: None,
            custom_pricing_count: 0,
            pricing_changed: false,
            skipped_empty: 0,
            skipped_lines: 0,
            files_cached: 0,
            files_reparsed: 0,
            scan_ms: 0.0,
            warning: None,
        };

        let mut view = DashboardView::new(false, DateScope::Today, false, true, false);
        view.active_tab = Tab::Sessions;
        view.ensure_filtered(&snap);

        // 1) Hero aggregates and in-scope session count MUST NOT be polluted by out-of-scope pin
        assert_eq!(view.cached_sessions.len(), 1);
        assert_eq!(view.cached_agg.total.calls, 2);
        assert_eq!(view.pinned_out_of_scope.len(), 1);

        // 2) Sessions tab list shows pinned session FIRST, followed by in-scope session; unpinned old stays excluded
        let tab_rows = view.display_sessions_for_tab();
        assert_eq!(tab_rows.len(), 2);
        assert_eq!(view.row_session(&tab_rows[0]).conversation_id, "old-pinned-0002");
        assert!(tab_rows[0].is_pinned);
        assert!(tab_rows[0].out_of_scope);
        assert_eq!(view.row_session(&tab_rows[1]).conversation_id, "today-sess-0001");
        assert!(!tab_rows[1].is_pinned);
        assert!(!tab_rows[1].out_of_scope);

        // 3) Exports exclude out-of-scope pinned rows so totals match overview exactly
        let csv = view.export_sessions_csv();
        assert!(csv.contains("today-se"));
        assert!(!csv.contains("old-pinn"));
        let json_out = view.export_snapshot_json(&snap);
        assert!(json_out.contains("today-se"));
        assert!(!json_out.contains("old-pinn"));

        // 4) Redactor covers out-of-scope pinned session (never `S-???` / `项目 ?`)
        view.privacy_mode = true;
        let red_id = view.format_session_id("old-pinned-0002");
        let red_title = view.format_session_title(&s_old_pinned);
        let red_ws = view.format_workspace_label(&s_old_pinned.workspace);
        assert_ne!(red_id, "S-???");
        assert!(red_id.starts_with("S-"));
        assert!(red_title.starts_with("会话 #"));
        assert!(!red_ws.contains('?'));
        assert!(red_ws.starts_with("项目 "));
        view.privacy_mode = false;

        // 5) session_for_detail, step_session, and ShowSessionCalls / call_rows all work on out-of-scope pin
        assert!(view.session_for_detail("old-pinned-0002").is_some());
        view.selected_session = Some("old-pinned-0002".into());
        assert!(view.step_session(1));
        assert_eq!(view.selected_session.as_deref(), Some("today-sess-0001"));
        assert!(view.step_session(-1));
        assert_eq!(view.selected_session.as_deref(), Some("old-pinned-0002"));

        // Default unfiltered call_rows only includes in-scope calls (2)
        assert_eq!(view.call_rows().len(), 2);
        // Drilling into the out-of-scope pinned session's calls reveals its 4 turns
        view.call_session_filter = Some("old-pinned-0002".into());
        assert_eq!(view.call_rows().len(), 4);

        // 6) Dangling/pruned pinned ID in session_meta must NOT inflate pinned_ids count
        let mut snap_dangling = snap.clone();
        snap_dangling.session_meta.insert(
            "pruned-ghost-9999".into(),
            SessionUserMeta { pinned: true, updated_at: None },
        );
        view.mark_data_dirty();
        view.ensure_filtered(&snap_dangling);
        assert_eq!(view.pinned_ids.len(), 1);
        assert!(!view.pinned_ids.contains("pruned-ghost-9999"));

        // 7) InspectSession / FilterSessions clear `pinned_only` so unpinned targets stay visible;
        //    ClearSessionFilter clears `pinned_only` when no drill-down filter is active.
        let mut human = true;
        let mut light = false;
        view.pinned_only = true;
        let _ = view.apply_action(
            HWND::default(),
            ClickAction::InspectSession("today-sess-0001".into()),
            &mut human,
            &mut light,
        );
        assert!(!view.pinned_only);
        assert_eq!(view.selected_session.as_deref(), Some("today-sess-0001"));

        view.pinned_only = true;
        let _ = view.apply_action(
            HWND::default(),
            ClickAction::FilterSessions(SessionFilter::Model("gemini-4-neon-xhigh".into())),
            &mut human,
            &mut light,
        );
        assert!(!view.pinned_only);
        assert_eq!(view.selected_session.as_deref(), Some("today-sess-0001"));

        // First ClearSessionFilter clears session_filter; second clears pinned_only
        view.pinned_only = true;
        let _ = view.apply_action(HWND::default(), ClickAction::ClearSessionFilter, &mut human, &mut light);
        assert!(view.session_filter.is_none());
        assert!(view.pinned_only);
        let _ = view.apply_action(HWND::default(), ClickAction::ClearSessionFilter, &mut human, &mut light);
        assert!(!view.pinned_only);
    }

    #[test]
    fn test_fmt_relative_time_cn() {
        let now = Local.with_ymd_and_hms(2026, 11, 24, 14, 30, 0).single().unwrap();
        assert_eq!(fmt_relative_time_cn(None, now), "-");
        assert_eq!(
            fmt_relative_time_cn(Some(now - chrono::Duration::seconds(20)), now),
            "刚刚"
        );
        assert_eq!(
            fmt_relative_time_cn(Some(now - chrono::Duration::minutes(12)), now),
            "12m前"
        );
        assert_eq!(
            fmt_relative_time_cn(Some(now - chrono::Duration::hours(3)), now),
            "3h前"
        );
        assert_eq!(
            fmt_relative_time_cn(Some(Local.with_ymd_and_hms(2026, 11, 23, 21, 0, 0).single().unwrap()), now),
            "昨天"
        );
        assert_eq!(
            fmt_relative_time_cn(Some(Local.with_ymd_and_hms(2026, 4, 12, 9, 0, 0).single().unwrap()), now),
            "04/12"
        );
    }

    #[test]
    fn test_daily_weekly_decoupled_from_scope_and_drilldown_expands() {
        use crate::stats::aggregate_sessions;
        use std::path::PathBuf;

        let today = Local::now().date_naive();
        let old_day = today.checked_sub_days(chrono::Days::new(14)).unwrap();
        let today_str = today.format("%Y-%m-%d").to_string();
        let old_day_str = old_day.format("%Y-%m-%d").to_string();
        let old_week_str = iso_week_key(old_day);

        let mut s_today = make_session(
            "today-uuid-0001",
            "C:/Users/alice/today-app",
            "gemini-4-neon-xhigh",
            (today.year(), today.month(), today.day()),
            2,
        );
        s_today.title = "今天任务".into();

        let mut s_old = make_session(
            "secret-hist-0002",
            "C:/Users/alice/secret-client",
            "claude-opus-5-5",
            (old_day.year(), old_day.month(), old_day.day()),
            3,
        );
        s_old.title = "历史机密排查".into();

        let all_sessions = vec![s_today, s_old];
        let all_agg = aggregate_sessions(&all_sessions);
        let snap = ScanSnapshot {
            data_dir: PathBuf::from("."),
            data_dirs: Vec::new(),
            sessions: all_sessions,
            agg: all_agg,
            statusline: None,
            statusline_changed: false,
            budget: BudgetConfig::default(),
            budget_changed: false,
            budget_path: None,
            session_meta: HashMap::new(),
            session_meta_changed: false,
            session_meta_path: None,
            archived_sessions_count: 0,
            archive_enabled: false,
            archive_path: None,
            model_rate_labels: HashMap::new(),
            pricing_rows: Vec::new(),
            pricing_path: None,
            custom_pricing_count: 0,
            pricing_changed: false,
            skipped_empty: 0,
            skipped_lines: 0,
            files_cached: 0,
            files_reparsed: 0,
            scan_ms: 0.0,
            warning: None,
        };

        let mut view = DashboardView::new(false, DateScope::Today, false, true, false);
        view.ensure_filtered(&snap);
        assert_eq!(view.all_sessions_count, 2);
        assert_eq!(view.all_total.calls, 5);
        assert_eq!(view.cached_agg.total.calls, 2);

        // 1) Overview/Models/Sessions/Calls/System respect DateScope; Daily/Weekly are decoupled
        for tab in [Tab::Overview, Tab::Models, Tab::Sessions, Tab::Calls, Tab::System] {
            view.active_tab = tab;
            assert!(view.tab_uses_scope());
        }
        for tab in [Tab::Daily, Tab::Weekly] {
            view.active_tab = tab;
            assert!(!view.tab_uses_scope());
            assert_eq!(view.scope, DateScope::Today);
        }

        // 2) Custom date bar hides on Daily/Weekly and returns on scoped tabs without losing scope
        view.scope = DateScope::Custom;
        view.active_tab = Tab::Daily;
        assert!(!view.is_custom_date_active());
        view.active_tab = Tab::Weekly;
        assert!(!view.is_custom_date_active());
        view.active_tab = Tab::Overview;
        assert!(view.is_custom_date_active());
        view.scope = DateScope::Today;

        // 3) Daily CSV & JSON exports follow full history on Daily/Weekly and stay scoped on Overview
        view.active_tab = Tab::Overview;
        let scoped_csv = view.export_daily_csv(&snap);
        assert!(scoped_csv.contains(&today_str));
        assert!(!scoped_csv.contains(&old_day_str));

        view.active_tab = Tab::Daily;
        let full_csv = view.export_daily_csv(&snap);
        assert!(full_csv.contains(&today_str));
        assert!(full_csv.contains(&old_day_str));

        view.privacy_mode = true;
        let full_json = view.export_snapshot_json(&snap);
        let parsed: serde_json::Value = serde_json::from_str(&full_json).expect("valid json");
        assert_eq!(parsed["scope"], serde_json::json!("All"));
        assert_eq!(parsed["overview"]["calls"], serde_json::json!(5));
        assert_eq!(parsed["sessions"].as_array().map(|a| a.len()), Some(2));
        assert!(!full_json.contains("历史机密排查"));
        assert!(!full_json.contains("secret-hist"));
        assert!(!full_json.contains("secret-client"));
        assert!(!full_json.contains("alice"));
        assert!(!full_json.contains("S-???"));
        assert!(!full_json.contains("项目 ?"));
        view.privacy_mode = false;

        // 4) scope_covers_filter accurately detects truncated weeks and out-of-scope dates
        let wed = NaiveDate::from_ymd_opt(2026, 11, 25).unwrap(); // Wednesday of 2026-W48
        let cur_wk = iso_week_key(wed);
        let prev_wk = iso_week_key(wed.checked_sub_days(chrono::Days::new(7)).unwrap());
        assert!(DashboardView::scope_covers_filter(
            DateScope::Today,
            None,
            None,
            &SessionFilter::Date("2026-11-25".into()),
            wed,
        ));
        assert!(!DashboardView::scope_covers_filter(
            DateScope::Today,
            None,
            None,
            &SessionFilter::Date("2026-11-24".into()),
            wed,
        ));
        // On Wednesday, Today does NOT cover Monday-Tuesday of the current ISO week, while Days7 does
        assert!(!DashboardView::scope_covers_filter(
            DateScope::Today,
            None,
            None,
            &SessionFilter::Week(cur_wk.clone()),
            wed,
        ));
        assert!(DashboardView::scope_covers_filter(
            DateScope::Days7,
            None,
            None,
            &SessionFilter::Week(cur_wk),
            wed,
        ));
        // Days7 does NOT cover all of the previous ISO week
        assert!(!DashboardView::scope_covers_filter(
            DateScope::Days7,
            None,
            None,
            &SessionFilter::Week(prev_wk),
            wed,
        ));

        // 5) Drilling into today keeps DateScope::Today; drilling into an older day/week auto-expands to All
        let mut human = true;
        let mut light = false;
        view.active_tab = Tab::Daily;
        view.scope = DateScope::Today;
        let (_, prefs_changed_today) = view.apply_action(
            HWND::default(),
            ClickAction::FilterSessions(SessionFilter::Date(today_str.clone())),
            &mut human,
            &mut light,
        );
        assert!(!prefs_changed_today);
        assert_eq!(view.scope, DateScope::Today);
        assert_eq!(view.active_tab, Tab::Sessions);

        view.active_tab = Tab::Daily;
        let (_, prefs_changed_old) = view.apply_action(
            HWND::default(),
            ClickAction::FilterSessions(SessionFilter::Date(old_day_str.clone())),
            &mut human,
            &mut light,
        );
        assert!(!prefs_changed_old);
        assert_eq!(view.scope, DateScope::All);
        assert!(view.dirty_filter);
        view.ensure_filtered(&snap);
        let rows = view.display_sessions_for_tab();
        assert_eq!(rows.len(), 1);
        assert_eq!(view.row_session(&rows[0]).conversation_id, "secret-hist-0002");
        assert_eq!(view.selected_session.as_deref(), Some("secret-hist-0002"));

        // Drilling into an older ISO week from Today also expands to All and returns full week sessions
        view.scope = DateScope::Today;
        view.dirty_filter = true;
        view.ensure_filtered(&snap);
        view.active_tab = Tab::Weekly;
        let (_, prefs_changed_wk) = view.apply_action(
            HWND::default(),
            ClickAction::FilterSessions(SessionFilter::Week(old_week_str)),
            &mut human,
            &mut light,
        );
        assert!(!prefs_changed_wk);
        assert_eq!(view.scope, DateScope::All);
        view.ensure_filtered(&snap);
        let wk_rows = view.display_sessions_for_tab();
        assert_eq!(wk_rows.len(), 1);
        assert_eq!(view.row_session(&wk_rows[0]).conversation_id, "secret-hist-0002");
    }

    #[test]
    fn test_agent_filter_search_aliases_and_privacy_mode() {
        let mut s_agy = make_session("agy-uuid-0001", "C:/Users/alice/secret-agy", "gemini-4-neon-xhigh", (2026, 11, 24), 2);
        s_agy.title = "机密 AGY 任务".into();
        s_agy.source = SourceKind::Cli;

        let mut s_claude = make_session("claude-uuid-0002", "C:/Users/alice/secret-cc", "claude-sonnet-4-6", (2026, 11, 24), 2);
        s_claude.title = "机密 Claude 任务".into();
        s_claude.source = SourceKind::ClaudeCode;

        let mut s_codex = make_session("codex-uuid-0003", "C:/Users/alice/secret-cx", "gpt-5.3-codex", (2026, 11, 24), 1);
        s_codex.title = "机密 Codex 任务".into();
        s_codex.source = SourceKind::Codex;

        let mut s_gemini = make_session("gem-uuid-0004", "C:/Users/alice/secret-gm", "gemini-2.5-pro", (2026, 11, 24), 1);
        s_gemini.title = "机密 Gemini 任务".into();
        s_gemini.source = SourceKind::GeminiCli;

        let sessions = vec![s_agy.clone(), s_claude.clone(), s_codex.clone(), s_gemini.clone()];
        let agg = crate::stats::aggregate_sessions(&sessions);
        let redactor = Redactor::build(&sessions, &agg);

        // 1) SessionFilter::Agent matches family & chip label is privacy-safe
        let f_claude = SessionFilter::Agent(AgentFamily::ClaudeCode);
        assert_eq!(f_claude.label(None), "来源: Claude  ×");
        assert_eq!(f_claude.label(Some(&redactor)), "来源: Claude  ×");
        assert!(!f_claude.slices_turns());
        assert!(f_claude.matches(&s_claude));
        assert!(!f_claude.matches(&s_agy));
        assert!(!f_claude.matches(&s_codex));
        assert_eq!(SessionFilter::Agent(AgentFamily::Antigravity).label(Some(&redactor)), "来源: AGY  ×");
        assert_eq!(SessionFilter::Agent(AgentFamily::Codex).label(Some(&redactor)), "来源: Codex  ×");
        assert_eq!(SessionFilter::Agent(AgentFamily::GeminiCli).label(Some(&redactor)), "来源: Gemini  ×");

        let mut view = DashboardView::new(false, DateScope::All, false, true, true);
        view.cached_sessions = sessions;
        view.cached_agg = agg;
        view.set_redactor_for_test(redactor);

        // SessionFilter::Agent filters display_sessions cleanly
        view.session_filter = Some(SessionFilter::Agent(AgentFamily::Codex));
        let codex_rows = view.display_sessions(true);
        assert_eq!(codex_rows.len(), 1);
        assert_eq!(view.row_session(&codex_rows[0]).conversation_id, "codex-uuid-0003");
        view.session_filter = None;

        // 2) Under privacy_mode, agent aliases & SourceKind::label_short() still match in Sessions & Calls
        for (q, expected_cid, expected_calls) in [
            ("claudecode", "claude-uuid-0002", 2usize),
            ("claude-code", "claude-uuid-0002", 2usize),
            ("openai-codex", "codex-uuid-0003", 1usize),
            ("gemini-cli", "gem-uuid-0004", 1usize),
            ("antigravity", "agy-uuid-0001", 2usize),
        ] {
            view.search_query = q.into();
            let s_hits = view.display_sessions(true);
            assert_eq!(s_hits.len(), 1, "sessions query={q}");
            assert_eq!(view.row_session(&s_hits[0]).conversation_id, expected_cid);
            let c_hits = view.call_rows();
            assert_eq!(c_hits.len(), expected_calls, "calls query={q}");
            assert!(c_hits.iter().all(|r| view.call_session(r).conversation_id == expected_cid));
        }

        // Private raw title/workspace remains completely unsearchable under privacy_mode
        view.search_query = "secret-cc".into();
        assert!(view.display_sessions(true).is_empty());
        assert!(view.call_rows().is_empty());
        view.search_query = "机密".into();
        assert!(view.display_sessions(true).is_empty());
        assert!(view.call_rows().is_empty());

        // 3) Multi-agent prefix tags preserve legacy AGY-only mode vs multi-agent mode
        assert_eq!(format_source_tag(SourceKind::Cli, false), None);
        assert_eq!(format_source_tag(SourceKind::App, false), Some("App"));
        assert_eq!(format_source_tag(SourceKind::Ide, false), Some("IDE"));
        assert_eq!(format_source_tag(SourceKind::Cli, true), Some("AGY"));
        assert_eq!(format_source_tag(SourceKind::App, true), Some("App"));
        assert_eq!(format_source_tag(SourceKind::Ide, true), Some("IDE"));
        assert_eq!(format_source_tag(SourceKind::ClaudeCode, true), Some("Claude"));
        assert_eq!(format_source_tag(SourceKind::Codex, true), Some("Codex"));
        assert_eq!(format_source_tag(SourceKind::GeminiCli, true), Some("Gemini"));
    }

    #[test]
    fn test_redact_data_dir_and_active_sources_all_agents() {
        assert_eq!(redact_data_dir(Path::new(r"C:\Users\alice\.claude")), "~/.claude");
        assert_eq!(redact_data_dir(Path::new("/home/alice/.claude/")), "~/.claude");
        assert_eq!(redact_data_dir(Path::new(r"C:\Users\alice\.config\claude")), "~/.config/claude");
        assert_eq!(redact_data_dir(Path::new("/home/alice/.config/claude/")), "~/.config/claude");
        assert_eq!(redact_data_dir(Path::new(r"C:\Users\alice\.codex")), "~/.codex");
        assert_eq!(redact_data_dir(Path::new("/Users/alice/.codex/")), "~/.codex");
        assert_eq!(redact_data_dir(Path::new(r"C:\Users\alice\.gemini\tmp")), "~/.gemini/tmp");
        assert_eq!(redact_data_dir(Path::new("/home/alice/.gemini/tmp/")), "~/.gemini/tmp");
        assert_eq!(
            redact_data_dir(Path::new(r"C:\Users\alice\.gemini\antigravity")),
            "~/.gemini/antigravity"
        );
        assert_eq!(
            redact_data_dir(Path::new(r"C:\Users\alice\.gemini\antigravity-ide")),
            "~/.gemini/antigravity-ide"
        );
        // Custom paths stay redacted
        assert_eq!(redact_data_dir(Path::new(r"D:\Private\my-codex-home")), "<已脱敏目录>");
        assert_eq!(redact_data_dir(Path::new(r"C:\Users\alice\.gemini")), "<已脱敏目录>");

        let dirs = vec![
            (SourceKind::Cli, std::path::PathBuf::from(r"C:\Users\alice\.gemini\antigravity-cli")),
            (SourceKind::ClaudeCode, std::path::PathBuf::from(r"C:\Users\alice\.claude")),
            (SourceKind::Codex, std::path::PathBuf::from(r"C:\Users\alice\.codex")),
            (SourceKind::GeminiCli, std::path::PathBuf::from(r"C:\Users\alice\.gemini")),
        ];
        let red_line = format_active_sources_line(&dirs, Path::new("."), true);
        assert!(red_line.contains("~/.gemini/antigravity-cli"));
        assert!(red_line.contains("~/.claude"));
        assert!(red_line.contains("~/.codex"));
        assert!(red_line.contains("~/.gemini/tmp"));
        assert!(!red_line.contains("alice"));
    }

    #[test]
    fn test_reasoning_tokens_thinking_and_non_agy_gen_secs_formatting() {
        let mut s_codex = make_session("codex-1", "C:/Users/alice/app", "gpt-5.3-codex", (2026, 11, 24), 1);
        s_codex.source = SourceKind::Codex;
        mutate_turns(&mut s_codex, |turns| {
            turns[0].reasoning_tokens = 1_250;
            turns[0].thinking_chars = 0;
            turns[0].gen_secs = None;
        });
        assert!(s_codex.turns[0].has_thinking());
        assert_eq!(format_turn_thinking(&s_codex.turns[0]).as_deref(), Some("1,250 tok"));

        // Even when both reasoning_tokens and thinking_chars are set (e.g. Gemini CLI), prefer `X tok`
        let mut t_gem = s_codex.turns[0].clone();
        t_gem.thinking_chars = 88;
        assert!(t_gem.has_thinking());
        assert_eq!(format_turn_thinking(&t_gem).as_deref(), Some("1,250 tok"));

        // Antigravity / Claude plaintext thinking shows `X 字`
        let mut s_agy = make_session("agy-1", "C:/Users/alice/app", "gemini-4-neon-xhigh", (2026, 11, 24), 1);
        s_agy.source = SourceKind::Cli;
        mutate_turns(&mut s_agy, |turns| {
            turns[0].reasoning_tokens = 0;
            turns[0].thinking_chars = 420;
            turns[0].gen_secs = None;
        });
        assert!(s_agy.turns[0].has_thinking());
        assert_eq!(format_turn_thinking(&s_agy.turns[0]).as_deref(), Some("420 字"));

        // No thinking -> None
        let mut t_plain = s_agy.turns[0].clone();
        t_plain.thinking_chars = 0;
        assert!(!t_plain.has_thinking());
        assert_eq!(format_turn_thinking(&t_plain), None);

        // Clipboard notes: non-AGY turn with gen_secs=None uses "当前日志无生成耗时记录" & reasoning tokens
        let view = DashboardView::new(false, DateScope::All, false, true, false);
        let codex_clip = view.format_call_detail_clipboard(&s_codex, &s_codex.turns[0], None);
        assert!(codex_clip.contains("当前日志无生成耗时记录"));
        assert!(codex_clip.contains("思考: 1,250 tok"));
        assert!(!codex_clip.contains("1,250 字"));

        let agy_clip = view.format_call_detail_clipboard(&s_agy, &s_agy.turns[0], None);
        assert!(agy_clip.contains("纯回复 / 压缩步骤无结束时间"));
        assert!(agy_clip.contains("思考: 420 字"));
    }

    #[test]
    fn test_on_hide_releases_caches_and_preserves_prefs() {
        let s1 = make_session("s1", "C:/Users/a/repo", "gemini-4-neon-xhigh", (2026, 11, 24), 3);
        let agg = crate::stats::aggregate_sessions(std::slice::from_ref(&s1));
        let mut view = DashboardView::new(true, DateScope::Days7, true, true, true);
        view.active_tab = Tab::Sessions;
        view.cached_sessions = vec![s1];
        view.cached_agg = agg;
        view.hit_boxes.push(HitBox {
            rect: rect_f(0.0, 0.0, 50.0, 20.0),
            action: ClickAction::Refresh,
        });
        view.plot_rects.push(rect_f(10.0, 10.0, 100.0, 50.0));
        view.discrete_plots.push(DiscretePlot::VerticalBars {
            plot: rect_f(0.0, 0.0, 100.0, 50.0),
            n: 5,
        });
        let _ = view.display_sessions(true);
        let _ = view.format_session_id("s1"); // lazily builds redactor
        assert!(view.redactor_built.get());

        // Populate transient banners, toast timers, and an empty-but-overallocated search_query
        view.set_banner_status("导出成功", false);
        view.set_pricing_status("已保存", false);
        let future = Instant::now() + std::time::Duration::from_secs(5);
        view.copied_until = Some(future);
        view.session_copied_until = Some(future);
        view.resume_cmd_copied_until = Some(future);
        view.call_copied_until = Some(future);
        view.search_query = "temporary query that allocates heap".into();
        view.search_query.clear();
        assert!(view.search_query.capacity() > 0);

        view.on_hide();

        // Heavy caches, transient banners/timers, and empty search buffer are released and marked dirty
        assert!(view.cached_sessions.is_empty());
        assert_eq!(view.cached_sessions.capacity(), 0);
        assert!(view.pinned_out_of_scope.is_empty());
        assert!(view.hit_boxes.is_empty());
        assert!(view.plot_rects.is_empty());
        assert!(view.discrete_plots.is_empty());
        assert!(!view.redactor_built.get());
        assert!(view.surface.is_none());
        assert!(view.dash_factories.is_none());
        assert!(view.dirty_filter);
        assert!(view.banner_status.is_none());
        assert!(view.pricing_status.is_none());
        assert!(view.copied_until.is_none());
        assert!(view.session_copied_until.is_none());
        assert!(view.resume_cmd_copied_until.is_none());
        assert!(view.call_copied_until.is_none());
        assert_eq!(view.search_query.capacity(), 0);

        // User preferences and navigation state stay intact
        assert!(view.fresh_only);
        assert_eq!(view.scope, DateScope::Days7);
        assert!(view.cost_mode);
        assert!(view.privacy_mode);
        assert_eq!(view.active_tab, Tab::Sessions);
    }

    #[test]
    fn test_compiled_turn_filter_utc_range_parity() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 25).unwrap(); // Wednesday of 2026-W48
        let prev_day = day.pred_opt().unwrap();
        let next_day = day.succ_opt().unwrap();
        let (wk_mon, wk_sun) = iso_week_bounds("2026-W48").unwrap();
        let before_wk = wk_mon.pred_opt().unwrap();
        let after_wk = wk_sun.succ_opt().unwrap();

        let make_local_ts = |d: NaiveDate, h: u32, m: u32, s: u32| -> DateTime<Utc> {
            Local
                .from_local_datetime(&d.and_hms_opt(h, m, s).unwrap())
                .earliest()
                .unwrap()
                .with_timezone(&Utc)
        };
        let probe_times = [
            make_local_ts(before_wk, 23, 59, 59),
            make_local_ts(wk_mon, 0, 0, 0),
            make_local_ts(prev_day, 23, 59, 59),
            make_local_ts(day, 0, 0, 0),
            make_local_ts(day, 12, 34, 56),
            make_local_ts(day, 23, 59, 59),
            make_local_ts(next_day, 0, 0, 0),
            make_local_ts(wk_sun, 23, 59, 59),
            make_local_ts(after_wk, 0, 0, 0),
        ];

        let date_f = SessionFilter::Date("2026-11-25".into());
        let compiled_date = date_f.compile();
        assert!(matches!(compiled_date, CompiledTurnFilter::UtcRange(..)));

        let week_f = SessionFilter::Week("2026-W48".into());
        let compiled_week = week_f.compile();
        assert!(matches!(compiled_week, CompiledTurnFilter::UtcRange(..)));

        let bad_date = SessionFilter::Date("not-a-date".into());
        let compiled_bad = bad_date.compile();
        assert!(matches!(compiled_bad, CompiledTurnFilter::Never));

        for ts in probe_times {
            let turn = TurnRecord {
                step_index: 0,
                timestamp_utc: Some(ts),
                model: Arc::from("m"),
                is_checkpoint: false,
                input_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cache_write_1h_tokens: 0,
                output_tokens: 1,
                reasoning_tokens: 0,
                tool_names: Box::new([]),
                thinking_chars: 0,
                gen_secs: None,
                cost: CostBreakdown::default(),
            };
            let local_d = ts.with_timezone(&Local).date_naive();
            assert_eq!(compiled_date.matches_turn(&turn), local_d == day, "date mismatch at {ts}");
            assert_eq!(
                compiled_week.matches_turn(&turn),
                local_d >= wk_mon && local_d <= wk_sun,
                "week mismatch at {ts}"
            );
            assert!(!compiled_bad.matches_turn(&turn));
        }

        // Turn with timestamp_utc == None never matches Date/Week filters
        let none_turn = TurnRecord {
            step_index: 0,
            timestamp_utc: None,
            model: Arc::from("m"),
            is_checkpoint: false,
            input_tokens: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cache_write_1h_tokens: 0,
            output_tokens: 1,
            reasoning_tokens: 0,
            tool_names: Box::new([]),
            thinking_chars: 0,
            gen_secs: None,
            cost: CostBreakdown::default(),
        };
        assert!(!compiled_date.matches_turn(&none_turn));
        assert!(!compiled_week.matches_turn(&none_turn));
    }

    #[test]
    fn test_ensure_filtered_defers_session_rows_cache_off_sessions_tab() {
        let s1 = make_session("s1", "C:/Users/a/repo", "gemini-4-neon-xhigh", (2026, 11, 25), 2);
        let s2 = make_session("s2", "C:/Users/a/repo", "claude-opus-4-6", (2026, 11, 25), 3);
        let sessions = vec![s1, s2];
        let agg = crate::stats::aggregate_sessions(&sessions);
        let snap = ScanSnapshot {
            data_dir: std::path::PathBuf::from("."),
            data_dirs: Vec::new(),
            sessions,
            agg,
            statusline: None,
            statusline_changed: false,
            budget: BudgetConfig::default(),
            budget_changed: false,
            budget_path: None,
            session_meta: HashMap::new(),
            session_meta_changed: false,
            session_meta_path: None,
            archived_sessions_count: 0,
            archive_enabled: false,
            archive_path: None,
            model_rate_labels: HashMap::new(),
            pricing_rows: Vec::new(),
            pricing_path: None,
            custom_pricing_count: 0,
            pricing_changed: false,
            skipped_empty: 0,
            skipped_lines: 0,
            files_cached: 0,
            files_reparsed: 0,
            scan_ms: 0.0,
            warning: None,
        };

        // On Tab::Overview, ensure_filtered must NOT eagerly build session_rows_cache
        let mut view = DashboardView::new(false, DateScope::All, false, true, false);
        view.active_tab = Tab::Overview;
        view.selected_session = Some("stale-deleted-id".into());
        view.ensure_filtered(&snap);
        assert!(view.session_rows_cache.borrow().is_none());
        assert!(view.selected_session.is_none());

        // On Tab::Sessions, ensure_filtered populates session_rows_cache and selects first row
        view.active_tab = Tab::Sessions;
        view.mark_data_dirty();
        view.ensure_filtered(&snap);
        assert!(view.session_rows_cache.borrow().is_some());
        assert!(view.selected_session.is_some());
    }

    #[test]
    fn test_fmt_buf_stack_utf8_truncation_safety() {
        let mut b = FmtBuf::new();
        let _ = write!(b, "{:.1}/{:.1} GB", 16.34, 32.0);
        assert_eq!(b.as_str(), "16.3/32.0 GB");
        b.clear();
        let _ = write!(b, "{}", "测".repeat(30)); // 90 bytes > 64-byte stack buf
        assert!(b.as_str().len() <= 64);
        assert_eq!(b.as_str().chars().count(), 21); // 21 * 3 = 63 bytes on valid char boundary
    }

    #[test]
    fn test_lazy_redactor_builds_only_in_privacy_mode() {
        let s1 = make_session("uuid-1", "C:/Users/alice/secret", "gemini-4-neon-xhigh", (2026, 11, 24), 2);
        let agg = crate::stats::aggregate_sessions(std::slice::from_ref(&s1));
        let mut view = DashboardView::new(false, DateScope::All, false, true, false);
        view.cached_sessions = vec![s1];
        view.cached_agg = agg;

        // Non-privacy mode never builds redactor tables
        assert_eq!(view.format_session_id("uuid-1"), "uuid-1");
        assert!(!view.redactor_built.get());

        // Flipping privacy_mode lazily builds on first access
        view.privacy_mode = true;
        let rid = view.format_session_id("uuid-1");
        assert!(view.redactor_built.get());
        assert!(rid.starts_with("S-"));

        // Toggling privacy_mode off clears the redactor immediately
        let mut human = true;
        let mut light = false;
        let _ = view.apply_action(HWND::default(), ClickAction::TogglePrivacyMode, &mut human, &mut light);
        assert!(!view.privacy_mode);
        assert!(!view.redactor_built.get());
    }

    #[test]
    fn test_discrete_plot_hover_gating() {
        let mut view = DashboardView::new(false, DateScope::Today, false, true, false);
        view.discrete_plots.push(DiscretePlot::VerticalBars {
            plot: rect_f(100.0, 100.0, 100.0, 50.0),
            n: 4, // slots: [100..125), [125..150), [150..175), [175..200)
        });
        view.discrete_plots.push(DiscretePlot::Heatmap(HeatmapGridGeom {
            gx0: 10.0,
            gy0: 10.0,
            cell: 10.0,
            gap: 2.0,
            n_weeks: 3,
        }));
        view.discrete_plots.push(DiscretePlot::RowRects {
            bounds: rect_f(300.0, 10.0, 80.0, 45.0),
            rows: vec![
                rect_f(300.0, 10.0, 80.0, 20.0),
                rect_f(300.0, 35.0, 80.0, 20.0),
            ],
        });
        view.discrete_plots.push(DiscretePlot::TimeSeries {
            x0: 400.0,
            pr: 490.0,
            pt_y: 10.0,
            pb_y: 60.0,
            dx: 10.0,
            n: 10,
        });

        // VerticalBars: entering bar 0 repaints once; moving inside bar 0 skips; entering bar 1 repaints once
        assert!(view.on_mouse_move(105.0, 120.0));
        assert!(!view.on_mouse_move(118.0, 130.0));
        assert!(view.on_mouse_move(130.0, 120.0));
        assert!(!view.on_mouse_move(145.0, 120.0));

        // Heatmap: entering cell (0,0) repaints; moving inside same cell skips; hitting gap leaves cell
        assert!(view.on_mouse_move(12.0, 12.0));
        assert!(!view.on_mouse_move(18.0, 18.0));
        assert!(view.on_mouse_move(21.0, 15.0)); // in gap -> None
        assert!(!view.on_mouse_move(21.5, 15.0));

        // RowRects: entering row 0 repaints; moving inside row 0 skips; row 1 repaints
        assert!(view.on_mouse_move(310.0, 15.0));
        assert!(!view.on_mouse_move(360.0, 20.0));
        assert!(view.on_mouse_move(310.0, 40.0));

        // TimeSeries: quantized by dx=10; moving within same sample skips; next sample repaints
        assert!(view.on_mouse_move(401.0, 30.0));
        assert!(!view.on_mouse_move(403.0, 35.0));
        assert!(view.on_mouse_move(412.0, 30.0));

        // Leaving everything repaints once, then on_mouse_leave returns false
        assert!(view.on_mouse_move(0.0, 0.0));
        assert!(!view.on_mouse_leave());
    }

    #[test]
    fn test_telemetry_tick_repaint_modes() {
        let mut view = DashboardView::new(false, DateScope::Today, false, true, false);
        view.dirty_filter = false;
        view.active_tab = Tab::Overview;
        view.overview_top = OverviewTopMode::WeeklyCompare;

        let mut tel = SystemTelemetry::new(4);
        tel.latest.cpu_pct = 24.4;
        tel.latest.mem_pct = 51.2;
        tel.latest.mem_used_gb = 16.34;
        tel.latest.mem_total_gb = 32.0;

        // First tick with no previous sig and no surface falls back to Full
        assert_eq!(view.on_telemetry_tick(&tel), TelemetryRepaintMode::Full);

        // Once signature is recorded, identical displayed values return Skip
        view.last_hero_telemetry_sig = Some(HeroTelemetrySig::from_telemetry(&tel));
        assert_eq!(view.on_telemetry_tick(&tel), TelemetryRepaintMode::Skip);

        // Sub-rounding jitter (24.4 -> 24.1, still "24%"; 16.34 -> 16.32, still "16.3") also returns Skip
        tel.latest.cpu_pct = 24.1;
        tel.latest.mem_used_gb = 16.32;
        assert_eq!(view.on_telemetry_tick(&tel), TelemetryRepaintMode::Skip);

        // Tab::System always requires Full repaint for live 90s waveform
        view.active_tab = Tab::System;
        assert_eq!(view.on_telemetry_tick(&tel), TelemetryRepaintMode::Full);
        view.active_tab = Tab::Overview;

        // Toast expiry forces a Full repaint to clear the banner
        view.had_active_toast = true;
        assert_eq!(view.on_telemetry_tick(&tel), TelemetryRepaintMode::Full);
        view.had_active_toast = false;

        // Visible CPU% change with no retained surface returns Full (caller falls back)
        tel.latest.cpu_pct = 37.0;
        assert_eq!(view.on_telemetry_tick(&tel), TelemetryRepaintMode::Full);
    }
}
