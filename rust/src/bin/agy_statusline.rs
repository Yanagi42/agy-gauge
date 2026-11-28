//! Fast native console bridge (`agy-statusline.exe`) for Antigravity CLI `/statusline`.
//!
//! Safety & Compliance:
//!   - Receives official session/quota JSON directly from `agy` over `stdin`
//!     (per https://antigravity.google/docs/cli/statusline).
//!   - Atomically saves a PII-stripped snapshot (`statusline_state.json`) ONLY inside
//!     this executable's own directory so `gauge.exe` and `gauge_stats.py`
//!     can display official remaining quotas & reset countdowns.
//!   - Strictly read-only on `~/.gemini`; zero network requests; zero SQLite access.

use std::io::{self, Read};
use std::path::PathBuf;

use chrono::Utc;

use gauge::pricing::PricingCatalog;
use gauge::quota::{
    fmt_countdown, parse_statusline_payload_with_width, save_statusline_state, StatuslineSnapshot,
};
use gauge::stats::{fmt_compact, scan_antigravity_today_totals, IncrementalScanner};

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
                    "agy-statusline — Official Antigravity CLI /statusline bridge for Gauge\n\
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
        match parse_statusline_payload_with_width(trimmed, now) {
            Some((snap, tw)) => {
                if let Some(w) = tw {
                    term_width = w;
                }
                Some(snap)
            }
            None => None,
        }
    } else {
        None
    };

    let target_state = state_path.or_else(gauge::quota::StatuslineReader::default_path);
    if let (Some(path), Some(snap)) = (target_state.as_deref(), parsed_snap.as_ref()) {
        // Persist sanitized official quota state for gauge.exe & gauge_stats.py.
        let _ = save_statusline_state(path, snap);
    }

    let (today_tokens, today_usd, today_calls) = if skip_scan {
        (0, 0.0, 0)
    } else {
        let pricing = PricingCatalog::new(PricingCatalog::default_override_path());
        let tc = scan_antigravity_today_totals(&IncrementalScanner::default_antigravity_dirs(), &pricing);
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
    use chrono::{Duration, Local};
    use gauge::stats::SourceKind;

    #[test]
    fn test_statusline_render_and_sanitize() {
        let now = Utc::now();
        let raw = r#"{
            "terminal_width": 88,
            "email": "secret@example.com",
            "cwd": "C:\\Users\\secret\\proj",
            "model": { "display_name": "Gemini 4 Neon" },
            "context_window": { "used_percentage": 35.0 },
            "quota": {
                "gemini-5h": { "remaining_fraction": 0.72, "reset_in_seconds": 7200 }
            }
        }"#;
        let (snap, tw) = parse_statusline_payload_with_width(raw, now).unwrap();
        assert_eq!(tw, Some(88));
        let out = render_statusline(Some(&snap), 1_500_000, 2.35, 12, false, false, 120);
        assert!(out.contains("Gemini 4 Neon"));
        assert!(out.contains("Ctx 35%"));
        assert!(out.contains("Gemini 5h 72%"));
        assert!(out.contains("今日 1.50M ($2.35)"));
        assert!(!out.contains("secret"));
    }

    #[test]
    fn test_today_totals_fast_path_parity_with_full_scanner() {
        let base = std::env::temp_dir().join(format!(
            "agy_statusline_fast_parity_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let cli_dir = base.join("antigravity-cli");
        let app_dir = base.join("antigravity");

        let now_utc = Utc::now();
        let today_ts = now_utc.to_rfc3339();
        let two_days_ago_ts = (now_utc - Duration::days(2)).to_rfc3339();

        // Session 1 (CLI): multi-day session with an older turn, a today turn, and a today checkpoint
        // inheriting its model from the prior turn, plus a GENERIC tool output mentioning `_tokens`.
        let s1_logs = cli_dir
            .join("brain")
            .join("sess-multi")
            .join(".system_generated")
            .join("logs");
        std::fs::create_dir_all(&s1_logs).unwrap();
        let s1_jsonl = [
            format!(
                r#"{{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"{two_days_ago_ts}","model":"gemini-4-neon-xhigh","input_tokens":50000,"cache_read_tokens":10000,"output_tokens":500}}"#
            ),
            format!(
                r#"{{"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"{today_ts}","model":"gemini-4-neon-xhigh","input_tokens":100000,"cache_read_tokens":200000,"output_tokens":1500,"tool_calls":[{{"name":"read_file","args":{{"huge":"blob"}}}}]}}"#
            ),
            format!(
                r#"{{"step_index":3,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"{today_ts}","content":"\"input_tokens\":999999"}}"#
            ),
            format!(
                r#"{{"step_index":4,"type":"CHECKPOINT","status":"DONE","created_at":"{today_ts}","input_tokens":20000,"cache_read_tokens":50000,"output_tokens":400}}"#
            ),
        ]
        .join("\n");
        std::fs::write(s1_logs.join("transcript.jsonl"), format!("{s1_jsonl}\n")).unwrap();

        // Session 2 (App): duplicate cid with fewer turns (CLI must win deduplication), plus a unique App session today.
        let dup_logs = app_dir
            .join("brain")
            .join("sess-multi")
            .join(".system_generated")
            .join("logs");
        let s2_logs = app_dir
            .join("brain")
            .join("sess-app-only")
            .join(".system_generated")
            .join("logs");
        std::fs::create_dir_all(&dup_logs).unwrap();
        std::fs::create_dir_all(&s2_logs).unwrap();
        std::fs::write(
            dup_logs.join("transcript.jsonl"),
            format!(
                r#"{{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"{today_ts}","model":"claude-opus-5-5","input_tokens":999,"cache_read_tokens":0,"output_tokens":111}}"#
            ),
        )
        .unwrap();
        std::fs::write(
            s2_logs.join("transcript.jsonl"),
            format!(
                r#"{{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"{today_ts}","model":"claude-opus-5-5","input_tokens":10000,"cache_read_tokens":0,"output_tokens":2000}}"#
            ),
        )
        .unwrap();

        let sources = vec![(SourceKind::Cli, cli_dir), (SourceKind::App, app_dir)];
        let pricing = PricingCatalog::new(None);

        let fast = scan_antigravity_today_totals(&sources, &pricing);
        let mut scanner = IncrementalScanner::new_multi(sources, None, None, None, None, false);
        let snap = scanner.scan();
        let today_key = Local::now().date_naive().format("%Y-%m-%d").to_string();
        let full = snap.agg.by_day.get(&today_key).copied().unwrap_or_default();

        assert_eq!(fast.calls, full.calls);
        assert_eq!(fast.calls, 3);
        assert_eq!(fast.input_tokens, full.input_tokens);
        assert_eq!(fast.cache_read_tokens, full.cache_read_tokens);
        assert_eq!(fast.cache_write_tokens, full.cache_write_tokens);
        assert_eq!(fast.output_tokens, full.output_tokens);
        assert_eq!(fast.peak_context, full.peak_context);
        assert!((fast.cost.total_usd() - full.cost.total_usd()).abs() < 1e-9);

        let _ = std::fs::remove_dir_all(&base);
    }
}
