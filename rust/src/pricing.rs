//! API cost pricing catalog (USD per 1M tokens) with optional local `pricing.json` overrides.
//!
//! Safety guarantee:
//!   - Never touches `~/.gemini/antigravity-cli` (strictly read-only on all agy data).
//!   - Reads optional `pricing.json` next to the executable (or `--pricing <path>`), hot-reloading
//!     on `(mtime, size)` change.
//!   - Only writes to that local `pricing.json` when the user explicitly saves or resets a custom
//!     model price inside the dashboard Pricing editor.

use chrono::NaiveDate;
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// USD price per 1,000,000 tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelRate {
    pub input: f64,       // Uncached input ($ / 1M)
    pub cache_read: f64,  // Prompt cache hit ($ / 1M)
    pub cache_write: f64, // Prompt cache creation ($ / 1M)
    pub output: f64,      // Output including thinking ($ / 1M)
}

impl ModelRate {
    pub const fn new(input: f64, cache_read: f64, cache_write: f64, output: f64) -> Self {
        Self {
            input,
            cache_read,
            cache_write,
            output,
        }
    }

    pub fn calculate(self, input: u64, cache_read: u64, cache_write: u64, output: u64) -> CostBreakdown {
        const PER_M: f64 = 1_000_000.0;
        let input_usd = (input as f64 / PER_M) * self.input;
        let cache_read_usd = (cache_read as f64 / PER_M) * self.cache_read;
        let cache_write_usd = (cache_write as f64 / PER_M) * self.cache_write;
        let output_usd = (output as f64 / PER_M) * self.output;
        let cache_saved_usd = if self.input > self.cache_read {
            (cache_read as f64 / PER_M) * (self.input - self.cache_read)
        } else {
            0.0
        };
        CostBreakdown {
            input_usd,
            cache_read_usd,
            cache_write_usd,
            output_usd,
            cache_saved_usd,
            unpriced_calls: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CostBreakdown {
    pub input_usd: f64,
    pub cache_read_usd: f64,
    pub cache_write_usd: f64,
    pub output_usd: f64,
    pub cache_saved_usd: f64,
    pub unpriced_calls: u64,
}

impl CostBreakdown {
    pub fn unpriced() -> Self {
        Self {
            unpriced_calls: 1,
            ..Default::default()
        }
    }

    pub fn total_usd(&self) -> f64 {
        self.input_usd + self.cache_read_usd + self.cache_write_usd + self.output_usd
    }

    pub fn non_cached_usd(&self) -> f64 {
        self.input_usd + self.cache_write_usd + self.output_usd
    }

    pub fn input_plus_write_usd(&self) -> f64 {
        self.input_usd + self.cache_write_usd
    }

    pub fn merge(&mut self, other: &CostBreakdown) {
        self.input_usd += other.input_usd;
        self.cache_read_usd += other.cache_read_usd;
        self.cache_write_usd += other.cache_write_usd;
        self.output_usd += other.output_usd;
        self.cache_saved_usd += other.cache_saved_usd;
        self.unpriced_calls = self.unpriced_calls.saturating_add(other.unpriced_calls);
    }

    pub fn has_unpriced(&self) -> bool {
        self.unpriced_calls > 0
    }

    pub fn is_fully_unpriced(&self, calls: u64) -> bool {
        calls > 0 && self.unpriced_calls >= calls
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSource {
    CustomExact,
    CustomPrefix,
    Builtin,
    Unpriced,
}

impl PriceSource {
    pub fn label_cn(self) -> &'static str {
        match self {
            PriceSource::CustomExact => "自定义(精确)",
            PriceSource::CustomPrefix => "自定义(前缀)",
            PriceSource::Builtin => "内置默认",
            PriceSource::Unpriced => "未定价",
        }
    }

    pub fn is_custom(self) -> bool {
        matches!(self, PriceSource::CustomExact | PriceSource::CustomPrefix)
    }
}

#[derive(Debug, Clone)]
pub struct PricingTableRow {
    pub model: String,
    pub source: PriceSource,
    pub matched_rule: String,
    pub rate: Option<ModelRate>,
    pub is_reference: bool, // false = observed in logs or custom rule; true = unused built-in reference
    pub custom_key: Option<String>,
    pub note: String,
}

struct BuiltinSpec {
    display: &'static str,
    prefixes: &'static [&'static str],
    base: ModelRate,
    long_threshold: Option<u64>,
    long_rate: Option<ModelRate>,
    intro_before: Option<(i32, u32, u32)>,
    intro_rate: Option<ModelRate>,
    note: &'static str,
}

impl BuiltinSpec {
    fn resolve(&self, ctx: u64, date: Option<NaiveDate>) -> ModelRate {
        if let (Some((y, m, d)), Some(ir)) = (self.intro_before, self.intro_rate) {
            if let Some(cutoff) = NaiveDate::from_ymd_opt(y, m, d) {
                if date.map(|dt| dt < cutoff).unwrap_or(false) {
                    return ir;
                }
            }
        }
        if let (Some(th), Some(lr)) = (self.long_threshold, self.long_rate) {
            if ctx > th {
                return lr;
            }
        }
        self.base
    }
}

const BUILTIN_SPECS: &[BuiltinSpec] = &[
    // --- Google Gemini 4 family ---
    BuiltinSpec {
        display: "gemini-4-neon",
        prefixes: &["gemini-4-neon"],
        base: ModelRate::new(3.00, 0.15, 3.00, 15.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "1M 统一价",
    },
    BuiltinSpec {
        display: "gemini-4-hydrogen",
        prefixes: &["gemini-4-hydrogen"],
        base: ModelRate::new(1.50, 0.075, 1.50, 7.50),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gemini-4-carbon",
        prefixes: &["gemini-4-carbon", "gemini-4-argon"],
        base: ModelRate::new(4.00, 0.20, 4.00, 20.00),
        long_threshold: None,
        long_rate: None,
        intro_before: Some((2026, 12, 1)),
        intro_rate: Some(ModelRate::new(2.00, 0.10, 2.00, 10.00)),
        note: "12/1前推广价 $2/$10",
    },
    // --- Google Gemini 3 / 2.5 family ---
    BuiltinSpec {
        display: "gemini-3.8-flash",
        prefixes: &["gemini-3-8-flash"],
        base: ModelRate::new(1.50, 0.15, 1.50, 7.50),
        long_threshold: None,
        long_rate: None,
        intro_before: Some((2027, 1, 1)),
        intro_rate: Some(ModelRate::new(0.75, 0.075, 0.75, 3.75)),
        note: "2026推广价 $0.75/$3.75",
    },
    BuiltinSpec {
        display: "gemini-3.5-flash-lite",
        prefixes: &["gemini-3-5-flash-lite", "gemini-2-5-flash-lite"],
        base: ModelRate::new(0.10, 0.025, 0.10, 0.40),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gemini-3.1-pro",
        prefixes: &["gemini-3-1-pro", "gemini-3-0-pro", "gemini-3-pro"],
        base: ModelRate::new(2.00, 0.20, 2.00, 12.00),
        long_threshold: Some(200_000),
        long_rate: Some(ModelRate::new(4.00, 0.40, 4.00, 18.00)),
        intro_before: None,
        intro_rate: None,
        note: ">200K 阶梯 $4/$18",
    },
    BuiltinSpec {
        display: "gemini-3-flash",
        prefixes: &["gemini-3-flash", "gemini-3-0-flash"],
        base: ModelRate::new(0.50, 0.05, 0.50, 3.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gemini-2.5-pro",
        prefixes: &["gemini-2-5-pro"],
        base: ModelRate::new(1.25, 0.125, 1.25, 10.00),
        long_threshold: Some(200_000),
        long_rate: Some(ModelRate::new(2.50, 0.25, 2.50, 15.00)),
        intro_before: None,
        intro_rate: None,
        note: ">200K 阶梯 $2.5/$15",
    },
    BuiltinSpec {
        display: "gemini-2.5-flash",
        prefixes: &["gemini-2-5-flash"],
        base: ModelRate::new(0.30, 0.03, 0.30, 2.50),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gemini-2.0-flash-lite",
        prefixes: &["gemini-2-0-flash-lite"],
        base: ModelRate::new(0.075, 0.01875, 0.075, 0.30),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gemini-2.0-flash",
        prefixes: &["gemini-2-0-flash"],
        base: ModelRate::new(0.10, 0.025, 0.10, 0.40),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    // --- Anthropic Claude family ---
    BuiltinSpec {
        display: "claude-opus-5-5",
        prefixes: &["claude-opus-5-5"],
        base: ModelRate::new(4.00, 0.20, 5.00, 20.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "1M 统一价",
    },
    BuiltinSpec {
        display: "claude-sonnet-5-5",
        prefixes: &["claude-sonnet-5-5"],
        base: ModelRate::new(2.00, 0.10, 2.50, 10.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "claude-haiku-5-5",
        prefixes: &["claude-haiku-5-5"],
        base: ModelRate::new(0.10, 0.01, 0.125, 0.50),
        long_threshold: Some(100_000),
        long_rate: Some(ModelRate::new(0.50, 0.05, 0.625, 2.50)),
        intro_before: None,
        intro_rate: None,
        note: ">100K 阶梯 $0.5/$2.5",
    },
    BuiltinSpec {
        display: "claude-fable-5-2",
        prefixes: &["claude-fable-5-2", "claude-mythos-5-2"],
        base: ModelRate::new(8.00, 0.20, 10.00, 40.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "claude-opus-5",
        prefixes: &["claude-opus-5"],
        base: ModelRate::new(5.00, 0.50, 6.25, 25.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "claude-sonnet-5",
        prefixes: &["claude-sonnet-5"],
        base: ModelRate::new(2.00, 0.20, 2.50, 10.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "claude-opus-4-6",
        prefixes: &["claude-opus-4-6", "claude-opus-4-5"],
        base: ModelRate::new(5.00, 0.50, 6.25, 25.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "claude-opus-4",
        prefixes: &["claude-opus-4"],
        base: ModelRate::new(15.00, 1.50, 18.75, 75.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "claude-sonnet-4",
        prefixes: &["claude-sonnet-4", "claude-3-7-sonnet", "claude-3-5-sonnet"],
        base: ModelRate::new(3.00, 0.30, 3.75, 15.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "claude-haiku-4-5",
        prefixes: &["claude-haiku-4-5"],
        base: ModelRate::new(1.00, 0.10, 1.25, 5.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "claude-3-5-haiku",
        prefixes: &["claude-3-5-haiku"],
        base: ModelRate::new(0.80, 0.08, 1.00, 4.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    // --- OpenAI GPT / Codex / o-series (most-specific prefixes before shorter ones like "gpt-5") ---
    BuiltinSpec {
        display: "gpt-5.4",
        prefixes: &["gpt-5-4"],
        base: ModelRate::new(2.50, 0.25, 2.50, 15.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gpt-5.3-codex",
        prefixes: &["gpt-5-3-codex"],
        base: ModelRate::new(1.75, 0.175, 1.75, 14.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gpt-5.2-codex",
        prefixes: &["gpt-5-2-codex"],
        base: ModelRate::new(1.75, 0.175, 1.75, 14.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gpt-5.2",
        prefixes: &["gpt-5-2"],
        base: ModelRate::new(1.75, 0.175, 1.75, 14.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gpt-5.1-codex-mini",
        prefixes: &["gpt-5-1-codex-mini"],
        base: ModelRate::new(0.25, 0.025, 0.25, 2.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gpt-5.1-codex",
        prefixes: &["gpt-5-1-codex"],
        base: ModelRate::new(1.25, 0.125, 1.25, 10.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gpt-5.1",
        prefixes: &["gpt-5-1"],
        base: ModelRate::new(1.25, 0.125, 1.25, 10.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gpt-5-codex",
        prefixes: &["gpt-5-codex"],
        base: ModelRate::new(1.25, 0.125, 1.25, 10.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gpt-5-mini",
        prefixes: &["gpt-5-mini"],
        base: ModelRate::new(0.25, 0.025, 0.25, 2.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gpt-5-nano",
        prefixes: &["gpt-5-nano"],
        base: ModelRate::new(0.05, 0.005, 0.05, 0.40),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gpt-5",
        prefixes: &["gpt-5"],
        base: ModelRate::new(1.25, 0.125, 1.25, 10.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "codex-mini-latest",
        prefixes: &["codex-mini-latest", "codex-mini"],
        base: ModelRate::new(1.50, 0.375, 1.50, 6.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gpt-4.1-mini",
        prefixes: &["gpt-4-1-mini"],
        base: ModelRate::new(0.40, 0.10, 0.40, 1.60),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gpt-4.1-nano",
        prefixes: &["gpt-4-1-nano"],
        base: ModelRate::new(0.10, 0.025, 0.10, 0.40),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gpt-4.1",
        prefixes: &["gpt-4-1"],
        base: ModelRate::new(2.00, 0.50, 2.00, 8.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gpt-4o-mini",
        prefixes: &["gpt-4o-mini"],
        base: ModelRate::new(0.15, 0.075, 0.15, 0.60),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "gpt-4o",
        prefixes: &["gpt-4o"],
        base: ModelRate::new(2.50, 1.25, 2.50, 10.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "o3-mini",
        prefixes: &["o3-mini", "o4-mini"],
        base: ModelRate::new(1.10, 0.55, 1.10, 4.40),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
    BuiltinSpec {
        display: "o3",
        prefixes: &["o3"],
        base: ModelRate::new(2.00, 0.50, 2.00, 8.00),
        long_threshold: None,
        long_rate: None,
        intro_before: None,
        intro_rate: None,
        note: "",
    },
];

#[derive(Debug, Clone)]
struct CustomRateRule {
    raw_key: String,   // Original user key as written in pricing.json
    exact_key: String, // Lowercase trimmed key preserving effort suffix (for exact match)
    prefix: String,    // Normalized key stripping effort suffix (for prefix/family match)
    has_effort_suffix: bool,
    base: ModelRate,
    long_threshold: Option<u64>,
    long_rate: Option<ModelRate>,
}

impl CustomRateRule {
    fn resolve(&self, total_input_tokens: u64) -> ModelRate {
        if let (Some(th), Some(lr)) = (self.long_threshold, self.long_rate) {
            if total_input_tokens > th {
                return lr;
            }
        }
        self.base
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSig {
    mtime_secs: u64,
    mtime_nanos: u32,
    len: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedRule {
    CustomExact(usize),
    CustomPrefix(usize),
    Builtin(usize),
    Unpriced,
}

const RATE_CACHE_MAX: usize = 512;

#[derive(Debug)]
pub struct PricingCatalog {
    override_path: Option<PathBuf>,
    override_sig: Option<FileSig>,
    custom_rules: Vec<CustomRateRule>,
    rate_cache: RefCell<HashMap<String, ResolvedRule>>,
}

impl Clone for PricingCatalog {
    fn clone(&self) -> Self {
        Self {
            override_path: self.override_path.clone(),
            override_sig: self.override_sig,
            custom_rules: self.custom_rules.clone(),
            rate_cache: RefCell::new(HashMap::new()),
        }
    }
}

impl Default for PricingCatalog {
    fn default() -> Self {
        Self::new(Self::default_override_path())
    }
}

impl PricingCatalog {
    pub fn new(override_path: Option<PathBuf>) -> Self {
        let mut cat = Self {
            override_path,
            override_sig: None,
            custom_rules: Vec::new(),
            rate_cache: RefCell::new(HashMap::new()),
        };
        cat.refresh();
        cat
    }

    pub fn default_override_path() -> Option<PathBuf> {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                return Some(dir.join("pricing.json"));
            }
        }
        None
    }

    pub fn override_path(&self) -> Option<&Path> {
        self.override_path.as_deref()
    }

    pub fn custom_rules_count(&self) -> usize {
        self.custom_rules.len()
    }

    /// Re-checks `pricing.json` mtime/size and reloads strictly read-only if changed.
    /// Returns `true` if effective pricing rules changed.
    pub fn refresh(&mut self) -> bool {
        let Some(path) = self.override_path.as_deref() else {
            return false;
        };
        let sig = regular_file_sig(path);
        if sig == self.override_sig {
            return false;
        }
        self.override_sig = sig;
        self.rate_cache.borrow_mut().clear();
        match sig {
            None => {
                let had = !self.custom_rules.is_empty();
                self.custom_rules.clear();
                had
            }
            Some(_) => {
                self.custom_rules = load_custom_rules(path);
                true
            }
        }
    }

    /// Saves or updates a custom model price rule in the local `pricing.json` file.
    pub fn upsert_custom_rule(&mut self, raw_model: &str, rate: ModelRate) -> Result<(), String> {
        let key = raw_model.trim();
        if key.is_empty() {
            return Err("模型名不能为空".to_string());
        }
        if !rate.input.is_finite()
            || !rate.cache_read.is_finite()
            || !rate.cache_write.is_finite()
            || !rate.output.is_finite()
            || rate.input < 0.0
            || rate.cache_read < 0.0
            || rate.cache_write < 0.0
            || rate.output < 0.0
        {
            return Err("单价必须为非负数字".to_string());
        }
        let path = self
            .override_path
            .clone()
            .ok_or_else(|| "未配置 pricing.json 路径".to_string())?;

        let (mut root, mut models_map) = load_pricing_json_object(&path);

        // Preserve any existing extra keys (such as long_context) on the matched entry if present.
        let existing_key = models_map
            .keys()
            .find(|k| k.eq_ignore_ascii_case(key))
            .cloned();
        let mut entry = existing_key
            .as_deref()
            .and_then(|k| models_map.remove(k))
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();

        entry.insert("input".to_string(), json!(round_rate(rate.input)));
        entry.insert("cache_read".to_string(), json!(round_rate(rate.cache_read)));
        entry.insert("cache_write".to_string(), json!(round_rate(rate.cache_write)));
        entry.insert("output".to_string(), json!(round_rate(rate.output)));
        models_map.insert(key.to_string(), Value::Object(entry));

        root.insert("models".to_string(), Value::Object(models_map));
        write_pricing_json_object(&path, &root)?;
        self.override_sig = None;
        self.refresh();
        Ok(())
    }

    /// Removes a custom model price rule from local `pricing.json` so it reverts to built-in/unpriced.
    pub fn remove_custom_rule(&mut self, raw_model: &str) -> Result<bool, String> {
        let key = raw_model.trim();
        if key.is_empty() {
            return Ok(false);
        }
        let path = self
            .override_path
            .clone()
            .ok_or_else(|| "未配置 pricing.json 路径".to_string())?;
        if regular_file_sig(&path).is_none() {
            return Ok(false);
        }
        let (mut root, mut models_map) = load_pricing_json_object(&path);
        let exact_target = canonical_exact_key(key);
        let norm_target = normalize_model_key(key);

        let remove_key = models_map
            .keys()
            .find(|k| canonical_exact_key(k) == exact_target)
            .or_else(|| models_map.keys().find(|k| normalize_model_key(k) == norm_target))
            .cloned();

        let Some(k) = remove_key else {
            return Ok(false);
        };
        models_map.remove(&k);
        root.insert("models".to_string(), Value::Object(models_map));
        write_pricing_json_object(&path, &root)?;
        self.override_sig = None;
        self.refresh();
        Ok(true)
    }

    fn resolve_target(&self, model: &str) -> ResolvedRule {
        if let Some(&hit) = self.rate_cache.borrow().get(model) {
            return hit;
        }
        let target = self.resolve_target_uncached(model);
        let mut cache = self.rate_cache.borrow_mut();
        if cache.len() < RATE_CACHE_MAX {
            cache.insert(model.to_string(), target);
        }
        target
    }

    fn resolve_target_uncached(&self, model: &str) -> ResolvedRule {
        if model.is_empty() || model == "unknown" || model == "compaction" || model == "(上下文压缩)" {
            return ResolvedRule::Unpriced;
        }
        let exact = canonical_exact_key(model);
        let norm = strip_effort_suffix_slice(&exact);
        if norm.is_empty() || norm == "unknown" || norm == "compaction" {
            return ResolvedRule::Unpriced;
        }

        // 1. Exact custom key match (preserves effort suffix like `gemini-4-neon-xhigh`).
        if let Some((idx, _)) = self
            .custom_rules
            .iter()
            .enumerate()
            .find(|(_, r)| r.exact_key == exact)
        {
            return ResolvedRule::CustomExact(idx);
        }

        // 2. Prefix/family custom match (longest matching prefix wins; skip rules explicitly scoped to an effort suffix).
        let mut best_custom: Option<(usize, usize)> = None;
        for (idx, rule) in self.custom_rules.iter().enumerate() {
            if rule.has_effort_suffix {
                continue;
            }
            if matches_model_prefix(norm, &rule.prefix) {
                let take = match best_custom {
                    None => true,
                    Some((_, prev_len)) => rule.prefix.len() > prev_len,
                };
                if take {
                    best_custom = Some((idx, rule.prefix.len()));
                }
            }
        }
        if let Some((idx, _)) = best_custom {
            return ResolvedRule::CustomPrefix(idx);
        }

        // 3. Built-in public API catalog.
        for (idx, spec) in BUILTIN_SPECS.iter().enumerate() {
            for &p in spec.prefixes {
                if matches_model_prefix(norm, p) {
                    return ResolvedRule::Builtin(idx);
                }
            }
        }
        ResolvedRule::Unpriced
    }

    pub fn resolve_detail(
        &self,
        model: &str,
        total_input_tokens: u64,
        date: Option<NaiveDate>,
    ) -> Option<(ModelRate, PriceSource, String)> {
        match self.resolve_target(model) {
            ResolvedRule::CustomExact(idx) => {
                let rule = &self.custom_rules[idx];
                Some((
                    rule.resolve(total_input_tokens),
                    PriceSource::CustomExact,
                    rule.raw_key.clone(),
                ))
            }
            ResolvedRule::CustomPrefix(idx) => {
                let rule = &self.custom_rules[idx];
                Some((
                    rule.resolve(total_input_tokens),
                    PriceSource::CustomPrefix,
                    rule.raw_key.clone(),
                ))
            }
            ResolvedRule::Builtin(idx) => {
                let spec = &BUILTIN_SPECS[idx];
                Some((
                    spec.resolve(total_input_tokens, date),
                    PriceSource::Builtin,
                    spec.display.to_string(),
                ))
            }
            ResolvedRule::Unpriced => None,
        }
    }

    pub fn resolve_rate(&self, model: &str, total_input_tokens: u64, date: Option<NaiveDate>) -> Option<ModelRate> {
        match self.resolve_target(model) {
            ResolvedRule::CustomExact(idx) | ResolvedRule::CustomPrefix(idx) => {
                Some(self.custom_rules[idx].resolve(total_input_tokens))
            }
            ResolvedRule::Builtin(idx) => Some(BUILTIN_SPECS[idx].resolve(total_input_tokens, date)),
            ResolvedRule::Unpriced => None,
        }
    }

    pub fn price_turn(
        &self,
        model: &str,
        input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
        date: Option<NaiveDate>,
    ) -> CostBreakdown {
        let ctx = input_tokens
            .saturating_add(cache_read_tokens)
            .saturating_add(cache_write_tokens);
        match self.resolve_rate(model, ctx, date) {
            Some(rate) => rate.calculate(input_tokens, cache_read_tokens, cache_write_tokens, output_tokens),
            None => CostBreakdown::unpriced(),
        }
    }

    pub fn rate_label(&self, model: &str, date: Option<NaiveDate>) -> Option<String> {
        let (r, src, _) = self.resolve_detail(model, 0, date)?;
        let tag = if src.is_custom() { " [自定义]" } else { "" };
        if r.cache_write > 0.0 && (r.cache_write - r.input).abs() > 1e-6 {
            Some(format!(
                "入{} · 缓读{} · 缓写{} · 出{}/M{}",
                fmt_rate(r.input),
                fmt_rate(r.cache_read),
                fmt_rate(r.cache_write),
                fmt_rate(r.output),
                tag,
            ))
        } else {
            Some(format!(
                "入{} · 缓读{} · 出{}/M{}",
                fmt_rate(r.input),
                fmt_rate(r.cache_read),
                fmt_rate(r.output),
                tag,
            ))
        }
    }

    /// Builds the full UI pricing table: observed + custom rules first, followed by built-in references.
    pub fn build_table(&self, observed_models: &[String], date: NaiveDate) -> Vec<PricingTableRow> {
        let mut rows = Vec::new();
        let mut seen_exact: HashSet<String> = HashSet::new();
        let mut covered_builtins: HashSet<&'static str> = HashSet::new();

        // 1. Models actually observed in local transcripts (excluding compaction/unknown).
        for model in observed_models {
            let exact = canonical_exact_key(model);
            if exact.is_empty() || exact == "unknown" || exact == "compaction" || model == "(上下文压缩)" {
                continue;
            }
            if !seen_exact.insert(exact) {
                continue;
            }
            match self.resolve_detail(model, 0, Some(date)) {
                Some((rate, src, matched)) => {
                    let note = BUILTIN_SPECS
                        .iter()
                        .find(|s| s.display == matched)
                        .map(|s| s.note)
                        .unwrap_or("");
                    rows.push(PricingTableRow {
                        model: model.clone(),
                        source: src,
                        matched_rule: matched.clone(),
                        rate: Some(rate),
                        is_reference: false,
                        custom_key: if src.is_custom() { Some(matched) } else { None },
                        note: note.to_string(),
                    });
                }
                None => {
                    rows.push(PricingTableRow {
                        model: model.clone(),
                        source: PriceSource::Unpriced,
                        matched_rule: "-".to_string(),
                        rate: None,
                        is_reference: false,
                        custom_key: None,
                        note: "点击设价".to_string(),
                    });
                }
            }
        }

        // 2. Custom rules from pricing.json not already listed as exact observed models.
        let mut sorted_custom = self.custom_rules.clone();
        sorted_custom.sort_by(|a, b| a.raw_key.cmp(&b.raw_key));
        for rule in &sorted_custom {
            if seen_exact.insert(rule.exact_key.clone()) {
                let src = if rule.has_effort_suffix {
                    PriceSource::CustomExact
                } else {
                    PriceSource::CustomPrefix
                };
                for spec in BUILTIN_SPECS {
                    if spec.prefixes.iter().any(|p| *p == rule.prefix) {
                        covered_builtins.insert(spec.display);
                    }
                }
                rows.push(PricingTableRow {
                    model: rule.raw_key.clone(),
                    source: src,
                    matched_rule: rule.raw_key.clone(),
                    rate: Some(rule.base),
                    is_reference: false,
                    custom_key: Some(rule.raw_key.clone()),
                    note: "pricing.json".to_string(),
                });
            }
        }

        // 3. Remaining built-in catalog reference entries.
        for spec in BUILTIN_SPECS {
            if covered_builtins.contains(spec.display) {
                continue;
            }
            let exact = canonical_exact_key(spec.display);
            if !seen_exact.insert(exact) {
                continue;
            }
            let (rate, src, matched) = self
                .resolve_detail(spec.display, 0, Some(date))
                .unwrap_or((spec.resolve(0, Some(date)), PriceSource::Builtin, spec.display.to_string()));
            rows.push(PricingTableRow {
                model: spec.display.to_string(),
                source: src,
                matched_rule: matched.clone(),
                rate: Some(rate),
                is_reference: true,
                custom_key: if src.is_custom() { Some(matched) } else { None },
                note: spec.note.to_string(),
            });
        }

        rows
    }
}

fn round_rate(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

pub fn fmt_rate_plain(v: f64) -> String {
    let r = round_rate(v);
    let s = format!("{r:.4}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.contains('.') {
        let dec = trimmed.split('.').nth(1).map(|d| d.len()).unwrap_or(0);
        if dec < 2 {
            format!("{r:.2}")
        } else {
            trimmed.to_string()
        }
    } else {
        format!("{r:.2}")
    }
}

fn fmt_rate(v: f64) -> String {
    format!("${}", fmt_rate_plain(v))
}

const EFFORT_SUFFIXES: &[&str] = &[
    "-xhigh", "-high", "-medium", "-med", "-low", "-minimal", "-max", "-none",
];

const PROVIDER_PREFIXES: &[&str] = &[
    "us.anthropic.",
    "eu.anthropic.",
    "global.anthropic.",
    "anthropic.",
];

/// Canonical lowercase key keeping effort suffixes intact (for exact per-model matching).
/// Strips path prefixes (`org/model`), Bedrock/Vertex provider prefixes (`us.anthropic.`, etc.),
/// and normalizes `@`, `.`, `_` separators to `-`.
pub fn canonical_exact_key(raw: &str) -> String {
    let mut s = raw.trim().to_ascii_lowercase();
    if let Some((_, after)) = s.rsplit_once('/') {
        s = after.to_string();
    }
    for pfx in PROVIDER_PREFIXES {
        if let Some(stripped) = s.strip_prefix(pfx) {
            s = stripped.to_string();
            break;
        }
    }
    s.replace(['@', '.', '_'], "-")
}

fn strip_effort_suffix_slice(exact: &str) -> &str {
    for suffix in EFFORT_SUFFIXES {
        if let Some(stripped) = exact.strip_suffix(suffix) {
            if !stripped.is_empty() {
                return stripped;
            }
        }
    }
    exact
}

/// Normalize model names to lowercase hyphen-separated base tokens for family/prefix matching.
pub fn normalize_model_key(raw: &str) -> String {
    let exact = canonical_exact_key(raw);
    let stripped = strip_effort_suffix_slice(&exact);
    if stripped.len() == exact.len() {
        exact
    } else {
        stripped.to_string()
    }
}

fn matches_model_prefix(norm: &str, prefix: &str) -> bool {
    if norm == prefix {
        return true;
    }
    if let Some(rest) = norm.strip_prefix(prefix) {
        return rest.starts_with('-');
    }
    false
}

fn regular_file_sig(path: &Path) -> Option<FileSig> {
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

fn load_custom_rules(path: &Path) -> Vec<CustomRateRule> {
    let Ok(mut fh) = File::open(path) else {
        return Vec::new();
    };
    let mut text = String::new();
    if fh.read_to_string(&mut text).is_err() {
        return Vec::new();
    }
    parse_custom_rules_json(&text)
}

fn parse_custom_rules_json(text: &str) -> Vec<CustomRateRule> {
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let map = root
        .get("models")
        .and_then(Value::as_object)
        .unwrap_or(&root);

    let mut rules = Vec::new();
    for (k, v) in map {
        let Some(obj) = v.as_object() else { continue };
        let Some(base) = parse_rate_obj(obj) else { continue };
        let exact_key = canonical_exact_key(k);
        let prefix = normalize_model_key(k);
        if prefix.is_empty() {
            continue;
        }
        let has_effort_suffix = exact_key != prefix;
        let long_threshold = obj.get("long_context_threshold").and_then(Value::as_u64);
        let long_rate = obj
            .get("long_context")
            .and_then(Value::as_object)
            .and_then(parse_rate_obj);
        rules.push(CustomRateRule {
            raw_key: k.trim().to_string(),
            exact_key,
            prefix,
            has_effort_suffix,
            base,
            long_threshold,
            long_rate,
        });
    }
    rules
}

fn load_pricing_json_object(path: &Path) -> (Map<String, Value>, Map<String, Value>) {
    if regular_file_sig(path).is_none() {
        return (Map::new(), Map::new());
    }
    if let Ok(text) = fs::read_to_string(path) {
        if let Ok(Value::Object(mut root)) = serde_json::from_str::<Value>(&text) {
            if let Some(Value::Object(models)) = root.remove("models") {
                return (root, models);
            }
            // Legacy flat format: migrate keys that look like rate objects into "models".
            let mut models = Map::new();
            let mut keep_root = Map::new();
            for (k, v) in root {
                if v.as_object().and_then(parse_rate_obj).is_some() {
                    models.insert(k, v);
                } else {
                    keep_root.insert(k, v);
                }
            }
            return (keep_root, models);
        }
    }
    (Map::new(), Map::new())
}

fn write_pricing_json_object(path: &Path, root: &Map<String, Value>) -> Result<(), String> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err("拒绝写入符号链接 pricing.json".to_string());
        }
    }
    let payload = serde_json::to_string_pretty(&Value::Object(root.clone()))
        .map_err(|e| format!("序列化失败: {e}"))?;
    crate::quota::atomic_write_json(path, &format!("{payload}\n"))
}

fn parse_rate_obj(obj: &serde_json::Map<String, Value>) -> Option<ModelRate> {
    let input = json_non_neg_f64(obj.get("input"))?;
    let output = json_non_neg_f64(obj.get("output"))?;
    let cache_read = json_non_neg_f64(obj.get("cache_read")).unwrap_or(0.0);
    let cache_write = json_non_neg_f64(obj.get("cache_write")).unwrap_or(input);
    Some(ModelRate::new(input, cache_read, cache_write, output))
}

fn json_non_neg_f64(v: Option<&Value>) -> Option<f64> {
    let f = v?.as_f64()?;
    if f.is_finite() && f >= 0.0 {
        Some(f)
    } else {
        None
    }
}

// ---------- USD Formatting ----------

fn fmt_usd_with_commas(v: f64) -> String {
    let rounded = (v * 100.0).round() as u64;
    let dollars = rounded / 100;
    let cents = rounded % 100;
    let d_str = crate::stats::fmt_commas(dollars);
    format!("${d_str}.{cents:02}")
}

pub fn fmt_usd_compact(v: f64) -> String {
    if !v.is_finite() || v <= 0.0 {
        return "$0.00".to_string();
    }
    if v >= 10_000.0 {
        format!("${:.1}K", v / 1_000.0)
    } else if v >= 1_000.0 {
        format!("${:.2}K", v / 1_000.0)
    } else if v >= 0.01 {
        format!("${v:.2}")
    } else if v >= 0.001 {
        format!("${v:.3}")
    } else {
        "<$0.001".to_string()
    }
}

pub fn fmt_usd_precise(v: f64) -> String {
    if !v.is_finite() || v <= 0.0 {
        return "$0.00".to_string();
    }
    if v >= 1_000.0 {
        fmt_usd_with_commas(v)
    } else if v >= 0.01 {
        format!("${v:.2}")
    } else if v >= 0.0001 {
        format!("${v:.4}")
    } else {
        "<$0.0001".to_string()
    }
}

pub fn fmt_usd(v: f64, human: bool) -> String {
    if human {
        fmt_usd_compact(v)
    } else {
        fmt_usd_precise(v)
    }
}

pub fn fmt_cost(cost: &CostBreakdown, calls: u64, human: bool) -> String {
    if calls == 0 {
        return "$0.00".to_string();
    }
    if cost.is_fully_unpriced(calls) {
        return "-".to_string();
    }
    let mut s = fmt_usd(cost.total_usd(), human);
    if cost.has_unpriced() {
        s.push('*');
    }
    s
}

pub fn fmt_cost_compact(cost: &CostBreakdown, calls: u64) -> String {
    fmt_cost(cost, calls, true)
}

pub fn fmt_usd_axis(v: f64, max_v: f64) -> String {
    if !v.is_finite() || v <= 0.0 {
        return "$0".to_string();
    }
    if max_v >= 1_000.0 {
        format!("${:.1}K", v / 1_000.0)
    } else if max_v >= 100.0 {
        format!("${:.0}", v)
    } else if max_v >= 10.0 {
        format!("${:.1}", v)
    } else if max_v >= 0.10 {
        format!("${:.2}", v)
    } else {
        format!("${:.3}", v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_rates_and_normalization() {
        let cat = PricingCatalog::new(None);
        let d_nov = NaiveDate::from_ymd_opt(2026, 11, 22);
        let d_dec = NaiveDate::from_ymd_opt(2026, 12, 5);

        let neon = cat.resolve_rate("gemini-4-neon-xhigh", 500_000, d_nov).unwrap();
        assert_eq!(neon, ModelRate::new(3.0, 0.15, 3.0, 15.0));

        let neon_hi = cat.resolve_rate("gemini-4-neon-high", 10_000, d_nov).unwrap();
        assert_eq!(neon_hi, neon);

        // Carbon intro vs standard by date
        let carb_nov = cat.resolve_rate("gemini-4-carbon-high", 100_000, d_nov).unwrap();
        assert_eq!(carb_nov, ModelRate::new(2.0, 0.10, 2.0, 10.0));
        let carb_dec = cat.resolve_rate("gemini-4-carbon-high", 100_000, d_dec).unwrap();
        assert_eq!(carb_dec, ModelRate::new(4.0, 0.20, 4.0, 20.0));

        // Claude Opus 5.5
        let opus = cat.resolve_rate("claude-opus-5-5-high", 240_000, d_nov).unwrap();
        assert_eq!(opus, ModelRate::new(4.0, 0.20, 5.0, 20.0));

        // Gemini 3.1 Pro long context tier (>200K)
        let pro_short = cat.resolve_rate("gemini-3.1-pro-preview", 150_000, d_nov).unwrap();
        let pro_long = cat.resolve_rate("gemini-3.1-pro-preview", 250_000, d_nov).unwrap();
        assert_eq!(pro_short.input, 2.0);
        assert_eq!(pro_long.input, 4.0);

        // Unknown model returns None
        assert!(cat.resolve_rate("mystery-model-9", 10_000, d_nov).is_none());
    }

    #[test]
    fn test_cost_calculation_and_cache_savings() {
        let cat = PricingCatalog::new(None);
        let c = cat.price_turn("gemini-4-neon-xhigh", 1_000_000, 10_000_000, 0, 500_000, None);
        assert!((c.input_usd - 3.0).abs() < 1e-9);
        assert!((c.cache_read_usd - 1.5).abs() < 1e-9);
        assert!((c.output_usd - 7.5).abs() < 1e-9);
        assert!((c.total_usd() - 12.0).abs() < 1e-9);
        assert!((c.non_cached_usd() - 10.5).abs() < 1e-9);
        assert!((c.cache_saved_usd - 28.5).abs() < 1e-9);
        assert_eq!(c.unpriced_calls, 0);

        let unk = cat.price_turn("unknown-x", 10_000, 0, 0, 5_000, None);
        assert_eq!(unk.unpriced_calls, 1);
        assert_eq!(unk.total_usd(), 0.0);
    }

    #[test]
    fn test_exact_vs_prefix_custom_override_and_persistence() {
        let path = std::env::temp_dir().join(format!("agy_gauge_pricing_rw_{}.json", std::process::id()));
        let _ = fs::remove_file(&path);

        let mut cat = PricingCatalog::new(Some(path.clone()));
        // Set family prefix override
        cat.upsert_custom_rule("gemini-4-neon", ModelRate::new(2.5, 0.12, 2.5, 12.5))
            .unwrap();
        // Set exact effort-specific override
        cat.upsert_custom_rule("gemini-4-neon-xhigh", ModelRate::new(6.0, 0.30, 6.0, 30.0))
            .unwrap();

        // Exact wins for xhigh, prefix applies to high
        let (rx, sx, _) = cat.resolve_detail("gemini-4-neon-xhigh", 0, None).unwrap();
        let (rh, sh, _) = cat.resolve_detail("gemini-4-neon-high", 0, None).unwrap();
        assert_eq!(sx, PriceSource::CustomExact);
        assert_eq!(rx.input, 6.0);
        assert_eq!(sh, PriceSource::CustomPrefix);
        assert_eq!(rh.input, 2.5);

        // Table includes observed + custom + remaining built-in references
        let d = NaiveDate::from_ymd_opt(2026, 11, 22).unwrap();
        let table = cat.build_table(&["gemini-4-neon-xhigh".into(), "brand-new-model".into()], d);
        assert_eq!(table[0].model, "gemini-4-neon-xhigh");
        assert_eq!(table[0].source, PriceSource::CustomExact);
        assert_eq!(table[1].model, "brand-new-model");
        assert_eq!(table[1].source, PriceSource::Unpriced);
        assert!(table.iter().any(|r| r.is_reference && r.model == "claude-opus-5-5"));

        // Removing exact rule reverts xhigh to prefix rule; removing prefix reverts to built-in $3
        assert!(cat.remove_custom_rule("gemini-4-neon-xhigh").unwrap());
        assert_eq!(cat.resolve_rate("gemini-4-neon-xhigh", 0, None).unwrap().input, 2.5);
        assert!(cat.remove_custom_rule("gemini-4-neon").unwrap());
        assert_eq!(cat.resolve_rate("gemini-4-neon-xhigh", 0, None).unwrap().input, 3.0);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_custom_pricing_json_override() {
        let json = r#"{
            "models": {
                "gemini-4-neon": { "input": 1.0, "cache_read": 0.05, "output": 5.0 },
                "my-private-model": {
                    "input": 2.0,
                    "cache_read": 0.2,
                    "output": 8.0,
                    "long_context_threshold": 100000,
                    "long_context": { "input": 4.0, "cache_read": 0.4, "output": 16.0 }
                }
            }
        }"#;
        let rules = parse_custom_rules_json(json);
        let cat = PricingCatalog {
            override_path: None,
            override_sig: None,
            custom_rules: rules,
            rate_cache: RefCell::new(HashMap::new()),
        };
        let r1 = cat.resolve_rate("gemini-4-neon-xhigh", 10_000, None).unwrap();
        assert_eq!(r1.input, 1.0);
        assert_eq!(r1.output, 5.0);

        let r2_s = cat.resolve_rate("my-private-model-high", 50_000, None).unwrap();
        let r2_l = cat.resolve_rate("my-private-model-high", 150_000, None).unwrap();
        assert_eq!(r2_s.input, 2.0);
        assert_eq!(r2_l.input, 4.0);
    }

    #[test]
    fn test_usd_formatting() {
        assert_eq!(fmt_usd_compact(0.0), "$0.00");
        assert_eq!(fmt_usd_compact(0.0004), "<$0.001");
        assert_eq!(fmt_usd_compact(0.0042), "$0.004");
        assert_eq!(fmt_usd_compact(12.345), "$12.35");
        assert_eq!(fmt_usd_compact(1480.0), "$1.48K");
        assert_eq!(fmt_usd_precise(1480.5), "$1,480.50");
        assert_eq!(fmt_usd_precise(0.0004), "$0.0004");
        assert_eq!(fmt_usd_axis(0.0, 5.0), "$0");
        assert_eq!(fmt_usd_axis(2.5, 5.0), "$2.50");
        assert_eq!(fmt_usd_axis(125.0, 250.0), "$125");
        assert_eq!(fmt_rate_plain(0.075), "0.075");
        assert_eq!(fmt_rate_plain(3.0), "3.00");
    }

    #[test]
    fn test_openai_codex_and_gemini2_builtin_rates_and_prefix_ordering() {
        let cat = PricingCatalog::new(None);
        let cases: &[(&str, &str, ModelRate)] = &[
            // OpenAI / Codex GPT-5 family — none may be shadowed by "gpt-5"
            ("gpt-5.4", "gpt-5.4", ModelRate::new(2.50, 0.25, 2.50, 15.00)),
            ("gpt-5.4-2026-03-05", "gpt-5.4", ModelRate::new(2.50, 0.25, 2.50, 15.00)),
            ("gpt-5.3-codex", "gpt-5.3-codex", ModelRate::new(1.75, 0.175, 1.75, 14.00)),
            ("gpt-5.3-codex-high", "gpt-5.3-codex", ModelRate::new(1.75, 0.175, 1.75, 14.00)),
            ("gpt-5.2-codex", "gpt-5.2-codex", ModelRate::new(1.75, 0.175, 1.75, 14.00)),
            ("gpt-5.2", "gpt-5.2", ModelRate::new(1.75, 0.175, 1.75, 14.00)),
            ("gpt-5.2-2025-12-11", "gpt-5.2", ModelRate::new(1.75, 0.175, 1.75, 14.00)),
            ("gpt-5.1-codex-mini", "gpt-5.1-codex-mini", ModelRate::new(0.25, 0.025, 0.25, 2.00)),
            ("gpt-5.1-codex", "gpt-5.1-codex", ModelRate::new(1.25, 0.125, 1.25, 10.00)),
            ("gpt-5.1", "gpt-5.1", ModelRate::new(1.25, 0.125, 1.25, 10.00)),
            ("gpt-5-codex", "gpt-5-codex", ModelRate::new(1.25, 0.125, 1.25, 10.00)),
            ("gpt-5-mini", "gpt-5-mini", ModelRate::new(0.25, 0.025, 0.25, 2.00)),
            ("gpt-5-nano", "gpt-5-nano", ModelRate::new(0.05, 0.005, 0.05, 0.40)),
            ("gpt-5", "gpt-5", ModelRate::new(1.25, 0.125, 1.25, 10.00)),
            ("gpt-5-2025-08-07", "gpt-5", ModelRate::new(1.25, 0.125, 1.25, 10.00)),
            ("codex-mini-latest", "codex-mini-latest", ModelRate::new(1.50, 0.375, 1.50, 6.00)),
            ("codex-mini-2025-05-16", "codex-mini-latest", ModelRate::new(1.50, 0.375, 1.50, 6.00)),
            // OpenAI GPT-4.1 family — mini/nano must not be shadowed by gpt-4.1
            ("gpt-4.1-mini", "gpt-4.1-mini", ModelRate::new(0.40, 0.10, 0.40, 1.60)),
            ("gpt-4.1-nano", "gpt-4.1-nano", ModelRate::new(0.10, 0.025, 0.10, 0.40)),
            ("gpt-4.1", "gpt-4.1", ModelRate::new(2.00, 0.50, 2.00, 8.00)),
            ("gpt-4.1-2025-04-14", "gpt-4.1", ModelRate::new(2.00, 0.50, 2.00, 8.00)),
            // Gemini 2.0 family — flash-lite must not be shadowed by flash
            ("gemini-2.0-flash-lite", "gemini-2.0-flash-lite", ModelRate::new(0.075, 0.01875, 0.075, 0.30)),
            ("gemini-2.0-flash-lite-001", "gemini-2.0-flash-lite", ModelRate::new(0.075, 0.01875, 0.075, 0.30)),
            ("gemini-2.0-flash", "gemini-2.0-flash", ModelRate::new(0.10, 0.025, 0.10, 0.40)),
            ("gemini-2.0-flash-001", "gemini-2.0-flash", ModelRate::new(0.10, 0.025, 0.10, 0.40)),
        ];

        for &(model, expected_rule, expected_rate) in cases {
            let (rate, src, matched) = cat
                .resolve_detail(model, 50_000, None)
                .unwrap_or_else(|| panic!("expected builtin rate for {model}"));
            assert_eq!(src, PriceSource::Builtin, "source for {model}");
            assert_eq!(matched, expected_rule, "matched rule for {model}");
            assert_eq!(rate, expected_rate, "rate for {model}");
        }
    }

    #[test]
    fn test_bedrock_and_vertex_model_normalization() {
        assert_eq!(
            canonical_exact_key("us.anthropic.claude-sonnet-4-5-20250929-v1:0"),
            "claude-sonnet-4-5-20250929-v1:0"
        );
        assert_eq!(
            canonical_exact_key("EU.Anthropic.claude-opus-4-6-v1"),
            "claude-opus-4-6-v1"
        );
        assert_eq!(
            canonical_exact_key("global.anthropic.claude-haiku-4-5-20251001-v1:0"),
            "claude-haiku-4-5-20251001-v1:0"
        );
        assert_eq!(
            canonical_exact_key("anthropic.claude-3-5-haiku-20241022-v1:0"),
            "claude-3-5-haiku-20241022-v1:0"
        );
        assert_eq!(
            canonical_exact_key("bedrock/us.anthropic.claude-sonnet-4-5-20250929-v1:0"),
            "claude-sonnet-4-5-20250929-v1:0"
        );
        assert_eq!(
            canonical_exact_key("claude-sonnet-4-5@20250929"),
            "claude-sonnet-4-5-20250929"
        );
        assert_eq!(
            canonical_exact_key("projects/p/locations/us-east5/publishers/anthropic/models/claude-opus-4-6@20260205"),
            "claude-opus-4-6-20260205"
        );

        let cat = PricingCatalog::new(None);
        let (r_sonnet, _, m_sonnet) = cat
            .resolve_detail("us.anthropic.claude-sonnet-4-5-20250929-v1:0", 10_000, None)
            .unwrap();
        assert_eq!(m_sonnet, "claude-sonnet-4");
        assert_eq!(r_sonnet, ModelRate::new(3.00, 0.30, 3.75, 15.00));

        let (r_vertex, _, m_vertex) = cat
            .resolve_detail("claude-sonnet-4-5@20250929", 10_000, None)
            .unwrap();
        assert_eq!(m_vertex, "claude-sonnet-4");
        assert_eq!(r_vertex, ModelRate::new(3.00, 0.30, 3.75, 15.00));

        let (r_eu_opus, _, m_eu_opus) = cat
            .resolve_detail("eu.anthropic.claude-opus-4-6-v1", 10_000, None)
            .unwrap();
        assert_eq!(m_eu_opus, "claude-opus-4-6");
        assert_eq!(r_eu_opus, ModelRate::new(5.00, 0.50, 6.25, 25.00));

        let (r_global_haiku, _, m_global_haiku) = cat
            .resolve_detail("global.anthropic.claude-haiku-4-5-20251001-v1:0", 10_000, None)
            .unwrap();
        assert_eq!(m_global_haiku, "claude-haiku-4-5");
        assert_eq!(r_global_haiku, ModelRate::new(1.00, 0.10, 1.25, 5.00));

        let (r_bare_anth, _, m_bare_anth) = cat
            .resolve_detail("anthropic.claude-3-5-haiku-20241022-v1:0", 10_000, None)
            .unwrap();
        assert_eq!(m_bare_anth, "claude-3-5-haiku");
        assert_eq!(r_bare_anth, ModelRate::new(0.80, 0.08, 1.00, 4.00));
    }
}
