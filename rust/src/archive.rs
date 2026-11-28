//! Lightweight local usage archive (`archive.json`) stored ONLY in the application's own
//! directory so history survives when Antigravity prunes old sessions from `~/.gemini`.
//!
//! Safety & Privacy:
//!   - NEVER writes inside `~/.gemini`, `~/.claude`, or `~/.codex`.
//!   - Stores only compact turn telemetry (timestamp, model, token counters, tool names,
//!     thinking length) and session metadata — never raw prompts or file contents.
//!   - Live sessions currently on disk ALWAYS take precedence over archived entries,
//!     guaranteeing zero double-counting.
//!   - Streaming `serde` reader & writer (zero intermediate `serde_json::Value` DOM or full-file
//!     `String`), keeping 100% `schema_version: 1` compatibility with `gauge_stats.py`.
//!   - Zero live-session turn duplication in RAM: persisted live sessions are tracked via a compact
//!     `LiveArchiveSig` fingerprint and serialized directly from `&[SessionInfo]`.

use chrono::{DateTime, Local, Utc};
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::pricing::{CostBreakdown, PricingCatalog};
use crate::quota::{atomic_write_file_with, default_companion_path};
use crate::sources::{
    deserialize_trimmed_interned, FlexTimestamp, FlexU64, OptFlexBool, OptFlexI64, OptFlexString,
};
use crate::stats::{
    compute_active_duration_secs, compute_primary_model, intern_str, is_compaction_model,
    price_turn_record, AgentFamily, LocalTzCache, SessionInfo, SourceDiscoveryFilter, SourceKind,
    TokenCounts, TurnRecord,
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
    pub turns: Arc<[TurnRecord]>,
}

type LastTurnSig = (u64, u64, u64, u64, u64, u64, Option<u32>, u64, usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LiveArchiveSig {
    turns_len: usize,
    source: SourceKind,
    is_subagent: bool,
    meta_hash: u64,
    last_turn: Option<LastTurnSig>,
}

fn fnv1a_hash_str(mut h: u64, s: &str) -> u64 {
    for &b in s.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= 0xff;
    h.wrapping_mul(0x100000001b3)
}

fn compute_meta_hash(
    title: &str,
    workspace: &str,
    parent_id: Option<&str>,
    parent_title: &str,
) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    h = fnv1a_hash_str(h, title);
    h = fnv1a_hash_str(h, workspace);
    h = fnv1a_hash_str(h, parent_id.unwrap_or(""));
    h = fnv1a_hash_str(h, parent_title);
    // Include lengths to make collisions virtually impossible even for crafted inputs.
    let mut dh = std::collections::hash_map::DefaultHasher::new();
    (h, title.len(), workspace.len(), parent_id.map(str::len), parent_title.len()).hash(&mut dh);
    dh.finish()
}

fn last_turn_sig(turns: &[TurnRecord]) -> Option<LastTurnSig> {
    turns.last().map(|t| {
        (
            t.output_tokens,
            t.input_tokens,
            t.cache_read_tokens,
            t.cache_write_tokens,
            t.cache_write_1h_tokens,
            t.reasoning_tokens,
            t.gen_secs,
            t.thinking_chars,
            t.tool_names.len(),
        )
    })
}

fn sig_from_session(s: &SessionInfo) -> LiveArchiveSig {
    LiveArchiveSig {
        turns_len: s.turns.len(),
        source: s.source,
        is_subagent: s.is_subagent,
        meta_hash: compute_meta_hash(
            &s.title,
            &s.workspace,
            s.parent_id.as_deref(),
            &s.parent_title,
        ),
        last_turn: last_turn_sig(&s.turns),
    }
}

fn sig_from_archived(a: &ArchivedSession) -> LiveArchiveSig {
    LiveArchiveSig {
        turns_len: a.turns.len(),
        source: a.source,
        is_subagent: a.is_subagent,
        meta_hash: compute_meta_hash(
            &a.title,
            &a.workspace,
            a.parent_id.as_deref(),
            &a.parent_title,
        ),
        last_turn: last_turn_sig(&a.turns),
    }
}

pub struct UsageArchive {
    path: Option<PathBuf>,
    enabled: bool,
    /// Deferred initial load flag: `archive.json` is streamed on first `sync_and_backfill_cached`
    /// with live-key filtering so live sessions never allocate `TurnRecord` buffers at startup.
    loaded: bool,
    /// Archived-only sessions (plus any unpersisted live sessions awaiting their first write).
    /// Once persisted to disk, live sessions are evicted from here into `live_sigs` so zero
    /// duplicate turn buffers remain in RAM.
    archived_only: HashMap<(AgentFamily, String), ArchivedSession>,
    /// Compact 48-byte fingerprints for persisted live sessions currently on disk.
    live_sigs: HashMap<(AgentFamily, String), LiveArchiveSig>,
    /// Keys confirmed present in `archive.json` on disk.
    on_disk_keys: HashSet<(AgentFamily, String)>,
    dirty: bool,
    /// Cached priced materializations of `archived_only` entries for fast scans when pricing is unchanged.
    cached_backfill_pricing_gen: u64,
    cached_backfill_filter: Option<SourceDiscoveryFilter>,
    cached_backfill_dirty: bool,
    cached_backfill: Vec<SessionInfo>,
}

impl UsageArchive {
    pub fn new(path: Option<PathBuf>, enabled: bool) -> Self {
        Self {
            path,
            enabled,
            loaded: !enabled,
            archived_only: HashMap::new(),
            live_sigs: HashMap::new(),
            on_disk_keys: HashSet::new(),
            dirty: false,
            cached_backfill_pricing_gen: u64::MAX,
            cached_backfill_filter: None,
            cached_backfill_dirty: true,
            cached_backfill: Vec::new(),
        }
    }

