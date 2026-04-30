//! M1-P4 测试:SessionManager + SessionArchive。

use std::sync::Arc;

use muagent::core::prelude::*;
use muagent::core::step::Step;
use muagent::core::tool::PendingCall;
use muagent::core::types::{Content, Message, ObsKind};
use muagent::prelude::*;
use muagent::storage::MemorySessionStore;
use serde_json::json;
use uuid::Uuid;

fn state(session: Uuid) -> RunState {
    RunState::new(Uuid::new_v4(), session, 100)
}

// ============ SessionManager tests ============

#[tokio::test]
async fn session_list_aggregates_runs() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let mgr = SessionManager::new(store.clone());

    let s1 = mgr.new_session();
    let s2 = mgr.new_session();

    // s1 has 2 runs
    let mut r1 = state(s1);
    r1.step = Step::Done {
        final_text: "a".into(),
    };
    r1.updated_ms = 100;
    store.save_delta(&r1, &[]).await.unwrap();

    let mut r2 = state(s1);
    r2.step = Step::Done {
        final_text: "b".into(),
    };
    r2.updated_ms = 200;
    store.save_delta(&r2, &[]).await.unwrap();

    // s2 has 1
    let mut r3 = state(s2);
    r3.step = Step::Paused {
        reason: muagent::core::step::PauseReason::HostRequested,
    };
    r3.updated_ms = 300;
    store.save_delta(&r3, &[]).await.unwrap();

    let infos = mgr.list_sessions(None).await.unwrap();
    assert_eq!(infos.len(), 2);
    // s2 is most recent
    assert_eq!(infos[0].session_id, s2);
    assert_eq!(infos[1].session_id, s1);
    assert_eq!(infos[1].run_count, 2);
}

#[tokio::test]
async fn session_continue_inherits_history() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let mgr = SessionManager::new(store.clone());
    let sid = mgr.new_session();

    let mut prev = state(sid);
    prev.history = vec![
        Message::User {
            content: Content::text("first"),
        },
        Message::Assistant {
            content: Content::text("ok"),
            tool_calls: vec![],
            thinking: vec![],
        },
    ];
    prev.step = Step::Done {
        final_text: "ok".into(),
    };
    prev.ensure_history_ids();
    let prev_ids = prev.history_ids.clone();
    store.save_delta(&prev, &[]).await.unwrap();

    let next = mgr.continue_session(sid, 200).await.unwrap();
    assert_eq!(next.session_id, sid);
    assert_eq!(next.parent_run_id, Some(prev.run_id));
    assert_eq!(next.history.len(), 2);
    assert_eq!(next.history_ids, prev_ids);
    assert!(matches!(next.step, Step::Ready));
}

#[tokio::test]
async fn session_continue_rejects_active_run() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let mgr = SessionManager::new(store.clone());
    let sid = mgr.new_session();

    let mut active = state(sid);
    active.history.push(Message::User {
        content: Content::text("still running"),
    });
    active.step = Step::ModelTurn;
    store.save_delta(&active, &[]).await.unwrap();

    let err = mgr.continue_session(sid, 200).await.unwrap_err();
    assert!(err.to_string().contains("done/failed/paused"));
}

#[tokio::test]
async fn session_fork_cuts_history() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let mgr = SessionManager::new(store.clone());
    let sid = mgr.new_session();

    let mut prev = state(sid);
    prev.history = vec![
        Message::User {
            content: Content::text("1"),
        },
        Message::Assistant {
            content: Content::text("2"),
            tool_calls: vec![],
            thinking: vec![],
        },
        Message::User {
            content: Content::text("3"),
        },
        Message::Assistant {
            content: Content::text("4"),
            tool_calls: vec![],
            thinking: vec![],
        },
    ];
    prev.step = Step::Done {
        final_text: "4".into(),
    };
    prev.ensure_history_ids();
    let prev_ids = prev.history_ids.clone();
    store.save_delta(&prev, &[]).await.unwrap();

    let forked = mgr.fork_from(prev.run_id, 2, 500).await.unwrap();
    assert_ne!(forked.session_id, sid, "fork 产生新 session");
    assert_eq!(forked.parent_run_id, Some(prev.run_id));
    assert_eq!(forked.history.len(), 2);
    assert_eq!(forked.history_ids, prev_ids[..2].to_vec());
}

