//! Editorial Direct2D & DirectWrite design system: curated neutral themes (Obsidian & Warm Paper),
//! smart system font detection (Segoe UI Variable / Maple Mono / JetBrains Mono), radial arc gauges,
//! smooth area waveforms, and minimal interactive charts.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::f32::consts::PI;
use std::mem::{size_of, zeroed};
use std::ptr;

use windows::core::{w, Interface, Result, PCWSTR};
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED,
    D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN, D2D1_GRADIENT_STOP,
    D2D1_PIXEL_FORMAT, D2D_POINT_2F, D2D_RECT_F, D2D_SIZE_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1LinearGradientBrush,
    ID2D1PathGeometry, ID2D1RenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle,
    D2D1_ANTIALIAS_MODE_ALIASED, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_ARC_SEGMENT,
    D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL, D2D1_CAP_STYLE_ROUND, D2D1_DRAW_TEXT_OPTIONS_CLIP,
    D2D1_ELLIPSE, D2D1_EXTEND_MODE_CLAMP, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_GAMMA_2_2,
    D2D1_LINE_JOIN_ROUND, D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES,
    D2D1_SWEEP_DIRECTION_CLOCKWISE, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection, IDWriteTextFormat,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_MEDIUM,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING,
    DWRITE_TEXT_METRICS, DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER,
    DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

pub const fn rgba(r: u8, g: u8, b: u8, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

pub const fn rgb(r: u8, g: u8, b: u8) -> D2D1_COLOR_F {
    rgba(r, g, b, 1.0)
}

pub fn with_alpha(c: D2D1_COLOR_F, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { a, ..c }
}

/// Curated editorial color themes — zero neon AI blue/purple/mint clichés.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub is_dark: bool,
    pub caption_colorref: u32, // 0x00BBGGRR for Win11 DWM titlebar
    pub bg: D2D1_COLOR_F,
    pub surface: D2D1_COLOR_F,
    pub surface_subtle: D2D1_COLOR_F,
    pub hover: D2D1_COLOR_F,
    pub border: D2D1_COLOR_F,
    pub grid: D2D1_COLOR_F,
    pub text: D2D1_COLOR_F,
    pub sub: D2D1_COLOR_F,
    pub muted: D2D1_COLOR_F,
    // Restrained Braun / Apple Health / Editorial data palette
    pub accent: D2D1_COLOR_F,        // Warm terracotta (primary focus, Input new, CPU)
    pub accent_soft: D2D1_COLOR_F,   // Subtle warm wash
    pub output: D2D1_COLOR_F,        // Warm champagne / ochre (Output tokens)
    pub cache: D2D1_COLOR_F,         // Calm stone / graphite (Cache read mass)
    pub ram: D2D1_COLOR_F,           // Muted Nordic steel (RAM)
    pub sage: D2D1_COLOR_F,          // Desaturated sage (healthy / savings)
    pub warn: D2D1_COLOR_F,          // Muted ochre
    pub danger: D2D1_COLOR_F,        // Muted coral
    // Pill glass
    pub pill_bg_top: D2D1_COLOR_F,
    pub pill_bg_bot: D2D1_COLOR_F,
    pub pill_bg_hover_top: D2D1_COLOR_F,
    pub pill_bg_hover_bot: D2D1_COLOR_F,
    pub pill_rim: D2D1_COLOR_F,
    pub pill_rim_hover: D2D1_COLOR_F,
    pub tooltip_bg: D2D1_COLOR_F,
    pub shadow_alpha: f32,
}

pub const THEME_OBSIDIAN: Theme = Theme {
    is_dark: true,
    caption_colorref: 0x00131111, // #111113
    bg: rgb(17, 17, 19),          // #111113 Pure warm neutral charcoal
    surface: rgb(24, 24, 27),     // #18181B Zinc 900
    surface_subtle: rgb(20, 20, 22),
    hover: rgb(35, 35, 39),       // #232327
    border: rgba(255, 255, 255, 0.075),
    grid: rgba(255, 255, 255, 0.045),
    text: rgb(237, 237, 239),     // #EDEDEF
    sub: rgb(142, 142, 147),      // #8E8E93 Apple system gray
    muted: rgb(92, 92, 98),       // #5C5C62
    accent: rgb(229, 133, 74),    // #E5854A Braun terracotta
    accent_soft: rgba(229, 133, 74, 0.14),
    output: rgb(224, 192, 120),   // #E0C078 Champagne sand gold
    cache: rgb(64, 67, 74),       // #40434A Quiet warm graphite
    ram: rgb(138, 162, 184),      // #8AA2B8 Muted steel
    sage: rgb(114, 168, 139),     // #72A88B Desaturated sage
    warn: rgb(217, 161, 74),      // #D9A14A
    danger: rgb(217, 107, 107),   // #D96B6B
    pill_bg_top: rgba(26, 26, 29, 0.93),
    pill_bg_bot: rgba(18, 18, 20, 0.95),
    pill_bg_hover_top: rgba(34, 34, 38, 0.97),
    pill_bg_hover_bot: rgba(23, 23, 26, 0.98),
    pill_rim: rgba(255, 255, 255, 0.11),
    pill_rim_hover: rgba(229, 133, 74, 0.55),
    tooltip_bg: rgba(15, 15, 17, 0.96),
    shadow_alpha: 1.0,
};

pub const THEME_PAPER: Theme = Theme {
    is_dark: false,
    caption_colorref: 0x00F0F4F5, // #F5F4F0
    bg: rgb(245, 244, 240),       // #F5F4F0 Warm alabaster paper
    surface: rgb(255, 255, 255),  // #FFFFFF Crisp card
    surface_subtle: rgb(237, 235, 229),
    hover: rgb(235, 232, 224),
    border: rgba(24, 24, 27, 0.085),
    grid: rgba(24, 24, 27, 0.055),
    text: rgb(26, 26, 28),        // #1A1A1C Deep ink
    sub: rgb(104, 104, 110),
    muted: rgb(150, 150, 156),
    accent: rgb(212, 102, 39),    // #D46627 Deep terracotta
    accent_soft: rgba(212, 102, 39, 0.12),
    output: rgb(191, 143, 58),    // #BF8F3A Warm ochre
    cache: rgb(211, 208, 199),    // #D3D0C7 Calm warm stone
    ram: rgb(92, 121, 146),       // #5C7992 Muted steel
    sage: rgb(74, 135, 104),      // #4A8768 Botanical sage
    warn: rgb(196, 134, 42),
    danger: rgb(196, 78, 78),
    pill_bg_top: rgba(255, 255, 255, 0.94),
    pill_bg_bot: rgba(246, 245, 241, 0.96),
    pill_bg_hover_top: rgba(255, 255, 255, 0.99),
    pill_bg_hover_bot: rgba(250, 249, 246, 0.99),
    pill_rim: rgba(24, 24, 27, 0.12),
    pill_rim_hover: rgba(212, 102, 39, 0.50),
    tooltip_bg: rgba(255, 255, 255, 0.97),
    shadow_alpha: 0.45,
};

pub fn theme_for(light: bool) -> &'static Theme {
    if light {
        &THEME_PAPER
    } else {
        &THEME_OBSIDIAN
    }
}

pub fn load_color(t: &Theme, pct: f32, warn: f32, high: f32) -> D2D1_COLOR_F {
    if pct >= high {
        t.danger
    } else if pct >= warn {
        t.warn
    } else {
        t.text
    }
}

const UTF16_STACK_CAP: usize = 128;
const MEASURE_CACHE_MAX: usize = 512;

#[inline]
fn with_utf16<R>(text: &str, f: impl FnOnce(&[u16]) -> R) -> R {
    let mut stack = [0u16; UTF16_STACK_CAP];
    let mut len = 0usize;
    let mut overflow = false;
    for unit in text.encode_utf16() {
        if len < UTF16_STACK_CAP {
            stack[len] = unit;
            len += 1;
        } else {
            overflow = true;
            break;
        }
    }
    if !overflow {
        f(&stack[..len])
    } else {
        let wide: Vec<u16> = text.encode_utf16().collect();
        f(&wide)
    }
}

#[derive(Default)]
struct MeasureCache {
    by_fmt: HashMap<usize, HashMap<String, f32>>,
    total: usize,
}

impl MeasureCache {
    #[inline]
    fn get(&self, fmt_key: usize, text: &str) -> Option<f32> {
        self.by_fmt.get(&fmt_key).and_then(|m| m.get(text)).copied()
    }

    fn insert(&mut self, fmt_key: usize, text: &str, width: f32) {
        if self.total >= MEASURE_CACHE_MAX {
            self.by_fmt.clear();
            self.total = 0;
        }
        let inner = self.by_fmt.entry(fmt_key).or_default();
        if inner.insert(text.to_string(), width).is_none() {
            self.total += 1;
        }
    }