    pub fn default_path() -> Option<PathBuf> {
        default_companion_path("archive.json")
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Streams `archive.json` directly via `serde` with zero intermediate `Value` DOM.
    /// When `only_keys` is `Some`, loads only matching keys (used to recover a live session
    /// that was pruned from disk after being evicted from `archived_only` into `live_sigs`).
    fn load(&mut self, only_keys: Option<&HashSet<(AgentFamily, String)>>) {
        let Some(path) = &self.path else { return };
        if crate::sources::stat_regular_file_fast(path).is_none() {
            return;
        }
        let Ok(f) = File::open(path) else { return };
        let reader = BufReader::with_capacity(64 * 1024, f);
        let mut de = serde_json::Deserializer::from_reader(reader);
        let seed = ArchiveRootSeed {
            only_keys,
            out: &mut self.archived_only,
            on_disk_keys: &mut self.on_disk_keys,
        };
        let _ = de::DeserializeSeed::deserialize(seed, &mut de);
        self.cached_backfill_dirty = true;
    }

    /// Lazy initial load filtered by `live_keys`:
    /// - For keys ALREADY present in `live_keys`, computes ONLY their 48-byte `LiveArchiveSig`
    ///   without allocating any `Vec<TurnRecord>`, `TurnRecord`, or `tool_names` buffers (~6–8 MiB saved!).
    /// - For pruned historical keys NOT in `live_keys`, materializes full `ArchivedSession`s for backfill.
    fn load_initial_filtered(&mut self, live_keys: &HashSet<(AgentFamily, &str)>) {
        self.loaded = true;
        let Some(path) = &self.path else { return };
        if crate::sources::stat_regular_file_fast(path).is_none() {
            return;
        }
        let Ok(f) = File::open(path) else { return };
        let reader = BufReader::with_capacity(64 * 1024, f);
        let mut de = serde_json::Deserializer::from_reader(reader);
        let mut mismatched: HashSet<(AgentFamily, String)> = HashSet::new();
        let seed = ArchiveInitialRootSeed {
            live_keys,
            archived_only: &mut self.archived_only,
            live_sigs: &mut self.live_sigs,
            on_disk_keys: &mut self.on_disk_keys,
            mismatched: &mut mismatched,
        };
        let _ = de::DeserializeSeed::deserialize(seed, &mut de);
        if !mismatched.is_empty() {
            self.load(Some(&mismatched));
        }
        self.archived_only.shrink_to_fit();
        self.cached_backfill_dirty = true;
    }

    /// Merges live sessions into the archive, persists if updated, and returns any archived-only
    /// sessions (re-priced using the active `PricingCatalog`) that no longer exist on disk.
    pub fn sync_and_backfill(
        &mut self,
        live_sessions: &[SessionInfo],
        pricing: &PricingCatalog,
        allow_persist: bool,
    ) -> Vec<SessionInfo> {
        self.sync_and_backfill_filtered(
            live_sessions,
            pricing,
            allow_persist,
            &SourceDiscoveryFilter::All,
        )
    }

    pub fn sync_and_backfill_filtered(
        &mut self,
        live_sessions: &[SessionInfo],
        pricing: &PricingCatalog,
        allow_persist: bool,
        filter: &SourceDiscoveryFilter,
    ) -> Vec<SessionInfo> {
        self.sync_and_backfill_cached(
            live_sessions,
            pricing,
            u64::MAX,
            allow_persist,
            true,
            filter,
        )
    }

    /// Full-featured sync with optional pricing generation cache and `sync_disk` control for
    /// debounced periodic writes vs immediate shutdown flush.
    pub fn sync_and_backfill_cached(
        &mut self,
        live_sessions: &[SessionInfo],
        pricing: &PricingCatalog,
        pricing_gen: u64,
        allow_persist: bool,
        sync_disk: bool,
        filter: &SourceDiscoveryFilter,
    ) -> Vec<SessionInfo> {
        if !self.enabled {
            return Vec::new();
        }

        let mut live_keys: HashSet<(AgentFamily, &str)> = HashSet::with_capacity(live_sessions.len());
        for s in live_sessions {
            live_keys.insert((s.source.agent_family(), s.conversation_id.as_str()));
        }
        if !self.loaded {
            self.load_initial_filtered(&live_keys);
        }

        let mut any_live_changed = false;

        for s in live_sessions {
            let family = s.source.agent_family();
            let new_sig = sig_from_session(s);

            if let Some(prev_sig) = self.live_sigs.get(&(family, s.conversation_id.clone())) {
                if *prev_sig != new_sig {
                    self.live_sigs
                        .insert((family, s.conversation_id.clone()), new_sig);
                    self.dirty = true;
                    any_live_changed = true;
                }
                continue;
            }

            let key = (family, s.conversation_id.clone());
            let prev_arch_sig = self.archived_only.get(&key).map(sig_from_archived);
            match prev_arch_sig {
                Some(prev_sig) if prev_sig == new_sig && self.on_disk_keys.contains(&key) => {
                    // Live session matches what is already on disk: drop duplicate ArchivedSession
                    // immediately and retain only the 48-byte LiveArchiveSig!
                    self.archived_only.remove(&key);
                    self.live_sigs.insert(key, new_sig);
                    self.cached_backfill_dirty = true;
                }
                Some(_) => {
                    // Live session updated since archive.json was last written.
                    self.dirty = true;
                    any_live_changed = true;
                    if self.on_disk_keys.contains(&key) {
                        self.archived_only.remove(&key);
                        self.live_sigs.insert(key, new_sig);
                    } else {
                        self.archived_only.insert(key, archived_from_session(s));
                    }
                    self.cached_backfill_dirty = true;
                }
                None => {
                    // Brand-new live session not yet in archive.json.
                    self.dirty = true;
                    any_live_changed = true;
                    self.archived_only.insert(key, archived_from_session(s));
                    self.cached_backfill_dirty = true;
                }
            }
        }

        // Detect any previously-live session that was pruned from disk since the last scan.
        let mut vanished_on_disk: HashSet<(AgentFamily, String)> = HashSet::new();
        self.live_sigs.retain(|k, _| {
            let still_live = live_keys.contains(&(k.0, k.1.as_str()));
            if !still_live {
                vanished_on_disk.insert(k.clone());
            }
            still_live
        });
        if !vanished_on_disk.is_empty() {
            // Recover vanished sessions from archive.json into `archived_only` so zero history is lost.
            self.load(Some(&vanished_on_disk));
            self.cached_backfill_dirty = true;
        }

        if allow_persist && self.dirty {
            if self.persist(live_sessions, sync_disk).is_ok() {
                self.dirty = false;
                // Now that all live sessions are safely persisted on disk, evict any remaining
                // live duplicates from `archived_only` into compact `live_sigs`.
                for s in live_sessions {
                    let key = (s.source.agent_family(), s.conversation_id.clone());
                    self.on_disk_keys.insert(key.clone());
                    self.archived_only.remove(&key);
                    self.live_sigs.insert(key, sig_from_session(s));
                }
                if self.archived_only.is_empty() {
                    self.archived_only.shrink_to_fit();
                }
                self.cached_backfill_dirty = true;
            }
        } else if any_live_changed {
            self.cached_backfill_dirty = true;
        }

        // Check if cached materialized backfills can be reused directly.
        let can_reuse_cache = pricing_gen != u64::MAX
            && !self.cached_backfill_dirty
            && self.cached_backfill_pricing_gen == pricing_gen
            && self.cached_backfill_filter.as_ref() == Some(filter);
        if can_reuse_cache {
            return self.cached_backfill.clone();
        }

        let mut tz_cache = LocalTzCache::new();
        let mut backfilled = Vec::new();
        for ((family, cid), arch) in &self.archived_only {
            if !filter.allows(*family)
                || live_keys.contains(&(*family, cid.as_str()))
                || arch.turns.is_empty()
            {
                continue;
            }
            backfilled.push(materialize_archived_session(arch, pricing, &mut tz_cache));
        }

        if pricing_gen != u64::MAX {
            self.cached_backfill = backfilled.clone();
            self.cached_backfill_pricing_gen = pricing_gen;
            self.cached_backfill_filter = Some(filter.clone());
            self.cached_backfill_dirty = false;
        }
        backfilled
    }

    /// Immediately persists pending dirty changes to `archive.json` (used on clean shutdown
    /// or when debounce interval elapses). Returns `true` if a write was performed.
    pub fn flush(&mut self, live_sessions: &[SessionInfo], sync_disk: bool) -> bool {
        if !self.enabled || !self.dirty {
            return false;
        }
        if self.persist(live_sessions, sync_disk).is_ok() {
            self.dirty = false;
            for s in live_sessions {
                let key = (s.source.agent_family(), s.conversation_id.clone());
                self.on_disk_keys.insert(key.clone());
                self.archived_only.remove(&key);
                self.live_sigs.insert(key, sig_from_session(s));
            }
            if self.archived_only.is_empty() {
                self.archived_only.shrink_to_fit();
            }
            true
        } else {
            false
        }
    }

    /// Streams `schema_version: 1` compact JSON directly to disk via `BufWriter` + atomic replace.
    fn persist(&self, live_sessions: &[SessionInfo], sync_disk: bool) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };

