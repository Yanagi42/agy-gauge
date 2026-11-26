//! Official `/statusline` quota bridge reader and local daily/weekly budget threshold evaluation.
//!
//! Safety guarantee:
//!   - Strictly read-only on all Antigravity data under `~/.gemini`.
//!   - Reads/writes optional local companion files (`statusline_state.json`, `budget.json`)
//!     ONLY inside the application's own directory.

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const STATUSLINE_STALE_SECS: i64 = 600; // 10 minutes before official statusline snapshot is marked cached/stale

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileSig {
    pub(crate) mtime_secs: u64,
    pub(crate) mtime_nanos: u32,
    pub(crate) len: u64,
}

pub(crate) fn regular_file_sig(path: &Path) -> Option<FileSig> {
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(FileSig {
        mtime_secs: mtime.as_secs(),
        mtime_nanos: mtime.subsec_nanos(),
        len: meta.len(),
    })
}

pub fn default_companion_path(filename: &str) -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join(filename)))
}

/// Atomic file write in the application's own directory (never touches `~/.gemini`).
pub fn atomic_write_json(path: &Path, text: &str) -> Result<(), String> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(format!("拒绝写入符号链接: {}", path.display()));
        }
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| format!("无法创建目录: {e}"))?;
        }
    }
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    {
        let mut f = File::create(&tmp).map_err(|e| format!("无法创建临时文件: {e}"))?;
        f.write_all(text.as_bytes())
            .map_err(|e| format!("写入失败: {e}"))?;
        let _ = f.sync_all();
    }
    if fs::rename(&tmp, path).is_err() {
        // On Windows rename fails if destination already exists; fall back to copy + remove.
        fs::copy(&tmp, path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("覆盖保存失败: {e}")
        })?;
        let _ = fs::remove_file(&tmp);
    }
    Ok(())
}

pub fn fmt_countdown(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 86_400 {
        let d = s / 86_400;
        let h = (s % 86_400) / 3600;
        format!("{}d{}h", d, h)
    } else if s >= 3600 {
        let h = s / 3600;
        let m = (s % 3600) / 60;
        format!("{}h{:02}m", h, m)
    } else if s >= 60 {
        let m = s / 60;
        format!("{}m", m)
    } else {
        format!("{}s", s)
    }
}

// ---------- Official `/statusline` Bridge State ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaProvenance {
    OfficialLive,
    OfficialStale,
    LocalEstimate,
}