    fn clear_and_shrink(&mut self) {
        self.by_fmt.clear();
        self.by_fmt.shrink_to_fit();
        self.total = 0;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct GradKey {
    y0: u32,
    y1: u32,
    c0: [u32; 4],
    c1: [u32; 4],
}

impl GradKey {
    #[inline]
    fn new(y0: f32, y1: f32, c0: D2D1_COLOR_F, c1: D2D1_COLOR_F) -> Self {
        Self {
            y0: y0.to_bits(),
            y1: y1.to_bits(),
            c0: [c0.r.to_bits(), c0.g.to_bits(), c0.b.to_bits(), c0.a.to_bits()],
            c1: [c1.r.to_bits(), c1.g.to_bits(), c1.b.to_bits(), c1.a.to_bits()],
        }
    }
}

/// Tiny 2-slot linear gradient cache bound to a single `ID2D1DCRenderTarget`.
/// Must be cleared before dropping the owning render target.
#[derive(Default)]
struct GradCache {
    slots: [Option<(GradKey, ID2D1LinearGradientBrush)>; 2],
    next_evict: usize,
}

impl GradCache {
    fn get(&self, key: GradKey) -> Option<ID2D1LinearGradientBrush> {
        for slot in &self.slots {
            if let Some((k, b)) = slot {
                if *k == key {
                    return Some(b.clone());
                }
            }
        }
        None
    }

    fn insert(&mut self, key: GradKey, brush: ID2D1LinearGradientBrush) {
        for slot in &mut self.slots {
            if slot.is_none() {
                *slot = Some((key, brush));
                return;
            }
        }
        let idx = self.next_evict % self.slots.len();
        self.slots[idx] = Some((key, brush));
        self.next_evict = (idx + 1) % self.slots.len();
    }

    fn clear(&mut self) {
        self.slots = [None, None];
        self.next_evict = 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RingArcKey {
    radius_bits: u32,
    start_bits: u32,
    sweep_half_deg: u16,
}

impl RingArcKey {
    #[inline]
    fn new(radius: f32, start_rad: f32, sweep_rad: f32) -> (Self, f32) {
        let half_deg = (sweep_rad.to_degrees() * 2.0).round().clamp(1.0, 719.0) as u16;
        let q_sweep = (half_deg as f32 * 0.5).to_radians();
        (
            Self {
                radius_bits: radius.to_bits(),
                start_bits: start_rad.to_bits(),
                sweep_half_deg: half_deg,
            },
            q_sweep,
        )
    }
}

/// Tiny 2-slot centered ring-arc geometry cache bound to a `D2DFactories` (Hero CPU + RAM rings).
/// Automatically torn down when `dash_factories` drops on dashboard hide.
#[derive(Default)]
struct RingArcCache {
    slots: [Option<(RingArcKey, ID2D1PathGeometry)>; 2],
    next_evict: usize,
}

impl RingArcCache {
    fn get(&self, key: RingArcKey) -> Option<ID2D1PathGeometry> {
        for slot in &self.slots {
            if let Some((k, g)) = slot {
                if *k == key {
                    return Some(g.clone());
                }
            }
        }
        None
    }

    fn insert(&mut self, key: RingArcKey, geo: ID2D1PathGeometry) {
        for slot in &mut self.slots {
            if slot.is_none() {
                *slot = Some((key, geo));
                return;
            }
        }
        let idx = self.next_evict % self.slots.len();
        self.slots[idx] = Some((key, geo));
        self.next_evict = (idx + 1) % self.slots.len();
    }

    fn clear(&mut self) {
        self.slots = [None, None];
        self.next_evict = 0;
    }
}

struct ActiveBrushScope {
    rt_ptr: usize,
    solid: ID2D1SolidColorBrush,
    grads: GradCache,
}

thread_local! {
    static ACTIVE_BRUSHES: RefCell<Option<ActiveBrushScope>> = const { RefCell::new(None) };
}

struct BrushScopeGuard<'a> {
    grad_owner: &'a RefCell<GradCache>,
}

impl Drop for BrushScopeGuard<'_> {
    fn drop(&mut self) {
        ACTIVE_BRUSHES.with(|tls| {
            if let Some(active) = tls.borrow_mut().take() {
                *self.grad_owner.borrow_mut() = active.grads;
            }
        });
    }
}

fn enter_brush_scope<'a>(
    base: &ID2D1RenderTarget,
    solid: &Option<ID2D1SolidColorBrush>,
    grad_owner: &'a RefCell<GradCache>,
) -> Option<BrushScopeGuard<'a>> {
    let solid = solid.as_ref()?.clone();
    let grads = std::mem::take(&mut *grad_owner.borrow_mut());
    let rt_ptr = base.as_raw() as usize;
    ACTIVE_BRUSHES.with(|tls| {
        *tls.borrow_mut() = Some(ActiveBrushScope {
            rt_ptr,
            solid,
            grads,
        });
    });
    Some(BrushScopeGuard { grad_owner })
}

#[inline]
fn with_solid_brush(
    rt: &ID2D1RenderTarget,
    color: D2D1_COLOR_F,
    f: impl FnOnce(&ID2D1SolidColorBrush),
) {
    let rt_ptr = rt.as_raw() as usize;
    let mut f = Some(f);
    ACTIVE_BRUSHES.with(|tls| {
        let borrow = tls.borrow();
        if let Some(active) = borrow.as_ref() {
            if active.rt_ptr == rt_ptr {
                unsafe { active.solid.SetColor(&color) };
                if let Some(cb) = f.take() {
                    cb(&active.solid);
                }
            }
        }
    });
    if let Some(cb) = f {
        if let Some(b) = unsafe { rt.CreateSolidColorBrush(&color, None).ok() } {
            cb(&b);
        }
    }
}

pub struct D2DFactories {
    pub d2d: ID2D1Factory,
    pub dwrite: IDWriteFactory,
    stroke_round_cap: Option<ID2D1StrokeStyle>,
    stroke_round_cap_join: Option<ID2D1StrokeStyle>,
    brand_track_geo: Option<ID2D1PathGeometry>,
    brand_reading_geo: Option<ID2D1PathGeometry>,
    measure_cache: RefCell<MeasureCache>,
    ring_arc_cache: RefCell<RingArcCache>,
}

impl D2DFactories {
    pub fn new() -> Result<Self> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

            let sp_cap = D2D1_STROKE_STYLE_PROPERTIES {
                startCap: D2D1_CAP_STYLE_ROUND,
                endCap: D2D1_CAP_STYLE_ROUND,
                ..Default::default()
            };
            let stroke_round_cap = d2d.CreateStrokeStyle(&sp_cap, None).ok();

            let sp_cap_join = D2D1_STROKE_STYLE_PROPERTIES {
                startCap: D2D1_CAP_STYLE_ROUND,
                endCap: D2D1_CAP_STYLE_ROUND,
                lineJoin: D2D1_LINE_JOIN_ROUND,
                ..Default::default()
            };
            let stroke_round_cap_join = d2d.CreateStrokeStyle(&sp_cap_join, None).ok();

            let brand_track_geo = Self::build_centered_arc_geo(&d2d, 3.85, 0.75 * PI, 1.5 * PI);
            let brand_reading_geo =
                Self::build_centered_arc_geo(&d2d, 3.85, 0.75 * PI, 193.0_f32.to_radians());

