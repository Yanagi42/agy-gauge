//! Tactile, instrument-grade bottom-right floating capsule (iStat Menus / Teenage Engineering inspired).
//!
//! Features:
//!   - True per-pixel alpha capsule with soft feathered ambient shadow & 0.75px neutral hairline rim.
//!   - User-composable display segments (Today Total / Non-Cached Δ / Cache Hit% / Active Ctx / CPU / RAM)
//!     with exact DirectWrite text-metric dynamic sizing so every combination stays optically balanced.
//!   - Restrained Obsidian Charcoal or Warm Paper theme (zero AI neon cyan/purple/mint!).
//!   - No redundant hover popup: hovering smoothly illuminates the rim; left-click opens dashboard.

use chrono::{Local, Utc};
use windows::core::{w, Result};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Direct2D::ID2D1RenderTarget;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, SetForegroundWindow, TrackPopupMenu,
    MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, TPM_LEFTALIGN,
    TPM_RETURNCMD, TPM_RIGHTBUTTON,
};

use crate::d2d::{
    draw_area_series, draw_line, draw_ring_gauge, draw_text, fill_circle, fill_round_rect,
    linear_v_brush, load_color, rect_f, rgba, stroke_round_rect, theme_for, with_alpha,
    D2DFactories, Fonts, LayeredSurface, Theme,
};
use crate::pricing::fmt_cost_compact;
use crate::quota::{AlertSeverity, QuotaProvenance};
use crate::stats::{fmt_compact, iso_week_key, ScanSnapshot, TokenCounts};
use crate::telemetry::{
    clamp_pos_to_work_area, compute_tray_flyout_pos, get_monitors, get_work_area,
    monitor_index_for_point, to_wide_null, wide_pcwstr, SystemTelemetry, PILL_ITEMS_ALL_MASK,
    PILL_ITEMS_DEFAULT, PILL_ITEM_ACTIVE_CTX, PILL_ITEM_CACHE_HIT, PILL_ITEM_COST, PILL_ITEM_CPU,
    PILL_ITEM_NON_CACHED, PILL_ITEM_QUOTA, PILL_ITEM_RAM, PILL_ITEM_TOTAL,
};

pub const CMD_OPEN_DETAIL: u32 = 1001;
pub const CMD_REFRESH: u32 = 1002;
pub const CMD_TOGGLE_PILL_VISIBLE: u32 = 1003;
pub const CMD_TOGGLE_LIGHT: u32 = 1004;
pub const CMD_TOGGLE_TOPMOST: u32 = 1005;
pub const CMD_TOGGLE_HUMAN: u32 = 1006;
pub const CMD_REDOCK: u32 = 1007;
pub const CMD_QUIT: u32 = 1008;
pub const CMD_TOGGLE_PRIVACY: u32 = 1009;
pub const CMD_TOGGLE_NOTIFICATIONS: u32 = 1010;
pub const CMD_TOGGLE_HIDE_FULLSCREEN: u32 = 1011;
pub const CMD_TOGGLE_AUTOSTART: u32 = 1012;
pub const CMD_TOGGLE_TRAY_HOVER: u32 = 1013;
pub const CMD_OPEN_SESSIONS_TAB: u32 = 1014;
pub const CMD_MONITOR_BASE: u32 = 1100;
pub const CMD_PILL_ITEM_BASE: u32 = 1200; // 1200..1208 -> bit 0..8
pub const CMD_PILL_ITEMS_RESET: u32 = 1210;
pub const CMD_QUICK_RESUME_BASE: u32 = 1300; // 1300..1304 -> top 5 resumable CLI sessions

#[derive(Debug, Clone)]
pub struct QuickSessionMenuItem {
    pub conversation_id: String,
    pub label: String,
}

const SHADOW_PAD: f32 = 10.0;
const PILL_H_STD: f32 = 30.0;

struct PillTokenSeg {
    text: String,
    width: f32,
    color: D2D1_COLOR_F,
}

