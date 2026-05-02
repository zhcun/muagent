//! `JsonlSessionStore` — lightweight file-backed `SessionStore`.
//!
//! Layout under the configured root:
//! - `runs/<run_id>.jsonl`   append-only save-delta records (`RunState` + step events)
//! - `kv/<session_id>.json`  session-scoped KV map
//! - `audit/<run_id>.jsonl`  append-only tool audit records
//!
//! This keeps the storage model simple and script-readable while still
//! preserving the step-level persistence contract.
//!
//! Important: this backend intentionally avoids store-root or session lock
//! files. Multiple processes may open the same root; the append-only run files
//! are the source of truth, and stale-state checks catch normal sequential
//! conflicts without leaving crash-stale lock sentinels behind.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::core::error::StoreError;
use crate::core::event::{Event, EventSeq, RunId, SessionId};
use crate::core::prelude::{RunFilter, RunHeader, RunStatus};
use crate::core::run_state::RunState;
use crate::core::store::{AuditFilter, SessionStore, ToolAuditRecord};

pub struct JsonlSessionStore {
    root: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SaveRecord {
    state: RunState,
    #[serde(default)]
    events: Vec<Event>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct KvEntry {
    updated_ms: i64,
    value_hex: String,
}

impl JsonlSessionStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let root = path.as_ref().to_path_buf();
        tokio::fs::create_dir_all(root.join("runs"))
            .await
            .map_err(io_err)?;
        tokio::fs::create_dir_all(root.join("kv"))
            .await
            .map_err(io_err)?;
        tokio::fs::create_dir_all(root.join("audit"))
            .await
            .map_err(io_err)?;
        Ok(Self { root })
    }

    fn run_path(&self, run_id: RunId) -> PathBuf {
        self.root.join("runs").join(format!("{run_id}.jsonl"))
    }

    fn kv_path(&self, session_id: SessionId) -> PathBuf {
        self.root.join("kv").join(format!("{session_id}.json"))
    }

    fn audit_path(&self, run_id: RunId) -> PathBuf {
        self.root.join("audit").join(format!("{run_id}.jsonl"))
    }

    async fn read_run_records(&self, run_id: RunId) -> Result<Vec<SaveRecord>, StoreError> {
        read_jsonl(&self.run_path(run_id)).await
    }

    async fn read_last_record(&self, run_id: RunId) -> Result<Option<SaveRecord>, StoreError> {
        let records = self.read_run_records(run_id).await?;
        Ok(records.into_iter().last())
    }

    async fn read_kv_map(
        &self,
        session_id: SessionId,
    ) -> Result<BTreeMap<String, KvEntry>, StoreError> {
        let path = self.kv_path(session_id);
        match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Corrupt(format!("kv {}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(io_err(e)),
        }
    }

    async fn write_kv_map(
        &self,
        session_id: SessionId,
        kv: &BTreeMap<String, KvEntry>,
    ) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec_pretty(kv)
            .map_err(|e| StoreError::Corrupt(format!("kv serialize: {e}")))?;
        write_atomic(&self.kv_path(session_id), &bytes).await
    }
}

#[async_trait]
impl SessionStore for JsonlSessionStore {
    async fn save_delta(&self, state: &RunState, events: &[Event]) -> Result<(), StoreError> {
        let existing = self.read_last_record(state.run_id).await?;
        if let Some(prev) = &existing {
            let expected_base = state.event_seq.saturating_sub(events.len() as u64);
            if prev.state.event_seq != expected_base {
                return Err(StoreError::StaleState {
                    expected: expected_base,
                    actual: prev.state.event_seq,
                });
            }
        }

        let mut state_for_store = state.clone();
        state_for_store.ensure_history_ids();
        state_for_store.retain_active_compaction_checkpoints();
        state_for_store
            .validate_history_identity()
            .map_err(|e| StoreError::Corrupt(format!("history identity invariant failed: {e}")))?;
        let record = SaveRecord {
            state: state_for_store,
            events: events.to_vec(),
        };
        append_jsonl(&self.run_path(state.run_id), &record).await
    }

    async fn load_run(&self, id: RunId) -> Result<Option<RunState>, StoreError> {
        Ok(self.read_last_record(id).await?.map(|r| {
            let mut state = r.state;
            state.ensure_history_ids();
            state
        }))
    }