            Ok(Self {
                d2d,
                dwrite,
                stroke_round_cap,
                stroke_round_cap_join,
                brand_track_geo,
                brand_reading_geo,
                measure_cache: RefCell::new(MeasureCache::default()),
                ring_arc_cache: RefCell::new(RingArcCache::default()),
            })
        }
    }

    fn get_or_build_ring_arc(
        &self,
        radius: f32,
        start_rad: f32,
        sweep_rad: f32,
    ) -> Option<ID2D1PathGeometry> {
        let (key, q_sweep) = RingArcKey::new(radius, start_rad, sweep_rad);
        if let Some(geo) = self.ring_arc_cache.borrow().get(key) {
            return Some(geo);
        }
        let geo = Self::build_centered_arc_geo(&self.d2d, radius, start_rad, q_sweep)?;
        self.ring_arc_cache.borrow_mut().insert(key, geo.clone());
        Some(geo)
    }

    fn build_centered_arc_geo(
        d2d: &ID2D1Factory,
        radius: f32,
        start_rad: f32,
        sweep_rad: f32,
    ) -> Option<ID2D1PathGeometry> {
        let end_rad = start_rad + sweep_rad;
        let sx = radius * start_rad.cos();
        let sy = radius * start_rad.sin();
        let ex = radius * end_rad.cos();
        let ey = radius * end_rad.sin();
        unsafe {
            let geo = d2d.CreatePathGeometry().ok()?;
            let sink = geo.Open().ok()?;
            sink.BeginFigure(pt(sx, sy), D2D1_FIGURE_BEGIN_HOLLOW);
            let seg = D2D1_ARC_SEGMENT {
                point: pt(ex, ey),
                size: D2D_SIZE_F {
                    width: radius,
                    height: radius,
                },
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: if sweep_rad > PI {
                    D2D1_ARC_SIZE_LARGE
                } else {
                    D2D1_ARC_SIZE_SMALL
                },
            };
            sink.AddArc(&seg);
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
            sink.Close().ok()?;
            Some(geo)
        }
    }

    pub fn clear_measure_cache(&self) {
        self.measure_cache.borrow_mut().clear_and_shrink();
        self.ring_arc_cache.borrow_mut().clear();
    }

    fn get_font_collection(&self) -> Option<IDWriteFontCollection> {
        unsafe {
            let mut coll: Option<IDWriteFontCollection> = None;
            if self.dwrite.GetSystemFontCollection(&mut coll, false).is_ok() {
                coll
            } else {
                None
            }
        }
    }

    fn pick_from_collection(
        coll: Option<&IDWriteFontCollection>,
        candidates: &[PCWSTR],
        fallback: PCWSTR,
    ) -> PCWSTR {
        if let Some(c) = coll {
            unsafe {
                for &name in candidates {
                    let mut idx = 0u32;
                    let mut exists = BOOL(0);
                    if c.FindFamilyName(name, &mut idx, &mut exists).is_ok() && exists.as_bool() {
                        return name;
                    }
                }
            }
        }
        fallback
    }

    pub fn make_format(
        &self,
        family: PCWSTR,
        size: f32,
        weight: DWRITE_FONT_WEIGHT,
        h_align: DWRITE_TEXT_ALIGNMENT,
        v_align: DWRITE_PARAGRAPH_ALIGNMENT,
        ellipsis: bool,
    ) -> Result<IDWriteTextFormat> {
        unsafe {
            let fmt = self.dwrite.CreateTextFormat(
                family,
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!("zh-CN"),
            )?;
            fmt.SetTextAlignment(h_align)?;
            fmt.SetParagraphAlignment(v_align)?;
            fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            if ellipsis {
                let sign = self.dwrite.CreateEllipsisTrimmingSign(&fmt)?;
                let trim = DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    delimiter: 0,
                    delimiterCount: 0,
                };
                fmt.SetTrimming(&trim, &sign)?;
            }
            Ok(fmt)
        }
    }

    pub fn measure_text_width(&self, text: &str, fmt: &IDWriteTextFormat) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let fmt_key = fmt.as_raw() as usize;
        if let Some(cached) = self.measure_cache.borrow().get(fmt_key, text) {
            return cached;
        }
        let measured = with_utf16(text, |wide| {
            if wide.is_empty() {
                return 0.0;
            }
            unsafe {
                if let Ok(layout) = self.dwrite.CreateTextLayout(wide, fmt, 1000.0, 100.0) {
                    let mut m: DWRITE_TEXT_METRICS = zeroed();
                    if layout.GetMetrics(&mut m).is_ok() {
                        return m.widthIncludingTrailingWhitespace.ceil();
                    }
                }
            }
            (text.chars().count() as f32 * 7.5).ceil()
        });
        self.measure_cache.borrow_mut().insert(fmt_key, text, measured);
        measured
    }
}

/// Lazily-created dashboard typography palette (released immediately on dashboard hide so
/// pill-only mode never loads or retains the 17 dashboard `IDWriteTextFormat` + UI font family).
pub struct DashFonts {
    pub ui_brand: IDWriteTextFormat,
    pub ui_section: IDWriteTextFormat,
    pub ui_tab: IDWriteTextFormat,
    pub ui_reg: IDWriteTextFormat,
    pub ui_reg_center: IDWriteTextFormat,
    pub ui_small: IDWriteTextFormat,
    pub ui_small_center: IDWriteTextFormat,
    pub ui_small_right: IDWriteTextFormat,
    pub ui_micro: IDWriteTextFormat,
    pub mono_hero: IDWriteTextFormat,
    pub mono_kpi: IDWriteTextFormat,
    pub mono_kpi_center: IDWriteTextFormat,
    pub mono_med: IDWriteTextFormat,
    pub mono_med_right: IDWriteTextFormat,
    pub mono_small: IDWriteTextFormat,
    pub mono_small_center: IDWriteTextFormat,
    pub mono_small_right: IDWriteTextFormat,
}

impl DashFonts {
    fn new(f: &D2DFactories, ui: PCWSTR, mono: PCWSTR) -> Result<Self> {
        Ok(Self {
            ui_brand: f.make_format(ui, 14.5, DWRITE_FONT_WEIGHT_BOLD, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, true)?,
            ui_section: f.make_format(ui, 12.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, true)?,
            ui_tab: f.make_format(ui, 12.0, DWRITE_FONT_WEIGHT_MEDIUM, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, true)?,
            ui_reg: f.make_format(ui, 12.0, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, true)?,
            ui_reg_center: f.make_format(ui, 12.0, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, true)?,
            ui_small: f.make_format(ui, 10.5, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, true)?,
            ui_small_center: f.make_format(ui, 10.5, DWRITE_FONT_WEIGHT_MEDIUM, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, true)?,
            ui_small_right: f.make_format(ui, 10.5, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, true)?,
            ui_micro: f.make_format(ui, 9.5, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, true)?,
            mono_hero: f.make_format(mono, 26.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, true)?,
            mono_kpi: f.make_format(mono, 16.5, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, true)?,
            mono_kpi_center: f.make_format(mono, 16.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, false)?,
            mono_med: f.make_format(mono, 12.0, DWRITE_FONT_WEIGHT_MEDIUM, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, true)?,
            mono_med_right: f.make_format(mono, 11.5, DWRITE_FONT_WEIGHT_MEDIUM, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, true)?,
            mono_small: f.make_format(mono, 10.5, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, true)?,
            mono_small_center: f.make_format(mono, 9.5, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, false)?,
            mono_small_right: f.make_format(mono, 10.0, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, true)?,
        })
    }
}

/// Editorial typography palette: eagerly allocates ONLY the 2 monospace formats used by the
/// floating pill; the 17 dashboard formats (`DashFonts`) are lazily created on first dashboard
/// render and released when the dashboard hides.
pub struct Fonts {
    pub pill_tok: IDWriteTextFormat,
    pub pill_num: IDWriteTextFormat,
    mono_family: PCWSTR,
    ui_family: Cell<Option<PCWSTR>>,
    dash: RefCell<Option<DashFonts>>,
}

impl Fonts {
    pub fn new(f: &D2DFactories) -> Result<Self> {
        let coll = f.get_font_collection();
        let mono = D2DFactories::pick_from_collection(
            coll.as_ref(),
            &[
                w!("Maple Mono Normal NF CN"),
                w!("JetBrainsMonoNL Nerd Font"),
                w!("Cascadia Mono"),
                w!("Consolas"),
            ],
            w!("Consolas"),
        );

        Ok(Self {
            pill_tok: f.make_format(mono, 12.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, false)?,
            pill_num: f.make_format(mono, 10.5, DWRITE_FONT_WEIGHT_MEDIUM, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, false)?,
            mono_family: mono,
            ui_family: Cell::new(None),
            dash: RefCell::new(None),
        })
    }

    /// Lazily initializes dashboard typography on first dashboard render and passes `&DashFonts`
    /// to `f_draw`. Caches the resolved static UI font family name across reopens so
    /// `GetSystemFontCollection` is called at most once.
    pub fn with_dash<R>(
        &self,
        factories: &D2DFactories,
        f_draw: impl FnOnce(&DashFonts) -> R,
    ) -> Result<R> {
        if self.dash.borrow().is_none() {
            let ui = match self.ui_family.get() {
                Some(cached) => cached,
                None => {
                    let coll = factories.get_font_collection();
                    let picked = D2DFactories::pick_from_collection(
                        coll.as_ref(),
                        &[
                            w!("Segoe UI Variable Text"),
                            w!("Segoe UI Variable"),
                            w!("Microsoft YaHei UI"),
                        ],
                        w!("Microsoft YaHei UI"),
                    );
                    self.ui_family.set(Some(picked));
                    picked
                }
            };
            let built = DashFonts::new(factories, ui, self.mono_family)?;
            *self.dash.borrow_mut() = Some(built);
        }
        let borrow = self.dash.borrow();
        Ok(f_draw(borrow.as_ref().expect("dashboard fonts initialized")))
    }

    /// Releases the 17 dashboard `IDWriteTextFormat` + ellipsis trimming COM objects when hidden.
    pub fn release_dashboard_fonts(&mut self) {
        *self.dash.get_mut() = None;
    }
}

unsafe fn create_bgra_dib(dc: HDC, width_px: i32, height_px: i32) -> Result<HBITMAP> {
    let mut bi: BITMAPINFO = zeroed();
    bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = width_px;
    bi.bmiHeader.biHeight = -height_px;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB.0;
    let mut bits: *mut std::ffi::c_void = ptr::null_mut();
    CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, None, 0)
}

/// Per-pixel-alpha software Direct2D surface for the floating pill (zero GPU heap!).
pub struct LayeredSurface {
    dc: HDC,
    bmp: HBITMAP,
    old_obj: HGDIOBJ,
    rt: Option<ID2D1DCRenderTarget>,
    solid_brush: Option<ID2D1SolidColorBrush>,
    grad_cache: RefCell<GradCache>,
    width_px: i32,
    height_px: i32,
    dpi: f32,
}

