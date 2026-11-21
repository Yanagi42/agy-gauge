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
    theme_for, with_alpha, BarItem, D2DFactories, Fonts, Theme, WindowSurface,
};
use crate::stats::{
    filter_snapshot, fmt_commas, fmt_compact, fmt_num, iso_week_key, iso_week_range_label,
    weekday_cn, workspace_short_name, AggregatedStats, DateScope, ScanSnapshot, SessionInfo,
    SessionSort, TokenCounts,
};
use crate::telemetry::{apply_win11_frame, copy_text_to_clipboard, SystemTelemetry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Daily,
    Weekly,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionFilter {
    Model(String),
    Workspace(String),
    Date(String),
    Week(String),
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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickAction {
    SelectTab(Tab),
    ToggleSystemView,
    SelectWeeklyMode(WeeklyMode),
    SelectBreakdown(BreakdownMode),
    SelectScope(DateScope),
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
    pub scope: DateScope,
    pub sort: SessionSort,
    pub session_filter: Option<SessionFilter>,
    pub fresh_only: bool,
    pub selected_session: Option<String>,
    pub mouse_dip: Option<(f32, f32)>,
    scroll_rows: usize,
    max_scroll: usize,
    sessions_vis_rows: usize,
    hit_boxes: Vec<HitBox>,
    copied_until: Option<Instant>,
    session_copied_until: Option<Instant>,
    cached_sessions: Vec<SessionInfo>,
    cached_agg: AggregatedStats,
    cached_date: NaiveDate,
    dirty_filter: bool,
}

fn cache_hit_color(theme: &Theme, c: &TokenCounts) -> D2D1_COLOR_F {
    if c.calls <= 1 && c.cache_read_tokens == 0 {
        theme.muted
    } else {
        let r = c.cache_hit_rate();
        if r < 65.0 {
            theme.warn
        } else if r >= 85.0 {
            theme.sage
        } else {
            theme.sub
        }
    }
}

impl DashboardView {
    pub fn new(fresh_only: bool, scope: DateScope) -> Self {
        Self {
            surface: None,
            active_tab: Tab::Overview,
            prev_token_tab: Tab::Overview,
            weekly_mode: WeeklyMode::Weekly,
            breakdown_mode: BreakdownMode::Model,
            scope,
            sort: SessionSort::Recent,
            session_filter: None,
            fresh_only,
            selected_session: None,
            mouse_dip: None,
            scroll_rows: 0,
            max_scroll: 0,
            sessions_vis_rows: 6,
            hit_boxes: Vec::new(),
            copied_until: None,
            session_copied_until: None,
            cached_sessions: Vec::new(),
            cached_agg: AggregatedStats::default(),
            cached_date: Local::now().date_naive(),
            dirty_filter: true,
        }
    }

    pub fn step_session(&mut self, delta: isize) -> bool {
        if self.active_tab != Tab::Sessions || self.cached_sessions.is_empty() {
            return false;
        }
        let sorted = self.sorted_sessions();
        if sorted.is_empty() {
            return false;
        }
        let cur_idx = self
            .selected_session
            .as_deref()
            .and_then(|id| sorted.iter().position(|s| s.conversation_id == id))
            .unwrap_or(0);
        let next_idx = (cur_idx as isize + delta).clamp(0, (sorted.len() - 1) as isize) as usize;
        if next_idx == cur_idx && self.selected_session.is_some() {
            return false;
        }
        self.selected_session = Some(sorted[next_idx].conversation_id.clone());
        let vis = self.sessions_vis_rows.max(1);
        if next_idx < self.scroll_rows {
            self.scroll_rows = next_idx;
        } else if next_idx >= self.scroll_rows + vis {
            self.scroll_rows = (next_idx + 1).saturating_sub(vis);
        }
        true
    }

    fn sorted_sessions(&self) -> Vec<SessionInfo> {
        let mut sorted: Vec<SessionInfo> = match &self.session_filter {
            Some(f) => self.cached_sessions.iter().filter(|s| f.matches(s)).cloned().collect(),
            None => self.cached_sessions.clone(),
        };
        match self.sort {
            SessionSort::Recent => sorted.sort_by(|a, b| b.last_time.cmp(&a.last_time)),
            SessionSort::NonCached => {
                sorted.sort_by(|a, b| b.counts.non_cached_tokens().cmp(&a.counts.non_cached_tokens()))
            }
            SessionSort::TotalTokens => {
                sorted.sort_by(|a, b| b.counts.total_tokens().cmp(&a.counts.total_tokens()))
            }
            SessionSort::HitRateLow => sorted.sort_by(|a, b| {
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
            }),
            SessionSort::PeakContext => {
                sorted.sort_by(|a, b| b.counts.peak_context.cmp(&a.counts.peak_context))
            }
        }
        sorted
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
            ClickAction::SelectScope(s) => {
                self.scope = s;
                self.scroll_rows = 0;
                self.dirty_filter = true;
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
                let t = &self.cached_agg.total;
                let scope_lbl = match self.scope {
                    DateScope::Today => "今日",
                    DateScope::Days7 => "近7天",
                    DateScope::Days30 => "近30天",
                    DateScope::All => "全部",
                };
                let text = format!(
                    "AGY CLI Token 用量 ({})\n- 会话: {} | 调用: {}\n- 新增输入: {} | 缓存读取: {} (命中 {:.1}%)\n- 输出: {} | 总 Token: {} (非缓存增量: {})\n- 峰值Ctx: {}",
                    scope_lbl,
                    self.cached_sessions.len(),
                    t.calls,
                    fmt_commas(t.input_tokens),
                    fmt_commas(t.cache_read_tokens),
                    t.cache_hit_rate(),
                    fmt_commas(t.output_tokens),
                    fmt_commas(t.total_tokens()),
                    fmt_commas(t.non_cached_tokens()),
                    fmt_commas(t.peak_context),
                );
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
                    let range = match (s.first_time, s.last_time) {
                        (Some(a), Some(b)) => format!("{} -> {}", a.format("%m/%d %H:%M"), b.format("%H:%M")),
                        _ => "-".to_string(),
                    };
                    let text = format!(
                        "AGY 会话明细 · {}\n- ID: {} | 时间: {} | 模型: {}{}\n- 轮数: {}{} | 缓存命中: {:.1}%\n- 新增输入: {} | 缓存读取: {} | 输出: {}\n- 总 Token: {} (非缓存增量: {}) | 峰值Ctx: {}",
                        s.title,
                        &s.conversation_id[..s.conversation_id.len().min(8)],
                        range,
                        s.primary_model(),
                        if ws.is_empty() { String::new() } else { format!(" | 项目: {}", ws) },
                        s.counts.calls,
                        if cp > 0 { format!(" ({}次压缩)", cp) } else { String::new() },
                        s.counts.cache_hit_rate(),
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
                let matches = self.sorted_sessions();
                let keep = self
                    .selected_session
                    .as_deref()
                    .map(|id| matches.iter().any(|s| s.conversation_id == id))
                    .unwrap_or(false);
                if !keep {
                    self.selected_session = matches.first().map(|s| s.conversation_id.clone());
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

        let visible = self.sorted_sessions();
        let keep = self
            .selected_session
            .as_ref()
            .map(|id| visible.iter().any(|s| &s.conversation_id == id))
            .unwrap_or(false);
        if !keep {
            self.selected_session = visible.first().map(|s| s.conversation_id.clone());
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
                Tab::Overview => self.draw_overview_tab(rt, fonts, theme, body, snapshot, human),
                Tab::Daily => self.draw_daily_tab(rt, fonts, theme, body, human),
                Tab::Weekly => self.draw_weekly_tab(rt, fonts, theme, body, human),
                Tab::Sessions => self.draw_sessions_tab(rt, fonts, theme, body, human),
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
        fill_circle(rt, 26.0, h * 0.5, 3.2, theme.accent);
        draw_text(rt, "AGY", &fonts.ui_brand, rect_f(35.0, 0.0, 42.0, h), theme.text);
        draw_text(rt, "/ 用量", &fonts.ui_small, rect_f(72.0, 1.0, 46.0, h), theme.muted);

        // Flattened 4 direct token tabs (System monitor lives on Hero CPU/RAM rings)
        let tabs = [
            (Tab::Overview, "概览", 48.0),
            (Tab::Daily, "按天", 48.0),
            (Tab::Weekly, "按周", 48.0),
            (Tab::Sessions, "会话", 48.0),
        ];
        let mut tx = 136.0;
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
                fill_round_rect(rt, rect_f(tx + 8.0, h - 2.5, tw - 16.0, 2.5), 1.25, theme.accent);
            }
            self.hit_boxes.push(HitBox { rect: r, action: ClickAction::SelectTab(tab) });
            tx += tw + 10.0;
        }

        // Right minimalist utility actions
        let mut rx = w - 20.0;
        let btn_y = 10.0;
        let btn_h = 26.0;

        let r_ref = rect_f(rx - 44.0, btn_y, 44.0, btn_h);
        self.draw_ghost_btn(rt, fonts, theme, r_ref, "刷新", false, ClickAction::Refresh);
        rx -= 50.0;

        let copied = self.copied_until.map(|t| Instant::now() < t).unwrap_or(false);
        let r_copy = rect_f(rx - 52.0, btn_y, 52.0, btn_h);
        self.draw_ghost_btn(
            rt,
            fonts,
            theme,
            r_copy,
            if copied { "已复制" } else { "复制" },
            copied,
            ClickAction::CopySummary,
        );
        rx -= 58.0;

        let r_km = rect_f(rx - 44.0, btn_y, 44.0, btn_h);
        self.draw_ghost_btn(rt, fonts, theme, r_km, if human { "K/M" } else { "精确" }, false, ClickAction::ToggleHuman);
        rx -= 50.0;

        let r_th = rect_f(rx - 46.0, btn_y, 46.0, btn_h);
        self.draw_ghost_btn(
            rt,
            fonts,
            theme,
            r_th,
            if light_theme { "暗色" } else { "浅色" },
            false,
            ClickAction::ToggleLightTheme,
        );
        rx -= 56.0;

        draw_line(rt, rx, 14.0, rx, h - 14.0, theme.border, 1.0);
        rx -= 10.0;

        // Scope filter (borderless editorial text toggles; persisted in Registry)
        let scopes = [
            (DateScope::All, "全部", 38.0),
            (DateScope::Days30, "30天", 40.0),
            (DateScope::Days7, "7天", 34.0),
            (DateScope::Today, "今日", 38.0),
        ];
        for (sc, lbl, sw) in scopes {
            let r = rect_f(rx - sw, btn_y, sw, btn_h);
            if r.left > tx + 8.0 {
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

        let t = &self.cached_agg.total;

        // Block 1: Hero Total + subtle ratio ribbon
        draw_text(rt, "总 Token 消耗", &fonts.ui_micro, rect_f(x0 + 18.0, y + 9.0, c1 - 28.0, 14.0), theme.muted);
        draw_text(rt, &fmt_num(t.total_tokens(), human), &fonts.mono_hero, rect_f(x0 + 18.0, y + 23.0, c1 - 28.0, 30.0), theme.text);
        let sub1 = format!("非缓存增量 {}  ·  缓存命中 {:.1}%", fmt_compact(t.non_cached_tokens()), t.cache_hit_rate());
        draw_text(rt, &sub1, &fonts.ui_micro, rect_f(x0 + 18.0, y + 53.0, c1 - 28.0, 14.0), theme.sub);

        let bar_l = x0 + 18.0;
        let bar_w = c1 - 36.0;
        let bar_y = y + h - 6.0;
        fill_round_rect(rt, rect_f(bar_l, bar_y, bar_w, 2.5), 1.25, theme.grid);
        if t.total_tokens() > 0 {
            let tot = t.total_tokens() as f32;
            let mut bx = bar_l;
            for (v, col) in [(t.cache_read_tokens, theme.cache), (t.input_tokens, theme.accent), (t.output_tokens, theme.output)] {
                if v == 0 { continue; }
                let sw = ((v as f32 / tot) * bar_w).max(2.0);
                if let Some(b) = brush(rt, col) {
                    unsafe { rt.FillRectangle(&rect_f(bx, bar_y, sw.min(bar_l + bar_w - bx), 2.5), &b) };
                }
                bx += sw;
            }
        }

        // Block 2: Fresh Input & Output
        draw_text(rt, "新增输入 / 输出", &fonts.ui_micro, rect_f(x1 + 18.0, y + 9.0, c2 - 28.0, 14.0), theme.muted);
        let io_str = format!("{}  /  {}", fmt_compact(t.input_tokens), fmt_compact(t.output_tokens));
        draw_text(rt, &io_str, &fonts.mono_kpi, rect_f(x1 + 18.0, y + 26.0, c2 - 28.0, 24.0), theme.text);
        let sub2 = format!("缓存读 {}  ·  峰值Ctx {}", fmt_compact(t.cache_read_tokens), fmt_compact(t.peak_context));
        draw_text(rt, &sub2, &fonts.ui_micro, rect_f(x1 + 18.0, y + 53.0, c2 - 28.0, 14.0), theme.sub);

        // Block 3: Sessions & Calls
        draw_text(rt, "活跃规模", &fonts.ui_micro, rect_f(x2 + 16.0, y + 9.0, c3 - 22.0, 14.0), theme.muted);
        let act_str = if c3 >= 195.0 {
            format!("{} 会话 · {} 次", self.cached_sessions.len(), t.calls)
        } else {
            format!("{}会话·{}次", self.cached_sessions.len(), t.calls)
        };
        draw_text(rt, &act_str, &fonts.mono_kpi, rect_f(x2 + 16.0, y + 26.0, c3 - 22.0, 24.0), theme.text);
        let avg = if t.calls > 0 { t.total_tokens() / t.calls } else { 0 };
        let sub3 = format!("均单次 {}  ·  {} 天", fmt_compact(avg), self.cached_agg.by_day.len());
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
                        BarItem {
                            label: format!("{:02}h", h),
                            cache: c.cache_read_tokens,
                            input: c.input_tokens,
                            output: c.output_tokens,
                            marker: false,
                            tooltip: vec![
                                format!("今日 {:02}:00 - {:02}:59 · {} 次", h, h, c.calls),
                                format!("总计   : {}", fmt_commas(c.total_tokens())),
                                format!("新输入 : {}", fmt_commas(c.input_tokens)),
                                format!("缓存读 : {} ({:.1}%)", fmt_commas(c.cache_read_tokens), c.cache_hit_rate()),
                                format!("输出   : {}", fmt_commas(c.output_tokens)),
                            ],
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };
            ("今日分时趋势", items, Vec::new(), "今日暂无用量 · 右上角可切换 7天 / 全部")
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
                    BarItem {
                        label: d.format("%m/%d").to_string(),
                        cache: c.cache_read_tokens,
                        input: c.input_tokens,
                        output: c.output_tokens,
                        marker: false,
                        tooltip: vec![
                            format!("{} ({}) · {} 次", k, weekday_cn(d), c.calls),
                            format!("总计   : {}", fmt_commas(c.total_tokens())),
                            format!("新输入 : {}", fmt_commas(c.input_tokens)),
                            format!("缓存读 : {} ({:.1}%)", fmt_commas(c.cache_read_tokens), c.cache_hit_rate()),
                            format!("输出   : {}", fmt_commas(c.output_tokens)),
                        ],
                    }
                })
                .collect();
            let title = if self.scope == DateScope::Days7 { "近 7 天用量趋势" } else { "每日用量趋势" };
            (title, items, days_keys, "暂无用量数据")
        };

        draw_text(rt, chart_title, &fonts.ui_section, rect_f(tl.left + 16.0, tl.top + 11.0, 130.0, 20.0), theme.text);
        self.draw_chart_legend_and_toggle(rt, fonts, theme, tl);
        let tl_plot = rect_f(tl.left + 10.0, tl.top + 38.0, left_w - 20.0, top_h - 46.0);
        let tl_slots = draw_editorial_bars(rt, fonts, theme, tl_plot, &bars, self.fresh_only, self.mouse_dip, empty_msg);
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

        let split_y = tr.top + (top_h * 0.44).clamp(96.0, 114.0);
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
        let wk_avg = if wk_days > 0 { wk_c.total_tokens() / wk_days as u64 } else { 0 };

        draw_text(
            rt,
            &format!("本周 · {}  ›", cur_wk),
            &fonts.ui_section,
            rect_f(tr.left + 16.0, tr.top + 10.0, 160.0, 20.0),
            if wk_hov { theme.accent } else { theme.text },
        );
        draw_text(rt, &iso_week_range_label(&cur_wk), &fonts.mono_small_right, rect_f(tr.right - 120.0, tr.top + 12.0, 104.0, 16.0), theme.muted);

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
            &format!("本周缓存命中 {:.1}%  ·  上周总量 {}", wk_c.cache_hit_rate(), prev_lbl),
            &fonts.ui_micro,
            rect_f(tr.left + 16.0, tr.top + 76.0, right_w - 32.0, 15.0),
            theme.muted,
        );

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
        draw_text(
            rt,
            if self.breakdown_mode == BreakdownMode::Model { "占比 · 缓存命中" } else { "占比 · 会话数" },
            &fonts.ui_small_right,
            rect_f(tr.right - 136.0, split_y + 9.0, 120.0, 16.0),
            theme.muted,
        );
        let bd_bounds = rect_f(tr.left + 16.0, split_y + 31.0, right_w - 32.0, (tr.bottom - split_y - 37.0).max(24.0));
        match self.breakdown_mode {
            BreakdownMode::Model => self.draw_compact_models(rt, fonts, theme, bd_bounds, human),
            BreakdownMode::Workspace => self.draw_compact_workspaces(rt, fonts, theme, bd_bounds, human),
        }

        // 3) Bottom Full-Width: Recent Active Sessions (spacious table with per-session cache hit rate)
        let bot = rect_f(body.left, tl.bottom + gap, bw, bot_h);
        fill_round_rect(rt, bot, 10.0, theme.surface);
        stroke_round_rect(rt, bot, 10.0, theme.border, 1.0);

        draw_text(rt, "最近活跃会话", &fonts.ui_section, rect_f(bot.left + 16.0, bot.top + 9.0, 96.0, 20.0), theme.text);
        draw_text(
            rt,
            &format!("共 {} 个", self.cached_sessions.len()),
            &fonts.ui_micro,
            rect_f(bot.left + 108.0, bot.top + 11.0, 80.0, 16.0),
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
        let model_w = if rr_w >= 860.0 { 136.0 } else { 118.0 };
        let model_x = rx - 338.0 - model_w;
        let title_x = rr_l + 90.0;
        let title_w = (model_x - 10.0 - title_x).max(80.0);

        // Subtle column header row
        let hdr_y = bot.top + 31.0;
        let hdr_h = 20.0;
        draw_text(rt, "时间", &fonts.ui_micro, rect_f(rr_l + 8.0, hdr_y, 76.0, hdr_h), theme.muted);
        draw_text(rt, "会话", &fonts.ui_micro, rect_f(title_x, hdr_y, title_w, hdr_h), theme.muted);
        draw_text(rt, "模型", &fonts.ui_small_right, rect_f(model_x, hdr_y, model_w, hdr_h), theme.muted);
        draw_text(rt, "轮数", &fonts.ui_small_right, rect_f(rx - 332.0, hdr_y, 42.0, hdr_h), theme.muted);
        draw_text(rt, "命中率", &fonts.ui_small_right, rect_f(rx - 284.0, hdr_y, 60.0, hdr_h), theme.muted);
        draw_text(rt, "非缓存", &fonts.ui_small_right, rect_f(rx - 218.0, hdr_y, 66.0, hdr_h), theme.muted);
        draw_text(rt, "总计", &fonts.ui_small_right, rect_f(rx - 146.0, hdr_y, 72.0, hdr_h), theme.muted);
        draw_text(rt, "峰值Ctx", &fonts.ui_small_right, rect_f(rx - 68.0, hdr_y, 68.0, hdr_h), theme.muted);
        draw_line(rt, bot.left + 14.0, bot.top + 51.0, bot.right - 14.0, bot.top + 51.0, theme.border, 1.0);

        let list_y = bot.top + 54.0;
        let avail_h = (bot.bottom - 8.0 - list_y).max(28.0);
        if self.cached_sessions.is_empty() {
            draw_text(
                rt,
                "当前范围暂无会话 · 点击右上角可切换 7天 / 30天 / 全部",
                &fonts.ui_reg_center,
                rect_f(bot.left, list_y, bw, avail_h),
                theme.muted,
            );
        } else {
            let max_vis = ((avail_h / 28.0).floor() as usize).clamp(1, 6);
            let show_n = self.cached_sessions.len().min(max_vis);
            let row_h = (avail_h / max_vis as f32).clamp(26.0, 32.0);
            for (i, s) in self.cached_sessions.iter().take(show_n).enumerate() {
                let ry = list_y + i as f32 * row_h;
                let rr = rect_f(rr_l, ry, rr_w, row_h - 2.0);
                let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&rr, mx, my)).unwrap_or(false);
                if hov {
                    fill_round_rect(rt, rr, 6.0, theme.hover);
                }
                if i > 0 {
                    draw_line(rt, bot.left + 16.0, ry - 1.0, bot.right - 16.0, ry - 1.0, theme.grid, 0.8);
                }
                let t_str = s.last_time.map(|t| t.format("%m/%d %H:%M").to_string()).unwrap_or_else(|| "-".into());
                let m_str = s.primary_model();
                let hit_str = format!("{:.1}%", s.counts.cache_hit_rate());
                let hit_col = cache_hit_color(theme, &s.counts);

                draw_text(rt, &t_str, &fonts.mono_small, rect_f(rr_l + 8.0, ry, 76.0, row_h - 2.0), theme.muted);
                draw_text(rt, &s.title, &fonts.ui_reg, rect_f(title_x, ry, title_w, row_h - 2.0), theme.text);
                draw_text(rt, &m_str, &fonts.mono_small_right, rect_f(model_x, ry, model_w, row_h - 2.0), theme.muted);
                draw_text(rt, &format!("{}轮", s.counts.calls), &fonts.mono_small_right, rect_f(rx - 332.0, ry, 42.0, row_h - 2.0), theme.muted);
                draw_text(rt, &hit_str, &fonts.mono_small_right, rect_f(rx - 284.0, ry, 60.0, row_h - 2.0), hit_col);
                draw_text(rt, &fmt_num(s.counts.non_cached_tokens(), human), &fonts.mono_small_right, rect_f(rx - 218.0, ry, 66.0, row_h - 2.0), theme.sub);
                draw_text(rt, &fmt_num(s.counts.total_tokens(), human), &fonts.mono_med_right, rect_f(rx - 146.0, ry, 72.0, row_h - 2.0), theme.text);
                draw_text(rt, &fmt_compact(s.counts.peak_context), &fonts.mono_small_right, rect_f(rx - 68.0, ry, 68.0, row_h - 2.0), theme.sub);

                self.hit_boxes.push(HitBox {
                    rect: rr,
                    action: ClickAction::InspectSession(s.conversation_id.clone()),
                });
            }
        }
    }

    fn draw_compact_models(&mut self, rt: &ID2D1RenderTarget, fonts: &Fonts, theme: &Theme, bounds: D2D_RECT_F, human: bool) {
        let mut items: Vec<(String, TokenCounts)> = self
            .cached_agg
            .by_model
            .iter()
            .map(|(k, c)| (k.clone(), *c))
            .collect();
        items.sort_by(|a, b| b.1.total_tokens().cmp(&a.1.total_tokens()));
        if items.is_empty() {
            draw_text(rt, "当前范围暂无模型数据", &fonts.ui_small, bounds, theme.muted);
            return;
        }
        let avail_h = (bounds.bottom - bounds.top).max(24.0);
        let max_show = if avail_h >= 82.0 { 3 } else { 2 };
        let show_n = items.len().min(max_show);
        let total_all = self.cached_agg.total.total_tokens().max(1);
        let row_h = (avail_h / show_n as f32).clamp(24.0, 34.0);
        let w = bounds.right - bounds.left;
        let palette = [theme.accent, theme.output, theme.ram];

        for (i, (name, c)) in items.into_iter().take(show_n).enumerate() {
            let y = bounds.top + i as f32 * row_h;
            let row_r = rect_f(bounds.left - 4.0, y - 2.0, w + 8.0, row_h.min(27.0));
            let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&row_r, mx, my)).unwrap_or(false);
            if hov {
                fill_round_rect(rt, row_r, 5.0, with_alpha(theme.text, 0.04));
            }
            let share = (c.total_tokens() as f64 / total_all as f64) as f32;
            draw_text(rt, &name, &fonts.mono_small, rect_f(bounds.left, y, w * 0.48, 16.0), if hov { theme.accent } else { theme.text });
            let stat = format!(
                "{} ({:.0}%) · {:.0}%",
                fmt_num(c.total_tokens(), human),
                share * 100.0,
                c.cache_hit_rate()
            );
            draw_text(rt, &stat, &fonts.mono_small_right, rect_f(bounds.left + w * 0.46, y, w * 0.54, 16.0), theme.sub);
            fill_round_rect(rt, rect_f(bounds.left, y + 18.0, w, 3.5), 1.75, theme.grid);
            fill_round_rect(rt, rect_f(bounds.left, y + 18.0, (w * share).max(3.0), 3.5), 1.75, palette[i % palette.len()]);
            self.hit_boxes.push(HitBox {
                rect: row_r,
                action: ClickAction::FilterSessions(SessionFilter::Model(name)),
            });
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
        items.sort_by(|a, b| b.3.total_tokens().cmp(&a.3.total_tokens()));
        if items.is_empty() {
            draw_text(rt, "当前范围暂无项目数据", &fonts.ui_small, bounds, theme.muted);
            return;
        }
        let avail_h = (bounds.bottom - bounds.top).max(24.0);
        let max_show = if avail_h >= 82.0 { 3 } else { 2 };
        let show_n = items.len().min(max_show);
        let total_all = self.cached_agg.total.total_tokens().max(1);
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
            let share = (c.total_tokens() as f64 / total_all as f64) as f32;
            draw_text(rt, &name, &fonts.mono_small, rect_f(bounds.left, y, w * 0.48, 16.0), if hov { theme.accent } else { theme.text });
            let stat = format!(
                "{} ({:.0}%) · {}会话",
                fmt_num(c.total_tokens(), human),
                share * 100.0,
                sess_cnt
            );
            draw_text(rt, &stat, &fonts.mono_small_right, rect_f(bounds.left + w * 0.46, y, w * 0.54, 16.0), theme.sub);
            fill_round_rect(rt, rect_f(bounds.left, y + 18.0, w, 3.5), 1.75, theme.grid);
            fill_round_rect(rt, rect_f(bounds.left, y + 18.0, (w * share).max(3.0), 3.5), 1.75, palette[i % palette.len()]);
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
        draw_text(rt, "总计", &fonts.ui_small_center, r_tot, if !self.fresh_only { theme.text } else { theme.muted });
        draw_text(rt, "仅增量", &fonts.ui_small_center, r_inc, if self.fresh_only { theme.accent } else { theme.muted });
        self.hit_boxes.push(HitBox { rect: tr, action: ClickAction::ToggleFreshOnly });

        let mut lx = tr.left - 168.0;
        let ly = card.top + 20.0;
        if lx > card.left + 135.0 {
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

        draw_text(rt, "按天用量明细", &fonts.ui_section, rect_f(chart_c.left + 16.0, chart_c.top + 10.0, 96.0, 20.0), theme.text);
        if self.scope == DateScope::Today {
            let hint_r = rect_f(chart_c.left + 114.0, chart_c.top + 9.0, 108.0, 22.0);
            self.draw_ghost_btn(rt, fonts, theme, hint_r, "仅今日 · 展开全部", true, ClickAction::SelectScope(DateScope::All));
        }
        self.draw_chart_legend_and_toggle(rt, fonts, theme, chart_c);

        let plot = rect_f(chart_c.left + 12.0, chart_c.top + 40.0, bw - 24.0, chart_h - 48.0);
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
            .map(|(day, c)| BarItem {
                label: if day.len() >= 10 { day[5..].to_string() } else { day.clone() },
                cache: c.cache_read_tokens,
                input: c.input_tokens,
                output: c.output_tokens,
                marker: false,
                tooltip: vec![
                    format!("{} · {} 次调用", day, c.calls),
                    format!("总计   : {}", fmt_commas(c.total_tokens())),
                    format!("新输入 : {}", fmt_commas(c.input_tokens)),
                    format!("缓存读 : {} ({:.1}%)", fmt_commas(c.cache_read_tokens), c.cache_hit_rate()),
                    format!("输出   : {}", fmt_commas(c.output_tokens)),
                    format!("峰值Ctx: {}", fmt_commas(c.peak_context)),
                ],
            })
            .collect();
        let day_slots = draw_editorial_bars(rt, fonts, theme, plot, &bars, self.fresh_only, self.mouse_dip, "暂无按天数据");
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
                vec![
                    (*day).clone(),
                    wd.into(),
                    c.calls.to_string(),
                    fmt_num(c.input_tokens, human),
                    fmt_num(c.cache_read_tokens, human),
                    fmt_num(c.output_tokens, human),
                    fmt_num(c.total_tokens(), human),
                    fmt_num(c.non_cached_tokens(), human),
                    format!("{:.1}%", c.cache_hit_rate()),
                    fmt_num(c.peak_context, human),
                ]
            })
            .collect();
        let row_actions: Vec<ClickAction> = days_asc
            .iter()
            .rev()
            .map(|(day, _)| ClickAction::FilterSessions(SessionFilter::Date((*day).clone())))
            .collect();
        self.draw_table(
            rt,
            fonts,
            theme,
            tbl_r,
            &[
                ("日期", 0.12, false),
                ("星期", 0.06, false),
                ("调用", 0.06, true),
                ("新增输入", 0.11, true),
                ("缓存读取", 0.12, true),
                ("输出", 0.10, true),
                ("总 Token", 0.12, true),
                ("非缓存", 0.11, true),
                ("命中率", 0.09, true),
                ("峰值Ctx", 0.11, true),
            ],
            &rows,
            Some(&row_actions),
        );
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
        let tbl_r = rect_f(body.left, chart_c.bottom + 12.0, bw, (body.bottom - chart_c.bottom - 12.0).max(110.0));

        match self.weekly_mode {
            WeeklyMode::Weekly => {
                let weeks_asc: Vec<(&String, &TokenCounts)> = self.cached_agg.by_week.iter().collect();
                let mut wow: HashMap<String, String> = HashMap::new();
                let mut prev: Option<u64> = None;
                for (wk, c) in &weeks_asc {
                    if let Some(p) = prev.filter(|&p| p > 0) {
                        let d = ((c.total_tokens() as f64 - p as f64) / p as f64) * 100.0;
                        wow.insert((*wk).clone(), format!("{:+.1}%", d));
                    } else {
                        wow.insert((*wk).clone(), "-".into());
                    }
                    prev = Some(c.total_tokens());
                }
                let start = weeks_asc.len().saturating_sub(16);
                let bars: Vec<BarItem> = weeks_asc[start..]
                    .iter()
                    .map(|(wk, c)| {
                        let ad = self.cached_agg.week_active_days.get(*wk).copied().unwrap_or(1).max(1) as u64;
                        BarItem {
                            label: wk.split('-').last().unwrap_or(wk).to_string(),
                            cache: c.cache_read_tokens,
                            input: c.input_tokens,
                            output: c.output_tokens,
                            marker: false,
                            tooltip: vec![
                                format!("{} ({})", wk, iso_week_range_label(wk)),
                                format!("总计   : {} (环比 {})", fmt_commas(c.total_tokens()), wow.get(*wk).map(String::as_str).unwrap_or("-")),
                                format!("日均   : {} ({} 活跃天)", fmt_commas(c.total_tokens() / ad), ad),
                                format!("新输入 : {}", fmt_commas(c.input_tokens)),
                                format!("缓存读 : {} ({:.1}%)", fmt_commas(c.cache_read_tokens), c.cache_hit_rate()),
                                format!("输出   : {}", fmt_commas(c.output_tokens)),
                            ],
                        }
                    })
                    .collect();
                let wk_slots = draw_editorial_bars(rt, fonts, theme, plot, &bars, self.fresh_only, self.mouse_dip, "暂无按周数据");
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
                            wow.get(*wk).cloned().unwrap_or_else(|| "-".into()),
                            format!("{:.1}%", c.cache_hit_rate()),
                        ]
                    })
                    .collect();
                let wk_actions: Vec<ClickAction> = weeks_asc
                    .iter()
                    .rev()
                    .map(|(wk, _)| ClickAction::FilterSessions(SessionFilter::Week((*wk).clone())))
                    .collect();
                self.draw_table(
                    rt,
                    fonts,
                    theme,
                    tbl_r,
                    &[
                        ("ISO 周", 0.095, false),
                        ("周一至周日", 0.115, false),
                        ("活跃", 0.055, true),
                        ("调用", 0.055, true),
                        ("日均Token", 0.10, true),
                        ("新增输入", 0.095, true),
                        ("缓存读取", 0.105, true),
                        ("输出", 0.09, true),
                        ("总 Token", 0.105, true),
                        ("周环比", 0.095, true),
                        ("命中率", 0.09, true),
                    ],
                    &rows,
                    Some(&wk_actions),
                );
            }
            WeeklyMode::Monthly => {
                let m_asc: Vec<(&String, &TokenCounts)> = self.cached_agg.by_month.iter().collect();
                let bars: Vec<BarItem> = m_asc
                    .iter()
                    .map(|(m, c)| BarItem {
                        label: (*m).clone(),
                        cache: c.cache_read_tokens,
                        input: c.input_tokens,
                        output: c.output_tokens,
                        marker: false,
                        tooltip: vec![
                            format!("{} · {} 次调用", m, c.calls),
                            format!("总计   : {}", fmt_commas(c.total_tokens())),
                            format!("新输入 : {}", fmt_commas(c.input_tokens)),
                            format!("缓存读 : {} ({:.1}%)", fmt_commas(c.cache_read_tokens), c.cache_hit_rate()),
                            format!("输出   : {}", fmt_commas(c.output_tokens)),
                        ],
                    })
                    .collect();
                let _ = draw_editorial_bars(rt, fonts, theme, plot, &bars, self.fresh_only, self.mouse_dip, "暂无按月数据");

                let rows: Vec<Vec<String>> = m_asc
                    .iter()
                    .rev()
                    .map(|(m, c)| {
                        vec![
                            (*m).clone(),
                            c.calls.to_string(),
                            fmt_num(c.input_tokens, human),
                            fmt_num(c.cache_read_tokens, human),
                            fmt_num(c.output_tokens, human),
                            fmt_num(c.total_tokens(), human),
                            fmt_num(c.non_cached_tokens(), human),
                            format!("{:.1}%", c.cache_hit_rate()),
                            fmt_num(c.peak_context, human),
                        ]
                    })
                    .collect();
                self.draw_table(
                    rt,
                    fonts,
                    theme,
                    tbl_r,
                    &[
                        ("月份", 0.11, false),
                        ("调用", 0.08, true),
                        ("新增输入", 0.12, true),
                        ("缓存读取", 0.13, true),
                        ("输出", 0.11, true),
                        ("总 Token", 0.13, true),
                        ("非缓存", 0.12, true),
                        ("命中率", 0.09, true),
                        ("峰值Ctx", 0.11, true),
                    ],
                    &rows,
                    None,
                );
            }
        }
    }

    // ---------- Tab 4: Session Explorer + Per-Session Cache Hit Rate & Turn Anatomy ----------

    fn draw_sessions_tab(&mut self, rt: &ID2D1RenderTarget, fonts: &Fonts, theme: &Theme, body: D2D_RECT_F, human: bool) {
        let bw = body.right - body.left;
        let bh = body.bottom - body.top;
        let top_h = (bh * 0.48).clamp(160.0, 265.0);
        let top_c = rect_f(body.left, body.top, bw, top_h);
        fill_round_rect(rt, top_c, 10.0, theme.surface);
        stroke_round_rect(rt, top_c, 10.0, theme.border, 1.0);

        let sorted = self.sorted_sessions();

        draw_text(rt, "会话记录", &fonts.ui_section, rect_f(top_c.left + 16.0, top_c.top + 9.0, 68.0, 20.0), theme.text);
        let cnt_str = if self.session_filter.is_some() {
            format!("{} / {} 个", sorted.len(), self.cached_sessions.len())
        } else {
            format!("{} 个", sorted.len())
        };
        draw_text(
            rt,
            &cnt_str,
            &fonts.ui_micro,
            rect_f(top_c.left + 84.0, top_c.top + 11.0, 64.0, 16.0),
            theme.muted,
        );

        let sorts = [
            (SessionSort::Recent, "最近", 40.0),
            (SessionSort::NonCached, "非缓存", 48.0),
            (SessionSort::TotalTokens, "总Token", 56.0),
            (SessionSort::HitRateLow, "命中低→高", 68.0),
            (SessionSort::PeakContext, "峰值Ctx", 54.0),
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

        // Dismissible drill-down filter chip (only visible when filtered!)
        if let Some(f) = self.session_filter.clone() {
            let chip_lbl = f.label();
            let chip_x = top_c.left + 150.0;
            let max_w = (sx - 10.0 - chip_x).max(0.0);
            if max_w >= 72.0 {
                let est_w = (chip_lbl.chars().count() as f32 * 8.2 + 22.0).clamp(84.0, max_w);
                let chip_r = rect_f(chip_x, top_c.top + 8.0, est_w, 22.0);
                self.draw_ghost_btn(rt, fonts, theme, chip_r, &chip_lbl, true, ClickAction::ClearSessionFilter);
            }
        }

        let rr_l = top_c.left + 10.0;
        let rr_w = bw - 24.0;
        let rx = rr_l + rr_w - 8.0;
        let model_w = if rr_w >= 880.0 { 134.0 } else { 122.0 };
        let model_x = rx - 338.0 - model_w;
        let title_x = rr_l + 146.0;
        let title_w = (model_x - 10.0 - title_x).max(80.0);

        // Column headers
        let hdr_y = top_c.top + 32.0;
        let hdr_h = 19.0;
        draw_text(rt, "ID", &fonts.ui_micro, rect_f(rr_l + 8.0, hdr_y, 52.0, hdr_h), theme.muted);
        draw_text(rt, "时间", &fonts.ui_micro, rect_f(rr_l + 64.0, hdr_y, 76.0, hdr_h), theme.muted);
        draw_text(rt, "会话", &fonts.ui_micro, rect_f(title_x, hdr_y, title_w, hdr_h), theme.muted);
        draw_text(rt, "模型", &fonts.ui_small_right, rect_f(model_x, hdr_y, model_w, hdr_h), theme.muted);
        draw_text(rt, "轮数", &fonts.ui_small_right, rect_f(rx - 332.0, hdr_y, 42.0, hdr_h), theme.muted);
        draw_text(rt, "命中率", &fonts.ui_small_right, rect_f(rx - 284.0, hdr_y, 60.0, hdr_h), theme.muted);
        draw_text(rt, "非缓存", &fonts.ui_small_right, rect_f(rx - 218.0, hdr_y, 66.0, hdr_h), theme.muted);
        draw_text(rt, "总计", &fonts.ui_small_right, rect_f(rx - 146.0, hdr_y, 72.0, hdr_h), theme.muted);
        draw_text(rt, "峰值Ctx", &fonts.ui_small_right, rect_f(rx - 68.0, hdr_y, 68.0, hdr_h), theme.muted);
        draw_line(rt, top_c.left + 14.0, top_c.top + 52.0, top_c.right - 14.0, top_c.top + 52.0, theme.border, 1.0);

        let list_y = top_c.top + 55.0;
        let row_h = 27.0;
        let vis = (((top_c.bottom - 8.0 - list_y) / row_h).floor() as usize).max(1);
        self.sessions_vis_rows = vis;
        self.max_scroll = sorted.len().saturating_sub(vis);
        if self.scroll_rows > self.max_scroll {
            self.scroll_rows = self.max_scroll;
        }

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
            for (vi, s) in sorted.iter().skip(self.scroll_rows).take(vis).enumerate() {
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
                let t_str = s.last_time.map(|t| t.format("%m/%d %H:%M").to_string()).unwrap_or_else(|| "-".into());
                let m_str = s.primary_model();
                let hit_str = format!("{:.1}%", s.counts.cache_hit_rate());
                let hit_col = cache_hit_color(theme, &s.counts);

                draw_text(rt, sid, &fonts.mono_small, rect_f(rr_l + 8.0, ry, 52.0, row_h - 2.0), theme.muted);
                draw_text(rt, &t_str, &fonts.mono_small, rect_f(rr_l + 64.0, ry, 76.0, row_h - 2.0), theme.sub);
                draw_text(rt, &s.title, &fonts.ui_reg, rect_f(title_x, ry, title_w, row_h - 2.0), theme.text);

                draw_text(rt, &m_str, &fonts.mono_small_right, rect_f(model_x, ry, model_w, row_h - 2.0), theme.muted);
                draw_text(rt, &format!("{}轮", s.counts.calls), &fonts.mono_small_right, rect_f(rx - 332.0, ry, 42.0, row_h - 2.0), theme.sub);
                draw_text(rt, &hit_str, &fonts.mono_small_right, rect_f(rx - 284.0, ry, 60.0, row_h - 2.0), hit_col);
                draw_text(rt, &fmt_num(s.counts.non_cached_tokens(), human), &fonts.mono_small_right, rect_f(rx - 218.0, ry, 66.0, row_h - 2.0), theme.sub);
                draw_text(rt, &fmt_num(s.counts.total_tokens(), human), &fonts.mono_med_right, rect_f(rx - 146.0, ry, 72.0, row_h - 2.0), theme.text);
                draw_text(rt, &fmt_compact(s.counts.peak_context), &fonts.mono_small_right, rect_f(rx - 68.0, ry, 68.0, row_h - 2.0), theme.sub);

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

        // Bottom: Turn-by-Turn Context Anatomy + Per-Session Cache Hit Summary
        let bot_c = rect_f(body.left, top_c.bottom + 12.0, bw, (body.bottom - top_c.bottom - 12.0).max(140.0));
        fill_round_rect(rt, bot_c, 10.0, theme.surface);
        stroke_round_rect(rt, bot_c, 10.0, theme.border, 1.0);

        let sel = self
            .selected_session
            .as_deref()
            .and_then(|id| self.cached_sessions.iter().find(|s| s.conversation_id == id))
            .cloned();

        if let Some(s) = sel {
            let hdr = format!("逐轮剖析 · {}", s.title);
            draw_text(rt, &hdr, &fonts.ui_section, rect_f(bot_c.left + 16.0, bot_c.top + 9.0, (bw - 320.0).max(160.0), 19.0), theme.text);
            let mut meta = format!(
                "{} 轮  ·  缓存命中 {:.1}%  ·  非缓存 {}  ·  总计 {}  ·  峰值Ctx {}",
                s.turns.len(),
                s.counts.cache_hit_rate(),
                fmt_compact(s.counts.non_cached_tokens()),
                fmt_compact(s.counts.total_tokens()),
                fmt_compact(s.counts.peak_context),
            );
            let cp = s.compaction_count();
            if cp > 0 {
                meta.push_str(&format!("  ·  {} 次压缩", cp));
            }
            let ws = s.workspace_short();
            if !ws.is_empty() {
                meta.push_str(&format!("  ·  {}", ws));
            }
            draw_text(rt, &meta, &fonts.mono_small, rect_f(bot_c.left + 16.0, bot_c.top + 28.0, (bw - 96.0).max(160.0), 15.0), theme.sub);
            let s_copied = self.session_copied_until.map(|t| Instant::now() < t).unwrap_or(false);
            let copy_r = rect_f(bot_c.right - 72.0, bot_c.top + 27.0, 58.0, 17.0);
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

            let bars: Vec<BarItem> = s
                .turns
                .iter()
                .map(|t| {
                    let lbl = if t.step_index >= 0 { format!("#{}", t.step_index) } else { "-".into() };
                    let ts = t.timestamp_utc.map(|u| u.with_timezone(&Local).format("%H:%M:%S").to_string()).unwrap_or_else(|| "-".into());
                    let step_hdr = if t.is_checkpoint {
                        format!("Step {} · 上下文压缩 ({})", lbl, ts)
                    } else {
                        format!("Step {} ({})", lbl, ts)
                    };
                    BarItem {
                        label: lbl.clone(),
                        cache: t.cache_read_tokens,
                        input: t.input_tokens,
                        output: t.output_tokens,
                        marker: t.is_checkpoint,
                        tooltip: vec![
                            step_hdr,
                            format!("模型   : {}", t.model),
                            format!("新输入 : {}", fmt_commas(t.input_tokens)),
                            format!("缓存读 : {} ({:.1}%)", fmt_commas(t.cache_read_tokens), t.cache_hit_rate()),
                            format!("Ctx    : {}", fmt_commas(t.total_input())),
                            format!("输出   : {}", fmt_commas(t.output_tokens)),
                        ],
                    }
                })
                .collect();
            let plot = rect_f(bot_c.left + 12.0, bot_c.top + 48.0, bw - 24.0, (bot_c.bottom - bot_c.top - 56.0).max(60.0));
            let _ = draw_editorial_bars(rt, fonts, theme, plot, &bars, self.fresh_only, self.mouse_dip, "无逐轮记录");
        } else {
            draw_text(rt, "点击上方会话查看逐轮上下文增长趋势", &fonts.ui_reg_center, bot_c, theme.muted);
        }
    }

    // ---------- System Telemetry Studio (Opened via Hero CPU/RAM Ring Block) ----------

    #[allow(clippy::too_many_arguments)]
    fn draw_system_tab(
        &self,
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

        let foot = format!(
            "严格只读 · 0 写盘 · 不碰 SQLite · 增量扫描命中 {} / {} 文件 ({:.1} ms) · {}",
            snapshot.files_cached,
            snapshot.files_cached + snapshot.files_reparsed,
            snapshot.scan_ms,
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
            let ry = body_top + vi as f32 * row_h;
            let rr = rect_f(bounds.left + 8.0, ry, bounds.right - bounds.left - 20.0, row_h - 1.0);
            let hovered = self.mouse_dip.map(|(mx, my)| hit_rect(&rr, mx, my)).unwrap_or(false);
            if hovered {
                fill_round_rect(rt, rr, 6.0, theme.hover);
            }
            if let Some(acts) = row_actions {
                if let Some(act) = acts.get(self.scroll_rows + vi) {
                    self.hit_boxes.push(HitBox {
                        rect: rr,
                        action: act.clone(),
                    });
                }
            }
            if vi > 0 {
                draw_line(rt, bounds.left + 14.0, ry, bounds.right - 14.0, ry, theme.grid, 0.8);
            }

            let mut rx = bounds.left + pad_x;
            for (ci, &(col_title, frac, right_align)) in cols.iter().enumerate() {
                let cw = inner_w * frac;
                let cell = row.get(ci).map(String::as_str).unwrap_or("");
                let color = if cell.starts_with('+') && cell.ends_with('%') {
                    theme.accent
                } else if cell.starts_with('-') && cell.ends_with('%') {
                    theme.sage
                } else if col_title == "命中率" {
                    match cell.trim_end_matches('%').parse::<f64>() {
                        Ok(v) if v >= 85.0 => theme.sage,
                        Ok(v) if v < 65.0 && v > 0.0 => theme.warn,
                        _ => theme.sub,
                    }
                } else if ci == 0 || col_title == "总 Token" {
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
            });
        }
        let mut models = BTreeSet::new();
        models.insert(model.to_string());
        SessionInfo {
            conversation_id: id.to_string(),
            title: id.to_string(),
            workspace: ws.to_string(),
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

        let mut view = DashboardView::new(false, DateScope::All);
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
    fn test_date_scope_roundtrip() {
        for sc in [DateScope::Today, DateScope::Days7, DateScope::Days30, DateScope::All] {
            assert_eq!(DateScope::from_u8(sc.to_u8()), sc);
        }
        assert_eq!(DateScope::from_u8(99), DateScope::Today);
    }
}