    async fn list_runs(&self, filter: RunFilter) -> Result<Vec<RunHeader>, StoreError> {
        let mut dir = tokio::fs::read_dir(self.root.join("runs"))
            .await
            .map_err(io_err)?;
        let mut out = Vec::new();
        while let Some(entry) = dir.next_entry().await.map_err(io_err)? {
            let file_type = entry.file_type().await.map_err(io_err)?;
            if !file_type.is_file() {
                continue;
            }
            let bytes = tokio::fs::read(entry.path()).await.map_err(io_err)?;
            let Some(last) = last_jsonl_record::<SaveRecord>(&bytes, &entry.path())? else {
                continue;
            };
            let state = last.state;
            if filter.session_id.is_some() && filter.session_id != Some(state.session_id) {
                continue;
            }
            if filter.since_ms.is_some() && state.updated_ms < filter.since_ms.unwrap_or_default() {
                continue;
            }
            let header = RunHeader {
                run_id: state.run_id,
                session_id: state.session_id,
                workspace_root: state.workspace_root.clone(),
                parent_run_id: state.parent_run_id,
                title: None,
                status: status_from_step(&state.step),
                turns: state.usage.turns,
                updated_ms: state.updated_ms,
            };
            if filter.status.map(|s| s == header.status).unwrap_or(true) {
                out.push(header);
            }
        }
        out.sort_by_key(|h| -h.updated_ms);
        if let Some(limit) = filter.limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    async fn delete_run(&self, id: RunId) -> Result<(), StoreError> {
        for path in [self.run_path(id), self.audit_path(id)] {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(e)),
            }
        }
        Ok(())
    }

    async fn query_events(
        &self,
        run_id: RunId,
        since_seq: EventSeq,
    ) -> Result<Vec<Event>, StoreError> {
        let records = self.read_run_records(run_id).await?;
        let mut dedup: BTreeMap<EventSeq, Event> = BTreeMap::new();
        for record in records {
            for event in record.events {
                dedup.entry(event.seq()).or_insert(event);
            }
        }
        Ok(dedup
            .into_iter()
            .filter_map(|(seq, event)| (seq > since_seq).then_some(event))
            .collect())
    }

    async fn kv_get(
        &self,
        session_id: SessionId,
        key: &str,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let map = self.read_kv_map(session_id).await?;
        match map.get(key) {
            Some(entry) => {
                Ok(Some(hex::decode(&entry.value_hex).map_err(|e| {
                    StoreError::Corrupt(format!("kv hex decode: {e}"))
                })?))
            }
            None => Ok(None),
        }
    }

    async fn kv_put(
        &self,
        session_id: SessionId,
        key: &str,
        value: &[u8],
    ) -> Result<(), StoreError> {
        let mut map = self.read_kv_map(session_id).await?;
        map.insert(
            key.to_string(),
            KvEntry {
                updated_ms: now_ms(),
                value_hex: hex::encode(value),
            },
        );
        self.write_kv_map(session_id, &map).await
    }

    async fn kv_list(
        &self,
        session_id: SessionId,
        prefix: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, StoreError> {
        let map = self.read_kv_map(session_id).await?;
        let mut out = Vec::new();
        for (key, value) in map {
            if key.starts_with(prefix) {
                out.push((
                    key,
                    hex::decode(&value.value_hex)
                        .map_err(|e| StoreError::Corrupt(format!("kv hex decode: {e}")))?,
                ));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    async fn record_tool_audit(&self, rec: &ToolAuditRecord) -> Result<(), StoreError> {
        append_jsonl(&self.audit_path(rec.run_id), rec).await
    }

    async fn query_tool_audit(
        &self,
        filter: &AuditFilter,
    ) -> Result<Vec<ToolAuditRecord>, StoreError> {
        let mut dir = tokio::fs::read_dir(self.root.join("audit"))
            .await
            .map_err(io_err)?;
        let mut out = Vec::new();
        while let Some(entry) = dir.next_entry().await.map_err(io_err)? {
            let file_type = entry.file_type().await.map_err(io_err)?;
            if !file_type.is_file() {
                continue;
            }
            for rec in read_jsonl::<ToolAuditRecord>(&entry.path()).await? {
                if filter
                    .session_id
                    .map(|sid| sid == rec.session_id)
                    .unwrap_or(true)
                    && filter.run_id.map(|rid| rid == rec.run_id).unwrap_or(true)
                    && filter
                        .tool_name
                        .as_ref()
                        .map(|n| n == &rec.tool_name)
                        .unwrap_or(true)
                    && filter.since_ms.map(|ts| rec.ts_ms >= ts).unwrap_or(true)
                {
                    out.push(rec);
                }
            }
        }
        out.sort_by_key(|rec| -rec.ts_ms);
        if let Some(limit) = filter.limit {
            out.truncate(limit);
        }
        Ok(out)
    }
}

fn status_from_step(step: &crate::core::step::Step) -> RunStatus {
    use crate::core::step::Step;
    match step {
        Step::Paused { .. } => RunStatus::Paused,
        Step::Done { .. } => RunStatus::Done,
        Step::Failed { .. } => RunStatus::Failed,
        _ => RunStatus::Active,
    }
}

async fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let line = serde_json::to_vec(value)
        .map_err(|e| StoreError::Corrupt(format!("jsonl serialize {}: {e}", path.display())))?;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(io_err)?;
    use tokio::io::AsyncWriteExt;
    file.write_all(&line).await.map_err(io_err)?;
    file.write_all(b"\n").await.map_err(io_err)?;
    // flush() only pushes Tokio's user-space buffer to the OS page cache;
    // sync_all() forces the kernel to push to disk. Without sync_all, a
    // power-loss between flush and the OS's lazy writeback loses the record
    // — which would silently violate save_delta's "step-level durable
    // persistence" contract. The cost is one fdatasync per save (~ms on
    // SSD; rare write path so amortized cost is fine for an interactive
    // agent runtime).
    file.flush().await.map_err(io_err)?;
    file.sync_all().await.map_err(io_err)?;
    Ok(())
}

async fn read_jsonl<T>(path: &Path) -> Result<Vec<T>, StoreError>
where
    T: for<'de> Deserialize<'de>,
{
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(io_err(e)),
    };
    parse_jsonl_records(&bytes, path)
}

fn parse_jsonl_records<T>(bytes: &[u8], path: &Path) -> Result<Vec<T>, StoreError>
where
    T: for<'de> Deserialize<'de>,
{
    let text = std::str::from_utf8(bytes)
        .map_err(|e| StoreError::Corrupt(format!("utf8 {}: {e}", path.display())))?;
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<T>(line) {
            Ok(value) => out.push(value),
            Err(_e) if idx + 1 == lines.len() && !text.ends_with('\n') => break,
            Err(e) => {
                return Err(StoreError::Corrupt(format!(
                    "jsonl {} line {}: {e}",
                    path.display(),
                    idx + 1
                )));
            }
        }
    }
    Ok(out)
}

