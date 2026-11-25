//! Fast native console bridge (`agy-statusline.exe`) for Antigravity CLI `/statusline`.
//!
//! Safety & Compliance:
//!   - Receives official session/quota JSON directly from `agy` over `stdin`
//!     (per https://antigravity.google/docs/cli/statusline).
//!   - Atomically saves a PII-stripped snapshot (`statusline_state.json`) ONLY inside
//!     this executable's own directory so `agy-token-monitor.exe` and `agy_token_stats.py`
//!     can display official remaining quotas & reset countdowns.
//!   - Strictly read-only on `~/.gemini`; zero network requests; zero SQLite access.

use std::io::{self, Read};
use std::path::PathBuf;

use chrono::{Local, Utc};

use agy_gauge::pricing::{fmt_cost_compact, PricingCatalog};
use agy_gauge::quota::{
    fmt_countdown, parse_statusline_payload, save_statusline_state, BudgetStore,
    StatuslineSnapshot,
};
use agy_gauge::stats::{fmt_compact, IncrementalScanner};

fn print_config_snippet() {
    let exe_str = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "agy-statusline.exe".to_string());
    let escaped = exe_str.replace('\\', "\\\\");
    println!(
        "{{\n  \"statusLine\": {{\n    \"type\": \"command\",\n    \"command\": \"\\\"{}\\\"\",\n    \"stack_with_default\": false\n  }}\n}}",
        escaped
    );
    eprintln!(
        "\n提示：请将上述片段手动合入 ~/.gemini/antigravity-cli/settings.json（本工具永不自动修改 ~/.gemini 目录），\n或在 agy TUI 内运行：/statusline \"{}\"",
        exe_str
    );
}

fn ansi(s: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn render_statusline(
    snap: Option<&StatuslineSnapshot>,
    today_tokens: u64,
    today_usd: f64,
    today_calls: u64,
    compact: bool,
    color: bool,
    term_width: usize,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    let now = Utc::now();

    if let Some(sl) = snap {
        if !sl.model_display.is_empty() {
            parts.push(ansi(&sl.model_display, "1;37", color));
        }
        if let Some(pct) = sl.context_used_pct {
            let col = if pct >= 80.0 {
                "31"
            } else if pct >= 55.0 {
                "33"
            } else {
                "36"
            };
            parts.push(ansi(&format!("Ctx {:.0}%", pct), col, color));
        }

        let max_buckets = if compact || term_width < 95 { 1 } else { 2 };
        for b in sl.buckets.iter().filter(|b| !b.disabled).take(max_buckets) {
            if let Some(rem_pct) = b.remaining_pct() {
                let col = if rem_pct <= 10.0 {
                    "1;31"
                } else if rem_pct <= 25.0 {
                    "33"
                } else {
                    "32"
                };
                let rem_t = b
                    .live_remaining_secs(sl.captured_at_utc, now)
                    .map(|s| format!(" ({})", fmt_countdown(s)))
                    .unwrap_or_default();
                let pace_mark = match b.pace_delta_pct(sl.captured_at_utc, now) {
                    Some(d) if d >= 18.0 => " ▲",
                    Some(d) if d >= 8.0 => " ▸",
                    _ => "",
                };
                let seg = if compact {
                    format!("{} {:.0}%{}", b.label, rem_pct, pace_mark)
                } else {
                    format!("{} {:.0}%{}{}", b.label, rem_pct, rem_t, pace_mark)
                };
                parts.push(ansi(&seg, col, color));
            }
        }
    }

    if today_calls > 0 || today_tokens > 0 {
        let cost_txt = if today_usd > 0.0 {
            format!(" (${:.2})", today_usd)
        } else {
            String::new()
        };
        parts.push(ansi(
            &format!("今日 {}{}", fmt_compact(today_tokens), cost_txt),
            "2;37",
            color,
        ));
    } else if let Some(usd) = snap.and_then(|s| s.official_cost_usd) {
        parts.push(ansi(&format!("会话 ${:.2}", usd), "2;37", color));
    }

    if parts.is_empty() {
        "AGY ready".to_string()
    } else {
        parts.join(&ansi(" · ", "90", color))
    }
}

fn main() {
    let mut compact = false;
    let mut no_color = false;
    let mut print_config = false;
    let mut state_path: Option<PathBuf> = None;
    let mut skip_scan = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--compact" => compact = true,
            "--no-color" => no_color = true,
            "--print-config" => print_config = true,
            "--no-scan" => skip_scan = true,
            "--state" => {
                if let Some(v) = args.next() {
                    state_path = Some(PathBuf::from(v));
                }
            }
            "--help" | "-h" => {
                println!(
                    "agy-statusline — Official Antigravity CLI /statusline bridge for agy-gauge\n\
                     \n\
                     Usage:\n\
                       agy-statusline.exe [--compact] [--no-color] [--state <path>] [--no-scan]\n\
                       agy-statusline.exe --print-config\n"
                );
                return;
            }
            _ => {}
        }
    }

    if print_config {
        print_config_snippet();
        return;
    }

    let mut raw_stdin = String::new();
    let _ = io::stdin().read_to_string(&mut raw_stdin);
    let trimmed = raw_stdin.trim();

    let now = Utc::now();
    let mut term_width: usize = 110;
    let parsed_snap = if !trimmed.is_empty() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(tw) = v.get("terminal_width").and_then(|x| x.as_u64()) {
                term_width = (tw as usize).clamp(40, 300);
            }
        }
        parse_statusline_payload(trimmed, now)
    } else {
        None
    };

    let target_state = state_path.or_else(agy_gauge::quota::StatuslineReader::default_path);
    if let (Some(path), Some(snap)) = (target_state.as_deref(), parsed_snap.as_ref()) {
        // Persist sanitized official quota state for agy-token-monitor.exe & agy_token_stats.py.
        let _ = save_statusline_state(path, snap);
    }

    let (today_tokens, today_usd, today_calls) = if skip_scan {
        (0, 0.0, 0)
    } else {
        let mut scanner = IncrementalScanner::new_multi(
            IncrementalScanner::default_antigravity_dirs(),
            PricingCatalog::default_override_path(),
            None,
            BudgetStore::default_path(),
            None,
            false,
        );
        let snap = scanner.scan();
        let today_key = Local::now().date_naive().format("%Y-%m-%d").to_string();
        let tc = snap.agg.by_day.get(&today_key).copied().unwrap_or_default();
        let _ = fmt_cost_compact(&tc.cost, tc.calls);
        (tc.total_tokens(), tc.cost.total_usd(), tc.calls)
    };

    let line = render_statusline(
        parsed_snap.as_ref(),
        today_tokens,
        today_usd,
        today_calls,
        compact,
        !no_color,
        term_width,
    );
    println!("{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_statusline_render_and_sanitize() {
        let now = Utc::now();
        let raw = r#"{
            "email": "secret@example.com",
            "cwd": "C:\\Users\\secret\\proj",
            "model": { "display_name": "Gemini 4 Neon" },
            "context_window": { "used_percentage": 35.0 },
            "quota": {
                "gemini-5h": { "remaining_fraction": 0.72, "reset_in_seconds": 7200 }
            }
        }"#;
        let snap = parse_statusline_payload(raw, now).unwrap();
        let out = render_statusline(Some(&snap), 1_500_000, 2.35, 12, false, false, 120);
        assert!(out.contains("Gemini 4 Neon"));
        assert!(out.contains("Ctx 35%"));
        assert!(out.contains("Gemini 5h 72%"));
        assert!(out.contains("今日 1.50M ($2.35)"));
        assert!(!out.contains("secret"));
    }
}
