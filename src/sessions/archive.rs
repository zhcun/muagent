//! SessionArchive:明文 JSONL 存档 + parts 分片。
//!
//! v3.1 设计文档 14.8 规定:每个 session 一个目录,含:
//! - `meta.json`:{title, timestamps, part_count, archived_msg_count, summary_count, status}
//! - `parts.jsonl`:每行一 part 索引 {part, file, turn_range, bytes, brief}
//! - `transcript-NNNN.jsonl`:已封存 part
//! - `transcript-current.jsonl`:活跃 part
//! - `summary.txt`:当前最新压缩摘要
//! - `summaries.jsonl`:当前 RunState 中所有 summary observation 的索引快照

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::core::event::SessionId;
use crate::core::run_state::RunState;
use crate::core::types::{Message, ObsKind};

use crate::adapters::{FileSystem, Uri};
use crate::sessions::token_estimate;

// ============ 配置 / 数据结构 ============

#[derive(Clone, Debug)]
pub struct ArchiveConfig {
    /// archive 根(FS 抽象路径,而非 OS 路径;M1-P4 先用 OS 直接实现,后续换 FS adapter)
    pub root: PathBuf,
    pub enabled: bool,
    pub rotation: ArchiveRotation,
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        Self {
            root: std::env::temp_dir().join("muagent-archive"),
            enabled: true,
            rotation: ArchiveRotation::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ArchiveRotation {
    pub max_bytes: u64,
    pub max_turns: u32,
    pub max_messages: u32,
}

impl Default for ArchiveRotation {
    fn default() -> Self {
        Self {
            max_bytes: 512 * 1024,
            max_turns: 50,
            max_messages: 200,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionMeta {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub created_ms: i64,
    #[serde(default)]
    pub updated_ms: i64,
    #[serde(default)]
    pub turn_count: u32,
    #[serde(default)]
    pub part_count: u32,
    #[serde(default)]
    pub archived_msg_count: u32,
    /// Length of the compactable in-memory history seen on the previous apply.
    ///
    /// `archived_msg_count` is append-only transcript rows. This field tracks
    /// the current RunState shape, which can shrink after compaction.
    #[serde(default)]
    pub last_history_len: u32,
    #[serde(default)]
    pub summary_count: u32,
    /// Message index at which the current part started (0 if no rotations yet).
    #[serde(default)]
    pub current_part_start_msg: u32,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub topics: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PartInfo {
    pub part: u32,
    pub file: String,
    pub turn_start: u32,
    pub turn_end: u32,
    pub bytes: u64,
    #[serde(default)]
    pub brief: String,
    pub sealed_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SummaryInfo {
    pub message_index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_kept_message_id: Option<String>,
    pub tokens_estimate: u32,
    pub updated_ms: i64,
    pub text: String,
}

// ============ SessionArchive ============

/// 基于 OS 文件系统直接实现(M1-P4 简化版,不经 FS adapter,便于测试)。
/// 后续可以 refactor 让它走 `Arc<dyn FileSystem>`。
pub struct SessionArchive {
    cfg: ArchiveConfig,
}

impl SessionArchive {
    pub fn new(cfg: ArchiveConfig) -> Self {
        Self { cfg }
    }

    pub fn root(&self) -> &Path {
        &self.cfg.root
    }

    fn session_dir(&self, sid: SessionId) -> PathBuf {
        self.cfg.root.join(sid.to_string())
    }

    fn current_path(&self, sid: SessionId) -> PathBuf {
        self.session_dir(sid).join("transcript-current.jsonl")
    }

    fn meta_path(&self, sid: SessionId) -> PathBuf {
        self.session_dir(sid).join("meta.json")
    }

    fn parts_path(&self, sid: SessionId) -> PathBuf {
        self.session_dir(sid).join("parts.jsonl")
    }

    fn summary_path(&self, sid: SessionId) -> PathBuf {
        self.session_dir(sid).join("summary.txt")
    }

    fn summaries_path(&self, sid: SessionId) -> PathBuf {
        self.session_dir(sid).join("summaries.jsonl")
    }

    /// 同步 RunState 到 archive:append 自上次以来新增的 messages,更新 meta,必要时 rotate。
    pub async fn apply(&self, state: &RunState) -> Result<ApplyOutcome, std::io::Error> {
        if !self.cfg.enabled {
            return Ok(ApplyOutcome::default());
        }

        let dir = self.session_dir(state.session_id);
        tokio::fs::create_dir_all(&dir).await?;

        // Load or init meta
        let mut meta = self.load_meta(state.session_id).await.unwrap_or_default();
        if meta.created_ms == 0 {
            meta.created_ms = state.created_ms;
        }
        meta.updated_ms = state.updated_ms;
        if meta.last_history_len == 0 && meta.archived_msg_count > 0 {
            meta.last_history_len = meta.archived_msg_count;
        }

        // Append new messages
        let mut start = meta.last_history_len as usize;
        if start > state.history.len() {
            // Compaction has shrunk/replaced the working history. The older raw
            // transcript rows remain archived; reset only the in-memory cursor
            // so future new messages can still append.
            start = state.history.len();
        }
        let mut appended_bytes = 0u64;
        if start < state.history.len() {
            let current = self.current_path(state.session_id);
            let mut f = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&current)
                .await?;
            for msg in &state.history[start..] {
                let line =
                    serde_json::to_string(msg).map_err(|e| std::io::Error::other(e.to_string()))?;
                f.write_all(line.as_bytes()).await?;
                f.write_all(b"\n").await?;
                appended_bytes += line.len() as u64 + 1;
            }
            f.flush().await?;
            meta.archived_msg_count = meta
                .archived_msg_count
                .saturating_add((state.history.len() - start) as u32);
        }
        meta.last_history_len = state.history.len() as u32;
        meta.turn_count = state.usage.turns;
        meta.status = status_label(state);

        // Rotation check
        let mut rotated = None;
        let current_size = tokio::fs::metadata(self.current_path(state.session_id))
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        let current_msgs_in_part = meta
            .archived_msg_count
            .saturating_sub(meta.current_part_start_msg);
        let rot = &self.cfg.rotation;
        let should_rotate =
            current_size >= rot.max_bytes || current_msgs_in_part >= rot.max_messages;
        // turn-aligned 简化:在任何 state 调用 apply 都视为 turn 边界
        // (Runner 每 step 结束后调 apply,ModelTurn 结束时必是 turn 边界)

        if should_rotate {
            rotated = Some(self.rotate(state.session_id, &mut meta).await?);
        }

        self.write_summary_snapshot(state, &mut meta).await?;
        self.write_meta(state.session_id, &meta).await?;

        Ok(ApplyOutcome {
            appended_bytes,
            rotated,
            archived_msg_count: meta.archived_msg_count,
        })
    }

    async fn load_meta(&self, sid: SessionId) -> Option<SessionMeta> {
        let data = tokio::fs::read(self.meta_path(sid)).await.ok()?;
        serde_json::from_slice(&data).ok()
    }

    async fn write_meta(&self, sid: SessionId, meta: &SessionMeta) -> Result<(), std::io::Error> {
        let s =
            serde_json::to_string_pretty(meta).map_err(|e| std::io::Error::other(e.to_string()))?;
        tokio::fs::write(self.meta_path(sid), s).await
    }

    async fn write_summary_snapshot(
        &self,
        state: &RunState,
        meta: &mut SessionMeta,
    ) -> Result<(), std::io::Error> {
        let summaries = state
            .history
            .iter()
            .enumerate()
            .filter_map(|(idx, msg)| match msg {
                Message::Observation {
                    kind: ObsKind::Summary,
                    text,
                } => {
                    let message_id = state.history_ids.get(idx).cloned();
                    let checkpoint = message_id.as_ref().and_then(|id| {
                        state
                            .compaction_checkpoints
                            .iter()
                            .find(|checkpoint| checkpoint.summary_message_id == *id)
                    });
                    Some(SummaryInfo {
                        message_index: idx as u32,
                        message_id,
                        checkpoint_id: checkpoint.map(|c| c.checkpoint_id.clone()),
                        first_kept_message_id: checkpoint
                            .and_then(|c| c.first_kept_message_id.clone()),
                        tokens_estimate: token_estimate::estimate_text_tokens(text),
                        updated_ms: state.updated_ms,
                        text: text.clone(),
                    })
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        let mut lines = String::new();
        for info in &summaries {
            let line =
                serde_json::to_string(info).map_err(|e| std::io::Error::other(e.to_string()))?;
            lines.push_str(&line);
            lines.push('\n');
        }

        tokio::fs::write(self.summaries_path(state.session_id), lines).await?;
        let latest = summaries
            .last()
            .map(|info| info.text.as_str())
            .unwrap_or_default();
        tokio::fs::write(self.summary_path(state.session_id), latest).await?;
        meta.summary_count = summaries.len() as u32;
        Ok(())
    }

    async fn rotate(
        &self,
        sid: SessionId,
        meta: &mut SessionMeta,
    ) -> Result<RotatedPart, std::io::Error> {
        let next_part = meta.part_count + 1;
        let seal_name = format!("transcript-{:04}.jsonl", next_part);
        let seal_path = self.session_dir(sid).join(&seal_name);
        let current = self.current_path(sid);

        let size = tokio::fs::metadata(&current)
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        // Atomic rename
        tokio::fs::rename(&current, &seal_path).await?;

        // Empty new current
        tokio::fs::write(&current, b"").await?;

        // Append parts.jsonl
        let info = PartInfo {
            part: next_part,
            file: seal_name.clone(),
            turn_start: meta.current_part_start_msg,
            turn_end: meta.archived_msg_count,
            bytes: size,
            brief: "(pending)".into(),
            sealed_ms: meta.updated_ms,
        };
        let line =
            serde_json::to_string(&info).map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.parts_path(sid))
            .await?;
        f.write_all(line.as_bytes()).await?;
        f.write_all(b"\n").await?;
        f.flush().await?;

        meta.part_count = next_part;
        meta.current_part_start_msg = meta.archived_msg_count;
        Ok(RotatedPart { info })
    }

    /// 生成 system prompt augmentation:引导 LLM 使用 archive 搜索跨 session 历史。
    pub fn prompt_augmentation(&self, session_id: Option<SessionId>) -> String {
        let root = self.cfg.root.display();
        let mut s = format!(
            "## Session archive\n\n\
             Your past sessions are archived at:\n  {root}\n\n\
             Top-level:\n  index.jsonl  one line per session\n\n\
             Per-session layout (long sessions are split into parts):\n  \
             <session_id>/\n    meta.json         basic session metadata\n    \
             parts.jsonl       index of sealed parts (append-only)\n    \
             summaries.jsonl   current summary observations with message indexes\n    \
             summary.txt       latest compacted summary snapshot\n    \
             transcript-NNNN.jsonl  sealed part\n    transcript-current.jsonl  active part\n\n\
             Use fs.list / fs.read / fs.stat to browse. Optionally use sh.exec with rg / grep / jq.\n"
        );
        if let Some(sid) = session_id {
            s.push_str(&format!("\nCurrent session: {}\n", sid));
        }
        s
    }
}

#[derive(Debug, Default)]
pub struct ApplyOutcome {
    pub appended_bytes: u64,
    pub rotated: Option<RotatedPart>,
    pub archived_msg_count: u32,
}

#[derive(Debug, Clone)]
pub struct RotatedPart {
    pub info: PartInfo,
}

fn status_label(state: &RunState) -> String {
    use crate::core::step::Step;
    match &state.step {
        Step::Done { .. } => "done".into(),
        Step::Failed { .. } => "failed".into(),
        Step::Paused { .. } => "paused".into(),
        _ => "active".into(),
    }
}

// Silence unused-warnings when FileSystem adapter is considered
#[allow(dead_code)]
pub(crate) fn _keep_fs_used(_: Arc<dyn FileSystem>, _: &Uri) {}
