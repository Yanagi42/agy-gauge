//! Editorial Direct2D & DirectWrite design system: curated neutral themes (Obsidian & Warm Paper),
//! smart system font detection (Segoe UI Variable / Maple Mono / JetBrains Mono), radial arc gauges,
//! smooth area waveforms, and minimal interactive charts.

use std::f32::consts::PI;
use std::mem::{size_of, zeroed};
use std::ptr;

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED,
    D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN, D2D1_GRADIENT_STOP,
    D2D1_PIXEL_FORMAT, D2D_POINT_2F, D2D_RECT_F, D2D_SIZE_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1LinearGradientBrush,
    ID2D1RenderTarget, ID2D1SolidColorBrush, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_ARC_SEGMENT,
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

pub struct D2DFactories {
    pub d2d: ID2D1Factory,
    pub dwrite: IDWriteFactory,
}

impl D2DFactories {
    pub fn new() -> Result<Self> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            Ok(Self { d2d, dwrite })
        }
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
        let wide: Vec<u16> = text.encode_utf16().collect();
        if wide.is_empty() {
            return 0.0;
        }
        unsafe {
            if let Ok(layout) = self.dwrite.CreateTextLayout(&wide, fmt, 1000.0, 100.0) {
                let mut m: DWRITE_TEXT_METRICS = zeroed();
                if layout.GetMetrics(&mut m).is_ok() {
                    return m.widthIncludingTrailingWhitespace.ceil();
                }
            }
        }
        (text.chars().count() as f32 * 7.5).ceil()
    }
}

/// Editorial typography palette with automatic preference for Segoe UI Variable & Maple/JetBrains Mono.
pub struct Fonts {
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
    pub pill_tok: IDWriteTextFormat,
    pub pill_num: IDWriteTextFormat,
    pub pill_micro: IDWriteTextFormat,
}

impl Fonts {
    pub fn new(f: &D2DFactories) -> Result<Self> {
        let coll = f.get_font_collection();
        let ui = D2DFactories::pick_from_collection(
            coll.as_ref(),
            &[w!("Segoe UI Variable Text"), w!("Segoe UI Variable"), w!("Microsoft YaHei UI")],
            w!("Microsoft YaHei UI"),
        );
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
            pill_tok: f.make_format(mono, 12.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, false)?,
            pill_num: f.make_format(mono, 10.5, DWRITE_FONT_WEIGHT_MEDIUM, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, false)?,
            pill_micro: f.make_format(ui, 9.0, DWRITE_FONT_WEIGHT_MEDIUM, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, false)?,
        })
    }
}

/// Per-pixel-alpha software Direct2D surface for the floating pill (zero GPU heap!).
pub struct LayeredSurface {
    dc: HDC,
    bmp: HBITMAP,
    old_obj: HGDIOBJ,
    rt: ID2D1DCRenderTarget,
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
            let mut bi: BITMAPINFO = zeroed();
            bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB.0;

            let mut bits: *mut std::ffi::c_void = ptr::null_mut();
            let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, None, 0)?;
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
            let rt = factories.d2d.CreateDCRenderTarget(&props)?;
            rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);

            Ok(Self {
                dc,
                bmp,
                old_obj,
                rt,
                width_px: w,
                height_px: h,
                dpi,
            })
        }
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
            let sub = RECT {
                left: 0,
                top: 0,
                right: self.width_px,
                bottom: self.height_px,
            };
            self.rt.BindDC(self.dc, &sub)?;
            self.rt.BeginDraw();
            self.rt.Clear(Some(&rgba(0, 0, 0, 0.0)));
            let base: ID2D1RenderTarget = windows::core::Interface::cast(&self.rt)?;
            draw_fn(&base);
            self.rt.EndDraw(None, None)?;

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
    rt: ID2D1DCRenderTarget,
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
            let mut bi: BITMAPINFO = zeroed();
            bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB.0;

            let mut bits: *mut std::ffi::c_void = ptr::null_mut();
            let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, None, 0)?;
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
            let rt = factories.d2d.CreateDCRenderTarget(&props)?;
            rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);

            Ok(Self {
                dc,
                bmp,
                old_obj,
                rt,
                width_px: w,
                height_px: h,
                dpi,
            })
        }
    }

    pub fn matches(&self, width_px: i32, height_px: i32, dpi: f32) -> bool {
        self.width_px == width_px && self.height_px == height_px && (self.dpi - dpi).abs() < 0.5
    }

    pub fn render_and_blit<F>(&self, target_hdc: Option<HDC>, draw_fn: F) -> Result<()>
    where
        F: FnOnce(&ID2D1RenderTarget),
    {
        unsafe {
            let sub = RECT {
                left: 0,
                top: 0,
                right: self.width_px,
                bottom: self.height_px,
            };
            self.rt.BindDC(self.dc, &sub)?;
            self.rt.BeginDraw();
            let base: ID2D1RenderTarget = windows::core::Interface::cast(&self.rt)?;
            draw_fn(&base);
            self.rt.EndDraw(None, None)?;

            if let Some(hdc) = target_hdc {
                let _ = BitBlt(hdc, 0, 0, self.width_px, self.height_px, self.dc, 0, 0, SRCCOPY);
            }
            Ok(())
        }
    }
}

