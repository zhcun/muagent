//! M1-P2 测试:JsonlSessionStore 端到端 —— 保存 state、读回、查询 events、thaw 恢复。

use std::sync::Arc;

use async_trait::async_trait;
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::*;
use muagent::core::step::Step;
use muagent::core::testing::{reply, CannedModel};
use muagent::core::types::{Content, Message};
use muagent::storage::JsonlSessionStore;
use serde_json::json;
use uuid::Uuid;

struct NoTools;
#[async_trait]
impl ToolExecutor for NoTools {
    async fn execute(
        &self,
        _c: &PendingCall,
        _ctx: &ToolContext,
        _ct: CancelToken,
    ) -> Result<ToolResult, muagent::core::error::ToolExecutorError> {
        Ok(ToolResult::ok("ok"))
    }
    fn idempotency_for(&self, _c: &PendingCall) -> Idempotency {
        Idempotency::Idempotent
    }
}

fn temp_store_root() -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("muagent-jsonl-store-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn new_state() -> RunState {
    RunState::new(Uuid::new_v4(), Uuid::new_v4(), 1)
}

fn build_runner(model: Arc<dyn ModelAdapter>, store: Arc<dyn SessionStore>) -> Runner {
    Runner::builder()
        .model(model)
        .tools(Arc::new(NoTools))
        .store(store)
        .tools_provider(|_s: &RunState| ActiveToolSet::default())
        .build()
        .unwrap()
}

#[tokio::test]
async fn jsonl_save_load_roundtrip() {
    let store: Arc<dyn SessionStore> =
        Arc::new(JsonlSessionStore::open(temp_store_root()).await.unwrap());

    let model = Arc::new(CannedModel::new(vec![reply::text("hello")]));
    let runner = build_runner(model, store.clone());

    let mut state = new_state();
    let rid = state.run_id;
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("hi"),
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

    let loaded = store.load_run(rid).await.unwrap().expect("found");
    assert_eq!(loaded.run_id, rid);
    assert!(matches!(loaded.step, Step::Done { .. }));
    assert_eq!(loaded.history.len(), state.history.len());
    assert_eq!(loaded.usage.turns, 1);

    let all = store.query_events(rid, 0).await.unwrap();
    assert!(
        all.len() >= 3,
        "expect at least 3 events, got {}",
        all.len()
    );
    for pair in all.windows(2) {
        assert!(pair[0].seq() < pair[1].seq());
    }
}

#[tokio::test]
async fn jsonl_list_runs_by_session() {
    let store: Arc<dyn SessionStore> =
        Arc::new(JsonlSessionStore::open(temp_store_root()).await.unwrap());

    let session_a = Uuid::new_v4();
    let session_b = Uuid::new_v4();

    for _ in 0..2 {
        let mut s = RunState::new(Uuid::new_v4(), session_a, 1);
        s.step = Step::Done {
            final_text: "a".into(),
        };
        store.save_delta(&s, &[]).await.unwrap();
    }
    let mut sb = RunState::new(Uuid::new_v4(), session_b, 1);
    sb.step = Step::Done {
        final_text: "b".into(),
    };
    store.save_delta(&sb, &[]).await.unwrap();

    let a_runs = store
        .list_runs(RunFilter {
            session_id: Some(session_a),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(a_runs.len(), 2);
    assert!(a_runs.iter().all(|h| h.session_id == session_a));

    let b_runs = store
        .list_runs(RunFilter {
            session_id: Some(session_b),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(b_runs.len(), 1);
}

#[tokio::test]
async fn jsonl_kv_scoped_per_session() {
    let store: Arc<dyn SessionStore> =
        Arc::new(JsonlSessionStore::open(temp_store_root()).await.unwrap());

    let s1 = Uuid::new_v4();
    let s2 = Uuid::new_v4();

    store.kv_put(s1, "name", b"alice").await.unwrap();
    store.kv_put(s2, "name", b"bob").await.unwrap();
    store.kv_put(s1, "pref.lang", b"zh").await.unwrap();

    assert_eq!(
        store.kv_get(s1, "name").await.unwrap().as_deref(),
        Some(b"alice".as_slice())
    );
    assert_eq!(
        store.kv_get(s2, "name").await.unwrap().as_deref(),
        Some(b"bob".as_slice())
    );
    assert!(store.kv_get(s1, "nonexistent").await.unwrap().is_none());

    let list = store.kv_list(s1, "pref.").await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].0, "pref.lang");
}

#[tokio::test]
async fn jsonl_thaw_and_continue() {
    let store: Arc<dyn SessionStore> =
        Arc::new(JsonlSessionStore::open(temp_store_root()).await.unwrap());

    let model1 = Arc::new(CannedModel::new(vec![reply::text("first")]));
    let runner1 = build_runner(model1, store.clone());

    let mut state = new_state();
    let rid = state.run_id;
    runner1
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("round 1"),
            },
        )
        .await
        .unwrap();
    for _ in 0..10 {
        if matches!(state.step, Step::Done { .. }) {
            break;
        }
        runner1.step(&mut state).await.unwrap();
    }
    assert!(matches!(state.step, Step::Done { .. }));
    let seq_after_run1 = state.event_seq;

    drop(runner1);
    let mut loaded = store.load_run(rid).await.unwrap().unwrap();
    assert_eq!(loaded.event_seq, seq_after_run1);

    let model2 = Arc::new(CannedModel::new(vec![reply::text("second")]));
    let runner2 = build_runner(model2, store.clone());

    runner2
        .submit_user_message(
            &mut loaded,
            Message::User {
                content: Content::text("round 2"),
            },
        )
        .await
        .unwrap();
    for _ in 0..10 {
        if matches!(loaded.step, Step::Done { .. }) {
            break;
        }
        runner2.step(&mut loaded).await.unwrap();
    }
    assert!(matches!(loaded.step, Step::Done { .. }));
    assert_eq!(loaded.usage.turns, 2, "should have accumulated two turns");
    assert!(loaded.event_seq > seq_after_run1, "seq should have grown");

    let round2 = store.query_events(rid, seq_after_run1).await.unwrap();
    assert!(!round2.is_empty());
    assert!(round2.iter().all(|e| e.seq() > seq_after_run1));
}

#[tokio::test]
async fn jsonl_optimistic_concurrency_stale() {
    let store: Arc<dyn SessionStore> =
        Arc::new(JsonlSessionStore::open(temp_store_root()).await.unwrap());

    let mut s1 = new_state();
    s1.event_seq = 5;
    store.save_delta(&s1, &[]).await.unwrap();

    let mut s_old = s1.clone();
    s_old.event_seq = 3;
    s_old.step = Step::Done {
        final_text: "stale".into(),
    };
    let err = store.save_delta(&s_old, &[]).await.unwrap_err();
    match err {
        muagent::core::error::StoreError::StaleState { expected, actual } => {
            assert_eq!(expected, 3);
            assert_eq!(actual, 5);
        }
        e => panic!("expected StaleState, got {e:?}"),
    }
}

#[tokio::test]
async fn jsonl_store_rejects_second_writer_for_same_root() {
    let root = temp_store_root();

    let first = JsonlSessionStore::open(&root).await.unwrap();
    let err = match JsonlSessionStore::open(&root).await {
        Ok(_) => panic!("expected second writer to be rejected"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("already open for writing"),
        "unexpected error: {err}"
    );

    drop(first);

    JsonlSessionStore::open(&root)
        .await
        .expect("lock file should be released on drop");
}

#[allow(dead_code)]
fn _keep_json_used() {
    let _ = json!(0);
}
