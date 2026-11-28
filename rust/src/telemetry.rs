//! Zero-subprocess Win32 system telemetry, multi-monitor geometry, DWM & clipboard helpers.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::{s, w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_IO_PENDING, ERROR_NOTIFY_ENUM_DIR,
    ERROR_OPERATION_ABORTED, FILETIME, GetLastError, GlobalFree, HANDLE, HWND, LPARAM, POINT,
    RECT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_CLOAKED,
    DWMWA_TRANSITIONS_FORCEDISABLED, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, HDC, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    GetProcessHeap, GlobalAlloc, GlobalLock, GlobalUnlock, HeapCompact, GMEM_MOVEABLE, HEAP_FLAGS,
};
use windows::Win32::System::ProcessStatus::{
    K32EmptyWorkingSet, K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_ENUMERATE_SUB_KEYS, KEY_READ, KEY_SET_VALUE,
    KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::System::SystemInformation::{GetSystemInfo, GlobalMemoryStatusEx, MEMORYSTATUSEX, SYSTEM_INFO};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentProcess, GetSystemTimes, ResetEvent, SetEvent,
    WaitForMultipleObjects, INFINITE,
};
use windows::Win32::UI::Controls::Dialogs::{
    GetSaveFileNameW, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, GetDpiForWindow, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_ICON, NIF_INFO,
    NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIIF_NOSOUND, NIIF_WARNING, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER, QUNS_BUSY, QUNS_PRESENTATION_MODE,
    QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetClassNameW, GetForegroundWindow, GetWindow, GetWindowLongPtrW, GetWindowRect,
    IsIconic, IsWindow, IsWindowVisible, PostMessageW, RegisterWindowMessageW,
    SetCoalescableTimer, SetWindowPos, SystemParametersInfoW, GWL_EXSTYLE, GW_HWNDPREV, HICON,
    HWND_BOTTOM, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, SPI_GETWORKAREA, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSENDCHANGING, SWP_NOSIZE, SWP_NOZORDER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    TIMERV_DEFAULT_COALESCING, WINDOWPOS, WM_APP, WS_EX_TOPMOST,
};

pub const WM_APP_SYNC_PREFS: u32 = WM_APP + 1;
pub const WM_APP_TRAY: u32 = WM_APP + 2;

pub const PILL_ITEM_TOTAL: u32 = 1 << 0;
pub const PILL_ITEM_NON_CACHED: u32 = 1 << 1;
pub const PILL_ITEM_CACHE_HIT: u32 = 1 << 2;
pub const PILL_ITEM_ACTIVE_CTX: u32 = 1 << 3;
pub const PILL_ITEM_CPU: u32 = 1 << 4;
pub const PILL_ITEM_RAM: u32 = 1 << 5;
pub const PILL_ITEM_COST: u32 = 1 << 6;
// Bit 7 retired (former local 5h billing block); keep bit 8 stable for existing Registry prefs.
pub const PILL_ITEM_QUOTA: u32 = 1 << 8;
pub const PILL_ITEMS_ALL_MASK: u32 = PILL_ITEM_TOTAL
    | PILL_ITEM_NON_CACHED
    | PILL_ITEM_CACHE_HIT
    | PILL_ITEM_ACTIVE_CTX
    | PILL_ITEM_CPU
    | PILL_ITEM_RAM
    | PILL_ITEM_COST
    | PILL_ITEM_QUOTA;
pub const PILL_ITEMS_DEFAULT: u32 = PILL_ITEM_TOTAL | PILL_ITEM_CPU | PILL_ITEM_RAM;

pub fn migrate_legacy_pill_items(mini: bool, pill_metric: u8) -> u32 {
    let tok_bit = match pill_metric {
        1 => PILL_ITEM_NON_CACHED,
        2 => PILL_ITEM_ACTIVE_CTX,
        _ => PILL_ITEM_TOTAL,
    };
    if mini {
        tok_bit
    } else {
        tok_bit | PILL_ITEM_CPU | PILL_ITEM_RAM
    }
}

#[derive(Debug, Clone, Copy)]
pub struct UserPrefs {
    pub light_theme: bool,
    pub mini: bool,
    pub topmost: bool,
    pub human: bool,
    pub cost_mode: bool,
    pub group_subagents: bool,
    pub privacy_mode: bool,
    pub pill_visible: bool,
    pub tray_hover_mode: bool,
    pub notifications_enabled: bool,
    pub hide_on_fullscreen: bool,
    pub monitor_index: usize,
    pub fresh_only: bool,
    pub date_scope: u8,
    pub pill_metric: u8,
    pub pill_items: u32,
    pub custom_pos: Option<(i32, i32)>,
    pub terminal_preset: u8,
    pub terminal_new_window: bool,
    pub terminal_keep_open: bool,
}

impl Default for UserPrefs {
    fn default() -> Self {
        Self {
            light_theme: false,
            mini: false,
            topmost: true,
            human: true,
            cost_mode: false,
            group_subagents: true,
            privacy_mode: false,
            pill_visible: true,
            tray_hover_mode: false,
            notifications_enabled: true,
            hide_on_fullscreen: true,
            monitor_index: 0,
            fresh_only: false,
            date_scope: 0, // Default to Today (matches corner pill)
            pill_metric: 0,
            pill_items: PILL_ITEMS_DEFAULT,
            custom_pos: None,
            terminal_preset: 0, // Default to TerminalPreset::WtPwsh
            terminal_new_window: false,
            terminal_keep_open: true,
        }
    }
}

const REG_SUBKEY: PCWSTR = w!("Software\\Gauge");
const LEGACY_REG_SUBKEY: PCWSTR = w!("Software\\AgyTokenMonitor");

fn reg_subkey_exists(subkey: PCWSTR) -> bool {
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey, 0, KEY_READ, &mut hkey).is_ok() {
            let _ = RegCloseKey(hkey);
            true
        } else {
            false
        }
    }
}

fn reg_read_u32_from(subkey: PCWSTR, name: PCWSTR) -> Option<u32> {
    unsafe {
        let mut val: u32 = 0;
        let mut sz: u32 = size_of::<u32>() as u32;
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            subkey,
            name,
            RRF_RT_REG_DWORD,
            None,
            Some((&mut val as *mut u32).cast()),
            Some(&mut sz),
        );
        if status.is_ok() {
            Some(val)
        } else {
            None
        }
    }
}

fn load_user_prefs_from(subkey: PCWSTR) -> UserPrefs {
    let mut p = UserPrefs::default();
    if let Some(v) = reg_read_u32_from(subkey, w!("light_theme")) {
        p.light_theme = v != 0;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("mini")) {
        p.mini = v != 0;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("topmost")) {
        p.topmost = v != 0;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("human")) {
        p.human = v != 0;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("cost_mode")) {
        p.cost_mode = v != 0;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("group_subagents")) {
        p.group_subagents = v != 0;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("privacy_mode")) {
        p.privacy_mode = v != 0;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("pill_visible")) {
        p.pill_visible = v != 0;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("tray_hover_mode")) {
        p.tray_hover_mode = v != 0;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("notifications_enabled")) {
        p.notifications_enabled = v != 0;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("hide_on_fullscreen")) {
        p.hide_on_fullscreen = v != 0;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("monitor_index")) {
        p.monitor_index = v as usize;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("fresh_only")) {
        p.fresh_only = v != 0;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("date_scope")) {
        p.date_scope = v.min(4) as u8;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("pill_metric")) {
        p.pill_metric = v.min(2) as u8;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("pill_items")) {
        let masked = v & PILL_ITEMS_ALL_MASK;
        p.pill_items = if masked != 0 { masked } else { PILL_ITEMS_DEFAULT };
    } else {
        p.pill_items = migrate_legacy_pill_items(p.mini, p.pill_metric);
    }
    p.mini = (p.pill_items & (PILL_ITEM_CPU | PILL_ITEM_RAM)) == 0;
    if reg_read_u32_from(subkey, w!("has_custom_pos")).unwrap_or(0) != 0 {
        if let (Some(x), Some(y)) = (
            reg_read_u32_from(subkey, w!("pos_x")),
            reg_read_u32_from(subkey, w!("pos_y")),
        ) {
            p.custom_pos = Some((x as i32, y as i32));
        }
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("terminal_preset")) {
        p.terminal_preset = v.min(7) as u8;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("terminal_new_window")) {
        p.terminal_new_window = v != 0;
    }
    if let Some(v) = reg_read_u32_from(subkey, w!("terminal_keep_open")) {
        p.terminal_keep_open = v != 0;
    }
    p
}

pub fn load_user_prefs() -> UserPrefs {
    if reg_subkey_exists(REG_SUBKEY) {
        return load_user_prefs_from(REG_SUBKEY);
    }
    if reg_subkey_exists(LEGACY_REG_SUBKEY) {
        let p = load_user_prefs_from(LEGACY_REG_SUBKEY);
        save_user_prefs(&p);
        return p;
    }
    UserPrefs::default()
}

pub fn save_user_prefs(p: &UserPrefs) {
    unsafe {
        let mut hkey = HKEY::default();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            REG_SUBKEY,
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
        .is_err()
        {
            return;
        }
        let (has_pos, px, py) = match p.custom_pos {
            Some((x, y)) => (1u32, x as u32, y as u32),
            None => (0u32, 0u32, 0u32),
        };
        let pill_items = {
            let m = p.pill_items & PILL_ITEMS_ALL_MASK;
            if m != 0 { m } else { PILL_ITEMS_DEFAULT }
        };
        let derived_mini = (pill_items & (PILL_ITEM_CPU | PILL_ITEM_RAM)) == 0;
        let entries: [(PCWSTR, u32); 22] = [
            (w!("light_theme"), u32::from(p.light_theme)),
            (w!("mini"), u32::from(derived_mini)),
            (w!("topmost"), u32::from(p.topmost)),
            (w!("human"), u32::from(p.human)),
            (w!("cost_mode"), u32::from(p.cost_mode)),
            (w!("group_subagents"), u32::from(p.group_subagents)),
            (w!("privacy_mode"), u32::from(p.privacy_mode)),
            (w!("pill_visible"), u32::from(p.pill_visible)),
            (w!("tray_hover_mode"), u32::from(p.tray_hover_mode)),
            (w!("notifications_enabled"), u32::from(p.notifications_enabled)),
            (w!("hide_on_fullscreen"), u32::from(p.hide_on_fullscreen)),
            (w!("monitor_index"), p.monitor_index as u32),
            (w!("fresh_only"), u32::from(p.fresh_only)),
            (w!("date_scope"), p.date_scope as u32),
            (w!("pill_metric"), p.pill_metric as u32),
            (w!("pill_items"), pill_items),
            (w!("has_custom_pos"), has_pos),
            (w!("pos_x"), px),
            (w!("pos_y"), py),
            (w!("terminal_preset"), p.terminal_preset.min(7) as u32),
            (w!("terminal_new_window"), u32::from(p.terminal_new_window)),
            (w!("terminal_keep_open"), u32::from(p.terminal_keep_open)),
        ];
        for (name, val) in entries {
            let bytes = val.to_le_bytes();
            let _ = RegSetValueExW(hkey, name, 0, REG_DWORD, Some(&bytes));
        }
        let _ = RegCloseKey(hkey);
    }
}

/// Find which monitor contains point (cx, cy) (checking full monitor bounds, fallback nearest).
pub fn monitor_index_for_point(cx: i32, cy: i32) -> Option<usize> {
    with_cached_monitors(|mons| {
        for (idx, m) in mons.iter().enumerate() {
            if cx >= m.monitor.left && cx <= m.monitor.right && cy >= m.monitor.top && cy <= m.monitor.bottom {
                return Some(idx);
            }
        }
        None
    })
}

fn nearest_monitor_idx(mons: &[MonitorDesc], cx: i32, cy: i32) -> usize {
    for (idx, m) in mons.iter().enumerate() {
        if cx >= m.monitor.left && cx <= m.monitor.right && cy >= m.monitor.top && cy <= m.monitor.bottom {
            return idx;
        }
    }
    let mut best_idx = 0;
    let mut best_dist = i64::MAX;
    for (idx, m) in mons.iter().enumerate() {
        let dx = if cx < m.monitor.left {
            (m.monitor.left - cx) as i64
        } else if cx > m.monitor.right {
            (cx - m.monitor.right) as i64
        } else {
            0
        };
        let dy = if cy < m.monitor.top {
            (m.monitor.top - cy) as i64
        } else if cy > m.monitor.bottom {
            (cy - m.monitor.bottom) as i64
        } else {
            0
        };
        let d2 = dx * dx + dy * dy;
        if d2 < best_dist {
            best_dist = d2;
            best_idx = idx;
        }
    }
    best_idx
}