impl QuotaProvenance {
    pub fn label_cn(self) -> &'static str {
        match self {
            QuotaProvenance::OfficialLive => "官方实时",
            QuotaProvenance::OfficialStale => "官方缓存",
            QuotaProvenance::LocalEstimate => "本地估算",
        }
    }

    pub fn label_en(self) -> &'static str {
        match self {
            QuotaProvenance::OfficialLive => "official",
            QuotaProvenance::OfficialStale => "official_cached",
            QuotaProvenance::LocalEstimate => "local_estimate",
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuotaBucket {
    pub id: String,
    pub label: String,
    pub remaining_fraction: Option<f64>,
    pub remaining_amount: Option<f64>,
    pub reset_time_utc: Option<DateTime<Utc>>,
    pub reset_in_seconds: Option<i64>,
    pub disabled: bool,
}

impl QuotaBucket {
    pub fn remaining_pct(&self) -> Option<f64> {
        self.remaining_fraction.map(|f| (f * 100.0).clamp(0.0, 100.0))
    }

    pub fn used_pct(&self) -> Option<f64> {
        self.remaining_pct().map(|r| (100.0 - r).clamp(0.0, 100.0))
    }

    pub fn live_remaining_secs(&self, captured_at: DateTime<Utc>, now_utc: DateTime<Utc>) -> Option<i64> {
        if let Some(rt) = self.reset_time_utc {
            return Some((rt - now_utc).num_seconds().max(0));
        }
        self.reset_in_seconds.map(|r| {
            let age = (now_utc - captured_at).num_seconds().max(0);
            (r - age).max(0)
        })
    }

    pub fn inferred_window_secs(&self, live_rem: Option<i64>) -> i64 {
        let low = self.id.to_ascii_lowercase();
        if low.contains("5h") || low.contains("5-hour") || low.contains("session") {
            return 5 * 3600;
        }
        if low.contains("week") || low.contains("7d") || low.contains("7-day") {
            return 7 * 86_400;
        }
        if low.contains("day") || low.contains("daily") || low.contains("24h") {
            return 86_400;
        }
        if low.contains("month") || low.contains("30d") {
            return 30 * 86_400;
        }
        match live_rem {
            Some(r) if r <= 5 * 3600 + 300 => 5 * 3600,
            Some(r) if r <= 86_400 + 300 => 86_400,
            _ => 7 * 86_400,
        }
    }

    /// Time elapsed fraction in `[0, 100]` for pace comparison against `used_pct()`.
    pub fn time_elapsed_pct(&self, captured_at: DateTime<Utc>, now_utc: DateTime<Utc>) -> Option<f64> {
        let rem = self.live_remaining_secs(captured_at, now_utc)?;
        let win = self.inferred_window_secs(Some(rem)).max(60);
        let elapsed = (win - rem).clamp(0, win);
        Some((elapsed as f64 / win as f64) * 100.0)
    }

    /// Positive means burning faster than steady pace (`used_pct - time_elapsed_pct`).
    pub fn pace_delta_pct(&self, captured_at: DateTime<Utc>, now_utc: DateTime<Utc>) -> Option<f64> {
        Some(self.used_pct()? - self.time_elapsed_pct(captured_at, now_utc)?)
    }
}

pub fn friendly_bucket_label(id: &str) -> String {
    let low = id.to_ascii_lowercase();
    let provider = if low.contains("argon") {
        "Argon"
    } else if low.contains("claude") || low.contains("anthropic") {
        "Claude"
    } else if low.contains("gpt") || low.contains("openai") {
        "GPT"
    } else if low.contains("gemini") {
        "Gemini"
    } else {
        ""
    };
    let span = if low.contains("5h") || low.contains("5-hour") {
        "5h"
    } else if low.contains("week") || low.contains("7d") {
        "7d"
    } else if low.contains("day") || low.contains("daily") || low.contains("24h") {
        "24h"
    } else if low.contains("month") {
        "30d"
    } else {
        ""
    };
    match (provider.is_empty(), span.is_empty()) {
        (false, false) => format!("{provider} {span}"),
        (false, true) => provider.to_string(),
        (true, false) => format!("{id} ({span})"),
        (true, true) => id.to_string(),
    }
}

#[derive(Debug, Clone)]
pub struct StatuslineSnapshot {
    pub captured_at_utc: DateTime<Utc>,
    pub plan_tier: String,
    pub model_display: String,
    pub agent_state: String,
    pub context_used_pct: Option<f64>,
    pub context_window_size: Option<u64>,
    pub official_cost_usd: Option<f64>,
    pub buckets: Vec<QuotaBucket>,
}

impl StatuslineSnapshot {
    pub fn provenance(&self, now_utc: DateTime<Utc>) -> QuotaProvenance {
        if self.buckets.is_empty() {
            return QuotaProvenance::LocalEstimate;
        }
        let age = (now_utc - self.captured_at_utc).num_seconds();
        if age <= STATUSLINE_STALE_SECS {
            QuotaProvenance::OfficialLive
        } else {
            QuotaProvenance::OfficialStale
        }
    }

    pub fn age_secs(&self, now_utc: DateTime<Utc>) -> i64 {
        (now_utc - self.captured_at_utc).num_seconds().max(0)
    }

    /// Returns the tightest active quota bucket (lowest remaining percentage).
    pub fn tightest_bucket(&self) -> Option<&QuotaBucket> {
        self.buckets
            .iter()
            .filter(|b| !b.disabled && b.remaining_fraction.is_some())
            .min_by(|a, b| {
                a.remaining_fraction
                    .unwrap_or(1.0)
                    .partial_cmp(&b.remaining_fraction.unwrap_or(1.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }
}

/// Parses an official Antigravity CLI `statusLine` JSON payload (as documented at
/// `https://antigravity.google/docs/cli/statusline`) or our saved wrapper file.
pub fn parse_statusline_payload(text: &str, fallback_now: DateTime<Utc>) -> Option<StatuslineSnapshot> {
    let val: Value = serde_json::from_str(text).ok()?;
    let root = val.as_object()?;

    let captured_at_utc = root
        .get("captured_at")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or(fallback_now);

    let payload = root
        .get("payload")
        .and_then(Value::as_object)
        .unwrap_or(root);

    let plan_tier = payload
        .get("plan_tier")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let agent_state = payload
        .get("agent_state")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    let model_display = match payload.get("model") {
        Some(Value::Object(m)) => m
            .get("display_name")
            .or_else(|| m.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    };

    let ctx_obj = payload.get("context_window").and_then(Value::as_object);
    let context_used_pct = ctx_obj
        .and_then(|c| c.get("used_percentage"))
        .and_then(Value::as_f64);
    let context_window_size = ctx_obj
        .and_then(|c| c.get("context_window_size"))
        .and_then(Value::as_u64);
    let official_cost_usd = payload
        .get("cost")
        .and_then(Value::as_object)
        .and_then(|c| c.get("total_usd"))
        .and_then(Value::as_f64);

    let mut buckets = Vec::new();
    if let Some(q_val) = payload.get("quota") {
        if let Some(q_map) = q_val.as_object() {
            // Handle both official map format `{ "gemini-weekly": { ... } }` and `{ "pools": [...] }`.
            if let Some(arr) = q_map.get("pools").and_then(Value::as_array) {
                for item in arr {
                    if let Some(obj) = item.as_object() {
                        let prov = obj.get("provider").and_then(Value::as_str).unwrap_or("quota");
                        let win = obj.get("window").and_then(Value::as_str).unwrap_or("");
                        let id = if win.is_empty() {
                            prov.to_string()
                        } else {
                            format!("{prov}-{win}")
                        };
                        if let Some(b) = parse_bucket_obj(&id, obj) {
                            buckets.push(b);
                        }
                    }
                }
            } else {
                for (id, v) in q_map {
                    if let Some(obj) = v.as_object() {
                        if let Some(b) = parse_bucket_obj(id, obj) {
                            buckets.push(b);
                        }
                    }
                }
            }
        }
    }

    buckets.sort_by(|a, b| {
        a.disabled
            .cmp(&b.disabled)
            .then_with(|| {
                a.remaining_fraction
                    .unwrap_or(2.0)
                    .partial_cmp(&b.remaining_fraction.unwrap_or(2.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.id.cmp(&b.id))
    });

    Some(StatuslineSnapshot {
        captured_at_utc,
        plan_tier,
        model_display,
        agent_state,
        context_used_pct,
        context_window_size,
        official_cost_usd,
        buckets,
    })
}

fn parse_bucket_obj(id: &str, obj: &Map<String, Value>) -> Option<QuotaBucket> {
    let remaining_fraction = obj
        .get("remaining_fraction")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite());
    let remaining_amount = obj
        .get("remaining_amount")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite());
    let reset_time_utc = obj
        .get("reset_time")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s.trim()).ok())
        .map(|d| d.with_timezone(&Utc));
    let reset_in_seconds = obj.get("reset_in_seconds").and_then(Value::as_i64);
    let disabled = obj.get("disabled").and_then(Value::as_bool).unwrap_or(false);

    if remaining_fraction.is_none() && remaining_amount.is_none() && !disabled {
        return None;
    }

    Some(QuotaBucket {
        id: id.to_string(),
        label: friendly_bucket_label(id),
        remaining_fraction: remaining_fraction.map(|f| f.clamp(0.0, 1.0)),
        remaining_amount,
        reset_time_utc,
        reset_in_seconds,
        disabled,
    })
}

/// Serializes a sanitized statusline bridge state (stripping PII like `email`, `cwd`, `workspace`,
/// and `conversation_title` so the cached file stays privacy-safe).
pub fn save_statusline_state(path: &Path, snap: &StatuslineSnapshot) -> Result<(), String> {
    let mut q_map = Map::new();
    for b in &snap.buckets {
        let mut bo = Map::new();
        if let Some(rf) = b.remaining_fraction {
            bo.insert("remaining_fraction".into(), json!((rf * 10000.0).round() / 10000.0));
        }
        if let Some(ra) = b.remaining_amount {
            bo.insert("remaining_amount".into(), json!(ra));
        }
        if let Some(rt) = b.reset_time_utc {
            bo.insert("reset_time".into(), json!(rt.to_rfc3339()));
        }
        if let Some(rs) = b.reset_in_seconds {
            bo.insert("reset_in_seconds".into(), json!(rs));
        }
        if b.disabled {
            bo.insert("disabled".into(), json!(true));
        }
        q_map.insert(b.id.clone(), Value::Object(bo));
    }

    let mut payload = Map::new();
    if !snap.plan_tier.is_empty() {
        payload.insert("plan_tier".into(), json!(snap.plan_tier));
    }
    if !snap.model_display.is_empty() {
        payload.insert("model".into(), json!({ "display_name": snap.model_display }));
    }
    if !snap.agent_state.is_empty() {
        payload.insert("agent_state".into(), json!(snap.agent_state));
    }
    if snap.context_used_pct.is_some() || snap.context_window_size.is_some() {
        let mut cw = Map::new();
        if let Some(pct) = snap.context_used_pct {
            cw.insert("used_percentage".into(), json!(pct));
        }
        if let Some(sz) = snap.context_window_size {
            cw.insert("context_window_size".into(), json!(sz));
        }
        payload.insert("context_window".into(), Value::Object(cw));
    }
    if let Some(usd) = snap.official_cost_usd {
        payload.insert("cost".into(), json!({ "total_usd": usd }));
    }
    payload.insert("quota".into(), Value::Object(q_map));

    let root = json!({
        "schema_version": 1,
        "captured_at": snap.captured_at_utc.to_rfc3339(),
        "payload": Value::Object(payload),
    });
    let text = serde_json::to_string_pretty(&root).map_err(|e| format!("序列化失败: {e}"))?;
    atomic_write_json(path, &format!("{text}\n"))
}

pub struct StatuslineReader {
    path: Option<PathBuf>,
    sig: Option<FileSig>,
    cached: Option<StatuslineSnapshot>,
}

impl StatuslineReader {
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut r = Self {
            path,
            sig: None,
            cached: None,
        };
        r.refresh();
        r
    }

    pub fn default_path() -> Option<PathBuf> {
        default_companion_path("statusline_state.json")
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn snapshot(&self) -> Option<&StatuslineSnapshot> {
        self.cached.as_ref()
    }

    /// Hot-reloads `statusline_state.json` if mtime/size changed. Returns `true` on change.
    pub fn refresh(&mut self) -> bool {
        let Some(path) = self.path.clone() else {
            return false;
        };
        let sig = regular_file_sig(&path);
        if sig == self.sig {
            return false;
        }
        self.sig = sig;
        match sig {
            None => {
                let had = self.cached.is_some();
                self.cached = None;
                had
            }
            Some(_) => {
                if let Ok(mut f) = File::open(&path) {
                    let mut text = String::new();
                    if f.read_to_string(&mut text).is_ok() {
                        self.cached = parse_statusline_payload(&text, Utc::now());
                        return true;
                    }
                }
                false
            }
        }
    }
}

// ---------- Budget & Alert Thresholds ----------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetConfig {
    pub daily_usd: f64,          // 0 = disabled
    pub weekly_usd: f64,         // 0 = disabled
    pub warn_pct: f64,           // default 70.0
    pub critical_pct: f64,       // default 90.0
    pub quota_warn_rem_pct: f64, // default 20.0 (alert when official quota remaining <= 20%)
    pub quota_crit_rem_pct: f64, // default 10.0
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            daily_usd: 0.0,
            weekly_usd: 0.0,
            warn_pct: 70.0,
            critical_pct: 90.0,
            quota_warn_rem_pct: 20.0,
            quota_crit_rem_pct: 10.0,
        }
    }
}

impl BudgetConfig {
    pub fn has_any_usd_budget(&self) -> bool {
        self.daily_usd > 0.0 || self.weekly_usd > 0.0
    }

    pub fn classify_usage_pct(&self, pct: f64) -> AlertSeverity {
        if pct >= self.critical_pct.max(1.0) {
            AlertSeverity::Critical
        } else if pct >= self.warn_pct.max(1.0) {
            AlertSeverity::Warn
        } else {
            AlertSeverity::Normal
        }
    }

    pub fn classify_quota_remaining(&self, rem_pct: f64) -> AlertSeverity {
        if rem_pct <= self.quota_crit_rem_pct {
            AlertSeverity::Critical
        } else if rem_pct <= self.quota_warn_rem_pct {
            AlertSeverity::Warn
        } else {
            AlertSeverity::Normal
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlertSeverity {
    Normal = 0,
    Warn = 1,
    Critical = 2,
}

#[derive(Debug, Clone)]
pub struct AlertEvent {
    pub dedup_key: String,
    pub severity: AlertSeverity,
    pub title: String,
    pub message: String,
}

pub fn evaluate_alerts(
    cfg: &BudgetConfig,
    today_key: &str,
    today_usd: f64,
    week_key: &str,
    week_usd: f64,
    statusline: Option<&StatuslineSnapshot>,
    now_utc: DateTime<Utc>,
) -> Vec<AlertEvent> {
    let mut events = Vec::new();

    if cfg.daily_usd > 0.0 {
        let pct = (today_usd / cfg.daily_usd) * 100.0;
        let sev = cfg.classify_usage_pct(pct);
        if sev != AlertSeverity::Normal {
            events.push(AlertEvent {
                dedup_key: format!("day:{}:{:?}", today_key, sev),
                severity: sev,
                title: if sev == AlertSeverity::Critical {
                    "Gauge 今日预算临界".into()
                } else {
                    "Gauge 今日预算预警".into()
                },
                message: format!(
                    "今日估算费用已达 ${:.2} / ${:.2} ({:.0}%)",
                    today_usd, cfg.daily_usd, pct
                ),
            });
        }
    }

    if cfg.weekly_usd > 0.0 {
        let pct = (week_usd / cfg.weekly_usd) * 100.0;
        let sev = cfg.classify_usage_pct(pct);
        if sev != AlertSeverity::Normal {
            events.push(AlertEvent {
                dedup_key: format!("week:{}:{:?}", week_key, sev),
                severity: sev,
                title: if sev == AlertSeverity::Critical {
                    "Gauge 本周预算临界".into()
                } else {
                    "Gauge 本周预算预警".into()
                },
                message: format!(
                    "{} 估算费用已达 ${:.2} / ${:.2} ({:.0}%)",
                    week_key, week_usd, cfg.weekly_usd, pct
                ),
            });
        }
    }

    if let Some(sl) = statusline {
        if sl.provenance(now_utc) != QuotaProvenance::LocalEstimate {
            for b in &sl.buckets {
                if b.disabled {
                    continue;
                }
                let Some(rem_pct) = b.remaining_pct() else { continue };
                let sev = cfg.classify_quota_remaining(rem_pct);
                if sev != AlertSeverity::Normal {
                    let reset_key = b
                        .reset_time_utc
                        .map(|t| t.format("%Y%m%d%H").to_string())
                        .unwrap_or_else(|| today_key.to_string());
                    let rem_t = b
                        .live_remaining_secs(sl.captured_at_utc, now_utc)
                        .map(fmt_countdown)
                        .unwrap_or_else(|| "-".into());
                    events.push(AlertEvent {
                        dedup_key: format!("quota:{}:{}:{:?}", b.id, reset_key, sev),
                        severity: sev,
                        title: if sev == AlertSeverity::Critical {
                            format!("AGY {} 额度即将耗尽", b.label)
                        } else {
                            format!("AGY {} 额度偏低", b.label)
                        },
                        message: format!(
                            "{} 剩余 {:.0}% · 重置倒计时 {}",
                            b.label, rem_pct, rem_t
                        ),
                    });
                }
            }
        }
    }

    events
}

pub struct BudgetStore {
    path: Option<PathBuf>,
    sig: Option<FileSig>,
    config: BudgetConfig,
}

impl BudgetStore {
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut s = Self {
            path,
            sig: None,
            config: BudgetConfig::default(),
        };
        s.refresh();
        s
    }

    pub fn default_path() -> Option<PathBuf> {
        default_companion_path("budget.json")
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn config(&self) -> BudgetConfig {
        self.config
    }

    pub fn refresh(&mut self) -> bool {
        let Some(path) = self.path.clone() else {
            return false;
        };
        let sig = regular_file_sig(&path);
        if sig == self.sig {
            return false;
        }
        self.sig = sig;
        match sig {
            None => {
                let changed = self.config != BudgetConfig::default();
                self.config = BudgetConfig::default();
                changed
            }
            Some(_) => {
                if let Ok(mut f) = File::open(&path) {
                    let mut text = String::new();
                    if f.read_to_string(&mut text).is_ok() {
                        if let Some(parsed) = parse_budget_json(&text) {
                            let changed = self.config != parsed;
                            self.config = parsed;
                            return changed;
                        }
                    }
                }
                false
            }
        }
    }

    pub fn save(&mut self, cfg: BudgetConfig) -> Result<(), String> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| "未配置 budget.json 路径".to_string())?;
        let warn_pct = cfg.warn_pct.clamp(1.0, 500.0);
        let critical_pct = cfg.critical_pct.clamp(warn_pct, 500.0);
        let quota_warn_rem_pct = cfg.quota_warn_rem_pct.clamp(1.0, 95.0);
        let quota_crit_rem_pct = cfg.quota_crit_rem_pct.clamp(0.0, quota_warn_rem_pct);
        let saved_cfg = BudgetConfig {
            daily_usd: (cfg.daily_usd.max(0.0) * 100.0).round() / 100.0,
            weekly_usd: (cfg.weekly_usd.max(0.0) * 100.0).round() / 100.0,
            warn_pct,
            critical_pct,
            quota_warn_rem_pct,
            quota_crit_rem_pct,
        };
        let val = json!({
            "daily_usd": saved_cfg.daily_usd,
            "weekly_usd": saved_cfg.weekly_usd,
            "warn_pct": saved_cfg.warn_pct,
            "critical_pct": saved_cfg.critical_pct,
            "quota_warn_rem_pct": saved_cfg.quota_warn_rem_pct,
            "quota_crit_rem_pct": saved_cfg.quota_crit_rem_pct,
        });
        let text = serde_json::to_string_pretty(&val).map_err(|e| format!("序列化失败: {e}"))?;
        atomic_write_json(&path, &format!("{text}\n"))?;
        self.config = saved_cfg;
        self.sig = regular_file_sig(&path);
        Ok(())
    }
}

pub fn parse_budget_json(text: &str) -> Option<BudgetConfig> {
    let val: Value = serde_json::from_str(text).ok()?;
    let obj = val.as_object()?;
    let mut cfg = BudgetConfig::default();
    let get_nonneg = |k: &str| {
        obj.get(k)
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v >= 0.0)
    };
    if let Some(v) = get_nonneg("daily_usd") {
        cfg.daily_usd = v;
    }
    if let Some(v) = get_nonneg("weekly_usd") {
        cfg.weekly_usd = v;
    }
    if let Some(v) = get_nonneg("warn_pct") {
        cfg.warn_pct = v.clamp(1.0, 500.0);
    }
    if let Some(v) = get_nonneg("critical_pct") {
        cfg.critical_pct = v.clamp(cfg.warn_pct, 500.0);
    }
    if let Some(v) = get_nonneg("quota_warn_rem_pct") {
        cfg.quota_warn_rem_pct = v.clamp(1.0, 95.0);
    }
    if let Some(v) = get_nonneg("quota_crit_rem_pct") {
        cfg.quota_crit_rem_pct = v.clamp(0.0, cfg.quota_warn_rem_pct);
    }
    Some(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_json_ignores_legacy_block_5h_usd() {
        let legacy = r#"{
            "block_5h_usd": 15.0,
            "daily_usd": 30.0,
            "weekly_usd": 120.0,
            "warn_pct": 75.0,
            "critical_pct": 95.0,
            "quota_warn_rem_pct": 25.0,
            "quota_crit_rem_pct": 12.0
        }"#;
        let cfg = parse_budget_json(legacy).expect("legacy budget.json should still parse");
        assert!((cfg.daily_usd - 30.0).abs() < 1e-9);
        assert!((cfg.weekly_usd - 120.0).abs() < 1e-9);
        assert!((cfg.warn_pct - 75.0).abs() < 1e-9);
        assert!((cfg.critical_pct - 95.0).abs() < 1e-9);
        assert!((cfg.quota_warn_rem_pct - 25.0).abs() < 1e-9);
        assert!((cfg.quota_crit_rem_pct - 12.0).abs() < 1e-9);
        assert!(cfg.has_any_usd_budget());
    }

    #[test]
    fn test_official_statusline_parsing_and_alerts() {
        let now = DateTime::parse_from_rfc3339("2026-11-23T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let raw = r#"{
            "plan_tier": "Pro",
            "agent_state": "working",
            "model": { "display_name": "Gemini 4 Neon (XHigh)" },
            "context_window": { "used_percentage": 24.5, "context_window_size": 1048576 },
            "cost": { "total_usd": 1.25 },
            "quota": {
                "gemini-weekly": {
                    "remaining_fraction": 0.85,
                    "reset_time": "2026-11-28T02:00:00Z",
                    "reset_in_seconds": 432000
                },
                "claude-5h": {
                    "remaining_fraction": 0.08,
                    "reset_time": "2026-11-23T04:00:00Z",
                    "reset_in_seconds": 7200
                }
            }
        }"#;
        let snap = parse_statusline_payload(raw, now).expect("valid payload");
        assert_eq!(snap.provenance(now), QuotaProvenance::OfficialLive);
        assert_eq!(snap.plan_tier, "Pro");
        assert_eq!(snap.buckets.len(), 2);
        // Tightest bucket comes first (claude-5h @ 8% remaining)
        assert_eq!(snap.buckets[0].id, "claude-5h");
        assert_eq!(snap.buckets[0].label, "Claude 5h");
        assert!((snap.buckets[0].remaining_pct().unwrap() - 8.0).abs() < 0.01);
        // 3h elapsed out of 5h = 60% time, 92% used -> pace delta +32%
        let delta = snap.buckets[0].pace_delta_pct(now, now).unwrap();
        assert!((delta - 32.0).abs() < 0.5);

        let cfg = BudgetConfig {
            daily_usd: 10.0,
            ..Default::default()
        };
        let alerts = evaluate_alerts(&cfg, "2026-11-23", 7.5, "2026-W48", 7.5, Some(&snap), now);
        assert_eq!(alerts.len(), 2);
        assert!(alerts.iter().any(|a| a.dedup_key == "day:2026-11-23:Warn"));
        assert!(alerts.iter().any(|a| a.dedup_key.starts_with("quota:claude-5h:") && a.severity == AlertSeverity::Critical));
    }
}