        // Build a sorted view over live_sessions + non-live archived_only entries (zero turn clones!).
        let mut live_by_key: HashMap<(AgentFamily, &str), &SessionInfo> =
            HashMap::with_capacity(live_sessions.len());
        for s in live_sessions {
            live_by_key.insert((s.source.agent_family(), s.conversation_id.as_str()), s);
        }

        let mut entries: Vec<((AgentFamily, &str), ArchiveWriteRef<'_>)> =
            Vec::with_capacity(live_by_key.len() + self.archived_only.len());
        for (k, &s) in &live_by_key {
            entries.push((*k, ArchiveWriteRef::Live(s)));
        }
        for (k, a) in &self.archived_only {
            let key_ref = (k.0, k.1.as_str());
            if !live_by_key.contains_key(&key_ref) {
                entries.push((key_ref, ArchiveWriteRef::Archived(a)));
            }
        }
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        let updated_at = Utc::now().to_rfc3339();
        let root_ser = ArchiveRootSerializer {
            updated_at: &updated_at,
            entries: &entries,
        };

        atomic_write_file_with(path, sync_disk, |bw| {
            serde_json::to_writer(bw, &root_ser).map_err(|e| format!("序列化归档失败: {e}"))
        })
    }
}

fn archived_from_session(s: &SessionInfo) -> ArchivedSession {
    ArchivedSession {
        conversation_id: s.conversation_id.clone(),
        title: s.title.clone(),
        workspace: s.workspace.clone(),
        source: s.source,
        is_subagent: s.is_subagent,
        parent_id: s.parent_id.clone(),
        parent_title: s.parent_title.clone(),
        turns: Arc::clone(&s.turns),
    }
}

// ---------- Streaming Serde Serializer (Schema Version 1) ----------

enum ArchiveWriteRef<'a> {
    Live(&'a SessionInfo),
    Archived(&'a ArchivedSession),
}

struct ArchiveRootSerializer<'a> {
    updated_at: &'a str,
    entries: &'a [((AgentFamily, &'a str), ArchiveWriteRef<'a>)],
}

impl<'a> Serialize for ArchiveRootSerializer<'a> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("schema_version", &1u8)?;
        map.serialize_entry("sessions", &ArchiveSessionsMapSerializer(self.entries))?;
        map.serialize_entry("updated_at", self.updated_at)?;
        map.end()
    }
}

struct ArchiveSessionsMapSerializer<'a>(&'a [((AgentFamily, &'a str), ArchiveWriteRef<'a>)]);

impl<'a> Serialize for ArchiveSessionsMapSerializer<'a> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        let mut key_buf = String::new();
        for ((family, cid), entry) in self.0 {
            if *family == AgentFamily::Antigravity {
                map.serialize_entry(*cid, &ArchiveSessionSerializer(entry))?;
            } else {
                key_buf.clear();
                key_buf.push_str(family.as_str());
                key_buf.push(':');
                key_buf.push_str(cid);
                map.serialize_entry(&key_buf, &ArchiveSessionSerializer(entry))?;
            }
        }
        map.end()
    }
}

struct ArchiveSessionSerializer<'a>(&'a ArchiveWriteRef<'a>);

impl<'a> Serialize for ArchiveSessionSerializer<'a> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (cid, title, workspace, source, is_subagent, parent_id, parent_title, turns): (
            &str,
            &str,
            &str,
            SourceKind,
            bool,
            Option<&str>,
            &str,
            &[TurnRecord],
        ) = match self.0 {
            ArchiveWriteRef::Live(s) => (
                &s.conversation_id,
                &s.title,
                &s.workspace,
                s.source,
                s.is_subagent,
                s.parent_id.as_deref(),
                &s.parent_title,
                &s.turns,
            ),
            ArchiveWriteRef::Archived(a) => (
                &a.conversation_id,
                &a.title,
                &a.workspace,
                a.source,
                a.is_subagent,
                a.parent_id.as_deref(),
                &a.parent_title,
                &a.turns,
            ),
        };
        let mut map = serializer.serialize_map(None)?;
        if source.agent_family() != AgentFamily::Antigravity {
            map.serialize_entry("conversation_id", cid)?;
        }
        if is_subagent {
            map.serialize_entry("is_subagent", &true)?;
        }
        if let Some(pid) = parent_id {
            map.serialize_entry("parent_id", pid)?;
        }
        if !parent_title.is_empty() {
            map.serialize_entry("parent_title", parent_title)?;
        }
        map.serialize_entry("source", source.as_str())?;
        map.serialize_entry("title", title)?;
        map.serialize_entry("turns", &ArchiveTurnsSeqSerializer(turns))?;
        map.serialize_entry("workspace", workspace)?;
        map.end()
    }
}

struct ArchiveTurnsSeqSerializer<'a>(&'a [TurnRecord]);

impl<'a> Serialize for ArchiveTurnsSeqSerializer<'a> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for t in self.0 {
            seq.serialize_element(&ArchiveTurnSerializer(t))?;
        }
        seq.end()
    }
}