pub fn clamp_pos_in_list(
    mons: &[MonitorDesc],
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    shadow_px: i32,
    clamp_x_to_target: bool,
) -> (i32, i32) {
    if mons.is_empty() {
        return (x, y);
    }
    let center_x = x + w / 2;
    let center_y = y + h / 2;
    let idx = nearest_monitor_idx(mons, center_x, center_y);
    let cur = &mons[idx];
    let work = cur.work;
    let margin = 2;

    let (min_y, max_y) = if clamp_x_to_target {
        let lo = work.top - shadow_px + margin;
        let hi = (work.bottom - h + shadow_px - margin).max(lo);
        (lo, hi)
    } else {
        let above_top = mons
            .iter()
            .filter(|m| {
                center_x >= m.monitor.left
                    && center_x <= m.monitor.right
                    && m.monitor.bottom <= cur.monitor.top + 10
            })
            .map(|m| m.work.top)
            .min();
        let below_bot = mons
            .iter()
            .filter(|m| {
                center_x >= m.monitor.left
                    && center_x <= m.monitor.right
                    && m.monitor.top >= cur.monitor.bottom - 10
            })
            .map(|m| m.work.bottom)
            .max();
        let lo = above_top.unwrap_or(work.top) - shadow_px + margin;
        let hi = (below_bot.unwrap_or(work.bottom) - h + shadow_px - margin).max(lo);
        (lo, hi)
    };
    let cy = y.clamp(min_y, max_y);

    let (min_x, max_x) = if clamp_x_to_target {
        let lo = work.left - shadow_px + margin;
        let hi = (work.right - w + shadow_px - margin).max(lo);
        (lo, hi)
    } else {
        let lo = mons.iter().map(|m| m.work.left).min().unwrap_or(work.left) - shadow_px + margin;
        let hi = (mons.iter().map(|m| m.work.right).max().unwrap_or(work.right) - w + shadow_px - margin).max(lo);
        (lo, hi)
    };
    let cx = x.clamp(min_x, max_x);
    (cx, cy)
}

/// Clamp a pill window during live drag: keeps the visible capsule strictly inside the
/// target monitor's work area (never sliding behind the taskbar!), while allowing horizontal
/// drag smoothly across connected monitors.
pub fn clamp_pos_to_monitors(x: i32, y: i32, w: i32, h: i32, shadow_px: i32) -> (i32, i32) {
    with_cached_monitors(|mons| clamp_pos_in_list(mons, x, y, w, h, shadow_px, false))
}

/// Clamp a settled/saved pill position strictly inside its target monitor's work area.
pub fn clamp_pos_to_work_area(x: i32, y: i32, w: i32, h: i32, shadow_px: i32) -> (i32, i32) {
    with_cached_monitors(|mons| clamp_pos_in_list(mons, x, y, w, h, shadow_px, true))
}

/// Computes the layered window `(win_x, win_y)` for the tray-hover capsule flyout so that the
/// visible pill aligns cleanly from the tray icon rightward toward the bottom-right corner
/// (keeping a comfortable breathing margin from the screen edge and taskbar) without clipping.
pub fn compute_tray_flyout_pos_in_list(
    mons: &[MonitorDesc],
    icon: RECT,
    px_w: i32,
    px_h: i32,
    shadow_px: i32,
    scale: f32,
) -> (i32, i32) {
    let vis_w = (px_w - shadow_px * 2).max(1);
    let vis_h = (px_h - shadow_px * 2).max(1);
    let gap = (6.0 * scale).round() as i32;
    let margin = (6.0 * scale).round() as i32;
    let right_margin = (18.0 * scale).round() as i32;
    let icx = (icon.left + icon.right) / 2;
    let icy = (icon.top + icon.bottom) / 2;

    if mons.is_empty() {
        return (icon.left - shadow_px, icon.top - gap - vis_h - shadow_px);
    }

    let cur = &mons[nearest_monitor_idx(mons, icx, icy)];
    let mon = cur.monitor;
    let work = cur.work;

    let clamp_x = |lo: i32, hi_edge: i32| -> i32 {
        let min_x = lo + margin;
        let max_x = (hi_edge - right_margin - vis_w).max(min_x);
        icon.left.clamp(min_x, max_x)
    };
    let clamp_y = |lo: i32, hi_edge: i32| -> i32 {
        let min_y = lo + margin;
        let max_y = (hi_edge - margin - vis_h).max(min_y);
        (icy - vis_h / 2).clamp(min_y, max_y)
    };

    let (vis_x, vis_y) = if mon.bottom - work.bottom >= 16 {
        // Bottom taskbar (most common)
        let vx = clamp_x(work.left, work.right);
        let vy = if icon.top >= work.bottom - 8 {
            // Icon sits directly on the bottom taskbar -> dock snugly above taskbar
            (work.bottom - gap - vis_h).max(work.top + margin)
        } else {
            // Icon sits inside the Windows 11 overflow flyout -> dock right above the icon
            let min_y = work.top + margin;
            let max_y = (work.bottom - gap - vis_h).max(min_y);
            (icon.top - gap - vis_h).clamp(min_y, max_y)
        };
        (vx, vy)
    } else if work.top - mon.top >= 16 {
        // Top taskbar
        let vx = clamp_x(work.left, work.right);
        let vy = if icon.bottom <= work.top + 8 {
            work.top + gap
        } else {
            let min_y = work.top + gap;
            let max_y = (work.bottom - margin - vis_h).max(min_y);
            (icon.bottom + gap).clamp(min_y, max_y)
        };
        (vx, vy)
    } else if mon.right - work.right >= 16 {
        // Right-edge vertical taskbar
        let vx = if icon.left >= work.right - 8 {
            (work.right - gap - vis_w).max(work.left + margin)
        } else {
            (icon.left - gap - vis_w).max(work.left + margin)
        };
        (vx, clamp_y(work.top, work.bottom))
    } else if work.left - mon.left >= 16 {
        // Left-edge vertical taskbar
        let vx = if icon.right <= work.left + 8 {
            work.left + gap
        } else {
            (icon.right + gap).min((work.right - margin - vis_w).max(work.left + gap))
        };
        (vx, clamp_y(work.top, work.bottom))
    } else {
        // Auto-hide taskbar (work == monitor): anchor right above the icon
        let vx = clamp_x(mon.left, mon.right);
        let vy = if icon.top - gap - vis_h >= mon.top + margin {
            icon.top - gap - vis_h
        } else {
            (icon.bottom + gap).min((mon.bottom - margin - vis_h).max(mon.top + margin))
        };
        (vx, vy)
    };

    (vis_x - shadow_px, vis_y - shadow_px)
}

pub fn compute_tray_flyout_pos(
    icon: RECT,
    px_w: i32,
    px_h: i32,
    shadow_px: i32,
    scale: f32,
) -> (i32, i32) {
    with_cached_monitors(|mons| compute_tray_flyout_pos_in_list(mons, icon, px_w, px_h, shadow_px, scale))
}

/// Returns `true` while the cursor stays on the tray icon, on the popped-up capsule, or inside
/// the narrow corridor bridging the tray icon to the capsule so moving up to click never flickers.
pub fn is_cursor_in_tray_hover_zone(
    cursor: (i32, i32),
    icon: RECT,
    pill_pos: (i32, i32),
    px_w: i32,
    px_h: i32,
    shadow_px: i32,
) -> bool {
    let (cx, cy) = cursor;
    let in_rect = |l: i32, t: i32, r: i32, b: i32| cx >= l && cx <= r && cy >= t && cy <= b;

    // 1) Over the tray icon itself (small forgiving halo)
    if in_rect(icon.left - 6, icon.top - 8, icon.right + 6, icon.bottom + 8) {
        return true;
    }

    // 2) Over the visible capsule body
    let vl = pill_pos.0 + shadow_px;
    let vt = pill_pos.1 + shadow_px;
    let vr = pill_pos.0 + px_w - shadow_px;
    let vb = pill_pos.1 + px_h - shadow_px;
    if in_rect(vl - 6, vt - 6, vr + 6, vb + 6) {
        return true;
    }

    // 3) Bridge corridor between tray icon and capsule
    if vb <= icon.top + 8 {
        // Capsule is above icon (bottom taskbar)
        let bl = icon.left - 14;
        let br = (icon.right + 24).min(vr + 6).max(icon.right + 14);
        if in_rect(bl, vb - 4, br, icon.top + 4) {
            return true;
        }
    } else if vt >= icon.bottom - 8 {
        // Capsule is below icon (top taskbar)
        let bl = icon.left - 14;
        let br = (icon.right + 24).min(vr + 6).max(icon.right + 14);
        if in_rect(bl, icon.bottom - 4, br, vt + 4) {
            return true;
        }
    } else if vr <= icon.left + 8 {
        // Capsule is left of icon (right taskbar)
        let bt = icon.top - 14;
        let bb = icon.bottom + 14;
        if in_rect(vr - 4, bt, icon.left + 4, bb) {
            return true;
        }
    } else if vl >= icon.right - 8 {
        // Capsule is right of icon (left taskbar)
        let bt = icon.top - 14;
        let bb = icon.bottom + 14;
        if in_rect(icon.right - 4, bt, vl + 4, bb) {
            return true;
        }
    }

    false
}

