//! Tactile, instrument-grade bottom-right floating capsule (iStat Menus / Teenage Engineering inspired).
//!
//! Features:
//!   - True per-pixel alpha capsule with soft feathered ambient shadow & 0.75px neutral hairline rim.
//!   - User-composable display segments (Today Total / Non-Cached Δ / Cache Hit% / Active Ctx / CPU / RAM)
//!     with exact DirectWrite text-metric dynamic sizing so every combination stays optically balanced.
//!   - Restrained Obsidian Charcoal or Warm Paper theme (zero AI neon cyan/purple/mint!).
//!   - No redundant hover popup: hovering smoothly illuminates the rim; left-click opens dashboard.

use chrono::Utc;
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
use crate::launcher::{TerminalAvailability, TerminalPreset};
use crate::pricing::fmt_cost_compact;
use crate::quota::{AlertSeverity, QuotaProvenance};
use crate::stats::{fmt_compact, ScanSnapshot, TokenCounts};
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
pub const CMD_TERMINAL_PRESET_BASE: u32 = 1400; // 1400..1407 -> TerminalPreset::ALL
pub const CMD_TERMINAL_TOGGLE_NEW_WIN: u32 = 1410;
pub const CMD_TERMINAL_TOGGLE_KEEP_OPEN: u32 = 1411;
pub const CMD_TERMINAL_OPEN_CONFIG: u32 = 1412;

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
    color_tag: u8,
}

const SEG_COL_TEXT: u8 = 0;
const SEG_COL_SUB: u8 = 1;
const SEG_COL_OUTPUT: u8 = 2;
const SEG_COL_MUTED: u8 = 3;
const SEG_COL_SAGE: u8 = 4;
const SEG_COL_WARN: u8 = 5;
const SEG_COL_DANGER: u8 = 6;

fn pill_hit_color(theme: &Theme, c: &TokenCounts) -> (D2D1_COLOR_F, u8) {
    if c.total_input() == 0 {
        (theme.muted, SEG_COL_MUTED)
    } else {
        let r = c.cache_hit_rate();
        if r >= 85.0 {
            (theme.sage, SEG_COL_SAGE)
        } else if r >= 60.0 {
            (theme.warn, SEG_COL_WARN)
        } else {
            (theme.danger, SEG_COL_DANGER)
        }
    }
}

fn severity_to_u8(s: AlertSeverity) -> u8 {
    match s {
        AlertSeverity::Normal => 0,
        AlertSeverity::Warn => 1,
        AlertSeverity::Critical => 2,
    }
}

fn quantize_spark_y(pct: f32) -> u8 {
    // Sparkline plot height is 14 DIP (PILL_H_STD - 16); 0.5 DIP buckets -> 0..=28.
    (pct.clamp(0.0, 100.0) * 0.28).round() as u8
}

fn cpu_color_band(pct: f32) -> u8 {
    if pct >= 85.0 {
        2
    } else if pct >= 60.0 {
        1
    } else {
        0
    }
}

fn ram_color_band(pct: f32) -> u8 {
    let ring = if pct >= 85.0 {
        2
    } else if pct >= 72.0 {
        1
    } else {
        0
    };
    let text = if pct >= 88.0 {
        2
    } else if pct >= 75.0 {
        1
    } else {
        0
    };
    (ring << 2) | text
}

