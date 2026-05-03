//! SessionManager:list / continue / fork / delete sessions。
//!
//! 这是 v3.1 Default Shell 的用户视角:多个 Run 可以共享同一 `session_id`
//! (续聊 / fork)。这里提供 host 友好的 API,**不**改 Runner 核心。

use std::sync::Arc;

use crate::core::error::StoreError;
use crate::core::event::{RunId, SessionId};
use crate::core::prelude::{RunFilter, RunHeader, RunStatus, SessionStore};
use crate::core::run_state::RunState;
use crate::core::step::Step;
use crate::core::types::{Content, Message};

pub struct SessionManager {
    store: Arc<dyn SessionStore>,
}

impl SessionManager {
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self { store }
    }

    /// 创建全新 session(仅返回 SessionId;真实 Run 在 host 调 runner 时建)。
    pub fn new_session(&self) -> SessionId {
        uuid::Uuid::new_v4()
    }

    /// 列出所有 session(按最近更新排序);从 runs 表聚合。
    pub async fn list_sessions(
        &self,
        limit: Option<usize>,
    ) -> Result<Vec<SessionInfo>, StoreError> {
        let runs = self.store.list_runs(RunFilter::default()).await?;

        // Aggregate user-visible runs per session. A freshly allocated
        // RunState is only an in-memory draft; it becomes a session once it
        // has transcript history. This keeps "open TUI then quit" and other
        // empty runs out of resume/session pickers.
        let mut by_sid: std::collections::HashMap<SessionId, SessionAggregate> =
            std::collections::HashMap::new();
        for r in runs {
            let Some(state) = self.store.load_run(r.run_id).await? else {
                continue;
            };
            if !is_user_visible_session_run(&state) {
                continue;
            }
            let entry = by_sid.entry(r.session_id).or_default();
            entry.run_count += 1;
            if r.updated_ms > entry.updated_ms {
                entry.updated_ms = r.updated_ms;
                entry.latest_status = r.status;
                entry.workspace_root = r.workspace_root;
                entry.title = last_user_message_brief(&state.history);
            }
        }
        let mut out: Vec<SessionInfo> = by_sid
            .into_iter()
            .map(|(session_id, agg)| SessionInfo {
                session_id,
                run_count: agg.run_count,
                updated_ms: agg.updated_ms,
                latest_status: agg.latest_status,
                workspace_root: agg.workspace_root,
                title: agg.title,
            })
            .collect();
        out.sort_by_key(|s| -s.updated_ms);
        if let Some(limit) = limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    /// 列出 session 下所有 run(time-ascending,最老在前)。
    pub async fn list_runs_in_session(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<RunHeader>, StoreError> {
        let mut rs = self
            .store
            .list_runs(RunFilter {
                session_id: Some(session_id),
                ..Default::default()
            })
            .await?;
        rs.sort_by_key(|r| r.updated_ms);
        Ok(rs)
    }

    pub async fn list_sessions_for_workspace(
        &self,
        workspace_root: &str,
        limit: Option<usize>,
    ) -> Result<Vec<SessionInfo>, StoreError> {
        let mut sessions = self.list_sessions(None).await?;
        sessions.retain(|s| s.workspace_root.as_deref() == Some(workspace_root));
        if let Some(limit) = limit {
            sessions.truncate(limit);
        }
        Ok(sessions)
    }

    /// 继续已有 session:取其最后一个 run 的 history,开新 run。返回的 RunState 是"空 step"(Ready)。
    pub async fn continue_session(
        &self,
        session_id: SessionId,
        now_ms: i64,
    ) -> Result<RunState, StoreError> {
        let runs = self.list_runs_in_session(session_id).await?;
        let mut latest_visible: Option<(RunHeader, RunState)> = None;
        for run in runs.into_iter().rev() {
            let Some(state) = self.store.load_run(run.run_id).await? else {
                continue;
            };
            if is_user_visible_session_run(&state) {
                latest_visible = Some((run, state));
                break;
            }
        }
        let Some((last, mut prev)) = latest_visible else {
            return Err(StoreError::NotFound);
        };
        if !is_continuable(&prev.step) {
            return Err(StoreError::Transient(
                "latest run is still active; only done/failed/paused runs can be continued".into(),
            ));
        }
        validate_history_prefix(&prev.history)?;

        let mut next = RunState::new(uuid::Uuid::new_v4(), session_id, now_ms);
        next.parent_run_id = Some(last.run_id);
        next.workspace_root = prev.workspace_root.clone();
        prev.ensure_history_ids();
        next.history = prev.history; // inherit full transcript
        next.history_ids = prev.history_ids;
        next.next_message_seq = prev.next_message_seq;
        next.next_checkpoint_seq = prev.next_checkpoint_seq;
        next.compaction_checkpoints = prev.compaction_checkpoints;
        Ok(next)
    }

    /// Fork:从某个 Run 的前 N 个 message 分叉新 Run(新 session_id)。
    /// 注意:fork 产生新的 session_id,避免"一棵树里两条并发时间线"混淆。
    pub async fn fork_from(
        &self,
        run_id: RunId,
        at_message_index: usize,
        now_ms: i64,
    ) -> Result<RunState, StoreError> {
        let mut prev = self
            .store
            .load_run(run_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        let cut = at_message_index.min(prev.history.len());
        validate_history_prefix(&prev.history[..cut])?;
        prev.ensure_history_ids();

        let mut next = RunState::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), now_ms);
        next.parent_run_id = Some(run_id);
        next.workspace_root = prev.workspace_root.clone();
        next.history = prev.history[..cut].to_vec();
        next.history_ids = prev.history_ids[..cut].to_vec();
        next.next_message_seq = prev.next_message_seq;
        next.next_checkpoint_seq = prev.next_checkpoint_seq;
        let kept_ids = next
            .history_ids
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        next.compaction_checkpoints = prev
            .compaction_checkpoints
            .into_iter()
            .filter(|checkpoint| kept_ids.contains(&checkpoint.summary_message_id))
            .collect();
        Ok(next)
    }

    /// 删除整个 session(所有 run + 它们的 events)。
    pub async fn delete_session(&self, session_id: SessionId) -> Result<u32, StoreError> {
        let runs = self.list_runs_in_session(session_id).await?;
        let n = runs.len() as u32;
        for r in runs {
            self.store.delete_run(r.run_id).await?;
        }
        Ok(n)
    }

    /// 朴素文本搜索:扫描 session 下所有 run 的 history,找含 keyword 的。
    /// v3.1 MVP:线性扫;后续换向量检索 addon。
    pub async fn search(
        &self,
        query: &str,
        limit: Option<usize>,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let sessions = self.list_sessions(None).await?;
        let mut out = Vec::new();
        for s in &sessions {
            let runs = self.list_runs_in_session(s.session_id).await?;
            let mut histories_by_run: std::collections::HashMap<RunId, Vec<Message>> =
                std::collections::HashMap::new();
            for r in runs {
                if let Some(state) = self.store.load_run(r.run_id).await? {
                    let scan_from = inherited_prefix_len(&state, &histories_by_run);
                    for (i, m) in state.history.iter().enumerate().skip(scan_from) {
                        if message_contains(m, query) {
                            out.push(SearchHit {
                                session_id: s.session_id,
                                run_id: r.run_id,
                                message_index: i,
                                brief: message_brief(m),
                            });
                        }
                    }
                    histories_by_run.insert(r.run_id, state.history);
                }
            }
        }
        if let Some(l) = limit {
            out.truncate(l);
        }
        Ok(out)
    }
}