struct ArchiveTurnSerializer<'a>(&'a TurnRecord);

impl<'a> Serialize for ArchiveTurnSerializer<'a> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let t = self.0;
        let mut map = serializer.serialize_map(None)?;
        if t.is_checkpoint {
            map.serialize_entry("cp", &true)?;
        }
        map.serialize_entry("cr", &t.cache_read_tokens)?;
        if t.cache_write_tokens > 0 {
            map.serialize_entry("cw", &t.cache_write_tokens)?;
        }
        if t.cache_write_1h_tokens > 0 {
            map.serialize_entry("cw1h", &t.cache_write_1h_tokens)?;
        }
        if let Some(g) = t.gen_secs {
            map.serialize_entry("gen", &g)?;
        }
        map.serialize_entry("in", &t.input_tokens)?;
        map.serialize_entry("m", t.model.as_ref())?;
        map.serialize_entry("out", &t.output_tokens)?;
        if t.reasoning_tokens > 0 {
            map.serialize_entry("rt", &t.reasoning_tokens)?;
        }
        map.serialize_entry("step", &t.step_index)?;
        if t.thinking_chars > 0 {
            map.serialize_entry("th", &t.thinking_chars)?;
        }
        if !t.tool_names.is_empty() {
            map.serialize_entry("tools", &ArchiveToolsSeqSerializer(&t.tool_names))?;
        }
        if let Some(ts) = t.timestamp_utc {
            map.serialize_entry("ts", &ts.to_rfc3339())?;
        }
        map.end()
    }
}

struct ArchiveToolsSeqSerializer<'a>(&'a [Arc<str>]);

impl<'a> Serialize for ArchiveToolsSeqSerializer<'a> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for name in self.0 {
            seq.serialize_element(name.as_ref())?;
        }
        seq.end()
    }
}

// ---------- Streaming Serde Deserializer (Schema Version 1) ----------

fn parse_archive_raw_key(raw_key: &str) -> (AgentFamily, &str) {
    if let Some((prefix, rest)) = raw_key.split_once(':') {
        if let Some(fam) = AgentFamily::from_str_loose(prefix) {
            return (fam, rest);
        }
    }
    (AgentFamily::Antigravity, raw_key)
}

struct ArchiveInitialRootSeed<'a, 'k> {
    live_keys: &'a HashSet<(AgentFamily, &'k str)>,
    archived_only: &'a mut HashMap<(AgentFamily, String), ArchivedSession>,
    live_sigs: &'a mut HashMap<(AgentFamily, String), LiveArchiveSig>,
    on_disk_keys: &'a mut HashSet<(AgentFamily, String)>,
    mismatched: &'a mut HashSet<(AgentFamily, String)>,
}

impl<'de, 'a, 'k> de::DeserializeSeed<'de> for ArchiveInitialRootSeed<'a, 'k> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V<'a, 'k> {
            live_keys: &'a HashSet<(AgentFamily, &'k str)>,
            archived_only: &'a mut HashMap<(AgentFamily, String), ArchivedSession>,
            live_sigs: &'a mut HashMap<(AgentFamily, String), LiveArchiveSig>,
            on_disk_keys: &'a mut HashSet<(AgentFamily, String)>,
            mismatched: &'a mut HashSet<(AgentFamily, String)>,
        }
        impl<'de, 'a, 'k> Visitor<'de> for V<'a, 'k> {
            type Value = ();
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("archive.json root object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while let Some(key) = map.next_key::<String>()? {
                    if key == "sessions" {
                        map.next_value_seed(ArchiveInitialSessionsMapSeed {
                            live_keys: self.live_keys,
                            archived_only: self.archived_only,
                            live_sigs: self.live_sigs,
                            on_disk_keys: self.on_disk_keys,
                            mismatched: self.mismatched,
                        })?;
                    } else {
                        let _: IgnoredAny = map.next_value()?;
                    }
                }
                Ok(())
            }
        }
        deserializer.deserialize_any(V {
            live_keys: self.live_keys,
            archived_only: self.archived_only,
            live_sigs: self.live_sigs,
            on_disk_keys: self.on_disk_keys,
            mismatched: self.mismatched,
        })
    }
}

struct ArchiveInitialSessionsMapSeed<'a, 'k> {
    live_keys: &'a HashSet<(AgentFamily, &'k str)>,
    archived_only: &'a mut HashMap<(AgentFamily, String), ArchivedSession>,
    live_sigs: &'a mut HashMap<(AgentFamily, String), LiveArchiveSig>,
    on_disk_keys: &'a mut HashSet<(AgentFamily, String)>,
    mismatched: &'a mut HashSet<(AgentFamily, String)>,
}

impl<'de, 'a, 'k> de::DeserializeSeed<'de> for ArchiveInitialSessionsMapSeed<'a, 'k> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V<'a, 'k> {
            live_keys: &'a HashSet<(AgentFamily, &'k str)>,
            archived_only: &'a mut HashMap<(AgentFamily, String), ArchivedSession>,
            live_sigs: &'a mut HashMap<(AgentFamily, String), LiveArchiveSig>,
            on_disk_keys: &'a mut HashSet<(AgentFamily, String)>,
            mismatched: &'a mut HashSet<(AgentFamily, String)>,
        }
        impl<'de, 'a, 'k> Visitor<'de> for V<'a, 'k> {
            type Value = ();
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("archive sessions map")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while let Some(raw_key) = map.next_key::<String>()? {
                    let inferred = parse_archive_raw_key(&raw_key);
                    if self.live_keys.contains(&inferred) {
                        if let Some((actual_key, sig)) =
                            map.next_value_seed(ArchiveLiveSigOnlySeed { raw_key: &raw_key })?
                        {
                            self.on_disk_keys.insert(actual_key.clone());
                            if self.live_keys.contains(&(actual_key.0, actual_key.1.as_str())) {
                                self.live_sigs.insert(actual_key, sig);
                            } else {
                                self.mismatched.insert(actual_key);
                            }
                        }
                    } else if let Some(entry) =
                        map.next_value_seed(ArchiveSessionSeed { raw_key: &raw_key })?
                    {
                        let key = (entry.source.agent_family(), entry.conversation_id.clone());
                        self.on_disk_keys.insert(key.clone());
                        if self.live_keys.contains(&(key.0, key.1.as_str())) {
                            self.live_sigs.insert(key, sig_from_archived(&entry));
                        } else {
                            self.archived_only.insert(key, entry);
                        }
                    }
                }
                Ok(())
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(())
            }
        }
        deserializer.deserialize_any(V {
            live_keys: self.live_keys,
            archived_only: self.archived_only,
            live_sigs: self.live_sigs,
            on_disk_keys: self.on_disk_keys,
            mismatched: self.mismatched,
        })
    }
}