#[tokio::test]
async fn session_fork_rejects_cut_inside_tool_batch() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let mgr = SessionManager::new(store.clone());
    let sid = mgr.new_session();

    let mut prev = state(sid);
    prev.history = vec![
        Message::User {
            content: Content::text("read file"),
        },
        Message::Assistant {
            content: Content::text("calling tool"),
            tool_calls: vec![PendingCall::new(
                "call_1",
                "fs_read",
                json!({"path":"file.txt"}),
            )],
            thinking: vec![],
        },
        Message::ToolResult {
            call_id: "call_1".into(),
            result: ToolResult::ok("file contents"),
        },
    ];
    prev.step = Step::Done {
        final_text: "done".into(),
    };
    store.save_delta(&prev, &[]).await.unwrap();

    let err = mgr.fork_from(prev.run_id, 2, 500).await.unwrap_err();
    assert!(err.to_string().contains("unresolved tool_calls"));
}

#[tokio::test]
async fn session_search_finds_hits() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let mgr = SessionManager::new(store.clone());
    let sid = mgr.new_session();

    let mut r = state(sid);
    r.history = vec![
        Message::User {
            content: Content::text("coffee please"),
        },
        Message::Assistant {
            content: Content::text("ok ordered coffee"),
            tool_calls: vec![],
            thinking: vec![],
        },
        Message::User {
            content: Content::text("tea this time"),
        },
    ];
    r.step = Step::Done {
        final_text: "ok".into(),
    };
    store.save_delta(&r, &[]).await.unwrap();

    let hits = mgr.search("coffee", None).await.unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h.session_id == sid));
}

#[tokio::test]
async fn session_search_dedups_inherited_history() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let mgr = SessionManager::new(store.clone());
    let sid = mgr.new_session();

    let mut r1 = state(sid);
    r1.history = vec![
        Message::User {
            content: Content::text("coffee please"),
        },
        Message::Assistant {
            content: Content::text("ok ordered coffee"),
            tool_calls: vec![],
            thinking: vec![],
        },
    ];
    r1.step = Step::Done {
        final_text: "ok".into(),
    };
    r1.updated_ms = 100;
    store.save_delta(&r1, &[]).await.unwrap();

    let mut r2 = mgr.continue_session(sid, 200).await.unwrap();
    r2.history.push(Message::User {
        content: Content::text("tea this time"),
    });
    r2.step = Step::Done {
        final_text: "tea".into(),
    };
    r2.updated_ms = 200;
    store.save_delta(&r2, &[]).await.unwrap();

    let hits = mgr.search("coffee", None).await.unwrap();
    assert_eq!(hits.len(), 2, "shared prefix should only be scanned once");
    assert!(hits.iter().all(|h| h.run_id == r1.run_id));
}

#[tokio::test]
async fn session_list_limit_applies_after_session_aggregation() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let mgr = SessionManager::new(store.clone());
    let s1 = mgr.new_session();
    let s2 = mgr.new_session();
    let s3 = mgr.new_session();

    for updated_ms in [300, 200] {
        let mut r = state(s1);
        r.step = Step::Done {
            final_text: "x".into(),
        };
        r.updated_ms = updated_ms;
        store.save_delta(&r, &[]).await.unwrap();
    }

    let mut r2 = state(s2);
    r2.step = Step::Done {
        final_text: "y".into(),
    };
    r2.updated_ms = 100;
    store.save_delta(&r2, &[]).await.unwrap();

    let mut r3 = state(s3);
    r3.step = Step::Done {
        final_text: "z".into(),
    };
    r3.updated_ms = 50;
    store.save_delta(&r3, &[]).await.unwrap();

    let infos = mgr.list_sessions(Some(2)).await.unwrap();
    assert_eq!(infos.len(), 2);
    assert_eq!(infos[0].session_id, s1);
    assert_eq!(infos[1].session_id, s2);
}