fn fmt_pct_buf(pct_round: u8, buf: &mut [u8; 8]) -> &str {
    let v = pct_round.min(100);
    let len = if v >= 100 {
        buf[0] = b'1';
        buf[1] = b'0';
        buf[2] = b'0';
        buf[3] = b'%';
        4
    } else if v >= 10 {
        buf[0] = b'0' + (v / 10);
        buf[1] = b'0' + (v % 10);
        buf[2] = b'%';
        3
    } else {
        buf[0] = b' ';
        buf[1] = b'0' + v;
        buf[2] = b'%';
        3
    };
    // Pure ASCII bytes.
    unsafe { std::str::from_utf8_unchecked(&buf[..len]) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PillSegCacheKey {
    snapshot_gen: u64,
    pill_items: u32,
    light_theme: bool,
    dpi_i: i32,
    quota_minute_bucket: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PillRenderKey {
    dpi_i: i32,
    win_pos: (i32, i32),
    px_size: (i32, i32),
    light_theme: bool,
    hovered: bool,
    dash_open: bool,
    recent_activity: bool,
    ctx_warn: bool,
    alert_severity: u8,
    pill_items: u32,
    segs_gen: u64,
    cpu_pct_round: Option<u8>,
    cpu_band: u8,
    cpu_spark_len: u8,
    cpu_spark_quant: [u8; 16],
    ram_pct_round: Option<u8>,
    ram_band: u8,
}

fn segs_visually_equal(a: &[PillTokenSeg], b: &[PillTokenSeg]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.text == y.text && x.color_tag == y.color_tag)
}

pub struct PillRenderer {
    surface: Option<LayeredSurface>,
    pos_cache_key: Option<(usize, Option<(i32, i32)>, i32, i32, i32)>,
    cached_pos: (i32, i32),
    flyout_pos_cache: Option<(RECT, i32, i32, i32, (i32, i32))>,
    seg_width_cache: Vec<((String, i32), f32)>,
    cached_seg_key: Option<PillSegCacheKey>,
    cached_today_key: String,
    cached_segs: Vec<PillTokenSeg>,
    cached_ctx_warn: bool,
    cached_segs_w: f32,
    cached_segs_gen: u64,
    last_render_key: Option<PillRenderKey>,
}

impl PillRenderer {
    pub fn new() -> Self {
        Self {
            surface: None,
            pos_cache_key: None,
            cached_pos: (0, 0),
            flyout_pos_cache: None,
            seg_width_cache: Vec::new(),
            cached_seg_key: None,
            cached_today_key: String::new(),
            cached_segs: Vec::new(),
            cached_ctx_warn: false,
            cached_segs_w: 0.0,
            cached_segs_gen: 0,
            last_render_key: None,
        }
    }

    pub fn invalidate_position(&mut self) {
        self.pos_cache_key = None;
        self.flyout_pos_cache = None;
    }

    pub fn invalidate_render(&mut self) {
        self.last_render_key = None;
        self.cached_seg_key = None;
    }

    /// Releases the software Direct2D DC render target + backing DIB + text-width/segment caches
    /// when the pill is hidden or idle in tray-hover mode. Returns `true` if a surface was held.
    pub fn release_surface(&mut self) -> bool {
        let had = self.surface.take().is_some();
        self.last_render_key = None;
        self.cached_seg_key = None;
        self.cached_today_key.clear();
        self.cached_today_key.shrink_to_fit();
        self.cached_segs.clear();
        self.cached_segs.shrink_to_fit();
        self.cached_ctx_warn = false;
        self.cached_segs_w = 0.0;
        self.seg_width_cache.clear();
        self.seg_width_cache.shrink_to_fit();
        had
    }

    pub fn size_px(&self) -> (i32, i32) {
        self.surface.as_ref().map(|s| s.size_px()).unwrap_or((1, 1))
    }

    pub fn shadow_px(&self) -> i32 {
        let scale = self.surface.as_ref().map(|s| s.dpi() / 96.0).unwrap_or(1.0);
        (SHADOW_PAD * scale).round() as i32
    }

    fn measure_seg_width_with(
        cache: &mut Vec<((String, i32), f32)>,
        text: &str,
        dpi_i: i32,
        min_w: f32,
        measure_fn: &mut impl FnMut(&str) -> f32,
    ) -> f32 {
        if let Some((_, cached)) = cache
            .iter()
            .find(|((k, d), _)| *d == dpi_i && k.as_str() == text)
        {
            return cached.max(min_w) + 2.0;
        }
        let measured = measure_fn(text);
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.push(((text.to_string(), dpi_i), measured));
        measured.max(min_w) + 2.0
    }

    /// Refreshes cached formatted token segments only when `snapshot_gen`, `pill_items`,
    /// `light_theme`, `dpi_i`, `today_key`, or the quota staleness minute changes.
    /// Returns `true` if segments were rebuilt (cache miss) and `false` on the zero-alloc fast path.
    #[allow(clippy::too_many_arguments)]
    fn ensure_token_segs_with(
        &mut self,
        snapshot_gen: u64,
        items: u32,
        has_sys: bool,
        light_theme: bool,
        dpi_i: i32,
        today_key: &str,
        snapshot: &ScanSnapshot,
        mut measure_fn: impl FnMut(&str) -> f32,
    ) -> bool {
        let show_quota = (items & PILL_ITEM_QUOTA) != 0;
        let now_utc = if show_quota && snapshot.statusline.is_some() {
            Some(Utc::now())
        } else {
            None
        };
        let quota_minute_bucket = now_utc.map(|t| t.timestamp() / 60).unwrap_or(0);
        let seg_key = PillSegCacheKey {
            snapshot_gen,
            pill_items: items,
            light_theme,
            dpi_i,
            quota_minute_bucket,
        };
        if self.cached_seg_key == Some(seg_key) && self.cached_today_key == today_key {
            return false;
        }

        let theme = theme_for(light_theme);
        let today_c: TokenCounts = snapshot
            .agg
            .by_day
            .get(today_key)
            .copied()
            .unwrap_or_default();

        let mut segs: Vec<PillTokenSeg> = Vec::with_capacity(6);
        let mut ctx_warn = false;

        if (items & PILL_ITEM_TOTAL) != 0 {
            let text = fmt_compact(today_c.total_tokens());
            let width =
                Self::measure_seg_width_with(&mut self.seg_width_cache, &text, dpi_i, 16.0, &mut measure_fn);
            segs.push(PillTokenSeg {
                text,
                width,
                color: theme.text,
                color_tag: SEG_COL_TEXT,
            });
        }
        if (items & PILL_ITEM_COST) != 0 {
            let text = fmt_cost_compact(&today_c.cost, today_c.calls);
            let width =
                Self::measure_seg_width_with(&mut self.seg_width_cache, &text, dpi_i, 24.0, &mut measure_fn);
            let day_sev = if snapshot.budget.daily_usd > 0.0 {
                snapshot
                    .budget
                    .classify_usage_pct((today_c.cost.total_usd() / snapshot.budget.daily_usd) * 100.0)
            } else {
                AlertSeverity::Normal
            };
            let (color, color_tag) = match day_sev {
                AlertSeverity::Critical => (theme.danger, SEG_COL_DANGER),
                AlertSeverity::Warn => (theme.warn, SEG_COL_WARN),
                AlertSeverity::Normal => {
                    if segs.is_empty() {
                        (theme.text, SEG_COL_TEXT)
                    } else {
                        (theme.output, SEG_COL_OUTPUT)
                    }
                }
            };
            segs.push(PillTokenSeg {
                text,
                width,
                color,
                color_tag,
            });
        }
        if show_quota {
            let official_seg = now_utc.and_then(|now_t| {
                snapshot
                    .statusline
                    .as_ref()
                    .filter(|s| s.provenance(now_t) != QuotaProvenance::LocalEstimate)
                    .and_then(|sl| sl.tightest_bucket())
                    .and_then(|b| b.remaining_pct())
                    .map(|rem_pct| {
                        let sev = snapshot.budget.classify_quota_remaining(rem_pct);
                        let (c, tag) = match sev {
                            AlertSeverity::Critical => (theme.danger, SEG_COL_DANGER),
                            AlertSeverity::Warn => (theme.warn, SEG_COL_WARN),
                            AlertSeverity::Normal => (theme.sage, SEG_COL_SAGE),
                        };
                        (format!("Q {:.0}%", rem_pct), c, tag)
                    })
            });
            // Only show the official quota segment when `/statusline` reports active buckets;
            // fall back to a quiet `Q --` placeholder only if it is the sole enabled pill item.
            let maybe_seg = official_seg.or_else(|| {
                let only_quota_enabled = (items & !PILL_ITEM_QUOTA) == 0 && !has_sys;
                only_quota_enabled.then(|| ("Q --".to_string(), theme.muted, SEG_COL_MUTED))
            });
            if let Some((text, color, color_tag)) = maybe_seg {
                let width = Self::measure_seg_width_with(
                    &mut self.seg_width_cache,
                    &text,
                    dpi_i,
                    28.0,
                    &mut measure_fn,
                );
                segs.push(PillTokenSeg {
                    text,
                    width,
                    color,
                    color_tag,
                });
            }
        }
        if (items & PILL_ITEM_NON_CACHED) != 0 {
            let text = format!("Δ{}", fmt_compact(today_c.non_cached_tokens()));
            let width =
                Self::measure_seg_width_with(&mut self.seg_width_cache, &text, dpi_i, 20.0, &mut measure_fn);
            let (color, color_tag) = if segs.is_empty() {
                (theme.text, SEG_COL_TEXT)
            } else {
                (theme.sub, SEG_COL_SUB)
            };
            segs.push(PillTokenSeg {
                text,
                width,
                color,
                color_tag,
            });
        }
        if (items & PILL_ITEM_CACHE_HIT) != 0 {
            let text = if today_c.total_input() > 0 {
                format!("{:.1}%", today_c.cache_hit_rate())
            } else {
                "--%".to_string()
            };
            let width =
                Self::measure_seg_width_with(&mut self.seg_width_cache, &text, dpi_i, 26.0, &mut measure_fn);
            let (color, color_tag) = pill_hit_color(theme, &today_c);
            segs.push(PillTokenSeg {
                text,
                width,
                color,
                color_tag,
            });
        }
        if (items & PILL_ITEM_ACTIVE_CTX) != 0 {
            let ctx = snapshot.sessions.first().map(|s| s.latest_context()).unwrap_or(0);
            ctx_warn = ctx >= 400_000;
            let text = format!("C {}", fmt_compact(ctx));
            let width =
                Self::measure_seg_width_with(&mut self.seg_width_cache, &text, dpi_i, 24.0, &mut measure_fn);
            let (color, color_tag) = if ctx_warn {
                (theme.warn, SEG_COL_WARN)
            } else if segs.is_empty() {
                (theme.text, SEG_COL_TEXT)
            } else {
                (theme.sub, SEG_COL_SUB)
            };
            segs.push(PillTokenSeg {
                text,
                width,
                color,
                color_tag,
            });
        }

        let mut segs_w = 0.0_f32;
        for (i, seg) in segs.iter().enumerate() {
            if i > 0 {
                segs_w += 12.0; // Subtle dot separator
            }
            segs_w += seg.width;
        }

        let prev_theme_dpi = self.cached_seg_key.map(|k| (k.light_theme, k.dpi_i));
        let visual_changed = self.cached_seg_key.is_none()
            || prev_theme_dpi != Some((light_theme, dpi_i))
            || self.cached_ctx_warn != ctx_warn
            || !segs_visually_equal(&self.cached_segs, &segs);
        if visual_changed {
            self.cached_segs_gen = self.cached_segs_gen.wrapping_add(1);
        }
        self.cached_segs = segs;
        self.cached_ctx_warn = ctx_warn;
        self.cached_segs_w = segs_w;
        self.cached_seg_key = Some(seg_key);
        if self.cached_today_key != today_key {
            self.cached_today_key.clear();
            self.cached_today_key.push_str(today_key);
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn build_render_key(
        &self,
        dpi_i: i32,
        win_pos: (i32, i32),
        px_size: (i32, i32),
        light_theme: bool,
        hovered: bool,
        dash_open: bool,
        recent_activity: bool,
        max_severity: AlertSeverity,
        items: u32,
        show_cpu: bool,
        show_ram: bool,
        telemetry: &SystemTelemetry,
    ) -> PillRenderKey {
        let tel = telemetry.latest;
        let (cpu_pct_round, cpu_band, cpu_spark_len, cpu_spark_quant) = if show_cpu {
            let mut q = [0u8; 16];
            let hist_len = telemetry.history.len();
            let q_len = if hist_len >= 2 {
                let n = hist_len.min(16);
                let start = hist_len - n;
                for (i, s) in telemetry.history.iter().skip(start).take(n).enumerate() {
                    q[i] = quantize_spark_y(s.cpu_pct);
                }
                n as u8
            } else {
                q[0] = quantize_spark_y(tel.cpu_pct);
                1
            };
            (
                Some(tel.cpu_pct.clamp(0.0, 100.0).round() as u8),
                cpu_color_band(tel.cpu_pct),
                q_len,
                q,
            )
        } else {
            (None, 0, 0, [0u8; 16])
        };
        let (ram_pct_round, ram_band) = if show_ram {
            (
                Some(tel.mem_pct.clamp(0.0, 100.0).round() as u8),
                ram_color_band(tel.mem_pct),
            )
        } else {
            (None, 0)
        };

        PillRenderKey {
            dpi_i,
            win_pos,
            px_size,
            light_theme,
            hovered,
            dash_open,
            recent_activity,
            ctx_warn: self.cached_ctx_warn,
            alert_severity: severity_to_u8(max_severity),
            pill_items: items,
            segs_gen: self.cached_segs_gen,
            cpu_pct_round,
            cpu_band,
            cpu_spark_len,
            cpu_spark_quant,
            ram_pct_round,
            ram_band,
        }
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
        today_key: &str,
        max_severity: AlertSeverity,
        snapshot_gen: u64,
        snapshot: &ScanSnapshot,
        telemetry: &SystemTelemetry,
    ) -> Result<(i32, i32)> {
        let scale = dpi / 96.0;
        let dpi_i = dpi.round() as i32;
        let theme = theme_for(light_theme);
        let items = {
            let m = pill_items & PILL_ITEMS_ALL_MASK;
            if m != 0 { m } else { PILL_ITEMS_DEFAULT }
        };

        let show_cpu = (items & PILL_ITEM_CPU) != 0;
        let show_ram = (items & PILL_ITEM_RAM) != 0;
        let has_sys = show_cpu || show_ram;

        self.ensure_token_segs_with(
            snapshot_gen,
            items,
            has_sys,
            light_theme,
            dpi_i,
            today_key,
            snapshot,
            |text| factories.measure_text_width(text, &fonts.pill_tok),
        );

        let mut content_w = 23.0 + self.cached_segs_w; // Left dot zone + cached token segments
        if !self.cached_segs.is_empty() && has_sys {
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

        // Cache monitor placement (both docked and tray-hover flyout) so 1s ticks never re-enumerate monitors.
        let shadow_px = (SHADOW_PAD * scale).round() as i32;
        let (win_x, win_y) = if let Some(icon_rect) = flyout_anchor {
            if let Some((ir, w, h, d, pos)) = self.flyout_pos_cache {
                if ir == icon_rect && w == px_w && h == px_h && d == dpi_i {
                    pos
                } else {
                    let computed = compute_tray_flyout_pos(icon_rect, px_w, px_h, shadow_px, scale);
                    self.flyout_pos_cache = Some((icon_rect, px_w, px_h, dpi_i, computed));
                    computed
                }
            } else {
                let computed = compute_tray_flyout_pos(icon_rect, px_w, px_h, shadow_px, scale);
                self.flyout_pos_cache = Some((icon_rect, px_w, px_h, dpi_i, computed));
                computed
            }
        } else {
            let key = (monitor_index, custom_pos, px_w, px_h, dpi_i);
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

        // Stack-only Copy dirty-check key: zero heap allocations when unchanged on 1s ticks.
        let render_key = self.build_render_key(
            dpi_i,
            (win_x, win_y),
            (px_w, px_h),
            light_theme,
            hovered,
            dash_open,
            recent_activity,
            max_severity,
            items,
            show_cpu,
            show_ram,
            telemetry,
        );

        if self.surface.is_some() && self.last_render_key == Some(render_key) {
            return Ok((win_x, win_y));
        }

        match &mut self.surface {
            Some(s) => {
                if s.size_px() != (px_w, px_h) || (s.dpi() - dpi).abs() > 0.5 {
                    s.resize(px_w, px_h, dpi)?;
                }
            }
            None => {
                self.surface = Some(LayeredSurface::new(factories, px_w, px_h, dpi)?);
            }
        }

        let segs = &self.cached_segs;
        let ctx_warn = self.cached_ctx_warn;
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
                    segs,
                    show_cpu,
                    show_ram,
                    telemetry,
                );
            })?;
            self.last_render_key = Some(render_key);
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

    // Live 16-sample micro CPU sparkline + % (zero heap allocations)
    if show_cpu {
        let wave_l = cur_x;
        let wave_w = 28.0;
        let wave_t = y + 8.0;
        let wave_b = y + ph - 8.0;

        draw_line(rt, wave_l, wave_b, wave_l + wave_w, wave_b, theme.grid, 0.8);
        let hist_len = telemetry.history.len();
        if hist_len >= 2 {
            let n = hist_len.min(16);
            let start = hist_len - n;
            let mut pts = [(0.0_f32, 0.0_f32); 16];
            let denom = (n - 1).max(1) as f32;
            for (i, s) in telemetry.history.iter().skip(start).take(n).enumerate() {
                let px = wave_l + (i as f32 / denom) * wave_w;
                let py = wave_b - (s.cpu_pct.clamp(0.0, 100.0) / 100.0) * (wave_b - wave_t);
                pts[i] = (px, py);
            }
            draw_area_series(factories, rt, &pts[..n], wave_b, wave_t, theme.accent, 1.3, 0.22);
        } else {
            let py = wave_b - (tel.cpu_pct.clamp(0.0, 100.0) / 100.0) * (wave_b - wave_t);
            draw_line(rt, wave_l, py, wave_l + wave_w, py, theme.accent, 1.3);
        }

        let cpu_col = load_color(theme, tel.cpu_pct, 60.0, 85.0);
        let mut cpu_buf = [0u8; 8];
        let cpu_str = fmt_pct_buf(tel.cpu_pct.clamp(0.0, 100.0).round() as u8, &mut cpu_buf);
        draw_text(
            rt,
            cpu_str,
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
        let mut ram_buf = [0u8; 8];
        let ram_str = fmt_pct_buf(tel.mem_pct.clamp(0.0, 100.0).round() as u8, &mut ram_buf);
        draw_text(
            rt,
            ram_str,
            &fonts.pill_num,
            rect_f(cur_x + 17.0, y, 30.0, ph),
            load_color(theme, tel.mem_pct, 75.0, 88.0),
        );
    }
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
    terminal_preset: TerminalPreset,
    terminal_new_window: bool,
    terminal_keep_open: bool,
    terminal_avail: &TerminalAvailability,
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

        let mut term_menu = None;
        if let Ok(tm) = CreatePopupMenu() {
            for (idx, &preset) in TerminalPreset::ALL.iter().enumerate() {
                let checked = preset == terminal_preset;
                let optional_third_party = matches!(
                    preset,
                    TerminalPreset::WezTerm | TerminalPreset::Alacritty | TerminalPreset::GitBash
                );
                let detected = terminal_avail.is_available(preset);
                let label = if optional_third_party && !detected {
                    format!("{} (未检测到)", preset.menu_label())
                } else {
                    preset.menu_label().to_string()
                };
                let wide = to_wide_null(&label);
                let mut flag = MF_STRING | if checked { MF_CHECKED } else { MF_UNCHECKED };
                if optional_third_party && !detected && !checked {
                    flag |= MF_GRAYED;
                }
                let _ = AppendMenuW(
                    tm,
                    flag,
                    (CMD_TERMINAL_PRESET_BASE as usize) + idx,
                    wide_pcwstr(&wide),
                );
            }
            let _ = AppendMenuW(tm, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(
                tm,
                MF_STRING | if terminal_new_window { MF_CHECKED } else { MF_UNCHECKED },
                CMD_TERMINAL_TOGGLE_NEW_WIN as usize,
                w!("Windows Terminal 开新窗口"),
            );
            let _ = AppendMenuW(
                tm,
                MF_STRING | if terminal_keep_open { MF_CHECKED } else { MF_UNCHECKED },
                CMD_TERMINAL_TOGGLE_KEEP_OPEN as usize,
                w!("退出会话后保留 Shell"),
            );
            let _ = AppendMenuW(tm, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(
                tm,
                MF_STRING,
                CMD_TERMINAL_OPEN_CONFIG as usize,
                w!("打开 / 生成 terminal.json..."),
            );
            let _ = AppendMenuW(menu, MF_POPUP, tm.0 as usize, w!("恢复终端"));
            term_menu = Some(tm);
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
        if let Some(tm) = term_menu {
            let _ = DestroyMenu(tm);
        }
        let _ = DestroyMenu(menu);
        if cmd.0 != 0 {
            Some(cmd.0 as u32)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fmt_pct_buf_and_quantize() {
        let mut buf = [0u8; 8];
        assert_eq!(fmt_pct_buf(0, &mut buf), " 0%");
        assert_eq!(fmt_pct_buf(9, &mut buf), " 9%");
        assert_eq!(fmt_pct_buf(42, &mut buf), "42%");
        assert_eq!(fmt_pct_buf(100, &mut buf), "100%");
        assert_eq!(fmt_pct_buf(255, &mut buf), "100%");

        assert_eq!(quantize_spark_y(0.0), 0);
        assert_eq!(quantize_spark_y(50.0), 14);
        assert_eq!(quantize_spark_y(100.0), 28);
        assert_eq!(quantize_spark_y(-5.0), 0);
        assert_eq!(quantize_spark_y(140.0), 28);
    }

    #[test]
    fn test_pill_renderer_zero_alloc_fast_path_and_invalidation() {
        use std::collections::HashMap;
        use std::path::PathBuf;

        let mut snap = ScanSnapshot {
            data_dir: PathBuf::from("."),
            data_dirs: Vec::new(),
            sessions: Vec::new(),
            agg: crate::stats::aggregate_sessions(&[]),
            statusline: None,
            statusline_changed: false,
            budget: crate::quota::BudgetConfig::default(),
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
        let mut today_c = TokenCounts::default();
        today_c.input_tokens = 12_000;
        today_c.output_tokens = 3_000;
        snap.agg.by_day.insert("2026-11-27".to_string(), today_c);

        let mut renderer = PillRenderer::new();
        let mut measure_calls = 0usize;
        let items = PILL_ITEM_TOTAL | PILL_ITEM_CPU | PILL_ITEM_RAM;

        // First build populates segment & width caches.
        let rebuilt1 = renderer.ensure_token_segs_with(
            1,
            items,
            true,
            false,
            96,
            "2026-11-27",
            &snap,
            |s| {
                measure_calls += 1;
                s.len() as f32 * 7.0
            },
        );
        assert!(rebuilt1);
        assert_eq!(measure_calls, 1);
        let gen1 = renderer.cached_segs_gen;

        // Unchanged tick hits cache with 0 rebuilds and 0 width measurements.
        let rebuilt2 = renderer.ensure_token_segs_with(
            1,
            items,
            true,
            false,
            96,
            "2026-11-27",
            &snap,
            |_| {
                measure_calls += 1;
                0.0
            },
        );
        assert!(!rebuilt2);
        assert_eq!(measure_calls, 1);
        assert_eq!(renderer.cached_segs_gen, gen1);

        // Snapshot bump whose formatted pill text is identical still hits width cache
        // and keeps segs_gen unchanged so D2D repaint is skipped.
        let rebuilt3 = renderer.ensure_token_segs_with(
            2,
            items,
            true,
            false,
            96,
            "2026-11-27",
            &snap,
            |_| {
                measure_calls += 1;
                0.0
            },
        );
        assert!(rebuilt3);
        assert_eq!(measure_calls, 1);
        assert_eq!(renderer.cached_segs_gen, gen1);

        // Changing today's tokens updates segs_gen and measures the new string once.
        snap.agg.by_day.get_mut("2026-11-27").unwrap().output_tokens = 500_000;
        let rebuilt4 = renderer.ensure_token_segs_with(
            3,
            items,
            true,
            false,
            96,
            "2026-11-27",
            &snap,
            |s| {
                measure_calls += 1;
                s.len() as f32 * 7.0
            },
        );
        assert!(rebuilt4);
        assert_eq!(measure_calls, 2);
        assert_ne!(renderer.cached_segs_gen, gen1);

        // Stack-only Copy PillRenderKey detects telemetry changes and matches when unchanged.
        let mut tel = SystemTelemetry::new(16);
        tel.latest.cpu_pct = 20.0;
        tel.latest.mem_pct = 55.0;
        let k1 = renderer.build_render_key(
            96,
            (100, 200),
            (180, 50),
            false,
            false,
            false,
            false,
            AlertSeverity::Normal,
            items,
            true,
            true,
            &tel,
        );
        let k2 = renderer.build_render_key(
            96,
            (100, 200),
            (180, 50),
            false,
            false,
            false,
            false,
            AlertSeverity::Normal,
            items,
            true,
            true,
            &tel,
        );
        assert_eq!(k1, k2);
        tel.latest.cpu_pct = 45.0;
        let k3 = renderer.build_render_key(
            96,
            (100, 200),
            (180, 50),
            false,
            false,
            false,
            false,
            AlertSeverity::Normal,
            items,
            true,
            true,
            &tel,
        );
        assert_ne!(k1, k3);
    }
}
