//! Tactile, instrument-grade bottom-right floating capsule (iStat Menus / Teenage Engineering inspired).
//!
//! Features:
//!   - True per-pixel alpha capsule with soft feathered ambient shadow & 0.75px neutral hairline rim.
//!   - Live 16-sample micro CPU sparkline waveform + anti-aliased radial RAM ring gauge.
//!   - Restrained Obsidian Charcoal or Warm Paper theme (zero AI neon cyan/purple/mint!).
//!   - No redundant hover popup: hovering smoothly illuminates the rim; left-click opens dashboard.

use chrono::Local;
use windows::core::{w, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::ID2D1RenderTarget;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, SetForegroundWindow, TrackPopupMenu,
    MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, TPM_LEFTALIGN, TPM_RETURNCMD,
    TPM_RIGHTBUTTON,
};

use crate::d2d::{
    draw_area_series, draw_line, draw_ring_gauge, draw_text, fill_circle, fill_round_rect,
    linear_v_brush, load_color, rect_f, rgba, stroke_round_rect, theme_for, with_alpha,
    D2DFactories, Fonts, LayeredSurface, Theme,
};
use crate::stats::{fmt_compact, ScanSnapshot, TokenCounts};
use crate::telemetry::{
    get_monitors, get_work_area, monitor_index_for_point, to_wide_null, wide_pcwstr, SystemTelemetry,
};

pub const CMD_OPEN_DETAIL: u32 = 1001;
pub const CMD_REFRESH: u32 = 1002;
pub const CMD_TOGGLE_MINI: u32 = 1003;
pub const CMD_TOGGLE_LIGHT: u32 = 1004;
pub const CMD_TOGGLE_TOPMOST: u32 = 1005;
pub const CMD_TOGGLE_HUMAN: u32 = 1006;
pub const CMD_REDOCK: u32 = 1007;
pub const CMD_QUIT: u32 = 1008;
pub const CMD_MONITOR_BASE: u32 = 1100;

