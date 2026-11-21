#![cfg_attr(not(test), windows_subsystem = "windows")]

//! Ultra-lightweight, strictly read-only Antigravity CLI (`agy`) Token & System Monitor.
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
mod stats;
mod telemetry;

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, HDC, PAINTSTRUCT};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW, GetMessageW,
    LoadCursorW, LoadImageW, PostMessageW, PostQuitMessage, RegisterClassExW, SetCursor,
    SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow, TranslateMessage, CS_HREDRAW,
    CS_VREDRAW, HICON, HWND_NOTOPMOST, HWND_TOPMOST, IDC_ARROW, IDC_HAND, IMAGE_ICON,
    LR_DEFAULTCOLOR, MINMAXINFO, MSG, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_RESTORE, SW_SHOW,
    SW_SHOWNOACTIVATE, WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_GETMINMAXINFO,
    WM_KEYDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONUP, WM_SETCURSOR,
    WM_SIZE, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW, WS_POPUP,
};

const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_PRINTCLIENT: u32 = 0x0318;

use d2d::{theme_for, D2DFactories, Fonts};
use dashboard::{DashboardView, Tab};
use pill::{
    show_context_menu, PillRenderer, CMD_MONITOR_BASE, CMD_OPEN_DETAIL, CMD_QUIT, CMD_REDOCK,
    CMD_REFRESH, CMD_TOGGLE_HUMAN, CMD_TOGGLE_LIGHT, CMD_TOGGLE_MINI, CMD_TOGGLE_TOPMOST,
};
use stats::{IncrementalScanner, ScanSnapshot};
use telemetry::{
    apply_win11_frame, broadcast_prefs_sync, enable_dpi_awareness, get_work_area, load_user_prefs,
    read_self_private_mb, save_user_prefs, window_dpi_scale, SingleInstanceGuard, SystemTelemetry,
    UserPrefs, WM_APP_SYNC_PREFS,
};

const TIMER_TELEMETRY: usize = 1;
const TIMER_TOKENS: usize = 2;

struct AppState {
    factories: D2DFactories,
    fonts: Fonts,
    scanner: IncrementalScanner,
    snapshot: ScanSnapshot,
    telemetry: SystemTelemetry,

    pill_hwnd: HWND,
    pill_renderer: PillRenderer,
    pill_hovered: bool,
    pill_tracking: bool,
    mini: bool,
    light_theme: bool,
    topmost: bool,
    human: bool,
    fresh_only: bool,
    monitor_index: usize,
    last_total_tokens: u64,
    recent_activity_until: Option<Instant>,

    dash_hwnd: Option<HWND>,
    dash_view: Option<DashboardView>,
    dashboard_only: bool,
    data_dir: PathBuf,
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

fn toggle_dashboard_safe() {
    // If dashboard is already open (detached or local), clicking the pill again closes it cleanly!
    unsafe {
        if let Ok(existing) = FindWindowW(w!("AgyTokenDashboardClass"), None) {
            if !existing.0.is_null() {
                let _ = PostMessageW(existing, WM_CLOSE, WPARAM(0), LPARAM(0));
                return;
            }
        }
    }
    open_or_focus_dashboard_safe();
}

fn open_or_focus_dashboard_safe() {
    unsafe {
        if let Ok(existing) = FindWindowW(w!("AgyTokenDashboardClass"), None) {
            if !existing.0.is_null() {
                let _ = ShowWindow(existing, SW_RESTORE);
                let _ = SetForegroundWindow(existing);
                let _ = InvalidateRect(existing, None, false);
                return;
            }
        }
    }

    let spawn_info = with_app(|app| {
        if app.dashboard_only {
            None
        } else {
            Some((app.data_dir.clone(), app.monitor_index, app.human, app.light_theme))
        }
    })
    .flatten();

    if let Some((data_dir, mon, human, light)) = spawn_info {
        if let Ok(exe) = std::env::current_exe() {
            let mut cmd = std::process::Command::new(exe);
            cmd.arg("--dashboard-only")
                .arg("--monitor")
                .arg(mon.to_string())
                .arg("--data-dir")
                .arg(data_dir);
            if !human {
                cmd.arg("--exact");
            }
            if light {
                cmd.arg("--light");
            }
            if cmd.spawn().is_ok() {
                return;
            }
        }
    }

    create_dashboard_window_local();
}

fn create_dashboard_window_local() {
    let existing = with_app(|app| app.dash_hwnd).flatten();
    if let Some(hwnd) = existing {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
            let _ = InvalidateRect(hwnd, None, false);
        }
        return;
    }

