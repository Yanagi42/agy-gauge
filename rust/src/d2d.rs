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
    DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP,
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

    fn pick_installed_font(&self, candidates: &[PCWSTR], fallback: PCWSTR) -> PCWSTR {
        unsafe {
            let mut coll: Option<IDWriteFontCollection> = None;
            if self.dwrite.GetSystemFontCollection(&mut coll, false).is_ok() {
                if let Some(c) = coll {
                    for &name in candidates {
                        let mut idx = 0u32;
                        let mut exists = BOOL(0);
                        if c.FindFamilyName(name, &mut idx, &mut exists).is_ok() && exists.as_bool() {
                            return name;
                        }
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
        let ui = f.pick_installed_font(
            &[w!("Segoe UI Variable Text"), w!("Segoe UI Variable"), w!("Microsoft YaHei UI")],
            w!("Microsoft YaHei UI"),
        );
        let mono = f.pick_installed_font(
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
            mono_small_right: f.make_format(mono, 10.0, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, false)?,
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

    let start_angle = -PI * 0.5;
    let sweep = (clamped / 100.0) * 2.0 * PI;
    let end_angle = start_angle + sweep;
    let sx = cx + radius * start_angle.cos();
    let sy = cy + radius * start_angle.sin();
    let ex = cx + radius * end_angle.cos();
    let ey = cy + radius * end_angle.sin();

    unsafe {
        if let Ok(geo) = factories.d2d.CreatePathGeometry() {
            if let Ok(sink) = geo.Open() {
                sink.BeginFigure(pt(sx, sy), D2D1_FIGURE_BEGIN_HOLLOW);
                let seg = D2D1_ARC_SEGMENT {
                    point: pt(ex, ey),
                    size: D2D_SIZE_F { width: radius, height: radius },
                    rotationAngle: 0.0,
                    sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                    arcSize: if sweep > PI { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL },
                };
                sink.AddArc(&seg);
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
                if sink.Close().is_ok() {
                    if let Some(b) = brush(rt, arc_col) {
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

#[derive(Debug, Clone)]
pub struct BarItem {
    pub label: String,
    pub cache: u64,
    pub input: u64,
    pub output: u64,
    pub tooltip: Vec<String>,
}

/// Editorial minimalist stacked bar chart supporting Total vs Fresh-Only (non-cached) display mode.
#[allow(clippy::too_many_arguments)]
pub fn draw_editorial_bars(
    rt: &ID2D1RenderTarget,
    fonts: &Fonts,
    theme: &Theme,
    plot_bounds: D2D_RECT_F,
    items: &[BarItem],
    fresh_only: bool,
    mouse_pos: Option<(f32, f32)>,
    empty_msg: &str,
) {
    if items.is_empty() {
        draw_text(rt, empty_msg, &fonts.ui_reg_center, plot_bounds, theme.muted);
        return;
    }

    let pad_l = 48.0;
    let pad_r = 12.0;
    let pad_t = 12.0;
    let pad_b = 22.0;
    let plot_l = plot_bounds.left + pad_l;
    let plot_r = plot_bounds.right - pad_r;
    let plot_t = plot_bounds.top + pad_t;
    let plot_b = plot_bounds.bottom - pad_b;
    let plot_w = (plot_r - plot_l).max(40.0);
    let plot_h = (plot_b - plot_t).max(36.0);

    let bar_val = |it: &BarItem| -> u64 {
        if fresh_only {
            it.input + it.output
        } else {
            it.cache + it.input + it.output
        }
    };

    let max_val = items.iter().map(bar_val).max().unwrap_or(0).max(1);

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
        let tv = (max_val as f64 * *frac as f64).round() as u64;
        draw_text(
            rt,
            &crate::stats::fmt_compact(tv),
            &fonts.mono_small_right,
            rect_f(plot_bounds.left, y - 7.0, pad_l - 8.0, 14.0),
            theme.muted,
        );
    }

    let n = items.len();
    let slot_w = plot_w / n as f32;
    let bar_w = (slot_w * 0.52).clamp(4.0, 28.0);
    let label_every = (((40.0 * n as f32) / plot_w).ceil() as usize).max(1);

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
        if total > 0 {
            let total_h = ((total as f64 / max_val as f64) as f32 * plot_h).max(3.0);
            let y_top = (plot_b - total_h).max(plot_t);

            let segs: [(u64, D2D1_COLOR_F); 3] = if fresh_only {
                [(0, theme.cache), (it.input, theme.accent), (it.output, theme.output)]
            } else {
                [(it.cache, theme.cache), (it.input, theme.accent), (it.output, theme.output)]
            };
            let last_nz = segs.iter().rposition(|(v, _)| *v > 0);
            let mut cur_y = plot_b;
            for (s_idx, &(val, col)) in segs.iter().enumerate() {
                if val == 0 {
                    continue;
                }
                let sh = ((val as f64 / total as f64) as f32 * total_h).max(2.0);
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
        } else {
            // Subtle baseline dot/tick for 0-usage calendar days so the rhythm stays visible
            fill_round_rect(rt, rect_f(x0, plot_b - 2.0, bar_w, 2.0), 1.0, theme.grid);
        }

        if idx % label_every == 0 || idx == n - 1 {
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
            let box_w = 216.0;
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
}