impl LayeredSurface {
    pub fn new(factories: &D2DFactories, width_px: i32, height_px: i32, dpi: f32) -> Result<Self> {
        let w = width_px.max(1);
        let h = height_px.max(1);
        unsafe {
            let dc = CreateCompatibleDC(HDC::default());
            if dc.is_invalid() {
                return Err(windows::core::Error::from_win32());
            }
            let bmp = match create_bgra_dib(dc, w, h) {
                Ok(b) => b,
                Err(e) => {
                    let _ = DeleteDC(dc);
                    return Err(e);
                }
            };
            let old_obj = SelectObject(dc, bmp);

            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: dpi,
                dpiY: dpi,
                ..Default::default()
            };
            let rt = match factories.d2d.CreateDCRenderTarget(&props) {
                Ok(r) => r,
                Err(e) => {
                    let _ = SelectObject(dc, old_obj);
                    let _ = DeleteObject(bmp);
                    let _ = DeleteDC(dc);
                    return Err(e);
                }
            };
            rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let solid_brush = rt.CreateSolidColorBrush(&rgba(0, 0, 0, 1.0), None).ok();

            Ok(Self {
                dc,
                bmp,
                old_obj,
                rt: Some(rt),
                solid_brush,
                grad_cache: RefCell::new(GradCache::default()),
                width_px: w,
                height_px: h,
                dpi,
            })
        }
    }

    /// Resizes the backing DIB and/or updates DPI in place while keeping the existing
    /// `ID2D1DCRenderTarget`, `HDC`, and cached brushes alive.
    #[allow(dead_code)]
    pub fn resize(&mut self, width_px: i32, height_px: i32, dpi: f32) -> Result<()> {
        let w = width_px.max(1);
        let h = height_px.max(1);
        if self.width_px == w && self.height_px == h && (self.dpi - dpi).abs() < 0.5 {
            return Ok(());
        }
        unsafe {
            if (self.dpi - dpi).abs() >= 0.5 {
                if let Some(rt) = &self.rt {
                    rt.SetDpi(dpi, dpi);
                }
                self.dpi = dpi;
            }
            if self.width_px != w || self.height_px != h {
                let new_bmp = create_bgra_dib(self.dc, w, h)?;
                let prev = SelectObject(self.dc, new_bmp);
                let _ = DeleteObject(prev);
                self.bmp = new_bmp;
                self.width_px = w;
                self.height_px = h;
            }
        }
        Ok(())
    }

    pub fn size_px(&self) -> (i32, i32) {
        (self.width_px, self.height_px)
    }

    pub fn dpi(&self) -> f32 {
        self.dpi
    }

    pub fn render_and_present<F>(&self, hwnd: HWND, x: i32, y: i32, draw_fn: F) -> Result<()>
    where
        F: FnOnce(&ID2D1RenderTarget),
    {
        unsafe {
            let Some(rt) = &self.rt else {
                return Ok(());
            };
            let sub = RECT {
                left: 0,
                top: 0,
                right: self.width_px,
                bottom: self.height_px,
            };
            rt.BindDC(self.dc, &sub)?;
            rt.BeginDraw();
            rt.Clear(Some(&rgba(0, 0, 0, 0.0)));
            let base: ID2D1RenderTarget = windows::core::Interface::cast(rt)?;
            {
                let _brush_guard = enter_brush_scope(&base, &self.solid_brush, &self.grad_cache);
                draw_fn(&base);
            }
            rt.EndDraw(None, None)?;

            let dst_pt = POINT { x, y };
            let sz = SIZE {
                cx: self.width_px,
                cy: self.height_px,
            };
            let src_pt = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                hwnd,
                HDC::default(),
                Some(&dst_pt),
                Some(&sz),
                self.dc,
                Some(&src_pt),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            Ok(())
        }
    }
}

impl Drop for LayeredSurface {
    fn drop(&mut self) {
        // Release all brushes bound to `rt` BEFORE releasing the Direct2D DC render target
        // and tearing down the backing GDI DC/bitmap.
        self.grad_cache.get_mut().clear();
        self.solid_brush.take();
        self.rt.take();
        unsafe {
            let _ = SelectObject(self.dc, self.old_obj);
            let _ = DeleteObject(self.bmp);
            let _ = DeleteDC(self.dc);
        }
    }
}

/// Flicker-free double-buffered Direct2D DC surface for the dashboard window.
pub struct WindowSurface {
    dc: HDC,
    bmp: HBITMAP,
    old_obj: HGDIOBJ,
    rt: Option<ID2D1DCRenderTarget>,
    solid_brush: Option<ID2D1SolidColorBrush>,
    grad_cache: RefCell<GradCache>,
    width_px: i32,
    height_px: i32,
    dpi: f32,
}

impl WindowSurface {
    pub fn new(factories: &D2DFactories, width_px: i32, height_px: i32, dpi: f32) -> Result<Self> {
        let w = width_px.max(1);
        let h = height_px.max(1);
        unsafe {
            let dc = CreateCompatibleDC(HDC::default());
            if dc.is_invalid() {
                return Err(windows::core::Error::from_win32());
            }
            let bmp = match create_bgra_dib(dc, w, h) {
                Ok(b) => b,
                Err(e) => {
                    let _ = DeleteDC(dc);
                    return Err(e);
                }
            };
            let old_obj = SelectObject(dc, bmp);

            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: dpi,
                dpiY: dpi,
                ..Default::default()
            };
            let rt = match factories.d2d.CreateDCRenderTarget(&props) {
                Ok(r) => r,
                Err(e) => {
                    let _ = SelectObject(dc, old_obj);
                    let _ = DeleteObject(bmp);
                    let _ = DeleteDC(dc);
                    return Err(e);
                }
            };
            rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let solid_brush = rt.CreateSolidColorBrush(&rgba(0, 0, 0, 1.0), None).ok();

            Ok(Self {
                dc,
                bmp,
                old_obj,
                rt: Some(rt),
                solid_brush,
                grad_cache: RefCell::new(GradCache::default()),
                width_px: w,
                height_px: h,
                dpi,
            })
        }
    }

    /// Resizes the backing DIB and/or updates DPI in place while reusing the existing
    /// `ID2D1DCRenderTarget`, `HDC`, and cached brushes.
    pub fn resize(&mut self, width_px: i32, height_px: i32, dpi: f32) -> Result<()> {
        let w = width_px.max(1);
        let h = height_px.max(1);
        if self.width_px == w && self.height_px == h && (self.dpi - dpi).abs() < 0.5 {
            return Ok(());
        }
        unsafe {
            if (self.dpi - dpi).abs() >= 0.5 {
                if let Some(rt) = &self.rt {
                    rt.SetDpi(dpi, dpi);
                }
                self.dpi = dpi;
            }
            if self.width_px != w || self.height_px != h {
                let new_bmp = create_bgra_dib(self.dc, w, h)?;
                let prev = SelectObject(self.dc, new_bmp);
                let _ = DeleteObject(prev);
                self.bmp = new_bmp;
                self.width_px = w;
                self.height_px = h;
            }
        }
        Ok(())
    }

    pub fn matches(&self, width_px: i32, height_px: i32, dpi: f32) -> bool {
        self.width_px == width_px && self.height_px == height_px && (self.dpi - dpi).abs() < 0.5
    }

    pub fn render_and_blit<F>(&self, target_hdc: Option<HDC>, draw_fn: F) -> Result<()>
    where
        F: FnOnce(&ID2D1RenderTarget),
    {
        unsafe {
            let Some(rt) = &self.rt else {
                return Ok(());
            };
            let sub = RECT {
                left: 0,
                top: 0,
                right: self.width_px,
                bottom: self.height_px,
            };
            rt.BindDC(self.dc, &sub)?;
            rt.BeginDraw();
            let base: ID2D1RenderTarget = windows::core::Interface::cast(rt)?;
            {
                let _brush_guard = enter_brush_scope(&base, &self.solid_brush, &self.grad_cache);
                draw_fn(&base);
            }
            rt.EndDraw(None, None)?;

            if let Some(hdc) = target_hdc {
                let _ = BitBlt(hdc, 0, 0, self.width_px, self.height_px, self.dc, 0, 0, SRCCOPY);
            }
            Ok(())
        }
    }

    /// Renders strictly inside `clip_dip` over the retained DIB and blits ONLY that dirty
    /// rectangle to `target_hdc` (used for 1s Hero Block 4 CPU/RAM updates on non-System tabs).
    pub fn render_clip_and_blit<F>(
        &self,
        target_hdc: Option<HDC>,
        clip_dip: D2D_RECT_F,
        draw_fn: F,
    ) -> Result<()>
    where
        F: FnOnce(&ID2D1RenderTarget),
    {
        unsafe {
            let Some(rt) = &self.rt else {
                return Ok(());
            };
            let sub = RECT {
                left: 0,
                top: 0,
                right: self.width_px,
                bottom: self.height_px,
            };
            rt.BindDC(self.dc, &sub)?;
            rt.BeginDraw();
            let base: ID2D1RenderTarget = windows::core::Interface::cast(rt)?;
            base.PushAxisAlignedClip(&clip_dip, D2D1_ANTIALIAS_MODE_ALIASED);
            {
                let _brush_guard = enter_brush_scope(&base, &self.solid_brush, &self.grad_cache);
                draw_fn(&base);
            }
            base.PopAxisAlignedClip();
            rt.EndDraw(None, None)?;

            if let Some(hdc) = target_hdc {
                let scale = self.dpi / 96.0;
                let px_l = ((clip_dip.left * scale).floor() as i32 - 1).clamp(0, self.width_px);
                let px_t = ((clip_dip.top * scale).floor() as i32 - 1).clamp(0, self.height_px);
                let px_r = ((clip_dip.right * scale).ceil() as i32 + 1).clamp(px_l, self.width_px);
                let px_b = ((clip_dip.bottom * scale).ceil() as i32 + 1).clamp(px_t, self.height_px);
                let bw = px_r - px_l;
                let bh = px_b - px_t;
                if bw > 0 && bh > 0 {
                    let _ = BitBlt(hdc, px_l, px_t, bw, bh, self.dc, px_l, px_t, SRCCOPY);
                }
            }
            Ok(())
        }
    }
}

