//! Lightweight local usage archive (`archive.json`) stored ONLY in the application's own
//! directory so history survives when Antigravity prunes old sessions from `~/.gemini`.
//!
//! Safety & Privacy:
//!   - NEVER writes inside `~/.gemini`.
//!   - Stores only compact turn telemetry (timestamp, model, token counters, tool names,
//!     thinking length) and session metadata — never raw prompts or file contents.
//!   - Live sessions currently on disk ALWAYS take precedence over archived entries,
//!     guaranteeing zero double-counting.

use chrono::{DateTime, Local, Utc};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::pricing::{CostBreakdown, PricingCatalog};
use crate::quota::{atomic_write_json, default_companion_path};
use crate::stats::{
    is_compaction_model, price_turn_record, AgentFamily, SessionInfo, SourceKind, TokenCounts,
    TurnRecord,
};

#[derive(Debug, Clone)]
pub struct ArchivedSession {
    pub conversation_id: String,
    pub title: String,
    pub workspace: String,
    pub source: SourceKind,
    pub is_subagent: bool,
    pub parent_id: Option<String>,
    pub parent_title: String,
    pub turns: Vec<TurnRecord>,
}

pub struct UsageArchive {
    path: Option<PathBuf>,
    enabled: bool,
    entries: HashMap<(AgentFamily, String), ArchivedSession>,
    dirty: bool,
}

impl UsageArchive {
    pub fn new(path: Option<PathBuf>, enabled: bool) -> Self {
        let mut a = Self {
            path,
            enabled,
            entries: HashMap::new(),
            dirty: false,
        };
        if enabled {
            a.load();
        }
        a
    }

