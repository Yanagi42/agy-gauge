#![cfg_attr(not(test), windows_subsystem = "windows")]

//! Ultra-lightweight, strictly read-only multi-agent Token & System Monitor
//! (Antigravity CLI/App/IDE + Claude Code + OpenAI Codex CLI + Gemini CLI).
//!
//! Craft & Architecture:
//!   - Bottom-right per-pixel-alpha floating pill (live CPU sparkline + radial RAM ring)
//!     rendered via Direct2D software DC + `UpdateLayeredWindow` (~8-11 MB permanent RAM).
//!   - Curated Obsidian Charcoal & Warm Paper editorial themes (zero neon AI clichés).
//!   - Clicking launches an independent `--dashboard-only` process so closing the dashboard
//!     reclaims 100% of its memory back to the OS immediately.

mod d2d;
mod dashboard;
mod pill;
mod telemetry;

pub use gauge::{archive, launcher, pricing, quota, session_meta, stats};

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{Local, NaiveDate, Utc};

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, CreateFontW, CreateSolidBrush, DeleteObject, EndPaint,
    InvalidateRect, SetBkColor, SetTextColor, UpdateWindow, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DEFAULT_PITCH, FF_MODERN, FW_NORMAL, HBRUSH, HDC, HFONT, HGDIOBJ,
    OUT_DEFAULT_PRECIS, PAINTSTRUCT,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE,
    TRACKMOUSEEVENT, VK_BACK, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_F5, VK_MENU, VK_RETURN, VK_SHIFT,
    VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, IsIconic, IsWindowVisible, KillTimer, LoadCursorW,
    LoadImageW, MoveWindow, PostMessageW, PostQuitMessage, RegisterClassExW, SendMessageW,
    SetCursor, SetForegroundWindow, SetTimer, SetWindowPos, SetWindowTextW, ShowWindow,
    TrackPopupMenu, TranslateMessage, CS_HREDRAW, CS_VREDRAW, ES_AUTOHSCROLL, HICON, HMENU,
    HWND_NOTOPMOST, HWND_TOPMOST, ICON_BIG, ICON_SMALL, IDC_ARROW, IDC_HAND, IMAGE_ICON,
    LR_DEFAULTCOLOR, MF_GRAYED, MF_SEPARATOR, MF_STRING, MINMAXINFO, MSG, SM_CXICON, SM_CXSMICON,
    SM_CYICON, SM_CYSMICON, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_RESTORE,
    SW_SHOW, SW_SHOWNOACTIVATE, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_TOPALIGN, WINDOWPOS,
    WM_ACTIVATEAPP, WM_CAPTURECHANGED, WM_CLOSE, WM_COMMAND, WM_CTLCOLOREDIT, WM_DESTROY,
    WM_DPICHANGED, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONUP, WM_SETCURSOR, WM_SETFONT, WM_SETICON,
    WM_SIZE, WM_TIMER, WM_WINDOWPOSCHANGING, WNDCLASSEXW, WS_CHILD, WS_CLIPCHILDREN, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_TABSTOP,
    WS_VISIBLE,
};

const WM_APP_SYNC_META: u32 = WM_APP_TRAY + 1;
const RES_ICON_NORMAL: u16 = 1;
const RES_ICON_WARN: u16 = 2;
const RES_ICON_CRIT: u16 = 3;

const CMD_SESS_RESUME: u32 = 6201;
const CMD_SESS_RESUME_PARENT: u32 = 6202;
const CMD_SESS_INSPECT_PARENT: u32 = 6203;
const CMD_SESS_COPY_CMD: u32 = 6204;
const CMD_SESS_OPEN_WS: u32 = 6205;
const CMD_SESS_TOGGLE_PIN: u32 = 6206;
const CMD_SESS_SHOW_CALLS: u32 = 6207;
const CMD_SESS_COPY_DETAIL: u32 = 6208;
const CMD_SESS_COPY_ID: u32 = 6209;
const CMD_SESS_SHOW_SUBS: u32 = 6210;

const WM_SETTINGCHANGE: u32 = 0x001A;
const SPI_SETWORKAREA: usize = 0x002F;
const WM_DISPLAYCHANGE: u32 = 0x007E;
const WM_SYSCOMMAND: u32 = 0x0112;
const SC_MINIMIZE: usize = 0xF020;
const SIZE_MINIMIZED: usize = 1;
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_PRINTCLIENT: u32 = 0x0318;
const EM_SETSEL: u32 = 0x00B1;
const EM_SETCUEBANNER: u32 = 0x1501;
const EN_SETFOCUS: u16 = 0x0100;
const EN_KILLFOCUS: u16 = 0x0200;
const EN_CHANGE: u16 = 0x0300;
const IDC_PRICING_EDIT_BASE: usize = 5000;
const IDC_SEARCH_EDIT: usize = 5100;
const IDC_DATE_EDIT_BASE: usize = 5200;
const CMD_EXPORT_JSON: u32 = 6101;
const CMD_EXPORT_SESSIONS_CSV: u32 = 6102;
const CMD_EXPORT_DAILY_CSV: u32 = 6103;
const CMD_EXPORT_STATUSLINE_CFG: u32 = 6104;
const CMD_EXPORT_CALLS_CSV: u32 = 6105;

use d2d::{theme_for, D2DFactories, Fonts};
use dashboard::{
    fmt_relative_time_cn, BreakdownMode, ClickAction, DailyViewMode, DashboardView, ModelViewMode,
    OverviewTopMode, Tab, WeeklyMode,
};
use pill::{
    show_context_menu, PillRenderer, QuickSessionMenuItem, CMD_MONITOR_BASE, CMD_OPEN_DETAIL,
    CMD_OPEN_SESSIONS_TAB, CMD_PILL_ITEMS_RESET, CMD_PILL_ITEM_BASE, CMD_QUICK_RESUME_BASE,
    CMD_QUIT, CMD_REDOCK, CMD_REFRESH, CMD_TOGGLE_AUTOSTART, CMD_TOGGLE_HIDE_FULLSCREEN,
    CMD_TOGGLE_HUMAN, CMD_TOGGLE_LIGHT, CMD_TOGGLE_NOTIFICATIONS, CMD_TOGGLE_PILL_VISIBLE,
    CMD_TOGGLE_PRIVACY, CMD_TOGGLE_TOPMOST, CMD_TOGGLE_TRAY_HOVER,
};
use pricing::ModelRate;
use stats::{DateScope, IncrementalScanner, ScanSnapshot};
use telemetry::{
    apply_pill_topmost, apply_win11_frame, broadcast_prefs_sync, clamp_pos_to_monitors,
    clamp_pos_to_work_area, copy_text_to_clipboard, dpi_for_point, enable_dpi_awareness,
    get_work_area, guard_topmost_windowpos, inspect_pill_topmost_health, is_autostart_enabled,
    is_cursor_in_tray_hover_zone, is_other_app_fullscreen, load_user_prefs,
    monitor_index_for_point, prompt_save_file, read_self_private_mb, read_self_working_set_mb,
    register_taskbar_created_msg, save_user_prefs, set_autostart_enabled,
    set_self_tray_icon_taskbar_promoted, to_wide_null, trim_self_working_set, wide_pcwstr,
    window_dpi_scale, SingleInstanceGuard, SystemTelemetry, TopmostHealth, TrayIcon, TrayPinResult,
    UserPrefs, PILL_ITEMS_ALL_MASK, PILL_ITEMS_DEFAULT, PILL_ITEM_CPU, PILL_ITEM_RAM,
    PILL_ITEM_TOTAL, WM_APP_SYNC_PREFS, WM_APP_TRAY,
};

const TIMER_TELEMETRY: usize = 1;
const TIMER_TOKENS: usize = 2;
const TIMER_TRAY_HOVER: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditTarget {
    Five(usize, u8),
    Search,
    Date(usize),
}

struct PricingEditControls {
    edits: [HWND; 5],
    search_edit: HWND,
    date_edits: [HWND; 2],
    font: HFONT,
    font_dpi: i32,
    bg_brush: HBRUSH,
    brush_light: bool,
    visible: bool,
    active_count: usize,
    editor_mode: u8, // 0 = none, 1 = pricing (5 fields), 2 = budget (4 fields)
    initialized_values: bool,
    last_rects_px: [(i32, i32, i32, i32); 5],
    search_visible: bool,
    suppress_search_change: bool,
    last_search_rect_px: (i32, i32, i32, i32),
    date_visible: bool,
    date_initialized: bool,
    last_date_rects_px: [(i32, i32, i32, i32); 2],
}

impl Default for PricingEditControls {
    fn default() -> Self {
        Self {
            edits: [HWND::default(); 5],
            search_edit: HWND::default(),
            date_edits: [HWND::default(); 2],
            font: HFONT::default(),
            font_dpi: 0,
            bg_brush: HBRUSH::default(),
            brush_light: false,
            visible: false,
            active_count: 0,
            editor_mode: 0,
            initialized_values: false,
            last_rects_px: [(0, 0, 0, 0); 5],
            search_visible: false,
            suppress_search_change: false,
            last_search_rect_px: (0, 0, 0, 0),
            date_visible: false,
            date_initialized: false,
            last_date_rects_px: [(0, 0, 0, 0); 2],
        }
    }
}

impl Drop for PricingEditControls {
    fn drop(&mut self) {
        unsafe {
            if !self.font.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
            if !self.bg_brush.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.bg_brush.0));
            }
        }
    }
}

impl PricingEditControls {
    fn input_colors(light: bool) -> (COLORREF, COLORREF) {
        if light {
            // bg #F6F4F1, text #1A1A1C (COLORREF is 0x00BBGGRR)
            (COLORREF(0x00F1F4F6), COLORREF(0x001C1A1A))
        } else {
            // bg #141416, text #EDEDEF
            (COLORREF(0x00161414), COLORREF(0x00EFEDED))
        }
    }

    fn all_hwnds(&self) -> [HWND; 8] {
        [
            self.edits[0],
            self.edits[1],
            self.edits[2],
            self.edits[3],
            self.edits[4],
            self.search_edit,
            self.date_edits[0],
            self.date_edits[1],
        ]
    }

    fn ensure_brush(&mut self, light: bool) {
        if !self.bg_brush.0.is_null() && self.brush_light == light {
            return;
        }
        unsafe {
            if !self.bg_brush.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.bg_brush.0));
            }
            let (bg, _) = Self::input_colors(light);
            self.bg_brush = CreateSolidBrush(bg);
            self.brush_light = light;
            for h in self.all_hwnds() {
                if !h.0.is_null() {
                    let _ = InvalidateRect(h, None, true);
                }
            }
        }
    }

    fn ensure_font(&mut self, dpi: f32) {
        let dpi_i = dpi.round() as i32;
        if !self.font.0.is_null() && self.font_dpi == dpi_i {
            return;
        }
        unsafe {
            if !self.font.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
            let height = -((13.5 * dpi / 96.0).round() as i32).max(11);
            self.font = CreateFontW(
                height,
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 | FF_MODERN.0) as u32,
                w!("Consolas"),
            );
            self.font_dpi = dpi_i;
            for h in self.all_hwnds() {
                if !h.0.is_null() {
                    let _ = SendMessageW(h, WM_SETFONT, WPARAM(self.font.0 as usize), LPARAM(1));
                }
            }
        }
    }

    fn create_single_edit(&self, parent: HWND, id: usize) -> Option<HWND> {
        unsafe {
            let hinst = GetModuleHandleW(None).unwrap_or_default();
            let style = WS_CHILD
                | WS_TABSTOP
                | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(ES_AUTOHSCROLL as u32);
            let h = CreateWindowExW(
                Default::default(),
                w!("EDIT"),
                w!(""),
                style,
                0,
                0,
                10,
                10,
                parent,
                HMENU(id as *mut core::ffi::c_void),
                hinst,
                None,
            )
            .ok()?;
            if !self.font.0.is_null() {
                let _ = SendMessageW(h, WM_SETFONT, WPARAM(self.font.0 as usize), LPARAM(0));
            }
            let _ = WS_VISIBLE;
            Some(h)
        }
    }

    fn ensure_created(&mut self, parent: HWND, dpi: f32, light: bool) {
        self.ensure_brush(light);
        self.ensure_font(dpi);
        if !self.edits[0].0.is_null() {
            return;
        }
        for i in 0..5 {
            if let Some(h) = self.create_single_edit(parent, IDC_PRICING_EDIT_BASE + i) {
                self.edits[i] = h;
            }
        }
    }

    fn ensure_search_created(&mut self, parent: HWND, dpi: f32, light: bool) {
        self.ensure_brush(light);
        self.ensure_font(dpi);
        if !self.search_edit.0.is_null() {
            return;
        }
        if let Some(h) = self.create_single_edit(parent, IDC_SEARCH_EDIT) {
            let cue = to_wide_null("搜索标题 / 模型 / 工具 / 来源 (/)");
            unsafe {
                let _ = SendMessageW(h, EM_SETCUEBANNER, WPARAM(1), LPARAM(cue.as_ptr() as isize));
            }
            self.search_edit = h;
        }
    }

    fn ensure_date_created(&mut self, parent: HWND, dpi: f32, light: bool) {
        self.ensure_brush(light);
        self.ensure_font(dpi);
        if !self.date_edits[0].0.is_null() {
            return;
        }
        for i in 0..2 {
            if let Some(h) = self.create_single_edit(parent, IDC_DATE_EDIT_BASE + i) {
                let cue = to_wide_null("YYYY-MM-DD");
                unsafe {
                    let _ = SendMessageW(h, EM_SETCUEBANNER, WPARAM(1), LPARAM(cue.as_ptr() as isize));
                }
                self.date_edits[i] = h;
            }
        }
    }

    fn sync_layout(
        &mut self,
        parent: HWND,
        dpi: f32,
        light: bool,
        rects_dip: Option<&[D2D_RECT_F]>,
    ) {
        let Some(rects) = rects_dip else {
            self.hide_five_edits(parent);
            return;
        };
        let count = rects.len().min(5);
        if count == 0 {
            self.hide_five_edits(parent);
            return;
        }
        self.ensure_created(parent, dpi, light);
        let scale = dpi / 96.0;
        let prev_count = self.active_count;
        for (i, r) in rects.iter().take(count).enumerate() {
            let h = self.edits[i];
            if h.0.is_null() {
                continue;
            }
            let x = (r.left * scale).round() as i32;
            let y = (r.top * scale).round() as i32;
            let w = ((r.right - r.left) * scale).round().max(12.0) as i32;
            let hh = ((r.bottom - r.top) * scale).round().max(12.0) as i32;
            let tuple = (x, y, w, hh);
            let was_shown = self.visible && i < prev_count;
            if !was_shown || self.last_rects_px[i] != tuple {
                unsafe {
                    let _ = MoveWindow(h, x, y, w, hh, true);
                }
                self.last_rects_px[i] = tuple;
            }
            if !was_shown {
                unsafe {
                    let _ = ShowWindow(h, SW_SHOW);
                }
            }
        }
        // Hide any trailing edits beyond `count` (e.g. 5th edit when switching Pricing -> 4-field Budget)
        for i in count..5 {
            let h = self.edits[i];
            if !h.0.is_null() {
                unsafe {
                    if GetFocus() == h && !parent.0.is_null() {
                        let _ = SetFocus(parent);
                    }
                    let _ = ShowWindow(h, SW_HIDE);
                }
            }
            self.last_rects_px[i] = (0, 0, 0, 0);
        }
        self.visible = true;
        self.active_count = count;
    }

    fn sync_search_layout(
        &mut self,
        parent: HWND,
        dpi: f32,
        light: bool,
        rect_dip: Option<D2D_RECT_F>,
    ) {
        let Some(r) = rect_dip else {
            if self.search_visible && !self.search_edit.0.is_null() {
                unsafe {
                    if GetFocus() == self.search_edit && !parent.0.is_null() {
                        let _ = SetFocus(parent);
                    }
                    let _ = ShowWindow(self.search_edit, SW_HIDE);
                }
                self.search_visible = false;
            }
            return;
        };
        self.ensure_search_created(parent, dpi, light);
        if self.search_edit.0.is_null() {
            return;
        }
        let scale = dpi / 96.0;
        let tuple = (
            (r.left * scale).round() as i32,
            (r.top * scale).round() as i32,
            ((r.right - r.left) * scale).round().max(16.0) as i32,
            ((r.bottom - r.top) * scale).round().max(12.0) as i32,
        );
        if !self.search_visible || self.last_search_rect_px != tuple {
            unsafe {
                let _ = MoveWindow(self.search_edit, tuple.0, tuple.1, tuple.2, tuple.3, true);
            }
            self.last_search_rect_px = tuple;
        }
        if !self.search_visible {
            unsafe {
                let _ = ShowWindow(self.search_edit, SW_SHOW);
            }
        }
        self.search_visible = true;
    }

    fn sync_date_layout(
        &mut self,
        parent: HWND,
        dpi: f32,
        light: bool,
        rects_dip: Option<[D2D_RECT_F; 2]>,
    ) {
        let Some(rects) = rects_dip else {
            if self.date_visible {
                unsafe {
                    let foc = GetFocus();
                    let had_focus = self.date_index_of(foc).is_some();
                    for h in self.date_edits {
                        if !h.0.is_null() {
                            let _ = ShowWindow(h, SW_HIDE);
                        }
                    }
                    if had_focus && !parent.0.is_null() {
                        let _ = SetFocus(parent);
                    }
                }
                self.date_visible = false;
            }
            return;
        };
        self.ensure_date_created(parent, dpi, light);
        let scale = dpi / 96.0;
        for (i, r) in rects.iter().enumerate() {
            let h = self.date_edits[i];
            if h.0.is_null() {
                continue;
            }
            let tuple = (
                (r.left * scale).round() as i32,
                (r.top * scale).round() as i32,
                ((r.right - r.left) * scale).round().max(16.0) as i32,
                ((r.bottom - r.top) * scale).round().max(12.0) as i32,
            );
            if !self.date_visible || self.last_date_rects_px[i] != tuple {
                unsafe {
                    let _ = MoveWindow(h, tuple.0, tuple.1, tuple.2, tuple.3, true);
                }
                self.last_date_rects_px[i] = tuple;
            }
            if !self.date_visible {
                unsafe {
                    let _ = ShowWindow(h, SW_SHOW);
                }
            }
        }
        self.date_visible = true;
    }

    fn hide_five_edits(&mut self, parent: HWND) {
        if !self.visible {
            self.editor_mode = 0;
            self.active_count = 0;
            return;
        }
        unsafe {
            let foc = GetFocus();
            let had_focus = self.index_of(foc).is_some();
            for h in self.edits {
                if !h.0.is_null() {
                    let _ = ShowWindow(h, SW_HIDE);
                }
            }
            if had_focus && !parent.0.is_null() {
                let _ = SetFocus(parent);
            }
        }
        self.visible = false;
        self.active_count = 0;
        self.editor_mode = 0;
    }

    fn hide_all(&mut self, parent: HWND) {
        self.hide_five_edits(parent);
        if self.search_visible && !self.search_edit.0.is_null() {
            unsafe {
                if GetFocus() == self.search_edit && !parent.0.is_null() {
                    let _ = SetFocus(parent);
                }
                let _ = ShowWindow(self.search_edit, SW_HIDE);
            }
            self.search_visible = false;
        }
        if self.date_visible {
            unsafe {
                let foc = GetFocus();
                let had_focus = self.date_index_of(foc).is_some();
                for h in self.date_edits {
                    if !h.0.is_null() {
                        let _ = ShowWindow(h, SW_HIDE);
                    }
                }
                if had_focus && !parent.0.is_null() {
                    let _ = SetFocus(parent);
                }
            }
            self.date_visible = false;
        }
    }

    fn reset_handles(&mut self) {
        self.edits = [HWND::default(); 5];
        self.search_edit = HWND::default();
        self.date_edits = [HWND::default(); 2];
        self.visible = false;
        self.active_count = 0;
        self.editor_mode = 0;
        self.initialized_values = false;
        self.last_rects_px = [(0, 0, 0, 0); 5];
        self.search_visible = false;
        self.suppress_search_change = false;
        self.last_search_rect_px = (0, 0, 0, 0);
        self.date_visible = false;
        self.date_initialized = false;
        self.last_date_rects_px = [(0, 0, 0, 0); 2];
    }

    fn read_hwnd_text(h: HWND) -> String {
        if h.0.is_null() {
            return String::new();
        }
        unsafe {
            let len = GetWindowTextLengthW(h);
            if len <= 0 {
                return String::new();
            }
            let mut buf = vec![0u16; (len + 1) as usize];
            let got = GetWindowTextW(h, &mut buf);
            String::from_utf16_lossy(&buf[..got as usize])
        }
    }

    fn set_values(&mut self, vals: &[String; 5]) {
        for (i, v) in vals.iter().enumerate() {
            let h = self.edits[i];
            if !h.0.is_null() {
                let wide = to_wide_null(v);
                unsafe {
                    let _ = SetWindowTextW(h, wide_pcwstr(&wide));
                }
            }
        }
        self.initialized_values = true;
    }

    fn set_budget_values(&mut self, vals: &[String; 4]) {
        for (i, v) in vals.iter().enumerate() {
            let h = self.edits[i];
            if !h.0.is_null() {
                let wide = to_wide_null(v);
                unsafe {
                    let _ = SetWindowTextW(h, wide_pcwstr(&wide));
                }
            }
        }
        if !self.edits[4].0.is_null() {
            unsafe {
                let _ = SetWindowTextW(self.edits[4], w!(""));
            }
        }
        self.initialized_values = true;
    }

    fn read_values(&self) -> [String; 5] {
        std::array::from_fn(|i| Self::read_hwnd_text(self.edits[i]))
    }

    fn set_search_text(&mut self, val: &str) {
        if self.search_edit.0.is_null() {
            return;
        }
        self.suppress_search_change = true;
        let wide = to_wide_null(val);
        unsafe {
            let _ = SetWindowTextW(self.search_edit, wide_pcwstr(&wide));
        }
        self.suppress_search_change = false;
    }

    fn read_search_text(&self) -> String {
        Self::read_hwnd_text(self.search_edit)
    }

    fn set_date_texts(&mut self, vals: &[String; 2]) {
        for (i, v) in vals.iter().enumerate() {
            let h = self.date_edits[i];
            if !h.0.is_null() {
                let wide = to_wide_null(v);
                unsafe {
                    let _ = SetWindowTextW(h, wide_pcwstr(&wide));
                }
            }
        }
        self.date_initialized = true;
    }

    fn read_date_texts(&self) -> [String; 2] {
        [
            Self::read_hwnd_text(self.date_edits[0]),
            Self::read_hwnd_text(self.date_edits[1]),
        ]
    }

    fn focus_field(&self, idx: usize) {
        if let Some(&h) = self.edits.get(idx) {
            if !h.0.is_null() {
                unsafe {
                    let _ = SetFocus(h);
                    let _ = SendMessageW(h, EM_SETSEL, WPARAM(0), LPARAM(-1));
                }
            }
        }
    }

    fn focus_search(&self) {
        if !self.search_edit.0.is_null() {
            unsafe {
                let _ = SetFocus(self.search_edit);
                let _ = SendMessageW(self.search_edit, EM_SETSEL, WPARAM(0), LPARAM(-1));
            }
        }
    }

    fn focus_date(&self, idx: usize) {
        if let Some(&h) = self.date_edits.get(idx) {
            if !h.0.is_null() {
                unsafe {
                    let _ = SetFocus(h);
                    let _ = SendMessageW(h, EM_SETSEL, WPARAM(0), LPARAM(-1));
                }
            }
        }
    }

    fn index_of(&self, hwnd: HWND) -> Option<usize> {
        if hwnd.0.is_null() {
            return None;
        }
        let n = self.active_count.min(5);
        self.edits[..n].iter().position(|&h| h == hwnd)
    }

    fn is_search_hwnd(&self, hwnd: HWND) -> bool {
        !hwnd.0.is_null() && hwnd == self.search_edit
    }

    fn date_index_of(&self, hwnd: HWND) -> Option<usize> {
        if hwnd.0.is_null() {
            return None;
        }
        self.date_edits.iter().position(|&h| h == hwnd)
    }

    fn classify_hwnd(&self, hwnd: HWND) -> Option<EditTarget> {
        if let Some(idx) = self.index_of(hwnd) {
            Some(EditTarget::Five(idx, self.editor_mode))
        } else if self.is_search_hwnd(hwnd) {
            Some(EditTarget::Search)
        } else {
            self.date_index_of(hwnd).map(EditTarget::Date)
        }
    }

    fn handle_ctlcolor(&mut self, hdc: HDC, light: bool) -> LRESULT {
        self.ensure_brush(light);
        let (bg, fg) = Self::input_colors(light);
        unsafe {
            SetTextColor(hdc, fg);
            SetBkColor(hdc, bg);
        }
        LRESULT(self.bg_brush.0 as isize)
    }
}