/// Zero-turn-allocation streaming visitor for sessions already live on disk: computes ONLY the
/// 48-byte `LiveArchiveSig` so startup never allocates ~20K `TurnRecord`s just to evict them.
struct ArchiveLiveSigOnlySeed<'a> {
    raw_key: &'a str,
}

impl<'de, 'a> de::DeserializeSeed<'de> for ArchiveLiveSigOnlySeed<'a> {
    type Value = Option<((AgentFamily, String), LiveArchiveSig)>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V<'a> {
            raw_key: &'a str,
        }
        impl<'de, 'a> Visitor<'de> for V<'a> {
            type Value = Option<((AgentFamily, String), LiveArchiveSig)>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("archived session object (sig-only)")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut title: Option<String> = None;
                let mut workspace: Option<String> = None;
                let mut source_str: Option<String> = None;
                let mut conversation_id: Option<String> = None;
                let mut is_subagent = false;
                let mut parent_id: Option<String> = None;
                let mut parent_title = String::new();
                let mut turns_sig: Option<(usize, Option<LastTurnSig>)> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "title" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                title = Some(s);
                            }
                        }
                        "workspace" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                workspace = Some(s);
                            }
                        }
                        "source" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                source_str = Some(s);
                            }
                        }
                        "conversation_id" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                if !s.is_empty() {
                                    conversation_id = Some(s);
                                }
                            }
                        }
                        "is_subagent" => {
                            if let OptFlexBool(Some(b)) = map.next_value()? {
                                is_subagent = b;
                            }
                        }
                        "parent_id" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                parent_id = Some(s);
                            }
                        }
                        "parent_title" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                parent_title = s;
                            }
                        }
                        "turns" => {
                            turns_sig = map.next_value_seed(ArchiveTurnsSigOnlySeed)?;
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let Some((turns_len, last_turn)) = turns_sig else {
                    return Ok(None);
                };
                if turns_len == 0 {
                    return Ok(None);
                }
                let source = source_str
                    .as_deref()
                    .map(SourceKind::from_str_lossy)
                    .unwrap_or(SourceKind::Cli);
                let cid = conversation_id.unwrap_or_else(|| {
                    let prefix = format!("{}:", source.agent_family().as_str());
                    self.raw_key
                        .strip_prefix(&prefix)
                        .unwrap_or(self.raw_key)
                        .to_string()
                });
                let eff_title = title.as_deref().unwrap_or("(已归档会话)");
                let eff_ws = workspace.as_deref().unwrap_or("(未知工作区)");
                let sig = LiveArchiveSig {
                    turns_len,
                    source,
                    is_subagent,
                    meta_hash: compute_meta_hash(
                        eff_title,
                        eff_ws,
                        parent_id.as_deref(),
                        &parent_title,
                    ),
                    last_turn,
                };
                Ok(Some(((source.agent_family(), cid), sig)))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }
        deserializer.deserialize_any(V {
            raw_key: self.raw_key,
        })
    }
}

struct ArchiveTurnsSigOnlySeed;

impl<'de> de::DeserializeSeed<'de> for ArchiveTurnsSigOnlySeed {
    type Value = Option<(usize, Option<LastTurnSig>)>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<(usize, Option<LastTurnSig>)>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("archived turns array (sig-only)")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut count = 0usize;
                let mut last: Option<LastTurnSig> = None;
                while let Some(opt_sig) = seq.next_element_seed(ArchiveTurnSigOnlySeed)? {
                    if let Some(s) = opt_sig {
                        count += 1;
                        last = Some(s);
                    }
                }
                Ok(Some((count, last)))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct ArchiveTurnSigOnlySeed;

impl<'de> de::DeserializeSeed<'de> for ArchiveTurnSigOnlySeed {
    type Value = Option<LastTurnSig>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<LastTurnSig>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("archived turn object (sig-only)")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut input_tokens = 0u64;
                let mut cache_read_tokens = 0u64;
                let mut cache_write_tokens = 0u64;
                let mut cache_write_1h_tokens = 0u64;
                let mut output_tokens = 0u64;
                let mut reasoning_tokens = 0u64;
                let mut tools_len = 0usize;
                let mut thinking_chars = 0u64;
                let mut gen_secs: Option<u32> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "in" => {
                            let FlexU64(v) = map.next_value()?;
                            input_tokens = v;
                        }
                        "cr" => {
                            let FlexU64(v) = map.next_value()?;
                            cache_read_tokens = v;
                        }
                        "cw" => {
                            let FlexU64(v) = map.next_value()?;
                            cache_write_tokens = v;
                        }
                        "cw1h" => {
                            let FlexU64(v) = map.next_value()?;
                            cache_write_1h_tokens = v;
                        }
                        "out" => {
                            let FlexU64(v) = map.next_value()?;
                            output_tokens = v;
                        }
                        "rt" => {
                            let FlexU64(v) = map.next_value()?;
                            reasoning_tokens = v;
                        }
                        "tools" => {
                            tools_len = map.next_value_seed(ArchiveToolsCountSeed)?;
                        }
                        "th" => {
                            let FlexU64(v) = map.next_value()?;
                            thinking_chars = v;
                        }
                        "gen" => {
                            if let crate::sources::OptFlexU64(Some(g)) = map.next_value()? {
                                gen_secs = Some(g.min(u32::MAX as u64) as u32);
                            }
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }

                Ok(Some((
                    output_tokens,
                    input_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                    cache_write_1h_tokens.min(cache_write_tokens),
                    reasoning_tokens,
                    gen_secs,
                    thinking_chars,
                    tools_len,
                )))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct ArchiveToolsCountSeed;

impl<'de> de::DeserializeSeed<'de> for ArchiveToolsCountSeed {
    type Value = usize;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = usize;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("tool names array (count-only)")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut n = 0usize;
                while let Some(non_empty) = seq.next_element_seed(ArchiveToolNonEmptySeed)? {
                    if non_empty {
                        n += 1;
                    }
                }
                Ok(n)
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(0)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(0)
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct ArchiveToolNonEmptySeed;

impl<'de> de::DeserializeSeed<'de> for ArchiveToolNonEmptySeed {
    type Value = bool;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = bool;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("tool name string")
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                Ok(!v.trim().is_empty())
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(false)
            }
            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(false)
            }
            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(false)
            }
            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(false)
            }
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(false)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(false)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(false)
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct ArchiveRootSeed<'a> {
    only_keys: Option<&'a HashSet<(AgentFamily, String)>>,
    out: &'a mut HashMap<(AgentFamily, String), ArchivedSession>,
    on_disk_keys: &'a mut HashSet<(AgentFamily, String)>,
}