    let Some((wx, wy, ww, wh, light)) = with_app(|app| {
        let work = get_work_area(app.monitor_index);
        let scale = if !app.pill_hwnd.0.is_null() {
            window_dpi_scale(app.pill_hwnd)
        } else {
            1.25
        };
        let avail_w = work.right - work.left;
        let avail_h = work.bottom - work.top;
        let ww = ((1060.0 * scale).round() as i32).min((avail_w as f32 * 0.90) as i32);
        let wh = ((710.0 * scale).round() as i32).min((avail_h as f32 * 0.88) as i32);
        let wx = work.left + (avail_w - ww) / 2;
        let wy = work.top + (avail_h - wh) / 2;
        (wx, wy, ww, wh, app.light_theme)
    }) else {
        return;
    };

    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        if let Ok(hwnd) = CreateWindowExW(
            Default::default(),
            w!("AgyTokenDashboardClass"),
            w!("AGY 用量统计"),
            WS_OVERLAPPEDWINDOW,
            wx,
            wy,
            ww,
            wh,
            None,
            None,
            hinst,
            None,
        ) {
            let th = theme_for(light);
            apply_win11_frame(hwnd, th.is_dark, th.caption_colorref);
            with_app(|app| {
                app.dash_hwnd = Some(hwnd);
                app.dash_view = Some(DashboardView::new(app.fresh_only));
            });
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

fn close_dashboard_safe() {
    let (hwnd, dash_only) = with_app(|app| {
        app.dash_view = None;
        (app.dash_hwnd.take(), app.dashboard_only)
    })
    .unwrap_or((None, false));
    if let Some(h) = hwnd {
        unsafe {
            let _ = DestroyWindow(h);
        }
    } else if !dash_only {
        unsafe {
            if let Ok(ext) = FindWindowW(w!("AgyTokenDashboardClass"), None) {
                if !ext.0.is_null() {
                    let _ = PostMessageW(ext, WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
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
        if persist {
            app.persist_and_sync(app.pill_hwnd);
        }
        app.pill_hwnd
    });
    if let Some(h) = hwnd {
        unsafe {
            let _ = SetWindowPos(
                h,
                if top { HWND_TOPMOST } else { HWND_NOTOPMOST },
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

impl AppState {
    fn to_prefs(&self) -> UserPrefs {
        UserPrefs {
            light_theme: self.light_theme,
            mini: self.mini,
            topmost: self.topmost,
            human: self.human,
            monitor_index: self.monitor_index,
            fresh_only: self.dash_view.as_ref().map(|v| v.fresh_only).unwrap_or(self.fresh_only),
        }
    }

    fn persist_and_sync(&mut self, sender: HWND) {
        let p = self.to_prefs();
        self.fresh_only = p.fresh_only;
        save_user_prefs(&p);
        broadcast_prefs_sync(sender);
    }

    fn apply_synced_prefs(&mut self, p: UserPrefs) {
        self.light_theme = p.light_theme;
        self.mini = p.mini;
        self.topmost = p.topmost;
        self.human = p.human;
        self.monitor_index = p.monitor_index;
        self.fresh_only = p.fresh_only;
        if let Some(view) = &mut self.dash_view {
            view.fresh_only = p.fresh_only;
        }
    }
    fn redraw_pill(&mut self) {
        if self.dashboard_only || self.pill_hwnd.0.is_null() {
            return;
        }
        let dpi = window_dpi_scale(self.pill_hwnd) * 96.0;
        let recent = self
            .recent_activity_until
            .map(|t| Instant::now() < t)
            .unwrap_or(false);
        let _ = self.pill_renderer.render(
            &self.factories,
            &self.fonts,
            self.pill_hwnd,
            dpi,
            self.monitor_index,
            self.mini,
            self.light_theme,
            self.pill_hovered,
            recent,
            &self.snapshot,
            &self.telemetry,
        );
    }

    fn invalidate_dashboard(&self) {
        if let Some(hwnd) = self.dash_hwnd {
            unsafe {
                let _ = InvalidateRect(hwnd, None, false);
            }
        }
    }

    fn refresh_tokens(&mut self, force: bool) {
        let fresh = self.scanner.scan();
        let changed = force
            || fresh.files_reparsed > 0
            || fresh.sessions.len() != self.snapshot.sessions.len();
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
            self.redraw_pill();
            self.invalidate_dashboard();
        }
    }
}

unsafe extern "system" fn pill_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_SETCURSOR => {
            if let Ok(hand) = LoadCursorW(None, IDC_HAND) {
                SetCursor(hand);
            }
            LRESULT(1)
        }
        WM_MOUSEMOVE => {
            with_app(|app| {
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
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            with_app(|app| {
                app.pill_tracking = false;
                if app.pill_hovered {
                    app.pill_hovered = false;
                    app.redraw_pill();
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            toggle_dashboard_safe();
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let state = with_app(|app| (app.mini, app.light_theme, app.topmost, app.human, app.monitor_index));
            let dash_open = FindWindowW(w!("AgyTokenDashboardClass"), None)
                .map(|h| !h.0.is_null())
                .unwrap_or(false);
            if let Some((mini, light, topmost, human, mon)) = state {
                if let Some(cmd) = show_context_menu(hwnd, dash_open, mini, light, topmost, human, mon) {
                    match cmd {
                        CMD_OPEN_DETAIL => toggle_dashboard_safe(),
                        CMD_REFRESH => {
                            with_app(|app| app.refresh_tokens(true));
                        }
                        CMD_TOGGLE_LIGHT => {
                            with_app(|app| {
                                app.light_theme = !app.light_theme;
                                app.redraw_pill();
                                app.persist_and_sync(hwnd);
                            });
                        }
                        CMD_TOGGLE_MINI => {
                            with_app(|app| {
                                app.mini = !app.mini;
                                app.redraw_pill();
                                app.persist_and_sync(hwnd);
                            });
                        }
                        CMD_TOGGLE_TOPMOST => set_pill_topmost_safe(!topmost, true),
                        CMD_TOGGLE_HUMAN => {
                            with_app(|app| {
                                app.human = !app.human;
                                app.redraw_pill();
                                app.persist_and_sync(hwnd);
                            });
                        }
                        CMD_REDOCK => {
                            with_app(|app| app.redraw_pill());
                        }
                        CMD_QUIT => {
                            close_dashboard_safe();
                            PostQuitMessage(0);
                        }
                        c if c >= CMD_MONITOR_BASE => {
                            with_app(|app| {
                                app.monitor_index = (c - CMD_MONITOR_BASE) as usize;
                                app.redraw_pill();
                                app.persist_and_sync(hwnd);
                            });
                        }
                        _ => {}
                    }
                }
            }
            LRESULT(0)
        }
        WM_APP_SYNC_PREFS => {
            let p = load_user_prefs();
            let top = p.topmost;
            with_app(|app| {
                app.apply_synced_prefs(p);
                app.redraw_pill();
            });
            set_pill_topmost_safe(top, false);
            LRESULT(0)
        }
        WM_TIMER => {
            match wparam.0 {
                TIMER_TELEMETRY => {
                    with_app(|app| {
                        app.telemetry.sample();
                        app.redraw_pill();
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
        WM_DPICHANGED => {
            with_app(|app| app.redraw_pill());
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
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
        WM_SIZE | WM_DPICHANGED => {
            with_app(|app| {
                if let Some(view) = &mut app.dash_view {
                    view.invalidate_surface();
                }
            });
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x_px = (lparam.0 & 0xFFFF) as i16 as f32;
            let y_px = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
            let scale = window_dpi_scale(hwnd);
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            with_app(|app| {
                if let Some(view) = &mut app.dash_view {
                    view.mouse_dip = Some((x_px / scale, y_px / scale));
                }
            });
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            with_app(|app| {
                if let Some(view) = &mut app.dash_view {
                    view.mouse_dip = None;
                }
            });
            let _ = InvalidateRect(hwnd, None, false);
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
            with_app(|app| {
                let action = app
                    .dash_view
                    .as_ref()
                    .and_then(|v| v.hit_test_click(x_px / scale, y_px / scale));
                if let Some(act) = action {
                    let mut need_refresh = false;
                    let mut prefs_changed = false;
                    if let Some(view) = &mut app.dash_view {
                        let res = view.apply_action(hwnd, act, &mut app.human, &mut app.light_theme);
                        need_refresh = res.0;
                        prefs_changed = res.1;
                        app.fresh_only = view.fresh_only;
                    }
                    if prefs_changed {
                        app.persist_and_sync(hwnd);
                    }
                    if need_refresh {
                        app.refresh_tokens(true);
                    }
                    app.redraw_pill();
                    app.invalidate_dashboard();
                }
            });
            LRESULT(0)
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
            if wparam.0 as u16 == VK_ESCAPE.0 {
                close_dashboard_safe();
                return LRESULT(0);
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
        WM_CLOSE => {
            close_dashboard_safe();
            LRESULT(0)
        }
        WM_DESTROY => {
            let dash_only = with_app(|app| {
                app.dash_hwnd = None;
                app.dash_view = None;
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
    data_dir: PathBuf,
    mini: Option<bool>,
    light: Option<bool>,
    open_detail: bool,
    dashboard_only: bool,
    human: Option<bool>,
    monitor: Option<usize>,
    allow_multiple: bool,
    self_test: bool,
}

fn parse_args() -> CliOpts {
    let mut opts = CliOpts {
        data_dir: IncrementalScanner::default_data_dir(),
        mini: None,
        light: None,
        open_detail: false,
        dashboard_only: false,
        human: None,
        monitor: None,
        allow_multiple: false,
        self_test: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mini" => opts.mini = Some(true),
            "--light" => opts.light = Some(true),
            "--dark" => opts.light = Some(false),
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
                    opts.data_dir = PathBuf::from(v);
                }
            }
            _ => {}
        }
    }
    opts
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
        prefs.mini = v;
    }
    if let Some(v) = opts.light {
        prefs.light_theme = v;
    }
    if let Some(v) = opts.human {
        prefs.human = v;
    }
    if let Some(v) = opts.monitor {
        prefs.monitor_index = v;
    }

    let factories = D2DFactories::new()?;
    let fonts = Fonts::new(&factories)?;
    let mut scanner = IncrementalScanner::new(opts.data_dir.clone());
    let snapshot = scanner.scan();
    let telemetry = SystemTelemetry::new(90);

    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let arrow = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
        let icon_res = PCWSTR(1 as *const u16);
        let icon_big = LoadImageW(hinst, icon_res, IMAGE_ICON, 32, 32, LR_DEFAULTCOLOR)
            .map(|h| HICON(h.0))
            .unwrap_or_default();
        let icon_sm = LoadImageW(hinst, icon_res, IMAGE_ICON, 16, 16, LR_DEFAULTCOLOR)
            .map(|h| HICON(h.0))
            .unwrap_or(icon_big);

        let pill_cls = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(pill_wndproc),
            hInstance: hinst.into(),
            hIcon: icon_big,
            hIconSm: icon_sm,
            hCursor: arrow,
            lpszClassName: w!("AgyTokenPillClass"),
            ..Default::default()
        };
        RegisterClassExW(&pill_cls);

        let dash_cls = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(dash_wndproc),
            hInstance: hinst.into(),
            hIcon: icon_big,
            hIconSm: icon_sm,
            hCursor: arrow,
            lpszClassName: w!("AgyTokenDashboardClass"),
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
                w!("AgyTokenPillClass"),
                w!("AGY Token Pill"),
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

        let initial_total = snapshot.agg.total.total_tokens();
        let state = AppState {
            factories,
            fonts,
            scanner,
            snapshot,
            telemetry,
            pill_hwnd,
            pill_renderer: PillRenderer::new(),
            pill_hovered: false,
            pill_tracking: false,
            mini: prefs.mini,
            light_theme: prefs.light_theme,
            topmost: prefs.topmost,
            human: prefs.human,
            fresh_only: prefs.fresh_only,
            monitor_index: prefs.monitor_index,
            last_total_tokens: initial_total,
            recent_activity_until: None,
            dash_hwnd: None,
            dash_view: None,
            dashboard_only: opts.dashboard_only,
            data_dir: opts.data_dir,
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
            let _ = ShowWindow(pill_hwnd, SW_SHOWNOACTIVATE);
            with_app(|app| app.redraw_pill());
            if opts.open_detail {
                open_or_focus_dashboard_safe();
            }
        }

        if opts.self_test {
            let idle_mb = read_self_private_mb();
            let cached_ms = with_app(|app| app.scanner.scan().scan_ms).unwrap_or(0.0);
            create_dashboard_window_local();
            for tab in [Tab::Overview, Tab::Trends, Tab::Sessions, Tab::System] {
                with_app(|app| {
                    if let (Some(hwnd), Some(view)) = (app.dash_hwnd, &mut app.dash_view) {
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
                    }
                });
            }
            let open_mb = read_self_private_mb();
            close_dashboard_safe();
            let summary = with_app(|app| {
                format!(
                    "RUST_SELF_TEST_OK theme={} sessions={} total_tokens={} pill_idle_mb={:.1} dash_open_mb={:.1} cached_ms={:.2}",
                    if app.light_theme { "paper" } else { "obsidian" },
                    app.snapshot.sessions.len(),
                    app.snapshot.agg.total.total_tokens(),
                    idle_mb,
                    open_mb,
                    cached_ms,
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