impl Drop for WindowSurface {
    fn drop(&mut self) {
        // Release all brushes bound to `rt` BEFORE releasing the Direct2D DC render target
        // and tearing down the backing GDI DC/bitmap.
        self.grad_cache.get_mut().clear();
        self.solid_brush.take();
        self.rt.take();
        unsafe {
            let _ = SelectObject(self.dc, self.old_obj);
            let _ = DeleteObject(self.bmp);
            let _ = DeleteDC(self.dc);
        }
    }
}

// ---------- Drawing Primitives ----------

pub fn rect_f(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

pub fn pt(x: f32, y: f32) -> D2D_POINT_2F {
    D2D_POINT_2F { x, y }
}

pub fn hit_rect(r: &D2D_RECT_F, x: f32, y: f32) -> bool {
    x >= r.left && x <= r.right && y >= r.top && y <= r.bottom
}

#[allow(dead_code)]
pub fn brush(rt: &ID2D1RenderTarget, color: D2D1_COLOR_F) -> Option<ID2D1SolidColorBrush> {
    let rt_ptr = rt.as_raw() as usize;
    let cached = ACTIVE_BRUSHES.with(|tls| {
        let borrow = tls.borrow();
        borrow.as_ref().and_then(|active| {
            if active.rt_ptr == rt_ptr {
                unsafe { active.solid.SetColor(&color) };
                Some(active.solid.clone())
            } else {
                None
            }
        })
    });
    cached.or_else(|| unsafe { rt.CreateSolidColorBrush(&color, None).ok() })
}

pub fn linear_v_brush(
    rt: &ID2D1RenderTarget,
    y0: f32,
    y1: f32,
    c0: D2D1_COLOR_F,
    c1: D2D1_COLOR_F,
) -> Option<ID2D1LinearGradientBrush> {
    let rt_ptr = rt.as_raw() as usize;
    let key = GradKey::new(y0, y1, c0, c1);
    let cached = ACTIVE_BRUSHES.with(|tls| {
        let borrow = tls.borrow();
        borrow.as_ref().and_then(|active| {
            if active.rt_ptr == rt_ptr {
                active.grads.get(key)
            } else {
                None
            }
        })
    });
    if cached.is_some() {
        return cached;
    }
    let created = unsafe {
        let stops = [
            D2D1_GRADIENT_STOP { position: 0.0, color: c0 },
            D2D1_GRADIENT_STOP { position: 1.0, color: c1 },
        ];
        let coll = rt
            .CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)
            .ok()?;
        let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
            startPoint: pt(0.0, y0),
            endPoint: pt(0.0, y1),
        };
        rt.CreateLinearGradientBrush(&props, None, &coll).ok()?
    };
    ACTIVE_BRUSHES.with(|tls| {
        if let Some(active) = tls.borrow_mut().as_mut() {
            if active.rt_ptr == rt_ptr {
                active.grads.insert(key, created.clone());
            }
        }
    });
    Some(created)
}

pub fn fill_rect(rt: &ID2D1RenderTarget, r: D2D_RECT_F, color: D2D1_COLOR_F) {
    with_solid_brush(rt, color, |b| unsafe {
        rt.FillRectangle(&r, b);
    });
}

pub fn fill_round_rect(rt: &ID2D1RenderTarget, r: D2D_RECT_F, radius: f32, color: D2D1_COLOR_F) {
    let rr = D2D1_ROUNDED_RECT {
        rect: r,
        radiusX: radius,
        radiusY: radius,
    };
    with_solid_brush(rt, color, |b| unsafe {
        rt.FillRoundedRectangle(&rr, b);
    });
}

pub fn stroke_round_rect(
    rt: &ID2D1RenderTarget,
    r: D2D_RECT_F,
    radius: f32,
    color: D2D1_COLOR_F,
    width: f32,
) {
    let rr = D2D1_ROUNDED_RECT {
        rect: r,
        radiusX: radius,
        radiusY: radius,
    };
    with_solid_brush(rt, color, |b| unsafe {
        rt.DrawRoundedRectangle(&rr, b, width, None);
    });
}

pub fn fill_circle(rt: &ID2D1RenderTarget, cx: f32, cy: f32, r: f32, color: D2D1_COLOR_F) {
    let ell = D2D1_ELLIPSE {
        point: pt(cx, cy),
        radiusX: r,
        radiusY: r,
    };
    with_solid_brush(rt, color, |b| unsafe {
        rt.FillEllipse(&ell, b);
    });
}

pub fn stroke_circle(rt: &ID2D1RenderTarget, cx: f32, cy: f32, r: f32, color: D2D1_COLOR_F, width: f32) {
    let ell = D2D1_ELLIPSE {
        point: pt(cx, cy),
        radiusX: r,
        radiusY: r,
    };
    with_solid_brush(rt, color, |b| unsafe {
        rt.DrawEllipse(&ell, b, width, None);
    });
}

pub fn draw_line(rt: &ID2D1RenderTarget, x0: f32, y0: f32, x1: f32, y1: f32, color: D2D1_COLOR_F, width: f32) {
    with_solid_brush(rt, color, |b| unsafe {
        rt.DrawLine(pt(x0, y0), pt(x1, y1), b, width, None);
    });
}

