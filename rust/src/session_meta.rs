//! Local user-curated session metadata (`session_meta.json`) stored ONLY in the application's
//! own directory (beside `gauge.exe`).
//!
//! Safety & Invariants:
//!   - NEVER reads or writes inside `~/.gemini`.
//!   - Hot-reloads on `(mtime, size)` signature changes and rejects symlinks on both read & write.
//!   - Uses atomic write (`atomic_write_json`) and auto-GCs empty entries (`!pinned`).
//!   - Forward-compatible serde: unknown keys in root or per-session objects are safely ignored on
//!     read and preserved on round-trip writes.

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::quota::{atomic_write_json, default_companion_path, regular_file_sig, FileSig};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionUserMeta {
    pub pinned: bool,
    pub updated_at: Option<DateTime<Utc>>,
}

impl SessionUserMeta {
    pub fn is_empty(&self) -> bool {
        !self.pinned
    }
}

pub struct SessionMetaStore {
    path: Option<PathBuf>,
    sig: Option<FileSig>,
    entries: HashMap<String, SessionUserMeta>,
    extra_fields: HashMap<String, Map<String, Value>>,
    changed_since_scan: bool,
}

impl SessionMetaStore {
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut store = Self {
            path,
            sig: None,
            entries: HashMap::new(),
            extra_fields: HashMap::new(),
            changed_since_scan: false,
        };
        store.refresh();
        store.changed_since_scan = false;
        store
    }

    pub fn default_path() -> Option<PathBuf> {
        default_companion_path("session_meta.json")
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn entries(&self) -> &HashMap<String, SessionUserMeta> {
        &self.entries
    }

    pub fn get(&self, cid: &str) -> Option<&SessionUserMeta> {
        self.entries.get(cid)
    }

    pub fn is_pinned(&self, cid: &str) -> bool {
        self.entries.get(cid).is_some_and(|e| e.pinned)
    }

    /// Re-checks `session_meta.json` mtime/size and reloads strictly read-only if changed.
    /// Also reports `true` if in-process mutations happened since the previous refresh/scan.
    pub fn refresh(&mut self) -> bool {
        let local_changed = std::mem::take(&mut self.changed_since_scan);
        let Some(path) = self.path.as_deref() else {
            return local_changed;
        };
        let sig = regular_file_sig(path);
        if sig == self.sig {
            return local_changed;
        }
        self.sig = sig;
        match sig {
            None => {
                let had = !self.entries.is_empty() || !self.extra_fields.is_empty();
                self.entries.clear();
                self.extra_fields.clear();
                local_changed || had
            }
            Some(_) => {
                if let Ok(mut f) = File::open(path) {
                    let mut text = String::new();
                    if f.read_to_string(&mut text).is_ok() {
                        if let Some((parsed_entries, parsed_extras)) =
                            parse_session_meta_with_extras(&text)
                        {
                            let disk_changed =
                                self.entries != parsed_entries || self.extra_fields != parsed_extras;
                            self.entries = parsed_entries;
                            self.extra_fields = parsed_extras;
                            return local_changed || disk_changed;
                        }
                    }
                }
                local_changed
            }
        }
    }

    /// Forces a reload from disk regardless of cached `(mtime, size)` signature (useful after
    /// cross-process sync notifications within the same filesystem timestamp granularity).
    pub fn force_reload(&mut self) -> bool {
        self.sig = None;
        self.refresh()
    }

    /// Toggles the `pinned` flag for `cid`, auto-GCs when `!pinned`, and persists atomically.
    /// Returns the new `pinned` state.
    pub fn toggle_pin(&mut self, cid: &str) -> Result<bool, String> {
        let cur = self.entries.get(cid.trim()).is_some_and(|e| e.pinned);
        self.set_pinned(cid, !cur)
    }

    /// Sets the `pinned` flag for `cid`, auto-GCs when `!pinned`, and persists atomically.
    pub fn set_pinned(&mut self, cid: &str, pinned: bool) -> Result<bool, String> {
        let cid = cid.trim();
        if cid.is_empty() {
            return Err("会话 ID 不能为空".to_string());
        }
        let prev = self.entries.get(cid).cloned();
        let mut next = prev.clone().unwrap_or_default();
        if next.pinned == pinned {
            return Ok(pinned);
        }
        next.pinned = pinned;
        next.updated_at = Some(Utc::now());

        let has_extra = self.extra_fields.get(cid).is_some_and(|m| !m.is_empty());
        if next.is_empty() && !has_extra {
            self.entries.remove(cid);
        } else {
            self.entries.insert(cid.to_string(), next);
        }

        if let Err(err) = self.persist() {
            match prev {
                Some(p) => {
                    self.entries.insert(cid.to_string(), p);
                }
                None => {
                    self.entries.remove(cid);
                }
            }
            return Err(err);
        }
        self.changed_since_scan = true;
        Ok(pinned)
    }

    /// Removes all local metadata for `cid` and persists atomically. Returns `true` if removed.
    pub fn clear_entry(&mut self, cid: &str) -> Result<bool, String> {
        let cid = cid.trim();
        if cid.is_empty() {
            return Ok(false);
        }
        let prev_entry = self.entries.remove(cid);
        let prev_extra = self.extra_fields.remove(cid);
        if prev_entry.is_none() && prev_extra.is_none() {
            return Ok(false);
        }
        if let Err(err) = self.persist() {
            if let Some(p) = prev_entry {
                self.entries.insert(cid.to_string(), p);
            }
            if let Some(ex) = prev_extra {
                self.extra_fields.insert(cid.to_string(), ex);
            }
            return Err(err);
        }
        self.changed_since_scan = true;
        Ok(true)
    }

    fn persist(&mut self) -> Result<(), String> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| "未配置 session_meta.json 路径".to_string())?;
        let mut sessions_map = Map::new();
        let mut keys: Vec<&String> = self
            .entries
            .keys()
            .chain(self.extra_fields.keys())
            .collect();
        keys.sort();
        keys.dedup();

        for cid in keys {
            let entry = self.entries.get(cid);
            let extra = self.extra_fields.get(cid);
            let has_extra = extra.is_some_and(|m| !m.is_empty());
            let has_known = entry.is_some_and(|e| !e.is_empty());
            if !has_known && !has_extra {
                continue;
            }
            let mut obj = extra.cloned().unwrap_or_default();
            if let Some(e) = entry {
                if e.pinned {
                    obj.insert("pinned".into(), json!(true));
                } else {
                    obj.remove("pinned");
                }
                if let Some(ts) = e.updated_at {
                    obj.insert("updated_at".into(), json!(ts.to_rfc3339()));
                } else {
                    obj.remove("updated_at");
                }
            }
            sessions_map.insert(cid.clone(), Value::Object(obj));
        }

        let root = json!({
            "schema_version": 1,
            "sessions": Value::Object(sessions_map),
        });
        let text = serde_json::to_string_pretty(&root)
            .map_err(|e| format!("序列化 session_meta.json 失败: {e}"))?;
        atomic_write_json(&path, &format!("{text}\n"))?;
        self.sig = regular_file_sig(&path);
        Ok(())
    }
}