#[tokio::test]
async fn session_delete_cascades() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let mgr = SessionManager::new(store.clone());
    let sid = mgr.new_session();

    let mut r = state(sid);
    r.step = Step::Done {
        final_text: "x".into(),
    };
    store.save_delta(&r, &[]).await.unwrap();

    let n = mgr.delete_session(sid).await.unwrap();
    assert_eq!(n, 1);
    assert!(mgr.list_runs_in_session(sid).await.unwrap().is_empty());
}

// ============ SessionArchive tests ============

fn tempdir() -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("muagent-archive-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[tokio::test]
async fn archive_append_and_meta() {
    let dir = tempdir();
    let archive = SessionArchive::new(ArchiveConfig {
        root: dir.clone(),
        enabled: true,
        rotation: ArchiveRotation {
            max_bytes: 10 * 1024 * 1024, // never rotate
            max_turns: 10_000,
            max_messages: 10_000,
        },
    });

    let sid = Uuid::new_v4();
    let mut rs = RunState::new(Uuid::new_v4(), sid, 100);
    rs.history = vec![
        Message::User {
            content: Content::text("first"),
        },
        Message::Assistant {
            content: Content::text("hi"),
            tool_calls: vec![],
            thinking: vec![],
        },
    ];
    rs.usage.turns = 1;

    let outcome = archive.apply(&rs).await.unwrap();
    assert!(outcome.appended_bytes > 0);
    assert_eq!(outcome.archived_msg_count, 2);
    assert!(outcome.rotated.is_none());

    // Files exist
    let session_dir = dir.join(sid.to_string());
    assert!(session_dir.join("transcript-current.jsonl").exists());
    assert!(session_dir.join("meta.json").exists());

    // Second apply appends only new messages
    rs.history.push(Message::User {
        content: Content::text("again"),
    });
    let outcome2 = archive.apply(&rs).await.unwrap();
    assert!(outcome2.appended_bytes > 0);
    assert!(outcome2.appended_bytes < outcome.appended_bytes); // only 1 msg vs 2
    assert_eq!(outcome2.archived_msg_count, 3);

    // Cleanup
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn archive_writes_summary_snapshot() {
    let dir = tempdir();
    let archive = SessionArchive::new(ArchiveConfig {
        root: dir.clone(),
        enabled: true,
        rotation: ArchiveRotation {
            max_bytes: 10 * 1024 * 1024,
            max_turns: 10_000,
            max_messages: 10_000,
        },
    });

    let sid = Uuid::new_v4();
    let mut rs = RunState::new(Uuid::new_v4(), sid, 100);
    rs.history = vec![
        Message::User {
            content: Content::text("first"),
        },
        Message::Observation {
            kind: ObsKind::Summary,
            text: "remember ALPHA-42".into(),
        },
    ];
    rs.ensure_history_ids();

    archive.apply(&rs).await.unwrap();

    let session_dir = dir.join(sid.to_string());
    let summary = std::fs::read_to_string(session_dir.join("summary.txt")).unwrap();
    assert_eq!(summary, "remember ALPHA-42");

    let summaries = std::fs::read_to_string(session_dir.join("summaries.jsonl")).unwrap();
    assert!(summaries.contains("\"message_index\":1"));
    assert!(summaries.contains("\"message_id\":\"m"));
    assert!(summaries.contains("ALPHA-42"));

    let meta: SessionMeta =
        serde_json::from_str(&std::fs::read_to_string(session_dir.join("meta.json")).unwrap())
            .unwrap();
    assert_eq!(meta.summary_count, 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn archive_continues_after_compaction_shrinks_history() {
    let dir = tempdir();
    let archive = SessionArchive::new(ArchiveConfig {
        root: dir.clone(),
        enabled: true,
        rotation: ArchiveRotation {
            max_bytes: 10 * 1024 * 1024,
            max_turns: 10_000,
            max_messages: 10_000,
        },
    });

    let sid = Uuid::new_v4();
    let mut rs = RunState::new(Uuid::new_v4(), sid, 100);
    rs.history = vec![
        Message::User {
            content: Content::text("old-1"),
        },
        Message::Assistant {
            content: Content::text("old-2"),
            tool_calls: vec![],
            thinking: vec![],
        },
        Message::User {
            content: Content::text("old-3"),
        },
        Message::Assistant {
            content: Content::text("old-4"),
            tool_calls: vec![],
            thinking: vec![],
        },
    ];
    archive.apply(&rs).await.unwrap();

    rs.history = vec![
        Message::Observation {
            kind: ObsKind::Summary,
            text: "compacted old turns".into(),
        },
        Message::User {
            content: Content::text("tail"),
        },
    ];
    let shrink = archive.apply(&rs).await.unwrap();
    assert_eq!(
        shrink.archived_msg_count, 4,
        "shrinking working history should not delete raw transcript rows"
    );

    rs.history.push(Message::User {
        content: Content::text("new-after-compact"),
    });
    let after = archive.apply(&rs).await.unwrap();
    assert!(after.appended_bytes > 0);
    assert_eq!(after.archived_msg_count, 5);

    let current =
        std::fs::read_to_string(dir.join(sid.to_string()).join("transcript-current.jsonl"))
            .unwrap();
    assert_eq!(current.lines().count(), 5);
    assert!(current.contains("new-after-compact"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn archive_rotation_triggers() {
    let dir = tempdir();
    let archive = SessionArchive::new(ArchiveConfig {
        root: dir.clone(),
        enabled: true,
        rotation: ArchiveRotation {
            max_bytes: 10_000_000,
            max_turns: 10,
            max_messages: 3, // rotate after 3 messages
        },
    });

    let sid = Uuid::new_v4();
    let mut rs = RunState::new(Uuid::new_v4(), sid, 100);
    rs.history = vec![
        Message::User {
            content: Content::text("1"),
        },
        Message::Assistant {
            content: Content::text("2"),
            tool_calls: vec![],
            thinking: vec![],
        },
        Message::User {
            content: Content::text("3"),
        },
        Message::Assistant {
            content: Content::text("4"),
            tool_calls: vec![],
            thinking: vec![],
        },
    ];
    let outcome = archive.apply(&rs).await.unwrap();
    assert_eq!(outcome.archived_msg_count, 4);
    assert!(
        outcome.rotated.is_some(),
        "should have rotated since messages>=3"
    );

    let parts_jsonl = dir.join(sid.to_string()).join("parts.jsonl");
    let parts_content = std::fs::read_to_string(&parts_jsonl).unwrap();
    assert!(parts_content.lines().count() >= 1);
    assert!(dir
        .join(sid.to_string())
        .join("transcript-0001.jsonl")
        .exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn archive_prompt_augmentation() {
    let dir = tempdir();
    let archive = SessionArchive::new(ArchiveConfig {
        root: dir.clone(),
        enabled: true,
        rotation: ArchiveRotation::default(),
    });
    let s = archive.prompt_augmentation(Some(Uuid::new_v4()));
    assert!(s.contains("Session archive"));
    assert!(s.contains(&dir.display().to_string()));
    assert!(s.contains("parts.jsonl"));
    assert!(s.contains("summary.txt"));
    assert!(s.contains("summaries.jsonl"));
    assert!(s.contains("transcript-"));
    let _ = std::fs::remove_dir_all(&dir);
}
