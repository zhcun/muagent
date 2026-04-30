//! MemorySessionStore —— 测试 / 短暂 run 用;不持久化到磁盘。
//!
//! 支持 `inject_crash_after_save(n)`:连续 `n` 次 save 成功后,第 `n+1` 次
//! 返回 `StoreError::Transient("injected")`,用于验证 at-least-once 契约。

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::core::error::StoreError;
use crate::core::event::{Event, EventSeq, RunId, SessionId};
use crate::core::prelude::{RunFilter, RunHeader, RunStatus};
use crate::core::run_state::RunState;
use crate::core::store::SessionStore;

/// In-memory state behind a *single* mutex so save_delta's CAS check and
/// the actual write are atomic. Earlier this used separate `Mutex<runs>`
/// and `Mutex<events>`, which left a race window: a concurrent reader
/// could observe state-without-events between the two locks.
#[derive(Default)]
struct Inner {
    runs: HashMap<RunId, RunState>,
    events: HashMap<RunId, Vec<Event>>,
}

#[derive(Default)]
pub struct MemorySessionStore {
    inner: Mutex<Inner>,
    kv: Mutex<HashMap<(SessionId, String), Vec<u8>>>,
    /// None = no injection;Some(n) = fail after n successful saves
    crash_after_n_saves: AtomicUsize,
    /// sentinel: 0 means disabled; >0 means "fail when consumed"
    save_count: AtomicUsize,
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 下一次 save_delta 成功时视为第 1 次;第 `n+1` 次起返错误。
    /// 设 0 = 禁用。
    pub fn inject_crash_after_save(&self, n: usize) {
        self.crash_after_n_saves
            .store(n.saturating_add(1), Ordering::SeqCst);
        self.save_count.store(0, Ordering::SeqCst);
    }

    pub fn disable_crash_injection(&self) {
        self.crash_after_n_saves.store(0, Ordering::SeqCst);
    }

    pub fn saves_observed(&self) -> usize {
        self.save_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    async fn save_delta(&self, state: &RunState, events: &[Event]) -> Result<(), StoreError> {
        // injector check:fail BEFORE writing(模拟 crash before commit)
        let threshold = self.crash_after_n_saves.load(Ordering::SeqCst);
        if threshold > 0 {
            let seen = self.save_count.load(Ordering::SeqCst) + 1;
            if seen >= threshold {
                // don't increment save_count — this is a failed attempt
                return Err(StoreError::Transient("injected crash".into()));
            }
        }

        // Single-lock atomic CAS + write. Strict contract (matches
        // JsonlStore so the shared `store_contract` suite passes both):
        // `state.event_seq - events.len() == prev.event_seq`. Each event
        // must claim exactly one seq increment; advancing seq without a
        // matching event is a caller bug (likely the Runner mutated state
        // without going through `commit`).
        let mut inner = self.inner.lock().unwrap();
        if let Some(existing) = inner.runs.get(&state.run_id) {
            let expected_base = state.event_seq.saturating_sub(events.len() as u64);
            if existing.event_seq != expected_base {
                return Err(StoreError::StaleState {
                    expected: expected_base,
                    actual: existing.event_seq,
                });
            }
        }
        let mut state_for_store = state.clone();
        state_for_store.ensure_history_ids();
        state_for_store.retain_active_compaction_checkpoints();
        state_for_store
            .validate_history_identity()
            .map_err(|e| StoreError::Corrupt(format!("history identity invariant failed: {e}")))?;
        inner.runs.insert(state.run_id, state_for_store);
        let v = inner.events.entry(state.run_id).or_default();
        for e in events {
            v.push(e.clone());
        }
        drop(inner);
        self.save_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn load_run(&self, id: RunId) -> Result<Option<RunState>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.runs.get(&id).cloned().map(|mut state| {
            state.ensure_history_ids();
            state
        }))
    }

    async fn list_runs(&self, filter: RunFilter) -> Result<Vec<RunHeader>, StoreError> {
        let inner = self.inner.lock().unwrap();
        let mut out: Vec<RunHeader> = inner
            .runs
            .values()
            .filter(|r| filter.session_id.map_or(true, |s| s == r.session_id))
            .map(|r| RunHeader {
                run_id: r.run_id,
                session_id: r.session_id,
                parent_run_id: r.parent_run_id,
                title: None,
                status: status_from_step(&r.step),
                turns: r.usage.turns,
                updated_ms: r.updated_ms,
            })
            .collect();
        out.sort_by_key(|h| -h.updated_ms);
        if let Some(limit) = filter.limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    async fn delete_run(&self, id: RunId) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.runs.remove(&id);
        inner.events.remove(&id);
        Ok(())
    }

    async fn query_events(
        &self,
        run_id: RunId,
        since_seq: EventSeq,
    ) -> Result<Vec<Event>, StoreError> {
        let inner = self.inner.lock().unwrap();
        let v = inner.events.get(&run_id).cloned().unwrap_or_default();
        Ok(v.into_iter().filter(|e| e.seq() > since_seq).collect())
    }

    async fn kv_get(
        &self,
        session_id: SessionId,
        key: &str,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let kv = self.kv.lock().unwrap();
        Ok(kv.get(&(session_id, key.to_string())).cloned())
    }

    async fn kv_put(
        &self,
        session_id: SessionId,
        key: &str,
        value: &[u8],
    ) -> Result<(), StoreError> {
        self.kv
            .lock()
            .unwrap()
            .insert((session_id, key.to_string()), value.to_vec());
        Ok(())
    }

    async fn kv_list(
        &self,
        session_id: SessionId,
        prefix: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, StoreError> {
        let kv = self.kv.lock().unwrap();
        Ok(kv
            .iter()
            .filter(|((sid, k), _)| *sid == session_id && k.starts_with(prefix))
            .map(|((_, k), v)| (k.clone(), v.clone()))
            .collect())
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

// Run the shared SessionStore contract suite. Same suite that JsonlStore
// runs — guarantees the memory backend doesn't drift to weaker semantics
// than production stores.
#[cfg(test)]
mod contract_tests {
    use super::MemorySessionStore;
    use crate::core::testing::store_contract;

    #[tokio::test]
    async fn rejects_stale_state() {
        store_contract::save_delta_rejects_stale_state(MemorySessionStore::new()).await;
    }
    #[tokio::test]
    async fn accepts_idempotent_and_forward() {
        store_contract::save_delta_accepts_idempotent_and_forward(MemorySessionStore::new()).await;
    }
    #[tokio::test]
    async fn save_then_load_roundtrip() {
        store_contract::save_then_load_roundtrip(MemorySessionStore::new()).await;
    }
    #[tokio::test]
    async fn query_events_in_seq_order() {
        store_contract::query_events_returns_in_seq_order(MemorySessionStore::new()).await;
    }
    #[tokio::test]
    async fn kv_put_get_roundtrip() {
        store_contract::kv_put_get_roundtrip(MemorySessionStore::new()).await;
    }
}