    pub fn default_path() -> Option<PathBuf> {
        default_companion_path("archive.json")
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn load(&mut self) {
        let Some(path) = &self.path else { return };
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return;
            }
        } else {
            return;
        }
        let Ok(mut f) = File::open(path) else { return };
        let mut text = String::new();
        if f.read_to_string(&mut text).is_err() {
            return;
        }
        let Ok(Value::Object(root)) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let Some(Value::Object(sessions)) = root.get("sessions") else {
            return;
        };
        for (raw_key, val) in sessions {
            if let Some(entry) = parse_archived_session(raw_key, val) {
                let key = (entry.source.agent_family(), entry.conversation_id.clone());
                self.entries.insert(key, entry);
            }
        }
    }

    /// Merges live sessions into the archive, persists if updated, and returns any archived-only
    /// sessions (re-priced using the active `PricingCatalog`) that no longer exist on disk.
    pub fn sync_and_backfill(
        &mut self,
        live_sessions: &[SessionInfo],
        pricing: &PricingCatalog,
        allow_persist: bool,
    ) -> Vec<SessionInfo> {
        if !self.enabled {
            return Vec::new();
        }
        let mut live_keys = BTreeSet::new();
        for s in live_sessions {
            let family = s.source.agent_family();
            let key = (family, s.conversation_id.clone());
            live_keys.insert((family, s.conversation_id.as_str()));
            let should_update = match self.entries.get(&key) {
                None => true,
                Some(prev) => {
                    prev.turns.len() != s.turns.len()
                        || prev.title != s.title
                        || prev.workspace != s.workspace
                        || prev.parent_title != s.parent_title
                        || prev.source != s.source
                        || prev.turns.last().map(|t| (
                            t.output_tokens,
                            t.input_tokens,
                            t.cache_read_tokens,
                            t.cache_write_tokens,
                            t.cache_write_1h_tokens,
                            t.reasoning_tokens,
                        )) != s.turns.last().map(|t| (
                            t.output_tokens,
                            t.input_tokens,
                            t.cache_read_tokens,
                            t.cache_write_tokens,
                            t.cache_write_1h_tokens,
                            t.reasoning_tokens,
                        ))
                }
            };
            if should_update {
                self.entries.insert(
                    key,
                    ArchivedSession {
                        conversation_id: s.conversation_id.clone(),
                        title: s.title.clone(),
                        workspace: s.workspace.clone(),
                        source: s.source,
                        is_subagent: s.is_subagent,
                        parent_id: s.parent_id.clone(),
                        parent_title: s.parent_title.clone(),
                        turns: s.turns.clone(),
                    },
                );
                self.dirty = true;
            }
        }

        if allow_persist && self.dirty {
            if self.persist().is_ok() {
                self.dirty = false;
            }
        }

        let mut backfilled = Vec::new();
        for ((family, cid), arch) in &self.entries {
            if live_keys.contains(&(*family, cid.as_str())) || arch.turns.is_empty() {
                continue;
            }
            backfilled.push(materialize_archived_session(arch, pricing));
        }
        backfilled
    }

    fn persist(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut sessions_map = Map::new();
        let mut keys: Vec<&(AgentFamily, String)> = self.entries.keys().collect();
        keys.sort();
        for k in keys {
            let s = &self.entries[k];
            let turns_json: Vec<Value> = s
                .turns
                .iter()
                .map(|t| {
                    let mut to = Map::new();
                    to.insert("step".into(), json!(t.step_index));
                    if let Some(ts) = t.timestamp_utc {
                        to.insert("ts".into(), json!(ts.to_rfc3339()));
                    }
                    to.insert("m".into(), json!(t.model));
                    if t.is_checkpoint {
                        to.insert("cp".into(), json!(true));
                    }
                    to.insert("in".into(), json!(t.input_tokens));
                    to.insert("cr".into(), json!(t.cache_read_tokens));
                    if t.cache_write_tokens > 0 {
                        to.insert("cw".into(), json!(t.cache_write_tokens));
                    }
                    if t.cache_write_1h_tokens > 0 {
                        to.insert("cw1h".into(), json!(t.cache_write_1h_tokens));
                    }
                    to.insert("out".into(), json!(t.output_tokens));
                    if t.reasoning_tokens > 0 {
                        to.insert("rt".into(), json!(t.reasoning_tokens));
                    }
                    if !t.tool_names.is_empty() {
                        to.insert("tools".into(), json!(t.tool_names));
                    }
                    if t.thinking_chars > 0 {
                        to.insert("th".into(), json!(t.thinking_chars));
                    }
                    if let Some(g) = t.gen_secs {
                        to.insert("gen".into(), json!(g));
                    }
                    Value::Object(to)
                })
                .collect();

            let mut so = Map::new();
            let json_key = if s.source.agent_family() == AgentFamily::Antigravity {
                s.conversation_id.clone()
            } else {
                so.insert("conversation_id".into(), json!(s.conversation_id));
                format!("{}:{}", s.source.agent_family().as_str(), s.conversation_id)
            };
            so.insert("title".into(), json!(s.title));
            so.insert("workspace".into(), json!(s.workspace));
            so.insert("source".into(), json!(s.source.as_str()));
            if s.is_subagent {
                so.insert("is_subagent".into(), json!(true));
            }
            if let Some(pid) = &s.parent_id {
                so.insert("parent_id".into(), json!(pid));
            }
            if !s.parent_title.is_empty() {
                so.insert("parent_title".into(), json!(s.parent_title));
            }
            so.insert("turns".into(), Value::Array(turns_json));
            sessions_map.insert(json_key, Value::Object(so));
        }

        let root = json!({
            "schema_version": 1,
            "updated_at": Utc::now().to_rfc3339(),
            "sessions": Value::Object(sessions_map),
        });
        let text = serde_json::to_string(&root).map_err(|e| format!("序列化归档失败: {e}"))?;
        atomic_write_json(path, &text)
    }
}