/// Notify peer windows (pill <-> independent dashboard) to reload saved preferences immediately.
pub fn broadcast_prefs_sync(exclude: HWND) {
    unsafe {
        for cls in [w!("GaugePillClass"), w!("GaugeDashboardClass")] {
            if let Ok(target) = FindWindowW(cls, None) {
                if !target.0.is_null() && target != exclude {
                    let _ = PostMessageW(
                        target,
                        WM_APP_SYNC_PREFS,
                        windows::Win32::Foundation::WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct TelemetrySample {
    pub timestamp: f64,
    pub cpu_pct: f32,
    pub mem_pct: f32,
    pub mem_used_gb: f32,
    pub mem_total_gb: f32,
    pub mem_avail_gb: f32,
    pub self_mb: f32,
    pub cpu_cores: u32,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct MonitorDesc {
    pub label: String,
    pub is_primary: bool,
    pub monitor: RECT,
    pub work: RECT,
}

pub struct SingleInstanceGuard {
    handle: Option<HANDLE>,
}

impl SingleInstanceGuard {
    pub fn acquire() -> Option<Self> {
        unsafe {
            let handle = CreateMutexW(None, false, w!("Local\\GaugeRustV1")).ok()?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(handle);
                return None;
            }
            Some(Self { handle: Some(handle) })
        }
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
    }
}

pub struct SystemTelemetry {
    prev_times: Option<(u64, u64, u64)>,
    cpu_cores: u32,
    max_samples: usize,
    pub history: VecDeque<TelemetrySample>,
    pub latest: TelemetrySample,
}

impl SystemTelemetry {
    pub fn new(cap: usize) -> Self {
        let max_samples = cap.max(1);
        let cpu_cores = unsafe {
            let mut si: SYSTEM_INFO = zeroed();
            GetSystemInfo(&mut si);
            si.dwNumberOfProcessors.max(1)
        };
        let prev_times = read_sys_times();
        let (mem_pct, mem_used_gb, mem_total_gb, mem_avail_gb) = read_memory();
        let initial = TelemetrySample {
            timestamp: now_secs(),
            cpu_pct: 0.0,
            mem_pct,
            mem_used_gb,
            mem_total_gb,
            mem_avail_gb,
            self_mb: 0.0,
            cpu_cores,
        };
        Self {
            prev_times,
            cpu_cores,
            max_samples,
            history: VecDeque::with_capacity(max_samples),
            latest: initial,
        }
    }

    /// Re-primes the CPU baseline after long idle/lock/sleep pauses so the next tick reflects
    /// fresh load rather than an average across the paused period.
    pub fn reset_cpu_baseline(&mut self) {
        self.prev_times = read_sys_times();
    }

    /// Queries self process private memory (`K32GetProcessMemoryInfo`) on demand
    /// (called only while Dashboard `Tab::System` is visible or during `--self-test`).
    pub fn refresh_self_private_mb(&mut self) -> f32 {
        let mb = read_self_private_mb();
        self.latest.self_mb = mb;
        mb
    }

    pub fn sample(&mut self) -> TelemetrySample {
        let cur = read_sys_times();
        let mut cpu_pct = self.latest.cpu_pct;
        let mut has_cpu_delta = false;
        if let (Some((pi, pk, pu)), Some((ci, ck, cu))) = (self.prev_times, cur) {
            let di = ci.saturating_sub(pi);
            let dk = ck.saturating_sub(pk);
            let du = cu.saturating_sub(pu);
            let total = dk.saturating_add(du);
            if total > 0 {
                let busy = total.saturating_sub(di);
                cpu_pct = ((busy as f64 / total as f64) * 100.0).clamp(0.0, 100.0) as f32;
                has_cpu_delta = true;
            }
        }
        if cur.is_some() {
            self.prev_times = cur;
        }

        let (mem_pct, mem_used_gb, mem_total_gb, mem_avail_gb) = read_memory();

        let sample = TelemetrySample {
            timestamp: now_secs(),
            cpu_pct,
            mem_pct,
            mem_used_gb,
            mem_total_gb,
            mem_avail_gb,
            self_mb: self.latest.self_mb,
            cpu_cores: self.cpu_cores,
        };
        self.latest = sample;
        if has_cpu_delta {
            while self.history.len() >= self.max_samples {
                self.history.pop_front();
            }
            self.history.push_back(sample);
        }
        sample
    }
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn ft_to_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

fn read_sys_times() -> Option<(u64, u64, u64)> {
    unsafe {
        let mut idle: FILETIME = zeroed();
        let mut kern: FILETIME = zeroed();
        let mut user: FILETIME = zeroed();
        GetSystemTimes(Some(&mut idle), Some(&mut kern), Some(&mut user)).ok()?;
        Some((ft_to_u64(idle), ft_to_u64(kern), ft_to_u64(user)))
    }
}

fn read_memory() -> (f32, f32, f32, f32) {
    unsafe {
        let mut ms: MEMORYSTATUSEX = zeroed();
        ms.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut ms).is_err() {
            return (0.0, 0.0, 0.0, 0.0);
        }
        let gb = (1024.0_f64).powi(3);
        let total = ms.ullTotalPhys as f64 / gb;
        let avail = ms.ullAvailPhys as f64 / gb;
        let used = (total - avail).max(0.0);
        let pct = if total > 0.0 {
            (used / total * 100.0) as f32
        } else {
            ms.dwMemoryLoad as f32
        };
        (pct, used as f32, total as f32, avail as f32)
    }
}

pub fn read_self_private_mb() -> f32 {
    unsafe {
        let mut pmc: PROCESS_MEMORY_COUNTERS_EX = zeroed();
        pmc.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let ok = K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut pmc as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            pmc.cb,
        );
        if !ok.as_bool() {
            return 0.0;
        }
        let bytes = if pmc.PrivateUsage > 0 {
            pmc.PrivateUsage
        } else {
            pmc.WorkingSetSize
        };
        bytes as f32 / (1024.0 * 1024.0)
    }
}

pub fn read_self_working_set_mb() -> f32 {
    unsafe {
        let mut pmc: PROCESS_MEMORY_COUNTERS_EX = zeroed();
        pmc.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        if !K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut pmc as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            pmc.cb,
        )
        .as_bool()
        {
            return 0.0;
        }
        pmc.WorkingSetSize as f32 / (1024.0 * 1024.0)
    }
}

/// Compacts the Win32 default process heap so empty LFH/segment pages are decommitted back to
/// the OS (reducing Private Bytes, not just Working Set).
pub fn compact_self_heap() {
    unsafe {
        if let Ok(heap) = GetProcessHeap() {
            let _ = HeapCompact(heap, HEAP_FLAGS(0));
        }
    }
}

/// Politely return idle dashboard pages (WARP / font cache) of THIS process only to Windows Standby RAM.
pub fn trim_self_working_set() {
    unsafe {
        let _ = K32EmptyWorkingSet(GetCurrentProcess());
    }
}

/// Compacts the process heap first so decommitted pages release Private Bytes, then trims idle
/// Working Set pages to Standby RAM. Call only on discrete lifecycle transitions (startup settle,
/// dashboard hide, pill surface release) — NEVER inside a periodic timer loop.
pub fn compact_heap_and_trim_working_set() {
    compact_self_heap();
    trim_self_working_set();
}

/// Schedules a Windows 8+ coalescable Win32 timer so idle wakeups align with OS timer coalescing
/// for better CPU package C-state residency.
pub fn set_coalesced_timer(hwnd: HWND, id: usize, elapse_ms: u32, tolerance_ms: u32) {
    if hwnd.0.is_null() {
        return;
    }
    let tol = if tolerance_ms == 0 {
        TIMERV_DEFAULT_COALESCING
    } else {
        tolerance_ms
    };
    unsafe {
        let _ = SetCoalescableTimer(hwnd, id, elapse_ms.max(10), None, tol);
    }
}

pub fn register_session_notifications(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION).is_ok() }
}

pub fn unregister_session_notifications(hwnd: HWND) {
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let _ = WTSUnRegisterSessionNotification(hwnd);
    }
}

/// Dynamically registers `GUID_CONSOLE_DISPLAY_STATE` (`WM_POWERBROADCAST` / `PBT_POWERSETTINGCHANGE`)
/// via `user32.dll` so Gauge pauses telemetry when the monitor turns off without extra crate features.
pub struct ConsoleDisplayPowerGuard {
    handle: *mut core::ffi::c_void,
}

#[repr(C)]
struct GuidRaw {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

#[repr(C)]
struct PowerBroadcastSettingHeader {
    power_setting: GuidRaw,
    data_length: u32,
    data: [u8; 4],
}

// {6FE69556-704A-47A0-8F24-C28D936FDA47} = GUID_CONSOLE_DISPLAY_STATE
const GUID_CONSOLE_DISPLAY_STATE_RAW: GuidRaw = GuidRaw {
    data1: 0x6FE6_9556,
    data2: 0x704A,
    data3: 0x47A0,
    data4: [0x8F, 0x24, 0xC2, 0x8D, 0x93, 0x6F, 0xDA, 0x47],
};

impl ConsoleDisplayPowerGuard {
    pub fn register(hwnd: HWND) -> Option<Self> {
        if hwnd.0.is_null() {
            return None;
        }
        unsafe {
            let user32 = GetModuleHandleW(w!("user32.dll")).ok()?;
            let func = GetProcAddress(user32, s!("RegisterPowerSettingNotification"))?;
            let register_fn: unsafe extern "system" fn(
                HANDLE,
                *const GuidRaw,
                u32,
            ) -> *mut core::ffi::c_void = std::mem::transmute(func);
            // DEVICE_NOTIFY_WINDOW_HANDLE = 0
            let h = register_fn(HANDLE(hwnd.0), &GUID_CONSOLE_DISPLAY_STATE_RAW, 0);
            if h.is_null() {
                None
            } else {
                Some(Self { handle: h })
            }
        }
    }
}

impl Drop for ConsoleDisplayPowerGuard {
    fn drop(&mut self) {
        if self.handle.is_null() {
            return;
        }
        unsafe {
            if let Ok(user32) = GetModuleHandleW(w!("user32.dll")) {
                if let Some(func) = GetProcAddress(user32, s!("UnregisterPowerSettingNotification")) {
                    let unregister_fn: unsafe extern "system" fn(*mut core::ffi::c_void) -> i32 =
                        std::mem::transmute(func);
                    let _ = unregister_fn(self.handle);
                }
            }
        }
        self.handle = ptr::null_mut();
    }
}

/// Parses `PBT_POWERSETTINGCHANGE` `lparam` for `GUID_CONSOLE_DISPLAY_STATE`.
/// Returns `Some(0)` when display is OFF, `Some(1)` when ON, `Some(2)` when dimmed.
pub fn parse_console_display_state(lparam: LPARAM) -> Option<u32> {
    if lparam.0 == 0 {
        return None;
    }
    unsafe {
        let hdr = &*(lparam.0 as *const PowerBroadcastSettingHeader);
        let g = &hdr.power_setting;
        if g.data1 == GUID_CONSOLE_DISPLAY_STATE_RAW.data1
            && g.data2 == GUID_CONSOLE_DISPLAY_STATE_RAW.data2
            && g.data3 == GUID_CONSOLE_DISPLAY_STATE_RAW.data3
            && g.data4 == GUID_CONSOLE_DISPLAY_STATE_RAW.data4
            && hdr.data_length >= 4
        {
            Some(u32::from_le_bytes(hdr.data))
        } else {
            None
        }
    }
}

fn u16_eq_ascii_icase(units: &[u16], ascii: &[u8]) -> bool {
    if units.len() != ascii.len() {
        return false;
    }
    units
        .iter()
        .zip(ascii.iter())
        .all(|(&u, &b)| u <= 0x7F && (u as u8).eq_ignore_ascii_case(&b))
}

fn u16_ends_with_ascii_icase(units: &[u16], suffix: &[u8]) -> bool {
    if units.len() < suffix.len() {
        return false;
    }
    u16_eq_ascii_icase(&units[units.len() - suffix.len()..], suffix)
}

fn u16_starts_with_ascii_icase(units: &[u16], prefix: &[u8]) -> bool {
    if units.len() < prefix.len() {
        return false;
    }
    u16_eq_ascii_icase(&units[..prefix.len()], prefix)
}

fn u16_file_name(units: &[u16]) -> &[u16] {
    match units.iter().rposition(|&c| c == b'\\' as u16 || c == b'/' as u16) {
        Some(pos) => &units[pos + 1..],
        None => units,
    }
}

fn u16_has_rejected_dir_segment(units: &[u16]) -> bool {
    // Only check directory components (excluding the trailing file name itself).
    let Some(last_sep) = units.iter().rposition(|&c| c == b'\\' as u16 || c == b'/' as u16) else {
        return false;
    };
    for seg in units[..last_sep].split(|&c| c == b'\\' as u16 || c == b'/' as u16) {
        if seg.is_empty() {
            continue;
        }
        if u16_eq_ascii_icase(seg, b"steps")
            || u16_eq_ascii_icase(seg, b"scratch")
            || u16_eq_ascii_icase(seg, b"tasks")
            || u16_eq_ascii_icase(seg, b"conversations")
        {
            return true;
        }
    }
    false
}

/// Zero-allocation UTF-16 relative-path filter for `ReadDirectoryChangesW` events.
/// Rejects noisy agent scratch/step/log/media/SQLite/temp writes and Gauge's own `archive.json`
/// so the UI thread is woken only for whitelisted token/config inputs.
pub fn is_watched_rel_path_relevant_u16(rel_units: &[u16], subtree: bool) -> bool {
    if rel_units.is_empty() {
        return false;
    }
    let name = u16_file_name(rel_units);
    if name.is_empty() {
        return false;
    }

    if !subtree {
        // Companion / exe directory: ONLY wake on external companion JSON inputs (never archive.json / *.tmp).
        return u16_eq_ascii_icase(name, b"statusline_state.json")
            || u16_eq_ascii_icase(name, b"pricing.json")
            || u16_eq_ascii_icase(name, b"budget.json")
            || u16_eq_ascii_icase(name, b"session_meta.json");
    }

    // Agent log subtree (`brain/`, `annotations/`, `cache/`, `projects/`, `sessions/`, `tmp/`, etc.):
    // 1) Reject high-churn noisy subdirectories (`steps\`, `scratch\`, `tasks\`, `conversations\`).
    if u16_has_rejected_dir_segment(rel_units) {
        return false;
    }

    // 2) Explicitly reject temp, SQLite/DB, image, markdown, text, and non-whitelisted secret/archive files.
    for bad_ext in [
        b".tmp".as_slice(),
        b".part",
        b".bak",
        b".swp",
        b".log",
        b".sqlite",
        b".sqlite3",
        b".sqlite-wal",
        b".sqlite-shm",
        b".sqlite-journal",
        b".db",
        b".db-wal",
        b".db-shm",
        b".db-journal",
        b".png",
        b".jpg",
        b".jpeg",
        b".webp",
        b".gif",
        b".svg",
        b".ico",
        b".md",
        b".txt",
        b".py",
        b".lock",
    ] {
        if u16_ends_with_ascii_icase(name, bad_ext) {
            return false;
        }
    }
    for bad_name in [
        b"archive.json".as_slice(),
        b"history.jsonl",
        b"auth.json",
        b"oauth_creds.json",
        b"google_accounts.json",
        b".credentials.json",
    ] {
        if u16_eq_ascii_icase(name, bad_name) {
            return false;
        }
    }
    if u16_starts_with_ascii_icase(name, b"settings") && u16_ends_with_ascii_icase(name, b".json") {
        return false;
    }

    // 3) Accept whitelisted filenames and JSON/JSONL/PBTXT log files.
    u16_eq_ascii_icase(name, b"transcript.jsonl")
        || u16_eq_ascii_icase(name, b"transcript_full.jsonl")
        || u16_eq_ascii_icase(name, b"conversation_metadata.json")
        || u16_eq_ascii_icase(name, b"sessions-index.json")
        || u16_eq_ascii_icase(name, b"session_index.jsonl")
        || u16_eq_ascii_icase(name, b"projects.json")
        || u16_eq_ascii_icase(name, b".project_root")
        || u16_ends_with_ascii_icase(name, b".pbtxt")
        || u16_ends_with_ascii_icase(name, b".jsonl")
        || u16_ends_with_ascii_icase(name, b".json")
}

const WATCHER_HINT_CAP: usize = 64;

/// Coalesced file-change hints collected by `DirWatcher` between debounce windows.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WatcherHints {
    pub paths: Vec<PathBuf>,
    pub overflow: bool,
    pub has_companion_change: bool,
}

impl WatcherHints {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.overflow && !self.has_companion_change
    }

    /// Records a relevant relative path under `watch_dir`.
    /// Returns `true` ONLY on the empty -> non-empty transition so the watcher thread posts
    /// at most one `PostMessageW` wakeup per debounce window.
    pub fn record_rel_u16(&mut self, watch_dir: &Path, rel_units: &[u16], subtree: bool) -> bool {
        let was_empty = self.is_empty();
        if !subtree {
            self.has_companion_change = true;
            return was_empty;
        }
        if self.overflow {
            return false;
        }
        let full = watch_dir.join(OsString::from_wide(rel_units));
        if !self.paths.iter().any(|p| p == &full) {
            if self.paths.len() >= WATCHER_HINT_CAP {
                self.paths.clear();
                self.overflow = true;
            } else {
                self.paths.push(full);
            }
        }
        was_empty
    }

    /// Marks the hint queue overflowed (triggers full fallback scan).
    /// Returns `true` ONLY on the empty -> non-empty transition.
    pub fn mark_overflow(&mut self) -> bool {
        let was_empty = self.is_empty();
        self.paths.clear();
        self.overflow = true;
        was_empty
    }
}

