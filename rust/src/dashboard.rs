//! Editorial-grade Direct2D + DirectWrite analytics dashboard.
//!
//! Design principles:
//!   - Warm neutral Obsidian Charcoal / Warm Paper themes (zero neon AI blue/purple/mint).
//!   - Flattened 4-tab underline navigation (`概览` | `按天` | `按周` | `会话`);
//!     Hero strip right-side CPU/RAM rings click-toggle the 90s System Monitor view.
//!   - Asymmetric 3-panel Overview: smart Trend + ISO Week/Model breakdown + Full-Width Recent Sessions
//!     with per-session cache hit rate.
//!   - Interactive "总计 / 仅增量" toggle and Registry-persisted DateScope (defaults to Today).

use chrono::{Local, NaiveDate, Timelike};
use std::collections::HashMap;
use std::time::Instant;

use windows::core::Result;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F};
use windows::Win32::Graphics::Direct2D::ID2D1RenderTarget;
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

use crate::d2d::{
    brush, draw_area_series, draw_editorial_bars, draw_line, draw_ring_gauge, draw_text,
    fill_circle, fill_round_rect, hit_rect, load_color, rect_f, stroke_round_rect,
    theme_for, with_alpha, BarItem, BarUnit, D2DFactories, Fonts, Theme, WindowSurface,
};
use crate::pricing::{
    fmt_cost, fmt_cost_compact, fmt_rate_plain, fmt_usd, fmt_usd_compact, fmt_usd_precise,
    PriceSource, PricingTableRow,
};
use crate::stats::{
    filter_snapshot, fmt_commas, fmt_compact, fmt_gap, fmt_num, is_compaction_model, iso_week_key,
    iso_week_range_label, weekday_cn, workspace_short_name, AggregatedStats, DateScope,
    ModelCacheStats, ScanSnapshot, SessionInfo, SessionSort, TokenCounts, TurnRecord,
};
use crate::telemetry::{apply_win11_frame, copy_text_to_clipboard, SystemTelemetry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Daily,
    Weekly,
    Models,
    Sessions,
    System,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSort {
    TotalDesc,
    CostDesc,
    HitRateAsc,
    HitRateDesc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionFilter {
    Model(String),
    Workspace(String),
    Date(String),
    Week(String),
    Parent(String, String), // (parent_conversation_id, parent_title)
}

impl SessionFilter {
    fn label(&self) -> String {
        match self {
            SessionFilter::Model(m) => format!("模型: {}  ×", m),
            SessionFilter::Workspace(ws) => {
                let s = workspace_short_name(ws);
                format!("项目: {}  ×", if s.is_empty() { ws.as_str() } else { s.as_str() })
            }
            SessionFilter::Date(d) => format!("日期: {}  ×", d),
            SessionFilter::Week(w) => format!("周: {}  ×", w),
            SessionFilter::Parent(_, title) => {
                let short: String = title.chars().take(18).collect();
                let ell = if title.chars().count() > 18 { "…" } else { "" };
                format!("任务组: {}{}  ×", short, ell)
            }
        }
    }

    fn matches(&self, s: &SessionInfo) -> bool {
        match self {
            SessionFilter::Model(m) => s.turns.iter().any(|t| t.model == *m),
            SessionFilter::Workspace(ws) => s.workspace == *ws,
            SessionFilter::Date(day) => s.turns.iter().any(|t| {
                t.timestamp_utc
                    .map(|u| u.with_timezone(&Local).format("%Y-%m-%d").to_string() == *day)
                    .unwrap_or(false)
            }),
            SessionFilter::Week(wk) => s.turns.iter().any(|t| {
                t.timestamp_utc
                    .map(|u| iso_week_key(u.with_timezone(&Local).date_naive()) == *wk)
                    .unwrap_or(false)
            }),
            SessionFilter::Parent(pid, _) => {
                s.conversation_id == *pid || s.parent_id.as_deref() == Some(pid.as_str())
            }
        }
    }
}

#[derive(Debug, Clone)]
struct DisplaySessionRow {
    session: SessionInfo,
    counts: TokenCounts,
    primary_model: String,
    last_time: Option<chrono::DateTime<Local>>,
    subagent_count: usize,
    is_child_row: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickAction {
    SelectTab(Tab),
    ToggleSystemView,
    SelectWeeklyMode(WeeklyMode),
    SelectBreakdown(BreakdownMode),
    SelectModelCardMetric(ModelCardMetric),
    SelectModelView(ModelViewMode),
    OpenPricingTable,
    PickPricingRow(usize),
    NewPricingEntry,
    SavePricingEntry,
    ResetPricingEntry,
    SelectModelSort(ModelSort),
    SelectScope(DateScope),
    SetCostMode(bool),
    ToggleCostMode,
    ToggleGroupSubagents,
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
}

struct HitBox {
    rect: D2D_RECT_F,
    action: ClickAction,
}

pub struct DashboardView {
    surface: Option<WindowSurface>,
    pub active_tab: Tab,
    prev_token_tab: Tab,
    pub weekly_mode: WeeklyMode,
    pub breakdown_mode: BreakdownMode,
    pub model_card_metric: ModelCardMetric,
    pub model_view: ModelViewMode,
    pub model_sort: ModelSort,
    pub scope: DateScope,
    pub sort: SessionSort,
    pub session_filter: Option<SessionFilter>,
    pub cost_mode: bool,
    pub group_subagents: bool,
    pub fresh_only: bool,
    pub selected_session: Option<String>,
    pub pricing_selected_model: Option<String>,
    pub pricing_editing_custom_key: Option<String>,
    pub pricing_status: Option<(String, bool, Instant)>,
    pub pricing_field_rects: Option<[D2D_RECT_F; 5]>,
    pub pricing_focused_field: Option<usize>,
    pub mouse_dip: Option<(f32, f32)>,
    scroll_rows: usize,
    max_scroll: usize,
    sessions_vis_rows: usize,
    hit_boxes: Vec<HitBox>,
    plot_rects: Vec<D2D_RECT_F>,
    last_hover_sig: (Option<usize>, bool),
    copied_until: Option<Instant>,
    session_copied_until: Option<Instant>,
    cached_sessions: Vec<SessionInfo>,
    cached_agg: AggregatedStats,
    cached_date: NaiveDate,
    dirty_filter: bool,
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

impl DashboardView {
    pub fn new(fresh_only: bool, scope: DateScope, cost_mode: bool, group_subagents: bool) -> Self {
        Self {
            surface: None,
            active_tab: Tab::Overview,
            prev_token_tab: Tab::Overview,
            weekly_mode: WeeklyMode::Weekly,
            breakdown_mode: BreakdownMode::Model,
            model_card_metric: ModelCardMetric::Volume,
            model_view: ModelViewMode::CacheCompare,
            model_sort: ModelSort::TotalDesc,
            scope,
            sort: SessionSort::Recent,
            session_filter: None,
            cost_mode,
            group_subagents,
            fresh_only,
            selected_session: None,
            pricing_selected_model: None,
            pricing_editing_custom_key: None,
            pricing_status: None,
            pricing_field_rects: None,
            pricing_focused_field: None,
            mouse_dip: None,
            scroll_rows: 0,
            max_scroll: 0,
            sessions_vis_rows: 6,
            hit_boxes: Vec::new(),
            plot_rects: Vec::new(),
            last_hover_sig: (None, false),
            copied_until: None,
            session_copied_until: None,
            cached_sessions: Vec::new(),
            cached_agg: AggregatedStats::default(),
            cached_date: Local::now().date_naive(),
            dirty_filter: true,
        }
    }

    fn bar_unit(&self) -> BarUnit {
        if self.cost_mode {
            BarUnit::Usd
        } else {
            BarUnit::Tokens
        }
    }

    fn make_bar(&self, label: String, c: &TokenCounts, marker: bool, tooltip: Vec<String>) -> BarItem {
        if self.cost_mode {
            BarItem {
                label,
                cache: c.cost.cache_read_usd,
                input: c.cost.input_plus_write_usd(),
                output: c.cost.output_usd,
                marker,
                tooltip,
            }
        } else {
            BarItem {
                label,
                cache: c.cache_read_tokens as f64,
                input: (c.input_tokens + c.cache_write_tokens) as f64,
                output: c.output_tokens as f64,
                marker,
                tooltip,
            }
        }
    }

    fn make_turn_bar(&self, label: String, t: &TurnRecord, tooltip: Vec<String>) -> BarItem {
        if self.cost_mode {
            BarItem {
                label,
                cache: t.cost.cache_read_usd,
                input: t.cost.input_plus_write_usd(),
                output: t.cost.output_usd,
                marker: t.is_checkpoint,
                tooltip,
            }
        } else {
            BarItem {
                label,
                cache: t.cache_read_tokens as f64,
                input: (t.input_tokens + t.cache_write_tokens) as f64,
                output: t.output_tokens as f64,
                marker: t.is_checkpoint,
                tooltip,
            }
        }
    }

    pub fn step_session(&mut self, delta: isize) -> bool {
        if self.active_tab != Tab::Sessions || self.cached_sessions.is_empty() {
            return false;
        }
        let sorted = self.display_sessions(true);
        if sorted.is_empty() {
            return false;
        }
        let cur_idx = self
            .selected_session
            .as_deref()
            .and_then(|id| sorted.iter().position(|r| r.session.conversation_id == id))
            .unwrap_or(0);
        let next_idx = (cur_idx as isize + delta).clamp(0, (sorted.len() - 1) as isize) as usize;
        if next_idx == cur_idx && self.selected_session.is_some() {
            return false;
        }
        self.selected_session = Some(sorted[next_idx].session.conversation_id.clone());
        let vis = self.sessions_vis_rows.max(1);
        if next_idx < self.scroll_rows {
            self.scroll_rows = next_idx;
        } else if next_idx >= self.scroll_rows + vis {
            self.scroll_rows = (next_idx + 1).saturating_sub(vis);
        }
        true
    }

    fn count_subagents_of(&self, parent_id: &str) -> (usize, TokenCounts) {
        let mut cnt = 0usize;
        let mut agg = TokenCounts::default();
        for s in &self.cached_sessions {
            if s.parent_id.as_deref() == Some(parent_id) {
                cnt += 1;
                agg.merge(&s.counts);
            }
        }
        (cnt, agg)
    }

    fn session_split_counts(&self) -> (usize, usize) {
        let sub = self.cached_sessions.iter().filter(|s| s.is_subagent).count();
        (self.cached_sessions.len().saturating_sub(sub), sub)
    }

    fn sort_display_rows(&self, rows: &mut [DisplaySessionRow], keep_parent_first: Option<&str>) {
        let ord_row = |a: &DisplaySessionRow, b: &DisplaySessionRow| -> std::cmp::Ordering {
            match self.sort {
                SessionSort::Recent => b.last_time.cmp(&a.last_time),
                SessionSort::Cost => b
                    .counts
                    .cost
                    .total_usd()
                    .partial_cmp(&a.counts.cost.total_usd())
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.counts.total_tokens().cmp(&a.counts.total_tokens())),
                SessionSort::NonCached => b.counts.non_cached_tokens().cmp(&a.counts.non_cached_tokens()),
                SessionSort::TotalTokens => b.counts.total_tokens().cmp(&a.counts.total_tokens()),
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
                SessionSort::PeakContext => b.counts.peak_context.cmp(&a.counts.peak_context),
            }
        };
        if let Some(pid) = keep_parent_first {
            rows.sort_by(|a, b| {
                let a_root = a.session.conversation_id == pid;
                let b_root = b.session.conversation_id == pid;
                b_root.cmp(&a_root).then_with(|| ord_row(a, b))
            });
        } else {
            rows.sort_by(ord_row);
        }
    }

    /// Builds session list rows, rolling up subagents into their parent session when `group_subagents`
    /// is enabled (unless drilling into a Parent group or filtering specifically to subagent-only models).
    fn display_sessions(&self, apply_filter: bool) -> Vec<DisplaySessionRow> {
        let filter = if apply_filter { self.session_filter.as_ref() } else { None };

        // Parent drill-down always expands parent + all its subagents flat for inspection.
        if let Some(SessionFilter::Parent(pid, _)) = filter {
            let mut rows: Vec<DisplaySessionRow> = self
                .cached_sessions
                .iter()
                .filter(|s| s.conversation_id == *pid || s.parent_id.as_deref() == Some(pid.as_str()))
                .map(|s| DisplaySessionRow {
                    counts: s.counts,
                    primary_model: s.primary_model(),
                    last_time: s.last_time,
                    subagent_count: 0,
                    is_child_row: s.parent_id.as_deref() == Some(pid.as_str()),
                    session: s.clone(),
                })
                .collect();
            self.sort_display_rows(&mut rows, Some(pid.as_str()));
            return rows;
        }

        // Model filter shows exact matching sessions (including subagents that ran that model).
        let force_flat = matches!(filter, Some(SessionFilter::Model(_)));
        let use_grouping = self.group_subagents && !force_flat;

        let filtered: Vec<&SessionInfo> = match filter {
            Some(f) => self.cached_sessions.iter().filter(|s| f.matches(s)).collect(),
            None => self.cached_sessions.iter().collect(),
        };

        if !use_grouping {
            let mut rows: Vec<DisplaySessionRow> = filtered
                .into_iter()
                .map(|s| DisplaySessionRow {
                    counts: s.counts,
                    primary_model: s.primary_model(),
                    last_time: s.last_time,
                    subagent_count: 0,
                    is_child_row: s.is_subagent,
                    session: s.clone(),
                })
                .collect();
            self.sort_display_rows(&mut rows, None);
            return rows;
        }

        let present_ids: std::collections::HashSet<&str> = filtered
            .iter()
            .map(|s| s.conversation_id.as_str())
            .collect();
        let mut children_by_parent: HashMap<&str, Vec<&SessionInfo>> = HashMap::new();
        for s in &filtered {
            if let Some(pid) = s.parent_id.as_deref() {
                if present_ids.contains(pid) {
                    children_by_parent.entry(pid).or_default().push(*s);
                }
            }
        }

        let mut rows = Vec::with_capacity(filtered.len());
        for s in &filtered {
            // Skip subagents whose parent is already shown in this view (they roll up into parent).
            if let Some(pid) = s.parent_id.as_deref() {
                if present_ids.contains(pid) {
                    continue;
                }
            }
            let mut counts = s.counts;
            let mut last_time = s.last_time;
            let mut sub_cnt = 0usize;
            if let Some(kids) = children_by_parent.get(s.conversation_id.as_str()) {
                sub_cnt = kids.len();
                for k in kids {
                    counts.merge(&k.counts);
                    if let Some(kt) = k.last_time {
                        if last_time.map(|pt| kt > pt).unwrap_or(true) {
                            last_time = Some(kt);
                        }
                    }
                }
            }
            rows.push(DisplaySessionRow {
                counts,
                primary_model: s.primary_model(),
                last_time,
                subagent_count: sub_cnt,
                is_child_row: s.is_subagent,
                session: (*s).clone(),
            });
        }
        self.sort_display_rows(&mut rows, None);
        rows
    }

    fn ordered_models(&self, sort: ModelSort) -> (Vec<(String, TokenCounts)>, Vec<(String, TokenCounts)>) {
        let mut primary: Vec<(String, TokenCounts)> = Vec::new();
        let mut secondary: Vec<(String, TokenCounts)> = Vec::new();
        for (name, c) in &self.cached_agg.by_model {
            if is_compaction_model(name) {
                secondary.push((name.clone(), *c));
            } else {
                primary.push((name.clone(), *c));
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

    pub fn set_pricing_status(&mut self, msg: impl Into<String>, is_error: bool) {
        let dur = if is_error { 4500 } else { 2600 };
        self.pricing_status = Some((
            msg.into(),
            is_error,
            Instant::now() + std::time::Duration::from_millis(dur),
        ));
    }

    pub fn is_pricing_editor_active(&self) -> bool {
        self.active_tab == Tab::Models && self.model_view == ModelViewMode::Pricing
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

    /// Updates cursor position and returns `true` only when hover visuals actually change
    /// (entering/leaving a hitbox or moving inside/leaving an interactive chart plot).
    pub fn on_mouse_move(&mut self, x: f32, y: f32) -> bool {
        self.mouse_dip = Some((x, y));
        let hb_idx = self.hit_boxes.iter().rposition(|hb| hit_rect(&hb.rect, x, y));
        let in_plot = self.plot_rects.iter().any(|r| hit_rect(r, x, y));
        let sig = (hb_idx, in_plot);
        let changed = in_plot || self.last_hover_sig.1 || sig != self.last_hover_sig;
        self.last_hover_sig = sig;
        changed
    }

    pub fn on_mouse_leave(&mut self) -> bool {
        self.mouse_dip = None;
        let had_hover = self.last_hover_sig.0.is_some() || self.last_hover_sig.1;
        self.last_hover_sig = (None, false);
        had_hover
    }

    pub fn hit_test_click(&self, x: f32, y: f32) -> Option<ClickAction> {
        for hb in self.hit_boxes.iter().rev() {
            if hit_rect(&hb.rect, x, y) {
                return Some(hb.action.clone());
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
            ClickAction::PickPricingRow(_)
            | ClickAction::NewPricingEntry
            | ClickAction::SavePricingEntry
            | ClickAction::ResetPricingEntry => (false, false),
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
                let t = self.cached_agg.total;
                let scope_lbl = match self.scope {
                    DateScope::Today => "今日",
                    DateScope::Days7 => "近7天",
                    DateScope::Days30 => "近30天",
                    DateScope::All => "全部",
                };
                let text = if self.active_tab == Tab::Models {
                    let (primary, secondary) = self.ordered_models(self.model_sort);
                    let mut lines = vec![
                        format!("AGY CLI 按模型用量与估算费用 ({})", scope_lbl),
                        "| 模型 | 调用 | 命中率 | 稳定命中(第2轮起) | 未命中 | 新增输入 | 缓存读取 | 输出 | 总 Token | 估算费用(USD) | 峰值Ctx |".to_string(),
                        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |".to_string(),
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
                        format!(
                            "| {}{} | {} | {:.1}% | {} | {} | {} | {} | {} | {} | {} | {} |",
                            name,
                            tag,
                            c.calls,
                            c.cache_hit_rate(),
                            mt,
                            miss,
                            fmt_compact(c.input_tokens),
                            fmt_compact(c.cache_read_tokens),
                            fmt_compact(c.output_tokens),
                            fmt_compact(c.total_tokens()),
                            fmt_cost(&c.cost, c.calls, false),
                            fmt_compact(c.peak_context),
                        )
                    };
                    for (name, c) in &primary {
                        lines.push(fmt_row(name, c, self.cached_agg.by_model_cache.get(name), ""));
                    }
                    for (name, c) in &secondary {
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
                        "AGY CLI Token & API 费用统计 ({})\n- 会话: {} | 调用: {}\n- 估算 API 费用: {} (新输入 {} / 缓存读 {} / 输出 {} · 缓存已省 {})\n- 新增输入: {} | 缓存读取: {} (命中 {:.1}%)\n- 输出: {} | 总 Token: {} (非缓存增量: {})\n- 峰值Ctx: {}",
                        scope_lbl,
                        self.cached_sessions.len(),
                        t.calls,
                        fmt_cost(&t.cost, t.calls, false),
                        fmt_usd_precise(t.cost.input_plus_write_usd()),
                        fmt_usd_precise(t.cost.cache_read_usd),
                        fmt_usd_precise(t.cost.output_usd),
                        fmt_usd_precise(t.cost.cache_saved_usd),
                        fmt_commas(t.input_tokens),
                        fmt_commas(t.cache_read_tokens),
                        t.cache_hit_rate(),
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
                let sel = self
                    .selected_session
                    .as_deref()
                    .and_then(|id| self.cached_sessions.iter().find(|s| s.conversation_id == id));
                if let Some(s) = sel {
                    let ws = s.workspace_short();
                    let cp = s.compaction_count();
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
                    let text = format!(
                        "AGY 会话明细 · {}\n- ID: {} | 时间: {} | 模型: {}{}\n- 轮数: {}{} | 缓存命中: {:.1}%\n- 估算 API 费用: {} (新输入 {} / 缓存读 {} / 输出 {} · 缓存已省 {})\n- 新增输入: {} | 缓存读取: {} | 输出: {}\n- 总 Token: {} (非缓存增量: {}) | 峰值Ctx: {}",
                        s.title,
                        &s.conversation_id[..s.conversation_id.len().min(8)],
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
                        fmt_commas(s.counts.output_tokens),
                        fmt_commas(s.counts.total_tokens()),
                        fmt_commas(s.counts.non_cached_tokens()),
                        fmt_commas(s.counts.peak_context),
                    );
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
                self.active_tab = Tab::Sessions;
                self.prev_token_tab = Tab::Sessions;
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::FilterSessions(filter) => {
                self.session_filter = Some(filter);
                self.active_tab = Tab::Sessions;
                self.prev_token_tab = Tab::Sessions;
                self.scroll_rows = 0;
                let matches = self.display_sessions(true);
                let keep = self
                    .selected_session
                    .as_deref()
                    .map(|id| matches.iter().any(|r| r.session.conversation_id == id))
                    .unwrap_or(false);
                if !keep {
                    self.selected_session = matches.first().map(|r| r.session.conversation_id.clone());
                }
                (false, false)
            }
            ClickAction::ClearSessionFilter => {
                self.session_filter = None;
                self.scroll_rows = 0;
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
        let (sessions, agg) = filter_snapshot(snapshot, self.scope);
        self.cached_sessions = sessions;
        self.cached_agg = agg;
        self.dirty_filter = false;

        let visible = self.display_sessions(true);
        let keep = self
            .selected_session
            .as_ref()
            .map(|id| visible.iter().any(|r| &r.session.conversation_id == id))
            .unwrap_or(false);
        if !keep {
            self.selected_session = visible.first().map(|r| r.session.conversation_id.clone());
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_to_hdc(
        &mut self,
        factories: &D2DFactories,
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

        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut rc);
        }
        let px_w = (rc.right - rc.left).max(1);
        let px_h = (rc.bottom - rc.top).max(1);
        let scale = dpi / 96.0;
        let w = px_w as f32 / scale;
        let h = px_h as f32 / scale;

        let need_new = match &self.surface {
            Some(s) => !s.matches(px_w, px_h, dpi),
            None => true,
        };
        if need_new {
            self.surface = Some(WindowSurface::new(factories, px_w, px_h, dpi)?);
        }

        self.hit_boxes.clear();
        self.plot_rects.clear();
        self.pricing_field_rects = None;
        let theme = theme_for(light_theme);

        let Some(surface) = self.surface.take() else {
            return Ok(());
        };
        let res = surface.render_and_blit(target_hdc, |rt| {
            unsafe {
                rt.Clear(Some(&theme.bg));
            }
            self.draw_header(rt, fonts, theme, w, human, light_theme);
            self.draw_hero_strip(factories, rt, fonts, theme, w, telemetry, human);

            let body = rect_f(20.0, 148.0, (w - 40.0).max(200.0), (h - 166.0).max(200.0));
            match self.active_tab {
                Tab::Overview => self.draw_overview_tab(factories, rt, fonts, theme, body, snapshot, human),
                Tab::Daily => self.draw_daily_tab(rt, fonts, theme, body, human),
                Tab::Weekly => self.draw_weekly_tab(rt, fonts, theme, body, human),
                Tab::Models => self.draw_models_tab(factories, rt, fonts, theme, body, snapshot, human),
                Tab::Sessions => self.draw_sessions_tab(factories, rt, fonts, theme, body, human),
                Tab::System => self.draw_system_tab(factories, rt, fonts, theme, body, snapshot, telemetry),
            }
        });
        if res.is_ok() {
            self.surface = Some(surface);
        }
        res
    }

    // ---------- Minimalist Editorial Header (4-Tab Flat Underline Nav) ----------

    fn draw_header(
        &mut self,
        rt: &ID2D1RenderTarget,
        fonts: &Fonts,
        theme: &Theme,
        w: f32,
        human: bool,
        light_theme: bool,
    ) {
        let h = 46.0;
        draw_line(rt, 0.0, h, w, h, theme.border, 1.0);

        // Minimalist brand mark
        fill_circle(rt, 24.0, h * 0.5, 3.2, theme.accent);
        draw_text(rt, "AGY", &fonts.ui_brand, rect_f(33.0, 0.0, 40.0, h), theme.text);
        draw_text(
            rt,
            if self.cost_mode { "/ 费用" } else { "/ 用量" },
            &fonts.ui_small,
            rect_f(69.0, 1.0, 44.0, h),
            theme.muted,
        );

        // Flattened 5 direct token tabs (System monitor lives on Hero CPU/RAM rings)
        let tabs = [
            (Tab::Overview, "概览", 42.0),
            (Tab::Daily, "按天", 42.0),
            (Tab::Weekly, "按周", 42.0),
            (Tab::Models, "模型", 42.0),
            (Tab::Sessions, "会话", 42.0),
        ];
        let mut tx = 122.0;
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
                fill_round_rect(rt, rect_f(tx + 7.0, h - 2.5, tw - 14.0, 2.5), 1.25, theme.accent);
            }
            self.hit_boxes.push(HitBox { rect: r, action: ClickAction::SelectTab(tab) });
            tx += tw + 6.0;
        }

        // Right minimalist utility actions
        let mut rx = w - 20.0;
        let btn_y = 10.0;
        let btn_h = 26.0;

        let r_ref = rect_f(rx - 40.0, btn_y, 40.0, btn_h);
        self.draw_ghost_btn(rt, fonts, theme, r_ref, "刷新", false, ClickAction::Refresh);
        rx -= 45.0;

        let copied = self.copied_until.map(|t| Instant::now() < t).unwrap_or(false);
        let r_copy = rect_f(rx - 48.0, btn_y, 48.0, btn_h);
        self.draw_ghost_btn(
            rt,
            fonts,
            theme,
            r_copy,
            if copied { "已复制" } else { "复制" },
            copied,
            ClickAction::CopySummary,
        );
        rx -= 53.0;

        let r_km = rect_f(rx - 40.0, btn_y, 40.0, btn_h);
        self.draw_ghost_btn(rt, fonts, theme, r_km, if human { "K/M" } else { "精确" }, false, ClickAction::ToggleHuman);
        rx -= 45.0;

        let r_th = rect_f(rx - 40.0, btn_y, 40.0, btn_h);
        self.draw_ghost_btn(
            rt,
            fonts,
            theme,
            r_th,
            if light_theme { "暗色" } else { "浅色" },
            false,
            ClickAction::ToggleLightTheme,
        );
        rx -= 47.0;

        // Tactile Tok | $ display mode switch (persisted in Registry, hotkey: D)
        let sw_w = 68.0;
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
        let r_tok = rect_f(r_mode.left + 2.0, r_mode.top + 2.0, 34.0, r_mode.bottom - r_mode.top - 4.0);
        let r_usd = rect_f(r_mode.left + 38.0, r_mode.top + 2.0, 28.0, r_mode.bottom - r_mode.top - 4.0);
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
        rx -= sw_w + 9.0;

        draw_line(rt, rx, 14.0, rx, h - 14.0, theme.border, 1.0);
        rx -= 9.0;

        // Scope filter (borderless editorial text toggles; persisted in Registry)
        let scopes = [
            (DateScope::All, "全部", 36.0),
            (DateScope::Days30, "30天", 38.0),
            (DateScope::Days7, "7天", 32.0),
            (DateScope::Today, "今日", 36.0),
        ];
        for (sc, lbl, sw) in scopes {
            let r = rect_f(rx - sw, btn_y, sw, btn_h);
            if r.left > tx + 6.0 {
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
            rx -= sw + 3.0;
        }
    }

    fn draw_ghost_btn(
        &mut self,
        rt: &ID2D1RenderTarget,
        fonts: &Fonts,
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
    fn draw_hero_strip(
        &mut self,
        factories: &D2DFactories,
        rt: &ID2D1RenderTarget,
        fonts: &Fonts,
        theme: &Theme,
        w: f32,
        telemetry: &SystemTelemetry,
        human: bool,
    ) {
        let y = 58.0;
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

        let t = self.cached_agg.total;

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
                    if let Some(b) = brush(rt, col) {
                        unsafe { rt.FillRectangle(&rect_f(bx, bar_y, sw.min(bar_l + bar_w - bx), 2.5), &b) };
                    }
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
                    if let Some(b) = brush(rt, col) {
                        unsafe { rt.FillRectangle(&rect_f(bx, bar_y, sw.min(bar_l + bar_w - bx), 2.5), &b) };
                    }
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
            draw_text(rt, "新增输入 / 输出", &fonts.ui_micro, rect_f(x1 + 18.0, y + 9.0, c2 - 28.0, 14.0), theme.muted);
            let io_str = format!("{}  /  {}", fmt_compact(t.input_tokens), fmt_compact(t.output_tokens));
            draw_text(rt, &io_str, &fonts.mono_kpi, rect_f(x1 + 18.0, y + 26.0, c2 - 28.0, 24.0), theme.text);
            let sub2 = format!("缓存读 {}  ·  峰值Ctx {}", fmt_compact(t.cache_read_tokens), fmt_compact(t.peak_context));
            draw_text(rt, &sub2, &fonts.ui_micro, rect_f(x1 + 18.0, y + 53.0, c2 - 28.0, 14.0), theme.sub);
        }

        // Block 3: Sessions & Calls
        let (root_sess, sub_sess) = self.session_split_counts();
        let sess_lbl = if sub_sess > 0 {
            format!("活跃规模 (含{}子会话)", sub_sess)
        } else {
            "活跃规模".to_string()
        };
        draw_text(rt, &sess_lbl, &fonts.ui_micro, rect_f(x2 + 16.0, y + 9.0, c3 - 22.0, 14.0), theme.muted);
        let disp_sess = if self.group_subagents && root_sess > 0 { root_sess } else { self.cached_sessions.len() };
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
            format!("均单次 {}  ·  {} 天", fmt_compact(avg), self.cached_agg.by_day.len())
        };
        draw_text(rt, &sub3, &fonts.ui_micro, rect_f(x2 + 16.0, y + 53.0, c3 - 22.0, 14.0), theme.sub);

        // Block 4: Clickable Radial Ring Gauges for CPU & RAM (toggles System Monitor view!)
        let b4 = rect_f(x3 + 4.0, y + 4.0, (band.right - x3 - 8.0).max(80.0), h - 8.0);
        let sys_act = self.active_tab == Tab::System;
        let sys_hov = self.mouse_dip.map(|(mx, my)| hit_rect(&b4, mx, my)).unwrap_or(false);
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
        self.hit_boxes.push(HitBox {
            rect: b4,
            action: ClickAction::ToggleSystemView,
        });

        let tel = telemetry.latest;
        let g_cy = y + h * 0.5;
        let c_cx = x3 + 32.0;
        draw_ring_gauge(factories, rt, c_cx, g_cy, 17.0, 3.0, tel.cpu_pct, theme.grid, theme.accent);
        draw_text(rt, &format!("{:.0}%", tel.cpu_pct), &fonts.mono_small_center, rect_f(c_cx - 18.0, g_cy - 8.0, 36.0, 16.0), theme.text);

        let r_cx = x3 + 80.0;
        draw_ring_gauge(factories, rt, r_cx, g_cy, 17.0, 3.0, tel.mem_pct, theme.grid, theme.ram);
        draw_text(rt, &format!("{:.0}%", tel.mem_pct), &fonts.mono_small_center, rect_f(r_cx - 18.0, g_cy - 8.0, 36.0, 16.0), theme.text);

        let tx_l = x3 + 106.0;
        let tx_w = (band.right - tx_l - 10.0).max(60.0);
        draw_text(
            rt,
            if sys_act { "系统监控  ‹ 返回" } else { "CPU / 内存  ›" },
            &fonts.ui_micro,
            rect_f(tx_l, y + 14.0, tx_w, 14.0),
            if sys_act { theme.accent } else if sys_hov { theme.text } else { theme.muted },
        );
        let ram_str = if tx_w >= 104.0 {
            format!("{:.1}/{:.1} GB", tel.mem_used_gb, tel.mem_total_gb)
        } else {
            format!("{:.1}/{:.0}G", tel.mem_used_gb, tel.mem_total_gb)
        };
        draw_text(
            rt,
            &ram_str,
            &fonts.mono_med,
            rect_f(tx_l, y + 31.0, tx_w, 18.0),
            theme.text,
        );
        draw_text(rt, &format!("{} 核实时", tel.cpu_cores), &fonts.ui_micro, rect_f(tx_l, y + 50.0, tx_w, 14.0), theme.sub);
    }

    // ---------- Tab 1: Redesigned 3-Panel Editorial Overview ----------

    #[allow(clippy::too_many_arguments)]
    fn draw_overview_tab(
        &mut self,
        factories: &D2DFactories,
        rt: &ID2D1RenderTarget,
        fonts: &Fonts,
        theme: &Theme,
        body: D2D_RECT_F,
        snapshot: &ScanSnapshot,
        human: bool,
    ) {
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

        let (chart_title, bars, bar_days, empty_msg) = if self.scope == DateScope::Today {
            let active_hours: Vec<usize> = self
                .cached_agg
                .by_hour
                .iter()
                .enumerate()
                .filter_map(|(h, c)| if c.calls > 0 { Some(h) } else { None })
                .collect();
            let items: Vec<BarItem> = if let (Some(&first_h), Some(&last_h)) =
                (active_hours.first(), active_hours.last())
            {
                let now_h = (Local::now().hour() as usize).min(23);
                let end_h = last_h.max(now_h).min(23);
                let mut start_h = first_h.min(end_h.saturating_sub(7));
                if end_h.saturating_sub(start_h) < 7 {
                    start_h = end_h.saturating_sub(7);
                }
                (start_h..=end_h)
                    .map(|h| {
                        let c = self.cached_agg.by_hour[h];
                        let tip = if self.cost_mode {
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
                        };
                        self.make_bar(format!("{:02}h", h), &c, false, tip)
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let title = if self.cost_mode { "今日分时费用 ($)" } else { "今日分时趋势" };
            (title, items, Vec::new(), "今日暂无用量 · 右上角可切换 7天 / 全部")
        } else {
            let n_days: u64 = match self.scope {
                DateScope::Days7 => 7,
                _ => {
                    let earliest = self
                        .cached_agg
                        .by_day
                        .keys()
                        .next()
                        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
                    match earliest {
                        Some(d) => ((today - d).num_days() + 1).clamp(7, 14) as u64,
                        None => 7,
                    }
                }
            };
            let mut days_keys: Vec<Option<String>> = Vec::new();
            let items: Vec<BarItem> = (0..n_days)
                .rev()
                .filter_map(|off| today.checked_sub_days(chrono::Days::new(off)))
                .map(|d| {
                    let k = d.format("%Y-%m-%d").to_string();
                    let c = self.cached_agg.by_day.get(&k).copied().unwrap_or_default();
                    days_keys.push(if c.calls > 0 { Some(k.clone()) } else { None });
                    let tip = if self.cost_mode {
                        vec![
                            format!("{} ({}) · {} 次", k, weekday_cn(d), c.calls),
                            format!("估算费用: {} (省 {})", fmt_cost(&c.cost, c.calls, false), fmt_usd_compact(c.cost.cache_saved_usd)),
                            format!("新输入  : {} ({})", fmt_usd_precise(c.cost.input_plus_write_usd()), fmt_compact(c.input_tokens)),
                            format!("缓存读  : {} ({:.1}%)", fmt_usd_precise(c.cost.cache_read_usd), c.cache_hit_rate()),
                            format!("输出    : {} ({})", fmt_usd_precise(c.cost.output_usd), fmt_compact(c.output_tokens)),
                            format!("总Token : {}", fmt_commas(c.total_tokens())),
                        ]
                    } else {
                        vec![
                            format!("{} ({}) · {} 次", k, weekday_cn(d), c.calls),
                            format!("总计   : {}", fmt_commas(c.total_tokens())),
                            format!("估算费 : {}", fmt_cost(&c.cost, c.calls, false)),
                            format!("新输入 : {}", fmt_commas(c.input_tokens)),
                            format!("缓存读 : {} ({:.1}%)", fmt_commas(c.cache_read_tokens), c.cache_hit_rate()),
                            format!("输出   : {}", fmt_commas(c.output_tokens)),
                        ]
                    };
                    self.make_bar(d.format("%m/%d").to_string(), &c, false, tip)
                })
                .collect();
            let title = if self.cost_mode {
                if self.scope == DateScope::Days7 { "近 7 天费用趋势 ($)" } else { "每日费用趋势 ($)" }
            } else if self.scope == DateScope::Days7 {
                "近 7 天用量趋势"
            } else {
                "每日用量趋势"
            };
            (title, items, days_keys, "暂无用量数据")
        };

        draw_text(rt, chart_title, &fonts.ui_section, rect_f(tl.left + 16.0, tl.top + 11.0, 146.0, 20.0), theme.text);
        self.draw_chart_legend_and_toggle(rt, fonts, theme, tl);
        let tl_plot = rect_f(tl.left + 10.0, tl.top + 38.0, left_w - 20.0, top_h - 46.0);
        self.plot_rects.push(tl_plot);
        let tl_slots = draw_editorial_bars(
            rt,
            fonts,
            theme,
            tl_plot,
            &bars,
            self.fresh_only,
            self.bar_unit(),
            self.mouse_dip,
            empty_msg,
        );
        for (rect, maybe_day) in tl_slots.into_iter().zip(bar_days.into_iter()) {
            if let Some(day) = maybe_day {
                self.hit_boxes.push(HitBox {
                    rect,
                    action: ClickAction::FilterSessions(SessionFilter::Date(day)),
                });
            }
        }

        // 2) Top-Right: This Week vs Last Week (full snapshot context) + Model Comparison
        let tr = rect_f(tl.right + gap, body.top, right_w, top_h);
        fill_round_rect(rt, tr, 10.0, theme.surface);
        stroke_round_rect(rt, tr, 10.0, theme.border, 1.0);

        let split_y = tr.top + (top_h * 0.41).clamp(93.0, 100.0);
        // Clickable upper ISO Week block -> jumps straight to Weekly tab
        let wk_hit = rect_f(tr.left + 4.0, tr.top + 4.0, right_w - 8.0, (split_y - tr.top - 8.0).max(60.0));
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
            &format!("本周 · {}  ›", cur_wk),
            &fonts.ui_section,
            rect_f(tr.left + 16.0, tr.top + 10.0, 160.0, 20.0),
            if wk_hov { theme.accent } else { theme.text },
        );
        draw_text(rt, &iso_week_range_label(&cur_wk), &fonts.mono_small_right, rect_f(tr.right - 120.0, tr.top + 12.0, 104.0, 16.0), theme.muted);

        if self.cost_mode {
            draw_text(
                rt,
                &fmt_cost(&wk_c.cost, wk_c.calls, human),
                &fonts.mono_kpi,
                rect_f(tr.left + 16.0, tr.top + 32.0, (right_w - 150.0).max(90.0), 24.0),
                theme.text,
            );
            let (wow_str, wow_col) = if prev_c.cost.total_usd() > 0.0 {
                let pct = ((wk_c.cost.total_usd() - prev_c.cost.total_usd()) / prev_c.cost.total_usd()) * 100.0;
                (format!("较上周 {:+.1}%", pct), if pct >= 0.0 { theme.accent } else { theme.sage })
            } else {
                ("上周无记录".to_string(), theme.muted)
            };
            draw_text(rt, &wow_str, &fonts.mono_small_right, rect_f(tr.right - 134.0, tr.top + 36.0, 118.0, 18.0), wow_col);
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
                rect_f(tr.left + 16.0, tr.top + 59.0, right_w - 32.0, 15.0),
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
                rect_f(tr.left + 16.0, tr.top + 76.0, right_w - 32.0, 15.0),
                theme.muted,
            );
        } else {
            let wk_avg = if wk_days > 0 { wk_c.total_tokens() / wk_days as u64 } else { 0 };
            draw_text(rt, &fmt_num(wk_c.total_tokens(), human), &fonts.mono_kpi, rect_f(tr.left + 16.0, tr.top + 32.0, (right_w - 150.0).max(90.0), 24.0), theme.text);

            let (wow_str, wow_col) = if prev_c.total_tokens() > 0 {
                let pct = ((wk_c.total_tokens() as f64 - prev_c.total_tokens() as f64)
                    / prev_c.total_tokens() as f64)
                    * 100.0;
                (format!("较上周 {:+.1}%", pct), if pct >= 0.0 { theme.accent } else { theme.sage })
            } else {
                ("上周无记录".to_string(), theme.muted)
            };
            draw_text(rt, &wow_str, &fonts.mono_small_right, rect_f(tr.right - 134.0, tr.top + 36.0, 118.0, 18.0), wow_col);

            draw_text(
                rt,
                &format!("日均 {}  ·  活跃 {} 天  ·  非缓存 {}", fmt_compact(wk_avg), wk_days, fmt_compact(wk_c.non_cached_tokens())),
                &fonts.ui_micro,
                rect_f(tr.left + 16.0, tr.top + 59.0, right_w - 32.0, 15.0),
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
                rect_f(tr.left + 16.0, tr.top + 76.0, right_w - 32.0, 15.0),
                theme.muted,
            );
        }

        draw_line(rt, tr.left + 16.0, split_y, tr.right - 16.0, split_y, theme.border, 1.0);

        // Quiet "模型 | 项目" breakdown toggle inside lower right section
        let mut bx = tr.left + 14.0;
        for (mode, lbl, bw_btn) in [
            (BreakdownMode::Model, "模型分布", 62.0),
            (BreakdownMode::Workspace, "项目分布", 62.0),
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
            bx += bw_btn + 4.0;
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
        } else {
            draw_text(
                rt,
                "占比 · 会话数",
                &fonts.ui_small_right,
                rect_f(tr.right - 136.0, split_y + 9.0, 120.0, 16.0),
                theme.muted,
            );
        }
        let bd_bounds = rect_f(tr.left + 16.0, split_y + 31.0, right_w - 32.0, (tr.bottom - split_y - 37.0).max(24.0));
        match self.breakdown_mode {
            BreakdownMode::Model => self.draw_compact_models(factories, rt, fonts, theme, bd_bounds, human),
            BreakdownMode::Workspace => self.draw_compact_workspaces(rt, fonts, theme, bd_bounds, human),
        }

        // 3) Bottom Full-Width: Recent Active Sessions (grouped by parent session when enabled)
        let bot = rect_f(body.left, tl.bottom + gap, bw, bot_h);
        fill_round_rect(rt, bot, 10.0, theme.surface);
        stroke_round_rect(rt, bot, 10.0, theme.border, 1.0);

        let mut recent_rows = self.display_sessions(false);
        recent_rows.sort_by(|a, b| b.last_time.cmp(&a.last_time));
        let (_, sub_total) = self.session_split_counts();

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
            "点击查看逐轮剖析 →",
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
        let metrics_w = 304.0; // 轮数(40) + 命中率(54) + 费用(62) + 总计(66) + 峰值Ctx(62) + gaps(20)
        let model_x = rx - metrics_w - model_w;
        let title_x = rr_l + 88.0;
        let title_w = (model_x - 10.0 - title_x).max(80.0);

        // Subtle column header row
        let hdr_y = bot.top + 31.0;
        let hdr_h = 20.0;
        draw_text(rt, "时间", &fonts.ui_micro, rect_f(rr_l + 8.0, hdr_y, 74.0, hdr_h), theme.muted);
        draw_text(rt, "会话", &fonts.ui_micro, rect_f(title_x, hdr_y, title_w, hdr_h), theme.muted);
        draw_text(rt, "模型", &fonts.ui_small_right, rect_f(model_x, hdr_y, model_w, hdr_h), theme.muted);
        draw_text(rt, "轮数", &fonts.ui_small_right, rect_f(rx - 300.0, hdr_y, 40.0, hdr_h), theme.muted);
        draw_text(rt, "命中率", &fonts.ui_small_right, rect_f(rx - 256.0, hdr_y, 54.0, hdr_h), theme.muted);
        draw_text(rt, "费用", &fonts.ui_small_right, rect_f(rx - 198.0, hdr_y, 62.0, hdr_h), theme.muted);
        draw_text(rt, "总计", &fonts.ui_small_right, rect_f(rx - 132.0, hdr_y, 66.0, hdr_h), theme.muted);
        draw_text(rt, "峰值Ctx", &fonts.ui_small_right, rect_f(rx - 62.0, hdr_y, 62.0, hdr_h), theme.muted);
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
            let row_h = (avail_h / max_vis as f32).clamp(26.0, 32.0);
            for (i, row) in recent_rows.iter().take(show_n).enumerate() {
                let s = &row.session;
                let ry = list_y + i as f32 * row_h;
                let rr = rect_f(rr_l, ry, rr_w, row_h - 2.0);
                let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&rr, mx, my)).unwrap_or(false);
                if hov {
                    fill_round_rect(rt, rr, 6.0, theme.hover);
                }
                if i > 0 {
                    draw_line(rt, bot.left + 16.0, ry - 1.0, bot.right - 16.0, ry - 1.0, theme.grid, 0.8);
                }
                let t_str = row.last_time.map(|t| t.format("%m/%d %H:%M").to_string()).unwrap_or_else(|| "-".into());
                let hit_str = format!("{:.1}%", row.counts.cache_hit_rate());
                let hit_col = cache_hit_color(theme, &row.counts);
                let cost_str = fmt_cost(&row.counts.cost, row.counts.calls, human);
                let title_disp = if row.is_child_row {
                    format!("↳ {}", s.title)
                } else {
                    s.title.clone()
                };
                let sub_badge = if !row.is_child_row && row.subagent_count > 0 {
                    Some(format!("+{}子", row.subagent_count))
                } else {
                    None
                };

                draw_text(rt, &t_str, &fonts.mono_small, rect_f(rr_l + 8.0, ry, 74.0, row_h - 2.0), theme.muted);
                let (actual_title_w, badge_rect) = if let Some(ref b) = sub_badge {
                    let bw = (factories.measure_text_width(b, &fonts.ui_small_center) + 10.0).ceil();
                    if title_w > bw + 40.0 {
                        let tw = (title_w - bw - 6.0).max(32.0);
                        let nat = factories.measure_text_width(&title_disp, &fonts.ui_reg);
                        let bx = title_x + nat.min(tw) + 6.0;
                        (tw, Some(rect_f(bx, ry + (row_h - 2.0 - 16.0) * 0.5, bw, 16.0)))
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
                    rect_f(title_x, ry, actual_title_w, row_h - 2.0),
                    if row.is_child_row { theme.sub } else { theme.text },
                );
                if let (Some(b), Some(br)) = (sub_badge.as_deref(), badge_rect) {
                    fill_round_rect(rt, br, 8.0, with_alpha(theme.accent, 0.14));
                    draw_text(rt, b, &fonts.ui_small_center, br, theme.accent);
                }
                draw_text(rt, &row.primary_model, &fonts.mono_small_right, rect_f(model_x, ry, model_w, row_h - 2.0), theme.muted);
                draw_text(rt, &format!("{}轮", row.counts.calls), &fonts.mono_small_right, rect_f(rx - 300.0, ry, 40.0, row_h - 2.0), theme.muted);
                draw_text(rt, &hit_str, &fonts.mono_small_right, rect_f(rx - 256.0, ry, 54.0, row_h - 2.0), hit_col);
                draw_text(
                    rt,
                    &cost_str,
                    if self.cost_mode { &fonts.mono_med_right } else { &fonts.mono_small_right },
                    rect_f(rx - 198.0, ry, 62.0, row_h - 2.0),
                    if self.cost_mode { theme.text } else { theme.output },
                );
                draw_text(
                    rt,
                    &fmt_num(row.counts.total_tokens(), human),
                    if self.cost_mode { &fonts.mono_small_right } else { &fonts.mono_med_right },
                    rect_f(rx - 132.0, ry, 66.0, row_h - 2.0),
                    if self.cost_mode { theme.sub } else { theme.text },
                );
                draw_text(rt, &fmt_compact(row.counts.peak_context), &fonts.mono_small_right, rect_f(rx - 62.0, ry, 62.0, row_h - 2.0), theme.sub);

                self.hit_boxes.push(HitBox {
                    rect: rr,
                    action: ClickAction::InspectSession(s.conversation_id.clone()),
                });
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_compact_models(
        &mut self,
        factories: &D2DFactories,
        rt: &ID2D1RenderTarget,
        fonts: &Fonts,
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
        let (primary, secondary) = self.ordered_models(sort);
        if primary.is_empty() && secondary.is_empty() {
            draw_text(rt, "当前范围暂无模型数据", &fonts.ui_small, bounds, theme.muted);
            return;
        }
        let avail_h = (bounds.bottom - bounds.top).max(24.0);
        let max_show: usize = if avail_h >= 94.0 { 4 } else if avail_h >= 66.0 { 3 } else { 2 };
        let show_secondary = !secondary.is_empty()
            && (primary.is_empty() || primary.len() < max_show || max_show >= 3);
        let max_primary = if show_secondary { max_show.saturating_sub(1) } else { max_show };

        let mut rows: Vec<(String, TokenCounts, bool)> = primary
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

        // Pre-format stats & hit strings so right columns take only their exact needed width,
        // giving maximum room to long model names.
        let formatted: Vec<(String, TokenCounts, bool, f32, String, String)> = rows
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

        self.plot_rects.push(bounds);
        let mut hover_tip: Option<(f32, String, String)> = None;

        for (i, (name, c, is_sec, share, stat, hit_s)) in formatted.into_iter().enumerate() {
            let y = bounds.top + i as f32 * row_h;
            if is_sec && i > 0 {
                draw_line(rt, bounds.left, y - 2.0, bounds.right, y - 2.0, theme.border, 0.8);
            }
            let row_r = rect_f(bounds.left - 4.0, y - 1.5, w + 8.0, row_h.min(24.0));
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
                hover_tip = Some((y, name.clone(), detail));
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

            draw_text(rt, &name, &fonts.mono_small, rect_f(bounds.left, y, name_w, 15.0), name_col);
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
                action: ClickAction::FilterSessions(SessionFilter::Model(name)),
            });
        }

        // Compact floating tooltip when hovering a row whose model name is truncated (narrow window)
        if let Some((ry, full_name, detail)) = hover_tip {
            if factories.measure_text_width(&full_name, &fonts.mono_small) > name_w - 2.0 {
                let tw = factories
                    .measure_text_width(&full_name, &fonts.mono_small)
                    .max(factories.measure_text_width(&detail, &fonts.mono_small))
                    + 20.0;
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
                draw_text(rt, &full_name, &fonts.mono_small, rect_f(tx + 9.0, ty + 5.0, tw - 18.0, 15.0), theme.text);
                draw_text(rt, &detail, &fonts.mono_small, rect_f(tx + 9.0, ty + 20.0, tw - 18.0, 15.0), theme.sub);
            }
        }
    }

    fn draw_compact_workspaces(&mut self, rt: &ID2D1RenderTarget, fonts: &Fonts, theme: &Theme, bounds: D2D_RECT_F, human: bool) {
        let mut items: Vec<(String, String, usize, TokenCounts)> = self
            .cached_agg
            .by_workspace
            .iter()
            .map(|(ws, (cnt, c))| {
                let short = workspace_short_name(ws);
                let label = if short.is_empty() { "(未知工作区)".to_string() } else { short };
                (ws.clone(), label, *cnt, *c)
            })
            .collect();
        if self.cost_mode {
            items.sort_by(|a, b| {
                b.3.cost
                    .total_usd()
                    .partial_cmp(&a.3.cost.total_usd())
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.3.total_tokens().cmp(&a.3.total_tokens()))
            });
        } else {
            items.sort_by(|a, b| b.3.total_tokens().cmp(&a.3.total_tokens()));
        }
        if items.is_empty() {
            draw_text(rt, "当前范围暂无项目数据", &fonts.ui_small, bounds, theme.muted);
            return;
        }
        let avail_h = (bounds.bottom - bounds.top).max(24.0);
        let max_show = if avail_h >= 82.0 { 3 } else { 2 };
        let show_n = items.len().min(max_show);
        let total_all_tok = self.cached_agg.total.total_tokens().max(1);
        let total_all_usd = self.cached_agg.total.cost.total_usd();
        let row_h = (avail_h / show_n as f32).clamp(24.0, 34.0);
        let w = bounds.right - bounds.left;
        let palette = [theme.accent, theme.output, theme.ram];

        for (i, (full_ws, name, sess_cnt, c)) in items.into_iter().take(show_n).enumerate() {
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

    fn draw_chart_legend_and_toggle(&mut self, rt: &ID2D1RenderTarget, fonts: &Fonts, theme: &Theme, card: D2D_RECT_F) {
        let tw = 112.0;
        let tr = rect_f(card.right - tw - 14.0, card.top + 9.0, tw, 22.0);
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
        let ly = card.top + 20.0;
        if lx > card.left + 152.0 {
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
    }

    // ---------- Tab 2: Direct Daily Analytics ----------

    fn draw_daily_tab(&mut self, rt: &ID2D1RenderTarget, fonts: &Fonts, theme: &Theme, body: D2D_RECT_F, human: bool) {
        let bw = body.right - body.left;
        let bh = body.bottom - body.top;
        let chart_h = (bh * 0.50).clamp(190.0, 275.0);
        let chart_c = rect_f(body.left, body.top, bw, chart_h);
        fill_round_rect(rt, chart_c, 10.0, theme.surface);
        stroke_round_rect(rt, chart_c, 10.0, theme.border, 1.0);

        draw_text(
            rt,
            if self.cost_mode { "按天费用明细 ($)" } else { "按天用量明细" },
            &fonts.ui_section,
            rect_f(chart_c.left + 16.0, chart_c.top + 10.0, 128.0, 20.0),
            theme.text,
        );
        if self.scope == DateScope::Today {
            let hint_r = rect_f(chart_c.left + 144.0, chart_c.top + 9.0, 108.0, 22.0);
            self.draw_ghost_btn(rt, fonts, theme, hint_r, "仅今日 · 展开全部", true, ClickAction::SelectScope(DateScope::All));
        }
        self.draw_chart_legend_and_toggle(rt, fonts, theme, chart_c);

        let plot = rect_f(chart_c.left + 12.0, chart_c.top + 40.0, bw - 24.0, chart_h - 48.0);
        self.plot_rects.push(plot);
        let tbl_r = rect_f(body.left, chart_c.bottom + 12.0, bw, (body.bottom - chart_c.bottom - 12.0).max(110.0));

        let days_asc: Vec<(&String, &TokenCounts)> = self.cached_agg.by_day.iter().collect();
        let chart_days: Vec<(String, TokenCounts)> = if self.scope != DateScope::Today && days_asc.len() < 7 && !days_asc.is_empty() {
            let today = Local::now().date_naive();
            (0..7u64)
                .rev()
                .filter_map(|off| today.checked_sub_days(chrono::Days::new(off)))
                .map(|d| {
                    let k = d.format("%Y-%m-%d").to_string();
                    (k.clone(), self.cached_agg.by_day.get(&k).copied().unwrap_or_default())
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
                let tip = if self.cost_mode {
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
                };
                self.make_bar(lbl, c, false, tip)
            })
            .collect();
        let day_slots = draw_editorial_bars(
            rt,
            fonts,
            theme,
            plot,
            &bars,
            self.fresh_only,
            self.bar_unit(),
            self.mouse_dip,
            "暂无按天数据",
        );
        for (rect, (day, c)) in day_slots.into_iter().zip(chart_days.iter()) {
            if c.calls > 0 {
                self.hit_boxes.push(HitBox {
                    rect,
                    action: ClickAction::FilterSessions(SessionFilter::Date(day.clone())),
                });
            }
        }

        let rows: Vec<Vec<String>> = days_asc
            .iter()
            .rev()
            .map(|(day, c)| {
                let wd = NaiveDate::parse_from_str(day, "%Y-%m-%d").map(weekday_cn).unwrap_or("-");
                if self.cost_mode {
                    vec![
                        (*day).clone(),
                        wd.into(),
                        c.calls.to_string(),
                        fmt_usd(c.cost.input_plus_write_usd(), human),
                        fmt_usd(c.cost.cache_read_usd, human),
                        fmt_usd(c.cost.output_usd, human),
                        fmt_cost(&c.cost, c.calls, human),
                        fmt_usd(c.cost.non_cached_usd(), human),
                        format!("{:.1}%", c.cache_hit_rate()),
                        fmt_num(c.total_tokens(), human),
                    ]
                } else {
                    vec![
                        (*day).clone(),
                        wd.into(),
                        c.calls.to_string(),
                        fmt_num(c.input_tokens, human),
                        fmt_num(c.cache_read_tokens, human),
                        fmt_num(c.output_tokens, human),
                        fmt_num(c.total_tokens(), human),
                        fmt_cost(&c.cost, c.calls, human),
                        format!("{:.1}%", c.cache_hit_rate()),
                        fmt_num(c.peak_context, human),
                    ]
                }
            })
            .collect();
        let row_actions: Vec<ClickAction> = days_asc
            .iter()
            .rev()
            .map(|(day, _)| ClickAction::FilterSessions(SessionFilter::Date((*day).clone())))
            .collect();
        let cols: &[(&str, f32, bool)] = if self.cost_mode {
            &[
                ("日期", 0.12, false),
                ("星期", 0.06, false),
                ("调用", 0.06, true),
                ("新入费用", 0.11, true),
                ("缓存费用", 0.11, true),
                ("输出费用", 0.10, true),
                ("估算总费", 0.12, true),
                ("非缓存费", 0.11, true),
                ("命中率", 0.09, true),
                ("总 Token", 0.12, true),
            ]
        } else {
            &[
                ("日期", 0.12, false),
                ("星期", 0.06, false),
                ("调用", 0.06, true),
                ("新增输入", 0.11, true),
                ("缓存读取", 0.12, true),
                ("输出", 0.10, true),
                ("总 Token", 0.12, true),
                ("估算费用", 0.11, true),
                ("命中率", 0.09, true),
                ("峰值Ctx", 0.11, true),
            ]
        };
        self.draw_table(rt, fonts, theme, tbl_r, cols, &rows, Some(&row_actions), None, None);
    }

    // ---------- Tab 3: Direct ISO Weekly (with quiet Monthly sub-toggle) ----------

    fn draw_weekly_tab(&mut self, rt: &ID2D1RenderTarget, fonts: &Fonts, theme: &Theme, body: D2D_RECT_F, human: bool) {
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
        if self.scope == DateScope::Today {
            let hint_r = rect_f(gx + 6.0, chart_c.top + 9.0, 108.0, 22.0);
            self.draw_ghost_btn(rt, fonts, theme, hint_r, "仅今日 · 展开全部", true, ClickAction::SelectScope(DateScope::All));
        }
        self.draw_chart_legend_and_toggle(rt, fonts, theme, chart_c);

        let plot = rect_f(chart_c.left + 12.0, chart_c.top + 40.0, bw - 24.0, chart_h - 48.0);
        self.plot_rects.push(plot);
        let tbl_r = rect_f(body.left, chart_c.bottom + 12.0, bw, (body.bottom - chart_c.bottom - 12.0).max(110.0));

        match self.weekly_mode {
            WeeklyMode::Weekly => {
                let weeks_asc: Vec<(&String, &TokenCounts)> = self.cached_agg.by_week.iter().collect();
                let mut wow: HashMap<String, String> = HashMap::new();
                let mut prev: Option<f64> = None;
                for (wk, c) in &weeks_asc {
                    let cur_v = if self.cost_mode { c.cost.total_usd() } else { c.total_tokens() as f64 };
                    if let Some(p) = prev.filter(|&p| p > 0.0) {
                        let d = ((cur_v - p) / p) * 100.0;
                        wow.insert((*wk).clone(), format!("{:+.1}%", d));
                    } else {
                        wow.insert((*wk).clone(), "-".into());
                    }
                    prev = Some(cur_v);
                }
                let start = weeks_asc.len().saturating_sub(16);
                let bars: Vec<BarItem> = weeks_asc[start..]
                    .iter()
                    .map(|(wk, c)| {
                        let ad = self.cached_agg.week_active_days.get(*wk).copied().unwrap_or(1).max(1) as u64;
                        let lbl = wk.split('-').last().unwrap_or(wk).to_string();
                        let tip = if self.cost_mode {
                            vec![
                                format!("{} ({})", wk, iso_week_range_label(wk)),
                                format!(
                                    "估算费用: {} (环比 {})",
                                    fmt_cost(&c.cost, c.calls, false),
                                    wow.get(*wk).map(String::as_str).unwrap_or("-")
                                ),
                                format!("日均费用: {} ({} 活跃天)", fmt_usd_compact(c.cost.total_usd() / ad as f64), ad),
                                format!("新输入  : {}  |  输出: {}", fmt_usd_precise(c.cost.input_plus_write_usd()), fmt_usd_precise(c.cost.output_usd)),
                                format!("缓存读  : {} (省 {})", fmt_usd_precise(c.cost.cache_read_usd), fmt_usd_compact(c.cost.cache_saved_usd)),
                                format!("总Token : {}", fmt_commas(c.total_tokens())),
                            ]
                        } else {
                            vec![
                                format!("{} ({})", wk, iso_week_range_label(wk)),
                                format!("总计   : {} (环比 {})", fmt_commas(c.total_tokens()), wow.get(*wk).map(String::as_str).unwrap_or("-")),
                                format!("估算费 : {} (省 {})", fmt_cost(&c.cost, c.calls, false), fmt_usd_compact(c.cost.cache_saved_usd)),
                                format!("日均   : {} ({} 活跃天)", fmt_commas(c.total_tokens() / ad), ad),
                                format!("新输入 : {}", fmt_commas(c.input_tokens)),
                                format!("缓存读 : {} ({:.1}%)", fmt_commas(c.cache_read_tokens), c.cache_hit_rate()),
                                format!("输出   : {}", fmt_commas(c.output_tokens)),
                            ]
                        };
                        self.make_bar(lbl, c, false, tip)
                    })
                    .collect();
                let wk_slots = draw_editorial_bars(
                    rt,
                    fonts,
                    theme,
                    plot,
                    &bars,
                    self.fresh_only,
                    self.bar_unit(),
                    self.mouse_dip,
                    "暂无按周数据",
                );
                for (rect, (wk, c)) in wk_slots.into_iter().zip(weeks_asc[start..].iter()) {
                    if c.calls > 0 {
                        self.hit_boxes.push(HitBox {
                            rect,
                            action: ClickAction::FilterSessions(SessionFilter::Week((*wk).clone())),
                        });
                    }
                }

                let rows: Vec<Vec<String>> = weeks_asc
                    .iter()
                    .rev()
                    .map(|(wk, c)| {
                        let ad = self.cached_agg.week_active_days.get(*wk).copied().unwrap_or(0);
                        if self.cost_mode {
                            let avg_usd = if ad > 0 { c.cost.total_usd() / ad as f64 } else { 0.0 };
                            vec![
                                (*wk).clone(),
                                iso_week_range_label(wk),
                                format!("{}天", ad),
                                c.calls.to_string(),
                                fmt_usd(avg_usd, human),
                                fmt_usd(c.cost.input_plus_write_usd(), human),
                                fmt_usd(c.cost.cache_read_usd, human),
                                fmt_usd(c.cost.output_usd, human),
                                fmt_cost(&c.cost, c.calls, human),
                                fmt_num(c.total_tokens(), human),
                                wow.get(*wk).cloned().unwrap_or_else(|| "-".into()),
                                format!("{:.1}%", c.cache_hit_rate()),
                            ]
                        } else {
                            let avg = if ad > 0 { c.total_tokens() / ad as u64 } else { 0 };
                            vec![
                                (*wk).clone(),
                                iso_week_range_label(wk),
                                format!("{}天", ad),
                                c.calls.to_string(),
                                fmt_num(avg, human),
                                fmt_num(c.input_tokens, human),
                                fmt_num(c.cache_read_tokens, human),
                                fmt_num(c.output_tokens, human),
                                fmt_num(c.total_tokens(), human),
                                fmt_cost(&c.cost, c.calls, human),
                                wow.get(*wk).cloned().unwrap_or_else(|| "-".into()),
                                format!("{:.1}%", c.cache_hit_rate()),
                            ]
                        }
                    })
                    .collect();
                let wk_actions: Vec<ClickAction> = weeks_asc
                    .iter()
                    .rev()
                    .map(|(wk, _)| ClickAction::FilterSessions(SessionFilter::Week((*wk).clone())))
                    .collect();
                let cols: &[(&str, f32, bool)] = if self.cost_mode {
                    &[
                        ("ISO 周", 0.09, false),
                        ("周一至周日", 0.105, false),
                        ("活跃", 0.05, true),
                        ("调用", 0.05, true),
                        ("日均费用", 0.09, true),
                        ("新入费用", 0.085, true),
                        ("缓存费用", 0.09, true),
                        ("输出费用", 0.08, true),
                        ("估算总费", 0.10, true),
                        ("总 Token", 0.095, true),
                        ("周环比", 0.085, true),
                        ("命中率", 0.08, true),
                    ]
                } else {
                    &[
                        ("ISO 周", 0.09, false),
                        ("周一至周日", 0.105, false),
                        ("活跃", 0.05, true),
                        ("调用", 0.05, true),
                        ("日均Token", 0.09, true),
                        ("新增输入", 0.085, true),
                        ("缓存读取", 0.09, true),
                        ("输出", 0.08, true),
                        ("总 Token", 0.10, true),
                        ("估算费用", 0.095, true),
                        ("周环比", 0.085, true),
                        ("命中率", 0.08, true),
                    ]
                };
                self.draw_table(rt, fonts, theme, tbl_r, cols, &rows, Some(&wk_actions), None, None);
            }
            WeeklyMode::Monthly => {
                let m_asc: Vec<(&String, &TokenCounts)> = self.cached_agg.by_month.iter().collect();
                let bars: Vec<BarItem> = m_asc
                    .iter()
                    .map(|(m, c)| {
                        let tip = if self.cost_mode {
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
                        };
                        self.make_bar((*m).clone(), c, false, tip)
                    })
                    .collect();
                let _ = draw_editorial_bars(
                    rt,
                    fonts,
                    theme,
                    plot,
                    &bars,
                    self.fresh_only,
                    self.bar_unit(),
                    self.mouse_dip,
                    "暂无按月数据",
                );

                let rows: Vec<Vec<String>> = m_asc
                    .iter()
                    .rev()
                    .map(|(m, c)| {
                        if self.cost_mode {
                            vec![
                                (*m).clone(),
                                c.calls.to_string(),
                                fmt_usd(c.cost.input_plus_write_usd(), human),
                                fmt_usd(c.cost.cache_read_usd, human),
                                fmt_usd(c.cost.output_usd, human),
                                fmt_cost(&c.cost, c.calls, human),
                                fmt_usd(c.cost.non_cached_usd(), human),
                                fmt_usd(c.cost.cache_saved_usd, human),
                                format!("{:.1}%", c.cache_hit_rate()),
                                fmt_num(c.total_tokens(), human),
                            ]
                        } else {
                            vec![
                                (*m).clone(),
                                c.calls.to_string(),
                                fmt_num(c.input_tokens, human),
                                fmt_num(c.cache_read_tokens, human),
                                fmt_num(c.output_tokens, human),
                                fmt_num(c.total_tokens(), human),
                                fmt_cost(&c.cost, c.calls, human),
                                fmt_num(c.non_cached_tokens(), human),
                                format!("{:.1}%", c.cache_hit_rate()),
                                fmt_num(c.peak_context, human),
                            ]
                        }
                    })
                    .collect();
                let cols: &[(&str, f32, bool)] = if self.cost_mode {
                    &[
                        ("月份", 0.10, false),
                        ("调用", 0.07, true),
                        ("新入费用", 0.11, true),
                        ("缓存费用", 0.115, true),
                        ("输出费用", 0.10, true),
                        ("估算总费", 0.115, true),
                        ("非缓存费", 0.105, true),
                        ("缓存已省", 0.105, true),
                        ("命中率", 0.08, true),
                        ("总 Token", 0.10, true),
                    ]
                } else {
                    &[
                        ("月份", 0.10, false),
                        ("调用", 0.07, true),
                        ("新增输入", 0.11, true),
                        ("缓存读取", 0.115, true),
                        ("输出", 0.10, true),
                        ("总 Token", 0.115, true),
                        ("估算费用", 0.105, true),
                        ("非缓存", 0.105, true),
                        ("命中率", 0.08, true),
                        ("峰值Ctx", 0.10, true),
                    ]
                };
                self.draw_table(rt, fonts, theme, tbl_r, cols, &rows, None, None, None);
            }
        }
    }

    // ---------- Tab 4: Per-Model Cache Hit Rate Comparison & Full Breakdown ----------

    fn draw_model_view_toggle(
        &mut self,
        rt: &ID2D1RenderTarget,
        fonts: &Fonts,
        theme: &Theme,
        card_left: f32,
        card_top: f32,
    ) -> f32 {
        let mut gx = card_left + 14.0;
        for (mode, lbl, gw) in [
            (ModelViewMode::CacheCompare, "缓存对比", 66.0),
            (ModelViewMode::Pricing, "单价表 ($)", 74.0),
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
        fonts: &Fonts,
        theme: &Theme,
        body: D2D_RECT_F,
        snapshot: &ScanSnapshot,
        human: bool,
    ) {
        if self.model_view == ModelViewMode::Pricing {
            self.draw_pricing_subview(rt, fonts, theme, body, snapshot);
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

        let (primary, secondary) = self.ordered_models(self.model_sort);
        let plot = rect_f(chart_c.left + 14.0, chart_c.top + 38.0, bw - 28.0, chart_h - 46.0);
        self.plot_rects.push(plot);

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
            let max_name_px = primary
                .iter()
                .chain(secondary.iter())
                .map(|(n, _)| factories.measure_text_width(n, &fonts.mono_small))
                .fold(140.0_f32, f32::max);
            let label_w = (max_name_px + 10.0).clamp(146.0, (plot_w * 0.34).max(232.0));
            let right_w = 182.0_f32.min(plot_w * 0.28);
            let track_l = plot.left + label_w + 10.0;
            let track_r = (plot.right - right_w - 10.0).max(track_l + 80.0);
            let track_w = track_r - track_l;
            let axis_h = 18.0;
            let rows_top = plot.top + 4.0;
            let rows_bot = (plot.bottom - axis_h).max(rows_top + 28.0);

            // Vertical percentage gridlines + 85% healthy guide
            for pct in [0, 25, 50, 75, 100] {
                let gx = track_l + (pct as f32 / 100.0) * track_w;
                let col = if pct == 0 || pct == 100 { theme.border } else { theme.grid };
                draw_line(rt, gx, rows_top, gx, rows_bot, col, 1.0);
                if pct == 0 || pct == 50 || pct == 100 {
                    draw_text(
                        rt,
                        &format!("{pct}%"),
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

            let max_chart_rows = (((rows_bot - rows_top) / 26.0).floor() as usize).clamp(2, 6);
            let show_sec_in_chart = !secondary.is_empty() && (primary.is_empty() || max_chart_rows >= 2);
            let max_pri = if show_sec_in_chart { max_chart_rows - 1 } else { max_chart_rows };
            let mut chart_items: Vec<(String, TokenCounts, bool)> = primary
                .iter()
                .take(max_pri)
                .map(|(n, c)| (n.clone(), *c, false))
                .collect();
            if show_sec_in_chart {
                if let Some((n, c)) = secondary.first() {
                    chart_items.push((n.clone(), *c, true));
                }
            }

            let n_rows = chart_items.len().max(1);
            let row_h = ((rows_bot - rows_top) / n_rows as f32).clamp(24.0, 36.0);
            let mut hovered_tip: Option<(f32, f32, Vec<String>)> = None;

            for (i, (name, c, is_sec)) in chart_items.iter().enumerate() {
                let ry = rows_top + i as f32 * row_h;
                if *is_sec && i > 0 {
                    draw_line(rt, plot.left, ry - 1.5, plot.right, ry - 1.5, theme.border, 0.9);
                }
                let rr = rect_f(plot.left - 4.0, ry + 1.0, (plot.right - plot.left) + 8.0, row_h - 3.0);
                let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&rr, mx, my)).unwrap_or(false);
                if hov {
                    fill_round_rect(rt, rr, 6.0, with_alpha(theme.text, 0.04));
                }

                let hit = c.cache_hit_rate();
                let col = if *is_sec { theme.cache } else { cache_hit_color(theme, c) };
                let name_col = if hov {
                    theme.accent
                } else if *is_sec {
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

                let hit_txt = format!("{:.1}%", hit);
                let meta_txt = if *is_sec {
                    format!("{}次 · {}", c.calls, fmt_cost(&c.cost, c.calls, true))
                } else if self.cost_mode {
                    format!("{}次 · {}", c.calls, fmt_cost(&c.cost, c.calls, human))
                } else {
                    format!("{}次 · {}", c.calls, fmt_num(c.total_tokens(), human))
                };
                draw_text(
                    rt,
                    &hit_txt,
                    if *is_sec { &fonts.mono_small_right } else { &fonts.mono_med_right },
                    rect_f(track_r + 8.0, ry, 50.0, row_h),
                    if *is_sec { theme.muted } else { col },
                );
                draw_text(
                    rt,
                    &meta_txt,
                    &fonts.mono_small_right,
                    rect_f(track_r + 62.0, ry, (plot.right - (track_r + 62.0)).max(50.0), row_h),
                    if self.cost_mode && !*is_sec { theme.output } else { theme.muted },
                );

                self.hit_boxes.push(HitBox {
                    rect: rr,
                    action: ClickAction::FilterSessions(SessionFilter::Model(name.clone())),
                });

                if hov {
                    let mcs = self.cached_agg.by_model_cache.get(name);
                    let mt_str = mcs
                        .and_then(ModelCacheStats::multi_turn_hit_rate)
                        .map(|r| format!("{r:.1}% (第2轮起 {} 次)", mcs.map(|m| m.multi_turn_calls).unwrap_or(0)))
                        .unwrap_or_else(|| "-".into());
                    let mut tip = vec![
                        if *is_sec {
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
                    if !*is_sec {
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
                    let (mx, _) = self.mouse_dip.unwrap_or((track_l + track_w * 0.5, ry));
                    hovered_tip = Some((mx, ry, tip));
                }
            }

            if let Some((mx, ry, lines)) = hovered_tip {
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
                let tx = (mx + 14.0).min(plot.right - tw - 4.0).max(plot.left + 4.0);
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
        let secondary_from = if secondary.is_empty() { None } else { Some(primary.len()) };

        let mut rows: Vec<Vec<String>> = Vec::with_capacity(primary.len() + secondary.len());
        let mut actions: Vec<ClickAction> = Vec::with_capacity(primary.len() + secondary.len());

        let cost_mode = self.cost_mode;
        let mut push_model_row = |name: String, c: TokenCounts, is_sec: bool| {
            let mcs = self.cached_agg.by_model_cache.get(&name);
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
                rows.push(vec![
                    name.clone(),
                    c.calls.to_string(),
                    format!("{:.1}%", c.cache_hit_rate()),
                    mt_str,
                    fmt_usd(c.cost.input_plus_write_usd(), human),
                    fmt_usd(c.cost.cache_read_usd, human),
                    fmt_usd(c.cost.output_usd, human),
                    fmt_cost(&c.cost, c.calls, human),
                    fmt_usd(c.cost.cache_saved_usd, human),
                    fmt_num(c.total_tokens(), human),
                    fmt_num(c.peak_context, human),
                ]);
            } else {
                rows.push(vec![
                    name.clone(),
                    c.calls.to_string(),
                    format!("{:.1}%", c.cache_hit_rate()),
                    mt_str,
                    miss_str,
                    fmt_num(c.input_tokens, human),
                    fmt_num(c.cache_read_tokens, human),
                    fmt_num(c.output_tokens, human),
                    fmt_num(c.total_tokens(), human),
                    fmt_cost(&c.cost, c.calls, human),
                    fmt_num(c.peak_context, human),
                ]);
            }
            actions.push(ClickAction::FilterSessions(SessionFilter::Model(name)));
        };

        for (name, c) in primary {
            push_model_row(name, c, false);
        }
        for (name, c) in secondary {
            push_model_row(name, c, true);
        }

        let cols: &[(&str, f32, bool)] = if self.cost_mode {
            &[
                ("模型", 0.255, false),
                ("调用", 0.05, true),
                ("命中率", 0.07, true),
                ("稳定命中", 0.075, true),
                ("新入费用", 0.08, true),
                ("缓存费用", 0.08, true),
                ("输出费用", 0.075, true),
                ("估算总费", 0.085, true),
                ("缓存已省", 0.08, true),
                ("总 Token", 0.08, true),
                ("峰值Ctx", 0.07, true),
            ]
        } else {
            &[
                ("模型", 0.255, false),
                ("调用", 0.05, true),
                ("命中率", 0.07, true),
                ("稳定命中", 0.075, true),
                ("未命中", 0.055, true),
                ("新增输入", 0.085, true),
                ("缓存读取", 0.09, true),
                ("输出", 0.07, true),
                ("总 Token", 0.09, true),
                ("估算费用", 0.08, true),
                ("峰值Ctx", 0.08, true),
            ]
        };

        self.draw_table(rt, fonts, theme, tbl_r, cols, &rows, Some(&actions), secondary_from, None);
    }

    fn draw_pricing_subview(
        &mut self,
        rt: &ID2D1RenderTarget,
        fonts: &Fonts,
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
        let path_hint = snapshot
            .pricing_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "pricing.json".to_string());
        let line1 = format!(
            "优先级：精确模型名 > 前缀匹配（如 gemini-4-neon 匹配全档位） > 内置默认  ·  Tab 切框 / Enter 保存"
        );
        let line2 = format!(
            "共 {} 条单价 · 已自定义 {} 项 · 写入本地 {}（对 ~/.gemini 保持绝对只读）",
            snapshot.pricing_rows.len(),
            snapshot.custom_pricing_count,
            path_hint,
        );
        draw_text(rt, &line1, &fonts.ui_micro, rect_f(inner_l, edit_c.top + 89.0, inner_r - inner_l, 15.0), theme.sub);
        draw_text(rt, &line2, &fonts.ui_micro, rect_f(inner_l, edit_c.top + 109.0, inner_r - inner_l, 15.0), theme.muted);

        // Full-width scrollable API pricing table
        let tbl_r = rect_f(body.left, edit_c.bottom + 12.0, bw, (body.bottom - edit_c.bottom - 12.0).max(110.0));
        let secondary_from = snapshot.pricing_rows.iter().position(|r| r.is_reference);
        let selected_idx = self.pricing_selected_model.as_deref().and_then(|sel| {
            snapshot
                .pricing_rows
                .iter()
                .position(|r| r.model.eq_ignore_ascii_case(sel))
        });

        let rows: Vec<Vec<String>> = snapshot
            .pricing_rows
            .iter()
            .map(|r| {
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
                vec![
                    r.model.clone(),
                    r.source.label_cn().to_string(),
                    rule_col,
                    in_s,
                    cr_s,
                    cw_s,
                    out_s,
                ]
            })
            .collect();
        let actions: Vec<ClickAction> = (0..snapshot.pricing_rows.len())
            .map(ClickAction::PickPricingRow)
            .collect();

        self.draw_table(
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
            &rows,
            Some(&actions),
            secondary_from,
            selected_idx,
        );
    }

    // ---------- Tab 4: Session Explorer + Per-Session Cache Hit Rate & Turn Anatomy ----------

    #[allow(clippy::too_many_arguments)]
    fn draw_sessions_tab(
        &mut self,
        factories: &D2DFactories,
        rt: &ID2D1RenderTarget,
        fonts: &Fonts,
        theme: &Theme,
        body: D2D_RECT_F,
        human: bool,
    ) {
        let bw = body.right - body.left;
        let bh = body.bottom - body.top;
        let top_h = (bh * 0.48).clamp(160.0, 265.0);
        let top_c = rect_f(body.left, body.top, bw, top_h);
        fill_round_rect(rt, top_c, 10.0, theme.surface);
        stroke_round_rect(rt, top_c, 10.0, theme.border, 1.0);

        let sorted = self.display_sessions(true);
        let (root_total, sub_total) = self.session_split_counts();

        draw_text(rt, "会话记录", &fonts.ui_section, rect_f(top_c.left + 16.0, top_c.top + 9.0, 66.0, 20.0), theme.text);
        let cnt_str = if self.session_filter.is_some() {
            format!("{} 条", sorted.len())
        } else if self.group_subagents && sub_total > 0 {
            format!("{} 主 (+{}子)", root_total, sub_total)
        } else if sub_total > 0 {
            format!("{} 个 ({}子)", sorted.len(), sub_total)
        } else {
            format!("{} 个", sorted.len())
        };
        let cnt_w = (factories.measure_text_width(&cnt_str, &fonts.ui_micro) + 6.0).clamp(44.0, 108.0);
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
            (SessionSort::NonCached, "非缓存", 46.0),
            (SessionSort::TotalTokens, "总Token", 54.0),
            (SessionSort::HitRateLow, "命中低→高", 66.0),
            (SessionSort::PeakContext, "峰值Ctx", 52.0),
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

        // Subagent grouping toggle button (shown whenever subagents exist or ungrouped)
        let mut left_cursor = top_c.left + 84.0 + cnt_w;
        if sub_total > 0 || !self.group_subagents {
            let grp_lbl = if self.group_subagents { "子会话:归组" } else { "子会话:独立" };
            let grp_w = 76.0;
            if left_cursor + grp_w + 8.0 < sx {
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
            let chip_lbl = f.label();
            let max_w = (sx - 8.0 - left_cursor).max(0.0);
            if max_w >= 72.0 {
                let measured = factories.measure_text_width(&chip_lbl, &fonts.ui_small_center) + 18.0;
                let est_w = measured.clamp(84.0, max_w);
                let chip_r = rect_f(left_cursor, top_c.top + 8.0, est_w, 22.0);
                self.draw_ghost_btn(rt, fonts, theme, chip_r, &chip_lbl, true, ClickAction::ClearSessionFilter);
            }
        }

        let rr_l = top_c.left + 10.0;
        let rr_w = bw - 24.0;
        let rx = rr_l + rr_w - 8.0;
        let list_y = top_c.top + 55.0;
        let row_h = 27.0;
        let vis = (((top_c.bottom - 8.0 - list_y) / row_h).floor() as usize).max(1);
        self.sessions_vis_rows = vis;
        self.max_scroll = sorted.len().saturating_sub(vis);
        if self.scroll_rows > self.max_scroll {
            self.scroll_rows = self.max_scroll;
        }

        // Dynamically size model column to fit long model names cleanly
        let max_model_px = sorted
            .iter()
            .skip(self.scroll_rows)
            .take(vis)
            .map(|r| factories.measure_text_width(&r.primary_model, &fonts.mono_small_right))
            .fold(132.0_f32, f32::max);
        let model_cap = (rr_w * 0.27).clamp(152.0, 224.0);
        let model_w = (max_model_px + 8.0).clamp(132.0, model_cap);
        let metrics_w = 304.0;
        let model_x = rx - metrics_w - model_w;
        let title_x = rr_l + 144.0;
        let title_w = (model_x - 10.0 - title_x).max(80.0);

        // Column headers
        let hdr_y = top_c.top + 32.0;
        let hdr_h = 19.0;
        draw_text(rt, "ID", &fonts.ui_micro, rect_f(rr_l + 8.0, hdr_y, 52.0, hdr_h), theme.muted);
        draw_text(rt, "时间", &fonts.ui_micro, rect_f(rr_l + 64.0, hdr_y, 74.0, hdr_h), theme.muted);
        draw_text(rt, "会话", &fonts.ui_micro, rect_f(title_x, hdr_y, title_w, hdr_h), theme.muted);
        draw_text(rt, "模型", &fonts.ui_small_right, rect_f(model_x, hdr_y, model_w, hdr_h), theme.muted);
        draw_text(rt, "轮数", &fonts.ui_small_right, rect_f(rx - 300.0, hdr_y, 40.0, hdr_h), theme.muted);
        draw_text(rt, "命中率", &fonts.ui_small_right, rect_f(rx - 256.0, hdr_y, 54.0, hdr_h), theme.muted);
        draw_text(rt, "费用", &fonts.ui_small_right, rect_f(rx - 198.0, hdr_y, 62.0, hdr_h), theme.muted);
        draw_text(rt, "总计", &fonts.ui_small_right, rect_f(rx - 132.0, hdr_y, 66.0, hdr_h), theme.muted);
        draw_text(rt, "峰值Ctx", &fonts.ui_small_right, rect_f(rx - 62.0, hdr_y, 62.0, hdr_h), theme.muted);
        draw_line(rt, top_c.left + 14.0, top_c.top + 52.0, top_c.right - 14.0, top_c.top + 52.0, theme.border, 1.0);

        if sorted.is_empty() {
            let empty_txt = if self.session_filter.is_some() {
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
            for (vi, row) in sorted.iter().skip(self.scroll_rows).take(vis).enumerate() {
                let s = &row.session;
                let ry = list_y + vi as f32 * row_h;
                let rr = rect_f(rr_l, ry, rr_w, row_h - 2.0);
                let sel = self.selected_session.as_deref() == Some(s.conversation_id.as_str());
                let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&rr, mx, my)).unwrap_or(false);
                if sel {
                    fill_round_rect(rt, rr, 6.0, theme.hover);
                    fill_round_rect(rt, rect_f(rr.left, rr.top + 5.0, 2.5, rr.bottom - rr.top - 10.0), 1.25, theme.accent);
                } else if hov {
                    fill_round_rect(rt, rr, 6.0, with_alpha(theme.text, 0.04));
                }
                let sid = &s.conversation_id[..s.conversation_id.len().min(8)];
                let t_str = row.last_time.map(|t| t.format("%m/%d %H:%M").to_string()).unwrap_or_else(|| "-".into());
                let hit_str = format!("{:.1}%", row.counts.cache_hit_rate());
                let hit_col = cache_hit_color(theme, &row.counts);
                let cost_str = fmt_cost(&row.counts.cost, row.counts.calls, human);
                let title_disp = if row.is_child_row {
                    format!("↳ {}", s.title)
                } else {
                    s.title.clone()
                };
                let sub_badge = if !row.is_child_row && row.subagent_count > 0 {
                    Some(format!("+{}子", row.subagent_count))
                } else {
                    None
                };

                draw_text(rt, sid, &fonts.mono_small, rect_f(rr_l + 8.0, ry, 52.0, row_h - 2.0), theme.muted);
                draw_text(rt, &t_str, &fonts.mono_small, rect_f(rr_l + 64.0, ry, 74.0, row_h - 2.0), theme.sub);
                let (actual_title_w, badge_rect) = if let Some(ref b) = sub_badge {
                    let bw = (factories.measure_text_width(b, &fonts.ui_small_center) + 10.0).ceil();
                    if title_w > bw + 40.0 {
                        let tw = (title_w - bw - 6.0).max(32.0);
                        let nat = factories.measure_text_width(&title_disp, &fonts.ui_reg);
                        let bx = title_x + nat.min(tw) + 6.0;
                        (tw, Some(rect_f(bx, ry + (row_h - 2.0 - 16.0) * 0.5, bw, 16.0)))
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
                    rect_f(title_x, ry, actual_title_w, row_h - 2.0),
                    if row.is_child_row { theme.sub } else { theme.text },
                );
                if let (Some(b), Some(br)) = (sub_badge.as_deref(), badge_rect) {
                    fill_round_rect(rt, br, 8.0, with_alpha(theme.accent, 0.14));
                    draw_text(rt, b, &fonts.ui_small_center, br, theme.accent);
                }

                draw_text(rt, &row.primary_model, &fonts.mono_small_right, rect_f(model_x, ry, model_w, row_h - 2.0), theme.muted);
                draw_text(rt, &format!("{}轮", row.counts.calls), &fonts.mono_small_right, rect_f(rx - 300.0, ry, 40.0, row_h - 2.0), theme.sub);
                draw_text(rt, &hit_str, &fonts.mono_small_right, rect_f(rx - 256.0, ry, 54.0, row_h - 2.0), hit_col);
                draw_text(
                    rt,
                    &cost_str,
                    if self.cost_mode { &fonts.mono_med_right } else { &fonts.mono_small_right },
                    rect_f(rx - 198.0, ry, 62.0, row_h - 2.0),
                    if self.cost_mode { theme.text } else { theme.output },
                );
                draw_text(
                    rt,
                    &fmt_num(row.counts.total_tokens(), human),
                    if self.cost_mode { &fonts.mono_small_right } else { &fonts.mono_med_right },
                    rect_f(rx - 132.0, ry, 66.0, row_h - 2.0),
                    if self.cost_mode { theme.sub } else { theme.text },
                );
                draw_text(rt, &fmt_compact(row.counts.peak_context), &fonts.mono_small_right, rect_f(rx - 62.0, ry, 62.0, row_h - 2.0), theme.sub);

                self.hit_boxes.push(HitBox {
                    rect: rr,
                    action: ClickAction::SelectSession(s.conversation_id.clone()),
                });
            }

            if self.max_scroll > 0 {
                let track_h = (top_c.bottom - 10.0 - list_y).max(20.0);
                let thumb_h = ((vis as f32 / sorted.len() as f32) * track_h).clamp(24.0, track_h);
                let ratio = self.scroll_rows as f32 / self.max_scroll as f32;
                let thumb_y = list_y + ratio * (track_h - thumb_h);
                fill_round_rect(rt, rect_f(top_c.right - 7.0, thumb_y, 3.0, thumb_h), 1.5, theme.muted);
            }
        }

        // Bottom: Turn-by-Turn Context Anatomy + Per-Session Cost & Subagent Drill-Down
        let bot_c = rect_f(body.left, top_c.bottom + 12.0, bw, (body.bottom - top_c.bottom - 12.0).max(140.0));
        fill_round_rect(rt, bot_c, 10.0, theme.surface);
        stroke_round_rect(rt, bot_c, 10.0, theme.border, 1.0);

        let sel = self
            .selected_session
            .as_deref()
            .and_then(|id| self.cached_sessions.iter().find(|s| s.conversation_id == id))
            .cloned();

        if let Some(s) = sel {
            let (sub_cnt, sub_agg) = self.count_subagents_of(&s.conversation_id);
            let hdr_prefix = if s.is_subagent {
                if self.cost_mode { "逐轮费用 [子会话]" } else { "逐轮剖析 [子会话]" }
            } else if self.cost_mode {
                "逐轮费用剖析"
            } else {
                "逐轮剖析"
            };
            let hdr = format!("{} · {}", hdr_prefix, s.title);
            draw_text(rt, &hdr, &fonts.ui_section, rect_f(bot_c.left + 16.0, bot_c.top + 9.0, (bw - 320.0).max(160.0), 19.0), theme.text);

            let mut meta = if self.cost_mode {
                format!(
                    "{} · 费用 {} · 省 {} · {}轮 · 命中 {:.1}% · 总计 {}",
                    s.primary_model(),
                    fmt_cost(&s.counts.cost, s.counts.calls, false),
                    fmt_usd_compact(s.counts.cost.cache_saved_usd),
                    s.turns.len(),
                    s.counts.cache_hit_rate(),
                    fmt_compact(s.counts.total_tokens()),
                )
            } else {
                format!(
                    "{} · {}轮 · 命中 {:.1}% · 费用 {} · 总计 {} · 峰值Ctx {}",
                    s.primary_model(),
                    s.turns.len(),
                    s.counts.cache_hit_rate(),
                    fmt_cost(&s.counts.cost, s.counts.calls, false),
                    fmt_compact(s.counts.total_tokens()),
                    fmt_compact(s.counts.peak_context),
                )
            };
            if sub_cnt > 0 {
                meta.push_str(&format!(
                    " · +{}子({})",
                    sub_cnt,
                    fmt_cost_compact(&sub_agg.cost, sub_agg.calls)
                ));
            } else if s.is_subagent && !s.parent_title.is_empty() {
                meta.push_str(&format!(" · 所属: {}", s.parent_title));
            }
            let cp = s.compaction_count();
            if cp > 0 {
                meta.push_str(&format!(" · {}压缩", cp));
            }
            let (misses, _) = s.cache_miss_stats();
            if misses > 0 {
                meta.push_str(&format!(" · {}未命中", misses));
            }
            let ws = s.workspace_short();
            if !ws.is_empty() {
                meta.push_str(&format!(" · {}", ws));
            }

            let s_copied = self.session_copied_until.map(|t| Instant::now() < t).unwrap_or(false);
            let copy_r = rect_f(bot_c.right - 72.0, bot_c.top + 27.0, 58.0, 17.0);
            let mut right_edge = copy_r.left - 6.0;
            let already_in_parent_group = matches!(
                &self.session_filter,
                Some(SessionFilter::Parent(pid, _))
                    if *pid == s.conversation_id || s.parent_id.as_deref() == Some(pid.as_str())
            );
            if sub_cnt > 0 && !already_in_parent_group {
                let lbl = format!("查看{}子会话 ›", sub_cnt);
                let btn_w = (factories.measure_text_width(&lbl, &fonts.ui_small_center) + 14.0).clamp(76.0, 118.0);
                let sub_r = rect_f(right_edge - btn_w, bot_c.top + 27.0, btn_w, 17.0);
                self.draw_ghost_btn(
                    rt,
                    fonts,
                    theme,
                    sub_r,
                    &lbl,
                    true,
                    ClickAction::FilterSessions(SessionFilter::Parent(
                        s.conversation_id.clone(),
                        s.title.clone(),
                    )),
                );
                right_edge = sub_r.left - 6.0;
            } else if s.is_subagent && !already_in_parent_group {
                if let Some(pid) = s.parent_id.clone() {
                    let ptitle = if s.parent_title.is_empty() {
                        pid[..pid.len().min(8)].to_string()
                    } else {
                        s.parent_title.clone()
                    };
                    let sub_r = rect_f(right_edge - 82.0, bot_c.top + 27.0, 82.0, 17.0);
                    self.draw_ghost_btn(
                        rt,
                        fonts,
                        theme,
                        sub_r,
                        "查看所属任务 ›",
                        true,
                        ClickAction::FilterSessions(SessionFilter::Parent(pid, ptitle)),
                    );
                    right_edge = sub_r.left - 6.0;
                }
            }
            draw_text(
                rt,
                &meta,
                &fonts.mono_small,
                rect_f(bot_c.left + 16.0, bot_c.top + 28.0, (right_edge - bot_c.left - 16.0).max(120.0), 15.0),
                theme.sub,
            );
            self.draw_ghost_btn(
                rt,
                fonts,
                theme,
                copy_r,
                if s_copied { "已复制" } else { "复制明细" },
                s_copied,
                ClickAction::CopySessionDetail,
            );
            self.draw_chart_legend_and_toggle(rt, fonts, theme, bot_c);

            let mut prev_ts: Option<chrono::DateTime<chrono::Utc>> = None;
            let bars: Vec<BarItem> = s
                .turns
                .iter()
                .enumerate()
                .map(|(idx, t)| {
                    let lbl = if t.step_index >= 0 { format!("#{}", t.step_index) } else { "-".into() };
                    let ts = t.timestamp_utc.map(|u| u.with_timezone(&Local).format("%H:%M:%S").to_string()).unwrap_or_else(|| "-".into());
                    let gap_s = match (prev_ts, t.timestamp_utc) {
                        (Some(p), Some(cur)) => {
                            let g = fmt_gap((cur - p).num_seconds());
                            if g.is_empty() { String::new() } else { format!(" · 隔 {}", g) }
                        }
                        _ => String::new(),
                    };
                    if t.timestamp_utc.is_some() {
                        prev_ts = t.timestamp_utc;
                    }
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
                    let tip = vec![
                        step_hdr,
                        format!("模型   : {}", t.model),
                        cost_line,
                        format!("新输入 : {}", fmt_commas(t.input_tokens)),
                        format!("缓存读 : {} ({:.1}%)", fmt_commas(t.cache_read_tokens), t.cache_hit_rate()),
                        format!("Ctx    : {}", fmt_commas(t.total_input())),
                        format!("输出   : {}", fmt_commas(t.output_tokens)),
                    ];
                    self.make_turn_bar(lbl, t, tip)
                })
                .collect();
            let plot = rect_f(bot_c.left + 12.0, bot_c.top + 48.0, bw - 24.0, (bot_c.bottom - bot_c.top - 56.0).max(60.0));
            self.plot_rects.push(plot);
            let _ = draw_editorial_bars(
                rt,
                fonts,
                theme,
                plot,
                &bars,
                self.fresh_only,
                self.bar_unit(),
                self.mouse_dip,
                "无逐轮记录",
            );
        } else {
            draw_text(rt, "点击上方会话查看逐轮上下文增长趋势", &fonts.ui_reg_center, bot_c, theme.muted);
        }
    }

    // ---------- System Telemetry Studio (Opened via Hero CPU/RAM Ring Block) ----------

    #[allow(clippy::too_many_arguments)]
    fn draw_system_tab(
        &mut self,
        factories: &D2DFactories,
        rt: &ID2D1RenderTarget,
        fonts: &Fonts,
        theme: &Theme,
        body: D2D_RECT_F,
        snapshot: &ScanSnapshot,
        telemetry: &SystemTelemetry,
    ) {
        let bw = body.right - body.left;
        let bh = body.bottom - body.top;
        let footer_h = 26.0;
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
        self.plot_rects.push(rect_f(pl, pt_y, pw, ph));

        for pct in [0, 50, 100] {
            let y = pb_y - (pct as f32 / 100.0) * ph;
            draw_line(rt, pl, y, pr, y, if pct == 0 { theme.border } else { theme.grid }, 1.0);
            draw_text(rt, &format!("{pct}%"), &fonts.mono_small_right, rect_f(chart_c.left + 4.0, y - 7.0, 34.0, 14.0), theme.muted);
        }

        if telemetry.history.len() >= 2 {
            let max_pts = 90usize;
            let n = telemetry.history.len();
            let dx = pw / (max_pts - 1) as f32;
            let x0 = pl + (max_pts.saturating_sub(n)) as f32 * dx;
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
        draw_text(rt, &format!("{:.0}%", cur.cpu_pct), &fonts.mono_kpi_center, rect_f(g1_x - 32.0, g1_y - 12.0, 64.0, 24.0), load_color(theme, cur.cpu_pct, 60.0, 85.0));
        draw_text(rt, "处理器负载", &fonts.ui_section, rect_f(c_left.left + 114.0, cy_top + 26.0, half_w - 130.0, 20.0), theme.text);
        draw_text(rt, &format!("{} 个逻辑核心  ·  1 秒原生 Win32 采样", cur.cpu_cores), &fonts.ui_small, rect_f(c_left.left + 114.0, cy_top + 52.0, half_w - 130.0, 18.0), theme.sub);
        draw_text(rt, "直接调用 GetSystemTimes，无命令行子进程", &fonts.ui_micro, rect_f(c_left.left + 114.0, cy_top + 76.0, half_w - 130.0, 16.0), theme.muted);

        let c_right = rect_f(c_left.right + 12.0, cy_top, half_w, cards_h);
        fill_round_rect(rt, c_right, 10.0, theme.surface);
        stroke_round_rect(rt, c_right, 10.0, theme.border, 1.0);
        let g2_x = c_right.left + 58.0;
        let g2_y = cy_top + cards_h * 0.5;
        draw_ring_gauge(factories, rt, g2_x, g2_y, 34.0, 5.5, cur.mem_pct, theme.grid, theme.ram);
        draw_text(rt, &format!("{:.0}%", cur.mem_pct), &fonts.mono_kpi_center, rect_f(g2_x - 32.0, g2_y - 12.0, 64.0, 24.0), load_color(theme, cur.mem_pct, 75.0, 88.0));
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
        let foot = format!(
            "严格只读 · 0 写盘 · 不碰 SQLite · 增量扫描命中 {} / {} 文件 ({:.1} ms){} · {}",
            snapshot.files_cached,
            snapshot.files_cached + snapshot.files_reparsed,
            snapshot.scan_ms,
            pricing_note,
            snapshot.data_dir.display()
        );
        draw_text(rt, &foot, &fonts.mono_small, rect_f(body.left + 4.0, c_left.bottom + 8.0, bw - 8.0, footer_h), theme.muted);
    }

    // ---------- Minimalist Editorial Table ----------

    #[allow(clippy::too_many_arguments)]
    fn draw_table(
        &mut self,
        rt: &ID2D1RenderTarget,
        fonts: &Fonts,
        theme: &Theme,
        bounds: D2D_RECT_F,
        cols: &[(&str, f32, bool)],
        rows: &[Vec<String>],
        row_actions: Option<&[ClickAction]>,
        secondary_from: Option<usize>,
        selected_row: Option<usize>,
    ) {
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

        if rows.is_empty() {
            draw_text(rt, "暂无明细数据", &fonts.ui_reg_center, rect_f(bounds.left, bounds.top + hdr_h, bounds.right - bounds.left, bounds.bottom - bounds.top - hdr_h), theme.muted);
            self.max_scroll = 0;
            return;
        }

        let row_h = 28.0;
        let body_top = bounds.top + hdr_h + 2.0;
        let vis_rows = (((bounds.bottom - 6.0 - body_top) / row_h).floor() as usize).max(1);
        self.max_scroll = rows.len().saturating_sub(vis_rows);
        if self.scroll_rows > self.max_scroll {
            self.scroll_rows = self.max_scroll;
        }

        for (vi, row) in rows.iter().skip(self.scroll_rows).take(vis_rows).enumerate() {
            let abs_idx = self.scroll_rows + vi;
            let is_secondary = secondary_from.map(|s| abs_idx >= s).unwrap_or(false);
            let is_sec_boundary = secondary_from == Some(abs_idx) && vi > 0;
            let is_selected = selected_row == Some(abs_idx);
            let ry = body_top + vi as f32 * row_h;
            let rr = rect_f(bounds.left + 8.0, ry, bounds.right - bounds.left - 20.0, row_h - 1.0);
            let hovered = self.mouse_dip.map(|(mx, my)| hit_rect(&rr, mx, my)).unwrap_or(false);
            if is_selected {
                fill_round_rect(rt, rr, 6.0, theme.hover);
                fill_round_rect(rt, rect_f(rr.left, rr.top + 5.0, 2.5, rr.bottom - rr.top - 10.0), 1.25, theme.accent);
            } else if hovered {
                fill_round_rect(rt, rr, 6.0, theme.hover);
            }
            if let Some(acts) = row_actions {
                if let Some(act) = acts.get(abs_idx) {
                    self.hit_boxes.push(HitBox {
                        rect: rr,
                        action: act.clone(),
                    });
                }
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
                } else if col_title == "估算费用" || col_title == "输出 $/M" {
                    theme.output
                } else if ci == 0 || col_title == "总 Token" || col_title == "估算总费" {
                    theme.text
                } else {
                    theme.sub
                };
                draw_text(
                    rt,
                    cell,
                    if right_align { &fonts.mono_small_right } else { &fonts.mono_small },
                    rect_f(rx, ry, cw - 6.0, row_h),
                    color,
                );
                rx += cw;
            }
        }

        if self.max_scroll > 0 {
            let track_h = (bounds.bottom - 8.0 - body_top).max(20.0);
            let thumb_h = ((vis_rows as f32 / rows.len() as f32) * track_h).clamp(24.0, track_h);
            let ratio = self.scroll_rows as f32 / self.max_scroll as f32;
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
    use chrono::{TimeZone, Utc};
    use std::collections::BTreeSet;

    fn make_session(id: &str, ws: &str, model: &str, ts_ymd: (i32, u32, u32), calls: u64) -> SessionInfo {
        let ts = Utc.with_ymd_and_hms(ts_ymd.0, ts_ymd.1, ts_ymd.2, 10, 0, 0).single();
        let mut counts = TokenCounts::default();
        let mut turns = Vec::new();
        for i in 0..calls {
            counts.add_call(1000, 9000, 0, 500);
            turns.push(TurnRecord {
                step_index: i as i64,
                timestamp_utc: ts,
                model: model.to_string(),
                is_checkpoint: false,
                input_tokens: 1000,
                cache_read_tokens: 9000,
                cache_write_tokens: 0,
                output_tokens: 500,
                cost: CostBreakdown::default(),
            });
        }
        let mut models = BTreeSet::new();
        models.insert(model.to_string());
        SessionInfo {
            conversation_id: id.to_string(),
            title: id.to_string(),
            workspace: ws.to_string(),
            is_subagent: false,
            parent_id: None,
            parent_title: String::new(),
            turns,
            counts,
            models,
            first_time: ts.map(|u| u.with_timezone(&Local)),
            last_time: ts.map(|u| u.with_timezone(&Local)),
        }
    }

    #[test]
    fn test_session_filter_and_keyboard_step() {
        let s1 = make_session("s1", "C:/Users/a/agent-home", "gemini-4-neon-xhigh", (2026, 11, 21), 5);
        let s2 = make_session("s2", "C:/Users/a/subvault", "gemini-4-neon-high", (2026, 11, 20), 2);

        assert!(SessionFilter::Workspace("C:/Users/a/agent-home".into()).matches(&s1));
        assert!(!SessionFilter::Workspace("C:/Users/a/agent-home".into()).matches(&s2));
        assert!(SessionFilter::Model("gemini-4-neon-high".into()).matches(&s2));
        assert!(SessionFilter::Week("2026-W47".into()).matches(&s1));

        let mut view = DashboardView::new(false, DateScope::All, false, true);
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

        let mut view = DashboardView::new(false, DateScope::All, false, true);
        view.cached_sessions = vec![parent, child];

        // Grouped mode rolls child into parent (1 row, 5 calls total, subagent_count=1)
        let grouped = view.display_sessions(true);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].session.conversation_id, "p1");
        assert_eq!(grouped[0].subagent_count, 1);
        assert_eq!(grouped[0].counts.calls, 5);

        // Filtering by subagent-only model reveals the matching subagent directly
        view.session_filter = Some(SessionFilter::Model("gemini-4-hydrogen-tiered-xhigh".into()));
        let by_model = view.display_sessions(true);
        assert_eq!(by_model.len(), 1);
        assert_eq!(by_model[0].session.conversation_id, "c1");
        assert!(by_model[0].is_child_row);

        // Parent drill-down expands parent first, then its subagent
        view.session_filter = Some(SessionFilter::Parent("p1".into(), "p1".into()));
        let expanded = view.display_sessions(true);
        assert_eq!(expanded.len(), 2);
        assert_eq!(expanded[0].session.conversation_id, "p1");
        assert!(!expanded[0].is_child_row);
        assert_eq!(expanded[1].session.conversation_id, "c1");
        assert!(expanded[1].is_child_row);
    }

    #[test]
    fn test_date_scope_roundtrip() {
        for sc in [DateScope::Today, DateScope::Days7, DateScope::Days30, DateScope::All] {
            assert_eq!(DateScope::from_u8(sc.to_u8()), sc);
        }
        assert_eq!(DateScope::from_u8(99), DateScope::Today);
    }

    #[test]
    fn test_smart_hover_repaint_gating() {
        let mut view = DashboardView::new(false, DateScope::Today, false, true);
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
        let mut view = DashboardView::new(false, DateScope::All, false, true);
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
}