fn pill_hit_color(theme: &Theme, c: &TokenCounts) -> D2D1_COLOR_F {
    if c.total_input() == 0 {
        theme.muted
    } else {
        let r = c.cache_hit_rate();
        if r >= 85.0 {
            theme.sage
        } else if r >= 60.0 {
            theme.warn
        } else {
            theme.danger
        }
    }
}

pub struct PillRenderer {
    surface: Option<LayeredSurface>,
    pos_cache_key: Option<(usize, Option<(i32, i32)>, i32, i32, i32)>,
    cached_pos: (i32, i32),
}

impl PillRenderer {
    pub fn new() -> Self {
        Self {
            surface: None,
            pos_cache_key: None,
            cached_pos: (0, 0),
        }
    }

    pub fn invalidate_position(&mut self) {
        self.pos_cache_key = None;
    }

    pub fn size_px(&self) -> (i32, i32) {
        self.surface.as_ref().map(|s| s.size_px()).unwrap_or((1, 1))
    }

    pub fn shadow_px(&self) -> i32 {
        let scale = self.surface.as_ref().map(|s| s.dpi() / 96.0).unwrap_or(1.0);
        (SHADOW_PAD * scale).round() as i32
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        factories: &D2DFactories,
        fonts: &Fonts,
        hwnd: HWND,
        dpi: f32,
        monitor_index: usize,
        custom_pos: Option<(i32, i32)>,
        flyout_anchor: Option<RECT>,
        light_theme: bool,
        hovered: bool,
        dash_open: bool,
        recent_activity: bool,
        pill_items: u32,
        snapshot: &ScanSnapshot,
        telemetry: &SystemTelemetry,
    ) -> Result<(i32, i32)> {
        let scale = dpi / 96.0;
        let theme = theme_for(light_theme);
        let items = {
            let m = pill_items & PILL_ITEMS_ALL_MASK;
            if m != 0 { m } else { PILL_ITEMS_DEFAULT }
        };

        let today_date = Local::now().date_naive();
        let today_key = today_date.format("%Y-%m-%d").to_string();
        let week_key = iso_week_key(today_date);
        let today_c: TokenCounts = snapshot
            .agg
            .by_day
            .get(&today_key)
            .copied()
            .unwrap_or_default();
        let week_c: TokenCounts = snapshot
            .agg
            .by_week
            .get(&week_key)
            .copied()
            .unwrap_or_default();
        let now_utc = Utc::now();

        let alerts = crate::quota::evaluate_alerts(
            &snapshot.budget,
            &today_key,
            today_c.cost.total_usd(),
            &week_key,
            week_c.cost.total_usd(),
            snapshot.statusline.as_ref(),
            now_utc,
        );
        let max_severity = alerts
            .iter()
            .map(|a| a.severity)
            .max()
            .unwrap_or(AlertSeverity::Normal);

        let show_cpu = (items & PILL_ITEM_CPU) != 0;
        let show_ram = (items & PILL_ITEM_RAM) != 0;
        let has_sys = show_cpu || show_ram;

        let mut segs: Vec<PillTokenSeg> = Vec::with_capacity(6);
        let mut ctx_warn = false;

        if (items & PILL_ITEM_TOTAL) != 0 {
            let text = fmt_compact(today_c.total_tokens());
            let width = factories.measure_text_width(&text, &fonts.pill_tok).max(16.0) + 2.0;
            segs.push(PillTokenSeg {
                text,
                width,
                color: theme.text,
            });
        }
        if (items & PILL_ITEM_COST) != 0 {
            let text = fmt_cost_compact(&today_c.cost, today_c.calls);
            let width = factories.measure_text_width(&text, &fonts.pill_tok).max(24.0) + 2.0;
            let day_sev = if snapshot.budget.daily_usd > 0.0 {
                snapshot
                    .budget
                    .classify_usage_pct((today_c.cost.total_usd() / snapshot.budget.daily_usd) * 100.0)
            } else {
                AlertSeverity::Normal
            };
            let color = match day_sev {
                AlertSeverity::Critical => theme.danger,
                AlertSeverity::Warn => theme.warn,
                AlertSeverity::Normal => {
                    if segs.is_empty() { theme.text } else { theme.output }
                }
            };
            segs.push(PillTokenSeg { text, width, color });
        }
        if (items & PILL_ITEM_QUOTA) != 0 {
            let official_seg = snapshot
                .statusline
                .as_ref()
                .filter(|s| s.provenance(now_utc) != QuotaProvenance::LocalEstimate)
                .and_then(|sl| sl.tightest_bucket())
                .and_then(|b| b.remaining_pct())
                .map(|rem_pct| {
                    let sev = snapshot.budget.classify_quota_remaining(rem_pct);
                    let c = match sev {
                        AlertSeverity::Critical => theme.danger,
                        AlertSeverity::Warn => theme.warn,
                        AlertSeverity::Normal => theme.sage,
                    };
                    (format!("Q {:.0}%", rem_pct), c)
                });
            // Only show the official quota segment when `/statusline` reports active buckets;
            // fall back to a quiet `Q --` placeholder only if it is the sole enabled pill item.
            let maybe_seg = official_seg.or_else(|| {
                let only_quota_enabled = (items & !PILL_ITEM_QUOTA) == 0 && !has_sys;
                only_quota_enabled.then(|| ("Q --".to_string(), theme.muted))
            });
            if let Some((text, color)) = maybe_seg {
                let width = factories.measure_text_width(&text, &fonts.pill_tok).max(28.0) + 2.0;
                segs.push(PillTokenSeg { text, width, color });
            }
        }
        if (items & PILL_ITEM_NON_CACHED) != 0 {
            let text = format!("Δ{}", fmt_compact(today_c.non_cached_tokens()));
            let width = factories.measure_text_width(&text, &fonts.pill_tok).max(20.0) + 2.0;
            let color = if segs.is_empty() { theme.text } else { theme.sub };
            segs.push(PillTokenSeg { text, width, color });
        }
        if (items & PILL_ITEM_CACHE_HIT) != 0 {
            let text = if today_c.total_input() > 0 {
                format!("{:.1}%", today_c.cache_hit_rate())
            } else {
                "--%".to_string()
            };
            let width = factories.measure_text_width(&text, &fonts.pill_tok).max(26.0) + 2.0;
            segs.push(PillTokenSeg {
                text,
                width,
                color: pill_hit_color(theme, &today_c),
            });
        }
        if (items & PILL_ITEM_ACTIVE_CTX) != 0 {
            let ctx = snapshot.sessions.first().map(|s| s.latest_context()).unwrap_or(0);
            ctx_warn = ctx >= 400_000;
            let text = format!("C {}", fmt_compact(ctx));
            let width = factories.measure_text_width(&text, &fonts.pill_tok).max(24.0) + 2.0;
            let color = if ctx_warn {
                theme.warn
            } else if segs.is_empty() {
                theme.text
            } else {
                theme.sub
            };
            segs.push(PillTokenSeg { text, width, color });
        }

        let mut content_w = 23.0; // Left dot zone
        for (i, seg) in segs.iter().enumerate() {
            if i > 0 {
                content_w += 12.0; // Subtle dot separator
            }
            content_w += seg.width;
        }
        if !segs.is_empty() && has_sys {
            content_w += 14.0; // Hairline vertical divider
        }
        if show_cpu {
            content_w += 63.0; // 28px sparkline + 5px gap + 30px %
        }
        if show_cpu && show_ram {
            content_w += 12.0; // Subtle dot separator
        }
        if show_ram {
            content_w += 47.0; // 13px ring + 4px gap + 30px %
        }
        content_w += 12.0; // Right padding

        let pw = content_w.max(68.0);
        let ph = PILL_H_STD;
        let dip_w = pw + SHADOW_PAD * 2.0;
        let dip_h = ph + SHADOW_PAD * 2.0;
        let px_w = (dip_w * scale).round() as i32;
        let px_h = (dip_h * scale).round() as i32;

        let need_new = match &self.surface {
            Some(s) => s.size_px() != (px_w, px_h) || (s.dpi() - dpi).abs() > 0.5,
            None => true,
        };
        if need_new {
            self.surface = Some(LayeredSurface::new(factories, px_w, px_h, dpi)?);
        }

        // Cache monitor placement so 1-second telemetry ticks never re-enumerate display monitors.
        let shadow_px = (SHADOW_PAD * scale).round() as i32;
        let (win_x, win_y) = if let Some(icon_rect) = flyout_anchor {
            compute_tray_flyout_pos(icon_rect, px_w, px_h, shadow_px, scale)
        } else {
            let key = (monitor_index, custom_pos, px_w, px_h, dpi.round() as i32);
            if self.pos_cache_key == Some(key) {
                self.cached_pos
            } else {
                let computed = if let Some((cx, cy)) = custom_pos {
                    if monitor_index_for_point(cx + px_w / 2, cy + px_h / 2).is_some() {
                        // Always clamp visible capsule inside that monitor's work area (never behind taskbar!)
                        clamp_pos_to_work_area(cx, cy, px_w, px_h, shadow_px)
                    } else {
                        let work = get_work_area(monitor_index);
                        let margin_px = (12.0 * scale).round() as i32;
                        (
                            (work.right - px_w + shadow_px - margin_px).max(work.left),
                            (work.bottom - px_h + shadow_px - margin_px).max(work.top),
                        )
                    }
                } else {
                    let work = get_work_area(monitor_index);
                    let margin_px = (12.0 * scale).round() as i32;
                    (
                        (work.right - px_w + shadow_px - margin_px).max(work.left),
                        (work.bottom - px_h + shadow_px - margin_px).max(work.top),
                    )
                };
                self.pos_cache_key = Some(key);
                self.cached_pos = computed;
                computed
            }
        };

        if let Some(surface) = &self.surface {
            surface.render_and_present(hwnd, win_x, win_y, |rt| {
                draw_pill_capsule(
                    factories,
                    rt,
                    fonts,
                    theme,
                    dip_w,
                    dip_h,
                    hovered,
                    dash_open,
                    recent_activity,
                    ctx_warn,
                    max_severity,
                    &segs,
                    show_cpu,
                    show_ram,
                    telemetry,
                );
            })?;
        }
        Ok((win_x, win_y))
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_pill_capsule(
    factories: &D2DFactories,
    rt: &ID2D1RenderTarget,
    fonts: &Fonts,
    theme: &Theme,
    dip_w: f32,
    dip_h: f32,
    hovered: bool,
    dash_open: bool,
    recent_activity: bool,
    ctx_warn: bool,
    alert_severity: AlertSeverity,
    segs: &[PillTokenSeg],
    show_cpu: bool,
    show_ram: bool,
    telemetry: &SystemTelemetry,
) {
    let pw = dip_w - SHADOW_PAD * 2.0;
    let ph = dip_h - SHADOW_PAD * 2.0;
    let x = SHADOW_PAD;
    let y = SHADOW_PAD;
    let radius = ph * 0.5;
    let pill_r = rect_f(x, y, pw, ph);

    // Feathered soft ambient shadow
    for (spread, dy, a) in [
        (5.0, 3.0, 0.08),
        (3.0, 2.0, 0.13),
        (1.5, 1.0, 0.18),
    ] {
        fill_round_rect(
            rt,
            rect_f(x - spread, y - spread + dy, pw + spread * 2.0, ph + spread * 2.0),
            radius + spread,
            rgba(0, 0, 0, a * theme.shadow_alpha),
        );
    }

    // Smooth neutral obsidian / warm paper capsule surface
    let (bg_t, bg_b) = if hovered {
        (theme.pill_bg_hover_top, theme.pill_bg_hover_bot)
    } else {
        (theme.pill_bg_top, theme.pill_bg_bot)
    };
    if let Some(grad) = linear_v_brush(rt, y, y + ph, bg_t, bg_b) {
        let rr = windows::Win32::Graphics::Direct2D::D2D1_ROUNDED_RECT {
            rect: pill_r,
            radiusX: radius,
            radiusY: radius,
        };
        unsafe { rt.FillRoundedRectangle(&rr, &grad) };
    } else {
        fill_round_rect(rt, pill_r, radius, bg_t);
    }

    // Ultra-crisp hairline rim (subtly tinted when hovered or dashboard is open)
    let rim = if hovered || dash_open { theme.pill_rim_hover } else { theme.pill_rim };
    stroke_round_rect(rt, rect_f(x + 0.5, y + 0.5, pw - 1.0, ph - 1.0), radius - 0.5, rim, 0.9);

    let cy = y + ph * 0.5;
    let dot_x = x + 13.0;
    let dot_col = if alert_severity == AlertSeverity::Critical {
        theme.danger
    } else if alert_severity == AlertSeverity::Warn || ctx_warn {
        theme.warn
    } else if recent_activity {
        theme.accent
    } else {
        theme.sage
    };
    if alert_severity != AlertSeverity::Normal || recent_activity || ctx_warn || hovered || dash_open {
        fill_circle(rt, dot_x, cy, 5.0, with_alpha(dot_col, 0.24));
    }
    fill_circle(rt, dot_x, cy, 2.6, dot_col);

    let mut cur_x = x + 23.0;

    // Composable Token segments (Total / Non-Cached Δ / Cache Hit% / Active Ctx)
    for (i, seg) in segs.iter().enumerate() {
        if i > 0 {
            fill_circle(rt, cur_x + 6.0, cy, 1.2, theme.muted);
            cur_x += 12.0;
        }
        draw_text(rt, &seg.text, &fonts.pill_tok, rect_f(cur_x, y, seg.width + 2.0, ph), seg.color);
        cur_x += seg.width;
    }

    let has_sys = show_cpu || show_ram;
    if !segs.is_empty() && has_sys {
        let div_x = cur_x + 7.0;
        draw_line(rt, div_x, y + 8.0, div_x, y + ph - 8.0, theme.border, 1.0);
        cur_x += 14.0;
    }

    let tel = telemetry.latest;

    // Live 16-sample micro CPU sparkline + %
    if show_cpu {
        let wave_l = cur_x;
        let wave_w = 28.0;
        let wave_t = y + 8.0;
        let wave_b = y + ph - 8.0;

        draw_line(rt, wave_l, wave_b, wave_l + wave_w, wave_b, theme.grid, 0.8);
        let hist_len = telemetry.history.len();
        if hist_len >= 2 {
            let slice: Vec<_> = telemetry.history.iter().rev().take(16).collect();
            let n = slice.len();
            let pts: Vec<(f32, f32)> = slice
                .iter()
                .rev()
                .enumerate()
                .map(|(i, s)| {
                    let px = wave_l + (i as f32 / (n - 1).max(1) as f32) * wave_w;
                    let py = wave_b - (s.cpu_pct.clamp(0.0, 100.0) / 100.0) * (wave_b - wave_t);
                    (px, py)
                })
                .collect();
            draw_area_series(factories, rt, &pts, wave_b, wave_t, theme.accent, 1.3, 0.22);
        } else {
            let py = wave_b - (tel.cpu_pct.clamp(0.0, 100.0) / 100.0) * (wave_b - wave_t);
            draw_line(rt, wave_l, py, wave_l + wave_w, py, theme.accent, 1.3);
        }

        let cpu_col = load_color(theme, tel.cpu_pct, 60.0, 85.0);
        draw_text(
            rt,
            &format!("{:>2.0}%", tel.cpu_pct.round()),
            &fonts.pill_num,
            rect_f(wave_l + wave_w + 5.0, y, 30.0, ph),
            cpu_col,
        );
        cur_x += 63.0;
    }

    if show_cpu && show_ram {
        fill_circle(rt, cur_x + 6.0, cy, 1.2, theme.muted);
        cur_x += 12.0;
    }

    // Crisp radial RAM ring gauge + %
    if show_ram {
        let ring_cx = cur_x + 6.5;
        let ram_col = if tel.mem_pct >= 85.0 {
            theme.danger
        } else if tel.mem_pct >= 72.0 {
            theme.warn
        } else {
            theme.ram
        };
        draw_ring_gauge(
            factories,
            rt,
            ring_cx,
            cy,
            5.5,
            2.0,
            tel.mem_pct,
            theme.border,
            ram_col,
        );
        draw_text(
            rt,
            &format!("{:>2.0}%", tel.mem_pct.round()),
            &fonts.pill_num,
            rect_f(cur_x + 17.0, y, 30.0, ph),
            load_color(theme, tel.mem_pct, 75.0, 88.0),
        );
    }
    let _ = &fonts.pill_micro;
}

#[allow(clippy::too_many_arguments)]
pub fn show_context_menu(
    hwnd: HWND,
    dash_open: bool,
    pill_visible: bool,
    tray_hover_mode: bool,
    light_theme: bool,
    topmost: bool,
    human: bool,
    privacy_mode: bool,
    notifications_enabled: bool,
    hide_on_fullscreen: bool,
    autostart_enabled: bool,
    current_monitor: usize,
    pill_items: u32,
    quick_sessions: &[QuickSessionMenuItem],
) -> Option<u32> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            CMD_OPEN_DETAIL as usize,
            if dash_open { w!("关闭详细统计") } else { w!("打开详细统计") },
        );