const SHADOW_PAD: f32 = 10.0;
const PILL_H_STD: f32 = 30.0;
const PILL_W_STD: f32 = 252.0;
const PILL_H_MINI: f32 = 28.0;
const PILL_W_MINI: f32 = 92.0;

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

    pub fn desired_dip_size(mini: bool) -> (f32, f32) {
        let (pw, ph) = if mini {
            (PILL_W_MINI, PILL_H_MINI)
        } else {
            (PILL_W_STD, PILL_H_STD)
        };
        (pw + SHADOW_PAD * 2.0, ph + SHADOW_PAD * 2.0)
    }

    pub fn size_px(&self) -> (i32, i32) {
        self.surface.as_ref().map(|s| s.size_px()).unwrap_or((1, 1))
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
        mini: bool,
        light_theme: bool,
        hovered: bool,
        dash_open: bool,
        recent_activity: bool,
        snapshot: &ScanSnapshot,
        telemetry: &SystemTelemetry,
    ) -> Result<(i32, i32)> {
        let scale = dpi / 96.0;
        let (dip_w, dip_h) = Self::desired_dip_size(mini);
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
        let key = (monitor_index, custom_pos, px_w, px_h, dpi.round() as i32);
        let (win_x, win_y) = if self.pos_cache_key == Some(key) {
            self.cached_pos
        } else {
            let computed = if let Some((cx, cy)) = custom_pos {
                if monitor_index_for_point(cx + px_w / 2, cy + px_h / 2).is_some() {
                    (cx, cy)
                } else {
                    let work = get_work_area(monitor_index);
                    let margin_px = (12.0 * scale).round() as i32;
                    let shadow_px = (SHADOW_PAD * scale).round() as i32;
                    (
                        (work.right - px_w + shadow_px - margin_px).max(work.left),
                        (work.bottom - px_h + shadow_px - margin_px).max(work.top),
                    )
                }
            } else {
                let work = get_work_area(monitor_index);
                let margin_px = (12.0 * scale).round() as i32;
                let shadow_px = (SHADOW_PAD * scale).round() as i32;
                (
                    (work.right - px_w + shadow_px - margin_px).max(work.left),
                    (work.bottom - px_h + shadow_px - margin_px).max(work.top),
                )
            };
            self.pos_cache_key = Some(key);
            self.cached_pos = computed;
            computed
        };

        let today_key = Local::now().date_naive().format("%Y-%m-%d").to_string();
        let today_c: TokenCounts = snapshot
            .agg
            .by_day
            .get(&today_key)
            .copied()
            .unwrap_or_default();
        // Always stay consistent with Dashboard's "今日" scope (show 0 on a fresh day)
        let tok_val = fmt_compact(today_c.total_tokens());

        let theme = theme_for(light_theme);
        if let Some(surface) = &self.surface {
            surface.render_and_present(hwnd, win_x, win_y, |rt| {
                draw_pill_capsule(
                    factories,
                    rt,
                    fonts,
                    theme,
                    dip_w,
                    dip_h,
                    mini,
                    hovered,
                    dash_open,
                    recent_activity,
                    &tok_val,
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
    mini: bool,
    hovered: bool,
    dash_open: bool,
    recent_activity: bool,
    tok_val: &str,
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
    let dot_col = if recent_activity { theme.accent } else { theme.sage };
    if recent_activity || hovered || dash_open {
        fill_circle(rt, dot_x, cy, 5.0, with_alpha(dot_col, 0.22));
    }
    fill_circle(rt, dot_x, cy, 2.6, dot_col);

    if mini {
        draw_text(rt, tok_val, &fonts.pill_tok, rect_f(x + 22.0, y, pw - 30.0, ph), theme.text);
        return;
    }

    // Left: Today's Token figure (crisp primary neutral ink)
    draw_text(rt, tok_val, &fonts.pill_tok, rect_f(x + 22.0, y, 62.0, ph), theme.text);

    // Whisper-quiet divider
    let div_x = x + 86.0;
    draw_line(rt, div_x, y + 8.0, div_x, y + ph - 8.0, theme.border, 1.0);

    // Center: Live 16-sample micro CPU sparkline + % (like iStat Menus)
    let wave_l = div_x + 9.0;
    let wave_w = 28.0;
    let wave_t = y + 8.0;
    let wave_b = y + ph - 8.0;
    let tel = telemetry.latest;

    // Subtle baseline for waveform
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

    // Subtle dot separator
    let mid_x = wave_l + wave_w + 38.0;
    fill_circle(rt, mid_x, cy, 1.2, theme.muted);

    // Right: Crisp radial RAM ring gauge + %
    let ring_cx = mid_x + 13.0;
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
        rect_f(ring_cx + 10.0, y, 32.0, ph),
        load_color(theme, tel.mem_pct, 75.0, 88.0),
    );
    let _ = &fonts.pill_micro;
}

pub fn show_context_menu(
    hwnd: HWND,
    dash_open: bool,
    mini: bool,
    light_theme: bool,
    topmost: bool,
    human: bool,
    current_monitor: usize,
) -> Option<u32> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            CMD_OPEN_DETAIL as usize,
            if dash_open { w!("关闭详细统计") } else { w!("打开详细统计") },
        );
        let _ = AppendMenuW(menu, MF_STRING, CMD_REFRESH as usize, w!("立即刷新"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(
            menu,
            MF_STRING | if light_theme { MF_CHECKED } else { MF_UNCHECKED },
            CMD_TOGGLE_LIGHT as usize,
            w!("浅色纸感主题 (Warm Paper)"),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING | if mini { MF_CHECKED } else { MF_UNCHECKED },
            CMD_TOGGLE_MINI as usize,
            w!("极简角标模式"),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING | if topmost { MF_CHECKED } else { MF_UNCHECKED },
            CMD_TOGGLE_TOPMOST as usize,
            w!("始终置顶"),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING | if human { MF_CHECKED } else { MF_UNCHECKED },
            CMD_TOGGLE_HUMAN as usize,
            w!("简写数字 (K/M)"),
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
        let _ = DestroyMenu(menu);
        if cmd.0 != 0 {
            Some(cmd.0 as u32)
        } else {
            None
        }
    }
}