fn parse_archived_session(raw_key: &str, val: &Value) -> Option<ArchivedSession> {
    let obj = val.as_object()?;
    let title = obj
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("(已归档会话)")
        .to_string();
    let workspace = obj
        .get("workspace")
        .and_then(Value::as_str)
        .unwrap_or("(未知工作区)")
        .to_string();
    let source = obj
        .get("source")
        .and_then(Value::as_str)
        .map(SourceKind::from_str_lossy)
        .unwrap_or(SourceKind::Cli);
    let conversation_id = obj
        .get("conversation_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| {
            let prefix = format!("{}:", source.agent_family().as_str());
            raw_key.strip_prefix(&prefix).unwrap_or(raw_key).to_string()
        });
    let is_subagent = obj
        .get("is_subagent")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let parent_id = obj
        .get("parent_id")
        .and_then(Value::as_str)
        .map(String::from);
    let parent_title = obj
        .get("parent_title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let arr = obj.get("turns").and_then(Value::as_array)?;
    let mut turns = Vec::with_capacity(arr.len());
    for tv in arr {
        let Some(to) = tv.as_object() else { continue };
        let step_index = to.get("step").and_then(Value::as_i64).unwrap_or(-1);
        let timestamp_utc = to
            .get("ts")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));
        let model = to
            .get("m")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let is_checkpoint = to.get("cp").and_then(Value::as_bool).unwrap_or(false);
        let input_tokens = to.get("in").and_then(Value::as_u64).unwrap_or(0);
        let cache_read_tokens = to.get("cr").and_then(Value::as_u64).unwrap_or(0);
        let cache_write_tokens = to.get("cw").and_then(Value::as_u64).unwrap_or(0);
        let cache_write_1h_tokens = to
            .get("cw1h")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(cache_write_tokens);
        let output_tokens = to.get("out").and_then(Value::as_u64).unwrap_or(0);
        let reasoning_tokens = to.get("rt").and_then(Value::as_u64).unwrap_or(0);
        let tool_names = to
            .get("tools")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let thinking_chars = to.get("th").and_then(Value::as_u64).unwrap_or(0);
        let gen_secs = to.get("gen").and_then(Value::as_u64).map(|g| g.min(u32::MAX as u64) as u32);
        turns.push(TurnRecord {
            step_index,
            timestamp_utc,
            model,
            is_checkpoint,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            cache_write_1h_tokens,
            output_tokens,
            reasoning_tokens,
            tool_names,
            thinking_chars,
            gen_secs,
            cost: CostBreakdown::default(),
        });
    }
    if turns.is_empty() {
        return None;
    }
    Some(ArchivedSession {
        conversation_id,
        title,
        workspace,
        source,
        is_subagent,
        parent_id,
        parent_title,
        turns,
    })
}