impl<'de, 'a> de::DeserializeSeed<'de> for ArchiveRootSeed<'a> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct RootVisitor<'a> {
            only_keys: Option<&'a HashSet<(AgentFamily, String)>>,
            out: &'a mut HashMap<(AgentFamily, String), ArchivedSession>,
            on_disk_keys: &'a mut HashSet<(AgentFamily, String)>,
        }
        impl<'de, 'a> Visitor<'de> for RootVisitor<'a> {
            type Value = ();
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("archive.json root object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while let Some(key) = map.next_key::<String>()? {
                    if key == "sessions" {
                        map.next_value_seed(ArchiveSessionsMapSeed {
                            only_keys: self.only_keys,
                            out: self.out,
                            on_disk_keys: self.on_disk_keys,
                        })?;
                    } else {
                        let _: IgnoredAny = map.next_value()?;
                    }
                }
                Ok(())
            }
        }
        deserializer.deserialize_any(RootVisitor {
            only_keys: self.only_keys,
            out: self.out,
            on_disk_keys: self.on_disk_keys,
        })
    }
}

struct ArchiveSessionsMapSeed<'a> {
    only_keys: Option<&'a HashSet<(AgentFamily, String)>>,
    out: &'a mut HashMap<(AgentFamily, String), ArchivedSession>,
    on_disk_keys: &'a mut HashSet<(AgentFamily, String)>,
}

impl<'de, 'a> de::DeserializeSeed<'de> for ArchiveSessionsMapSeed<'a> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct SessionsVisitor<'a> {
            only_keys: Option<&'a HashSet<(AgentFamily, String)>>,
            out: &'a mut HashMap<(AgentFamily, String), ArchivedSession>,
            on_disk_keys: &'a mut HashSet<(AgentFamily, String)>,
        }
        impl<'de, 'a> Visitor<'de> for SessionsVisitor<'a> {
            type Value = ();
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("archive sessions map")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while let Some(raw_key) = map.next_key::<String>()? {
                    // Quick filter when recovering specific vanished keys.
                    if let Some(wanted) = self.only_keys {
                        let maybe_match = wanted.iter().any(|(fam, cid)| {
                            raw_key == *cid
                                || raw_key
                                    .strip_prefix(fam.as_str())
                                    .and_then(|r| r.strip_prefix(':'))
                                    == Some(cid.as_str())
                        });
                        if !maybe_match {
                            let _: IgnoredAny = map.next_value()?;
                            continue;
                        }
                    }
                    let parsed = map.next_value_seed(ArchiveSessionSeed { raw_key: &raw_key })?;
                    if let Some(entry) = parsed {
                        let key = (entry.source.agent_family(), entry.conversation_id.clone());
                        if let Some(wanted) = self.only_keys {
                            if !wanted.contains(&key) {
                                continue;
                            }
                        }
                        self.on_disk_keys.insert(key.clone());
                        self.out.insert(key, entry);
                    }
                }
                Ok(())
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(())
            }
        }
        deserializer.deserialize_any(SessionsVisitor {
            only_keys: self.only_keys,
            out: self.out,
            on_disk_keys: self.on_disk_keys,
        })
    }
}

struct ArchiveSessionSeed<'a> {
    raw_key: &'a str,
}

impl<'de, 'a> de::DeserializeSeed<'de> for ArchiveSessionSeed<'a> {
    type Value = Option<ArchivedSession>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct SessionVisitor<'a> {
            raw_key: &'a str,
        }
        impl<'de, 'a> Visitor<'de> for SessionVisitor<'a> {
            type Value = Option<ArchivedSession>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("archived session object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut title: Option<String> = None;
                let mut workspace: Option<String> = None;
                let mut source_str: Option<String> = None;
                let mut conversation_id: Option<String> = None;
                let mut is_subagent = false;
                let mut parent_id: Option<String> = None;
                let mut parent_title = String::new();
                let mut turns: Option<Vec<TurnRecord>> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "title" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                title = Some(s);
                            }
                        }
                        "workspace" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                workspace = Some(s);
                            }
                        }
                        "source" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                source_str = Some(s);
                            }
                        }
                        "conversation_id" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                if !s.is_empty() {
                                    conversation_id = Some(s);
                                }
                            }
                        }
                        "is_subagent" => {
                            if let OptFlexBool(Some(b)) = map.next_value()? {
                                is_subagent = b;
                            }
                        }
                        "parent_id" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                parent_id = Some(s);
                            }
                        }
                        "parent_title" => {
                            if let OptFlexString(Some(s)) = map.next_value()? {
                                parent_title = s;
                            }
                        }
                        "turns" => {
                            turns = map.next_value_seed(ArchiveTurnsSeed)?;
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let Some(turns_vec) = turns else {
                    return Ok(None);
                };
                if turns_vec.is_empty() {
                    return Ok(None);
                }
                let source = source_str
                    .as_deref()
                    .map(SourceKind::from_str_lossy)
                    .unwrap_or(SourceKind::Cli);
                let cid = conversation_id.unwrap_or_else(|| {
                    let prefix = format!("{}:", source.agent_family().as_str());
                    self.raw_key
                        .strip_prefix(&prefix)
                        .unwrap_or(self.raw_key)
                        .to_string()
                });
                Ok(Some(ArchivedSession {
                    conversation_id: cid,
                    title: title.unwrap_or_else(|| "(已归档会话)".to_string()),
                    workspace: workspace.unwrap_or_else(|| "(未知工作区)".to_string()),
                    source,
                    is_subagent,
                    parent_id,
                    parent_title,
                    turns: turns_vec.into(),
                }))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }
        deserializer.deserialize_any(SessionVisitor {
            raw_key: self.raw_key,
        })
    }
}

struct ArchiveTurnsSeed;

