//! Zero-subprocess Win32 system telemetry, multi-monitor geometry, DWM & clipboard helpers.

use std::collections::VecDeque;
use std::mem::{size_of, zeroed};
use std::ptr;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::{PCWSTR, PWSTR, w};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, FILETIME, GetLastError, HANDLE, HGLOBAL, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_CLOAKED,
    DWMWA_TRANSITIONS_FORCEDISABLED, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, HDC, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::ProcessStatus::{
    K32EmptyWorkingSet, K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_ENUMERATE_SUB_KEYS, KEY_READ, KEY_SET_VALUE,
    KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_EXPAND_SZ,
    RRF_RT_REG_SZ,
};
use windows::Win32::System::SystemInformation::{GetSystemInfo, GlobalMemoryStatusEx, MEMORYSTATUSEX, SYSTEM_INFO};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, GetSystemTimes};
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
    FindWindowW, GetClassNameW, GetWindow, GetWindowLongPtrW, GetWindowRect, IsIconic, IsWindow,
    IsWindowVisible, PostMessageW, RegisterWindowMessageW, SetWindowPos, SystemParametersInfoW,
    GWL_EXSTYLE, GW_HWNDPREV, HICON, HWND_BOTTOM, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST,
    SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSENDCHANGING, SWP_NOSIZE, SWP_NOZORDER,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WINDOWPOS, WM_APP, WS_EX_TOPMOST,
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
pub const PILL_ITEM_BLOCK_5H: u32 = 1 << 7;
pub const PILL_ITEM_QUOTA: u32 = 1 << 8;
pub const PILL_ITEMS_ALL_MASK: u32 = PILL_ITEM_TOTAL
    | PILL_ITEM_NON_CACHED
    | PILL_ITEM_CACHE_HIT
    | PILL_ITEM_ACTIVE_CTX
    | PILL_ITEM_CPU
    | PILL_ITEM_RAM
    | PILL_ITEM_COST
    | PILL_ITEM_BLOCK_5H
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
        }
    }
}

const REG_SUBKEY: PCWSTR = w!("Software\\AgyTokenMonitor");