        let mut quick_menu = None;
        if let Ok(qm) = CreatePopupMenu() {
            if privacy_mode {
                let _ = AppendMenuW(qm, MF_STRING | MF_GRAYED, 0, w!("脱敏模式已隐藏"));
            } else if quick_sessions.is_empty() {
                let _ = AppendMenuW(qm, MF_STRING | MF_GRAYED, 0, w!("暂无可继续的 CLI 会话"));
            } else {
                for (idx, item) in quick_sessions.iter().take(5).enumerate() {
                    let wide = to_wide_null(&item.label);
                    let _ = AppendMenuW(
                        qm,
                        MF_STRING,
                        (CMD_QUICK_RESUME_BASE as usize) + idx,
                        wide_pcwstr(&wide),
                    );
                }
            }
            let _ = AppendMenuW(qm, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(qm, MF_STRING, CMD_OPEN_SESSIONS_TAB as usize, w!("打开会话管理…"));
            let _ = AppendMenuW(menu, MF_POPUP, qm.0 as usize, w!("最近会话"));
            quick_menu = Some(qm);
        }
        let _ = AppendMenuW(
            menu,
            MF_STRING | if tray_hover_mode { MF_CHECKED } else { MF_UNCHECKED },
            CMD_TOGGLE_TRAY_HOVER as usize,
            w!("托盘悬停模式 (悬停图标显示胶囊)"),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            CMD_TOGGLE_PILL_VISIBLE as usize,
            if tray_hover_mode {
                w!("恢复常驻悬浮胶囊")
            } else if pill_visible {
                w!("隐藏悬浮胶囊 (托盘保活)")
            } else {
                w!("显示悬浮胶囊")
            },
        );
        let _ = AppendMenuW(menu, MF_STRING, CMD_REFRESH as usize, w!("立即刷新"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);

        let mut items_menu = None;
        if let Ok(im) = CreatePopupMenu() {
            for (bit_idx, bit, lbl) in [
                (0usize, PILL_ITEM_TOTAL, w!("今日总 Token")),
                (6usize, PILL_ITEM_COST, w!("今日估算费用 ($)")),
                (8usize, PILL_ITEM_QUOTA, w!("官方配额 (Q · AGY)")),
                (1usize, PILL_ITEM_NON_CACHED, w!("今日非缓存增量 (Δ)")),
                (2usize, PILL_ITEM_CACHE_HIT, w!("今日缓存命中率 (%)")),
                (3usize, PILL_ITEM_ACTIVE_CTX, w!("活跃会话上下文 (Ctx)")),
            ] {
                let flag = MF_STRING | if (pill_items & bit) != 0 { MF_CHECKED } else { MF_UNCHECKED };
                let _ = AppendMenuW(im, flag, (CMD_PILL_ITEM_BASE as usize) + bit_idx, lbl);
            }
            let _ = AppendMenuW(im, MF_SEPARATOR, 0, None);
            for (bit_idx, bit, lbl) in [
                (4usize, PILL_ITEM_CPU, w!("CPU 波形")),
                (5usize, PILL_ITEM_RAM, w!("内存环")),
            ] {
                let flag = MF_STRING | if (pill_items & bit) != 0 { MF_CHECKED } else { MF_UNCHECKED };
                let _ = AppendMenuW(im, flag, (CMD_PILL_ITEM_BASE as usize) + bit_idx, lbl);
            }
            let _ = AppendMenuW(im, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(im, MF_STRING, CMD_PILL_ITEMS_RESET as usize, w!("恢复默认显示"));
            let _ = AppendMenuW(menu, MF_POPUP, im.0 as usize, w!("胶囊显示内容"));
            items_menu = Some(im);
        }

        let _ = AppendMenuW(
            menu,
            MF_STRING | if light_theme { MF_CHECKED } else { MF_UNCHECKED },
            CMD_TOGGLE_LIGHT as usize,
            w!("浅色纸感主题 (Warm Paper)"),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING | if topmost { MF_CHECKED } else { MF_UNCHECKED },
            CMD_TOGGLE_TOPMOST as usize,
            w!("始终置顶"),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING | if hide_on_fullscreen { MF_CHECKED } else { MF_UNCHECKED },
            CMD_TOGGLE_HIDE_FULLSCREEN as usize,
            w!("全屏应用自动隐藏胶囊"),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING | if notifications_enabled { MF_CHECKED } else { MF_UNCHECKED },
            CMD_TOGGLE_NOTIFICATIONS as usize,
            w!("预算与配额系统通知"),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING | if human { MF_CHECKED } else { MF_UNCHECKED },
            CMD_TOGGLE_HUMAN as usize,
            w!("简写数字 (K/M)"),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING | if privacy_mode { MF_CHECKED } else { MF_UNCHECKED },
            CMD_TOGGLE_PRIVACY as usize,
            w!("脱敏模式 (隐藏隐私)"),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING | if autostart_enabled { MF_CHECKED } else { MF_UNCHECKED },
            CMD_TOGGLE_AUTOSTART as usize,
            w!("开机自动启动"),
        );

        let monitors = get_monitors();
        let mut sub_menu = None;
        if monitors.len() > 1 {
            if let Ok(sub) = CreatePopupMenu() {
                for (idx, m) in monitors.iter().enumerate() {
                    let wide = to_wide_null(&m.label);
                    let flag = MF_STRING | if idx == current_monitor { MF_CHECKED } else { MF_UNCHECKED };
                    let _ = AppendMenuW(sub, flag, (CMD_MONITOR_BASE as usize) + idx, wide_pcwstr(&wide));
                }
                let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("切换显示屏幕"));
                sub_menu = Some(sub);
            }
        }

        let _ = AppendMenuW(menu, MF_STRING, CMD_REDOCK as usize, w!("重新贴靠右下角"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT as usize, w!("退出"));

        let mut pt = windows::Win32::Foundation::POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_LEFTALIGN | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            0,
            hwnd,
            None,
        );
        if let Some(sub) = sub_menu {
            let _ = DestroyMenu(sub);
        }
        if let Some(im) = items_menu {
            let _ = DestroyMenu(im);
        }
        if let Some(qm) = quick_menu {
            let _ = DestroyMenu(qm);
        }
        let _ = DestroyMenu(menu);
        if cmd.0 != 0 {
            Some(cmd.0 as u32)
        } else {
            None
        }
    }
}
