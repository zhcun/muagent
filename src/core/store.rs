//! SessionStore trait + list/delete/kv API。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::core::error::StoreError;
use crate::core::event::{Event, EventSeq, RunId, SessionId};
use crate::core::run_state::RunState;

#[async_trait]
pub trait SessionStore: Send + Sync {
    /// 原子写:state + events 在同一事务内落盘。
    /// 实现侧应检测 stale state(乐观并发):若 `state.event_seq` 已落后于存储端,
    /// 返 `StoreError::StaleState`。
    async fn save_delta(&self, state: &RunState, events: &[Event]) -> Result<(), StoreError>;

    async fn load_run(&self, id: RunId) -> Result<Option<RunState>, StoreError>;

    async fn list_runs(&self, filter: RunFilter) -> Result<Vec<RunHeader>, StoreError>;

    async fn delete_run(&self, id: RunId) -> Result<(), StoreError>;

    async fn query_events(
        &self,
        run_id: RunId,
        since_seq: EventSeq,
    ) -> Result<Vec<Event>, StoreError>;

    async fn kv_get(&self, session_id: SessionId, key: &str)
        -> Result<Option<Vec<u8>>, StoreError>;
    async fn kv_put(
        &self,
        session_id: SessionId,
        key: &str,
        value: &[u8],
    ) -> Result<(), StoreError>;
    async fn kv_list(
        &self,
        session_id: SessionId,
        prefix: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, StoreError>;

    /// Record one tool-call audit entry. Stores never fail Runner progress —
    /// on error, implementations may log and return Ok(()) rather than
    /// propagating. Default impl is a no-op for backward compatibility.
    async fn record_tool_audit(&self, _rec: &ToolAuditRecord) -> Result<(), StoreError> {
        Ok(())
    }

    /// Read back audit records (host / user inspection).
    /// Default: empty. Real backends should override.
    async fn query_tool_audit(
        &self,
        _filter: &AuditFilter,
    ) -> Result<Vec<ToolAuditRecord>, StoreError> {
        Ok(vec![])
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolAuditRecord {
    pub ts_ms: i64,
    pub session_id: SessionId,
    pub run_id: RunId,
    pub call_id: String,
    pub tool_name: String,
    /// e.g. "read_only" / "mutating" / "destructive" / "capability_mutation"
    pub side_effects: String,
    pub ok: bool,
    pub retryable: bool,
    pub args_hash: String,
    /// PII-sanitized view of the tool args (JSON).
    pub args_sanitized: String,
    /// First 200 chars of the tool result content.
    pub brief: String,
    pub duration_ms: u32,
}

#[derive(Clone, Debug, Default)]
pub struct AuditFilter {
    pub session_id: Option<SessionId>,
    pub run_id: Option<RunId>,
    pub tool_name: Option<String>,
    pub since_ms: Option<i64>,
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct RunFilter {
    pub session_id: Option<SessionId>,
    pub status: Option<RunStatus>,
    pub since_ms: Option<i64>,
    pub limit: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Active,
    Paused,
    Done,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunHeader {
    pub run_id: RunId,
    pub session_id: SessionId,
    pub parent_run_id: Option<RunId>,
    pub title: Option<String>,
    pub status: RunStatus,
    pub turns: u32,
    pub updated_ms: i64,
}
