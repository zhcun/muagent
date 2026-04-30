//! Offline: tool_audit records land in JSONL store + PII is redacted + filters work.

use std::sync::Arc;

use async_trait::async_trait;
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::*;
use muagent::core::sanitize::sanitize_json;
use muagent::core::step::Step;
use muagent::core::testing::{reply, CannedModel};
use muagent::core::types::{Content, Message};
use muagent::storage::JsonlSessionStore;
use muagent::storage::MemorySessionStore;
use serde_json::json;
use uuid::Uuid;

struct EchoTools;
#[async_trait]
impl ToolExecutor for EchoTools {
    async fn execute(
        &self,
        c: &PendingCall,
        _ctx: &ToolContext,
        _k: CancelToken,
    ) -> Result<ToolResult, muagent::core::error::ToolExecutorError> {
        Ok(ToolResult::ok(format!("ran {}", c.tool_name)))
    }
    fn idempotency_for(&self, _c: &PendingCall) -> Idempotency {
        Idempotency::Idempotent
    }
}

struct AuditFailingStore {
    inner: Arc<dyn SessionStore>,
}

#[async_trait]
impl SessionStore for AuditFailingStore {
    async fn save_delta(&self, state: &RunState, events: &[Event]) -> Result<(), StoreError> {
        self.inner.save_delta(state, events).await
    }

    async fn load_run(&self, id: RunId) -> Result<Option<RunState>, StoreError> {
        self.inner.load_run(id).await
    }

    async fn list_runs(&self, filter: RunFilter) -> Result<Vec<RunHeader>, StoreError> {
        self.inner.list_runs(filter).await
    }

    async fn delete_run(&self, id: RunId) -> Result<(), StoreError> {
        self.inner.delete_run(id).await
    }

    async fn query_events(
        &self,
        run_id: RunId,
        since_seq: EventSeq,
    ) -> Result<Vec<Event>, StoreError> {
        self.inner.query_events(run_id, since_seq).await
    }

    async fn kv_get(
        &self,
        session_id: SessionId,
        key: &str,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.kv_get(session_id, key).await
    }

    async fn kv_put(
        &self,
        session_id: SessionId,
        key: &str,
        value: &[u8],
    ) -> Result<(), StoreError> {
        self.inner.kv_put(session_id, key, value).await
    }

    async fn kv_list(
        &self,
        session_id: SessionId,
        prefix: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, StoreError> {
        self.inner.kv_list(session_id, prefix).await
    }

    async fn record_tool_audit(&self, _rec: &ToolAuditRecord) -> Result<(), StoreError> {
        Err(StoreError::Io("audit sink unavailable".into()))
    }

    async fn query_tool_audit(
        &self,
        filter: &AuditFilter,
    ) -> Result<Vec<ToolAuditRecord>, StoreError> {
        self.inner.query_tool_audit(filter).await
    }
}

fn temp_store_root() -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("muagent-audit-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[tokio::test]
async fn sanitize_redacts_secret_keys() {
    let raw = json!({"user":"alice", "password":"p", "nested":{"api_key":"x"}, "safe":"ok"});
    let out = sanitize_json(raw);
    assert!(!out.contains("\"p\""));
    assert!(!out.contains("\"x\""));
    assert!(out.contains(r#""user":"alice""#));
    assert!(out.contains(r#""safe":"ok""#));
    assert_eq!(out.matches("<redacted>").count(), 2);
}

#[tokio::test]
async fn audit_row_written_and_sanitized() {
    let store: Arc<dyn SessionStore> =
        Arc::new(JsonlSessionStore::open(temp_store_root()).await.unwrap());

    let session_id = Uuid::new_v4();
    let run_id = Uuid::new_v4();

    let model = Arc::new(CannedModel::new(vec![
        reply::with_calls(
            "",
            vec![PendingCall::new(
                "c1",
                "do_api",
                json!({"endpoint":"/things","api_key":"sk-topsecret"}),
            )],
        ),
        reply::done("done"),
    ]));
    let runner = Runner::builder()
        .model(model)
        .tools(Arc::new(EchoTools))
        .store(store.clone())
        .tools_provider(|_s: &RunState| ActiveToolSet::default())
        .build()
        .unwrap();

    let mut state = RunState::new(run_id, session_id, 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("go"),
            },
        )
        .await
        .unwrap();
    for _ in 0..10 {
        if matches!(state.step, Step::Done { .. }) {
            break;
        }
        runner.step(&mut state).await.unwrap();
    }

    let rows = store
        .query_tool_audit(&AuditFilter {
            session_id: Some(session_id),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "expected one audit row, got {rows:?}");
    let row = &rows[0];
    assert_eq!(row.tool_name, "do_api");
    assert_eq!(row.run_id, run_id);
    assert_eq!(row.call_id, "c1");
    assert!(row.ok);
    assert!(
        !row.args_sanitized.contains("sk-topsecret"),
        "api_key leaked: {}",
        row.args_sanitized
    );
    assert!(row.args_sanitized.contains("<redacted>"));
    assert!(row.args_sanitized.contains("/things"));
    // brief carried the tool's return value.
    assert!(row.brief.contains("ran do_api"));
}

#[tokio::test]
async fn audit_write_failure_does_not_stop_tool_flow() {
    let inner: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let store: Arc<dyn SessionStore> = Arc::new(AuditFailingStore { inner });

    let model = Arc::new(CannedModel::new(vec![
        reply::with_calls("", vec![PendingCall::new("c1", "do_work", json!({}))]),
        reply::done("done"),
    ]));
    let runner = Runner::builder()
        .model(model)
        .tools(Arc::new(EchoTools))
        .store(store)
        .tools_provider(|_s: &RunState| ActiveToolSet::default())
        .build()
        .unwrap();

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("run tool"),
            },
        )
        .await
        .unwrap();

    for _ in 0..10 {
        if matches!(state.step, Step::Done { .. }) {
            break;
        }
        runner.step(&mut state).await.unwrap();
    }

    assert!(matches!(state.step, Step::Done { .. }));
    assert_eq!(state.usage.tool_calls, 1);
}

#[tokio::test]
async fn audit_filter_by_tool_name() {
    let store: Arc<dyn SessionStore> =
        Arc::new(JsonlSessionStore::open(temp_store_root()).await.unwrap());
    let sid = Uuid::new_v4();
    let rid = Uuid::new_v4();

    let model = Arc::new(CannedModel::new(vec![
        reply::with_calls(
            "",
            vec![
                PendingCall::new("c1", "alpha", json!({})),
                PendingCall::new("c2", "beta", json!({})),
            ],
        ),
        reply::done("done"),
    ]));
    let runner = Runner::builder()
        .model(model)
        .tools(Arc::new(EchoTools))
        .store(store.clone())
        .tools_provider(|_s: &RunState| ActiveToolSet::default())
        .build()
        .unwrap();

    let mut state = RunState::new(rid, sid, 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("x"),
            },
        )
        .await
        .unwrap();
    for _ in 0..15 {
        if matches!(state.step, Step::Done { .. }) {
            break;
        }
        runner.step(&mut state).await.unwrap();
    }

    let only_alpha = store
        .query_tool_audit(&AuditFilter {
            tool_name: Some("alpha".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(only_alpha.len(), 1);
    assert_eq!(only_alpha[0].tool_name, "alpha");

    let all = store
        .query_tool_audit(&AuditFilter::default())
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
}