fn last_jsonl_record<T>(bytes: &[u8], path: &Path) -> Result<Option<T>, StoreError>
where
    T: for<'de> Deserialize<'de>,
{
    Ok(parse_jsonl_records::<T>(bytes, path)?.into_iter().last())
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(io_err)?;
    }
    let tmp = path.with_extension("tmp");
    // Write tmp + fsync data BEFORE rename. Otherwise rename can succeed
    // while the data is only in page cache; a power-loss after rename but
    // before writeback leaves an empty/torn file at `path` (POSIX rename
    // is atomic for the directory entry, NOT for the file's data blocks).
    {
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .await
            .map_err(io_err)?;
        f.write_all(bytes).await.map_err(io_err)?;
        f.sync_all().await.map_err(io_err)?;
    }
    tokio::fs::rename(&tmp, path).await.map_err(io_err)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn io_err(e: std::io::Error) -> StoreError {
    StoreError::Io(e.to_string())
}

// Run the shared SessionStore contract suite. Same suite that
// MemorySessionStore runs — guarantees both backends honor identical
// semantics (no "memory weaker than prod" drift).
#[cfg(test)]
mod contract_tests {
    use super::JsonlSessionStore;
    use crate::core::testing::store_contract;

    fn tmpdir() -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("muagent-jsonl-contract-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    async fn make_store() -> JsonlSessionStore {
        JsonlSessionStore::open(tmpdir()).await.unwrap()
    }

    #[tokio::test]
    async fn rejects_stale_state() {
        store_contract::save_delta_rejects_stale_state(make_store().await).await;
    }
    #[tokio::test]
    async fn accepts_idempotent_and_forward() {
        store_contract::save_delta_accepts_idempotent_and_forward(make_store().await).await;
    }
    #[tokio::test]
    async fn save_then_load_roundtrip() {
        store_contract::save_then_load_roundtrip(make_store().await).await;
    }
    #[tokio::test]
    async fn query_events_in_seq_order() {
        store_contract::query_events_returns_in_seq_order(make_store().await).await;
    }
    #[tokio::test]
    async fn kv_put_get_roundtrip() {
        store_contract::kv_put_get_roundtrip(make_store().await).await;
    }
}
