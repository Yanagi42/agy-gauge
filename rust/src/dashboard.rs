//! Editorial-grade Direct2D + DirectWrite analytics dashboard.
//!
//! Design principles:
//!   - Warm neutral Obsidian Charcoal / Warm Paper themes (zero neon AI blue/purple/mint).
//!   - Underline editorial navigation, hairline dividers, radial ring gauges, 2x2 Bento Overview.
//!   - Interactive "总计 / 仅增量" toggle so non-cached fresh work is never drowned out by 95% cache read.
//!   - Mouse hover crosshair on the 90-second live CPU/RAM telemetry studio.

use chrono::{Local, NaiveDate};
use std::collections::HashMap;
use std::time::Instant;

use windows::core::Result;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
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
    weekday_cn, AggregatedStats, DateScope, ScanSnapshot, SessionInfo, SessionSort, TokenCounts,
};
use crate::telemetry::{apply_win11_frame, copy_text_to_clipboard, SystemTelemetry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Trends,
    Sessions,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendGranularity {
    Daily,
    Weekly,
    Monthly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickAction {
    SelectTab(Tab),
    SelectGranularity(TrendGranularity),
    SelectScope(DateScope),
    ToggleFreshOnly,
    ToggleLightTheme,
    ToggleHuman,
    CopySummary,
    Refresh,
    SelectSort(SessionSort),
    SelectSession(String),
    InspectSession(String),
}

struct HitBox {
    rect: D2D_RECT_F,
    action: ClickAction,
}

pub struct DashboardView {
    surface: Option<WindowSurface>,
    pub active_tab: Tab,
    pub granularity: TrendGranularity,
    pub scope: DateScope,
    pub sort: SessionSort,
    pub fresh_only: bool,
    pub selected_session: Option<String>,
    pub mouse_dip: Option<(f32, f32)>,
    scroll_rows: usize,
    max_scroll: usize,
    hit_boxes: Vec<HitBox>,
    copied_until: Option<Instant>,
    cached_sessions: Vec<SessionInfo>,
    cached_agg: AggregatedStats,
    dirty_filter: bool,
}

impl DashboardView {
    pub fn new(fresh_only: bool) -> Self {
        Self {
            surface: None,
            active_tab: Tab::Overview,
            granularity: TrendGranularity::Daily,
            scope: DateScope::All,
            sort: SessionSort::Recent,
            fresh_only,
            selected_session: None,
            mouse_dip: None,
            scroll_rows: 0,
            max_scroll: 0,
            hit_boxes: Vec::new(),
            copied_until: None,
            cached_sessions: Vec::new(),
            cached_agg: AggregatedStats::default(),
            dirty_filter: true,
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
                self.active_tab = t;
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::SelectGranularity(g) => {
                self.granularity = g;
                self.scroll_rows = 0;
                (false, false)
            }
            ClickAction::SelectScope(s) => {
                self.scope = s;
                self.scroll_rows = 0;
                self.dirty_filter = true;
                (false, false)
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
                    "AGY CLI Token 用量 ({})\n- 会话: {} | 调用: {}\n- 新增输入: {} | 缓存读取: {} (命中 {:.1}%)\n- 输出: {} | 总 Token: {} (非缓存增量: {})\n- 峰值上下文: {}",
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
                self.active_tab = Tab::Sessions;
                self.scroll_rows = 0;
                (false, false)
            }
        }
    }

    fn ensure_filtered(&mut self, snapshot: &ScanSnapshot) {
        if !self.dirty_filter {
            return;
        }
        let (sessions, agg) = filter_snapshot(snapshot, self.scope);
        self.cached_sessions = sessions;
        self.cached_agg = agg;
        self.dirty_filter = false;

        let keep = self
            .selected_session
            .as_ref()
            .map(|id| self.cached_sessions.iter().any(|s| &s.conversation_id == id))
            .unwrap_or(false);
        if !keep {
            self.selected_session = self.cached_sessions.first().map(|s| s.conversation_id.clone());
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
                Tab::Overview => self.draw_overview_tab(factories, rt, fonts, theme, body, telemetry, human),
                Tab::Trends => self.draw_trends_tab(rt, fonts, theme, body, human),
                Tab::Sessions => self.draw_sessions_tab(rt, fonts, theme, body, human),
                Tab::System => self.draw_system_tab(factories, rt, fonts, theme, body, snapshot, telemetry),
            }
        });
        if res.is_ok() {
            self.surface = Some(surface);
        }
        res
    }

    // ---------- Minimalist Editorial Header (Underline Navigation) ----------

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

        // Clean underline tabs (no clunky boxed pill-in-pill)
        let tabs = [
            (Tab::Overview, "概览", 48.0),
            (Tab::Trends, "按天 & 按周", 82.0),
            (Tab::Sessions, "会话追踪", 68.0),
            (Tab::System, "系统监控", 68.0),
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

        // Refresh
        let r_ref = rect_f(rx - 44.0, btn_y, 44.0, btn_h);
        self.draw_ghost_btn(rt, fonts, theme, r_ref, "刷新", false, ClickAction::Refresh);
        rx -= 50.0;

        // Copy summary
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

        // K/M toggle
        let r_km = rect_f(rx - 44.0, btn_y, 44.0, btn_h);
        self.draw_ghost_btn(rt, fonts, theme, r_km, if human { "K/M" } else { "精确" }, false, ClickAction::ToggleHuman);
        rx -= 50.0;

        // Theme toggle (Obsidian / Paper)
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

        // Quiet vertical divider
        draw_line(rt, rx, 14.0, rx, h - 14.0, theme.border, 1.0);
        rx -= 10.0;

        // Scope filter (borderless editorial text toggles)
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

    // ---------- Typographic Hero Strip (Clean Horizontal Band, No RGB Boxes) ----------

    #[allow(clippy::too_many_arguments)]
    fn draw_hero_strip(
        &self,
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
        let c3 = bw * 0.21;
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

        // Block 2: Fresh Input & Output (warm accents only on tiny legend dots, numbers stay clean neutral!)
        draw_text(rt, "新增输入 / 输出", &fonts.ui_micro, rect_f(x1 + 18.0, y + 9.0, c2 - 28.0, 14.0), theme.muted);
        let io_str = format!("{}  /  {}", fmt_compact(t.input_tokens), fmt_compact(t.output_tokens));
        draw_text(rt, &io_str, &fonts.mono_kpi, rect_f(x1 + 18.0, y + 26.0, c2 - 28.0, 24.0), theme.text);
        let sub2 = format!("缓存读 {}  ·  峰值Ctx {}", fmt_compact(t.cache_read_tokens), fmt_compact(t.peak_context));
        draw_text(rt, &sub2, &fonts.ui_micro, rect_f(x1 + 18.0, y + 53.0, c2 - 28.0, 14.0), theme.sub);

        // Block 3: Sessions & Calls
        draw_text(rt, "活跃规模", &fonts.ui_micro, rect_f(x2 + 18.0, y + 9.0, c3 - 28.0, 14.0), theme.muted);
        let act_str = format!("{} 会话 · {} 次", self.cached_sessions.len(), t.calls);
        draw_text(rt, &act_str, &fonts.mono_kpi, rect_f(x2 + 18.0, y + 26.0, c3 - 28.0, 24.0), theme.text);
        let avg = if t.calls > 0 { t.total_tokens() / t.calls } else { 0 };
        let sub3 = format!("均单次 {}  ·  覆盖 {} 天", fmt_compact(avg), self.cached_agg.by_day.len());
        draw_text(rt, &sub3, &fonts.ui_micro, rect_f(x2 + 18.0, y + 53.0, c3 - 28.0, 14.0), theme.sub);

        // Block 4: Radial Ring Gauges for CPU & RAM
        let tel = telemetry.latest;
        let g_cy = y + h * 0.5;
        let c_cx = x3 + 34.0;
        draw_ring_gauge(factories, rt, c_cx, g_cy, 17.0, 3.0, tel.cpu_pct, theme.grid, theme.accent);
        draw_text(rt, &format!("{:.0}%", tel.cpu_pct), &fonts.mono_small_center, rect_f(c_cx - 18.0, g_cy - 8.0, 36.0, 16.0), theme.text);

        let r_cx = x3 + 84.0;
        draw_ring_gauge(factories, rt, r_cx, g_cy, 17.0, 3.0, tel.mem_pct, theme.grid, theme.ram);
        draw_text(rt, &format!("{:.0}%", tel.mem_pct), &fonts.mono_small_center, rect_f(r_cx - 18.0, g_cy - 8.0, 36.0, 16.0), theme.text);

        let tx_l = x3 + 114.0;
        let tx_w = (band.right - tx_l - 12.0).max(60.0);
        draw_text(rt, "CPU / 内存", &fonts.ui_micro, rect_f(tx_l, y + 15.0, tx_w, 14.0), theme.muted);
        draw_text(
            rt,
            &format!("{:.1}/{:.1} GB", tel.mem_used_gb, tel.mem_total_gb),
            &fonts.mono_med,
            rect_f(tx_l, y + 31.0, tx_w, 18.0),
            theme.text,
        );
        draw_text(rt, &format!("{} 核实时", tel.cpu_cores), &fonts.ui_micro, rect_f(tx_l, y + 50.0, tx_w, 14.0), theme.sub);
    }

    // ---------- Tab 1: Editorial 2x2 Bento Overview ----------

    #[allow(clippy::too_many_arguments)]
    fn draw_overview_tab(
        &mut self,
        factories: &D2DFactories,
        rt: &ID2D1RenderTarget,
        fonts: &Fonts,
        theme: &Theme,
        body: D2D_RECT_F,
        telemetry: &SystemTelemetry,
        human: bool,
    ) {
        let gap = 14.0;
        let bw = body.right - body.left;
        let bh = body.bottom - body.top;
        let left_w = ((bw - gap) * 0.61).round();
        let right_w = bw - gap - left_w;
        let top_h = ((bh - gap) * 0.55).round();
        let bot_h = bh - gap - top_h;

        // 1) Top-Left: 14-day Daily Trend with "总计 | 仅增量" toggle
        let tl = rect_f(body.left, body.top, left_w, top_h);
        fill_round_rect(rt, tl, 10.0, theme.surface);
        stroke_round_rect(rt, tl, 10.0, theme.border, 1.0);
        draw_text(rt, "每日用量趋势", &fonts.ui_section, rect_f(tl.left + 16.0, tl.top + 11.0, 120.0, 20.0), theme.text);
        self.draw_chart_legend_and_toggle(rt, fonts, theme, tl);

        let today = Local::now().date_naive();
        let days14: Vec<BarItem> = (0..14u64)
            .rev()
            .filter_map(|off| today.checked_sub_days(chrono::Days::new(off)))
            .map(|d| {
                let k = d.format("%Y-%m-%d").to_string();
                let c = self.cached_agg.by_day.get(&k).copied().unwrap_or_default();
                BarItem {
                    label: d.format("%m/%d").to_string(),
                    cache: c.cache_read_tokens,
                    input: c.input_tokens,
                    output: c.output_tokens,
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

        let tl_plot = rect_f(tl.left + 10.0, tl.top + 38.0, left_w - 20.0, top_h - 46.0);
        draw_editorial_bars(rt, fonts, theme, tl_plot, &days14, self.fresh_only, self.mouse_dip, "暂无数据");

        // 2) Top-Right: 24h Rhythm Heatmap + Model Breakdown
        let tr = rect_f(tl.right + gap, body.top, right_w, top_h);
        fill_round_rect(rt, tr, 10.0, theme.surface);
        stroke_round_rect(rt, tr, 10.0, theme.border, 1.0);
        draw_text(rt, "24 小时活跃节律", &fonts.ui_section, rect_f(tr.left + 16.0, tr.top + 11.0, 140.0, 20.0), theme.text);

        let split_y = tr.top + top_h * 0.54;
        self.draw_hourly_rhythm(rt, fonts, theme, rect_f(tr.left + 14.0, tr.top + 36.0, right_w - 28.0, split_y - tr.top - 42.0));

        draw_line(rt, tr.left + 16.0, split_y, tr.right - 16.0, split_y, theme.border, 1.0);
        draw_text(rt, "模型分布", &fonts.ui_section, rect_f(tr.left + 16.0, split_y + 10.0, 100.0, 18.0), theme.text);
        self.draw_compact_models(rt, fonts, theme, rect_f(tr.left + 16.0, split_y + 34.0, right_w - 32.0, tr.bottom - split_y - 42.0), human);

        // 3) Bottom-Left: Top 5 Recent Sessions (Click jumps to Sessions tab!)
        let bl = rect_f(body.left, tl.bottom + gap, left_w, bot_h);
        fill_round_rect(rt, bl, 10.0, theme.surface);
        stroke_round_rect(rt, bl, 10.0, theme.border, 1.0);
        draw_text(rt, "最近活跃会话", &fonts.ui_section, rect_f(bl.left + 16.0, bl.top + 11.0, 140.0, 20.0), theme.text);
        draw_text(rt, "点击查看逐轮上下文 →", &fonts.ui_micro, rect_f(bl.right - 150.0, bl.top + 13.0, 134.0, 16.0), theme.muted);

        let list_y = bl.top + 38.0;
        let row_h = ((bot_h - 48.0) / 5.0).clamp(24.0, 34.0);
        if self.cached_sessions.is_empty() {
            draw_text(rt, "当前范围暂无会话", &fonts.ui_reg_center, rect_f(bl.left, list_y, left_w, bot_h - 48.0), theme.muted);
        } else {
            for (i, s) in self.cached_sessions.iter().take(5).enumerate() {
                let ry = list_y + i as f32 * row_h;
                let rr = rect_f(bl.left + 10.0, ry, left_w - 20.0, row_h - 2.0);
                let hov = self.mouse_dip.map(|(mx, my)| hit_rect(&rr, mx, my)).unwrap_or(false);
                if hov {
                    fill_round_rect(rt, rr, 6.0, theme.hover);
                }
                if i > 0 {
                    draw_line(rt, bl.left + 16.0, ry - 1.0, bl.right - 16.0, ry - 1.0, theme.grid, 0.8);
                }
                let t_str = s.last_time.map(|t| t.format("%m/%d %H:%M").to_string()).unwrap_or_else(|| "-".into());
                let rx = rr.right - 8.0;
                let title_w = (rx - 220.0 - (rr.left + 84.0)).max(80.0);
                draw_text(rt, &t_str, &fonts.mono_small, rect_f(rr.left + 8.0, ry, 72.0, row_h - 2.0), theme.muted);
                draw_text(rt, &s.title, &fonts.ui_reg, rect_f(rr.left + 84.0, ry, title_w, row_h - 2.0), theme.text);
                draw_text(rt, &format!("{}轮", s.counts.calls), &fonts.mono_small_right, rect_f(rx - 204.0, ry, 46.0, row_h - 2.0), theme.muted);
                draw_text(rt, &fmt_num(s.counts.total_tokens(), human), &fonts.mono_med_right, rect_f(rx - 150.0, ry, 74.0, row_h - 2.0), theme.text);
                draw_text(rt, &format!("Ctx {}", fmt_compact(s.counts.peak_context)), &fonts.mono_small_right, rect_f(rx - 70.0, ry, 70.0, row_h - 2.0), theme.sub);

                self.hit_boxes.push(HitBox {
                    rect: rr,
                    action: ClickAction::InspectSession(s.conversation_id.clone()),
                });
            }
        }

        // 4) Bottom-Right: Current ISO Week Scorecard + Live Mini Dual Sparkline
        let br = rect_f(tl.right + gap, tr.bottom + gap, right_w, bot_h);
        fill_round_rect(rt, br, 10.0, theme.surface);
        stroke_round_rect(rt, br, 10.0, theme.border, 1.0);

        let cur_wk = iso_week_key(today);
        let wk_c = self.cached_agg.by_week.get(&cur_wk).copied().unwrap_or_default();
        let wk_days = self.cached_agg.week_active_days.get(&cur_wk).copied().unwrap_or(0);
        let wk_avg = if wk_days > 0 { wk_c.total_tokens() / wk_days as u64 } else { 0 };

        draw_text(rt, &format!("本周 · {}", cur_wk), &fonts.ui_section, rect_f(br.left + 16.0, br.top + 11.0, 140.0, 20.0), theme.text);
        draw_text(rt, &iso_week_range_label(&cur_wk), &fonts.mono_small_right, rect_f(br.right - 116.0, br.top + 13.0, 100.0, 16.0), theme.muted);

        draw_text(rt, &fmt_num(wk_c.total_tokens(), human), &fonts.mono_kpi, rect_f(br.left + 16.0, br.top + 36.0, right_w - 32.0, 24.0), theme.text);
        draw_text(
            rt,
            &format!("日均 {}  ·  活跃 {} 天  ·  非缓存 {}", fmt_compact(wk_avg), wk_days, fmt_compact(wk_c.non_cached_tokens())),
            &fonts.ui_micro,
            rect_f(br.left + 16.0, br.top + 63.0, right_w - 32.0, 15.0),
            theme.sub,
        );

        let sep_y = br.top + 88.0;
        draw_line(rt, br.left + 16.0, sep_y, br.right - 16.0, sep_y, theme.border, 1.0);
        let tel = telemetry.latest;
        draw_text(rt, "实时 CPU / 内存波形", &fonts.ui_micro, rect_f(br.left + 16.0, sep_y + 8.0, 130.0, 14.0), theme.muted);
        draw_text(
            rt,
            &format!("CPU {:.0}% · RAM {:.0}%", tel.cpu_pct, tel.mem_pct),
            &fonts.mono_small_right,
            rect_f(br.right - 146.0, sep_y + 8.0, 130.0, 14.0),
            theme.sub,
        );

        let sp_l = br.left + 16.0;
        let sp_r = br.right - 16.0;
        let sp_t = sep_y + 28.0;
        let sp_b = br.bottom - 14.0;
        if sp_b > sp_t + 16.0 && telemetry.history.len() >= 2 {
            let max_pts = 45usize;
            let slice: Vec<_> = telemetry.history.iter().rev().take(max_pts).collect();
            let n = slice.len();
            let dx = (sp_r - sp_l) / (max_pts - 1) as f32;
            let x0 = sp_l + (max_pts - n) as f32 * dx;
            let mut c_pts = Vec::with_capacity(n);
            let mut m_pts = Vec::with_capacity(n);
            for (i, s) in slice.iter().rev().enumerate() {
                let px = x0 + i as f32 * dx;
                c_pts.push((px, sp_b - (s.cpu_pct.clamp(0.0, 100.0) / 100.0) * (sp_b - sp_t)));
                m_pts.push((px, sp_b - (s.mem_pct.clamp(0.0, 100.0) / 100.0) * (sp_b - sp_t)));
            }
            draw_line(rt, sp_l, sp_b, sp_r, sp_b, theme.grid, 1.0);
            draw_area_series(factories, rt, &m_pts, sp_b, sp_t, theme.ram, 1.4, 0.10);
            draw_area_series(factories, rt, &c_pts, sp_b, sp_t, theme.accent, 1.6, 0.18);
        }
    }

    fn draw_hourly_rhythm(&self, rt: &ID2D1RenderTarget, fonts: &Fonts, theme: &Theme, bounds: D2D_RECT_F) {
        let w = bounds.right - bounds.left;
        let h = bounds.bottom - bounds.top;
        let plot_b = bounds.bottom - 16.0;
        let plot_h = (h - 20.0).max(20.0);
        let max_v = self
            .cached_agg
            .by_hour
            .iter()
            .map(|c| if self.fresh_only { c.non_cached_tokens() } else { c.total_tokens() })
            .max()
            .unwrap_or(0)
            .max(1);

        let slot = w / 24.0;
        let bw = (slot * 0.62).clamp(3.0, 10.0);
        let mut hov_h: Option<usize> = None;
        if let Some((mx, my)) = self.mouse_dip {
            if mx >= bounds.left && mx <= bounds.right && my >= bounds.top && my <= plot_b {
                let idx = ((mx - bounds.left) / slot).floor() as isize;
                if (0..24).contains(&idx) {
                    hov_h = Some(idx as usize);
                }
            }
        }

        for (hr, c) in self.cached_agg.by_hour.iter().enumerate() {
            let val = if self.fresh_only { c.non_cached_tokens() } else { c.total_tokens() };
            let cx = bounds.left + (hr as f32 + 0.5) * slot;
            let x0 = cx - bw * 0.5;
            if val > 0 {
                let ratio = (val as f64 / max_v as f64) as f32;
                let bh = (ratio * plot_h).max(3.0);
                let col = with_alpha(theme.accent, 0.28 + 0.72 * ratio);
                fill_round_rect(rt, rect_f(x0, plot_b - bh, bw, bh), (bw * 0.35).min(2.5), col);
            } else {
                fill_round_rect(rt, rect_f(x0, plot_b - 2.0, bw, 2.0), 1.0, theme.grid);
            }
            if hr % 6 == 0 || hr == 23 {
                draw_text(rt, &format!("{:02}", hr), &fonts.mono_small_center, rect_f(cx - 12.0, plot_b + 2.0, 24.0, 14.0), theme.muted);
            }
        }

        if let Some(hr) = hov_h {
            let c = &self.cached_agg.by_hour[hr];
            let tip = format!("{:02}:00  ·  {} ({}次)", hr, fmt_compact(c.total_tokens()), c.calls);
            draw_text(rt, &tip, &fonts.mono_small_right, rect_f(bounds.right - 170.0, bounds.top - 22.0, 170.0, 16.0), theme.accent);
        }
    }

    fn draw_compact_models(&self, rt: &ID2D1RenderTarget, fonts: &Fonts, theme: &Theme, bounds: D2D_RECT_F, human: bool) {
        let mut items: Vec<(&String, &TokenCounts)> = self.cached_agg.by_model.iter().collect();
        items.sort_by(|a, b| b.1.total_tokens().cmp(&a.1.total_tokens()));
        if items.is_empty() {
            draw_text(rt, "暂无模型数据", &fonts.ui_small, bounds, theme.muted);
            return;
        }
        let total_all = self.cached_agg.total.total_tokens().max(1);
        let row_h = ((bounds.bottom - bounds.top) / items.len().clamp(1, 3) as f32).clamp(24.0, 34.0);
        let w = bounds.right - bounds.left;
        for (i, (name, c)) in items.iter().take(3).enumerate() {
            let y = bounds.top + i as f32 * row_h;
            let share = (c.total_tokens() as f64 / total_all as f64) as f32;
            draw_text(rt, name, &fonts.mono_small, rect_f(bounds.left, y, w * 0.58, 16.0), theme.text);
            let stat = format!("{}  ({:.0}%)", fmt_num(c.total_tokens(), human), share * 100.0);
            draw_text(rt, &stat, &fonts.mono_small_right, rect_f(bounds.left + w * 0.56, y, w * 0.44, 16.0), theme.sub);
            fill_round_rect(rt, rect_f(bounds.left, y + 18.0, w, 3.5), 1.75, theme.grid);
            fill_round_rect(rt, rect_f(bounds.left, y + 18.0, (w * share).max(3.0), 3.5), 1.75, if i == 0 { theme.accent } else { theme.output });
        }
    }

    fn draw_chart_legend_and_toggle(&mut self, rt: &ID2D1RenderTarget, fonts: &Fonts, theme: &Theme, card: D2D_RECT_F) {
        // Right-aligned "总计 | 仅增量" toggle pill
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

        // Quiet inline dot legend just left of toggle
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

    // ---------- Tab 2: Deep Daily / Weekly (ISO) / Monthly Trends ----------

    fn draw_trends_tab(&mut self, rt: &ID2D1RenderTarget, fonts: &Fonts, theme: &Theme, body: D2D_RECT_F, human: bool) {
        let bw = body.right - body.left;
        let bh = body.bottom - body.top;
        let chart_h = (bh * 0.50).clamp(190.0, 275.0);
        let chart_c = rect_f(body.left, body.top, bw, chart_h);
        fill_round_rect(rt, chart_c, 10.0, theme.surface);
        stroke_round_rect(rt, chart_c, 10.0, theme.border, 1.0);

        // Granularity sub-tabs inside chart card header
        let mut gx = chart_c.left + 14.0;
        for (g, lbl, gw) in [
            (TrendGranularity::Daily, "按天", 46.0),
            (TrendGranularity::Weekly, "按周 (ISO)", 76.0),
            (TrendGranularity::Monthly, "按月", 46.0),
        ] {
            let r = rect_f(gx, chart_c.top + 9.0, gw, 22.0);
            let act = self.granularity == g;
            if act {
                fill_round_rect(rt, r, 6.0, theme.hover);
            }
            draw_text(rt, lbl, &fonts.ui_small_center, r, if act { theme.text } else { theme.muted });
            self.hit_boxes.push(HitBox { rect: r, action: ClickAction::SelectGranularity(g) });
            gx += gw + 4.0;
        }
        self.draw_chart_legend_and_toggle(rt, fonts, theme, chart_c);

        let plot = rect_f(chart_c.left + 12.0, chart_c.top + 40.0, bw - 24.0, chart_h - 48.0);
        let tbl_r = rect_f(body.left, chart_c.bottom + 12.0, bw, (body.bottom - chart_c.bottom - 12.0).max(110.0));

        match self.granularity {
            TrendGranularity::Daily => {
                let days_asc: Vec<(&String, &TokenCounts)> = self.cached_agg.by_day.iter().collect();
                let chart_days: Vec<(String, TokenCounts)> = if self.scope != DateScope::Today && days_asc.len() < 14 && !days_asc.is_empty() {
                    let today = Local::now().date_naive();
                    (0..14u64)
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
                draw_editorial_bars(rt, fonts, theme, plot, &bars, self.fresh_only, self.mouse_dip, "暂无按天数据");

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
                );
            }
            TrendGranularity::Weekly => {
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
                draw_editorial_bars(rt, fonts, theme, plot, &bars, self.fresh_only, self.mouse_dip, "暂无按周数据");

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
                );
            }
            TrendGranularity::Monthly => {
                let m_asc: Vec<(&String, &TokenCounts)> = self.cached_agg.by_month.iter().collect();
                let bars: Vec<BarItem> = m_asc
                    .iter()
                    .map(|(m, c)| BarItem {
                        label: (*m).clone(),
                        cache: c.cache_read_tokens,
                        input: c.input_tokens,
                        output: c.output_tokens,
                        tooltip: vec![
                            format!("{} · {} 次调用", m, c.calls),
                            format!("总计   : {}", fmt_commas(c.total_tokens())),
                            format!("新输入 : {}", fmt_commas(c.input_tokens)),
                            format!("缓存读 : {} ({:.1}%)", fmt_commas(c.cache_read_tokens), c.cache_hit_rate()),
                            format!("输出   : {}", fmt_commas(c.output_tokens)),
                        ],
                    })
                    .collect();
                draw_editorial_bars(rt, fonts, theme, plot, &bars, self.fresh_only, self.mouse_dip, "暂无按月数据");

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
                );
            }
        }
    }

    // ---------- Tab 3: Session Explorer + Turn-by-Turn Context Anatomy ----------

    fn draw_sessions_tab(&mut self, rt: &ID2D1RenderTarget, fonts: &Fonts, theme: &Theme, body: D2D_RECT_F, human: bool) {
        let bw = body.right - body.left;
        let bh = body.bottom - body.top;
        let top_h = (bh * 0.46).clamp(150.0, 250.0);
        let top_c = rect_f(body.left, body.top, bw, top_h);
        fill_round_rect(rt, top_c, 10.0, theme.surface);
        stroke_round_rect(rt, top_c, 10.0, theme.border, 1.0);

        draw_text(rt, "会话记录", &fonts.ui_section, rect_f(top_c.left + 16.0, top_c.top + 10.0, 90.0, 20.0), theme.text);

        let sorts = [
            (SessionSort::Recent, "最近", 40.0),
            (SessionSort::TotalTokens, "总Token", 56.0),
            (SessionSort::OutputTokens, "输出", 40.0),
            (SessionSort::PeakContext, "峰值Ctx", 54.0),
        ];
        let mut sx = top_c.right - 14.0;
        for (so, lbl, sw) in sorts.iter().rev() {
            sx -= *sw + 4.0;
            let r = rect_f(sx, top_c.top + 9.0, *sw, 22.0);
            let act = self.sort == *so;
            if act {
                fill_round_rect(rt, r, 5.0, theme.hover);
            }
            draw_text(rt, lbl, &fonts.ui_small_center, r, if act { theme.text } else { theme.muted });
            self.hit_boxes.push(HitBox { rect: r, action: ClickAction::SelectSort(*so) });
        }

        let mut sorted = self.cached_sessions.clone();
        match self.sort {
            SessionSort::Recent => sorted.sort_by(|a, b| b.last_time.cmp(&a.last_time)),
            SessionSort::TotalTokens => sorted.sort_by(|a, b| b.counts.total_tokens().cmp(&a.counts.total_tokens())),
            SessionSort::OutputTokens => sorted.sort_by(|a, b| b.counts.output_tokens.cmp(&a.counts.output_tokens)),
            SessionSort::PeakContext => sorted.sort_by(|a, b| b.counts.peak_context.cmp(&a.counts.peak_context)),
        }

        let list_y = top_c.top + 38.0;
        let row_h = 28.0;
        let vis = (((top_c.bottom - 8.0 - list_y) / row_h).floor() as usize).max(1);
        self.max_scroll = sorted.len().saturating_sub(vis);
        if self.scroll_rows > self.max_scroll {
            self.scroll_rows = self.max_scroll;
        }

        for (vi, s) in sorted.iter().skip(self.scroll_rows).take(vis).enumerate() {
            let ry = list_y + vi as f32 * row_h;
            let rr = rect_f(top_c.left + 10.0, ry, bw - 22.0, row_h - 2.0);
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
            let m_str = s.models.iter().next().cloned().unwrap_or_else(|| "-".into());
            let rx = rr.right - 10.0;
            let title_x = rr.left + 162.0;
            let title_w = (rx - 404.0 - title_x).max(80.0);

            draw_text(rt, sid, &fonts.mono_small, rect_f(rr.left + 10.0, ry, 62.0, row_h - 2.0), theme.muted);
            draw_text(rt, &t_str, &fonts.mono_small, rect_f(rr.left + 76.0, ry, 82.0, row_h - 2.0), theme.sub);
            draw_text(rt, &s.title, &fonts.ui_reg, rect_f(title_x, ry, title_w, row_h - 2.0), theme.text);

            draw_text(rt, &m_str, &fonts.mono_small_right, rect_f(rx - 398.0, ry, 146.0, row_h - 2.0), theme.muted);
            draw_text(rt, &format!("{}轮", s.counts.calls), &fonts.mono_small_right, rect_f(rx - 246.0, ry, 46.0, row_h - 2.0), theme.sub);
            draw_text(rt, &fmt_num(s.counts.total_tokens(), human), &fonts.mono_med_right, rect_f(rx - 194.0, ry, 86.0, row_h - 2.0), theme.text);
            draw_text(rt, &format!("峰值 {}", fmt_compact(s.counts.peak_context)), &fonts.mono_small_right, rect_f(rx - 100.0, ry, 100.0, row_h - 2.0), theme.sub);

            self.hit_boxes.push(HitBox {
                rect: rr,
                action: ClickAction::SelectSession(s.conversation_id.clone()),
            });
        }

        // Bottom: Turn-by-Turn Context Anatomy
        let bot_c = rect_f(body.left, top_c.bottom + 12.0, bw, (body.bottom - top_c.bottom - 12.0).max(140.0));
        fill_round_rect(rt, bot_c, 10.0, theme.surface);
        stroke_round_rect(rt, bot_c, 10.0, theme.border, 1.0);

        let sel = self
            .selected_session
            .as_deref()
            .and_then(|id| self.cached_sessions.iter().find(|s| s.conversation_id == id))
            .cloned();

        if let Some(s) = sel {
            let hdr = format!("逐轮上下文剖析 · {}  ({} 轮)", s.title, s.turns.len());
            draw_text(rt, &hdr, &fonts.ui_section, rect_f(bot_c.left + 16.0, bot_c.top + 10.0, (bw - 320.0).max(160.0), 20.0), theme.text);
            self.draw_chart_legend_and_toggle(rt, fonts, theme, bot_c);

            let bars: Vec<BarItem> = s
                .turns
                .iter()
                .map(|t| {
                    let lbl = if t.step_index >= 0 { format!("#{}", t.step_index) } else { "-".into() };
                    let ts = t.timestamp_utc.map(|u| u.with_timezone(&Local).format("%H:%M:%S").to_string()).unwrap_or_else(|| "-".into());
                    BarItem {
                        label: lbl.clone(),
                        cache: t.cache_read_tokens,
                        input: t.input_tokens,
                        output: t.output_tokens,
                        tooltip: vec![
                            format!("Step {} ({})", lbl, ts),
                            format!("模型   : {}", t.model),
                            format!("新输入 : {}", fmt_commas(t.input_tokens)),
                            format!("缓存读 : {} ({:.1}%)", fmt_commas(t.cache_read_tokens), t.cache_hit_rate()),
                            format!("上下文 : {}", fmt_commas(t.total_input())),
                            format!("输出   : {}", fmt_commas(t.output_tokens)),
                        ],
                    }
                })
                .collect();
            let plot = rect_f(bot_c.left + 12.0, bot_c.top + 40.0, bw - 24.0, bot_c.bottom - bot_c.top - 48.0);
            draw_editorial_bars(rt, fonts, theme, plot, &bars, self.fresh_only, self.mouse_dip, "无逐轮记录");
        } else {
            draw_text(rt, "点击上方会话查看逐轮上下文增长趋势", &fonts.ui_reg_center, bot_c, theme.muted);
        }
    }

    // ---------- Tab 4: Minimalist Telemetry Studio (90s Waveform + Interactive Crosshair) ----------

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

        // Legend top-right
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

            // Interactive hover crosshair along the 90s timeline
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

        // Bottom 2 clean radial gauge cards
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
            &format!("详情窗独立进程 {:.1} MB · 关闭即归零 · 常驻胶囊 ~8–11 MB", cur.self_mb),
            &fonts.ui_micro,
            rect_f(c_right.left + 114.0, cy_top + 76.0, half_w - 130.0, 16.0),
            theme.muted,
        );

        // Quiet single-line editorial footer (no noisy neon trust badges!)
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

    fn draw_table(
        &mut self,
        rt: &ID2D1RenderTarget,
        fonts: &Fonts,
        theme: &Theme,
        bounds: D2D_RECT_F,
        cols: &[(&str, f32, bool)],
        rows: &[Vec<String>],
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
            if vi > 0 {
                draw_line(rt, bounds.left + 14.0, ry, bounds.right - 14.0, ry, theme.grid, 0.8);
            }

            let mut rx = bounds.left + pad_x;
            for (ci, &(_, frac, right_align)) in cols.iter().enumerate() {
                let cw = inner_w * frac;
                let cell = row.get(ci).map(String::as_str).unwrap_or("");
                let color = if cell.starts_with('+') && cell.ends_with('%') {
                    theme.accent
                } else if cell.starts_with('-') && cell.ends_with('%') {
                    theme.sage
                } else if ci == 0 {
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