/// Parses a raw `FILE_NOTIFY_INFORMATION` buffer returned by `ReadDirectoryChangesW`,
/// rejecting noisy files with zero heap allocations and locking `queue` lazily only when
/// at least one whitelisted path matches. Returns `true` if `queue` transitioned empty -> non-empty.
fn collect_notify_buffer_into_queue(
    buf: &[u32],
    bytes_returned: usize,
    watch_dir: &Path,
    subtree: bool,
    queue: &Mutex<WatcherHints>,
) -> bool {
    let raw_bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * size_of::<u32>())
    };
    let limit = bytes_returned.min(raw_bytes.len());
    let header_sz = size_of::<FILE_NOTIFY_INFORMATION>();
    if limit < header_sz {
        return false;
    }

    let mut guard: Option<std::sync::MutexGuard<'_, WatcherHints>> = None;
    let mut should_notify = false;
    let mut offset = 0usize;
    while offset + header_sz <= limit {
        // Every FILE_NOTIFY_INFORMATION entry is DWORD-aligned (4 bytes).
        if offset % 4 != 0 {
            break;
        }
        let info = unsafe { &*(raw_bytes.as_ptr().add(offset) as *const FILE_NOTIFY_INFORMATION) };
        let name_bytes = info.FileNameLength as usize;
        // FileName begins immediately after NextEntryOffset (4) + Action (4) + FileNameLength (4) = 12 bytes.
        let name_start = offset + 12;
        if name_bytes >= 2 && name_start + name_bytes <= limit {
            let units = unsafe {
                std::slice::from_raw_parts(
                    raw_bytes.as_ptr().add(name_start) as *const u16,
                    name_bytes / 2,
                )
            };
            if is_watched_rel_path_relevant_u16(units, subtree) {
                if guard.is_none() {
                    guard = queue.lock().ok();
                }
                if let Some(ref mut g) = guard {
                    if g.record_rel_u16(watch_dir, units, subtree) {
                        should_notify = true;
                    }
                }
            }
        }
        if info.NextEntryOffset == 0 {
            break;
        }
        let next = offset.saturating_add(info.NextEntryOffset as usize);
        if next <= offset {
            break;
        }
        offset = next;
    }
    should_notify
}

struct WatchSlot {
    watch_dir: PathBuf,
    dir_handle: HANDLE,
    event_handle: HANDLE,
    subtree: bool,
    active: bool,
    // Heap-pinned so kernel overlapped I/O pointers remain valid even if the outer Vec mutates.
    ov: Box<OVERLAPPED>,
    // 4 KiB DWORD-aligned buffer for FILE_NOTIFY_INFORMATION records.
    buf: Box<[u32; 1024]>,
}

impl WatchSlot {
    unsafe fn arm(&mut self, filter: FILE_NOTIFY_CHANGE) -> bool {
        let _ = ResetEvent(self.event_handle);
        *self.ov = zeroed();
        self.ov.hEvent = self.event_handle;
        let res = ReadDirectoryChangesW(
            self.dir_handle,
            self.buf.as_mut_ptr().cast(),
            (self.buf.len() * size_of::<u32>()) as u32,
            self.subtree,
            filter,
            None,
            Some(&mut *self.ov),
            None,
        );
        match res {
            Ok(()) => {
                self.active = true;
                true
            }
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                self.active = true;
                true
            }
            Err(_) => {
                self.active = false;
                false
            }
        }
    }

    unsafe fn cancel_and_close(mut self) {
        if self.active {
            let _ = CancelIoEx(self.dir_handle, Some(&*self.ov));
            let mut dummy = 0u32;
            // Wait synchronously until kernel cancels the pending overlapped request so `ov` and `buf`
            // are guaranteed no longer referenced by the OS before being dropped.
            let _ = GetOverlappedResult(self.dir_handle, &*self.ov, &mut dummy, true);
            self.active = false;
        }
        let _ = CloseHandle(self.event_handle);
        let _ = CloseHandle(self.dir_handle);
    }
}

/// Lightweight sleeping directory-change watcher using overlapped `ReadDirectoryChangesW` +
/// `WaitForMultipleObjects` with strict UTF-16 filename filtering and targeted path hint queuing.
/// Consumes 0% CPU while idle and posts at most one UI wakeup per debounce window.
pub struct DirWatcher {
    stop_event: HANDLE,
    hints: Arc<Mutex<WatcherHints>>,
    healthy: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl DirWatcher {
    pub fn start(watches: Vec<(PathBuf, bool)>, notify_hwnd: HWND, notify_msg: u32) -> Option<Self> {
        if notify_hwnd.0.is_null() || watches.is_empty() {
            return None;
        }
        let mut unique: Vec<(PathBuf, bool)> = Vec::new();
        for (dir, subtree) in watches {
            if !dir.is_dir() {
                continue;
            }
            if let Some(existing) = unique.iter_mut().find(|(p, _)| *p == dir) {
                existing.1 |= subtree;
                continue;
            }
            unique.push((dir, subtree));
            if unique.len() >= 63 {
                // MAXIMUM_WAIT_OBJECTS = 64; index 0 is reserved for stop_event.
                break;
            }
        }
        if unique.is_empty() {
            return None;
        }

        let stop_event = unsafe { CreateEventW(None, true, false, None).ok()? };
        let stop_raw = stop_event.0 as isize;
        let hwnd_raw = notify_hwnd.0 as isize;
        let hints = Arc::new(Mutex::new(WatcherHints::default()));
        let hints_bg = Arc::clone(&hints);
        let healthy = Arc::new(AtomicBool::new(true));
        let healthy_bg = Arc::clone(&healthy);

        let builder = thread::Builder::new()
            .name("gauge-watch".to_string())
            .stack_size(64 * 1024);

        let join = match builder.spawn(move || {
            let stop_h = HANDLE(stop_raw as *mut core::ffi::c_void);
            let target_hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
            let filter = FILE_NOTIFY_CHANGE_FILE_NAME
                | FILE_NOTIFY_CHANGE_DIR_NAME
                | FILE_NOTIFY_CHANGE_SIZE
                | FILE_NOTIFY_CHANGE_LAST_WRITE;

            let expected_slots = unique.len();
            let mut slots: Vec<WatchSlot> = Vec::with_capacity(expected_slots);
            for (dir, subtree) in unique {
                let wide = to_wide_null(&dir.to_string_lossy());
                let dir_h = unsafe {
                    CreateFileW(
                        wide_pcwstr(&wide),
                        FILE_LIST_DIRECTORY.0,
                        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                        None,
                        OPEN_EXISTING,
                        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                        None,
                    )
                };
                let Ok(dir_handle) = dir_h else {
                    continue;
                };
                if dir_handle.is_invalid() || dir_handle.0.is_null() {
                    continue;
                }
                let Ok(event_handle) = (unsafe { CreateEventW(None, true, false, None) }) else {
                    unsafe {
                        let _ = CloseHandle(dir_handle);
                    }
                    continue;
                };
                let mut slot = WatchSlot {
                    watch_dir: dir,
                    dir_handle,
                    event_handle,
                    subtree,
                    active: false,
                    ov: Box::new(OVERLAPPED::default()),
                    buf: Box::new([0u32; 1024]),
                };
                if unsafe { slot.arm(filter) } {
                    slots.push(slot);
                } else {
                    unsafe { slot.cancel_and_close() };
                }
            }

            if slots.len() < expected_slots || slots.is_empty() {
                healthy_bg.store(false, Ordering::Release);
            }

            let mut wait_handles: Vec<HANDLE> = Vec::with_capacity(slots.len() + 1);
            if !slots.is_empty() {
                loop {
                    wait_handles.clear();
                    wait_handles.push(stop_h);
                    for s in &slots {
                        wait_handles.push(s.event_handle);
                    }
                    let wait = unsafe { WaitForMultipleObjects(&wait_handles, false, INFINITE) };
                    if wait == WAIT_OBJECT_0 {
                        break;
                    }
                    let base = WAIT_OBJECT_0.0;
                    if wait.0 < base || (wait.0 - base) as usize >= wait_handles.len() {
                        healthy_bg.store(false, Ordering::Release);
                        break;
                    }
                    let slot_idx = (wait.0 - base) as usize - 1;
                    let mut bytes_returned = 0u32;
                    let ov_res = unsafe {
                        GetOverlappedResult(
                            slots[slot_idx].dir_handle,
                            &*slots[slot_idx].ov,
                            &mut bytes_returned,
                            false,
                        )
                    };
                    slots[slot_idx].active = false;

                    let (mut should_post, fatal) = match ov_res {
                        Ok(()) => {
                            if bytes_returned == 0 {
                                // Kernel notify buffer overflowed: force a fallback full scan.
                                let post = hints_bg
                                    .lock()
                                    .map(|mut g| g.mark_overflow())
                                    .unwrap_or(true);
                                (post, false)
                            } else {
                                let post = collect_notify_buffer_into_queue(
                                    &*slots[slot_idx].buf,
                                    bytes_returned as usize,
                                    &slots[slot_idx].watch_dir,
                                    slots[slot_idx].subtree,
                                    &hints_bg,
                                );
                                (post, false)
                            }
                        }
                        Err(e) if e.code() == ERROR_NOTIFY_ENUM_DIR.to_hresult() => {
                            let post = hints_bg
                                .lock()
                                .map(|mut g| g.mark_overflow())
                                .unwrap_or(true);
                            (post, false)
                        }
                        Err(e) if e.code() == ERROR_OPERATION_ABORTED.to_hresult() => (false, true),
                        Err(_) => (false, true),
                    };

                    if fatal || !unsafe { slots[slot_idx].arm(filter) } {
                        healthy_bg.store(false, Ordering::Release);
                        if let Ok(mut g) = hints_bg.lock() {
                            if g.mark_overflow() {
                                should_post = true;
                            }
                        }
                        let dead = slots.remove(slot_idx);
                        unsafe { dead.cancel_and_close() };
                        if slots.is_empty() {
                            if should_post {
                                unsafe {
                                    let _ = PostMessageW(target_hwnd, notify_msg, WPARAM(0), LPARAM(0));
                                }
                            }
                            break;
                        }
                    }

                    if should_post {
                        unsafe {
                            let _ = PostMessageW(target_hwnd, notify_msg, WPARAM(0), LPARAM(0));
                        }
                    }
                }
            }

            healthy_bg.store(false, Ordering::Release);
            for slot in slots {
                unsafe { slot.cancel_and_close() };
            }
        }) {
            Ok(j) => j,
            Err(_) => {
                unsafe {
                    let _ = CloseHandle(stop_event);
                }
                return None;
            }
        };

        Some(Self {
            stop_event,
            hints,
            healthy,
            join: Some(join),
        })
    }

    /// Returns `true` when all requested watched directories are actively armed on the OS watcher.
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    /// Drains all queued file-change hints since the last scan, resetting the queue to empty
    /// so the next relevant change posts a fresh UI notification.
    pub fn drain_hints(&self) -> WatcherHints {
        match self.hints.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(_) => WatcherHints {
                paths: Vec::new(),
                overflow: true,
                has_companion_change: false,
            },
        }
    }
}