fn load_res_icon(res_id: u16, w_px: i32, h_px: i32) -> HICON {
    unsafe {
        let Ok(hinst) = GetModuleHandleW(None) else {
            return HICON::default();
        };
        LoadImageW(
            hinst,
            PCWSTR(res_id as usize as *const u16),
            IMAGE_ICON,
            w_px.max(16),
            h_px.max(16),
            LR_DEFAULTCOLOR,
        )
        .map(|h| HICON(h.0))
        .unwrap_or_default()
    }
}

fn system_icon_metrics_for_dpi(dpi: u32) -> ((i32, i32), (i32, i32)) {
    let dpi = dpi.max(96);
    unsafe {
        let big_w = GetSystemMetricsForDpi(SM_CXICON, dpi).max(16);
        let big_h = GetSystemMetricsForDpi(SM_CYICON, dpi).max(16);
        let sm_w = GetSystemMetricsForDpi(SM_CXSMICON, dpi).max(16);
        let sm_h = GetSystemMetricsForDpi(SM_CYSMICON, dpi).max(16);
        ((big_w, big_h), (sm_w, sm_h))
    }
}

/// Owns HiDPI tray icons (Normal/Warn/Critical) and per-window dashboard icons safely without
/// ever destroying `WNDCLASSEXW` class icons or double-freeing fallback handles.
struct AppIcons {
    class_sm_fallback: HICON,
    tray_normal: HICON,
    tray_warn: HICON,
    tray_crit: HICON,
    tray_dpi: u32,
    owned_tray: Vec<HICON>,
    dash_big: Option<HICON>,
    dash_sm: Option<HICON>,
    dash_dpi: u32,
}

impl AppIcons {
    fn new(class_sm_fallback: HICON, sys_dpi: u32) -> Self {
        let mut icons = Self {
            class_sm_fallback,
            tray_normal: class_sm_fallback,
            tray_warn: class_sm_fallback,
            tray_crit: class_sm_fallback,
            tray_dpi: 0,
            owned_tray: Vec::new(),
            dash_big: None,
            dash_sm: None,
            dash_dpi: 0,
        };
        icons.reload_tray_for_dpi(sys_dpi);
        icons
    }

    fn push_owned_unique(list: &mut Vec<HICON>, h: HICON) {
        if !h.0.is_null() && !list.contains(&h) {
            list.push(h);
        }
    }

    fn reload_tray_for_dpi(&mut self, dpi: u32) -> bool {
        let dpi = dpi.max(96);
        if self.tray_dpi == dpi && !self.tray_normal.0.is_null() {
            return false;
        }
        let fallback = self.class_sm_fallback;
        let (_, (sm_w, sm_h)) = system_icon_metrics_for_dpi(dpi);
        let normal_raw = load_res_icon(RES_ICON_NORMAL, sm_w, sm_h);
        let warn_raw = load_res_icon(RES_ICON_WARN, sm_w, sm_h);
        let crit_raw = load_res_icon(RES_ICON_CRIT, sm_w, sm_h);

        let new_normal = if !normal_raw.0.is_null() { normal_raw } else { fallback };
        let new_warn = if !warn_raw.0.is_null() { warn_raw } else { new_normal };
        let new_crit = if !crit_raw.0.is_null() { crit_raw } else { new_normal };

        let mut new_owned = Vec::new();
        Self::push_owned_unique(&mut new_owned, normal_raw);
        Self::push_owned_unique(&mut new_owned, warn_raw);
        Self::push_owned_unique(&mut new_owned, crit_raw);

        let old_owned = std::mem::replace(&mut self.owned_tray, new_owned);
        self.tray_normal = new_normal;
        self.tray_warn = new_warn;
        self.tray_crit = new_crit;
        self.tray_dpi = dpi;

        for h in old_owned {
            if !self.owned_tray.contains(&h) && h != fallback {
                unsafe {
                    let _ = DestroyIcon(h);
                }
            }
        }
        true
    }

    fn tray_for_severity(&self, sev: quota::AlertSeverity) -> HICON {
        match sev {
            quota::AlertSeverity::Critical => self.tray_crit,
            quota::AlertSeverity::Warn => self.tray_warn,
            quota::AlertSeverity::Normal => self.tray_normal,
        }
    }

    fn apply_dashboard_icons_for_dpi(&mut self, hwnd: HWND, dpi: u32) {
        let dpi = dpi.max(96);
        if hwnd.0.is_null() || self.dash_dpi == dpi {
            return;
        }
        let ((big_w, big_h), (sm_w, sm_h)) = system_icon_metrics_for_dpi(dpi);
        let big = load_res_icon(RES_ICON_NORMAL, big_w, big_h);
        let sm = load_res_icon(RES_ICON_NORMAL, sm_w, sm_h);
        if big.0.is_null() && sm.0.is_null() {
            return;
        }
        let prev_big = self.dash_big.take();
        let prev_sm = self.dash_sm.take();
        unsafe {
            if !big.0.is_null() {
                let _ = SendMessageW(hwnd, WM_SETICON, WPARAM(ICON_BIG as usize), LPARAM(big.0 as isize));
                self.dash_big = Some(big);
            }
            if !sm.0.is_null() {
                let _ = SendMessageW(hwnd, WM_SETICON, WPARAM(ICON_SMALL as usize), LPARAM(sm.0 as isize));
                self.dash_sm = Some(sm);
            }
            if let Some(old) = prev_big {
                if !old.0.is_null() && Some(old) != prev_sm {
                    let _ = DestroyIcon(old);
                }
            }
            if let Some(old) = prev_sm {
                if !old.0.is_null() {
                    let _ = DestroyIcon(old);
                }
            }
        }
        self.dash_dpi = dpi;
    }

    fn clear_dashboard_icons(&mut self) {
        let prev_big = self.dash_big.take();
        let prev_sm = self.dash_sm.take();
        self.dash_dpi = 0;
        unsafe {
            if let Some(old) = prev_big {
                if !old.0.is_null() && Some(old) != prev_sm {
                    let _ = DestroyIcon(old);
                }
            }
            if let Some(old) = prev_sm {
                if !old.0.is_null() {
                    let _ = DestroyIcon(old);
                }
            }
        }
    }
}

impl Drop for AppIcons {
    fn drop(&mut self) {
        self.clear_dashboard_icons();
        for h in self.owned_tray.drain(..) {
            if !h.0.is_null() {
                unsafe {
                    let _ = DestroyIcon(h);
                }
            }
        }
    }
}

fn broadcast_meta_sync(exclude: HWND) {
    unsafe {
        for cls in [w!("GaugePillClass"), w!("GaugeDashboardClass")] {
            if let Ok(target) = FindWindowW(cls, None) {
                if !target.0.is_null() && target != exclude {
                    let _ = PostMessageW(target, WM_APP_SYNC_META, WPARAM(0), LPARAM(0));
                }
            }
        }
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut cut: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

fn build_quick_session_menu_items(snapshot: &ScanSnapshot) -> Vec<QuickSessionMenuItem> {
    let now = Local::now();
    let show_multi = snapshot.has_non_antigravity();
    let mut cands: Vec<(bool, Option<chrono::DateTime<Local>>, &stats::SessionInfo)> = snapshot
        .sessions
        .iter()
        .filter(|s| {
            launcher::classify_resume_capability(s, &snapshot.sessions).is_direct_resumable()
                && launcher::validate_conversation_id(&s.conversation_id)
        })
        .map(|s| {
            let pinned = snapshot
                .session_meta
                .get(&s.conversation_id)
                .is_some_and(|m| m.pinned);
            (pinned, s.last_time, s)
        })
        .collect();
    cands.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    cands
        .into_iter()
        .take(5)
        .map(|(pinned, last_t, s)| {
            let star = if pinned { "★ " } else { "" };
            let src_prefix = dashboard::format_source_tag(s.source, show_multi)
                .map(|t| format!("[{t}] "))
                .unwrap_or_default();
            let title = truncate_chars(s.title.trim(), 24);
            let ws = s.workspace_short();
            let rel = fmt_relative_time_cn(last_t, now);
            let label = if ws.is_empty() {
                format!("{star}{src_prefix}{title} ({rel})")
            } else {
                format!("{star}{src_prefix}{title} · {} ({rel})", truncate_chars(&ws, 14))
            };
            QuickSessionMenuItem {
                conversation_id: s.conversation_id.clone(),
                label,
            }
        })
        .collect()
}

struct AppState {
    factories: D2DFactories,
    fonts: Fonts,
    icons: AppIcons,
    scanner: IncrementalScanner,
    snapshot: ScanSnapshot,
    telemetry: SystemTelemetry,

    pill_hwnd: HWND,
    pill_renderer: PillRenderer,
    pill_pos: (i32, i32),
    custom_pos: Option<(i32, i32)>,
    drag_down: bool,
    dragging: bool,
    drag_start_cursor: (i32, i32),
    drag_start_win: (i32, i32),
    pill_hovered: bool,
    pill_tracking: bool,
    pill_menu_open: bool,
    topmost_unyielding_blocker: Option<isize>,
    light_theme: bool,
    topmost: bool,
    human: bool,
    cost_mode: bool,
    group_subagents: bool,
    privacy_mode: bool,
    pill_visible: bool,
    tray_hover_mode: bool,
    tray_hover_shown: bool,
    tray_hover_icon_rect: Option<RECT>,
    tray_hover_leave_since: Option<Instant>,
    tray_hover_suppress_until: Option<Instant>,
    tray_pin_retries: u8,
    fullscreen_hidden: bool,
    notifications_enabled: bool,
    hide_on_fullscreen: bool,
    fresh_only: bool,
    date_scope: DateScope,
    pill_items: u32,
    monitor_index: usize,
    last_total_tokens: u64,
    recent_activity_until: Option<Instant>,
    tray: Option<TrayIcon>,
    taskbar_created_msg: u32,
    fired_alerts: HashSet<String>,
    last_tray_tip: String,
    self_test_mode: bool,

    dash_owner_hwnd: Option<HWND>,
    dash_hwnd: Option<HWND>,
    dash_view: Option<DashboardView>,
    pricing_edits: PricingEditControls,
    dash_visible: bool,
    dash_tracking: bool,
    dash_monitor_index: usize,
    dashboard_only: bool,
}

thread_local! {
    static APP: RefCell<Option<AppState>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut AppState) -> R) -> Option<R> {
    APP.with(|cell| {
        let Ok(mut borrow) = cell.try_borrow_mut() else {
            return None;
        };
        borrow.as_mut().map(f)
    })
}

fn dashboard_rect_for_monitor(monitor_index: usize, pill_hwnd: HWND) -> (i32, i32, i32, i32) {
    let work = get_work_area(monitor_index);
    let scale = if !pill_hwnd.0.is_null() {
        window_dpi_scale(pill_hwnd)
    } else {
        1.25
    };
    let avail_w = work.right - work.left;
    let avail_h = work.bottom - work.top;
    let ww = ((1060.0 * scale).round() as i32).min((avail_w as f32 * 0.90) as i32);
    let wh = ((710.0 * scale).round() as i32).min((avail_h as f32 * 0.88) as i32);
    let wx = work.left + (avail_w - ww) / 2;
    let wy = work.top + (avail_h - wh) / 2;
    (wx, wy, ww, wh)
}

fn toggle_dashboard_safe() {
    let (local_hwnd, local_visible) = with_app(|app| (app.dash_hwnd, app.dash_visible))
        .unwrap_or((None, false));
    if let Some(h) = local_hwnd {
        let actually_shown = unsafe { IsWindowVisible(h).as_bool() && !IsIconic(h).as_bool() };
        if local_visible && actually_shown {
            hide_dashboard_safe();
            return;
        }
        open_or_focus_dashboard_safe();
        return;
    }
    unsafe {
        if let Ok(existing) = FindWindowW(w!("GaugeDashboardClass"), None) {
            if !existing.0.is_null() && IsWindowVisible(existing).as_bool() && !IsIconic(existing).as_bool() {
                let _ = PostMessageW(existing, WM_CLOSE, WPARAM(0), LPARAM(0));
                return;
            }
        }
    }
    open_or_focus_dashboard_safe();
}

fn open_or_focus_dashboard_safe() {
    let local_info = with_app(|app| {
        app.dash_hwnd.map(|hwnd| {
            let moved_mon = app.dash_monitor_index != app.monitor_index;
            let fallback_rect = dashboard_rect_for_monitor(app.monitor_index, app.pill_hwnd);
            if moved_mon {
                app.dash_monitor_index = app.monitor_index;
            }
            app.dash_visible = true;
            (hwnd, app.light_theme, moved_mon, fallback_rect)
        })
    })
    .flatten();

    if let Some((hwnd, light, moved_mon, fallback_rect)) = local_info {
        unsafe {
            let th = theme_for(light);
            apply_win11_frame(hwnd, th.is_dark, th.caption_colorref);
            // Always use SW_RESTORE if iconic (e.g., after Win+D / Win+M), otherwise SW_SHOW.
            let was_iconic = IsIconic(hwnd).as_bool();
            let _ = ShowWindow(hwnd, if was_iconic { SW_RESTORE } else { SW_SHOW });

            // Guard against degenerate/off-screen rect (e.g., after monitor disconnect or minimized stub).
            let mut rc = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rc);
            let rw = rc.right - rc.left;
            let rh = rc.bottom - rc.top;
            let off_screen = monitor_index_for_point((rc.left + rc.right) / 2, (rc.top + rc.bottom) / 2).is_none();
            if moved_mon || rw < 400 || rh < 300 || off_screen {
                let (wx, wy, ww, wh) = fallback_rect;
                let _ = SetWindowPos(hwnd, None, wx, wy, ww, wh, SWP_NOZORDER | SWP_NOACTIVATE);
            }
            let _ = SetForegroundWindow(hwnd);
            let _ = InvalidateRect(hwnd, None, false);
            let _ = UpdateWindow(hwnd);
        }
        with_app(|app| app.redraw_pill());
        ensure_pill_topmost_if_needed(false);
        return;
    }

    unsafe {
        if let Ok(existing) = FindWindowW(w!("GaugeDashboardClass"), None) {
            if !existing.0.is_null() {
                let _ = ShowWindow(existing, SW_RESTORE);
                let _ = SetForegroundWindow(existing);
                let _ = InvalidateRect(existing, None, false);
                return;
            }
        }
    }
    create_dashboard_window_local();
}