impl<'de> de::DeserializeSeed<'de> for ArchiveTurnsSeed {
    type Value = Option<Vec<TurnRecord>>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<Vec<TurnRecord>>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("archived turns array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut turns = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(t) = seq.next_element_seed(ArchiveTurnSeed)? {
                    if let Some(rec) = t {
                        turns.push(rec);
                    }
                }
                Ok(Some(turns))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct ArchiveTurnSeed;

impl<'de> de::DeserializeSeed<'de> for ArchiveTurnSeed {
    type Value = Option<TurnRecord>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<TurnRecord>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("archived turn object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut step_index: i64 = -1;
                let mut timestamp_utc: Option<DateTime<Utc>> = None;
                let mut model: Option<Arc<str>> = None;
                let mut is_checkpoint = false;
                let mut input_tokens = 0u64;
                let mut cache_read_tokens = 0u64;
                let mut cache_write_tokens = 0u64;
                let mut cache_write_1h_tokens = 0u64;
                let mut output_tokens = 0u64;
                let mut reasoning_tokens = 0u64;
                let mut tool_names: Box<[Arc<str>]> = Box::new([]);
                let mut thinking_chars = 0u64;
                let mut gen_secs: Option<u32> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "step" => {
                            if let OptFlexI64(Some(v)) = map.next_value()? {
                                step_index = v;
                            }
                        }
                        "ts" => {
                            let FlexTimestamp(ts) = map.next_value()?;
                            timestamp_utc = ts;
                        }
                        "m" => {
                            model = map.next_value_seed(crate::sources::InternedStrSeed)?;
                        }
                        "cp" => {
                            if let OptFlexBool(Some(b)) = map.next_value()? {
                                is_checkpoint = b;
                            }
                        }
                        "in" => {
                            let FlexU64(v) = map.next_value()?;
                            input_tokens = v;
                        }
                        "cr" => {
                            let FlexU64(v) = map.next_value()?;
                            cache_read_tokens = v;
                        }
                        "cw" => {
                            let FlexU64(v) = map.next_value()?;
                            cache_write_tokens = v;
                        }
                        "cw1h" => {
                            let FlexU64(v) = map.next_value()?;
                            cache_write_1h_tokens = v;
                        }
                        "out" => {
                            let FlexU64(v) = map.next_value()?;
                            output_tokens = v;
                        }
                        "rt" => {
                            let FlexU64(v) = map.next_value()?;
                            reasoning_tokens = v;
                        }
                        "tools" => {
                            tool_names = map.next_value_seed(ArchiveToolsSeed)?;
                        }
                        "th" => {
                            let FlexU64(v) = map.next_value()?;
                            thinking_chars = v;
                        }
                        "gen" => {
                            if let crate::sources::OptFlexU64(Some(g)) = map.next_value()? {
                                gen_secs = Some(g.min(u32::MAX as u64) as u32);
                            }
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }

                Ok(Some(TurnRecord {
                    step_index,
                    timestamp_utc,
                    model: model.unwrap_or_else(|| intern_str("unknown")),
                    is_checkpoint,
                    input_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                    cache_write_1h_tokens: cache_write_1h_tokens.min(cache_write_tokens),
                    output_tokens,
                    reasoning_tokens,
                    tool_names,
                    thinking_chars,
                    gen_secs,
                    cost: CostBreakdown::default(),
                }))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(None)
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct ArchiveToolsSeed;

impl<'de> de::DeserializeSeed<'de> for ArchiveToolsSeed {
    type Value = Box<[Arc<str>]>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Box<[Arc<str>]>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("tool names array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut tools = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(name) = seq.next_element_seed(ArchiveRawToolSeed)? {
                    if let Some(s) = name {
                        tools.push(s);
                    }
                }
                Ok(tools.into_boxed_slice())
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(Box::new([]))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Box::new([]))
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct ArchiveRawToolSeed;

impl<'de> de::DeserializeSeed<'de> for ArchiveRawToolSeed {
    type Value = Option<Arc<str>>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserialize_trimmed_interned(deserializer)
    }
}

fn materialize_archived_session(
    arch: &ArchivedSession,
    pricing: &PricingCatalog,
    tz_cache: &mut LocalTzCache,
) -> SessionInfo {
    let mut turns: Vec<TurnRecord> = arch.turns.to_vec();
    let fallback_model: Option<Arc<str>> = {
        let mut by_m: HashMap<&str, u64> = HashMap::new();
        for t in &turns {
            if t.is_checkpoint || is_compaction_model(&t.model) || &*t.model == "unknown" {
                continue;
            }
            *by_m.entry(&t.model).or_insert(0) += t.total_input() + t.output_tokens;
        }
        by_m.into_iter()
            .max_by_key(|&(_, tok)| tok)
            .map(|(m, _)| intern_str(m))
    };
    let mut last_real: Option<Arc<str>> = None;
    let mut counts = TokenCounts::default();
    let mut models = std::collections::BTreeSet::new();
    let mut first_time: Option<DateTime<Local>> = None;
    let mut last_time: Option<DateTime<Local>> = None;
    let mut compaction_count = 0u32;
    let mut thinking_turns = 0u32;

    for t in &mut turns {
        let eff_model: &str =
            if !t.is_checkpoint && !is_compaction_model(&t.model) && &*t.model != "unknown" {
                last_real = Some(Arc::clone(&t.model));
                &t.model
            } else if !is_compaction_model(&t.model) && &*t.model != "unknown" {
                &t.model
            } else if let Some(prev) = last_real.as_deref() {
                prev
            } else {
                fallback_model.as_deref().unwrap_or(&t.model)
            };
        let ts_local = t.timestamp_utc.map(|u| tz_cache.to_local(u));
        let local_date = ts_local.map(|l| l.date_naive());
        t.cost = price_turn_record(pricing, eff_model, t, local_date);
        counts.add_call_with_cost(
            t.input_tokens,
            t.cache_read_tokens,
            t.cache_write_tokens,
            t.output_tokens,
            t.cost,
        );
        if !models.contains(t.model.as_ref()) {
            models.insert(t.model.to_string());
        }
        if t.is_checkpoint {
            compaction_count += 1;
        }
        if t.has_thinking() {
            thinking_turns += 1;
        }
        if let Some(ts_l) = ts_local {
            first_time = Some(match first_time {
                Some(prev) if prev <= ts_l => prev,
                _ => ts_l,
            });
            last_time = Some(match last_time {
                Some(prev) if prev >= ts_l => prev,
                _ => ts_l,
            });
        }
    }

    let primary_model = compute_primary_model(&turns);
    let active_duration_secs = compute_active_duration_secs(&turns);

    SessionInfo {
        conversation_id: arch.conversation_id.clone(),
        title: arch.title.clone(),
        workspace: arch.workspace.clone(),
        source: arch.source,
        is_archived: true,
        is_subagent: arch.is_subagent,
        parent_id: arch.parent_id.clone(),
        parent_title: arch.parent_title.clone(),
        turns: turns.into(),
        counts,
        models,
        first_time,
        last_time,
        last_file_mtime_utc: None,
        is_running_hint: false,
        primary_model,
        active_duration_secs,
        compaction_count,
        thinking_turns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::{json, Value};

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
                model: intern_str("gemini-4-neon-xhigh"),
                is_checkpoint: false,
                input_tokens: 100_000,
                cache_read_tokens: 400_000,
                cache_write_tokens: 0,
                cache_write_1h_tokens: 0,
                output_tokens: 2_000,
                reasoning_tokens: 0,
                tool_names: vec![intern_str("read_file")].into_boxed_slice(),
                thinking_chars: 120,
                gen_secs: None,
                cost: CostBreakdown::default(),
            }]
            .into(),
            counts: TokenCounts::default(),
            models: std::collections::BTreeSet::new(),
            first_time: None,
            last_time: None,
            last_file_mtime_utc: None,
            is_running_hint: false,
            primary_model: intern_str("gemini-4-neon-xhigh"),
            active_duration_secs: 15,
            compaction_count: 0,
            thinking_turns: 1,
        };