fn message_contains(m: &Message, q: &str) -> bool {
    match m {
        Message::User { content }
        | Message::System { content }
        | Message::Assistant { content, .. } => content_text(content).contains(q),
        Message::ToolResult { result, .. } => result.text().contains(q),
        Message::Observation { text, .. } => text.contains(q),
    }
}

fn inherited_prefix_len(
    state: &RunState,
    histories_by_run: &std::collections::HashMap<RunId, Vec<Message>>,
) -> usize {
    let Some(parent_run_id) = state.parent_run_id else {
        return 0;
    };
    let Some(parent_history) = histories_by_run.get(&parent_run_id) else {
        return 0;
    };
    if state.history.len() >= parent_history.len()
        && state
            .history
            .as_slice()
            .starts_with(parent_history.as_slice())
    {
        parent_history.len()
    } else {
        0
    }
}

fn validate_history_prefix(history: &[Message]) -> Result<(), StoreError> {
    use std::collections::BTreeSet;

    let mut pending: BTreeSet<String> = BTreeSet::new();
    for (idx, msg) in history.iter().enumerate() {
        match msg {
            Message::Assistant { tool_calls, .. } => {
                if !pending.is_empty() {
                    return Err(StoreError::Transient(format!(
                        "history becomes invalid at message {idx}: a new assistant turn appears before all tool_results for the previous tool batch",
                    )));
                }
                pending.extend(tool_calls.iter().map(|call| call.id.clone()));
            }
            Message::ToolResult { call_id, .. } if !pending.remove(call_id) => {
                return Err(StoreError::Transient(format!(
                    "history becomes invalid at message {idx}: tool_result `{call_id}` has no matching pending tool_call",
                )));
            }
            Message::ToolResult { .. } => {}
            _ if !pending.is_empty() => {
                return Err(StoreError::Transient(format!(
                    "history becomes invalid at message {idx}: tool_results must immediately follow an assistant tool batch",
                )));
            }
            _ => {}
        }
    }

    if pending.is_empty() {
        Ok(())
    } else {
        let missing = pending.into_iter().collect::<Vec<_>>().join(", ");
        Err(StoreError::Transient(format!(
            "history ends with unresolved tool_calls: {missing}",
        )))
    }
}