impl Drop for DirWatcher {
    fn drop(&mut self) {
        self.healthy.store(false, Ordering::Release);
        unsafe {
            let _ = SetEvent(self.stop_event);
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        unsafe {
            let _ = CloseHandle(self.stop_event);
        }
    }
}

pub fn enable_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

pub fn window_dpi_scale(hwnd: HWND) -> f32 {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        if dpi > 0 {
            dpi as f32 / 96.0
        } else {
            1.0
        }
    }
}

pub fn dpi_for_point(x: i32, y: i32) -> f32 {
    unsafe {
        let hmon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut dx = 96u32;
        let mut dy = 96u32;
        if GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_ok() && dx > 0 {
            dx as f32
        } else {
            96.0
        }
    }
}

fn monitors_cache() -> &'static Mutex<Option<Vec<MonitorDesc>>> {
    static CACHE: OnceLock<Mutex<Option<Vec<MonitorDesc>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Invalidates the cached monitor geometry list (call on `WM_DISPLAYCHANGE`,
/// `WM_DPICHANGED`, and `WM_SETTINGCHANGE(SPI_SETWORKAREA)`).
pub fn invalidate_monitors_cache() {
    if let Ok(mut guard) = monitors_cache().lock() {
        *guard = None;
    }
}

fn with_cached_monitors<R>(f: impl FnOnce(&[MonitorDesc]) -> R) -> R {
    if let Ok(mut guard) = monitors_cache().lock() {
        let list = guard.get_or_insert_with(enumerate_monitors_uncached);
        return f(list);
    }
    let fresh = enumerate_monitors_uncached();
    f(&fresh)
}

fn enumerate_monitors_uncached() -> Vec<MonitorDesc> {
    struct RawMon {
        is_primary: bool,
        monitor: RECT,
        work: RECT,
        w: i32,
        h: i32,
    }
    unsafe extern "system" fn enum_proc(
        hmon: HMONITOR,
        _hdc: HDC,
        _rc: *mut RECT,
        lparam: LPARAM,
    ) -> windows::Win32::Foundation::BOOL {
        let list = &mut *(lparam.0 as *mut Vec<RawMon>);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            list.push(RawMon {
                is_primary: (mi.dwFlags & 1) != 0,
                monitor: mi.rcMonitor,
                work: mi.rcWork,
                w: mi.rcMonitor.right - mi.rcMonitor.left,
                h: mi.rcMonitor.bottom - mi.rcMonitor.top,
            });
        }
        true.into()
    }

    let mut raw: Vec<RawMon> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(enum_proc),
            LPARAM((&mut raw as *mut Vec<RawMon>) as isize),
        );
    }

    if raw.is_empty() {
        let mut rc: RECT = unsafe { zeroed() };
        unsafe {
            let _ = SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some((&mut rc as *mut RECT).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
        }
        return vec![MonitorDesc {
            label: "主屏".to_string(),
            is_primary: true,
            monitor: rc,
            work: rc,
        }];
    }

    raw.sort_by_key(|m| (!m.is_primary, m.work.left, m.work.top));
    raw.into_iter()
        .enumerate()
        .map(|(idx, m)| {
            let tag = if m.is_primary {
                format!("主屏 ({}×{})", m.w, m.h)
            } else {
                format!("屏幕 {} ({}×{})", idx + 1, m.w, m.h)
            };
            MonitorDesc {
                label: tag,
                is_primary: m.is_primary,
                monitor: m.monitor,
                work: m.work,
            }
        })
        .collect()
}

pub fn get_monitors() -> Vec<MonitorDesc> {
    with_cached_monitors(|mons| mons.to_vec())
}

pub fn get_work_area(monitor_index: usize) -> RECT {
    with_cached_monitors(|mons| {
        if let Some(m) = mons.get(monitor_index) {
            m.work
        } else if let Some(m) = mons.first() {
            m.work
        } else {
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040,
            }
        }
    })
}

pub fn apply_win11_frame(hwnd: HWND, dark: bool, caption_colorref: u32) {
    unsafe {
        let dark_val: i32 = if dark { 1 } else { 0 };
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&dark_val as *const i32).cast(),
            size_of::<i32>() as u32,
        );
        let round: i32 = DWMWCP_ROUND.0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&round as *const i32).cast(),
            size_of::<i32>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            (&caption_colorref as *const u32).cast(),
            size_of::<u32>() as u32,
        );
        let no_anim: i32 = 1;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            (&no_anim as *const i32).cast(),
            size_of::<i32>() as u32,
        );
    }
}

pub fn copy_text_to_clipboard(hwnd: HWND, text: &str) -> bool {
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * 2;
    unsafe {
        if OpenClipboard(hwnd).is_err() {
            return false;
        }
        let _ = EmptyClipboard();
        let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
            let _ = CloseClipboard();
            return false;
        };
        let dst = GlobalLock(hmem) as *mut u16;
        if dst.is_null() {
            let _ = GlobalFree(hmem);
            let _ = CloseClipboard();
            return false;
        }
        ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
        let _ = GlobalUnlock(hmem);
        let ok = SetClipboardData(CF_UNICODETEXT, HANDLE(hmem.0)).is_ok();
        if !ok {
            let _ = GlobalFree(hmem);
        }
        let _ = CloseClipboard();
        ok
    }
}

pub fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn wide_pcwstr(buf: &[u16]) -> PCWSTR {
    PCWSTR(buf.as_ptr())
}

// ---------- Windows System Tray, Balloon Alerts, Autostart, Fullscreen & File Dialog ----------

const RUN_SUBKEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const AUTOSTART_NAME: PCWSTR = w!("Gauge");
const LEGACY_AUTOSTART_NAME: PCWSTR = w!("AgyGauge");

pub fn is_autostart_enabled() -> bool {
    unsafe {
        let mut sz: u32 = 0;
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_SUBKEY,
            AUTOSTART_NAME,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut sz),
        )
        .is_ok()
            && sz > 2
    }
}

pub fn set_autostart_enabled(enable: bool) -> bool {
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_SUBKEY, 0, KEY_READ | KEY_WRITE, &mut hkey).is_err() {
            return false;
        }
        let ok = if enable {
            if let Ok(exe) = std::env::current_exe() {
                let cmd = format!("\"{}\"", exe.display());
                let wide = to_wide_null(&cmd);
                let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
                let written = RegSetValueExW(hkey, AUTOSTART_NAME, 0, REG_SZ, Some(bytes)).is_ok();
                if written {
                    let _ = RegDeleteValueW(hkey, LEGACY_AUTOSTART_NAME);
                }
                written
            } else {
                false
            }
        } else {
            let removed_new = RegDeleteValueW(hkey, AUTOSTART_NAME).is_ok();
            let removed_old = RegDeleteValueW(hkey, LEGACY_AUTOSTART_NAME).is_ok();
            removed_new || removed_old
        };
        let _ = RegCloseKey(hkey);
        ok
    }
}

pub fn is_other_app_fullscreen() -> bool {
    unsafe {
        match SHQueryUserNotificationState() {
            Ok(state) => matches!(
                state,
                QUNS_BUSY | QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE
            ),
            Err(_) => false,
        }
    }
}

/// Gates `SHQueryUserNotificationState()` behind foreground HWND + rect change detection
/// with a 4-second fallback so idle ticks avoid redundant Explorer/Shell IPC queries.
#[derive(Default)]
pub struct FullscreenProbe {
    last_fg: isize,
    last_fg_rect: RECT,
    last_checked: Option<Instant>,
    last_result: bool,
}

impl FullscreenProbe {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn invalidate(&mut self) {
        self.last_checked = None;
    }

    pub fn check(&mut self, now: Instant) -> bool {
        let (fg_raw, fg_rect) = unsafe {
            let fg = GetForegroundWindow();
            let mut rc = RECT::default();
            if !fg.0.is_null() {
                let _ = GetWindowRect(fg, &mut rc);
            }
            (fg.0 as isize, rc)
        };
        if let Some(prev) = self.last_checked {
            if fg_raw == self.last_fg
                && fg_rect == self.last_fg_rect
                && now.duration_since(prev) < Duration::from_secs(4)
            {
                return self.last_result;
            }
        }
        let res = is_other_app_fullscreen();
        self.last_fg = fg_raw;
        self.last_fg_rect = fg_rect;
        self.last_checked = Some(now);
        self.last_result = res;
        res
    }
}

pub fn register_taskbar_created_msg() -> u32 {
    unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
}

fn copy_utf16_fixed<const N: usize>(dst: &mut [u16; N], src: &str) {
    dst.fill(0);
    let mut i = 0usize;
    for u in src.encode_utf16() {
        if i + 1 >= N {
            break;
        }
        dst[i] = u;
        i += 1;
    }
}

pub struct TrayIcon {
    hwnd: HWND,
    icon: HICON,
    active: bool,
}

impl TrayIcon {
    pub fn new(hwnd: HWND, icon: HICON, tip: &str) -> Self {
        let mut t = Self {
            hwnd,
            icon,
            active: false,
        };
        t.add(tip);
        t
    }