pub fn draw_text(
    rt: &ID2D1RenderTarget,
    text: &str,
    fmt: &IDWriteTextFormat,
    r: D2D_RECT_F,
    color: D2D1_COLOR_F,
) {
    if text.is_empty() {
        return;
    }
    with_utf16(text, |wide| {
        with_solid_brush(rt, color, |b| unsafe {
            rt.DrawText(
                wide,
                fmt,
                &r,
                b,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        });
    });
}

/// Draws an anti-aliased rounded-cap circular arc given start and sweep angles in radians
/// (Direct2D screen space: 0 = 3 o'clock, clockwise positive).
#[allow(clippy::too_many_arguments)]
pub fn draw_arc_rad(
    factories: &D2DFactories,
    rt: &ID2D1RenderTarget,
    cx: f32,
    cy: f32,
    radius: f32,
    stroke_w: f32,
    start_rad: f32,
    sweep_rad: f32,
    color: D2D1_COLOR_F,
) {
    if sweep_rad <= 0.001 || radius <= 0.0 {
        return;
    }
    if sweep_rad >= 2.0 * PI - 0.005 {
        stroke_circle(rt, cx, cy, radius, color, stroke_w);
        return;
    }
    if let Some(geo) = factories.get_or_build_ring_arc(radius, start_rad, sweep_rad) {
        unsafe {
            let mut prev = Matrix3x2::default();
            rt.GetTransform(&mut prev);
            let shifted = Matrix3x2 {
                M11: prev.M11,
                M12: prev.M12,
                M21: prev.M21,
                M22: prev.M22,
                M31: prev.M31 + cx * prev.M11 + cy * prev.M21,
                M32: prev.M32 + cx * prev.M12 + cy * prev.M22,
            };
            rt.SetTransform(&shifted);
            with_solid_brush(rt, color, |b| {
                rt.DrawGeometry(&geo, b, stroke_w, factories.stroke_round_cap.as_ref());
            });
            rt.SetTransform(&prev);
        }
    }
}

/// Tactile anti-aliased radial arc gauge (like macOS iStat Menus / Apple Activity rings).
pub fn draw_ring_gauge(
    factories: &D2DFactories,
    rt: &ID2D1RenderTarget,
    cx: f32,
    cy: f32,
    radius: f32,
    stroke_w: f32,
    pct: f32,
    track_col: D2D1_COLOR_F,
    arc_col: D2D1_COLOR_F,
) {
    stroke_circle(rt, cx, cy, radius, track_col, stroke_w);
    let clamped = pct.clamp(0.0, 100.0);
    if clamped <= 0.5 {
        return;
    }
    if clamped >= 99.5 {
        stroke_circle(rt, cx, cy, radius, arc_col, stroke_w);
        return;
    }
    draw_arc_rad(
        factories,
        rt,
        cx,
        cy,
        radius,
        stroke_w,
        -PI * 0.5,
        (clamped / 100.0) * 2.0 * PI,
        arc_col,
    );
}

/// 14 DIP Braun Paper Dial brand micro-icon (1:1 optical match to Concept C `app_16.svg`).
/// `alert_level`: 0 = normal, 1 = warn, 2 = critical.
pub fn draw_brand_dial_icon(
    factories: &D2DFactories,
    rt: &ID2D1RenderTarget,
    cx: f32,
    cy: f32,
    is_dark: bool,
    alert_level: u8,
) {
    let (arc_col, core_col, dot_col) = match alert_level {
        2 => (rgb(217, 107, 107), rgb(217, 107, 107), rgb(217, 107, 107)),
        1 => (rgb(217, 161, 74), rgb(217, 161, 74), rgb(217, 161, 74)),
        _ => (rgb(212, 102, 39), rgb(229, 133, 74), rgb(74, 135, 104)),
    };

    // 1) Obsidian bezel + subtle rim on dark header
    fill_circle(rt, cx, cy, 7.0, rgb(27, 27, 32));
    if is_dark {
        stroke_circle(rt, cx, cy, 6.75, rgba(255, 255, 255, 0.14), 0.75);
    }

    // 2) Warm paper dial face
    fill_circle(rt, cx, cy, 5.5, rgb(246, 244, 237));

    // 3-4) 270 deg track arc + primary reading arc using prebuilt static geometries
    if let (Some(track_geo), Some(read_geo)) = (&factories.brand_track_geo, &factories.brand_reading_geo) {
        unsafe {
            let mut prev = Matrix3x2::default();
            rt.GetTransform(&mut prev);
            let shifted = Matrix3x2 {
                M11: prev.M11,
                M12: prev.M12,
                M21: prev.M21,
                M22: prev.M22,
                M31: prev.M31 + cx * prev.M11 + cy * prev.M21,
                M32: prev.M32 + cx * prev.M12 + cy * prev.M22,
            };
            rt.SetTransform(&shifted);
            with_solid_brush(rt, rgb(210, 206, 195), |b| {
                rt.DrawGeometry(track_geo, b, 1.45, factories.stroke_round_cap.as_ref());
            });
            with_solid_brush(rt, arc_col, |b| {
                rt.DrawGeometry(read_geo, b, 1.45, factories.stroke_round_cap.as_ref());
            });
            rt.SetTransform(&prev);
        }
    } else {
        let start_rad = 0.75 * PI;
        draw_arc_rad(factories, rt, cx, cy, 3.85, 1.45, start_rad, 1.5 * PI, rgb(210, 206, 195));
        draw_arc_rad(factories, rt, cx, cy, 3.85, 1.45, start_rad, 193.0_f32.to_radians(), arc_col);
    }

    // 5) Deep ink pointer + hub + core dot
    let ink = rgb(26, 26, 28);
    with_solid_brush(rt, ink, |b| unsafe {
        rt.DrawLine(
            pt(cx, cy + 0.2),
            pt(cx + 2.33, cy - 1.26),
            b,
            1.05,
            factories.stroke_round_cap.as_ref(),
        );
    });
    fill_circle(rt, cx, cy + 0.2, 1.15, ink);
    fill_circle(rt, cx, cy + 0.2, 0.45, core_col);

    // 6) Bottom opening status dot
    fill_circle(rt, cx, cy + 3.9, 0.65, dot_col);
}

/// Smooth antialiased polyline with soft translucent vertical gradient area fill.
pub fn draw_area_series(
    factories: &D2DFactories,
    rt: &ID2D1RenderTarget,
    points: &[(f32, f32)],
    y_base: f32,
    y_top: f32,
    line_col: D2D1_COLOR_F,
    stroke_w: f32,
    fill_alpha: f32,
) {
    if points.len() < 2 {
        return;
    }
    unsafe {
        if fill_alpha > 0.0 {
            if let Ok(area_geo) = factories.d2d.CreatePathGeometry() {
                if let Ok(sink) = area_geo.Open() {
                    sink.BeginFigure(pt(points[0].0, y_base), D2D1_FIGURE_BEGIN_FILLED);
                    for &(x, y) in points {
                        sink.AddLine(pt(x, y));
                    }
                    sink.AddLine(pt(points[points.len() - 1].0, y_base));
                    sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                    if sink.Close().is_ok() {
                        let top_c = with_alpha(line_col, fill_alpha);
                        let bot_c = with_alpha(line_col, 0.0);
                        if let Some(grad) = linear_v_brush(rt, y_top, y_base, top_c, bot_c) {
                            rt.FillGeometry(&area_geo, &grad, None);
                        }
                    }
                }
            }
        }
        if let Ok(line_geo) = factories.d2d.CreatePathGeometry() {
            if let Ok(sink) = line_geo.Open() {
                sink.BeginFigure(pt(points[0].0, points[0].1), D2D1_FIGURE_BEGIN_HOLLOW);
                for &(x, y) in &points[1..] {
                    sink.AddLine(pt(x, y));
                }
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
                if sink.Close().is_ok() {
                    with_solid_brush(rt, line_col, |b| {
                        rt.DrawGeometry(&line_geo, b, stroke_w, factories.stroke_round_cap_join.as_ref());
                    });
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarUnit {
    Tokens,
    Usd,
}

#[derive(Debug, Clone)]
pub struct BarItem {
    pub label: Cow<'static, str>,
    pub cache: f64,
    pub input: f64,
    pub output: f64,
    pub marker: bool,
}

/// Computes the exact X-axis label stride used by `draw_editorial_bars`, so callers with
/// hundreds of bars (e.g., long sessions) can skip allocating unused intermediate labels.
#[inline]
pub fn editorial_bar_label_stride(plot_bounds: D2D_RECT_F, n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let plot_w = (plot_bounds.right - plot_bounds.left - 66.0).max(40.0);
    (((40.0 * n as f32) / plot_w).ceil() as usize).max(1)
}

/// Editorial minimalist stacked bar chart supporting Total vs Fresh-Only (non-cached)
/// and both Token and USD display units. Tooltips are constructed lazily only for the
/// single hovered bar index. Returns `(slot_rects, inner_plot_rect)`.
#[allow(clippy::too_many_arguments)]
pub fn draw_editorial_bars<F>(
    rt: &ID2D1RenderTarget,
    fonts: &DashFonts,
    theme: &Theme,
    plot_bounds: D2D_RECT_F,
    items: &[BarItem],
    fresh_only: bool,
    unit: BarUnit,
    mouse_pos: Option<(f32, f32)>,
    empty_msg: &str,
    tooltip_fn: F,
) -> (Vec<D2D_RECT_F>, Option<D2D_RECT_F>)
where
    F: FnOnce(usize) -> Vec<String>,
{
    if items.is_empty() {
        draw_text(rt, empty_msg, &fonts.ui_reg_center, plot_bounds, theme.muted);
        return (Vec::new(), None);
    }

    let pad_l = 54.0;
    let pad_r = 12.0;
    let pad_t = 12.0;
    let pad_b = 22.0;
    let plot_l = plot_bounds.left + pad_l;
    let plot_r = plot_bounds.right - pad_r;
    let plot_t = plot_bounds.top + pad_t;
    let plot_b = plot_bounds.bottom - pad_b;
    let plot_w = (plot_r - plot_l).max(40.0);
    let plot_h = (plot_b - plot_t).max(36.0);
    let inner_plot = D2D_RECT_F {
        left: plot_l,
        top: plot_t,
        right: plot_l + plot_w,
        bottom: plot_t + plot_h,
    };

    let bar_val = |it: &BarItem| -> f64 {
        if fresh_only {
            it.input + it.output
        } else {
            it.cache + it.input + it.output
        }
    };

    let raw_max = items.iter().map(bar_val).fold(0.0_f64, f64::max);
    let max_val = match unit {
        BarUnit::Tokens => raw_max.max(1.0),
        BarUnit::Usd => raw_max.max(0.001),
    };

    // 3 quiet horizontal rules (0%, 50%, 100%)
    for (i, frac) in [0.0_f32, 0.5, 1.0].iter().enumerate() {
        let y = plot_b - plot_h * *frac;
        draw_line(
            rt,
            plot_l,
            y,
            plot_r,
            y,
            if i == 0 { theme.border } else { theme.grid },
            1.0,
        );
        let tv = max_val * (*frac as f64);
        let axis_lbl = match unit {
            BarUnit::Tokens => crate::stats::fmt_compact(tv.round() as u64),
            BarUnit::Usd => crate::pricing::fmt_usd_axis(tv, max_val),
        };
        draw_text(
            rt,
            &axis_lbl,
            &fonts.mono_small_right,
            rect_f(plot_bounds.left, y - 7.0, pad_l - 8.0, 14.0),
            theme.muted,
        );
    }

    let n = items.len();
    let slot_w = plot_w / n as f32;
    let bar_w = if slot_w >= 6.0 {
        (slot_w * 0.56).clamp(3.0, 28.0)
    } else {
        (slot_w * 0.72).clamp(1.0, (slot_w - 0.5).max(1.0))
    };
    let label_every = editorial_bar_label_stride(plot_bounds, n);
    let mut slot_rects = Vec::with_capacity(n);

    let mut hover_idx: Option<usize> = None;
    if let Some((mx, my)) = mouse_pos {
        if mx >= plot_l && mx <= plot_r && my >= plot_t && my <= plot_b {
            let idx = ((mx - plot_l) / slot_w).floor() as isize;
            if idx >= 0 && (idx as usize) < n {
                hover_idx = Some(idx as usize);
            }
        }
    }

    for (idx, it) in items.iter().enumerate() {
        let slot_l = plot_l + idx as f32 * slot_w;
        slot_rects.push(rect_f(slot_l, plot_t, slot_w, plot_h));
        let cx = slot_l + slot_w * 0.5;
        let x0 = cx - bar_w * 0.5;
        let is_hov = hover_idx == Some(idx);

        if is_hov {
            fill_round_rect(
                rt,
                rect_f(slot_l + 1.0, plot_t, (slot_w - 2.0).max(2.0), plot_h),
                5.0,
                with_alpha(theme.accent, 0.07),
            );
        }

        let total = bar_val(it);
        if total > 0.0 {
            let total_h = ((total / max_val) as f32 * plot_h).max(3.0);
            let y_top = (plot_b - total_h).max(plot_t);

            let segs: [(f64, D2D1_COLOR_F); 3] = if fresh_only {
                [(0.0, theme.cache), (it.input, theme.accent), (it.output, theme.output)]
            } else {
                [(it.cache, theme.cache), (it.input, theme.accent), (it.output, theme.output)]
            };
            let last_nz = segs.iter().rposition(|(v, _)| *v > 0.0);
            let mut cur_y = plot_b;
            for (s_idx, &(val, col)) in segs.iter().enumerate() {
                if val <= 0.0 {
                    continue;
                }
                let sh = ((val / total) as f32 * total_h).max(2.0);
                let ny = if Some(s_idx) == last_nz {
                    y_top
                } else {
                    (cur_y - sh).max(y_top)
                };
                let r = rect_f(x0, ny, bar_w, (cur_y - ny).max(1.0));
                if Some(s_idx) == last_nz {
                    fill_round_rect(rt, r, (bar_w * 0.24).min(3.5), col);
                } else {
                    fill_rect(rt, r, col);
                }
                cur_y = ny;
            }
            if it.marker {
                let dot_y = (y_top - 5.0).max(plot_t + 2.0);
                fill_circle(rt, cx, dot_y, 2.3, theme.accent);
            }
        } else {
            // Subtle baseline dot/tick for 0-usage calendar days so the rhythm stays visible
            fill_round_rect(rt, rect_f(x0, plot_b - 2.0, bar_w, 2.0), 1.0, theme.grid);
        }

        let is_last = idx == n - 1;
        let show_regular = idx % label_every == 0
            && (label_every == 1 || (idx + ((label_every + 1) / 2) < n - 1));
        if is_last || show_regular {
            draw_text(
                rt,
                &it.label,
                &fonts.mono_small_center,
                rect_f(cx - 24.0, plot_b + 5.0, 48.0, 15.0),
                if is_hov { theme.text } else { theme.muted },
            );
        }
    }

    // Crisp editorial floating tooltip (built lazily only for the hovered bar)
    if let Some(idx) = hover_idx {
        let lines = tooltip_fn(idx);
        if !lines.is_empty() {
            let line_h = 16.0;
            let max_units: f32 = lines
                .iter()
                .map(|s| {
                    s.chars()
                        .map(|c| if c.is_ascii() { 1.0_f32 } else { 1.85_f32 })
                        .sum::<f32>()
                })
                .fold(0.0, f32::max);
            let box_w = (max_units * 6.8 + 28.0).clamp(224.0, (plot_bounds.right - plot_bounds.left - 16.0).min(360.0));
            let box_h = lines.len() as f32 * line_h + 16.0;
            let bar_r = plot_l + (idx as f32 + 1.0) * slot_w;
            let bar_l = plot_l + idx as f32 * slot_w;
            let tx = if bar_r + 10.0 + box_w <= plot_bounds.right - 4.0 {
                bar_r + 10.0
            } else {
                (bar_l - box_w - 10.0).max(plot_bounds.left + 4.0)
            };
            let ty = (plot_t + 2.0).min(plot_bounds.bottom - box_h - 4.0);
            let tr = rect_f(tx, ty, box_w, box_h);

            fill_round_rect(rt, rect_f(tx + 2.0, ty + 4.0, box_w, box_h), 8.0, rgba(0, 0, 0, 0.30 * theme.shadow_alpha));
            fill_round_rect(rt, tr, 8.0, theme.tooltip_bg);
            stroke_round_rect(rt, tr, 8.0, theme.border, 1.0);

            for (i, line) in lines.iter().enumerate() {
                draw_text(
                    rt,
                    line,
                    if i == 0 { &fonts.ui_section } else { &fonts.mono_small },
                    rect_f(tx + 12.0, ty + 8.0 + i as f32 * line_h, box_w - 24.0, line_h),
                    if i == 0 { theme.text } else { theme.sub },
                );
            }
        }
    }
    (slot_rects, Some(inner_plot))
}

/// Tactile quota / budget horizontal bar with an optional pace tick mark showing elapsed time fraction.
#[allow(clippy::too_many_arguments)]
pub fn draw_pace_bar(
    rt: &ID2D1RenderTarget,
    theme: &Theme,
    track: D2D_RECT_F,
    fill_pct: f32,
    pace_pct: Option<f32>,
    fill_col: D2D1_COLOR_F,
) {
    let h = (track.bottom - track.top).max(4.0);
    let w = (track.right - track.left).max(8.0);
    let r = h * 0.5;
    fill_round_rect(rt, track, r, theme.grid);
    let clamped = fill_pct.clamp(0.0, 100.0);
    if clamped > 0.3 {
        let fw = ((clamped / 100.0) * w).max(h.min(w));
        fill_round_rect(rt, rect_f(track.left, track.top, fw, h), r, fill_col);
    }
    if let Some(p) = pace_pct {
        let px = track.left + (p.clamp(0.0, 100.0) / 100.0) * w;
        let cy = track.top + h * 0.5;
        draw_line(rt, px, track.top - 2.0, px, track.bottom + 2.0, theme.text, 1.4);
        fill_circle(rt, px, cy, (h * 0.32).clamp(1.6, 2.6), theme.surface);
        stroke_circle(rt, px, cy, (h * 0.32).clamp(1.6, 2.6), theme.text, 1.1);
    }
}

#[derive(Debug, Clone)]
pub struct HeatmapDay {
    pub date_key: String,
    pub weekday_idx: usize, // 0 = Mon .. 6 = Sun
    pub week_col: usize,    // 0 .. n_weeks-1
    pub month_label: Option<String>,
    pub value: f64,
    pub calls: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct HeatmapGridGeom {
    pub gx0: f32,
    pub gy0: f32,
    pub cell: f32,
    pub gap: f32,
    pub n_weeks: usize,
}

/// Editorial GitHub-style 7-row (Mon..Sun) activity calendar heatmap in restrained terracotta tones.
/// Tooltips are built lazily only for the hovered day index.
/// Returns `((cell_rect, date_key) for every non-empty day cell, grid geometry for discrete hover)`.
#[allow(clippy::too_many_arguments)]
pub fn draw_activity_heatmap<F>(
    rt: &ID2D1RenderTarget,
    fonts: &DashFonts,
    theme: &Theme,
    bounds: D2D_RECT_F,
    n_weeks: usize,
    days: &[HeatmapDay],
    mouse_pos: Option<(f32, f32)>,
    tooltip_fn: F,
) -> (Vec<(D2D_RECT_F, String)>, Option<HeatmapGridGeom>)
where
    F: FnOnce(usize) -> Vec<String>,
{
    if n_weeks == 0 || days.is_empty() {
        draw_text(rt, "暂无活跃记录", &fonts.ui_reg_center, bounds, theme.muted);
        return (Vec::new(), None);
    }

    let left_lbl_w = 24.0;
    let top_lbl_h = 18.0;
    let bot_leg_h = 20.0;
    let avail_w = (bounds.right - bounds.left - left_lbl_w - 8.0).max(80.0);
    let avail_h = (bounds.bottom - bounds.top - top_lbl_h - bot_leg_h - 4.0).max(56.0);

    let gap = 3.5_f32;
    let cell_w = ((avail_w - gap * (n_weeks as f32 - 1.0)) / n_weeks as f32).floor();
    let cell_h = ((avail_h - gap * 6.0) / 7.0).floor();
    let cell = cell_w.min(cell_h).clamp(8.0, 22.0);
    let grid_w = n_weeks as f32 * cell + (n_weeks as f32 - 1.0) * gap;
    let grid_h = 7.0 * cell + 6.0 * gap;
    let gx0 = bounds.left + left_lbl_w + ((avail_w - grid_w) * 0.5).max(0.0);
    let gy0 = bounds.top + top_lbl_h + ((avail_h - grid_h) * 0.5).max(0.0);
    let geom = HeatmapGridGeom {
        gx0,
        gy0,
        cell,
        gap,
        n_weeks,
    };

    // Weekday row labels (Mon / Wed / Fri / Sun)
    for (row, lbl) in [(0usize, "一"), (2, "三"), (4, "五"), (6, "日")] {
        let ry = gy0 + row as f32 * (cell + gap);
        draw_text(
            rt,
            lbl,
            &fonts.ui_micro,
            rect_f(gx0 - 20.0, ry, 16.0, cell),
            theme.muted,
        );
    }

    // Positive quantile scale (log-softened so one mega-day doesn't wash out normal days)
    let max_val = days.iter().map(|d| d.value).fold(0.0_f64, f64::max);
    let log_max = (max_val + 1.0).ln().max(1e-6);
    let level_color = |val: f64| -> D2D1_COLOR_F {
        if val <= 0.0 {
            return if theme.is_dark {
                rgba(255, 255, 255, 0.045)
            } else {
                rgba(24, 24, 27, 0.055)
            };
        }
        let ratio = ((val + 1.0).ln() / log_max).clamp(0.0, 1.0);
        let alpha = if ratio < 0.28 {
            0.24
        } else if ratio < 0.54 {
            0.46
        } else if ratio < 0.80 {
            0.72
        } else {
            0.96
        };
        with_alpha(theme.accent, alpha)
    };

    let mut drawn_month_x: f32 = -100.0;
    let mut click_cells = Vec::new();
    let mut hovered: Option<(D2D_RECT_F, usize)> = None;
    let rad = (cell * 0.22).clamp(2.0, 4.0);

    for (idx, d) in days.iter().enumerate() {
        let cx = gx0 + d.week_col as f32 * (cell + gap);
        let cy = gy0 + d.weekday_idx as f32 * (cell + gap);
        if let Some(ref ml) = d.month_label {
            if cx - drawn_month_x >= 34.0 {
                draw_text(rt, ml, &fonts.mono_small, rect_f(cx, gy0 - 16.0, 38.0, 14.0), theme.muted);
                drawn_month_x = cx;
            }
        }
        let cr = rect_f(cx, cy, cell, cell);
        let is_hov = mouse_pos.map(|(mx, my)| hit_rect(&cr, mx, my)).unwrap_or(false);
        fill_round_rect(rt, cr, rad, level_color(d.value));
        if is_hov {
            stroke_round_rect(rt, cr, rad, theme.text, 1.2);
            hovered = Some((cr, idx));
        }
        if d.calls > 0 {
            click_cells.push((cr, d.date_key.clone()));
        }
    }

    // Bottom quiet intensity legend
    let leg_y = gy0 + grid_h + 6.0;
    let sq = 9.0;
    let leg_r = gx0 + grid_w;
    draw_text(rt, "多", &fonts.ui_micro, rect_f(leg_r - 14.0, leg_y - 1.0, 14.0, 13.0), theme.muted);
    let mut lx = leg_r - 18.0 - sq;
    for a in [0.96_f32, 0.72, 0.46, 0.24, 0.0] {
        let col = if a <= 0.01 {
            if theme.is_dark { rgba(255, 255, 255, 0.05) } else { rgba(24, 24, 27, 0.06) }
        } else {
            with_alpha(theme.accent, a)
        };
        fill_round_rect(rt, rect_f(lx, leg_y + 1.0, sq, sq), 2.0, col);
        lx -= sq + 3.0;
    }
    draw_text(rt, "少", &fonts.ui_micro, rect_f(lx - 14.0, leg_y - 1.0, 14.0, 13.0), theme.muted);
    draw_text(
        rt,
        "点击任意有数据方格可筛选当天会话",
        &fonts.ui_micro,
        rect_f(gx0, leg_y - 1.0, (lx - 24.0 - gx0).max(80.0), 13.0),
        theme.muted,
    );

    // Floating editorial tooltip (built lazily only for hovered day)
    if let Some((cr, idx)) = hovered {
        let lines = tooltip_fn(idx);
        if !lines.is_empty() {
            let line_h = 15.5;
            let max_units: f32 = lines
                .iter()
                .map(|s| s.chars().map(|c| if c.is_ascii() { 1.0_f32 } else { 1.82_f32 }).sum::<f32>())
                .fold(0.0, f32::max);
            let box_w = (max_units * 6.5 + 24.0).clamp(210.0, 320.0);
            let box_h = lines.len() as f32 * line_h + 14.0;
            let tx = if cr.right + 10.0 + box_w <= bounds.right - 4.0 {
                cr.right + 10.0
            } else {
                (cr.left - box_w - 10.0).max(bounds.left + 4.0)
            };
            let ty = (cr.top - 4.0).clamp(bounds.top + 2.0, (bounds.bottom - box_h - 4.0).max(bounds.top + 2.0));
            let tr = rect_f(tx, ty, box_w, box_h);
            fill_round_rect(rt, rect_f(tx + 2.0, ty + 3.0, box_w, box_h), 8.0, rgba(0, 0, 0, 0.28 * theme.shadow_alpha));
            fill_round_rect(rt, tr, 8.0, theme.tooltip_bg);
            stroke_round_rect(rt, tr, 8.0, theme.border, 1.0);
            for (i, line) in lines.iter().enumerate() {
                draw_text(
                    rt,
                    line,
                    if i == 0 { &fonts.ui_section } else { &fonts.mono_small },
                    rect_f(tx + 10.0, ty + 7.0 + i as f32 * line_h, box_w - 20.0, line_h),
                    if i == 0 { theme.text } else { theme.sub },
                );
            }
        }
    }

    (click_cells, Some(geom))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_with_utf16_stack_and_heap_paths() {
        assert_eq!(with_utf16("", |s| s.to_vec()), Vec::<u16>::new());
        let short = "Gauge 仪表盘 123";
        let expected_short: Vec<u16> = short.encode_utf16().collect();
        assert!(expected_short.len() < UTF16_STACK_CAP);
        assert_eq!(with_utf16(short, |s| s.to_vec()), expected_short);

        let long = "a".repeat(UTF16_STACK_CAP + 37);
        let expected_long: Vec<u16> = long.encode_utf16().collect();
        assert_eq!(with_utf16(&long, |s| s.to_vec()), expected_long);
    }

    #[test]
    fn test_measure_cache_hit_bounded_evict_and_clear() {
        let mut cache = MeasureCache::default();
        assert_eq!(cache.get(1, "hello"), None);
        cache.insert(1, "hello", 42.0);
        assert_eq!(cache.get(1, "hello"), Some(42.0));
        assert_eq!(cache.get(2, "hello"), None);

        // Fill up to MEASURE_CACHE_MAX and verify inserting one more resets cleanly
        for i in 0..MEASURE_CACHE_MAX {
            cache.insert(1, &format!("k{i}"), i as f32);
        }
        assert!(cache.total <= MEASURE_CACHE_MAX);
        cache.insert(1, "overflow-entry", 99.0);
        assert_eq!(cache.get(1, "overflow-entry"), Some(99.0));
        assert!(cache.total <= MEASURE_CACHE_MAX);

        cache.clear_and_shrink();
        assert_eq!(cache.total, 0);
        assert_eq!(cache.get(1, "overflow-entry"), None);
    }

    #[test]
    fn test_ring_arc_key_quantization_and_2slot_cache() {
        let (k1, q1) = RingArcKey::new(17.0, -PI * 0.5, 0.25 * 2.0 * PI);
        let (k2, _) = RingArcKey::new(17.0, -PI * 0.5, 0.2504 * 2.0 * PI);
        let (k3, _) = RingArcKey::new(17.0, -PI * 0.5, 0.50 * 2.0 * PI);
        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
        assert!((q1.to_degrees() - 90.0).abs() < 0.01);

        // Exercise actual D2D centered geometry cache (hit, 2-slot capacity, FIFO eviction, clear)
        if let Ok(fac) = D2DFactories::new() {
            let g1 = fac.get_or_build_ring_arc(17.0, -PI * 0.5, 0.25 * 2.0 * PI).expect("arc1");
            let g1_hit = fac.get_or_build_ring_arc(17.0, -PI * 0.5, 0.2503 * 2.0 * PI).expect("arc1 hit");
            assert_eq!(g1.as_raw(), g1_hit.as_raw());

            let g2 = fac.get_or_build_ring_arc(17.0, -PI * 0.5, 0.60 * 2.0 * PI).expect("arc2");
            assert_ne!(g1.as_raw(), g2.as_raw());
            // Both slots warm (CPU + RAM rings coexist without evicting each other)
            let g1_again = fac.get_or_build_ring_arc(17.0, -PI * 0.5, 0.25 * 2.0 * PI).expect("arc1 still cached");
            let g2_again = fac.get_or_build_ring_arc(17.0, -PI * 0.5, 0.60 * 2.0 * PI).expect("arc2 still cached");
            assert_eq!(g1.as_raw(), g1_again.as_raw());
            assert_eq!(g2.as_raw(), g2_again.as_raw());

            // 3rd distinct arc evicts slot 0 cleanly
            let _g3 = fac.get_or_build_ring_arc(34.0, -PI * 0.5, 0.80 * 2.0 * PI).expect("arc3");
            assert!(fac.ring_arc_cache.borrow().slots.iter().all(|s| s.is_some()));

            fac.clear_measure_cache();
            assert!(fac.ring_arc_cache.borrow().slots.iter().all(|s| s.is_none()));
        }
    }

    #[test]
    fn test_editorial_bar_label_stride() {
        let bounds = rect_f(0.0, 0.0, 566.0, 200.0); // plot_w = 500
        assert_eq!(editorial_bar_label_stride(bounds, 0), 1);
        assert_eq!(editorial_bar_label_stride(bounds, 10), 1);
        assert_eq!(editorial_bar_label_stride(bounds, 500), 40);
    }
}