fn reg_read_u32(name: PCWSTR) -> Option<u32> {
    unsafe {
        let mut val: u32 = 0;
        let mut sz: u32 = size_of::<u32>() as u32;
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            REG_SUBKEY,
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

pub fn load_user_prefs() -> UserPrefs {
    let mut p = UserPrefs::default();
    if let Some(v) = reg_read_u32(w!("light_theme")) {
        p.light_theme = v != 0;
    }
    if let Some(v) = reg_read_u32(w!("mini")) {
        p.mini = v != 0;
    }
    if let Some(v) = reg_read_u32(w!("topmost")) {
        p.topmost = v != 0;
    }
    if let Some(v) = reg_read_u32(w!("human")) {
        p.human = v != 0;
    }
    if let Some(v) = reg_read_u32(w!("cost_mode")) {
        p.cost_mode = v != 0;
    }
    if let Some(v) = reg_read_u32(w!("group_subagents")) {
        p.group_subagents = v != 0;
    }
    if let Some(v) = reg_read_u32(w!("privacy_mode")) {
        p.privacy_mode = v != 0;
    }
    if let Some(v) = reg_read_u32(w!("pill_visible")) {
        p.pill_visible = v != 0;
    }
    if let Some(v) = reg_read_u32(w!("tray_hover_mode")) {
        p.tray_hover_mode = v != 0;
    }
    if let Some(v) = reg_read_u32(w!("notifications_enabled")) {
        p.notifications_enabled = v != 0;
    }
    if let Some(v) = reg_read_u32(w!("hide_on_fullscreen")) {
        p.hide_on_fullscreen = v != 0;
    }
    if let Some(v) = reg_read_u32(w!("monitor_index")) {
        p.monitor_index = v as usize;
    }
    if let Some(v) = reg_read_u32(w!("fresh_only")) {
        p.fresh_only = v != 0;
    }
    if let Some(v) = reg_read_u32(w!("date_scope")) {
        p.date_scope = v.min(4) as u8;
    }
    if let Some(v) = reg_read_u32(w!("pill_metric")) {
        p.pill_metric = v.min(2) as u8;
    }
    if let Some(v) = reg_read_u32(w!("pill_items")) {
        let masked = v & PILL_ITEMS_ALL_MASK;
        p.pill_items = if masked != 0 { masked } else { PILL_ITEMS_DEFAULT };
    } else {
        p.pill_items = migrate_legacy_pill_items(p.mini, p.pill_metric);
    }
    p.mini = (p.pill_items & (PILL_ITEM_CPU | PILL_ITEM_RAM)) == 0;
    if reg_read_u32(w!("has_custom_pos")).unwrap_or(0) != 0 {
        if let (Some(x), Some(y)) = (reg_read_u32(w!("pos_x")), reg_read_u32(w!("pos_y"))) {
            p.custom_pos = Some((x as i32, y as i32));
        }
    }
    p
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
        let entries: [(PCWSTR, u32); 19] = [
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
    let mons = get_monitors();
    for (idx, m) in mons.iter().enumerate() {
        if cx >= m.monitor.left && cx <= m.monitor.right && cy >= m.monitor.top && cy <= m.monitor.bottom {
            return Some(idx);
        }
    }
    None
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
    let mons = get_monitors();
    clamp_pos_in_list(&mons, x, y, w, h, shadow_px, false)
}

/// Clamp a settled/saved pill position strictly inside its target monitor's work area.
pub fn clamp_pos_to_work_area(x: i32, y: i32, w: i32, h: i32, shadow_px: i32) -> (i32, i32) {
    let mons = get_monitors();
    clamp_pos_in_list(&mons, x, y, w, h, shadow_px, true)
}

/// Computes the layered window `(win_x, win_y)` for the tray-hover capsule flyout so that the
/// visible pill hugs snugly right above (or adjacent to) the taskbar/tray icon without clipping.
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
    let gap = (4.0 * scale).round() as i32;
    let margin = (6.0 * scale).round() as i32;
    let icx = (icon.left + icon.right) / 2;
    let icy = (icon.top + icon.bottom) / 2;

    if mons.is_empty() {
        return (icx - vis_w / 2 - shadow_px, icon.top - gap - vis_h - shadow_px);
    }

    let cur = &mons[nearest_monitor_idx(mons, icx, icy)];
    let mon = cur.monitor;
    let work = cur.work;

    let clamp_x = |lo: i32, hi_edge: i32| -> i32 {
        let min_x = lo + margin;
        let max_x = (hi_edge - margin - vis_w).max(min_x);
        (icx - vis_w / 2).clamp(min_x, max_x)
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
    let mons = get_monitors();
    compute_tray_flyout_pos_in_list(&mons, icon, px_w, px_h, shadow_px, scale)
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
        let br = icon.right + 14;
        if in_rect(bl, vb - 4, br, icon.top + 4) {
            return true;
        }
    } else if vt >= icon.bottom - 8 {
        // Capsule is below icon (top taskbar)
        let bl = icon.left - 14;
        let br = icon.right + 14;
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
        for cls in [w!("AgyTokenPillClass"), w!("AgyTokenDashboardClass")] {
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
            let handle = CreateMutexW(None, false, w!("Local\\AgyTokenMonitorRustV1")).ok()?;
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
    pub history: VecDeque<TelemetrySample>,
    pub latest: TelemetrySample,
}

impl SystemTelemetry {
    pub fn new(cap: usize) -> Self {
        let cpu_cores = unsafe {
            let mut si: SYSTEM_INFO = zeroed();
            GetSystemInfo(&mut si);
            si.dwNumberOfProcessors.max(1)
        };
        let prev_times = read_sys_times();
        let (mem_pct, mem_used_gb, mem_total_gb, mem_avail_gb) = read_memory();
        let self_mb = read_self_private_mb();
        let initial = TelemetrySample {
            timestamp: now_secs(),
            cpu_pct: 0.0,
            mem_pct,
            mem_used_gb,
            mem_total_gb,
            mem_avail_gb,
            self_mb,
            cpu_cores,
        };
        Self {
            prev_times,
            cpu_cores,
            history: VecDeque::with_capacity(cap),
            latest: initial,
        }
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
        let self_mb = read_self_private_mb();

        let sample = TelemetrySample {
            timestamp: now_secs(),
            cpu_pct,
            mem_pct,
            mem_used_gb,
            mem_total_gb,
            mem_avail_gb,
            self_mb,
            cpu_cores: self.cpu_cores,
        };
        self.latest = sample;
        if has_cpu_delta {
            if self.history.len() == self.history.capacity() {
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

/// Politely return idle dashboard pages (WARP / font cache) of THIS process only to Windows Standby RAM.
pub fn trim_self_working_set() {
    unsafe {
        let _ = K32EmptyWorkingSet(GetCurrentProcess());
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

pub fn get_monitors() -> Vec<MonitorDesc> {
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

pub fn get_work_area(monitor_index: usize) -> RECT {
    let mons = get_monitors();
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
            let _ = CloseClipboard();
            return false;
        }
        ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
        let _ = GlobalUnlock(hmem);
        let ok = SetClipboardData(CF_UNICODETEXT, HANDLE(hmem.0)).is_ok();
        let _ = CloseClipboard();
        let _ = HGLOBAL(hmem.0);
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
const AUTOSTART_NAME: PCWSTR = w!("AgyGauge");

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
                RegSetValueExW(hkey, AUTOSTART_NAME, 0, REG_SZ, Some(bytes)).is_ok()
            } else {
                false
            }
        } else {
            RegDeleteValueW(hkey, AUTOSTART_NAME).is_ok()
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
pub enum TrayPromoteResult {
    NotFound,
    AlreadyPromoted,
    PromotedNow,
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
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
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

/// Promotes (pins) this executable's notification icon directly onto the visible Windows 11
/// taskbar (beside the IME indicator and clock, outside the `^` overflow chevron) by setting
/// `IsPromoted = 1` in the user's `HKCU\Control Panel\NotifyIconSettings\<id>` entry.
pub fn promote_self_tray_icon_on_taskbar() -> TrayPromoteResult {
    let Ok(self_exe) = std::env::current_exe() else {
        return TrayPromoteResult::NotFound;
    };
    let target = normalize_exe_path_for_compare(&self_exe.to_string_lossy());
    if target.is_empty() {
        return TrayPromoteResult::NotFound;
    }

    const NOTIFY_SETTINGS_KEY: PCWSTR = w!("Control Panel\\NotifyIconSettings");
    let mut found_already = false;
    let mut promoted_now = false;

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
            return TrayPromoteResult::NotFound;
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
            let uid_match = reg_read_key_u32(sub, w!("UID"))
                .map(|u| u == 1)
                .unwrap_or(true);
            let cur_promoted = reg_read_key_u32(sub, w!("IsPromoted"));
            let _ = RegCloseKey(sub);

            if !exe_match || !uid_match {
                continue;
            }
            if cur_promoted == Some(1) {
                found_already = true;
                continue;
            }

            let mut sub_w = HKEY::default();
            if RegOpenKeyExW(root, sub_name, 0, KEY_SET_VALUE, &mut sub_w).is_ok() {
                let one = 1u32.to_le_bytes();
                if RegSetValueExW(sub_w, w!("IsPromoted"), 0, REG_DWORD, Some(&one)).is_ok() {
                    promoted_now = true;
                }
                let _ = RegCloseKey(sub_w);
            }
        }
        let _ = RegCloseKey(root);
    }

    if promoted_now {
        TrayPromoteResult::PromotedNow
    } else if found_already {
        TrayPromoteResult::AlreadyPromoted
    } else {
        TrayPromoteResult::NotFound
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

/// Pure classifier used by both live Win32 inspection and unit tests.
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

fn read_window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 96];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        String::new()
    }
}

/// Inspects live Win32 Z-order above `pill_hwnd` (read-only, microsecond-level) to detect
/// lost `WS_EX_TOPMOST`, ghost topmost desync, or overlapping non-system topmost windows.
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
        let mut above = Vec::new();
        let mut cur = GetWindow(pill_hwnd, GW_HWNDPREV).unwrap_or_default();
        let mut steps = 0usize;
        while !cur.0.is_null() && steps < 96 {
            steps += 1;
            if IsWindowVisible(cur).as_bool()
                && !IsIconic(cur).as_bool()
                && !is_window_cloaked(cur)
            {
                let mut rc = RECT::default();
                if GetWindowRect(cur, &mut rc).is_ok() && rc.right > rc.left && rc.bottom > rc.top {
                    above.push(WindowAboveDesc {
                        hwnd_raw: cur.0 as isize,
                        class_name: read_window_class(cur),
                        is_topmost: window_has_topmost_style(cur),
                        rect: rc,
                    });
                }
            }
            cur = GetWindow(cur, GW_HWNDPREV).unwrap_or_default();
        }
        classify_topmost_health(has_top, pill_rc, &above)
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

        // 1) Bottom taskbar, icon centered with room on right -> docks 4px above taskbar, centered on icon
        let icon_mid = RECT { left: 2280, top: 1392, right: 2312, bottom: 1440 };
        let pos = compute_tray_flyout_pos_in_list(&mons, icon_mid, 240, 50, 10, 1.0);
        // vis_w = 220, vis_h = 30; icx = 2296 -> vis_x = 2186 -> win_x = 2176
        // vis_y = 1392 - 4 - 30 = 1358 -> win_y = 1348 (vis_bottom = 1388)
        assert_eq!(pos, (2176, 1348));

        // 2) Bottom taskbar near far-right clock -> clamped 6px inside right screen edge
        let icon_right = RECT { left: 2500, top: 1392, right: 2532, bottom: 1440 };
        let pos_r = compute_tray_flyout_pos_in_list(&mons, icon_right, 240, 50, 10, 1.0);
        let vis_right = pos_r.0 + 240 - 10;
        assert_eq!(vis_right, 2554); // 2560 - 6
        assert_eq!(pos_r.1 + 50 - 10, 1388); // 4px above taskbar

        // 3) Icon inside Windows 11 overflow flyout (above taskbar) -> docks right above the overflow icon
        let icon_overflow = RECT { left: 2300, top: 1280, right: 2332, bottom: 1312 };
        let pos_ov = compute_tray_flyout_pos_in_list(&mons, icon_overflow, 240, 50, 10, 1.0);
        assert_eq!(pos_ov.1 + 50 - 10, 1276); // 1280 - 4

        // 4) Hover zone includes icon, bridge corridor, and visible capsule, excludes adjacent taskbar icons
        assert!(is_cursor_in_tray_hover_zone((2296, 1416), icon_mid, pos, 240, 50, 10)); // on icon
        assert!(is_cursor_in_tray_hover_zone((2296, 1390), icon_mid, pos, 240, 50, 10)); // 4px bridge
        assert!(is_cursor_in_tray_hover_zone((2240, 1370), icon_mid, pos, 240, 50, 10)); // on pill
        assert!(!is_cursor_in_tray_hover_zone((2400, 1416), icon_mid, pos, 240, 50, 10)); // adjacent tray icon
        assert!(!is_cursor_in_tray_hover_zone((2296, 1200), icon_mid, pos, 240, 50, 10)); // far above pill

        assert_eq!(
            normalize_exe_path_for_compare(r#"\\?\C:/Users/Qingz/agy-token-monitor.exe"#),
            r"c:\users\qingz\agy-token-monitor.exe"
        );
    }
}
