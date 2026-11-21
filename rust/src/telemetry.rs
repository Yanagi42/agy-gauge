//! Zero-subprocess Win32 system telemetry, multi-monitor geometry, DWM & clipboard helpers.

use std::collections::VecDeque;
use std::mem::{size_of, zeroed};
use std::ptr;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::{PCWSTR, w};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, FILETIME, GetLastError, HANDLE, HGLOBAL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegGetValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, RRF_RT_REG_DWORD,
};
use windows::Win32::System::SystemInformation::{GetSystemInfo, GlobalMemoryStatusEx, MEMORYSTATUSEX, SYSTEM_INFO};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, GetSystemTimes};
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, PostMessageW, SystemParametersInfoW, SPI_GETWORKAREA,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_APP,
};

pub const WM_APP_SYNC_PREFS: u32 = WM_APP + 1;

#[derive(Debug, Clone, Copy)]
pub struct UserPrefs {
    pub light_theme: bool,
    pub mini: bool,
    pub topmost: bool,
    pub human: bool,
    pub monitor_index: usize,
    pub fresh_only: bool,
}

impl Default for UserPrefs {
    fn default() -> Self {
        Self {
            light_theme: false,
            mini: false,
            topmost: true,
            human: true,
            monitor_index: 0,
            fresh_only: false,
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
    if let Some(v) = reg_read_u32(w!("monitor_index")) {
        p.monitor_index = v as usize;
    }
    if let Some(v) = reg_read_u32(w!("fresh_only")) {
        p.fresh_only = v != 0;
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
        let entries: [(PCWSTR, u32); 6] = [
            (w!("light_theme"), u32::from(p.light_theme)),
            (w!("mini"), u32::from(p.mini)),
            (w!("topmost"), u32::from(p.topmost)),
            (w!("human"), u32::from(p.human)),
            (w!("monitor_index"), p.monitor_index as u32),
            (w!("fresh_only"), u32::from(p.fresh_only)),
        ];
        for (name, val) in entries {
            let bytes = val.to_le_bytes();
            let _ = RegSetValueExW(hkey, name, 0, REG_DWORD, Some(&bytes));
        }
        let _ = RegCloseKey(hkey);
    }
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
        let initial = TelemetrySample {
            timestamp: now_secs(),
            cpu_pct: 0.0,
            mem_pct: 0.0,
            mem_used_gb: 0.0,
            mem_total_gb: 0.0,
            mem_avail_gb: 0.0,
            self_mb: 0.0,
            cpu_cores,
        };
        let mut s = Self {
            prev_times,
            cpu_cores,
            history: VecDeque::with_capacity(cap),
            latest: initial,
        };
        s.sample();
        s
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

pub fn get_monitors() -> Vec<MonitorDesc> {
    struct RawMon {
        is_primary: bool,
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