        {
            let mut a1 = UsageArchive::new(Some(arch_path.clone()), true);
            let back = a1.sync_and_backfill(&[s1.clone()], &pricing, true);
            assert!(back.is_empty());
            // Verify zero live-session duplication in archived_only after persist!
            assert!(a1.archived_only.is_empty());
            assert_eq!(a1.live_sigs.len(), 1);
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
            assert_eq!(back[0].turns[0].tool_names.len(), 1);
            assert_eq!(&*back[0].turns[0].tool_names[0], "read_file");

            // When live session IS present, archive must NOT duplicate it and drops it from archived_only.
            let back2 = a2.sync_and_backfill(&[s1], &pricing, true);
            assert!(back2.is_empty());
            assert!(a2.archived_only.is_empty());

            // If live session later disappears mid-process, archive lazily reloads it from disk!
            let back3 = a2.sync_and_backfill(&[], &pricing, true);
            assert_eq!(back3.len(), 1);
            assert_eq!(back3[0].conversation_id, "conv-keep");
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
                model: intern_str("claude-sonnet-4-6"),
                is_checkpoint: false,
                input_tokens: 1_000,
                cache_read_tokens: 0,
                cache_write_tokens: 1_000_000,
                cache_write_1h_tokens: 1_000_000,
                output_tokens: 2_000,
                reasoning_tokens: 800,
                tool_names: Box::new([]),
                thinking_chars: 0,
                gen_secs: None,
                cost: CostBreakdown::default(),
            }]
            .into(),
            counts: TokenCounts::default(),
            models: std::collections::BTreeSet::new(),
            first_time: None,
            last_time: None,
            last_file_mtime_utc: None,
            is_running_hint: false,
            primary_model: intern_str("claude-sonnet-4-6"),
            active_duration_secs: 15,
            compaction_count: 0,
            thinking_turns: 1,
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

        // Filtered backfill: AgyOnly must NOT leak archived Claude sessions.
        let mut a_filt = UsageArchive::new(Some(arch_path.clone()), true);
        let agy_only = a_filt.sync_and_backfill_filtered(
            &[],
            &pricing,
            false,
            &SourceDiscoveryFilter::AgyOnly,
        );
        assert_eq!(agy_only.len(), 1);
        assert_eq!(agy_only[0].source, SourceKind::Cli);

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

    #[test]
    fn test_archive_lazy_live_filtered_load_avoids_materializing_live_sessions() {
        let dir = std::env::temp_dir().join(format!(
            "agy_gauge_arch_lazy_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let arch_path = dir.join("archive.json");
        let pricing = PricingCatalog::new(None);
        let ts = Utc.with_ymd_and_hms(2026, 11, 25, 10, 0, 0).unwrap();

        let mk_sess = |cid: &str, title: &str, out: u64| SessionInfo {
            conversation_id: cid.into(),
            title: title.into(),
            workspace: "C:/proj".into(),
            source: SourceKind::Cli,
            is_archived: false,
            is_subagent: false,
            parent_id: None,
            parent_title: String::new(),
            turns: vec![TurnRecord {
                step_index: 1,
                timestamp_utc: Some(ts),
                model: intern_str("gemini-4-neon-xhigh"),
                is_checkpoint: false,
                input_tokens: 10_000,
                cache_read_tokens: 20_000,
                cache_write_tokens: 0,
                cache_write_1h_tokens: 0,
                output_tokens: out,
                reasoning_tokens: 0,
                tool_names: vec![intern_str("read_file")].into_boxed_slice(),
                thinking_chars: 50,
                gen_secs: Some(5),
                cost: CostBreakdown::default(),
            }]
            .into(),
            counts: TokenCounts::default(),
            models: std::collections::BTreeSet::new(),
            first_time: None,
            last_time: None,
            last_file_mtime_utc: None,
            is_running_hint: false,
            primary_model: intern_str("gemini-4-neon-xhigh"),
            active_duration_secs: 15,
            compaction_count: 0,
            thinking_turns: 1,
        };

        let s_live = mk_sess("live-sess-1", "Live One", 500);
        let s_pruned = mk_sess("pruned-sess-2", "Pruned Two", 900);

        // Persist both sessions into archive.json.
        {
            let mut writer = UsageArchive::new(Some(arch_path.clone()), true);
            let _ = writer.sync_and_backfill(&[s_live.clone(), s_pruned.clone()], &pricing, true);
        }

        // Fresh UsageArchive defers disk load until first sync!
        let mut reader = UsageArchive::new(Some(arch_path.clone()), true);
        assert!(!reader.loaded);
        assert!(reader.archived_only.is_empty());
        assert!(reader.live_sigs.is_empty());

        // First sync with s_live on disk: ONLY s_pruned is materialized into archived_only;
        // s_live goes straight to 48-byte live_sigs with zero TurnRecord allocations & !dirty!
        let back = reader.sync_and_backfill(&[s_live.clone()], &pricing, false);
        assert!(reader.loaded);
        assert!(!reader.is_dirty());
        assert_eq!(reader.archived_only.len(), 1);
        assert!(reader
            .archived_only
            .contains_key(&(AgentFamily::Antigravity, "pruned-sess-2".into())));
        assert_eq!(reader.live_sigs.len(), 1);
        assert!(reader
            .live_sigs
            .contains_key(&(AgentFamily::Antigravity, "live-sess-1".into())));
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].conversation_id, "pruned-sess-2");

        // If s_live later vanishes from disk, lazy single-key recovery restores all its turns!
        let back_all = reader.sync_and_backfill(&[], &pricing, false);
        assert_eq!(back_all.len(), 2);
        let recovered = back_all
            .iter()
            .find(|s| s.conversation_id == "live-sess-1")
            .unwrap();
        assert_eq!(recovered.turns.len(), 1);
        assert_eq!(recovered.turns[0].output_tokens, 500);
        assert_eq!(recovered.turns[0].tool_names.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
