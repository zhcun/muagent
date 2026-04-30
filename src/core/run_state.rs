//! RunState(durable,含 schema_version + migration)+ Usage。

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use crate::core::error::StoreError;
use crate::core::event::{EventSeq, RunId, SessionId};
use crate::core::step::Step;
use crate::core::tool::ToolResult;
use crate::core::types::{Content, Message, ObsKind};

pub const CURRENT_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RunState {
    pub schema_version: u32,

    pub run_id: RunId,
    pub session_id: SessionId,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<RunId>,

    pub step: Step,
    pub history: Vec<Message>,
    /// Stable IDs parallel to `history`.
    ///
    /// We keep IDs outside `Message` so provider wire schemas and existing
    /// transcript JSON stay stable. The ledger lets compaction checkpoints refer
    /// to durable message identities instead of fragile post-compaction indexes.
    #[serde(default)]
    pub history_ids: Vec<String>,
    #[serde(default)]
    pub next_message_seq: u64,
    #[serde(default)]
    pub next_checkpoint_seq: u64,
    #[serde(default)]
    pub compaction_checkpoints: Vec<CompactionCheckpoint>,
    pub event_seq: EventSeq,
    pub usage: Usage,

    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionCheckpoint {
    pub checkpoint_id: String,
    pub summary_message_id: String,
    #[serde(default, skip_serializing_if = "MessageIdRange::is_empty")]
    pub removed_message_range: MessageIdRange,
    #[serde(default, skip_serializing_if = "MessageIdRange::is_empty")]
    pub summary_input_message_range: MessageIdRange,
    /// Deprecated compatibility field. New checkpoints use
    /// `removed_message_range` to avoid growing RunState snapshots linearly
    /// with every compacted message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_message_ids: Vec<String>,
    /// Deprecated compatibility field. New checkpoints use
    /// `summary_input_message_range`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summary_input_message_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_kept_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_message_ids: Vec<String>,
    pub replaced_turns: usize,
    pub replaced_messages: usize,
    pub tokens_before: u32,
    pub tokens_after: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageIdRange {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message_id: Option<String>,
    pub count: usize,
}

impl MessageIdRange {
    pub fn from_ids(ids: &[String]) -> Self {
        Self {
            first_message_id: ids.first().cloned(),
            last_message_id: ids.last().cloned(),
            count: ids.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0 && self.first_message_id.is_none() && self.last_message_id.is_none()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub tokens_prompt: u32,
    pub tokens_completion: u32,
    pub cost_usd: f64,
    pub turns: u32,
    pub tool_calls: u32,
    /// Accumulated cached input tokens READ across all turns (cheap reuse).
    #[serde(default)]
    pub tokens_cache_read: u32,
    /// Accumulated cached input tokens WRITTEN across all turns (one-time
    /// cost when a new breakpoint is introduced).
    #[serde(default)]
    pub tokens_cache_write: u32,
    /// Accumulated reasoning / thinking tokens billed across all turns.
    #[serde(default)]
    pub tokens_thinking: u32,
}

impl RunState {
    pub fn new(run_id: RunId, session_id: SessionId, now_ms: i64) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA,
            run_id,
            session_id,
            parent_run_id: None,
            step: Step::Ready,
            history: Vec::new(),
            history_ids: Vec::new(),
            next_message_seq: 1,
            next_checkpoint_seq: 1,
            compaction_checkpoints: Vec::new(),
            event_seq: 0,
            usage: Usage::default(),
            created_ms: now_ms,
            updated_ms: now_ms,
        }
    }

    pub fn ensure_history_ids(&mut self) {
        if self.next_message_seq == 0 {
            self.next_message_seq = 1;
        }
        self.reconcile_next_message_seq();
        if self.history_ids.len() > self.history.len() {
            self.history_ids.truncate(self.history.len());
        }
        while self.history_ids.len() < self.history.len() {
            let id = self.allocate_message_id();
            self.history_ids.push(id);
        }
    }

    pub fn allocate_message_id(&mut self) -> String {
        if self.next_message_seq == 0 {
            self.next_message_seq = 1;
        }
        let seq = self.next_message_seq;
        self.next_message_seq = self.next_message_seq.saturating_add(1);
        format!("m{seq:012}")
    }

    pub fn allocate_checkpoint_id(&mut self) -> String {
        if self.next_checkpoint_seq == 0 {
            self.next_checkpoint_seq = 1;
        }
        let seq = self.next_checkpoint_seq;
        self.next_checkpoint_seq = self.next_checkpoint_seq.saturating_add(1);
        format!("c{seq:012}")
    }

    pub fn push_message(&mut self, msg: Message) {
        self.ensure_history_ids();
        let id = self.allocate_message_id();
        self.history.push(msg);
        self.history_ids.push(id);
    }

    pub fn replace_history_with_ids(&mut self, history: Vec<Message>, history_ids: Vec<String>) {
        self.history = history;
        self.history_ids = history_ids;
        self.ensure_history_ids();
    }

    pub fn record_compaction_checkpoint(&mut self, checkpoint: CompactionCheckpoint) {
        self.retain_active_compaction_checkpoints();
        self.compaction_checkpoints.push(checkpoint);
    }

    pub fn retain_active_compaction_checkpoints(&mut self) {
        let active_ids = self.history_ids.iter().collect::<BTreeSet<_>>();
        self.compaction_checkpoints
            .retain(|checkpoint| active_ids.contains(&checkpoint.summary_message_id));
    }

    pub fn validate_history_identity(&self) -> Result<(), String> {
        if self.history.len() != self.history_ids.len() {
            return Err(format!(
                "history length {} != history_ids length {}",
                self.history.len(),
                self.history_ids.len()
            ));
        }

        let mut seen = BTreeSet::new();
        for (idx, id) in self.history_ids.iter().enumerate() {
            if id.trim().is_empty() {
                return Err(format!("history_ids[{idx}] is empty"));
            }
            if !seen.insert(id) {
                return Err(format!("duplicate history id `{id}`"));
            }
        }

        let checkpoint_ids = self
            .compaction_checkpoints
            .iter()
            .map(|checkpoint| checkpoint.checkpoint_id.as_str())
            .collect::<Vec<_>>();
        let mut seen_checkpoints = BTreeSet::new();
        for id in checkpoint_ids {
            if id.trim().is_empty() {
                return Err("compaction checkpoint id is empty".into());
            }
            if !seen_checkpoints.insert(id) {
                return Err(format!("duplicate compaction checkpoint id `{id}`"));
            }
        }

        let active_ids = self.history_ids.iter().collect::<BTreeSet<_>>();
        for checkpoint in &self.compaction_checkpoints {
            let Some(summary_index) = self
                .history_ids
                .iter()
                .position(|id| id == &checkpoint.summary_message_id)
            else {
                return Err(format!(
                    "checkpoint `{}` references missing summary message id `{}`",
                    checkpoint.checkpoint_id, checkpoint.summary_message_id
                ));
            };
            match &self.history[summary_index] {
                Message::Observation {
                    kind: ObsKind::Summary,
                    ..
                } => {}
                _ => {
                    return Err(format!(
                        "checkpoint `{}` summary_message_id `{}` does not point to a summary observation",
                        checkpoint.checkpoint_id, checkpoint.summary_message_id
                    ));
                }
            }

            if checkpoint.removed_message_range.count != 0
                && checkpoint.removed_message_range.count != checkpoint.replaced_messages
            {
                return Err(format!(
                    "checkpoint `{}` removed range count {} != replaced_messages {}",
                    checkpoint.checkpoint_id,
                    checkpoint.removed_message_range.count,
                    checkpoint.replaced_messages
                ));
            }
            if checkpoint.removed_message_range.count == 0
                && !checkpoint.removed_message_ids.is_empty()
                && checkpoint.removed_message_ids.len() != checkpoint.replaced_messages
            {
                return Err(format!(
                    "checkpoint `{}` removed id list length {} != replaced_messages {}",
                    checkpoint.checkpoint_id,
                    checkpoint.removed_message_ids.len(),
                    checkpoint.replaced_messages
                ));
            }
            for pinned_id in &checkpoint.pinned_message_ids {
                if !active_ids.contains(pinned_id) {
                    return Err(format!(
                        "checkpoint `{}` pinned missing message id `{pinned_id}`",
                        checkpoint.checkpoint_id
                    ));
                }
            }
        }

        Ok(())
    }

    fn reconcile_next_message_seq(&mut self) {
        let max_seen = self
            .history_ids
            .iter()
            .filter_map(|id| id.strip_prefix('m'))
            .filter_map(|digits| digits.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        self.next_message_seq = self.next_message_seq.max(max_seen.saturating_add(1)).max(1);
    }

    /// 递增 seq 并返回新值(用于下一个 event)。
    pub fn next_seq(&mut self) -> EventSeq {
        self.event_seq = self.event_seq.saturating_add(1);
        self.event_seq
    }

    pub fn push_assistant(
        &mut self,
        text: impl Into<String>,
        tool_calls: Vec<crate::core::tool::PendingCall>,
    ) {
        self.push_message(Message::Assistant {
            content: Content::Text(text.into()),
            tool_calls,
            thinking: vec![],
        });
    }

    /// Push an assistant turn with reasoning artifacts attached. Use this
    /// when the adapter returned `ModelReply::thinking` — Runner does.
    pub fn push_assistant_with_thinking(
        &mut self,
        text: impl Into<String>,
        tool_calls: Vec<crate::core::tool::PendingCall>,
        thinking: Vec<crate::core::thinking::ThinkingArtifact>,
    ) {
        self.push_message(Message::Assistant {
            content: Content::Text(text.into()),
            tool_calls,
            thinking,
        });
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.push_message(Message::User {
            content: Content::Text(text.into()),
        });
    }

    pub fn push_tool_result(&mut self, call_id: &str, r: &ToolResult) {
        self.push_message(Message::ToolResult {
            call_id: call_id.to_string(),
            result: r.clone(),
        });
    }

    pub fn push_observation(&mut self, kind: ObsKind, text: impl Into<String>) {
        self.push_message(Message::Observation {
            kind,
            text: text.into(),
        });
    }

    /// Thaw 时从 JSON 按 schema_version migrate 到 CURRENT_SCHEMA。
    pub fn migrate_from(raw: serde_json::Value, from: u32) -> Result<Self, StoreError> {
        match from {
            v if v == CURRENT_SCHEMA => {
                let mut state: Self =
                    serde_json::from_value(raw).map_err(|e| StoreError::Corrupt(e.to_string()))?;
                state.ensure_history_ids();
                Ok(state)
            }
            0 => {
                // Example future path: v0 → v1 field rename/default
                Err(StoreError::Corrupt("v0 migration not implemented".into()))
            }
            v if v > CURRENT_SCHEMA => Err(StoreError::Incompatible {
                found: v,
                supported_max: CURRENT_SCHEMA,
            }),
            _ => Err(StoreError::Corrupt(format!("unknown schema {from}"))),
        }
    }
}