fn materialize_archived_session(arch: &ArchivedSession, pricing: &PricingCatalog) -> SessionInfo {
    let mut turns = arch.turns.clone();
    let fallback_model = {
        let mut by_m: HashMap<&str, u64> = HashMap::new();
        for t in &turns {
            if t.is_checkpoint || is_compaction_model(&t.model) || t.model == "unknown" {
                continue;
            }
            *by_m.entry(t.model.as_str()).or_insert(0) += t.total_input() + t.output_tokens;
        }
        by_m.into_iter()
            .max_by_key(|&(_, tok)| tok)
            .map(|(m, _)| m.to_string())
    };
    let mut last_real: Option<String> = None;
    let mut counts = TokenCounts::default();
    let mut models = BTreeSet::new();
    let mut first_time: Option<DateTime<Local>> = None;
    let mut last_time: Option<DateTime<Local>> = None;

    for t in &mut turns {
        let eff_model = if !t.is_checkpoint && !is_compaction_model(&t.model) && t.model != "unknown" {
            last_real = Some(t.model.clone());
            t.model.as_str()
        } else if !is_compaction_model(&t.model) && t.model != "unknown" {
            t.model.as_str()
        } else if let Some(prev) = last_real.as_deref() {
            prev
        } else {
            fallback_model.as_deref().unwrap_or(t.model.as_str())
        };
        let local_date = t.timestamp_utc.map(|u| u.with_timezone(&Local).date_naive());
        t.cost = price_turn_record(pricing, eff_model, t, local_date);
        counts.add_call_with_cost(
            t.input_tokens,
            t.cache_read_tokens,
            t.cache_write_tokens,
            t.output_tokens,
            t.cost,
        );
        models.insert(t.model.clone());
        if let Some(ts_utc) = t.timestamp_utc {
            let ts_local = ts_utc.with_timezone(&Local);
            first_time = Some(match first_time {
                Some(prev) if prev <= ts_local => prev,
                _ => ts_local,
            });
            last_time = Some(match last_time {
                Some(prev) if prev >= ts_local => prev,
                _ => ts_local,
            });
        }
    }

    SessionInfo {
        conversation_id: arch.conversation_id.clone(),
        title: arch.title.clone(),
        workspace: arch.workspace.clone(),
        source: arch.source,
        is_archived: true,
        is_subagent: arch.is_subagent,
        parent_id: arch.parent_id.clone(),
        parent_title: arch.parent_title.clone(),
        turns,
        counts,
        models,
        first_time,
        last_time,
        last_file_mtime_utc: None,
        is_running_hint: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn test_archive_persists_and_backfills_deleted_sessions_without_duplication() {
        let dir = std::env::temp_dir().join(format!("agy_gauge_arch_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let arch_path = dir.join("archive.json");
        let pricing = PricingCatalog::new(None);

        let ts = Utc.with_ymd_and_hms(2026, 11, 20, 10, 0, 0).unwrap();
        let s1 = SessionInfo {
            conversation_id: "conv-keep".into(),
            title: "Keep Me".into(),
            workspace: "C:/proj".into(),
            source: SourceKind::Cli,
            is_archived: false,
            is_subagent: false,
            parent_id: None,
            parent_title: String::new(),
            turns: vec![TurnRecord {
                step_index: 1,
                timestamp_utc: Some(ts),
                model: "gemini-4-neon-xhigh".into(),
                is_checkpoint: false,
                input_tokens: 100_000,
                cache_read_tokens: 400_000,
                cache_write_tokens: 0,
                cache_write_1h_tokens: 0,
                output_tokens: 2_000,
                reasoning_tokens: 0,
                tool_names: vec!["read_file".into()],
                thinking_chars: 120,
                gen_secs: None,
                cost: CostBreakdown::default(),
            }],
            counts: TokenCounts::default(),
            models: BTreeSet::new(),
            first_time: None,
            last_time: None,
            last_file_mtime_utc: None,
            is_running_hint: false,
        };

        {
            let mut a1 = UsageArchive::new(Some(arch_path.clone()), true);
            let back = a1.sync_and_backfill(&[s1.clone()], &pricing, true);
            assert!(back.is_empty());
        }
        assert!(arch_path.is_file());

        // Reload in a fresh instance: when live session is gone, archive backfills it with re-computed cost.
        {
            let mut a2 = UsageArchive::new(Some(arch_path.clone()), true);
            let back = a2.sync_and_backfill(&[], &pricing, true);
            assert_eq!(back.len(), 1);
            assert!(back[0].is_archived);
            assert_eq!(back[0].counts.total_tokens(), 502_000);
            assert!(back[0].counts.cost.total_usd() > 0.3);
            assert_eq!(back[0].turns[0].tool_names, vec!["read_file"]);

            // When live session IS present, archive must NOT duplicate it.
            let back2 = a2.sync_and_backfill(&[s1], &pricing, true);
            assert!(back2.is_empty());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_archive_multi_agent_keys_optional_fields_and_legacy_v1_compat() {
        let dir = std::env::temp_dir().join(format!(
            "agy_gauge_arch_multi_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let arch_path = dir.join("archive.json");
        let pricing = PricingCatalog::new(None);
        let ts = Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap();

        // Seed a legacy v1 archive.json (no `rt`, no `cw1h`, bare AGY key).
        let legacy = json!({
            "schema_version": 1,
            "sessions": {
                "shared-id": {
                    "title": "Legacy AGY",
                    "workspace": "C:/agy",
                    "source": "cli",
                    "turns": [{
                        "step": 1,
                        "ts": ts.to_rfc3339(),
                        "m": "claude-sonnet-4-6",
                        "in": 1000,
                        "cr": 2000,
                        "out": 500
                    }]
                }
            }
        });
        std::fs::write(&arch_path, legacy.to_string()).unwrap();

        // Sync a Claude Code session with the SAME conversation_id, carrying `rt` and `cw1h`.
        let cc = SessionInfo {
            conversation_id: "shared-id".into(),
            title: "Claude Session".into(),
            workspace: "C:/cc".into(),
            source: SourceKind::ClaudeCode,
            is_archived: false,
            is_subagent: false,
            parent_id: None,
            parent_title: String::new(),
            turns: vec![TurnRecord {
                step_index: 1,
                timestamp_utc: Some(ts),
                model: "claude-sonnet-4-6".into(),
                is_checkpoint: false,
                input_tokens: 1_000,
                cache_read_tokens: 0,
                cache_write_tokens: 1_000_000,
                cache_write_1h_tokens: 1_000_000,
                output_tokens: 2_000,
                reasoning_tokens: 800,
                tool_names: vec![],
                thinking_chars: 0,
                gen_secs: None,
                cost: CostBreakdown::default(),
            }],
            counts: TokenCounts::default(),
            models: BTreeSet::new(),
            first_time: None,
            last_time: None,
            last_file_mtime_utc: None,
            is_running_hint: false,
        };

        {
            let mut a1 = UsageArchive::new(Some(arch_path.clone()), true);
            // AGY session is archived-only while Claude session is live -> AGY backfills, no collision!
            let back = a1.sync_and_backfill(&[cc.clone()], &pricing, true);
            assert_eq!(back.len(), 1);
            assert_eq!(back[0].source, SourceKind::Cli);
            assert_eq!(back[0].turns[0].reasoning_tokens, 0);
            assert_eq!(back[0].turns[0].cache_write_1h_tokens, 0);
        }

        // Verify persisted JSON omits `rt`/`cw1h` when 0 (AGY) and keeps them when >0 (Claude).
        let raw = std::fs::read_to_string(&arch_path).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        let agy_turn = &parsed["sessions"]["shared-id"]["turns"][0];
        assert!(agy_turn.get("rt").is_none());
        assert!(agy_turn.get("cw1h").is_none());
        let cc_turn = &parsed["sessions"]["claude-code:shared-id"]["turns"][0];
        assert_eq!(cc_turn["rt"], 800);
        assert_eq!(cc_turn["cw1h"], 1_000_000);

        // Reload fresh archive with no live sessions -> both sessions backfill with distinct sources
        // and Claude's 1h cache write is priced at 2.0x input ($6.00/M for Sonnet 4.6, vs $3.75/M 5m).
        {
            let mut a2 = UsageArchive::new(Some(arch_path.clone()), true);
            let mut back = a2.sync_and_backfill(&[], &pricing, true);
            assert_eq!(back.len(), 2);
            back.sort_by_key(|s| s.source);
            assert_eq!(back[0].source, SourceKind::Cli);
            assert_eq!(back[1].source, SourceKind::ClaudeCode);
            assert_eq!(back[1].conversation_id, "shared-id");
            assert_eq!(back[1].turns[0].reasoning_tokens, 800);
            assert!(back[1].turns[0].has_thinking());
            assert_eq!(back[1].turns[0].cache_write_1h_tokens, 1_000_000);
            assert!((back[1].turns[0].cost.cache_write_usd - 6.0).abs() < 1e-6);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