fn create_dashboard_window_local() {
    let existing = with_app(|app| app.dash_hwnd).flatten();
    if let Some(hwnd) = existing {
        with_app(|app| app.dash_visible = true);
        unsafe {
            let cmd = if IsIconic(hwnd).as_bool() { SW_RESTORE } else { SW_SHOW };
            let _ = ShowWindow(hwnd, cmd);
            let _ = SetForegroundWindow(hwnd);
            let _ = InvalidateRect(hwnd, None, false);
            let _ = UpdateWindow(hwnd);
        }
        with_app(|app| app.redraw_pill());
        ensure_pill_topmost_if_needed(false);
        return;
    }

    let Some((wx, wy, ww, wh, light, mon_idx)) = with_app(|app| {
        let (wx, wy, ww, wh) = dashboard_rect_for_monitor(app.monitor_index, app.pill_hwnd);
        (wx, wy, ww, wh, app.light_theme, app.monitor_index)
    }) else {
        return;
    };

    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        // Use a hidden WS_EX_TOOLWINDOW owner so the dashboard keeps its full native Win11
        // rounded titlebar & controls while NEVER adding a button to the Windows taskbar!
        let owner_hwnd = with_app(|app| app.dash_owner_hwnd).flatten().or_else(|| {
            let h = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("GaugeOwnerClass"),
                w!(""),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                hinst,
                None,
            )
            .ok();
            if let Some(oh) = h {
                with_app(|app| app.dash_owner_hwnd = Some(oh));
            }
            h
        });

        if let Ok(hwnd) = CreateWindowExW(
            Default::default(),
            w!("GaugeDashboardClass"),
            w!("Gauge"),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            wx,
            wy,
            ww,
            wh,
            owner_hwnd.unwrap_or_default(),
            None,
            hinst,
            None,
        ) {
            let th = theme_for(light);
            apply_win11_frame(hwnd, th.is_dark, th.caption_colorref);
            let win_dpi = (window_dpi_scale(hwnd) * 96.0).round() as u32;
            with_app(|app| {
                app.icons.apply_dashboard_icons_for_dpi(hwnd, win_dpi);
                app.dash_hwnd = Some(hwnd);
                app.dash_view = Some(DashboardView::new(
                    app.fresh_only,
                    app.date_scope,
                    app.cost_mode,
                    app.group_subagents,
                    app.privacy_mode,
                ));
                app.dash_visible = true;
                app.dash_monitor_index = mon_idx;
            });
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            let _ = InvalidateRect(hwnd, None, false);
            let _ = UpdateWindow(hwnd);
            with_app(|app| app.redraw_pill());
            ensure_pill_topmost_if_needed(false);
        }
    }
}

fn open_dashboard_to_sessions_tab_safe() {
    open_or_focus_dashboard_safe();
    with_app(|app| {
        if let Some(view) = &mut app.dash_view {
            view.open_sessions_tab();
        }
        if let Some(dh) = app.dash_hwnd {
            app.pricing_edits.hide_five_edits(dh);
        }
        app.invalidate_dashboard();
    });
}

fn hide_dashboard_safe() {
    let (hwnd, dash_only) = with_app(|app| {
        app.dash_visible = false;
        if let Some(dh) = app.dash_hwnd {
            app.pricing_edits.hide_all(dh);
        }
        if let Some(view) = &mut app.dash_view {
            view.mouse_dip = None;
            view.pricing_focused_field = None;
            view.search_focused = false;
            view.custom_date_focused = None;
            // Genuinely free the large offscreen DIB + D2D DCRenderTarget while hidden
            view.invalidate_surface();
        }
        (app.dash_hwnd, app.dashboard_only)
    })
    .unwrap_or((None, false));

    if dash_only {
        destroy_dashboard_safe();
        return;
    }
    if let Some(h) = hwnd {
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
        let pill_top = with_app(|app| {
            app.redraw_pill();
            (app.pill_hwnd, app.is_pill_effectively_topmost() && app.should_pill_be_visible())
        });
        if let Some((ph, true)) = pill_top {
            apply_pill_topmost(ph, true, false);
        }
        trim_self_working_set();
    }
}