fn content_text(c: &Content) -> String {
    match c {
        Content::Text(s) => s.clone(),
        Content::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                crate::core::types::ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn message_brief(m: &Message) -> String {
    let full = match m {
        Message::User { content }
        | Message::System { content }
        | Message::Assistant { content, .. } => content_text(content),
        Message::ToolResult { result, .. } => result.text(),
        Message::Observation { text, .. } => text.clone(),
    };
    full.chars().take(120).collect()
}

fn last_user_message_brief(history: &[Message]) -> Option<String> {
    history.iter().rev().find_map(|message| {
        let Message::User { content } = message else {
            return None;
        };
        let text = content_text(content);
        let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if compact.is_empty() {
            None
        } else {
            Some(compact.chars().take(120).collect())
        }
    })
}

pub struct SessionInfo {
    pub session_id: SessionId,
    pub run_count: u32,
    pub updated_ms: i64,
    pub latest_status: RunStatus,
    pub workspace_root: Option<String>,
    pub title: Option<String>,
}

/// Per-session accumulator used inside `list_sessions`. Replaces a
/// `(u32, i64, RunStatus, Option<String>, Option<RunId>, Option<String>)`
/// tuple that clippy (rightly) flagged as too anonymous.
#[derive(Default)]
struct SessionAggregate {
    run_count: u32,
    updated_ms: i64,
    latest_status: RunStatus,
    workspace_root: Option<String>,
    title: Option<String>,
}

pub struct SearchHit {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub message_index: usize,
    pub brief: String,
}

/// 判断某 Run 是否处于"可以继续"的状态。
pub fn is_continuable(step: &Step) -> bool {
    matches!(
        step,
        Step::Done { .. } | Step::Failed { .. } | Step::Paused { .. }
    )
}

fn is_user_visible_session_run(state: &RunState) -> bool {
    !state.history.is_empty()
}