    pub fn add(&mut self, tip: &str) {
        if self.hwnd.0.is_null() {
            return;
        }
        unsafe {
            let mut nid: NOTIFYICONDATAW = zeroed();
            nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd;
            nid.uID = 1;
            nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            nid.uCallbackMessage = WM_APP_TRAY;
            nid.hIcon = self.icon;
            copy_utf16_fixed(&mut nid.szTip, tip);
            if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                self.active = true;
            }
        }
    }

    pub fn update_tooltip(&self, tip: &str) {
        if !self.active || self.hwnd.0.is_null() {
            return;
        }
        unsafe {
            let mut nid: NOTIFYICONDATAW = zeroed();
            nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd;
            nid.uID = 1;
            nid.uFlags = NIF_TIP;
            copy_utf16_fixed(&mut nid.szTip, tip);
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    pub fn update_icon(&mut self, icon: HICON) {
        if icon.0.is_null() || self.icon == icon {
            return;
        }
        self.icon = icon;
        if !self.active || self.hwnd.0.is_null() {
            return;
        }
        unsafe {
            let mut nid: NOTIFYICONDATAW = zeroed();
            nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd;
            nid.uID = 1;
            nid.uFlags = NIF_ICON;
            nid.hIcon = self.icon;
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    pub fn get_rect(&self) -> Option<RECT> {
        if !self.active || self.hwnd.0.is_null() {
            return None;
        }
        unsafe {
            let mut id: NOTIFYICONIDENTIFIER = zeroed();
            id.cbSize = size_of::<NOTIFYICONIDENTIFIER>() as u32;
            id.hWnd = self.hwnd;
            id.uID = 1;
            Shell_NotifyIconGetRect(&id)
                .ok()
                .filter(|r| r.right > r.left && r.bottom > r.top)
        }
    }

    pub fn show_balloon(&self, title: &str, message: &str, is_critical: bool, is_warn: bool) {
        if !self.active || self.hwnd.0.is_null() {
            return;
        }
        unsafe {
            let mut nid: NOTIFYICONDATAW = zeroed();
            nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd;
            nid.uID = 1;
            nid.uFlags = NIF_INFO;
            nid.dwInfoFlags = if is_critical {
                NIIF_ERROR
            } else if is_warn {
                NIIF_WARNING | NIIF_NOSOUND
            } else {
                NIIF_INFO | NIIF_NOSOUND
            };
            copy_utf16_fixed(&mut nid.szInfoTitle, title);
            copy_utf16_fixed(&mut nid.szInfo, message);
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    pub fn remove(&mut self) {
        if !self.active || self.hwnd.0.is_null() {
            return;
        }
        unsafe {
            let mut nid: NOTIFYICONDATAW = zeroed();
            nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd;
            nid.uID = 1;
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
        self.active = false;
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        self.remove();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayPinResult {
    NotFound,
    AlreadyMatched,
    UpdatedNow,
}

fn desired_tray_promoted_write(current: Option<u32>, want_promoted: bool) -> Option<u32> {
    let currently_promoted = current.unwrap_or(0) != 0;
    if currently_promoted == want_promoted {
        None
    } else {
        Some(u32::from(want_promoted))
    }
}

fn normalize_exe_path_for_compare(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('"');
    let stripped = trimmed
        .strip_prefix(r"\\?\")
        .or_else(|| trimmed.strip_prefix("//?/"))
        .unwrap_or(trimmed);
    stripped.replace('/', "\\").to_ascii_lowercase()
}

fn reg_read_key_u32(hkey: HKEY, name: PCWSTR) -> Option<u32> {
    unsafe {
        let mut val: u32 = 0;
        let mut sz: u32 = size_of::<u32>() as u32;
        if RegGetValueW(
            hkey,
            None,
            name,
            RRF_RT_REG_DWORD,
            None,
            Some((&mut val as *mut u32).cast()),
            Some(&mut sz),
        )
        .is_ok()
        {
            Some(val)
        } else {
            None
        }
    }
}

fn reg_read_key_string(hkey: HKEY, name: PCWSTR) -> Option<String> {
    unsafe {
        let mut sz: u32 = 0;
        // RRF_RT_REG_SZ (without RRF_NOEXPAND) reads REG_SZ and auto-expands REG_EXPAND_SZ.
        // Passing RRF_RT_REG_EXPAND_SZ without RRF_NOEXPAND fails with ERROR_INVALID_PARAMETER.
        let flags = RRF_RT_REG_SZ;
        if RegGetValueW(hkey, None, name, flags, None, None, Some(&mut sz)).is_err() || sz < 2 {
            return None;
        }
        let mut buf = vec![0u16; (sz as usize + 1) / 2];
        if RegGetValueW(
            hkey,
            None,
            name,
            flags,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut sz),
        )
        .is_err()
        {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

/// Sets whether this executable's notification icon is promoted (pinned) directly onto the visible
/// Windows 11 taskbar (`promoted = true`, `IsPromoted = 1`, beside IME/clock outside `^`) or folded
/// cleanly back into the `^` overflow chevron (`promoted = false`, `IsPromoted = 0`) in the user's
/// `HKCU\Control Panel\NotifyIconSettings\<id>` entry.
pub fn set_self_tray_icon_taskbar_promoted(promoted: bool) -> TrayPinResult {
    let Ok(self_exe) = std::env::current_exe() else {
        return TrayPinResult::NotFound;
    };
    let target = normalize_exe_path_for_compare(&self_exe.to_string_lossy());
    if target.is_empty() {
        return TrayPinResult::NotFound;
    }

    const NOTIFY_SETTINGS_KEY: PCWSTR = w!("Control Panel\\NotifyIconSettings");
    let mut found_matched = false;
    let mut updated_now = false;

    unsafe {
        let mut root = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            NOTIFY_SETTINGS_KEY,
            0,
            KEY_READ | KEY_ENUMERATE_SUB_KEYS,
            &mut root,
        )
        .is_err()
        {
            return TrayPinResult::NotFound;
        }

        let mut index = 0u32;
        let mut name_buf = [0u16; 256];
        loop {
            let mut name_len = name_buf.len() as u32;
            let status = RegEnumKeyExW(
                root,
                index,
                PWSTR(name_buf.as_mut_ptr()),
                &mut name_len,
                None,
                PWSTR::null(),
                None,
                None,
            );
            if status.is_err() {
                break;
            }
            index += 1;

            let sub_name = PCWSTR(name_buf.as_ptr());
            let mut sub = HKEY::default();
            if RegOpenKeyExW(root, sub_name, 0, KEY_READ, &mut sub).is_err() {
                continue;
            }
            let exe_match = reg_read_key_string(sub, w!("ExecutablePath"))
                .map(|p| normalize_exe_path_for_compare(&p) == target)
                .unwrap_or(false);
            if !exe_match {
                let _ = RegCloseKey(sub);
                continue;
            }
            let uid_match = reg_read_key_u32(sub, w!("UID"))
                .map(|u| u == 1)
                .unwrap_or(true);
            if !uid_match {
                let _ = RegCloseKey(sub);
                continue;
            }
            let cur_promoted = reg_read_key_u32(sub, w!("IsPromoted"));
            let _ = RegCloseKey(sub);
            let Some(next_val) = desired_tray_promoted_write(cur_promoted, promoted) else {
                found_matched = true;
                continue;
            };

            let mut sub_w = HKEY::default();
            if RegOpenKeyExW(root, sub_name, 0, KEY_SET_VALUE, &mut sub_w).is_ok() {
                let bytes = next_val.to_le_bytes();
                if RegSetValueExW(sub_w, w!("IsPromoted"), 0, REG_DWORD, Some(&bytes)).is_ok() {
                    updated_now = true;
                }
                let _ = RegCloseKey(sub_w);
            }
        }
        let _ = RegCloseKey(root);
    }

    if updated_now {
        TrayPinResult::UpdatedNow
    } else if found_matched {
        TrayPinResult::AlreadyMatched
    } else {
        TrayPinResult::NotFound
    }
}

pub fn prompt_save_file(
    owner: HWND,
    default_filename: &str,
    filter_label: &str,
    filter_pattern: &str,
    def_ext: &str,
) -> Option<std::path::PathBuf> {
    let mut file_buf = vec![0u16; 1024];
    for (i, u) in default_filename.encode_utf16().take(file_buf.len() - 1).enumerate() {
        file_buf[i] = u;
    }
    let mut filter_wide: Vec<u16> = Vec::new();
    filter_wide.extend(filter_label.encode_utf16());
    filter_wide.push(0);
    filter_wide.extend(filter_pattern.encode_utf16());
    filter_wide.push(0);
    filter_wide.extend("All Files (*.*)".encode_utf16());
    filter_wide.push(0);
    filter_wide.extend("*.*".encode_utf16());
    filter_wide.push(0);
    filter_wide.push(0);

    let ext_wide = to_wide_null(def_ext);

    unsafe {
        let mut ofn: OPENFILENAMEW = zeroed();
        ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
        ofn.hwndOwner = owner;
        ofn.lpstrFilter = PCWSTR(filter_wide.as_ptr());
        ofn.lpstrFile = windows::core::PWSTR(file_buf.as_mut_ptr());
        ofn.nMaxFile = file_buf.len() as u32;
        ofn.lpstrDefExt = PCWSTR(ext_wide.as_ptr());
        ofn.Flags = OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
        if GetSaveFileNameW(&mut ofn).as_bool() {
            let len = file_buf.iter().position(|&c| c == 0).unwrap_or(file_buf.len());
            if len > 0 {
                return Some(std::path::PathBuf::from(String::from_utf16_lossy(&file_buf[..len])));
            }
        }
    }
    None
}

// ---------- Robust Topmost Z-Order Diagnostics & Recovery ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopmostHealth {
    /// Pill still carries `WS_EX_TOPMOST` and sits above all regular windows & non-system overlays.
    Healthy,
    /// Pill still carries `WS_EX_TOPMOST` but another non-system topmost window overlaps it.
    NeedRaise(isize),
    /// `WS_EX_TOPMOST` was stripped (0) or a non-topmost window sits above it (blocker HWND).
    NeedHardReset(isize),
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct WindowAboveDesc {
    pub hwnd_raw: isize,
    pub class_name: String,
    pub is_topmost: bool,
    pub rect: RECT,
}

pub fn rects_intersect(a: &RECT, b: &RECT) -> bool {
    a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top
}

/// System shells, transient menus, IME popups, and taskbars that should legitimately sit
/// above user floating widgets without fighting for Z-order.
pub fn is_system_overlay_class(cls: &str) -> bool {
    matches!(
        cls,
        "#32768"
            | "tooltips_class32"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "Shell_InputSwitchTopLevelWindow"
            | "NotifyIconOverflowWindow"
            | "TaskListThumbnailWnd"
            | "TopLevelWindowForOverflowXamlIsland"
            | "Windows.UI.Core.CoreWindow"
            | "XamlExplorerHostIslandWindow"
            | "ForegroundStaging"
            | "MultitaskingViewFrame"
            | "TaskSwitcherWnd"
            | "IME"
            | "MSCTFIME UI"
            | "CicMarshalWndClass"
            | "EdgeUiInputTopWndClass"
            | "SysShadow"
    )
}

/// Pure classifier used by unit tests (mirrors live zero-alloc `inspect_pill_topmost_health`).
#[cfg(test)]
pub fn classify_topmost_health(
    has_ex_topmost: bool,
    pill_rect: RECT,
    above_visible: &[WindowAboveDesc],
) -> TopmostHealth {
    if !has_ex_topmost {
        return TopmostHealth::NeedHardReset(0);
    }
    let mut overlapped_by_non_system_topmost: Option<isize> = None;
    for w in above_visible {
        if w.rect.right <= w.rect.left || w.rect.bottom <= w.rect.top {
            continue;
        }
        if !w.is_topmost {
            // Any real visible non-topmost window above our topmost pill indicates Windows
            // fullscreen demotion / Win+D / Z-order desync ("ghost topmost").
            if !is_system_overlay_class(&w.class_name) {
                return TopmostHealth::NeedHardReset(w.hwnd_raw);
            }
            continue;
        }
        if !is_system_overlay_class(&w.class_name) && rects_intersect(&pill_rect, &w.rect) {
            overlapped_by_non_system_topmost.get_or_insert(w.hwnd_raw);
        }
    }
    if let Some(blocker) = overlapped_by_non_system_topmost {
        TopmostHealth::NeedRaise(blocker)
    } else {
        TopmostHealth::Healthy
    }
}

pub fn window_has_topmost_style(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        (ex & WS_EX_TOPMOST.0) != 0
    }
}

fn is_window_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
    }
}

fn is_system_overlay_class_u16(units: &[u16]) -> bool {
    if units.is_empty() || units.len() > 64 {
        return false;
    }
    let mut ascii = [0u8; 64];
    for (i, &u) in units.iter().enumerate() {
        if u > 0x7F {
            return false;
        }
        ascii[i] = u as u8;
    }
    // Every byte is <= 0x7F, so valid UTF-8 is guaranteed.
    let s = unsafe { std::str::from_utf8_unchecked(&ascii[..units.len()]) };
    is_system_overlay_class(s)
}

fn is_hwnd_system_overlay_noalloc(hwnd: HWND) -> bool {
    let mut buf = [0u16; 96];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len <= 0 {
        return false;
    }
    is_system_overlay_class_u16(&buf[..len as usize])
}

/// Inspects live Win32 Z-order above `pill_hwnd` with zero heap allocations and minimal DWM IPC
/// to detect lost `WS_EX_TOPMOST`, ghost topmost desync, or overlapping non-system topmost windows.
pub fn inspect_pill_topmost_health(pill_hwnd: HWND) -> TopmostHealth {
    if pill_hwnd.0.is_null() {
        return TopmostHealth::Healthy;
    }
    unsafe {
        if !IsWindowVisible(pill_hwnd).as_bool() {
            return TopmostHealth::Healthy;
        }
        let has_top = window_has_topmost_style(pill_hwnd);
        if !has_top {
            return TopmostHealth::NeedHardReset(0);
        }
        let mut pill_rc = RECT::default();
        if GetWindowRect(pill_hwnd, &mut pill_rc).is_err() {
            return TopmostHealth::Healthy;
        }
        let mut overlapped_by_non_system_topmost: Option<isize> = None;
        let mut cur = GetWindow(pill_hwnd, GW_HWNDPREV).unwrap_or_default();
        let mut steps = 0usize;
        while !cur.0.is_null() && steps < 96 {
            steps += 1;
            if IsWindowVisible(cur).as_bool() && !IsIconic(cur).as_bool() {
                let mut rc = RECT::default();
                if GetWindowRect(cur, &mut rc).is_ok() && rc.right > rc.left && rc.bottom > rc.top {
                    let is_topmost = window_has_topmost_style(cur);
                    if !is_topmost {
                        // Cheap class check BEFORE DWM IPC (`is_window_cloaked`): skips system
                        // overlays immediately without round-tripping to dwm.exe.
                        if !is_hwnd_system_overlay_noalloc(cur) && !is_window_cloaked(cur) {
                            return TopmostHealth::NeedHardReset(cur.0 as isize);
                        }
                    } else if overlapped_by_non_system_topmost.is_none()
                        && rects_intersect(&pill_rc, &rc)
                        && !is_hwnd_system_overlay_noalloc(cur)
                        && !is_window_cloaked(cur)
                    {
                        overlapped_by_non_system_topmost = Some(cur.0 as isize);
                    }
                }
            }
            cur = GetWindow(cur, GW_HWNDPREV).unwrap_or_default();
        }
        if let Some(blocker) = overlapped_by_non_system_topmost {
            TopmostHealth::NeedRaise(blocker)
        } else {
            TopmostHealth::Healthy
        }
    }
}

/// Restores topmost Z-order for `pill_hwnd`. When `hard_reset` is true, performs a silent
/// `HWND_NOTOPMOST -> HWND_TOPMOST` cycle to break Windows ghost-topmost / fullscreen demotion.
pub fn apply_pill_topmost(pill_hwnd: HWND, topmost: bool, hard_reset: bool) {
    if pill_hwnd.0.is_null() {
        return;
    }
    unsafe {
        if topmost {
            if hard_reset {
                let _ = SetWindowPos(
                    pill_hwnd,
                    HWND_NOTOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOSENDCHANGING,
                );
            }
            let _ = SetWindowPos(
                pill_hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOSENDCHANGING,
            );
        } else {
            let _ = SetWindowPos(
                pill_hwnd,
                HWND_NOTOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOSENDCHANGING,
            );
        }
    }
}

/// Guards `WM_WINDOWPOSCHANGING` for a topmost pill: intercepts attempts to demote the pill
/// below any non-topmost window (`HWND_BOTTOM`, `HWND_NOTOPMOST`, `HWND_TOP`, or a non-topmost
/// HWND), while allowing legitimate topmost overlays (context menus, taskbar) to stay above.
pub fn guard_topmost_windowpos(wp: &mut WINDOWPOS, topmost_enabled: bool) -> bool {
    if !topmost_enabled || (wp.flags.0 & SWP_NOZORDER.0) != 0 {
        return false;
    }
    let after = wp.hwndInsertAfter;
    if after == HWND_TOPMOST {
        return false;
    }
    let is_demotion = after == HWND_BOTTOM
        || after == HWND_NOTOPMOST
        || after == HWND_TOP
        || (unsafe { IsWindow(after).as_bool() } && !window_has_topmost_style(after));
    if is_demotion {
        wp.hwndInsertAfter = HWND_TOPMOST;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dual_monitor_clamp_never_enters_primary_taskbar() {
        let mons = vec![
            MonitorDesc {
                label: "Primary".into(),
                is_primary: true,
                monitor: RECT { left: 0, top: 0, right: 2560, bottom: 1440 },
                work: RECT { left: 0, top: 0, right: 2560, bottom: 1380 },
            },
            MonitorDesc {
                label: "Secondary".into(),
                is_primary: false,
                monitor: RECT { left: 2560, top: -97, right: 5440, bottom: 1703 },
                work: RECT { left: 2560, top: -97, right: 5440, bottom: 1607 },
            },
        ];
        let (w, h, shadow) = (340, 63, 13);
        // Both live drag (clamp_x=false) and settled clamp (clamp_x=true) must stop strictly above primary work.bottom (1380)
        for clamp_x in [false, true] {
            let (cx, cy) = clamp_pos_in_list(&mons, 2225, 1385, w, h, shadow, clamp_x);
            assert!(cy + h - shadow <= 1380, "visible bottom {} entered primary taskbar (clamp_x={})!", cy + h - shadow, clamp_x);
            assert!(cx + w - shadow <= 2560);
        }

        // Dragging horizontally across to secondary monitor uses secondary work.bottom (1607)
        let (cx2, cy2) = clamp_pos_in_list(&mons, 3000, 1650, w, h, shadow, false);
        assert_eq!(cx2, 3000);
        assert!(cy2 + h - shadow <= 1607, "visible bottom {} entered secondary taskbar!", cy2 + h - shadow);

        // Vertically stacked monitors allow dragging upward across the seam, while still blocking bottom taskbar
        let stacked = vec![
            MonitorDesc {
                label: "Top".into(),
                is_primary: false,
                monitor: RECT { left: 0, top: -1080, right: 1920, bottom: 0 },
                work: RECT { left: 0, top: -1080, right: 1920, bottom: -48 },
            },
            MonitorDesc {
                label: "Bottom".into(),
                is_primary: true,
                monitor: RECT { left: 0, top: 0, right: 1920, bottom: 1080 },
                work: RECT { left: 0, top: 0, right: 1920, bottom: 1032 },
            },
        ];
        let (_, sy_up) = clamp_pos_in_list(&stacked, 500, -100, w, h, shadow, false);
        assert_eq!(sy_up, -100);
        let (_, sy_down) = clamp_pos_in_list(&stacked, 500, 1050, w, h, shadow, false);
        assert!(sy_down + h - shadow <= 1032);
    }

    #[test]
    fn test_pill_items_legacy_migration() {
        assert_eq!(migrate_legacy_pill_items(false, 0), PILL_ITEMS_DEFAULT);
        assert_eq!(migrate_legacy_pill_items(true, 0), PILL_ITEM_TOTAL);
        assert_eq!(migrate_legacy_pill_items(true, 1), PILL_ITEM_NON_CACHED);
        assert_eq!(
            migrate_legacy_pill_items(false, 2),
            PILL_ITEM_ACTIVE_CTX | PILL_ITEM_CPU | PILL_ITEM_RAM
        );
    }

    #[test]
    fn test_zorder_health_classification() {
        let pill = RECT { left: 1700, top: 1300, right: 1900, bottom: 1340 };

        // 1. Healthy when nothing sits above, or only system overlays / non-overlapping topmost windows
        let ok_above = vec![
            WindowAboveDesc {
                hwnd_raw: 1,
                class_name: "Shell_TrayWnd".into(),
                is_topmost: true,
                rect: RECT { left: 0, top: 1300, right: 1920, bottom: 1440 },
            },
            WindowAboveDesc {
                hwnd_raw: 2,
                class_name: "#32768".into(),
                is_topmost: true,
                rect: pill,
            },
            WindowAboveDesc {
                hwnd_raw: 3,
                class_name: "SomeOtherWidget".into(),
                is_topmost: true,
                rect: RECT { left: 10, top: 10, right: 200, bottom: 60 },
            },
        ];
        assert_eq!(classify_topmost_health(true, pill, &ok_above), TopmostHealth::Healthy);

        // 2. Missing WS_EX_TOPMOST -> NeedHardReset
        assert_eq!(classify_topmost_health(false, pill, &[]), TopmostHealth::NeedHardReset(0));

        // 3. Ghost topmost (WS_EX_TOPMOST still set, but a regular non-topmost app sits above) -> NeedHardReset
        let ghost = vec![WindowAboveDesc {
            hwnd_raw: 42,
            class_name: "Chrome_WidgetWin_1".into(),
            is_topmost: false,
            rect: RECT { left: 0, top: 0, right: 1920, bottom: 1380 },
        }];
        assert_eq!(classify_topmost_health(true, pill, &ghost), TopmostHealth::NeedHardReset(42));

        // 4. Overlapping non-system topmost window -> NeedRaise
        let overlap_top = vec![WindowAboveDesc {
            hwnd_raw: 99,
            class_name: "OtherTopmostApp".into(),
            is_topmost: true,
            rect: RECT { left: 1600, top: 1200, right: 1920, bottom: 1380 },
        }];
        assert_eq!(classify_topmost_health(true, pill, &overlap_top), TopmostHealth::NeedRaise(99));
    }

    #[test]
    fn test_win32_pill_topmost_recovery_and_guard() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, ShowWindow, SW_SHOWNOACTIVATE, WS_EX_LAYERED,
            WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
        };

        unsafe {
            let hinst = GetModuleHandleW(None).unwrap_or_default();
            let pill = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                w!("STATIC"),
                w!("TestPill"),
                WS_POPUP,
                100,
                100,
                160,
                40,
                None,
                None,
                hinst,
                None,
            )
            .unwrap();
            let regular = CreateWindowExW(
                WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("TestRegular"),
                WS_POPUP,
                80,
                80,
                260,
                160,
                None,
                None,
                hinst,
                None,
            )
            .unwrap();
            let other_top = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                w!("STATIC"),
                w!("TestOtherTop"),
                WS_POPUP,
                90,
                90,
                200,
                100,
                None,
                None,
                hinst,
                None,
            )
            .unwrap();

            let _ = ShowWindow(regular, SW_SHOWNOACTIVATE);
            let _ = ShowWindow(pill, SW_SHOWNOACTIVATE);
            apply_pill_topmost(pill, true, false);
            assert!(window_has_topmost_style(pill));

            // 1) Simulate demotion (e.g. fullscreen / Win+D stripping topmost and placing regular app above)
            apply_pill_topmost(pill, false, false);
            let _ = SetWindowPos(regular, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            assert!(matches!(inspect_pill_topmost_health(pill), TopmostHealth::NeedHardReset(_)));

            // Hard-reset recovery brings pill back above regular window
            apply_pill_topmost(pill, true, true);
            assert!(window_has_topmost_style(pill));
            assert_eq!(inspect_pill_topmost_health(pill), TopmostHealth::Healthy);

            // 2) Simulate overlapping peer topmost window covering pill
            let _ = ShowWindow(other_top, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(other_top, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            assert_eq!(inspect_pill_topmost_health(pill), TopmostHealth::NeedRaise(other_top.0 as isize));
            apply_pill_topmost(pill, true, false);
            assert_eq!(inspect_pill_topmost_health(pill), TopmostHealth::Healthy);

            // 3) Guard intercepts demotion below regular window or HWND_BOTTOM, allows topmost peer
            let mut wp_demote = WINDOWPOS {
                hwnd: pill,
                hwndInsertAfter: regular,
                ..Default::default()
            };
            assert!(guard_topmost_windowpos(&mut wp_demote, true));
            assert_eq!(wp_demote.hwndInsertAfter, HWND_TOPMOST);

            let mut wp_bottom = WINDOWPOS {
                hwnd: pill,
                hwndInsertAfter: HWND_BOTTOM,
                ..Default::default()
            };
            assert!(guard_topmost_windowpos(&mut wp_bottom, true));
            assert_eq!(wp_bottom.hwndInsertAfter, HWND_TOPMOST);

            let mut wp_peer = WINDOWPOS {
                hwnd: pill,
                hwndInsertAfter: other_top,
                ..Default::default()
            };
            assert!(!guard_topmost_windowpos(&mut wp_peer, true));
            assert_eq!(wp_peer.hwndInsertAfter, other_top);

            let _ = DestroyWindow(other_top);
            let _ = DestroyWindow(regular);
            let _ = DestroyWindow(pill);
        }
    }

    #[test]
    fn test_tray_flyout_positioning_and_hover_zone() {
        let primary = MonitorDesc {
            label: "Primary".into(),
            is_primary: true,
            monitor: RECT { left: 0, top: 0, right: 2560, bottom: 1440 },
            work: RECT { left: 0, top: 0, right: 2560, bottom: 1392 },
        };
        let mons = vec![primary];

        // 1) Bottom taskbar, icon with room on right -> docks 6px above taskbar, anchored from icon.left rightward
        let icon_mid = RECT { left: 2280, top: 1392, right: 2312, bottom: 1440 };
        let pos = compute_tray_flyout_pos_in_list(&mons, icon_mid, 240, 50, 10, 1.0);
        // vis_w = 220, vis_h = 30; vis_x = 2280 -> win_x = 2270 (vis_right = 2500, 60px from right edge)
        // vis_y = 1392 - 6 - 30 = 1356 -> win_y = 1346 (vis_bottom = 1386)
        assert_eq!(pos, (2270, 1346));

        // 2) Bottom taskbar near right side -> clamped with 18px breathing margin inside right screen edge
        let icon_right = RECT { left: 2420, top: 1392, right: 2452, bottom: 1440 };
        let pos_r = compute_tray_flyout_pos_in_list(&mons, icon_right, 240, 50, 10, 1.0);
        let vis_right = pos_r.0 + 240 - 10;
        assert_eq!(vis_right, 2542); // 2560 - 18
        assert_eq!(pos_r.1 + 50 - 10, 1386); // 6px above taskbar

        // 3) Icon inside Windows 11 overflow flyout (above taskbar) -> docks right above the overflow icon
        let icon_overflow = RECT { left: 2300, top: 1280, right: 2332, bottom: 1312 };
        let pos_ov = compute_tray_flyout_pos_in_list(&mons, icon_overflow, 240, 50, 10, 1.0);
        assert_eq!(pos_ov.1 + 50 - 10, 1274); // 1280 - 6

        // 4) Hover zone includes icon, bridge corridor (including up-right path), and visible capsule, excludes adjacent taskbar icons
        assert!(is_cursor_in_tray_hover_zone((2296, 1416), icon_mid, pos, 240, 50, 10)); // on icon
        assert!(is_cursor_in_tray_hover_zone((2296, 1389), icon_mid, pos, 240, 50, 10)); // 6px bridge
        assert!(is_cursor_in_tray_hover_zone((2330, 1389), icon_mid, pos, 240, 50, 10)); // up-right bridge
        assert!(is_cursor_in_tray_hover_zone((2380, 1370), icon_mid, pos, 240, 50, 10)); // on pill
        assert!(!is_cursor_in_tray_hover_zone((2400, 1416), icon_mid, pos, 240, 50, 10)); // adjacent tray icon
        assert!(!is_cursor_in_tray_hover_zone((2296, 1200), icon_mid, pos, 240, 50, 10)); // far above pill

        assert_eq!(
            normalize_exe_path_for_compare(r#"\\?\C:/Users/Qingz/gauge.exe"#),
            r"c:\users\qingz\gauge.exe"
        );
        assert_eq!(desired_tray_promoted_write(None, true), Some(1));
        assert_eq!(desired_tray_promoted_write(Some(0), true), Some(1));
        assert_eq!(desired_tray_promoted_write(Some(1), true), None);
        assert_eq!(desired_tray_promoted_write(None, false), None);
        assert_eq!(desired_tray_promoted_write(Some(0), false), None);
        assert_eq!(desired_tray_promoted_write(Some(1), false), Some(0));

        // Verify reg_read_key_string actually succeeds against a real Win32 HKCU REG_SZ value
        // (guards against the RegGetValueW RRF_RT_REG_EXPAND_SZ without RRF_NOEXPAND pitfall).
        unsafe {
            let mut hkey = HKEY::default();
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Shell Folders"),
                0,
                KEY_READ,
                &mut hkey,
            )
            .is_ok()
            {
                let desktop = reg_read_key_string(hkey, w!("Desktop"));
                let _ = RegCloseKey(hkey);
                assert!(
                    desktop.as_deref().map(|s| !s.is_empty()).unwrap_or(false),
                    "reg_read_key_string failed to read HKCU Shell Folders\\Desktop"
                );
            }
        }
    }

    #[test]
    fn test_tray_icon_update_icon_preserves_state_and_short_circuits() {
        let icon_a = HICON(1 as *mut core::ffi::c_void);
        let icon_b = HICON(2 as *mut core::ffi::c_void);
        let mut tray = TrayIcon::new(HWND::default(), icon_a, "init");
        assert!(!tray.active);
        assert_eq!(tray.icon, icon_a);

        // Same icon short-circuits without mutating state
        tray.update_icon(icon_a);
        assert_eq!(tray.icon, icon_a);

        // Null icon is ignored so tray never blanks out
        tray.update_icon(HICON::default());
        assert_eq!(tray.icon, icon_a);

        // New non-null icon updates stored handle (preserved across TaskbarCreated / remove+add)
        tray.update_icon(icon_b);
        assert_eq!(tray.icon, icon_b);
    }

    #[test]
    fn test_system_telemetry_exact_max_samples_and_noalloc_overlay_class() {
        let mut tel = SystemTelemetry::new(5);
        assert_eq!(tel.latest.self_mb, 0.0);
        // Push synthetic history samples directly through sample() cap logic
        let sample = tel.latest;
        for _ in 0..12 {
            while tel.history.len() >= tel.max_samples {
                tel.history.pop_front();
            }
            tel.history.push_back(sample);
        }
        assert_eq!(tel.history.len(), 5);
        assert!(tel.refresh_self_private_mb() > 0.0);
        assert!(tel.latest.self_mb > 0.0);

        // UTF-16 no-alloc system overlay classifier parity
        let tray_u16: Vec<u16> = "Shell_TrayWnd".encode_utf16().collect();
        let chrome_u16: Vec<u16> = "Chrome_WidgetWin_1".encode_utf16().collect();
        let cjk_u16: Vec<u16> = "窗口类名".encode_utf16().collect();
        assert!(is_system_overlay_class_u16(&tray_u16));
        assert!(!is_system_overlay_class_u16(&chrome_u16));
        assert!(!is_system_overlay_class_u16(&cjk_u16));
        assert!(!is_system_overlay_class_u16(&[]));

        // Console display power setting parser
        let pbs = PowerBroadcastSettingHeader {
            power_setting: GUID_CONSOLE_DISPLAY_STATE_RAW,
            data_length: 4,
            data: 0u32.to_le_bytes(),
        };
        assert_eq!(
            parse_console_display_state(LPARAM((&pbs as *const PowerBroadcastSettingHeader) as isize)),
            Some(0)
        );
        assert_eq!(parse_console_display_state(LPARAM(0)), None);
    }

    #[test]
    fn test_dir_watcher_starts_and_stops_cleanly() {
        let dir = std::env::temp_dir().join(format!("gauge_dir_watch_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Dummy message-only handle (invalid HWND is fine as long as stop happens cleanly before any file write)
        let dummy_hwnd = HWND(1 as *mut core::ffi::c_void);
        let watcher = DirWatcher::start(vec![(dir.clone(), false)], dummy_hwnd, WM_APP + 99);
        assert!(watcher.is_some());
        let watcher = watcher.unwrap();
        assert!(watcher.is_healthy());
        assert!(watcher.drain_hints().is_empty());
        drop(watcher);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_watcher_hints_coalescing_dedup_and_overflow() {
        let root = PathBuf::from(r"C:\fake\brain");
        let comp = PathBuf::from(r"C:\fake\exe");
        let mut h = WatcherHints::default();
        assert!(h.is_empty());

        // First relevant path transitions empty -> non-empty (signals PostMessageW once).
        let p1: Vec<u16> = r"abc\.system_generated\logs\transcript.jsonl".encode_utf16().collect();
        assert!(h.record_rel_u16(&root, &p1, true));
        assert!(!h.is_empty());
        assert_eq!(h.paths.len(), 1);
        assert_eq!(h.paths[0], root.join(r"abc\.system_generated\logs\transcript.jsonl"));

        // Duplicate path does NOT trigger a second wakeup or duplicate entry.
        assert!(!h.record_rel_u16(&root, &p1, true));
        assert_eq!(h.paths.len(), 1);

        // Second distinct path coalesces into the same wakeup window.
        let p2: Vec<u16> = r"def\conversation_metadata.json".encode_utf16().collect();
        assert!(!h.record_rel_u16(&root, &p2, true));
        assert_eq!(h.paths.len(), 2);

        // Companion file sets has_companion_change without polluting transcript paths.
        let p_comp: Vec<u16> = "statusline_state.json".encode_utf16().collect();
        assert!(!h.record_rel_u16(&comp, &p_comp, false));
        assert!(h.has_companion_change);
        assert_eq!(h.paths.len(), 2);

        // Draining resets to empty; next companion-only change triggers exactly one wakeup.
        let drained = std::mem::take(&mut h);
        assert!(h.is_empty());
        assert_eq!(drained.paths.len(), 2);
        assert!(drained.has_companion_change);

        assert!(h.record_rel_u16(&comp, &p_comp, false));
        assert!(!h.record_rel_u16(&comp, &p_comp, false));
        assert!(h.has_companion_change && h.paths.is_empty());

        // Exceeding WATCHER_HINT_CAP transitions cleanly to overflow=true and clears paths.
        h = WatcherHints::default();
        for i in 0..=WATCHER_HINT_CAP {
            let rel: Vec<u16> = format!("s{i}\\transcript.jsonl").encode_utf16().collect();
            let post = h.record_rel_u16(&root, &rel, true);
            assert_eq!(post, i == 0);
        }
        assert!(h.overflow);
        assert!(h.paths.is_empty());

        // Verify raw FILE_NOTIFY_INFORMATION multi-entry parsing filters noise and collects all relevant paths.
        fn push_fni(raw: &mut Vec<u8>, name: &str, has_next: bool) {
            let start = raw.len();
            let u16s: Vec<u16> = name.encode_utf16().collect();
            let name_bytes = (u16s.len() * 2) as u32;
            let entry_len = (12 + name_bytes as usize + 3) & !3;
            let next_off = if has_next { entry_len as u32 } else { 0u32 };
            raw.extend_from_slice(&next_off.to_le_bytes());
            raw.extend_from_slice(&3u32.to_le_bytes()); // FILE_ACTION_MODIFIED
            raw.extend_from_slice(&name_bytes.to_le_bytes());
            for u in u16s {
                raw.extend_from_slice(&u.to_le_bytes());
            }
            while raw.len() - start < entry_len {
                raw.push(0);
            }
        }
        let mut raw = Vec::new();
        push_fni(&mut raw, r"abc\.system_generated\steps\1\output.txt", true);
        push_fni(&mut raw, r"abc\.system_generated\logs\transcript.jsonl", true);
        push_fni(&mut raw, r"abc\scratch\tmp.json", true);
        push_fni(&mut raw, r"def\.system_generated\logs\transcript.jsonl", false);
        let mut words = vec![0u32; raw.len().div_ceil(4)];
        unsafe {
            std::ptr::copy_nonoverlapping(raw.as_ptr(), words.as_mut_ptr() as *mut u8, raw.len());
        }
        let q = Mutex::new(WatcherHints::default());
        assert!(collect_notify_buffer_into_queue(&words, raw.len(), &root, true, &q));
        // Second batch while queue is still non-empty must NOT request another PostMessageW.
        assert!(!collect_notify_buffer_into_queue(&words, raw.len(), &root, true, &q));
        let collected = std::mem::take(&mut *q.lock().unwrap());
        assert!(!collected.overflow);
        assert_eq!(collected.paths.len(), 2);
        assert_eq!(collected.paths[0], root.join(r"abc\.system_generated\logs\transcript.jsonl"));
        assert_eq!(collected.paths[1], root.join(r"def\.system_generated\logs\transcript.jsonl"));
    }

    #[test]
    fn test_watcher_rel_path_filter_whitelist_and_rejects() {
        let matches = |p: &str, subtree: bool| {
            let u16s: Vec<u16> = p.encode_utf16().collect();
            is_watched_rel_path_relevant_u16(&u16s, subtree)
        };

        // Agent log subtree (subtree = true): whitelisted transcripts & metadata match
        assert!(matches(r"abc-123\.system_generated\logs\transcript.jsonl", true));
        assert!(matches(r"abc-123\.system_generated\logs\transcript_full.jsonl", true));
        assert!(matches(r"abc-123.pbtxt", true));
        assert!(matches(r"conversation_metadata.json", true));
        assert!(matches(r"proj\sessions-index.json", true));
        assert!(matches(r"session_index.jsonl", true));
        assert!(matches(r"2026\11\27\rollout-123.jsonl", true));
        assert!(matches(r"proj\chats\session-01.json", true));
        assert!(matches(r"proj\.project_root", true));
        assert!(matches(r"项目中文\subagents\agent-01.jsonl", true));

        // Agent log subtree: noisy steps, scratch, tasks, temp, media, markdown, archive & sqlite MUST be rejected
        assert!(!matches(r"abc-123\.system_generated\steps\12\output.txt", true));
        assert!(!matches(r"abc-123\.system_generated\steps\12\step.json", true));
        assert!(!matches(r"abc-123\.system_generated\tasks\task-42.log", true));
        assert!(!matches(r"abc-123\scratch\foo.py", true));
        assert!(!matches(r"abc-123\scratch\dump.json", true));
        assert!(!matches(r"abc-123\implementation_plan.md", true));
        assert!(!matches(r"abc-123\screenshot.png", true));
        assert!(!matches(r"abc-123\transcript.jsonl.tmp", true));
        assert!(!matches(r"conversations\state.sqlite-wal", true));
        assert!(!matches(r"state.db-shm", true));
        assert!(!matches(r"archive.json", true));
        assert!(!matches(r"history.jsonl", true));
        assert!(!matches(r"settings.json", true));
        assert!(!matches(r"oauth_creds.json", true));

        // Companion/exe dir (subtree = false): ONLY companion configs match; archive.json & *.tmp rejected
        assert!(matches("statusline_state.json", false));
        assert!(matches("pricing.json", false));
        assert!(matches("budget.json", false));
        assert!(matches("session_meta.json", false));
        assert!(!matches("archive.json", false));
        assert!(!matches("archive.json.tmp", false));
        assert!(!matches("statusline_state.json.tmp", false));
        assert!(!matches("gauge.exe", false));
        assert!(!matches("terminal.json", false));
    }

    #[test]
    fn test_user_prefs_terminal_defaults() {
        let p = UserPrefs::default();
        assert_eq!(p.terminal_preset, crate::launcher::TerminalPreset::WtPwsh.to_u8());
        assert!(!p.terminal_new_window);
        assert!(p.terminal_keep_open);
    }
}