fn destroy_dashboard_safe() {
    let (hwnd, owner_hwnd, dash_only) = with_app(|app| {
        app.dash_visible = false;
        app.dash_view = None;
        app.pricing_edits.reset_handles();
        app.icons.clear_dashboard_icons();
        (app.dash_hwnd.take(), app.dash_owner_hwnd.take(), app.dashboard_only)
    })
    .unwrap_or((None, None, false));
    if let Some(h) = hwnd {
        unsafe {
            let _ = DestroyWindow(h);
        }
    } else if !dash_only {
        unsafe {
            if let Ok(ext) = FindWindowW(w!("GaugeDashboardClass"), None) {
                if !ext.0.is_null() {
                    let _ = PostMessageW(ext, WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
        }
    }
    if let Some(oh) = owner_hwnd {
        unsafe {
            let _ = DestroyWindow(oh);
        }
    }
    if dash_only {
        unsafe {
            PostQuitMessage(0);
        }
    }
}

fn set_pill_topmost_safe(top: bool, persist: bool) {
    let hwnd = with_app(|app| {
        app.topmost = top;
        app.topmost_unyielding_blocker = None;
        if persist {
            app.persist_and_sync(app.pill_hwnd);
        }
        app.pill_hwnd
    });
    if let Some(h) = hwnd {
        // Use hard_reset when explicitly enabling topmost so any ghost-topmost Z-order desync is broken.
        apply_pill_topmost(h, top, top);
    }
}

fn ensure_pill_topmost_if_needed(force_raise: bool) {
    let info = with_app(|app| {
        if app.dashboard_only
            || !app.is_pill_effectively_topmost()
            || !app.should_pill_be_visible()
            || app.pill_menu_open
            || app.drag_down
        {
            None
        } else {
            Some((app.pill_hwnd, app.topmost_unyielding_blocker))
        }
    })
    .flatten();
    let Some((hwnd, unyielding)) = info else {
        return;
    };
    if hwnd.0.is_null() {
        return;
    }
    let health = inspect_pill_topmost_health(hwnd);
    match health {
        TopmostHealth::Healthy => {
            if unyielding.is_some() {
                with_app(|app| app.topmost_unyielding_blocker = None);
            }
            if force_raise {
                apply_pill_topmost(hwnd, true, false);
            }
        }
        TopmostHealth::NeedHardReset(blocker) => {
            if !force_raise && blocker != 0 && unyielding == Some(blocker) {
                return;
            }
            apply_pill_topmost(hwnd, true, true);
            // Verify recovery: if the exact same elevated/exclusive-fullscreen window still sits above,
            // pause retries for that blocker until it leaves or foreground changes.
            let after = inspect_pill_topmost_health(hwnd);
            let still_blocked = match after {
                TopmostHealth::NeedHardReset(b) | TopmostHealth::NeedRaise(b) if b != 0 && b == blocker => Some(b),
                _ => None,
            };
            with_app(|app| app.topmost_unyielding_blocker = still_blocked);
        }
        TopmostHealth::NeedRaise(blocker) => {
            if !force_raise && blocker != 0 && unyielding == Some(blocker) {
                return;
            }
            apply_pill_topmost(hwnd, true, false);
            let after = inspect_pill_topmost_health(hwnd);
            let still_blocked = match after {
                TopmostHealth::NeedRaise(b) | TopmostHealth::NeedHardReset(b) if b != 0 && b == blocker => Some(b),
                _ => None,
            };
            with_app(|app| app.topmost_unyielding_blocker = still_blocked);
        }
    }
}

impl AppState {
    fn to_prefs(&self) -> UserPrefs {
        UserPrefs {
            light_theme: self.light_theme,
            mini: (self.pill_items & (PILL_ITEM_CPU | PILL_ITEM_RAM)) == 0,
            topmost: self.topmost,
            human: self.human,
            cost_mode: self.dash_view.as_ref().map(|v| v.cost_mode).unwrap_or(self.cost_mode),
            group_subagents: self
                .dash_view
                .as_ref()
                .map(|v| v.group_subagents)
                .unwrap_or(self.group_subagents),
            privacy_mode: self
                .dash_view
                .as_ref()
                .map(|v| v.privacy_mode)
                .unwrap_or(self.privacy_mode),
            pill_visible: self.pill_visible,
            tray_hover_mode: self.tray_hover_mode,
            notifications_enabled: self.notifications_enabled,
            hide_on_fullscreen: self.hide_on_fullscreen,
            monitor_index: self.monitor_index,
            fresh_only: self.dash_view.as_ref().map(|v| v.fresh_only).unwrap_or(self.fresh_only),
            date_scope: self.dash_view.as_ref().map(|v| v.scope).unwrap_or(self.date_scope).to_u8(),
            pill_metric: 0,
            pill_items: self.pill_items,
            custom_pos: self.custom_pos,
        }
    }

    fn persist_and_sync(&mut self, sender: HWND) {
        let p = self.to_prefs();
        self.cost_mode = p.cost_mode;
        self.group_subagents = p.group_subagents;
        self.privacy_mode = p.privacy_mode;
        self.fresh_only = p.fresh_only;
        self.date_scope = DateScope::from_u8(p.date_scope);
        self.pill_items = p.pill_items;
        save_user_prefs(&p);
        broadcast_prefs_sync(sender);
    }

    fn apply_synced_prefs(&mut self, p: UserPrefs) {
        self.light_theme = p.light_theme;
        self.topmost = p.topmost;
        self.human = p.human;
        self.cost_mode = p.cost_mode;
        self.group_subagents = p.group_subagents;
        self.privacy_mode = p.privacy_mode;
        self.pill_visible = p.pill_visible;
        if self.tray_hover_mode != p.tray_hover_mode {
            self.tray_hover_mode = p.tray_hover_mode;
            self.tray_hover_shown = false;
            self.tray_hover_leave_since = None;
            self.tray_pin_retries = 6;
            if !self.pill_hwnd.0.is_null() {
                unsafe {
                    let _ = KillTimer(self.pill_hwnd, TIMER_TRAY_HOVER);
                }
            }
        }
        self.notifications_enabled = p.notifications_enabled;
        self.hide_on_fullscreen = p.hide_on_fullscreen;
        self.monitor_index = p.monitor_index;
        self.fresh_only = p.fresh_only;
        self.date_scope = DateScope::from_u8(p.date_scope);
        self.pill_items = p.pill_items;
        self.custom_pos = p.custom_pos;
        if let Some(view) = &mut self.dash_view {
            view.cost_mode = p.cost_mode;
            view.group_subagents = p.group_subagents;
            view.privacy_mode = p.privacy_mode;
            view.fresh_only = p.fresh_only;
            if view.scope != self.date_scope {
                view.scope = self.date_scope;
                view.mark_data_dirty();
            }
        }
    }

    fn should_pill_be_visible(&self) -> bool {
        if self.dashboard_only {
            return false;
        }
        if self.tray_hover_mode {
            self.tray_hover_shown
        } else {
            self.pill_visible && !self.fullscreen_hidden
        }
    }

    fn is_pill_effectively_topmost(&self) -> bool {
        self.topmost || (self.tray_hover_mode && self.tray_hover_shown)
    }

    fn redraw_pill(&mut self) {
        if self.pill_hwnd.0.is_null() || !self.should_pill_be_visible() {
            return;
        }
        let flyout_anchor = if self.tray_hover_mode {
            self.tray_hover_icon_rect
        } else {
            None
        };
        let dpi = if let Some(ir) = flyout_anchor {
            dpi_for_point((ir.left + ir.right) / 2, (ir.top + ir.bottom) / 2)
        } else {
            window_dpi_scale(self.pill_hwnd) * 96.0
        };
        let recent = self
            .recent_activity_until
            .map(|t| Instant::now() < t)
            .unwrap_or(false);
        if let Ok(pos) = self.pill_renderer.render(
            &self.factories,
            &self.fonts,
            self.pill_hwnd,
            dpi,
            self.monitor_index,
            self.custom_pos,
            flyout_anchor,
            self.light_theme,
            self.pill_hovered,
            self.dash_visible,
            recent,
            self.pill_items,
            &self.snapshot,
            &self.telemetry,
        ) {
            self.pill_pos = pos;
        }
    }

    fn show_tray_hover_flyout(&mut self, icon_rect: RECT) {
        if !self.tray_hover_mode || self.dashboard_only || self.pill_hwnd.0.is_null() {
            return;
        }
        let rect_changed = self.tray_hover_icon_rect != Some(icon_rect);
        self.tray_hover_icon_rect = Some(icon_rect);
        self.tray_hover_leave_since = None;
        if !self.tray_hover_shown {
            self.tray_hover_shown = true;
            self.redraw_pill();
            unsafe {
                let _ = ShowWindow(self.pill_hwnd, SW_SHOWNOACTIVATE);
                apply_pill_topmost(self.pill_hwnd, true, true);
                let _ = SetTimer(self.pill_hwnd, TIMER_TRAY_HOVER, 100, None);
            }
        } else if rect_changed {
            self.redraw_pill();
        }
    }

    fn hide_tray_hover_flyout(&mut self) {
        if !self.tray_hover_shown {
            return;
        }
        self.tray_hover_shown = false;
        self.tray_hover_leave_since = None;
        self.pill_hovered = false;
        if !self.pill_hwnd.0.is_null() {
            unsafe {
                let _ = KillTimer(self.pill_hwnd, TIMER_TRAY_HOVER);
                let _ = ShowWindow(self.pill_hwnd, SW_HIDE);
            }
        }
    }

    fn tick_tray_hover_guard(&mut self) {
        if !self.tray_hover_mode || !self.tray_hover_shown || self.pill_hwnd.0.is_null() {
            if !self.pill_hwnd.0.is_null() {
                unsafe {
                    let _ = KillTimer(self.pill_hwnd, TIMER_TRAY_HOVER);
                }
            }
            return;
        }
        if self.pill_menu_open {
            self.tray_hover_leave_since = None;
            return;
        }
        let mut pt = POINT::default();
        unsafe {
            if GetCursorPos(&mut pt).is_err() {
                return;
            }
        }
        if let Some(fresh_rect) = self.tray.as_ref().and_then(TrayIcon::get_rect) {
            self.tray_hover_icon_rect = Some(fresh_rect);
        }
        let Some(icon_rect) = self.tray_hover_icon_rect else {
            self.hide_tray_hover_flyout();
            return;
        };
        let (pw, ph) = self.pill_renderer.size_px();
        let sh = self.pill_renderer.shadow_px();
        if is_cursor_in_tray_hover_zone((pt.x, pt.y), icon_rect, self.pill_pos, pw, ph, sh) {
            self.tray_hover_leave_since = None;
        } else {
            match self.tray_hover_leave_since {
                None => self.tray_hover_leave_since = Some(Instant::now()),
                Some(t0) if t0.elapsed() >= Duration::from_millis(220) => {
                    self.hide_tray_hover_flyout();
                }
                _ => {}
            }
        }
    }

    fn sync_tray_taskbar_pin(&mut self) {
        if self.self_test_mode || self.dashboard_only {
            self.tray_pin_retries = 0;
            return;
        }
        match set_self_tray_icon_taskbar_promoted(self.tray_hover_mode) {
            TrayPinResult::UpdatedNow => {
                self.tray_pin_retries = 0;
                let tip = self.build_tray_tooltip();
                if let Some(tray) = &mut self.tray {
                    tray.remove();
                    tray.add(&tip);
                }
                self.last_tray_tip = tip;
            }
            TrayPinResult::AlreadyMatched => {
                self.tray_pin_retries = 0;
            }
            TrayPinResult::NotFound => {
                self.tray_pin_retries = self.tray_pin_retries.saturating_sub(1);
            }
        }
    }

    fn apply_pill_visibility(&mut self) {
        if self.dashboard_only || self.pill_hwnd.0.is_null() {
            return;
        }
        let should_show = self.should_pill_be_visible();
        unsafe {
            let currently_visible = IsWindowVisible(self.pill_hwnd).as_bool();
            if should_show && !currently_visible {
                self.redraw_pill();
                let _ = ShowWindow(self.pill_hwnd, SW_SHOWNOACTIVATE);
                if self.is_pill_effectively_topmost() {
                    apply_pill_topmost(self.pill_hwnd, true, true);
                }
            } else if !should_show && currently_visible {
                let _ = ShowWindow(self.pill_hwnd, SW_HIDE);
            }
        }
    }

    fn check_fullscreen_guard(&mut self) {
        if self.dashboard_only || self.self_test_mode || self.tray_hover_mode || !self.pill_visible {
            return;
        }
        let want_hidden = self.hide_on_fullscreen && is_other_app_fullscreen();
        if want_hidden != self.fullscreen_hidden {
            self.fullscreen_hidden = want_hidden;
            self.apply_pill_visibility();
        }
    }

    fn build_tray_tooltip(&self) -> String {
        if self.tray_hover_mode {
            // Suppress Windows' native gray text tooltip so it never covers our hover capsule flyout.
            return String::new();
        }
        let today_key = Local::now().date_naive().format("%Y-%m-%d").to_string();
        let tc = self
            .snapshot
            .agg
            .by_day
            .get(&today_key)
            .copied()
            .unwrap_or_default();
        let mut tip = format!(
            "Gauge · 今日 {} ({})",
            stats::fmt_compact(tc.total_tokens()),
            pricing::fmt_cost_compact(&tc.cost, tc.calls),
        );
        if let Some(sl) = &self.snapshot.statusline {
            if let Some(b) = sl.tightest_bucket() {
                if let Some(rp) = b.remaining_pct() {
                    tip.push_str(&format!(" · {} {:.0}%", b.label, rp));
                }
            }
        }
        tip
    }

    fn sync_tray_and_alerts(&mut self) {
        if self.self_test_mode {
            return;
        }
        let tip = self.build_tray_tooltip();
        if tip != self.last_tray_tip {
            if let Some(tray) = &self.tray {
                tray.update_tooltip(&tip);
            }
            self.last_tray_tip = tip;
        }

        let today_date = Local::now().date_naive();
        let today_key = today_date.format("%Y-%m-%d").to_string();
        let week_key = stats::iso_week_key(today_date);
        let today_usd = self
            .snapshot
            .agg
            .by_day
            .get(&today_key)
            .map(|c| c.cost.total_usd())
            .unwrap_or(0.0);
        let week_usd = self
            .snapshot
            .agg
            .by_week
            .get(&week_key)
            .map(|c| c.cost.total_usd())
            .unwrap_or(0.0);
        let events = quota::evaluate_alerts(
            &self.snapshot.budget,
            &today_key,
            today_usd,
            &week_key,
            week_usd,
            self.snapshot.statusline.as_ref(),
            Utc::now(),
        );
        // Always sync the 3-state tray icon BEFORE the notifications_enabled early-return so
        // muting balloon popups never freezes tray alert color feedback.
        let max_sev = events
            .iter()
            .map(|e| e.severity)
            .max()
            .unwrap_or(quota::AlertSeverity::Normal);
        let target_icon = self.icons.tray_for_severity(max_sev);
        if let Some(tray) = &mut self.tray {
            tray.update_icon(target_icon);
        }

        if !self.notifications_enabled {
            return;
        }
        for ev in events {
            if self.fired_alerts.insert(ev.dedup_key) {
                if let Some(tray) = &self.tray {
                    tray.show_balloon(
                        &ev.title,
                        &ev.message,
                        ev.severity == quota::AlertSeverity::Critical,
                        ev.severity == quota::AlertSeverity::Warn,
                    );
                }
            }
        }
    }

    fn invalidate_dashboard(&self) {
        if self.dash_visible {
            if let Some(hwnd) = self.dash_hwnd {
                unsafe {
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
        }
    }

    fn refresh_tokens(&mut self, force: bool) {
        let prev_running: HashMap<String, bool> = self
            .snapshot
            .sessions
            .iter()
            .filter(|s| s.is_running_hint)
            .map(|s| (s.conversation_id.clone(), true))
            .collect();
        let fresh = self.scanner.scan();
        let running_changed = {
            let mut cur_running_cnt = 0usize;
            let mut diff = false;
            for s in &fresh.sessions {
                if s.is_running_hint {
                    cur_running_cnt += 1;
                    if !prev_running.contains_key(&s.conversation_id) {
                        diff = true;
                        break;
                    }
                }
            }
            diff || cur_running_cnt != prev_running.len()
        };
        let changed = force
            || fresh.pricing_changed
            || fresh.statusline_changed
            || fresh.budget_changed
            || fresh.session_meta_changed
            || fresh.files_reparsed > 0
            || fresh.sessions.len() != self.snapshot.sessions.len()
            || running_changed;
        self.snapshot = fresh;
        if changed {
            let cur_tot = self.snapshot.agg.total.total_tokens();
            if cur_tot != self.last_total_tokens {
                self.last_total_tokens = cur_tot;
                self.recent_activity_until = Some(Instant::now() + Duration::from_secs(8));
            }
            if let Some(view) = &mut self.dash_view {
                view.mark_data_dirty();
            }
            self.sync_tray_and_alerts();
            self.redraw_pill();
            self.invalidate_dashboard();
        }
    }

    fn lookup_session(&self, cid: &str) -> Option<stats::SessionInfo> {
        self.snapshot
            .sessions
            .iter()
            .find(|s| s.conversation_id == cid)
            .cloned()
    }

    fn notify_banner(&mut self, msg: impl Into<String>, is_err: bool) {
        let text = msg.into();
        if let Some(view) = &mut self.dash_view {
            view.set_banner_status(text, is_err);
        }
        self.invalidate_dashboard();
    }

    fn handle_toggle_pin(&mut self, sender: HWND, cid: &str) {
        match self.scanner.toggle_session_pin(cid) {
            Ok(now_pinned) => {
                self.refresh_tokens(true);
                broadcast_meta_sync(sender);
                self.notify_banner(
                    if now_pinned { "已置顶该会话" } else { "已取消置顶" },
                    false,
                );
            }
            Err(err) => {
                self.notify_banner(err, true);
            }
        }
    }

    fn handle_copy_resume_cmd(&mut self, hwnd: HWND, cid: &str) {
        if self.privacy_mode {
            self.notify_banner("脱敏模式已拦截，按 P 关闭后重试", true);
            return;
        }
        let Some(s) = self.lookup_session(cid) else {
            self.notify_banner("未找到该会话", true);
            return;
        };
        let cap = launcher::classify_resume_capability(&s, &self.snapshot.sessions);
        if !cap.is_direct_resumable() {
            let reason = cap
                .disabled_reason_cn()
                .or_else(|| cap.blocked_reason().map(launcher::BlockedReason::message_cn))
                .unwrap_or("该会话不支持终端恢复");
            self.notify_banner(reason, true);
            return;
        }
        match launcher::format_resume_command_for_session(&s) {
            Ok(cmd) => {
                if copy_text_to_clipboard(hwnd, &cmd) {
                    if let Some(view) = &mut self.dash_view {
                        view.resume_cmd_copied_until =
                            Some(Instant::now() + Duration::from_millis(1600));
                    }
                    let cli_name = launcher::cli_binary_name_for_source(s.source).unwrap_or("终端");
                    self.notify_banner(
                        format!("已复制 {cli_name} 恢复命令，粘贴至终端即可继续"),
                        false,
                    );
                } else {
                    self.notify_banner("写入剪贴板失败", true);
                }
            }
            Err(err) => self.notify_banner(err, true),
        }
    }

    fn handle_open_workspace(&mut self, hwnd: HWND, cid: &str) {
        if self.privacy_mode {
            self.notify_banner("脱敏模式已拦截，按 P 关闭后重试", true);
            return;
        }
        let Some(s) = self.lookup_session(cid) else {
            self.notify_banner("未找到该会话", true);
            return;
        };
        match launcher::open_workspace_folder(hwnd, &s.workspace) {
            Ok(_) => self.notify_banner("已在资源管理器打开工作区目录", false),
            Err(err) => self.notify_banner(err, true),
        }
    }

    fn handle_copy_full_session_id(&mut self, hwnd: HWND, cid: &str) {
        if self.privacy_mode {
            self.notify_banner("脱敏模式已拦截，按 P 关闭后重试", true);
            return;
        }
        if copy_text_to_clipboard(hwnd, cid) {
            self.notify_banner("已复制完整会话 ID", false);
        } else {
            self.notify_banner("写入剪贴板失败", true);
        }
    }

    /// Launches a terminal to resume `cid`. Returns `true` when invoked from the dashboard and
    /// launch succeeds so the dashboard auto-hides (stays open on privacy block or launch error
    /// so the error toast remains visible; emits a quiet tray balloon on workspace fallback).
    fn handle_resume_session(&mut self, hwnd: HWND, cid: &str, from_dashboard: bool) -> bool {
        if self.privacy_mode {
            if from_dashboard {
                self.notify_banner("脱敏模式已拦截，按 P 关闭后重试", true);
            } else if self.notifications_enabled {
                if let Some(tray) = &self.tray {
                    tray.show_balloon("Gauge", "脱敏模式已拦截，请先关闭脱敏模式", false, true);
                }
            }
            return false;
        }
        let Some(s) = self.lookup_session(cid) else {
            self.notify_banner("未找到该会话", true);
            return false;
        };
        let cap = launcher::classify_resume_capability(&s, &self.snapshot.sessions);
        if !cap.is_direct_resumable() {
            let reason = cap
                .disabled_reason_cn()
                .or_else(|| cap.blocked_reason().map(launcher::BlockedReason::message_cn))
                .unwrap_or("该会话不支持终端恢复");
            self.notify_banner(reason, true);
            return false;
        }
        let had_ws_spec = launcher::normalize_workspace_path_str(&s.workspace).is_some();
        match launcher::launch_resume_for_session(&s) {
            Ok(outcome) => {
                if outcome.workspace_fallback && had_ws_spec {
                    let msg = format!(
                        "原工作区目录不存在，已通过 {} 在用户主目录恢复会话",
                        outcome.backend.label()
                    );
                    if from_dashboard {
                        self.notify_banner(msg, false);
                    } else if self.notifications_enabled {
                        if let Some(tray) = &self.tray {
                            tray.show_balloon("Gauge", &msg, false, false);
                        }
                    }
                    // Auto-hide dashboard on launch; also emit quiet balloon if enabled so user sees fallback notice.
                    if from_dashboard && self.notifications_enabled {
                        if let Some(tray) = &self.tray {
                            tray.show_balloon(
                                "Gauge",
                                "原工作区目录不存在，已在用户主目录恢复会话",
                                false,
                                false,
                            );
                        }
                    }
                    from_dashboard
                } else {
                    if from_dashboard {
                        self.notify_banner(
                            format!("已通过 {} 打开会话", outcome.backend.label()),
                            false,
                        );
                    }
                    from_dashboard
                }
            }
            Err(err) => {
                if launcher::resolve_cli_exe_for_source(s.source).is_none() {
                    if let Ok(cmd) = launcher::format_resume_command_for_session(&s) {
                        if copy_text_to_clipboard(hwnd, &cmd) {
                            let cli_name =
                                launcher::cli_binary_name_for_source(s.source).unwrap_or("CLI");
                            let msg = format!("未找到 {cli_name} 命令，已自动复制恢复命令到剪贴板");
                            if from_dashboard {
                                self.notify_banner(msg, true);
                            } else if self.notifications_enabled {
                                if let Some(tray) = &self.tray {
                                    tray.show_balloon("Gauge", &msg, false, true);
                                }
                            }
                            return false;
                        }
                    }
                }
                if from_dashboard {
                    self.notify_banner(err, true);
                } else if self.notifications_enabled {
                    if let Some(tray) = &self.tray {
                        tray.show_balloon("Gauge", &err, false, true);
                    }
                }
                false
            }
        }
    }

    fn load_pricing_row_into_editor(&mut self, idx: usize, focus_price: bool) {
        let Some(row) = self.snapshot.pricing_rows.get(idx).cloned() else {
            return;
        };
        let fields = DashboardView::format_row_fields(&row);
        if let Some(dh) = self.dash_hwnd {
            let dpi = window_dpi_scale(dh) * 96.0;
            self.pricing_edits.ensure_created(dh, dpi, self.light_theme);
        }
        self.pricing_edits.editor_mode = 1;
        self.pricing_edits.set_values(&fields);
        if let Some(view) = &mut self.dash_view {
            view.pricing_selected_model = Some(row.model.clone());
            view.pricing_editing_custom_key = row.custom_key.clone();
        }
        if focus_price {
            self.pricing_edits.focus_field(1);
        }
    }

    fn clear_pricing_editor_for_new(&mut self) {
        if let Some(dh) = self.dash_hwnd {
            let dpi = window_dpi_scale(dh) * 96.0;
            self.pricing_edits.ensure_created(dh, dpi, self.light_theme);
        }
        self.pricing_edits.editor_mode = 1;
        self.pricing_edits
            .set_values(&[String::new(), String::new(), "0.00".into(), String::new(), String::new()]);
        if let Some(view) = &mut self.dash_view {
            view.pricing_selected_model = None;
            view.pricing_editing_custom_key = None;
            view.set_pricing_status("输入模型名（或前缀）与单价，按 Enter 或点保存生效", false);
        }
        self.pricing_edits.focus_field(0);
    }

    fn save_pricing_from_editor(&mut self) {
        let vals = self.pricing_edits.read_values();
        let model = vals[0].trim().to_string();
        if model.is_empty() {
            if let Some(view) = &mut self.dash_view {
                view.set_pricing_status("请输入模型名或前缀（如 gemini-4-neon）", true);
            }
            self.pricing_edits.focus_field(0);
            self.invalidate_dashboard();
            return;
        }
        let parse_price = |raw: &str| -> std::result::Result<f64, ()> {
            let cleaned = raw.trim().trim_start_matches('$').trim();
            let v: f64 = cleaned.parse().map_err(|_| ())?;
            if v.is_finite() && v >= 0.0 {
                Ok(v)
            } else {
                Err(())
            }
        };
        let Ok(input) = parse_price(&vals[1]) else {
            if let Some(view) = &mut self.dash_view {
                view.set_pricing_status("「输入 $/1M」格式无效，请输入非负数字", true);
            }
            self.pricing_edits.focus_field(1);
            self.invalidate_dashboard();
            return;
        };
        let cache_read = if vals[2].trim().is_empty() {
            0.0
        } else {
            match parse_price(&vals[2]) {
                Ok(v) => v,
                Err(_) => {
                    if let Some(view) = &mut self.dash_view {
                        view.set_pricing_status("「缓存读 $/1M」格式无效，请输入非负数字", true);
                    }
                    self.pricing_edits.focus_field(2);
                    self.invalidate_dashboard();
                    return;
                }
            }
        };
        let cache_write = if vals[3].trim().is_empty() {
            input
        } else {
            match parse_price(&vals[3]) {
                Ok(v) => v,
                Err(_) => {
                    if let Some(view) = &mut self.dash_view {
                        view.set_pricing_status("「缓存写 $/1M」格式无效，请输入非负数字或留空", true);
                    }
                    self.pricing_edits.focus_field(3);
                    self.invalidate_dashboard();
                    return;
                }
            }
        };
        let Ok(output) = parse_price(&vals[4]) else {
            if let Some(view) = &mut self.dash_view {
                view.set_pricing_status("「输出 $/1M」格式无效，请输入非负数字", true);
            }
            self.pricing_edits.focus_field(4);
            self.invalidate_dashboard();
            return;
        };

        let rate = ModelRate::new(input, cache_read, cache_write, output);
        match self.scanner.upsert_pricing_rule(&model, rate) {
            Ok(()) => {
                self.refresh_tokens(true);
                let matched_idx = self
                    .snapshot
                    .pricing_rows
                    .iter()
                    .position(|r| r.model.eq_ignore_ascii_case(&model));
                if let Some(idx) = matched_idx {
                    self.load_pricing_row_into_editor(idx, false);
                } else if let Some(view) = &mut self.dash_view {
                    view.pricing_selected_model = Some(model.clone());
                    view.pricing_editing_custom_key = Some(model.clone());
                }
                if let Some(view) = &mut self.dash_view {
                    view.set_pricing_status(format!("已保存 {model}，全盘费用已实时重算"), false);
                }
                self.invalidate_dashboard();
            }
            Err(err) => {
                if let Some(view) = &mut self.dash_view {
                    view.set_pricing_status(err, true);
                }
                self.invalidate_dashboard();
            }
        }
    }

    fn reset_pricing_from_editor(&mut self) {
        let target = self
            .dash_view
            .as_ref()
            .and_then(|v| v.pricing_editing_custom_key.clone())
            .or_else(|| {
                let m = self.pricing_edits.read_values()[0].trim().to_string();
                if m.is_empty() { None } else { Some(m) }
            });
        let Some(key) = target else {
            return;
        };
        match self.scanner.remove_pricing_rule(&key) {
            Ok(true) => {
                let prev_sel = self
                    .dash_view
                    .as_ref()
                    .and_then(|v| v.pricing_selected_model.clone());
                self.refresh_tokens(true);
                let next_idx = prev_sel
                    .as_deref()
                    .and_then(|s| {
                        self.snapshot
                            .pricing_rows
                            .iter()
                            .position(|r| r.model.eq_ignore_ascii_case(s))
                    })
                    .or_else(|| if self.snapshot.pricing_rows.is_empty() { None } else { Some(0) });
                if let Some(idx) = next_idx {
                    self.load_pricing_row_into_editor(idx, false);
                } else {
                    self.clear_pricing_editor_for_new();
                }
                if let Some(view) = &mut self.dash_view {
                    view.set_pricing_status(format!("已移除自定义规则 {key}，恢复默认单价"), false);
                }
                self.invalidate_dashboard();
            }
            Ok(false) => {
                if let Some(view) = &mut self.dash_view {
                    view.set_pricing_status("当前模型使用内置单价，无需重置", false);
                }
                self.invalidate_dashboard();
            }
            Err(err) => {
                if let Some(view) = &mut self.dash_view {
                    view.set_pricing_status(err, true);
                }
                self.invalidate_dashboard();
            }
        }
    }

    fn load_budget_into_editor(&mut self) {
        let fields = DashboardView::format_budget_fields(&self.snapshot.budget);
        if let Some(dh) = self.dash_hwnd {
            let dpi = window_dpi_scale(dh) * 96.0;
            self.pricing_edits.ensure_created(dh, dpi, self.light_theme);
        }
        self.pricing_edits.editor_mode = 2;
        self.pricing_edits.set_budget_values(&fields);
    }

    fn save_budget_from_editor(&mut self) {
        let vals = self.pricing_edits.read_values();
        let parse_usd = |raw: &str| -> std::result::Result<f64, ()> {
            let t = raw.trim().trim_start_matches('$').trim();
            if t.is_empty() {
                return Ok(0.0);
            }
            let v: f64 = t.parse().map_err(|_| ())?;
            if v.is_finite() && v >= 0.0 {
                Ok(v)
            } else {
                Err(())
            }
        };
        let parse_pct = |raw: &str, default_val: f64| -> std::result::Result<f64, ()> {
            let t = raw.trim().trim_end_matches('%').trim();
            if t.is_empty() {
                return Ok(default_val);
            }
            let v: f64 = t.parse().map_err(|_| ())?;
            if v.is_finite() && (1.0..=500.0).contains(&v) {
                Ok(v)
            } else {
                Err(())
            }
        };

        let Ok(daily_usd) = parse_usd(&vals[0]) else {
            if let Some(view) = &mut self.dash_view {
                view.set_pricing_status("「每日预算 ($)」格式无效，请输入非负数字或留空", true);
            }
            self.pricing_edits.focus_field(0);
            self.invalidate_dashboard();
            return;
        };
        let Ok(weekly_usd) = parse_usd(&vals[1]) else {
            if let Some(view) = &mut self.dash_view {
                view.set_pricing_status("「每周预算 ($)」格式无效，请输入非负数字或留空", true);
            }
            self.pricing_edits.focus_field(1);
            self.invalidate_dashboard();
            return;
        };
        let Ok(warn_pct) = parse_pct(&vals[2], 80.0) else {
            if let Some(view) = &mut self.dash_view {
                view.set_pricing_status("「预警比例 (%)」需在 1 到 500 之间", true);
            }
            self.pricing_edits.focus_field(2);
            self.invalidate_dashboard();
            return;
        };
        let Ok(critical_pct) = parse_pct(&vals[3], 100.0) else {
            if let Some(view) = &mut self.dash_view {
                view.set_pricing_status("「临界比例 (%)」需在 1 到 500 之间", true);
            }
            self.pricing_edits.focus_field(3);
            self.invalidate_dashboard();
            return;
        };
        if critical_pct < warn_pct {
            if let Some(view) = &mut self.dash_view {
                view.set_pricing_status("临界比例不能低于预警比例", true);
            }
            self.pricing_edits.focus_field(3);
            self.invalidate_dashboard();
            return;
        }

        let cfg = quota::BudgetConfig {
            daily_usd,
            weekly_usd,
            warn_pct,
            critical_pct,
            quota_warn_rem_pct: self.snapshot.budget.quota_warn_rem_pct,
            quota_crit_rem_pct: self.snapshot.budget.quota_crit_rem_pct,
        };
        match self.scanner.save_budget(cfg) {
            Ok(()) => {
                self.fired_alerts.clear();
                self.refresh_tokens(true);
                self.load_budget_into_editor();
                if let Some(view) = &mut self.dash_view {
                    view.set_pricing_status("已保存预算阈值至本地 budget.json，胶囊与提醒已生效", false);
                }
                self.invalidate_dashboard();
            }
            Err(err) => {
                if let Some(view) = &mut self.dash_view {
                    view.set_pricing_status(err, true);
                }
                self.invalidate_dashboard();
            }
        }
    }

    fn clear_budget_from_editor(&mut self) {
        let cfg = quota::BudgetConfig::default();
        match self.scanner.save_budget(cfg) {
            Ok(()) => {
                self.fired_alerts.clear();
                self.refresh_tokens(true);
                self.load_budget_into_editor();
                if let Some(view) = &mut self.dash_view {
                    view.set_pricing_status("已清空预算限制（配额低余量提醒仍保持默认）", false);
                }
                self.invalidate_dashboard();
            }
            Err(err) => {
                if let Some(view) = &mut self.dash_view {
                    view.set_pricing_status(err, true);
                }
                self.invalidate_dashboard();
            }
        }
    }

    fn apply_custom_dates_from_editor(&mut self) {
        let [raw_since, raw_until] = self.pricing_edits.read_date_texts();
        let parse_d = |raw: &str| -> std::result::Result<Option<NaiveDate>, ()> {
            let t = raw.trim();
            if t.is_empty() {
                return Ok(None);
            }
            NaiveDate::parse_from_str(t, "%Y-%m-%d").map(Some).map_err(|_| ())
        };
        let Ok(since) = parse_d(&raw_since) else {
            if let Some(view) = &mut self.dash_view {
                view.set_banner_status("开始日期格式错误，请输入 YYYY-MM-DD", true);
            }
            self.pricing_edits.focus_date(0);
            self.invalidate_dashboard();
            return;
        };
        let Ok(until) = parse_d(&raw_until) else {
            if let Some(view) = &mut self.dash_view {
                view.set_banner_status("结束日期格式错误，请输入 YYYY-MM-DD", true);
            }
            self.pricing_edits.focus_date(1);
            self.invalidate_dashboard();
            return;
        };
        if let (Some(a), Some(b)) = (since, until) {
            if a > b {
                if let Some(view) = &mut self.dash_view {
                    view.set_banner_status("开始日期不能晚于结束日期", true);
                }
                self.pricing_edits.focus_date(0);
                self.invalidate_dashboard();
                return;
            }
        }
        if let Some(view) = &mut self.dash_view {
            view.custom_since = since;
            view.custom_until = until;
            view.mark_data_dirty();
            let s_lbl = since.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "不限".into());
            let u_lbl = until.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "不限".into());
            view.set_banner_status(format!("已应用自定义范围：{s_lbl} 至 {u_lbl}"), false);
        }
        if let Some(dh) = self.dash_hwnd {
            unsafe {
                let _ = SetFocus(dh);
            }
        }
        self.invalidate_dashboard();
    }

    fn sync_inline_controls(&mut self, hwnd: HWND) {
        if !self.dash_visible {
            self.pricing_edits.hide_all(hwnd);
            return;
        }
        let dpi = window_dpi_scale(hwnd) * 96.0;
        let (is_pricing, is_budget, p_rects, b_rects, search_rect, date_rects, date_vals) =
            match &self.dash_view {
                Some(v) => (
                    v.is_pricing_editor_active(),
                    v.is_budget_editor_active(),
                    v.pricing_field_rects,
                    v.budget_field_rects,
                    if v.is_search_active() { v.search_field_rect } else { None },
                    if v.is_custom_date_active() { v.custom_date_rects } else { None },
                    v.format_custom_date_fields(),
                ),
                None => {
                    self.pricing_edits.hide_all(hwnd);
                    return;
                }
            };

        if is_pricing {
            if self.pricing_edits.editor_mode != 1 {
                self.pricing_edits.editor_mode = 1;
                self.pricing_edits.initialized_values = false;
            }
            self.pricing_edits
                .sync_layout(hwnd, dpi, self.light_theme, p_rects.as_ref().map(|r| r.as_slice()));
            if !self.pricing_edits.initialized_values && !self.snapshot.pricing_rows.is_empty() {
                self.load_pricing_row_into_editor(0, false);
            }
        } else if is_budget {
            if self.pricing_edits.editor_mode != 2 {
                self.pricing_edits.editor_mode = 2;
                self.pricing_edits.initialized_values = false;
            }
            self.pricing_edits
                .sync_layout(hwnd, dpi, self.light_theme, b_rects.as_ref().map(|r| r.as_slice()));
            if !self.pricing_edits.initialized_values {
                self.load_budget_into_editor();
            }
        } else {
            self.pricing_edits.hide_five_edits(hwnd);
        }

        self.pricing_edits
            .sync_search_layout(hwnd, dpi, self.light_theme, search_rect);
        self.pricing_edits
            .sync_date_layout(hwnd, dpi, self.light_theme, date_rects);
        if date_rects.is_some() && !self.pricing_edits.date_initialized {
            self.pricing_edits.set_date_texts(&date_vals);
        }
    }
}

fn show_export_menu_safe(hwnd: HWND) {
    let pt = with_app(|app| {
        if let Some(r) = app.dash_view.as_ref().and_then(|v| v.export_btn_rect) {
            let scale = window_dpi_scale(hwnd);
            let mut p = POINT {
                x: (r.left * scale).round() as i32,
                y: ((r.bottom + 4.0) * scale).round() as i32,
            };
            unsafe {
                let _ = ClientToScreen(hwnd, &mut p);
            }
            return p;
        }
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        p
    })
    .unwrap_or_default();

    let chosen = unsafe {
        let Ok(hmenu) = CreatePopupMenu() else {
            return;
        };
        let add_item = |id: u32, label: &str| {
            let w = to_wide_null(label);
            let _ = AppendMenuW(hmenu, MF_STRING, id as usize, wide_pcwstr(&w));
        };
        add_item(CMD_EXPORT_JSON, "导出当前统计快照 (JSON)...");
        add_item(CMD_EXPORT_SESSIONS_CSV, "导出当前会话列表 (CSV)...");
        add_item(CMD_EXPORT_CALLS_CSV, "导出当前调用记录 (CSV)...");
        add_item(CMD_EXPORT_DAILY_CSV, "导出每日汇总表 (CSV)...");
        let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
        add_item(CMD_EXPORT_STATUSLINE_CFG, "复制 /statusline 配置片段");

        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            hmenu,
            TPM_LEFTALIGN | TPM_TOPALIGN | TPM_RETURNCMD,
            pt.x,
            pt.y,
            0,
            hwnd,
            None,
        )
        .0 as u32;
        let _ = DestroyMenu(hmenu);
        if cmd == 0 { None } else { Some(cmd) }
    };

    let Some(cmd) = chosen else {
        return;
    };

    if cmd == CMD_EXPORT_STATUSLINE_CFG {
        let snippet = DashboardView::format_statusline_config_snippet();
        if copy_text_to_clipboard(hwnd, &snippet) {
            with_app(|app| {
                if let Some(view) = &mut app.dash_view {
                    view.set_banner_status(
                        "已复制 /statusline 配置（粘入 ~/.gemini/antigravity-cli/settings.json 接入 agy 官方配额）",
                        false,
                    );
                }
                app.invalidate_dashboard();
            });
        }
        return;
    }

    let today_str = Local::now().date_naive().format("%Y%m%d").to_string();
    let (default_name, filter_lbl, filter_pat, ext) = match cmd {
        CMD_EXPORT_JSON => (
            format!("gauge-usage-{today_str}.json"),
            "JSON 文件 (*.json)",
            "*.json",
            "json",
        ),
        CMD_EXPORT_SESSIONS_CSV => (
            format!("gauge-sessions-{today_str}.csv"),
            "CSV 文件 (*.csv)",
            "*.csv",
            "csv",
        ),
        CMD_EXPORT_DAILY_CSV => (
            format!("gauge-daily-{today_str}.csv"),
            "CSV 文件 (*.csv)",
            "*.csv",
            "csv",
        ),
        CMD_EXPORT_CALLS_CSV => (
            format!("gauge-calls-{today_str}.csv"),
            "CSV 文件 (*.csv)",
            "*.csv",
            "csv",
        ),
        _ => return,
    };

    let Some(path) = prompt_save_file(hwnd, &default_name, filter_lbl, filter_pat, ext) else {
        return;
    };

    let content = with_app(|app| {
        let view = app.dash_view.as_ref()?;
        match cmd {
            CMD_EXPORT_JSON => Some(view.export_snapshot_json(&app.snapshot)),
            CMD_EXPORT_SESSIONS_CSV => Some(view.export_sessions_csv()),
            CMD_EXPORT_DAILY_CSV => Some(view.export_daily_csv(&app.snapshot)),
            CMD_EXPORT_CALLS_CSV => Some(view.export_calls_csv()),
            _ => None,
        }
    })
    .flatten();

    let Some(body) = content else {
        return;
    };

    let write_res = std::fs::write(&path, body.as_bytes());
    with_app(|app| {
        if let Some(view) = &mut app.dash_view {
            match write_res {
                Ok(()) => {
                    let fname = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("导出文件");
                    let priv_note = if view.privacy_mode { "（已脱敏）" } else { "" };
                    view.set_banner_status(format!("已导出至 {fname}{priv_note}"), false);
                }
                Err(e) => {
                    view.set_banner_status(format!("导出失败：{e}"), true);
                }
            }
        }
        app.invalidate_dashboard();
    });
}

fn show_session_context_menu_safe(hwnd: HWND, cid: String, anchor: Option<POINT>) {
    let info = with_app(|app| {
        let s = app.lookup_session(&cid)?;
        let cap = launcher::classify_resume_capability(&s, &app.snapshot.sessions);
        let has_ws = launcher::normalize_workspace_path_str(&s.workspace).is_some();
        let is_pinned = app
            .snapshot
            .session_meta
            .get(&cid)
            .is_some_and(|m| m.pinned);
        let sub_cnt = app
            .dash_view
            .as_ref()
            .map(|v| v.count_subagents_of(&cid).0)
            .unwrap_or(0);
        let title = s.title.clone();
        let source = s.source;
        let show_multi = app.snapshot.has_non_antigravity();
        let resumable_parent_id = launcher::find_resumable_parent(&s, &app.snapshot.sessions)
            .map(|p| p.conversation_id.clone());
        let parent_id = resumable_parent_id.clone().or_else(|| match &cap {
            launcher::ResumeCapability::Subagent { parent_id, .. } => parent_id.clone(),
            _ => None,
        });
        Some((
            cap,
            has_ws,
            is_pinned,
            sub_cnt,
            title,
            source,
            show_multi,
            resumable_parent_id,
            parent_id,
        ))
    })
    .flatten();

    let Some((
        cap,
        has_ws,
        is_pinned,
        sub_cnt,
        title,
        source,
        show_multi,
        resumable_parent_id,
        parent_id,
    )) = info
    else {
        return;
    };

    let pt = anchor.unwrap_or_else(|| {
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        p
    });

    // Run TrackPopupMenu completely OUTSIDE `with_app` to avoid re-entrant `APP` RefCell panics.
    let chosen = unsafe {
        let Ok(hmenu) = CreatePopupMenu() else {
            return;
        };
        let add_item = |id: u32, label: &str, enabled: bool| {
            let w = to_wide_null(label);
            let flags = MF_STRING | if enabled { Default::default() } else { MF_GRAYED };
            let _ = AppendMenuW(hmenu, flags, id as usize, wide_pcwstr(&w));
        };

        let blocked_msg = cap
            .disabled_reason_cn()
            .or_else(|| cap.blocked_reason().map(launcher::BlockedReason::message_cn))
            .unwrap_or("不支持终端恢复");
        let cli_bin = launcher::cli_binary_name_for_source(source);

        match &cap {
            launcher::ResumeCapability::CliDirect => {
                let lbl = match (show_multi, cli_bin) {
                    (true, Some(bin)) => format!("在终端继续会话 ({bin})\tEnter"),
                    _ => "在终端继续会话\tEnter".to_string(),
                };
                add_item(CMD_SESS_RESUME, &lbl, true);
            }
            launcher::ResumeCapability::Subagent { .. } if resumable_parent_id.is_some() => {
                let lbl = match (show_multi, cli_bin) {
                    (true, Some(bin)) => format!("在终端继续所属父会话 ({bin})"),
                    _ => "在终端继续所属父会话".to_string(),
                };
                add_item(CMD_SESS_RESUME_PARENT, &lbl, true);
                add_item(CMD_SESS_INSPECT_PARENT, "定位所属父会话", true);
            }
            launcher::ResumeCapability::Subagent {
                parent_id: Some(_), ..
            } => {
                add_item(CMD_SESS_INSPECT_PARENT, "定位所属父会话", true);
                add_item(0, blocked_msg, false);
            }
            _ => {
                add_item(0, blocked_msg, false);
            }
        }
        let copy_cmd_lbl = match (show_multi, cli_bin) {
            (true, Some(bin)) if cap.is_direct_resumable() => {
                format!("复制 {bin} 恢复命令\tCtrl+Shift+C")
            }
            _ => "复制恢复命令\tCtrl+Shift+C".to_string(),
        };
        add_item(CMD_SESS_COPY_CMD, &copy_cmd_lbl, cap.is_direct_resumable());
        add_item(CMD_SESS_OPEN_WS, "打开工作区目录\tO", has_ws);
        let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
        add_item(
            CMD_SESS_TOGGLE_PIN,
            if is_pinned { "取消置顶\tS" } else { "置顶会话\tS" },
            true,
        );
        if sub_cnt > 0 {
            add_item(CMD_SESS_SHOW_SUBS, &format!("查看 {} 个子会话", sub_cnt), true);
        }
        let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
        add_item(CMD_SESS_SHOW_CALLS, "查看调用记录", true);
        add_item(CMD_SESS_COPY_DETAIL, "复制会话明细", true);
        add_item(CMD_SESS_COPY_ID, "复制完整会话 ID", true);

        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            hmenu,
            TPM_LEFTALIGN | TPM_TOPALIGN | TPM_RETURNCMD,
            pt.x,
            pt.y,
            0,
            hwnd,
            None,
        )
        .0 as u32;
        let _ = DestroyMenu(hmenu);
        if cmd == 0 { None } else { Some(cmd) }
    };

    let Some(cmd) = chosen else {
        return;
    };

    let mut auto_hide = false;
    with_app(|app| match cmd {
        CMD_SESS_RESUME => {
            auto_hide = app.handle_resume_session(hwnd, &cid, true);
        }
        CMD_SESS_RESUME_PARENT => {
            if let Some(pid) = resumable_parent_id.as_deref() {
                auto_hide = app.handle_resume_session(hwnd, pid, true);
            }
        }
        CMD_SESS_INSPECT_PARENT => {
            if let Some(pid) = parent_id {
                if let Some(view) = &mut app.dash_view {
                    let _ = view.apply_action(
                        hwnd,
                        ClickAction::InspectSession(pid),
                        &mut app.human,
                        &mut app.light_theme,
                    );
                }
                app.invalidate_dashboard();
            }
        }
        CMD_SESS_COPY_CMD => {
            app.handle_copy_resume_cmd(hwnd, &cid);
        }
        CMD_SESS_OPEN_WS => {
            app.handle_open_workspace(hwnd, &cid);
        }
        CMD_SESS_TOGGLE_PIN => {
            app.handle_toggle_pin(hwnd, &cid);
        }
        CMD_SESS_SHOW_SUBS => {
            if let Some(view) = &mut app.dash_view {
                let _ = view.apply_action(
                    hwnd,
                    ClickAction::FilterSessions(dashboard::SessionFilter::Parent(cid.clone(), title)),
                    &mut app.human,
                    &mut app.light_theme,
                );
            }
            app.invalidate_dashboard();
        }
        CMD_SESS_SHOW_CALLS => {
            if let Some(view) = &mut app.dash_view {
                let _ = view.apply_action(
                    hwnd,
                    ClickAction::ShowSessionCalls(cid.clone()),
                    &mut app.human,
                    &mut app.light_theme,
                );
            }
            app.invalidate_dashboard();
        }
        CMD_SESS_COPY_DETAIL => {
            if let Some(view) = &mut app.dash_view {
                view.selected_session = Some(cid.clone());
                let _ = view.apply_action(
                    hwnd,
                    ClickAction::CopySessionDetail,
                    &mut app.human,
                    &mut app.light_theme,
                );
                view.set_banner_status("已复制会话明细到剪贴板", false);
            }
            app.invalidate_dashboard();
        }
        CMD_SESS_COPY_ID => {
            app.handle_copy_full_session_id(hwnd, &cid);
        }
        _ => {}
    });
    if auto_hide {
        hide_dashboard_safe();
    }
}

fn handle_pill_or_tray_context_menu(hwnd: HWND) {
    let state = with_app(|app| {
        app.pill_menu_open = true;
        let quick_sessions = if app.privacy_mode {
            Vec::new()
        } else {
            build_quick_session_menu_items(&app.snapshot)
        };
        (
            app.light_theme,
            app.topmost,
            app.human,
            app.privacy_mode,
            app.pill_visible,
            app.tray_hover_mode,
            app.notifications_enabled,
            app.hide_on_fullscreen,
            app.monitor_index,
            app.dash_visible,
            app.pill_items,
            quick_sessions,
        )
    });
    if let Some((
        light,
        topmost,
        human,
        privacy_mode,
        pill_visible,
        tray_hover_mode,
        notifications_enabled,
        hide_on_fullscreen,
        mon,
        local_vis,
        items,
        quick_sessions,
    )) = state
    {
        let autostart = is_autostart_enabled();
        let dash_open = local_vis
            || unsafe {
                FindWindowW(w!("GaugeDashboardClass"), None)
                    .map(|h| !h.0.is_null() && IsWindowVisible(h).as_bool())
                    .unwrap_or(false)
            };
        let chosen = show_context_menu(
            hwnd,
            dash_open,
            pill_visible,
            tray_hover_mode,
            light,
            topmost,
            human,
            privacy_mode,
            notifications_enabled,
            hide_on_fullscreen,
            autostart,
            mon,
            items,
            &quick_sessions,
        );
        with_app(|app| app.pill_menu_open = false);
        let mut handled_topmost = false;
        if let Some(cmd) = chosen {
            match cmd {
                CMD_OPEN_DETAIL => toggle_dashboard_safe(),
                CMD_OPEN_SESSIONS_TAB => open_dashboard_to_sessions_tab_safe(),
                c if (CMD_QUICK_RESUME_BASE..CMD_QUICK_RESUME_BASE + 5).contains(&c) => {
                    let idx = (c - CMD_QUICK_RESUME_BASE) as usize;
                    if let Some(item) = quick_sessions.get(idx) {
                        with_app(|app| {
                            let _ = app.handle_resume_session(hwnd, &item.conversation_id, false);
                        });
                    }
                }
                CMD_TOGGLE_TRAY_HOVER => {
                    let restored_top = with_app(|app| {
                        app.tray_hover_mode = !app.tray_hover_mode;
                        app.hide_tray_hover_flyout();
                        if !app.tray_hover_mode {
                            app.pill_visible = true;
                            app.fullscreen_hidden = false;
                            app.pill_renderer.invalidate_position();
                        }
                        app.tray_pin_retries = 6;
                        app.sync_tray_taskbar_pin();
                        app.apply_pill_visibility();
                        app.sync_tray_and_alerts();
                        app.persist_and_sync(hwnd);
                        (!app.tray_hover_mode, app.topmost)
                    });
                    if let Some((true, top)) = restored_top {
                        set_pill_topmost_safe(top, false);
                    }
                    handled_topmost = true;
                }
                CMD_TOGGLE_PILL_VISIBLE => {
                    let restored_top = with_app(|app| {
                        if app.tray_hover_mode {
                            app.tray_hover_mode = false;
                            app.hide_tray_hover_flyout();
                            app.pill_visible = true;
                            app.fullscreen_hidden = false;
                            app.pill_renderer.invalidate_position();
                            app.tray_pin_retries = 6;
                            app.sync_tray_taskbar_pin();
                        } else {
                            app.pill_visible = !app.pill_visible;
                            if app.pill_visible {
                                app.fullscreen_hidden = false;
                            }
                        }
                        app.apply_pill_visibility();
                        app.sync_tray_and_alerts();
                        app.persist_and_sync(hwnd);
                        (app.pill_visible, app.topmost)
                    });
                    if let Some((true, top)) = restored_top {
                        set_pill_topmost_safe(top, false);
                    }
                    handled_topmost = true;
                }
                CMD_REFRESH => {
                    with_app(|app| app.refresh_tokens(true));
                }
                CMD_TOGGLE_LIGHT => {
                    let dh = with_app(|app| {
                        app.light_theme = !app.light_theme;
                        app.redraw_pill();
                        app.persist_and_sync(hwnd);
                        app.invalidate_dashboard();
                        (app.dash_hwnd, app.light_theme)
                    });
                    if let Some((Some(dh), new_light)) = dh {
                        let th = theme_for(new_light);
                        apply_win11_frame(dh, th.is_dark, th.caption_colorref);
                    }
                }
                CMD_TOGGLE_TOPMOST => {
                    set_pill_topmost_safe(!topmost, true);
                    handled_topmost = true;
                }
                CMD_TOGGLE_HIDE_FULLSCREEN => {
                    with_app(|app| {
                        app.hide_on_fullscreen = !app.hide_on_fullscreen;
                        if !app.hide_on_fullscreen && app.fullscreen_hidden {
                            app.fullscreen_hidden = false;
                            app.apply_pill_visibility();
                        }
                        app.persist_and_sync(hwnd);
                    });
                }
                CMD_TOGGLE_NOTIFICATIONS => {
                    with_app(|app| {
                        app.notifications_enabled = !app.notifications_enabled;
                        app.persist_and_sync(hwnd);
                    });
                }
                CMD_TOGGLE_AUTOSTART => {
                    let _ = set_autostart_enabled(!autostart);
                }
                CMD_TOGGLE_HUMAN => {
                    with_app(|app| {
                        app.human = !app.human;
                        app.redraw_pill();
                        app.persist_and_sync(hwnd);
                        app.invalidate_dashboard();
                    });
                }
                CMD_TOGGLE_PRIVACY => {
                    with_app(|app| {
                        app.privacy_mode = !app.privacy_mode;
                        if let Some(view) = &mut app.dash_view {
                            view.privacy_mode = app.privacy_mode;
                        }
                        app.persist_and_sync(hwnd);
                        app.invalidate_dashboard();
                    });
                }
                CMD_REDOCK => {
                    with_app(|app| {
                        let left_hover = app.tray_hover_mode;
                        app.tray_hover_mode = false;
                        app.hide_tray_hover_flyout();
                        if left_hover {
                            app.tray_pin_retries = 6;
                            app.sync_tray_taskbar_pin();
                        }
                        app.pill_visible = true;
                        app.fullscreen_hidden = false;
                        app.custom_pos = None;
                        app.pill_renderer.invalidate_position();
                        app.apply_pill_visibility();
                        app.redraw_pill();
                        app.sync_tray_and_alerts();
                        app.persist_and_sync(hwnd);
                    });
                    if topmost {
                        set_pill_topmost_safe(true, false);
                        handled_topmost = true;
                    }
                }
                CMD_QUIT => {
                    with_app(|app| {
                        if let Some(tray) = &mut app.tray {
                            tray.remove();
                        }
                    });
                    destroy_dashboard_safe();
                    unsafe {
                        PostQuitMessage(0);
                    }
                    handled_topmost = true;
                }
                CMD_PILL_ITEMS_RESET => {
                    with_app(|app| {
                        app.pill_items = PILL_ITEMS_DEFAULT;
                        app.pill_renderer.invalidate_position();
                        app.redraw_pill();
                        app.persist_and_sync(hwnd);
                    });
                }
                c if (CMD_PILL_ITEM_BASE..CMD_PILL_ITEM_BASE + 9).contains(&c) => {
                    let bit = 1u32 << (c - CMD_PILL_ITEM_BASE);
                    with_app(|app| {
                        let next = (app.pill_items ^ bit) & PILL_ITEMS_ALL_MASK;
                        if next != 0 {
                            app.pill_items = next;
                            app.pill_renderer.invalidate_position();
                            app.redraw_pill();
                            app.persist_and_sync(hwnd);
                        }
                    });
                }
                c if c >= CMD_MONITOR_BASE => {
                    with_app(|app| {
                        app.monitor_index = (c - CMD_MONITOR_BASE) as usize;
                        app.custom_pos = None;
                        app.pill_renderer.invalidate_position();
                        app.redraw_pill();
                        app.persist_and_sync(hwnd);
                    });
                    if topmost {
                        set_pill_topmost_safe(true, false);
                        handled_topmost = true;
                    }
                }
                _ => {}
            }
        }
        if !handled_topmost {
            ensure_pill_topmost_if_needed(true);
        }
    } else {
        with_app(|app| app.pill_menu_open = false);
    }
}

unsafe extern "system" fn pill_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_WINDOWPOSCHANGING => {
            let guard_on = with_app(|app| app.is_pill_effectively_topmost() && !app.pill_menu_open)
                .unwrap_or(false);
            if guard_on && lparam.0 != 0 {
                let wp = &mut *(lparam.0 as *mut WINDOWPOS);
                guard_topmost_windowpos(wp, true);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_ACTIVATEAPP => {
            // Re-verify pill topmost whenever the app loses foreground activation (e.g. clicking another window).
            if wparam.0 == 0 {
                ensure_pill_topmost_if_needed(false);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_SETCURSOR => {
            if let Ok(hand) = LoadCursorW(None, IDC_HAND) {
                SetCursor(hand);
            }
            LRESULT(1)
        }
        WM_LBUTTONDOWN => {
            let mut pt = POINT {
                x: (lparam.0 & 0xFFFF) as i16 as i32,
                y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
            };
            let _ = ClientToScreen(hwnd, &mut pt);
            let allow_drag = with_app(|app| {
                if app.tray_hover_mode {
                    app.drag_down = false;
                    app.dragging = false;
                    return false;
                }
                app.drag_down = true;
                app.dragging = false;
                app.drag_start_cursor = (pt.x, pt.y);
                app.drag_start_win = app.pill_pos;
                true
            })
            .unwrap_or(false);
            if allow_drag {
                let _ = SetCapture(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let mut move_target: Option<(i32, i32)> = None;
            let mut pt = POINT {
                x: (lparam.0 & 0xFFFF) as i16 as i32,
                y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
            };
            let _ = ClientToScreen(hwnd, &mut pt);
            with_app(|app| {
                if app.drag_down {
                    let dx = pt.x - app.drag_start_cursor.0;
                    let dy = pt.y - app.drag_start_cursor.1;
                    if !app.dragging && (dx.abs() >= 4 || dy.abs() >= 4) {
                        app.dragging = true;
                    }
                    if app.dragging {
                        let (pw, ph) = app.pill_renderer.size_px();
                        let sh = app.pill_renderer.shadow_px();
                        let (nx, ny) = clamp_pos_to_monitors(
                            app.drag_start_win.0 + dx,
                            app.drag_start_win.1 + dy,
                            pw,
                            ph,
                            sh,
                        );
                        app.pill_pos = (nx, ny);
                        app.custom_pos = Some((nx, ny));
                        move_target = Some((nx, ny));
                    }
                    return;
                }
                if !app.pill_tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                    app.pill_tracking = true;
                }
                if !app.pill_hovered {
                    app.pill_hovered = true;
                    app.redraw_pill();
                }
            });
            if let Some((nx, ny)) = move_target {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    nx,
                    ny,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            with_app(|app| {
                app.pill_tracking = false;
                if app.pill_hovered && !app.dragging {
                    app.pill_hovered = false;
                    app.redraw_pill();
                }
            });
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            let keep_top = with_app(|app| {
                if app.dragging {
                    let (pw, ph) = app.pill_renderer.size_px();
                    let sh = app.pill_renderer.shadow_px();
                    let settled = clamp_pos_to_work_area(app.pill_pos.0, app.pill_pos.1, pw, ph, sh);
                    app.pill_pos = settled;
                    app.custom_pos = Some(settled);
                    app.redraw_pill();
                }
                app.drag_down = false;
                app.dragging = false;
                app.topmost
            })
            .unwrap_or(false);
            if keep_top {
                ensure_pill_topmost_if_needed(true);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (was_dragging, settled_pos, keep_topmost) = with_app(|app| {
                let was = app.dragging;
                app.drag_down = false;
                app.dragging = false;
                let mut moved = None;
                if was {
                    let (pw, ph) = app.pill_renderer.size_px();
                    let sh = app.pill_renderer.shadow_px();
                    let settled = clamp_pos_to_work_area(app.pill_pos.0, app.pill_pos.1, pw, ph, sh);
                    app.pill_pos = settled;
                    if let Some(mon) = monitor_index_for_point(settled.0 + pw / 2, settled.1 + ph / 2) {
                        app.monitor_index = mon;
                    }
                    app.custom_pos = Some(settled);
                    app.pill_renderer.invalidate_position();
                    app.persist_and_sync(hwnd);
                    moved = Some(settled);
                }
                (was, moved, app.topmost)
            })
            .unwrap_or((false, None, true));
            let _ = ReleaseCapture();
            if let Some((sx, sy)) = settled_pos {
                let _ = SetWindowPos(
                    hwnd,
                    if keep_topmost { HWND_TOPMOST } else { HWND_NOTOPMOST },
                    sx,
                    sy,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE,
                );
                with_app(|app| app.redraw_pill());
            }
            if !was_dragging {
                with_app(|app| {
                    if app.tray_hover_mode {
                        app.hide_tray_hover_flyout();
                    }
                });
                toggle_dashboard_safe();
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            with_app(|app| {
                if app.tray_hover_mode {
                    app.hide_tray_hover_flyout();
                }
            });
            handle_pill_or_tray_context_menu(hwnd);
            LRESULT(0)
        }
        WM_APP_TRAY => {
            let ev = (lparam.0 & 0xFFFF) as u32;
            match ev {
                WM_MOUSEMOVE => {
                    with_app(|app| {
                        if !app.tray_hover_mode || app.pill_menu_open || app.self_test_mode {
                            return;
                        }
                        if app
                            .tray_hover_suppress_until
                            .map(|t| Instant::now() < t)
                            .unwrap_or(false)
                        {
                            return;
                        }
                        let icon_rect = app
                            .tray
                            .as_ref()
                            .and_then(TrayIcon::get_rect)
                            .unwrap_or_else(|| {
                                let mut pt = POINT::default();
                                let _ = GetCursorPos(&mut pt);
                                RECT {
                                    left: pt.x - 12,
                                    top: pt.y - 12,
                                    right: pt.x + 12,
                                    bottom: pt.y + 12,
                                }
                            });
                        app.show_tray_hover_flyout(icon_rect);
                    });
                }
                WM_LBUTTONUP => {
                    with_app(|app| {
                        app.tray_hover_suppress_until = Some(Instant::now() + Duration::from_millis(600));
                        app.hide_tray_hover_flyout();
                    });
                    toggle_dashboard_safe();
                }
                WM_RBUTTONUP => {
                    with_app(|app| {
                        app.tray_hover_suppress_until = Some(Instant::now() + Duration::from_millis(600));
                        app.hide_tray_hover_flyout();
                    });
                    handle_pill_or_tray_context_menu(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_SYNC_PREFS => {
            let p = load_user_prefs();
            let top = p.topmost;
            with_app(|app| {
                app.apply_synced_prefs(p);
                if app.tray_pin_retries > 0 {
                    app.sync_tray_taskbar_pin();
                }
                app.apply_pill_visibility();
                app.sync_tray_and_alerts();
                app.redraw_pill();
            });
            set_pill_topmost_safe(top, false);
            LRESULT(0)
        }
        WM_APP_SYNC_META => {
            with_app(|app| {
                app.scanner.reload_session_meta();
                app.refresh_tokens(true);
            });
            LRESULT(0)
        }
        WM_TIMER => {
            match wparam.0 {
                TIMER_TELEMETRY => {
                    with_app(|app| {
                        app.telemetry.sample();
                        if app.tray_pin_retries > 0 {
                            app.sync_tray_taskbar_pin();
                        }
                        app.check_fullscreen_guard();
                        app.redraw_pill();
                        app.invalidate_dashboard();
                    });
                    ensure_pill_topmost_if_needed(false);
                }
                TIMER_TOKENS => {
                    with_app(|app| {
                        app.refresh_tokens(false);
                    });
                }
                TIMER_TRAY_HOVER => {
                    with_app(|app| {
                        app.tick_tray_hover_guard();
                    });
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DPICHANGED | WM_DISPLAYCHANGE => {
            let sys_dpi = GetDpiForSystem().max(96);
            let keep_top = with_app(|app| {
                if app.icons.reload_tray_for_dpi(sys_dpi) {
                    app.sync_tray_and_alerts();
                }
                app.pill_renderer.invalidate_position();
                app.redraw_pill();
                app.topmost
            })
            .unwrap_or(false);
            if keep_top {
                set_pill_topmost_safe(true, false);
            }
            LRESULT(0)
        }
        WM_SETTINGCHANGE if wparam.0 == SPI_SETWORKAREA => {
            let keep_top = with_app(|app| {
                app.pill_renderer.invalidate_position();
                app.redraw_pill();
                app.topmost
            })
            .unwrap_or(false);
            if keep_top {
                set_pill_topmost_safe(true, false);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            with_app(|app| {
                if let Some(tray) = &mut app.tray {
                    tray.remove();
                }
            });
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => {
            let readd = with_app(|app| app.taskbar_created_msg != 0 && msg == app.taskbar_created_msg)
                .unwrap_or(false);
            if readd {
                with_app(|app| {
                    let tip = app.build_tray_tooltip();
                    if let Some(tray) = &mut app.tray {
                        tray.add(&tip);
                    }
                    app.tray_pin_retries = 6;
                    app.sync_tray_taskbar_pin();
                });
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

unsafe extern "system" fn dash_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_GETMINMAXINFO => {
            let mmi = &mut *(lparam.0 as *mut MINMAXINFO);
            let scale = window_dpi_scale(hwnd);
            mmi.ptMinTrackSize.x = (840.0 * scale).round() as i32;
            mmi.ptMinTrackSize.y = (560.0 * scale).round() as i32;
            LRESULT(0)
        }
        WM_SYSCOMMAND if (wparam.0 & 0xFFF0) == SC_MINIMIZE => {
            // Tool-owned taskbar-less windows minimize to an ugly bottom-left titlebar stub on Windows.
            // Fold cleanly back into the floating pill instead (releasing surface RAM immediately).
            let dash_only = with_app(|app| app.dashboard_only).unwrap_or(false);
            if !dash_only {
                hide_dashboard_safe();
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_SIZE if wparam.0 == SIZE_MINIMIZED => {
            let dash_only = with_app(|app| app.dashboard_only).unwrap_or(false);
            if !dash_only {
                hide_dashboard_safe();
            }
            LRESULT(0)
        }
        WM_SIZE => {
            with_app(|app| {
                if let Some(view) = &mut app.dash_view {
                    view.invalidate_surface();
                }
            });
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let wp_dpi = (wparam.0 & 0xFFFF) as u32;
            let new_dpi = if wp_dpi >= 96 {
                wp_dpi
            } else {
                (window_dpi_scale(hwnd) * 96.0).round() as u32
            };
            with_app(|app| {
                app.icons.apply_dashboard_icons_for_dpi(hwnd, new_dpi);
                if let Some(view) = &mut app.dash_view {
                    view.invalidate_surface();
                }
            });
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let x_px = (lparam.0 & 0xFFFF) as i16 as f32;
            let y_px = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
            let scale = window_dpi_scale(hwnd);
            let hit_cid = with_app(|app| {
                app.dash_view
                    .as_ref()
                    .and_then(|v| v.hit_test_session_row(x_px / scale, y_px / scale))
            })
            .flatten();
            if let Some(cid) = hit_cid {
                show_session_context_menu_safe(hwnd, cid, None);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x_px = (lparam.0 & 0xFFFF) as i16 as f32;
            let y_px = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
            let scale = window_dpi_scale(hwnd);
            let (need_track, need_paint) = with_app(|app| {
                let need = !app.dash_tracking;
                if need {
                    app.dash_tracking = true;
                }
                let paint = app
                    .dash_view
                    .as_mut()
                    .map(|v| v.on_mouse_move(x_px / scale, y_px / scale))
                    .unwrap_or(false);
                (need, paint)
            })
            .unwrap_or((false, false));
            if need_track {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
            }
            if need_paint {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let need_paint = with_app(|app| {
                app.dash_tracking = false;
                app.dash_view.as_mut().map(|v| v.on_mouse_leave()).unwrap_or(false)
            })
            .unwrap_or(false);
            if need_paint {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if (lparam.0 & 0xFFFF) as u32 == 1 {
                let hovering = with_app(|app| {
                    app.dash_view.as_ref().map(|v| v.is_hovering_interactive()).unwrap_or(false)
                })
                .unwrap_or(false);
                if let Ok(cur) = LoadCursorW(None, if hovering { IDC_HAND } else { IDC_ARROW }) {
                    SetCursor(cur);
                    return LRESULT(1);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONUP => {
            let x_px = (lparam.0 & 0xFFFF) as i16 as f32;
            let y_px = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
            let scale = window_dpi_scale(hwnd);
            let action = with_app(|app| {
                app.dash_view
                    .as_ref()
                    .and_then(|v| v.hit_test_click(x_px / scale, y_px / scale))
            })
            .flatten();
            if let Some(act) = action {
                if matches!(act, ClickAction::ExportMenu) {
                    show_export_menu_safe(hwnd);
                    return LRESULT(0);
                }
                if let ClickAction::SessionMoreMenu(cid) = act {
                    let anchor = with_app(|app| {
                        app.dash_view
                            .as_ref()
                            .and_then(|v| v.detail_more_btn_rect)
                            .map(|r| {
                                let sc = window_dpi_scale(hwnd);
                                let mut p = POINT {
                                    x: (r.left * sc).round() as i32,
                                    y: ((r.bottom + 4.0) * sc).round() as i32,
                                };
                                let _ = ClientToScreen(hwnd, &mut p);
                                p
                            })
                    })
                    .flatten();
                    show_session_context_menu_safe(hwnd, cid, anchor);
                    return LRESULT(0);
                }
                let mut auto_hide = false;
                with_app(|app| {
                    match act {
                        ClickAction::PickPricingRow(idx) => {
                            app.load_pricing_row_into_editor(idx, true);
                            app.invalidate_dashboard();
                            return;
                        }
                        ClickAction::NewPricingEntry => {
                            app.clear_pricing_editor_for_new();
                            app.invalidate_dashboard();
                            return;
                        }
                        ClickAction::SavePricingEntry => {
                            app.save_pricing_from_editor();
                            return;
                        }
                        ClickAction::ResetPricingEntry => {
                            app.reset_pricing_from_editor();
                            return;
                        }
                        ClickAction::SaveBudgetEntry => {
                            app.save_budget_from_editor();
                            return;
                        }
                        ClickAction::ClearBudgetEntry => {
                            app.clear_budget_from_editor();
                            return;
                        }
                        ClickAction::ApplyCustomDate => {
                            app.apply_custom_dates_from_editor();
                            return;
                        }
                        ClickAction::ClearSearch => {
                            app.pricing_edits.set_search_text("");
                            if let Some(view) = &mut app.dash_view {
                                view.search_query.clear();
                                view.scroll_rows = 0;
                            }
                            app.invalidate_dashboard();
                            return;
                        }
                        ClickAction::TogglePinSession(cid) => {
                            app.handle_toggle_pin(hwnd, &cid);
                            return;
                        }
                        ClickAction::ResumeSession(cid) => {
                            auto_hide = app.handle_resume_session(hwnd, &cid, true);
                            return;
                        }
                        ClickAction::CopyResumeCmd(cid) => {
                            app.handle_copy_resume_cmd(hwnd, &cid);
                            return;
                        }
                        ClickAction::OpenWorkspace(cid) => {
                            app.handle_open_workspace(hwnd, &cid);
                            return;
                        }
                        _ => {}
                    }
                    let mut need_refresh = false;
                    let mut prefs_changed = false;
                    if let Some(view) = &mut app.dash_view {
                        let res = view.apply_action(hwnd, act, &mut app.human, &mut app.light_theme);
                        need_refresh = res.0;
                        prefs_changed = res.1;
                        app.cost_mode = view.cost_mode;
                        app.group_subagents = view.group_subagents;
                        app.privacy_mode = view.privacy_mode;
                        app.fresh_only = view.fresh_only;
                        app.date_scope = view.scope;
                    }
                    let (p_act, b_act, s_act, d_act) = app
                        .dash_view
                        .as_ref()
                        .map(|v| {
                            (
                                v.is_pricing_editor_active(),
                                v.is_budget_editor_active(),
                                v.is_search_active(),
                                v.is_custom_date_active(),
                            )
                        })
                        .unwrap_or((false, false, false, false));
                    if !p_act && !b_act {
                        app.pricing_edits.hide_five_edits(hwnd);
                    }
                    if !s_act {
                        app.pricing_edits.sync_search_layout(hwnd, 96.0, app.light_theme, None);
                    }
                    if !d_act {
                        app.pricing_edits.sync_date_layout(hwnd, 96.0, app.light_theme, None);
                    }
                    if prefs_changed {
                        app.persist_and_sync(hwnd);
                    }
                    if need_refresh {
                        app.refresh_tokens(true);
                    }
                    app.redraw_pill();
                    app.invalidate_dashboard();
                });
                if auto_hide {
                    hide_dashboard_safe();
                }
            }
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
            with_app(|app| app.pricing_edits.handle_ctlcolor(hdc, app.light_theme))
                .unwrap_or_else(|| DefWindowProcW(hwnd, msg, wparam, lparam))
        }
        WM_COMMAND => {
            let code = ((wparam.0 >> 16) & 0xFFFF) as u16;
            let ctrl = HWND(lparam.0 as *mut core::ffi::c_void);
            if code == EN_CHANGE {
                with_app(|app| {
                    if app.pricing_edits.is_search_hwnd(ctrl)
                        && !app.pricing_edits.suppress_search_change
                    {
                        let q = app.pricing_edits.read_search_text();
                        if let Some(view) = &mut app.dash_view {
                            if view.search_query != q {
                                view.search_query = q;
                                view.scroll_rows = 0;
                                app.invalidate_dashboard();
                            }
                        }
                    }
                });
                return LRESULT(0);
            }
            if code == EN_SETFOCUS || code == EN_KILLFOCUS {
                with_app(|app| {
                    let is_set = code == EN_SETFOCUS;
                    if let Some(idx) = app.pricing_edits.index_of(ctrl) {
                        if let Some(view) = &mut app.dash_view {
                            if is_set {
                                view.pricing_focused_field = Some(idx);
                            } else if view.pricing_focused_field == Some(idx) {
                                view.pricing_focused_field = None;
                            }
                        }
                    } else if app.pricing_edits.is_search_hwnd(ctrl) {
                        if let Some(view) = &mut app.dash_view {
                            view.search_focused = is_set;
                        }
                    } else if let Some(didx) = app.pricing_edits.date_index_of(ctrl) {
                        if let Some(view) = &mut app.dash_view {
                            if is_set {
                                view.custom_date_focused = Some(didx);
                            } else if view.custom_date_focused == Some(didx) {
                                view.custom_date_focused = None;
                            }
                        }
                    }
                    app.invalidate_dashboard();
                });
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_APP_SYNC_PREFS => {
            let p = load_user_prefs();
            let th = theme_for(p.light_theme);
            apply_win11_frame(hwnd, th.is_dark, th.caption_colorref);
            with_app(|app| {
                app.apply_synced_prefs(p);
            });
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_APP_SYNC_META => {
            with_app(|app| {
                app.scanner.reload_session_meta();
                app.refresh_tokens(true);
            });
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16;
            with_app(|app| {
                if let Some(view) = &mut app.dash_view {
                    view.on_scroll(delta);
                }
            });
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let vk = wparam.0 as u16;
            let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
            let shift = (GetKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
            let alt = (GetKeyState(VK_MENU.0 as i32) as u16 & 0x8000) != 0;
            if vk == VK_ESCAPE.0 {
                hide_dashboard_safe();
                return LRESULT(0);
            }
            if vk == 0xBF && !ctrl && !alt {
                // '/' -> focus inline search (stays on Calls tab, otherwise jumps to Sessions)
                with_app(|app| {
                    if let Some(view) = &mut app.dash_view {
                        if view.active_tab != Tab::Calls {
                            view.active_tab = Tab::Sessions;
                        }
                        view.scroll_rows = 0;
                    }
                    app.pricing_edits.hide_five_edits(hwnd);
                });
                let _ = InvalidateRect(hwnd, None, false);
                let _ = UpdateWindow(hwnd);
                with_app(|app| {
                    app.sync_inline_controls(hwnd);
                    app.pricing_edits.focus_search();
                });
                return LRESULT(0);
            }

            // Sessions tab management shortcuts:
            //   Enter          -> Resume selected session in terminal (auto-hide on success)
            //   Ctrl+Shift+C   -> Copy resume command
            //   O (!ctrl !alt) -> Open workspace folder
            //   S (!ctrl !alt) -> Toggle pin
            let sess_ctx = with_app(|app| {
                app.dash_view
                    .as_ref()
                    .filter(|v| v.active_tab == Tab::Sessions)
                    .and_then(|v| v.selected_session.clone())
            })
            .flatten();
            if let Some(cid) = sess_ctx {
                if vk == VK_RETURN.0 && !ctrl && !alt {
                    let auto_hide = with_app(|app| app.handle_resume_session(hwnd, &cid, true))
                        .unwrap_or(false);
                    if auto_hide {
                        hide_dashboard_safe();
                    }
                    return LRESULT(0);
                }
                if vk == 0x43 && ctrl && shift && !alt {
                    with_app(|app| app.handle_copy_resume_cmd(hwnd, &cid));
                    return LRESULT(0);
                }
                if vk == 0x4F && !ctrl && !alt && !shift {
                    with_app(|app| app.handle_open_workspace(hwnd, &cid));
                    return LRESULT(0);
                }
                if vk == 0x53 && !ctrl && !alt && !shift {
                    with_app(|app| app.handle_toggle_pin(hwnd, &cid));
                    return LRESULT(0);
                }
            }

            let action = match vk {
                0x31 if !ctrl && !alt => Some(ClickAction::SelectTab(Tab::Overview)), // '1'
                0x32 if !ctrl && !alt => Some(ClickAction::SelectTab(Tab::Daily)),    // '2'
                0x33 if !ctrl && !alt => Some(ClickAction::SelectTab(Tab::Weekly)),   // '3'
                0x34 if !ctrl && !alt => Some(ClickAction::SelectTab(Tab::Models)),   // '4'
                0x35 if !ctrl && !alt => Some(ClickAction::SelectTab(Tab::Sessions)), // '5'
                0x36 if !ctrl && !alt => Some(ClickAction::ToggleSystemView),         // '6'
                0x4C if !ctrl && !alt => Some(ClickAction::SelectTab(Tab::Calls)),    // 'L' (call log)
                0x42 if !ctrl && !alt => Some(ClickAction::OpenBudgetView),           // 'B'
                0x44 if !ctrl && !alt => Some(ClickAction::ToggleCostMode),           // 'D'
                0x50 if !ctrl && !alt => Some(ClickAction::TogglePrivacyMode),        // 'P'
                0x54 if !ctrl && !alt => Some(ClickAction::ToggleFreshOnly),          // 'T'
                0x52 if !alt => Some(ClickAction::Refresh),                           // 'R'
                x if x == VK_F5.0 => Some(ClickAction::Refresh),
                x if x == VK_BACK.0 => Some(ClickAction::ClearSessionFilter),
                0x43 if ctrl && !shift && !alt => Some(ClickAction::CopySummary),     // Ctrl+C
                _ => None,
            };
            if let Some(act) = action {
                with_app(|app| {
                    let mut need_refresh = false;
                    let mut prefs_changed = false;
                    if let Some(view) = &mut app.dash_view {
                        let res = view.apply_action(hwnd, act, &mut app.human, &mut app.light_theme);
                        need_refresh = res.0;
                        prefs_changed = res.1;
                        app.cost_mode = view.cost_mode;
                        app.group_subagents = view.group_subagents;
                        app.privacy_mode = view.privacy_mode;
                        app.fresh_only = view.fresh_only;
                        app.date_scope = view.scope;
                    }
                    let (p_act, b_act, s_act, d_act) = app
                        .dash_view
                        .as_ref()
                        .map(|v| {
                            (
                                v.is_pricing_editor_active(),
                                v.is_budget_editor_active(),
                                v.is_search_active(),
                                v.is_custom_date_active(),
                            )
                        })
                        .unwrap_or((false, false, false, false));
                    if !p_act && !b_act {
                        app.pricing_edits.hide_five_edits(hwnd);
                    }
                    if !s_act {
                        app.pricing_edits.sync_search_layout(hwnd, 96.0, app.light_theme, None);
                    }
                    if !d_act {
                        app.pricing_edits.sync_date_layout(hwnd, 96.0, app.light_theme, None);
                    }
                    if prefs_changed {
                        app.persist_and_sync(hwnd);
                    }
                    if need_refresh {
                        app.refresh_tokens(true);
                    }
                    app.redraw_pill();
                    app.invalidate_dashboard();
                });
                return LRESULT(0);
            }
            if vk == VK_UP.0 || vk == VK_DOWN.0 {
                let delta = if vk == VK_UP.0 { -1 } else { 1 };
                let changed = with_app(|app| {
                    app.dash_view.as_mut().map(|v| v.step_session(delta)).unwrap_or(false)
                })
                .unwrap_or(false);
                if changed {
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let dpi = window_dpi_scale(hwnd) * 96.0;
            with_app(|app| {
                if let Some(view) = &mut app.dash_view {
                    let _ = view.render_to_hdc(
                        &app.factories,
                        &app.fonts,
                        hwnd,
                        Some(hdc),
                        dpi,
                        &app.snapshot,
                        &app.telemetry,
                        app.human,
                        app.light_theme,
                    );
                }
            });
            let _ = EndPaint(hwnd, &ps);
            with_app(|app| app.sync_inline_controls(hwnd));
            LRESULT(0)
        }
        WM_PRINTCLIENT => {
            let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
            let dpi = window_dpi_scale(hwnd) * 96.0;
            with_app(|app| {
                if let Some(view) = &mut app.dash_view {
                    let _ = view.render_to_hdc(
                        &app.factories,
                        &app.fonts,
                        hwnd,
                        Some(hdc),
                        dpi,
                        &app.snapshot,
                        &app.telemetry,
                        app.human,
                        app.light_theme,
                    );
                }
            });
            LRESULT(0)
        }
        WM_TIMER => {
            match wparam.0 {
                TIMER_TELEMETRY => {
                    with_app(|app| {
                        app.telemetry.sample();
                        app.invalidate_dashboard();
                    });
                }
                TIMER_TOKENS => {
                    with_app(|app| {
                        app.refresh_tokens(false);
                    });
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_ACTIVATEAPP => {
            if wparam.0 == 0 {
                ensure_pill_topmost_if_needed(false);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_CLOSE => {
            hide_dashboard_safe();
            LRESULT(0)
        }
        WM_DESTROY => {
            let dash_only = with_app(|app| {
                app.dash_visible = false;
                app.dash_hwnd = None;
                app.dash_view = None;
                app.pricing_edits.reset_handles();
                app.icons.clear_dashboard_icons();
                app.dashboard_only
            })
            .unwrap_or(false);
            if dash_only {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

struct CliOpts {
    data_dirs: Vec<(stats::SourceKind, PathBuf)>,
    discovery_filter: stats::SourceDiscoveryFilter,
    pricing_path: Option<PathBuf>,
    no_archive: bool,
    mini: Option<bool>,
    tray_hover: Option<bool>,
    light: Option<bool>,
    cost_mode: Option<bool>,
    privacy_mode: Option<bool>,
    open_detail: bool,
    dashboard_only: bool,
    human: Option<bool>,
    monitor: Option<usize>,
    allow_multiple: bool,
    self_test: bool,
}

fn parse_args_from<I, S>(iter: I, discover_if_empty: bool) -> CliOpts
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut opts = CliOpts {
        data_dirs: Vec::new(),
        discovery_filter: stats::SourceDiscoveryFilter::All,
        pricing_path: None,
        no_archive: false,
        mini: None,
        tray_hover: None,
        light: None,
        cost_mode: None,
        privacy_mode: None,
        open_detail: false,
        dashboard_only: false,
        human: None,
        monitor: None,
        allow_multiple: false,
        self_test: false,
    };
    let mut agy_only = false;
    let mut agent_specs: Vec<String> = Vec::new();
    let mut args = iter.into_iter().map(Into::into);
    while let Some(arg) = args.next() {
        if let Some(csv) = arg
            .strip_prefix("--agents=")
            .or_else(|| arg.strip_prefix("--agent="))
        {
            agent_specs.push(csv.to_string());
            continue;
        }
        match arg.as_str() {
            "--mini" => opts.mini = Some(true),
            "--tray-hover" => opts.tray_hover = Some(true),
            "--no-tray-hover" => opts.tray_hover = Some(false),
            "--light" => opts.light = Some(true),
            "--dark" => opts.light = Some(false),
            "--cost" | "--usd" => opts.cost_mode = Some(true),
            "--tokens" => opts.cost_mode = Some(false),
            "--redact" | "--privacy" => opts.privacy_mode = Some(true),
            "--no-redact" => opts.privacy_mode = Some(false),
            "--no-archive" => opts.no_archive = true,
            "--agy-only" => agy_only = true,
            "--agents" | "--agent" => {
                if let Some(v) = args.next() {
                    agent_specs.push(v);
                }
            }
            "--open-detail" => opts.open_detail = true,
            "--dashboard-only" => opts.dashboard_only = true,
            "--exact" => opts.human = Some(false),
            "--allow-multiple" => opts.allow_multiple = true,
            "--self-test" => opts.self_test = true,
            "--monitor" => {
                if let Some(v) = args.next() {
                    opts.monitor = v.parse().ok();
                }
            }
            "--data-dir" => {
                if let Some(v) = args.next() {
                    let p = PathBuf::from(v);
                    let kind = stats::SourceKind::infer_from_path(&p);
                    opts.data_dirs.push((kind, p));
                }
            }
            "--pricing" => {
                if let Some(v) = args.next() {
                    opts.pricing_path = Some(PathBuf::from(v));
                }
            }
            _ => {}
        }
    }
    opts.discovery_filter = if agy_only {
        stats::SourceDiscoveryFilter::AgyOnly
    } else if !agent_specs.is_empty() {
        stats::SourceDiscoveryFilter::parse_csv(&agent_specs.join(","))
            .unwrap_or(stats::SourceDiscoveryFilter::All)
    } else {
        stats::SourceDiscoveryFilter::All
    };
    if discover_if_empty && opts.data_dirs.is_empty() {
        opts.data_dirs = IncrementalScanner::discover_data_dirs(&opts.discovery_filter);
    }
    opts
}

fn parse_args() -> CliOpts {
    parse_args_from(std::env::args().skip(1), true)
}

fn run_app(opts: CliOpts) -> Result<()> {
    enable_dpi_awareness();
    let _guard = if !opts.allow_multiple && !opts.self_test && !opts.dashboard_only {
        match SingleInstanceGuard::acquire() {
            Some(g) => Some(g),
            None => return Ok(()),
        }
    } else {
        None
    };

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let mut prefs = if opts.self_test {
        UserPrefs::default()
    } else {
        load_user_prefs()
    };
    if let Some(v) = opts.mini {
        if v {
            let trimmed = prefs.pill_items & !(PILL_ITEM_CPU | PILL_ITEM_RAM);
            prefs.pill_items = if trimmed != 0 { trimmed } else { PILL_ITEM_TOTAL };
        } else {
            prefs.pill_items |= PILL_ITEM_CPU | PILL_ITEM_RAM;
        }
    }
    if let Some(v) = opts.tray_hover {
        prefs.tray_hover_mode = v;
    }
    if let Some(v) = opts.light {
        prefs.light_theme = v;
    }
    if let Some(v) = opts.human {
        prefs.human = v;
    }
    if let Some(v) = opts.cost_mode {
        prefs.cost_mode = v;
    }
    if let Some(v) = opts.privacy_mode {
        prefs.privacy_mode = v;
    }
    if let Some(v) = opts.monitor {
        prefs.monitor_index = v;
    }

    let factories = D2DFactories::new()?;
    let fonts = Fonts::new(&factories)?;
    let pricing_override = opts
        .pricing_path
        .clone()
        .or_else(pricing::PricingCatalog::default_override_path);
    let mut scanner = IncrementalScanner::new_multi(
        opts.data_dirs.clone(),
        pricing_override,
        quota::StatuslineReader::default_path(),
        quota::BudgetStore::default_path(),
        archive::UsageArchive::default_path(),
        !opts.no_archive && !opts.self_test,
    )
    .with_session_meta(if opts.self_test {
        None
    } else {
        session_meta::SessionMetaStore::default_path()
    });
    let snapshot = scanner.scan();
    let telemetry = SystemTelemetry::new(90);

    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let arrow = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
        let sys_dpi = GetDpiForSystem().max(96);
        let ((big_w, big_h), (sm_w, sm_h)) = system_icon_metrics_for_dpi(sys_dpi);
        let icon_big = load_res_icon(RES_ICON_NORMAL, big_w, big_h);
        let icon_sm = {
            let sm = load_res_icon(RES_ICON_NORMAL, sm_w, sm_h);
            if !sm.0.is_null() { sm } else { icon_big }
        };
        let icons = AppIcons::new(icon_sm, sys_dpi);

        let pill_cls = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(pill_wndproc),
            hInstance: hinst.into(),
            hIcon: icon_big,
            hIconSm: icon_sm,
            hCursor: arrow,
            lpszClassName: w!("GaugePillClass"),
            ..Default::default()
        };
        RegisterClassExW(&pill_cls);

        unsafe extern "system" fn owner_wndproc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        let owner_cls = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(owner_wndproc),
            hInstance: hinst.into(),
            lpszClassName: w!("GaugeOwnerClass"),
            ..Default::default()
        };
        RegisterClassExW(&owner_cls);

        let dash_cls = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(dash_wndproc),
            hInstance: hinst.into(),
            hIcon: icon_big,
            hIconSm: icon_sm,
            hCursor: arrow,
            lpszClassName: w!("GaugeDashboardClass"),
            ..Default::default()
        };
        RegisterClassExW(&dash_cls);

        let pill_ex = WS_EX_LAYERED
            | WS_EX_TOOLWINDOW
            | WS_EX_NOACTIVATE
            | if prefs.topmost { WS_EX_TOPMOST } else { Default::default() };

        let pill_hwnd = if opts.dashboard_only {
            HWND::default()
        } else {
            CreateWindowExW(
                pill_ex,
                w!("GaugePillClass"),
                w!("Gauge Pill"),
                WS_POPUP,
                0,
                0,
                10,
                10,
                None,
                None,
                hinst,
                None,
            )?
        };

        let taskbar_created_msg = register_taskbar_created_msg();
        let tray = if !opts.dashboard_only && !opts.self_test && !pill_hwnd.0.is_null() {
            let init_tip = if prefs.tray_hover_mode { "" } else { "Gauge · 初始化中" };
            Some(TrayIcon::new(pill_hwnd, icons.tray_normal, init_tip))
        } else {
            None
        };

        let initial_total = snapshot.agg.total.total_tokens();
        let state = AppState {
            factories,
            fonts,
            icons,
            scanner,
            snapshot,
            telemetry,
            pill_hwnd,
            pill_renderer: PillRenderer::new(),
            pill_pos: (0, 0),
            custom_pos: prefs.custom_pos,
            drag_down: false,
            dragging: false,
            drag_start_cursor: (0, 0),
            drag_start_win: (0, 0),
            pill_hovered: false,
            pill_tracking: false,
            pill_menu_open: false,
            topmost_unyielding_blocker: None,
            light_theme: prefs.light_theme,
            topmost: prefs.topmost,
            human: prefs.human,
            cost_mode: prefs.cost_mode,
            group_subagents: prefs.group_subagents,
            privacy_mode: prefs.privacy_mode,
            pill_visible: prefs.pill_visible,
            tray_hover_mode: prefs.tray_hover_mode,
            tray_hover_shown: false,
            tray_hover_icon_rect: None,
            tray_hover_leave_since: None,
            tray_hover_suppress_until: None,
            tray_pin_retries: 6,
            fullscreen_hidden: false,
            notifications_enabled: prefs.notifications_enabled,
            hide_on_fullscreen: prefs.hide_on_fullscreen,
            fresh_only: prefs.fresh_only,
            date_scope: DateScope::from_u8(prefs.date_scope),
            pill_items: prefs.pill_items,
            monitor_index: prefs.monitor_index,
            last_total_tokens: initial_total,
            recent_activity_until: None,
            tray,
            taskbar_created_msg,
            fired_alerts: HashSet::new(),
            last_tray_tip: String::new(),
            self_test_mode: opts.self_test,
            dash_owner_hwnd: None,
            dash_hwnd: None,
            dash_view: None,
            pricing_edits: PricingEditControls::default(),
            dash_visible: false,
            dash_tracking: false,
            dash_monitor_index: prefs.monitor_index,
            dashboard_only: opts.dashboard_only,
        };

        APP.with(|cell| {
            *cell.borrow_mut() = Some(state);
        });

        if opts.dashboard_only {
            create_dashboard_window_local();
            if let Some(dh) = with_app(|app| app.dash_hwnd).flatten() {
                let _ = SetTimer(dh, TIMER_TELEMETRY, 1000, None);
                let _ = SetTimer(dh, TIMER_TOKENS, 3000, None);
            }
        } else {
            with_app(|app| {
                app.sync_tray_and_alerts();
                app.sync_tray_taskbar_pin();
            });
            if (!prefs.tray_hover_mode && prefs.pill_visible) || opts.self_test {
                with_app(|app| app.redraw_pill());
                let _ = ShowWindow(pill_hwnd, SW_SHOWNOACTIVATE);
                ensure_pill_topmost_if_needed(true);
            }
            trim_self_working_set();
            if opts.open_detail {
                open_or_focus_dashboard_safe();
            }
        }

        if opts.self_test {
            let cold_ms = with_app(|app| app.snapshot.scan_ms).unwrap_or(0.0);
            let idle_mb = read_self_private_mb();
            let cached_ms = with_app(|app| app.scanner.scan().scan_ms).unwrap_or(0.0);
            let t_open = Instant::now();
            create_dashboard_window_local();
            let open_ms = t_open.elapsed().as_secs_f64() * 1000.0;
            for (cost_m, priv_m) in [(false, false), (true, false), (false, true)] {
                for tab in [Tab::Overview, Tab::Daily, Tab::Weekly, Tab::Models, Tab::Sessions, Tab::Calls, Tab::System] {
                    with_app(|app| {
                        if let (Some(hwnd), Some(view)) = (app.dash_hwnd, &mut app.dash_view) {
                            view.cost_mode = cost_m;
                            view.privacy_mode = priv_m;
                            view.active_tab = tab;
                            let dpi = window_dpi_scale(hwnd) * 96.0;
                            let _ = view.render_to_hdc(
                                &app.factories,
                                &app.fonts,
                                hwnd,
                                None,
                                dpi,
                                &app.snapshot,
                                &app.telemetry,
                                app.human,
                                app.light_theme,
                            );
                            if tab == Tab::Overview {
                                view.overview_top = OverviewTopMode::WeeklyCompare;
                                view.breakdown_mode = BreakdownMode::Workspace;
                                let _ = view.render_to_hdc(
                                    &app.factories,
                                    &app.fonts,
                                    hwnd,
                                    None,
                                    dpi,
                                    &app.snapshot,
                                    &app.telemetry,
                                    app.human,
                                    app.light_theme,
                                );
                                view.overview_top = OverviewTopMode::QuotaPace;
                                view.breakdown_mode = BreakdownMode::Tools;
                                let _ = view.render_to_hdc(
                                    &app.factories,
                                    &app.fonts,
                                    hwnd,
                                    None,
                                    dpi,
                                    &app.snapshot,
                                    &app.telemetry,
                                    app.human,
                                    app.light_theme,
                                );
                                view.breakdown_mode = BreakdownMode::Source;
                                let _ = view.render_to_hdc(
                                    &app.factories,
                                    &app.fonts,
                                    hwnd,
                                    None,
                                    dpi,
                                    &app.snapshot,
                                    &app.telemetry,
                                    app.human,
                                    app.light_theme,
                                );
                                view.breakdown_mode = BreakdownMode::Model;
                            }
                            if tab == Tab::Daily {
                                view.daily_view = DailyViewMode::Heatmap;
                                let _ = view.render_to_hdc(
                                    &app.factories,
                                    &app.fonts,
                                    hwnd,
                                    None,
                                    dpi,
                                    &app.snapshot,
                                    &app.telemetry,
                                    app.human,
                                    app.light_theme,
                                );
                                view.daily_view = DailyViewMode::Bars;
                            }
                            if tab == Tab::Models {
                                view.model_view = ModelViewMode::Pricing;
                                let _ = view.render_to_hdc(
                                    &app.factories,
                                    &app.fonts,
                                    hwnd,
                                    None,
                                    dpi,
                                    &app.snapshot,
                                    &app.telemetry,
                                    app.human,
                                    app.light_theme,
                                );
                                view.model_view = ModelViewMode::Budget;
                                let _ = view.render_to_hdc(
                                    &app.factories,
                                    &app.fonts,
                                    hwnd,
                                    None,
                                    dpi,
                                    &app.snapshot,
                                    &app.telemetry,
                                    app.human,
                                    app.light_theme,
                                );
                                view.model_view = ModelViewMode::CacheCompare;
                            }
                            if tab == Tab::Weekly {
                                view.weekly_mode = WeeklyMode::Monthly;
                                let _ = view.render_to_hdc(
                                    &app.factories,
                                    &app.fonts,
                                    hwnd,
                                    None,
                                    dpi,
                                    &app.snapshot,
                                    &app.telemetry,
                                    app.human,
                                    app.light_theme,
                                );
                                view.weekly_mode = WeeklyMode::Weekly;
                            }
                            if tab == Tab::Sessions {
                                view.scope = DateScope::Custom;
                                view.mark_data_dirty();
                                let _ = view.render_to_hdc(
                                    &app.factories,
                                    &app.fonts,
                                    hwnd,
                                    None,
                                    dpi,
                                    &app.snapshot,
                                    &app.telemetry,
                                    app.human,
                                    app.light_theme,
                                );
                                view.scope = DateScope::All;
                                view.mark_data_dirty();
                            }
                        }
                    });
                }
            }
            let open_mb = read_self_private_mb();
            hide_dashboard_safe();
            let hide_priv_mb = read_self_private_mb();
            let hide_ws_mb = read_self_working_set_mb();
            let t_reopen = Instant::now();
            open_or_focus_dashboard_safe();
            let reopen_ms = t_reopen.elapsed().as_secs_f64() * 1000.0;
            destroy_dashboard_safe();
            let summary = with_app(|app| {
                format!(
                    "RUST_SELF_TEST_OK theme={} sessions={} total_tokens={} total_usd={:.2} cold_scan_ms={:.1} cached_ms={:.2} open1_ms={:.1} reopen_ms={:.1} pill_idle_mb={:.1} dash_open_mb={:.1} hide_priv_mb={:.1} hide_ws_mb={:.1}",
                    if app.light_theme { "paper" } else { "obsidian" },
                    app.snapshot.sessions.len(),
                    app.snapshot.agg.total.total_tokens(),
                    app.snapshot.agg.total.cost.total_usd(),
                    cold_ms,
                    cached_ms,
                    open_ms,
                    reopen_ms,
                    idle_mb,
                    open_mb,
                    hide_priv_mb,
                    hide_ws_mb,
                )
            })
            .unwrap_or_default();
            use std::io::Write;
            let _ = writeln!(std::io::stdout(), "{summary}");
            if !pill_hwnd.0.is_null() {
                let _ = DestroyWindow(pill_hwnd);
            }
            APP.with(|cell| *cell.borrow_mut() = None);
            CoUninitialize();
            return Ok(());
        }

        if !opts.dashboard_only {
            let _ = SetTimer(pill_hwnd, TIMER_TELEMETRY, 1000, None);
            let _ = SetTimer(pill_hwnd, TIMER_TOKENS, 3000, None);
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_KEYDOWN {
                let target = with_app(|app| app.pricing_edits.classify_hwnd(msg.hwnd)).flatten();
                if let Some(t) = target {
                    let vk = msg.wParam.0 as u16;
                    let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
                    let shift = (GetKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
                    if ctrl && vk == 0x41 {
                        let _ = SendMessageW(msg.hwnd, EM_SETSEL, WPARAM(0), LPARAM(-1));
                        continue;
                    }
                    match t {
                        EditTarget::Five(idx, mode) => {
                            if vk == VK_TAB.0 {
                                let count = if mode == 2 { 4 } else { 5 };
                                let next = if shift {
                                    (idx + count - 1) % count
                                } else {
                                    (idx + 1) % count
                                };
                                with_app(|app| app.pricing_edits.focus_field(next));
                                continue;
                            }
                            if vk == VK_RETURN.0 {
                                with_app(|app| {
                                    if mode == 2 {
                                        app.save_budget_from_editor();
                                    } else {
                                        app.save_pricing_from_editor();
                                    }
                                });
                                continue;
                            }
                            if vk == VK_ESCAPE.0 {
                                with_app(|app| {
                                    if let Some(dh) = app.dash_hwnd {
                                        let _ = SetFocus(dh);
                                    }
                                    if let Some(view) = &mut app.dash_view {
                                        view.pricing_focused_field = None;
                                    }
                                    app.invalidate_dashboard();
                                });
                                continue;
                            }
                        }
                        EditTarget::Search => {
                            if vk == VK_ESCAPE.0 {
                                with_app(|app| {
                                    app.pricing_edits.set_search_text("");
                                    if let Some(view) = &mut app.dash_view {
                                        view.search_query.clear();
                                        view.search_focused = false;
                                        view.scroll_rows = 0;
                                    }
                                    if let Some(dh) = app.dash_hwnd {
                                        let _ = SetFocus(dh);
                                    }
                                    app.invalidate_dashboard();
                                });
                                continue;
                            }
                            if vk == VK_RETURN.0 {
                                with_app(|app| {
                                    if let Some(dh) = app.dash_hwnd {
                                        let _ = SetFocus(dh);
                                    }
                                    if let Some(view) = &mut app.dash_view {
                                        view.search_focused = false;
                                    }
                                    app.invalidate_dashboard();
                                });
                                continue;
                            }
                        }
                        EditTarget::Date(idx) => {
                            if vk == VK_TAB.0 {
                                with_app(|app| app.pricing_edits.focus_date(1 - idx));
                                continue;
                            }
                            if vk == VK_RETURN.0 {
                                with_app(|app| app.apply_custom_dates_from_editor());
                                continue;
                            }
                            if vk == VK_ESCAPE.0 {
                                with_app(|app| {
                                    if let Some(dh) = app.dash_hwnd {
                                        let _ = SetFocus(dh);
                                    }
                                    if let Some(view) = &mut app.dash_view {
                                        view.custom_date_focused = None;
                                    }
                                    app.invalidate_dashboard();
                                });
                                continue;
                            }
                        }
                    }
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        APP.with(|cell| *cell.borrow_mut() = None);
        CoUninitialize();
    }

    Ok(())
}

fn main() {
    let opts = parse_args();
    let _ = run_app(opts);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn mk_snap(sessions: Vec<stats::SessionInfo>, data_dirs: Vec<(stats::SourceKind, PathBuf)>) -> ScanSnapshot {
        let agg = stats::aggregate_sessions(&sessions);
        ScanSnapshot {
            data_dir: PathBuf::from("."),
            data_dirs,
            sessions,
            agg,
            statusline: None,
            statusline_changed: false,
            budget: quota::BudgetConfig::default(),
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
        }
    }

    fn mk_sess(cid: &str, title: &str, source: stats::SourceKind, is_subagent: bool) -> stats::SessionInfo {
        let now = Some(Local::now());
        stats::SessionInfo {
            conversation_id: cid.to_string(),
            title: title.to_string(),
            workspace: String::new(),
            source,
            is_archived: false,
            is_subagent,
            parent_id: None,
            parent_title: String::new(),
            turns: Vec::new(),
            counts: stats::TokenCounts::default(),
            models: BTreeSet::new(),
            first_time: now,
            last_time: now,
            last_file_mtime_utc: None,
            is_running_hint: false,
        }
    }

    #[test]
    fn test_cli_discovery_filter_flags() {
        let def = parse_args_from(Vec::<String>::new(), false);
        assert_eq!(def.discovery_filter, stats::SourceDiscoveryFilter::All);

        let agy = parse_args_from(["--agy-only"], false);
        assert_eq!(agy.discovery_filter, stats::SourceDiscoveryFilter::AgyOnly);

        let csv = parse_args_from(["--agents", "claude,codex"], false);
        assert_eq!(
            csv.discovery_filter,
            stats::SourceDiscoveryFilter::Families(vec![
                stats::AgentFamily::ClaudeCode,
                stats::AgentFamily::Codex,
            ])
        );

        let eq_and_alias = parse_args_from(["--agent=gemini", "--agents", "agy"], false);
        assert_eq!(
            eq_and_alias.discovery_filter,
            stats::SourceDiscoveryFilter::Families(vec![
                stats::AgentFamily::GeminiCli,
                stats::AgentFamily::Antigravity,
            ])
        );

        // --agy-only takes precedence over --agents (parity with Python CLI)
        let agy_wins = parse_args_from(["--agents", "claude,codex", "--agy-only"], false);
        assert_eq!(agy_wins.discovery_filter, stats::SourceDiscoveryFilter::AgyOnly);

        // --data-dir stays backward compatible
        let dd = parse_args_from(["--data-dir", r"C:\Users\alice\.claude"], false);
        assert_eq!(dd.data_dirs.len(), 1);
        assert_eq!(dd.data_dirs[0].0, stats::SourceKind::ClaudeCode);
    }

    #[test]
    fn test_quick_session_menu_prefixes_single_vs_multi_agent() {
        let s_agy = mk_sess("11111111-1111-1111-1111-111111111111", "AGY task", stats::SourceKind::Cli, false);
        let s_app = mk_sess("22222222-2222-2222-2222-222222222222", "App task", stats::SourceKind::App, false);
        let snap_agy = mk_snap(vec![s_agy.clone(), s_app.clone()], Vec::new());
        let items_agy = build_quick_session_menu_items(&snap_agy);
        assert_eq!(items_agy.len(), 1);
        assert!(!items_agy[0].label.contains("[AGY]"));
        assert!(items_agy[0].label.starts_with("AGY task"));

        let s_claude = mk_sess("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", "Claude task", stats::SourceKind::ClaudeCode, false);
        let s_codex = mk_sess("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb", "Codex task", stats::SourceKind::Codex, false);
        let s_gemini = mk_sess("cccccccc-cccc-cccc-cccc-cccccccccccc", "Gemini task", stats::SourceKind::GeminiCli, false);
        let s_sub = mk_sess("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa__sub1", "Sub task", stats::SourceKind::ClaudeCode, true);
        let snap_multi = mk_snap(
            vec![s_agy, s_app, s_claude, s_codex, s_gemini, s_sub],
            Vec::new(),
        );
        let items_multi = build_quick_session_menu_items(&snap_multi);
        assert_eq!(items_multi.len(), 4);
        assert!(items_multi.iter().any(|i| i.label.starts_with("[AGY] AGY task")));
        assert!(items_multi.iter().any(|i| i.label.starts_with("[Claude] Claude task")));
        assert!(items_multi.iter().any(|i| i.label.starts_with("[Codex] Codex task")));
        assert!(items_multi.iter().any(|i| i.label.starts_with("[Gemini] Gemini task")));
    }
}