impl Drop for WindowSurface {
    fn drop(&mut self) {
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

pub fn brush(rt: &ID2D1RenderTarget, color: D2D1_COLOR_F) -> Option<ID2D1SolidColorBrush> {
    unsafe { rt.CreateSolidColorBrush(&color, None).ok() }
}

pub fn linear_v_brush(
    rt: &ID2D1RenderTarget,
    y0: f32,
    y1: f32,
    c0: D2D1_COLOR_F,
    c1: D2D1_COLOR_F,
) -> Option<ID2D1LinearGradientBrush> {
    unsafe {
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
        rt.CreateLinearGradientBrush(&props, None, &coll).ok()
    }
}

pub fn fill_round_rect(rt: &ID2D1RenderTarget, r: D2D_RECT_F, radius: f32, color: D2D1_COLOR_F) {
    if let Some(b) = brush(rt, color) {
        let rr = D2D1_ROUNDED_RECT {
            rect: r,
            radiusX: radius,
            radiusY: radius,
        };
        unsafe { rt.FillRoundedRectangle(&rr, &b) };
    }
}

pub fn stroke_round_rect(
    rt: &ID2D1RenderTarget,
    r: D2D_RECT_F,
    radius: f32,
    color: D2D1_COLOR_F,
    width: f32,
) {
    if let Some(b) = brush(rt, color) {
        let rr = D2D1_ROUNDED_RECT {
            rect: r,
            radiusX: radius,
            radiusY: radius,
        };
        unsafe { rt.DrawRoundedRectangle(&rr, &b, width, None) };
    }
}

pub fn fill_circle(rt: &ID2D1RenderTarget, cx: f32, cy: f32, r: f32, color: D2D1_COLOR_F) {
    if let Some(b) = brush(rt, color) {
        let ell = D2D1_ELLIPSE {
            point: pt(cx, cy),
            radiusX: r,
            radiusY: r,
        };
        unsafe { rt.FillEllipse(&ell, &b) };
    }
}

pub fn stroke_circle(rt: &ID2D1RenderTarget, cx: f32, cy: f32, r: f32, color: D2D1_COLOR_F, width: f32) {
    if let Some(b) = brush(rt, color) {
        let ell = D2D1_ELLIPSE {
            point: pt(cx, cy),
            radiusX: r,
            radiusY: r,
        };
        unsafe { rt.DrawEllipse(&ell, &b, width, None) };
    }
}

pub fn draw_line(rt: &ID2D1RenderTarget, x0: f32, y0: f32, x1: f32, y1: f32, color: D2D1_COLOR_F, width: f32) {
    if let Some(b) = brush(rt, color) {
        unsafe { rt.DrawLine(pt(x0, y0), pt(x1, y1), &b, width, None) };
    }
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
    if let Some(b) = brush(rt, color) {
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            rt.DrawText(
                &wide,
                fmt,
                &r,
                &b,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }
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
    let end_rad = start_rad + sweep_rad;
    let sx = cx + radius * start_rad.cos();
    let sy = cy + radius * start_rad.sin();
    let ex = cx + radius * end_rad.cos();
    let ey = cy + radius * end_rad.sin();

    unsafe {
        if let Ok(geo) = factories.d2d.CreatePathGeometry() {
            if let Ok(sink) = geo.Open() {
                sink.BeginFigure(pt(sx, sy), D2D1_FIGURE_BEGIN_HOLLOW);
                let seg = D2D1_ARC_SEGMENT {
                    point: pt(ex, ey),
                    size: D2D_SIZE_F { width: radius, height: radius },
                    rotationAngle: 0.0,
                    sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                    arcSize: if sweep_rad > PI { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL },
                };
                sink.AddArc(&seg);
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
                if sink.Close().is_ok() {
                    if let Some(b) = brush(rt, color) {
                        let sp = D2D1_STROKE_STYLE_PROPERTIES {
                            startCap: D2D1_CAP_STYLE_ROUND,
                            endCap: D2D1_CAP_STYLE_ROUND,
                            ..Default::default()
                        };
                        let style = factories.d2d.CreateStrokeStyle(&sp, None).ok();
                        rt.DrawGeometry(&geo, &b, stroke_w, style.as_ref());
                    }
                }
            }
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

    // 3) 270 deg track arc: 135 deg -> 45 deg clockwise
    let start_rad = 0.75 * PI; // 135 deg
    draw_arc_rad(
        factories,
        rt,
        cx,
        cy,
        3.85,
        1.45,
        start_rad,
        1.5 * PI, // 270 deg
        rgb(210, 206, 195),
    );

    // 4) Primary reading arc (~71.5%): 135 deg -> -32 deg (193 deg sweep)
    let sweep_reading = 193.0_f32.to_radians();
    draw_arc_rad(factories, rt, cx, cy, 3.85, 1.45, start_rad, sweep_reading, arc_col);

    // 5) Deep ink pointer + hub + core dot
    let ink = rgb(26, 26, 28);
    unsafe {
        if let Some(b) = brush(rt, ink) {
            let sp = D2D1_STROKE_STYLE_PROPERTIES {
                startCap: D2D1_CAP_STYLE_ROUND,
                endCap: D2D1_CAP_STYLE_ROUND,
                ..Default::default()
            };
            let style = factories.d2d.CreateStrokeStyle(&sp, None).ok();
            rt.DrawLine(
                pt(cx, cy + 0.2),
                pt(cx + 2.33, cy - 1.26),
                &b,
                1.05,
                style.as_ref(),
            );
        }
    }
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
                    if let Some(b) = brush(rt, line_col) {
                        let sp = D2D1_STROKE_STYLE_PROPERTIES {
                            startCap: D2D1_CAP_STYLE_ROUND,
                            endCap: D2D1_CAP_STYLE_ROUND,
                            lineJoin: D2D1_LINE_JOIN_ROUND,
                            ..Default::default()
                        };
                        let style = factories.d2d.CreateStrokeStyle(&sp, None).ok();
                        rt.DrawGeometry(&line_geo, &b, stroke_w, style.as_ref());
                    }
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
    pub label: String,
    pub cache: f64,
    pub input: f64,
    pub output: f64,
    pub marker: bool,
    pub tooltip: Vec<String>,
}

/// Editorial minimalist stacked bar chart supporting Total vs Fresh-Only (non-cached)
/// and both Token and USD display units.
#[allow(clippy::too_many_arguments)]
pub fn draw_editorial_bars(
    rt: &ID2D1RenderTarget,
    fonts: &Fonts,
    theme: &Theme,
    plot_bounds: D2D_RECT_F,
    items: &[BarItem],
    fresh_only: bool,
    unit: BarUnit,
    mouse_pos: Option<(f32, f32)>,
    empty_msg: &str,
) -> Vec<D2D_RECT_F> {
    if items.is_empty() {
        draw_text(rt, empty_msg, &fonts.ui_reg_center, plot_bounds, theme.muted);
        return Vec::new();
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
    let label_every = (((40.0 * n as f32) / plot_w).ceil() as usize).max(1);
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
                } else if let Some(b) = brush(rt, col) {
                    unsafe { rt.FillRectangle(&r, &b) };
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

    // Crisp editorial floating tooltip
    if let Some(idx) = hover_idx {
        let lines = &items[idx].tooltip;
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
    slot_rects
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
    pub tooltip: Vec<String>,
}

/// Editorial GitHub-style 7-row (Mon..Sun) activity calendar heatmap in restrained terracotta tones.
/// Returns `(cell_rect, date_key)` for every non-empty day cell so callers can wire drill-down clicks.
#[allow(clippy::too_many_arguments)]
pub fn draw_activity_heatmap(
    rt: &ID2D1RenderTarget,
    fonts: &Fonts,
    theme: &Theme,
    bounds: D2D_RECT_F,
    n_weeks: usize,
    days: &[HeatmapDay],
    mouse_pos: Option<(f32, f32)>,
) -> Vec<(D2D_RECT_F, String)> {
    if n_weeks == 0 || days.is_empty() {
        draw_text(rt, "暂无活跃记录", &fonts.ui_reg_center, bounds, theme.muted);
        return Vec::new();
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
    let mut hovered: Option<(D2D_RECT_F, &HeatmapDay)> = None;
    let rad = (cell * 0.22).clamp(2.0, 4.0);

    for d in days {
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
            hovered = Some((cr, d));
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

    // Floating editorial tooltip
    if let Some((cr, d)) = hovered {
        let lines = &d.tooltip;
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

    click_cells
}