pub fn parse_session_meta_json(text: &str) -> Option<HashMap<String, SessionUserMeta>> {
    parse_session_meta_with_extras(text).map(|(entries, _)| entries)
}

fn parse_session_meta_with_extras(
    text: &str,
) -> Option<(HashMap<String, SessionUserMeta>, HashMap<String, Map<String, Value>>)> {
    let val: Value = serde_json::from_str(text).ok()?;
    let root = val.as_object()?;
    let sessions = match root.get("sessions") {
        Some(Value::Object(map)) => map,
        Some(_) => return None,
        None => return Some((HashMap::new(), HashMap::new())),
    };

    let mut entries = HashMap::new();
    let mut extras = HashMap::new();
    for (cid, item) in sessions {
        let cid_trim = cid.trim();
        if cid_trim.is_empty() {
            continue;
        }
        let Some(obj) = item.as_object() else {
            continue;
        };
        let pinned = obj.get("pinned").and_then(Value::as_bool).unwrap_or(false);
        let updated_at = obj
            .get("updated_at")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s.trim()).ok())
            .map(|dt| dt.with_timezone(&Utc));

        let mut extra = Map::new();
        for (k, v) in obj {
            if !matches!(k.as_str(), "pinned" | "updated_at") {
                extra.insert(k.clone(), v.clone());
            }
        }

        let meta = SessionUserMeta { pinned, updated_at };
        if !extra.is_empty() {
            extras.insert(cid_trim.to_string(), extra);
            entries.insert(cid_trim.to_string(), meta);
        } else if !meta.is_empty() {
            entries.insert(cid_trim.to_string(), meta);
        }
    }
    Some((entries, extras))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_meta_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agy_gauge_session_meta_{tag}_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_store_pin_auto_gc_and_hot_reload() {
        let dir = temp_meta_dir("crud");
        let path = dir.join("session_meta.json");

        let mut store = SessionMetaStore::new(Some(path.clone()));
        assert!(store.entries().is_empty());

        // Pin session
        assert!(store.toggle_pin("sess-1").unwrap());
        assert!(store.is_pinned("sess-1"));
        assert!(path.is_file());

        // Reload in a fresh store
        let mut store2 = SessionMetaStore::new(Some(path.clone()));
        let e = store2.get("sess-1").expect("reloaded entry");
        assert!(e.pinned);
        assert!(e.updated_at.is_some());

        // Unpin -> entry becomes empty and is auto-GC'd from both memory and disk
        assert!(!store.toggle_pin("sess-1").unwrap());
        assert!(store.get("sess-1").is_none());
        let raw = fs::read_to_string(&path).unwrap();
        let parsed = parse_session_meta_json(&raw).unwrap();
        assert!(parsed.is_empty());

        // External write triggers hot-reload via force_reload
        fs::write(
            &path,
            r#"{"schema_version":1,"sessions":{"sess-ext":{"pinned":true}}}"#,
        )
        .unwrap();
        assert!(store2.force_reload());
        assert!(store2.is_pinned("sess-ext"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_forward_compatible_unknown_fields_preserved() {
        let dir = temp_meta_dir("fwd");
        let path = dir.join("session_meta.json");
        fs::write(
            &path,
            r#"{
                "schema_version": 1,
                "future_root_flag": true,
                "sessions": {
                    "sess-f": {
                        "pinned": false,
                        "note": "Phase 2 note",
                        "tags": ["refactor"]
                    }
                }
            }"#,
        )
        .unwrap();

        let mut store = SessionMetaStore::new(Some(path.clone()));
        assert!(!store.is_pinned("sess-f"));
        assert!(store.set_pinned("sess-f", true).unwrap());

        let saved_text = fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&saved_text).unwrap();
        let obj = v["sessions"]["sess-f"].as_object().unwrap();
        assert_eq!(obj["pinned"], json!(true));
        assert_eq!(obj["note"], json!("Phase 2 note"));
        assert_eq!(obj["tags"], json!(["refactor"]));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_isolated_none_store_blocks_writes() {
        let mut store = SessionMetaStore::new(None);
        assert!(store.entries().is_empty());
        assert!(!store.refresh());
        assert!(store.toggle_pin("sess-1").is_err());
        assert!(store.set_pinned("sess-1", true).is_err());
        assert!(store.entries().is_empty());
    }
}
