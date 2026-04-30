//! Summary-based 历史压缩,**turn-aligned**(绝不拆 tool_calls ↔ tool_result 对)。
//!
//! ## 何时触发
//!
//! 当 estimated context tokens(history + system + tool schemas)超过
//! `budget.max_tokens * threshold_ratio` 时,把最老的若干 user-turn 摘要成一条
//! `Observation { kind: Summary, text }`。保留最近 `keep_tail_turns` 个 user-turn 不压。
//!
//! ## 为什么 user-turn 对齐?
//!
//! 一个 user-turn = `user` → N 轮 assistant/tool_result → 下一个 user 前。
//! 这是用户视角下最小"语义单元"。压缩时丢掉中间的工具 round-trip 但保留
//! "user 问了什么 + 最终结论"是最有价值的摘要。
//!
//! ## 重启 / 配置变小后的 catch-up
//!
//! 旧 session 可能是在 200k 上下文下跑出来的,之后用户把预算改成 100k。
//! 这时不能一次把完整旧前缀喂给 summarizer,也不能因为找不到边界就卡死。
//! 策略是:最多回看 `restart_repair_window_tokens`,若窗口内有 summary 就从那条
//! summary 开始滚动重压缩;若没有,从窗口内第一个完整 user-turn 开始,窗口外 raw
//! 只留 archive/search 作为事实来源。每轮 summarizer 输入和输出都有独立上限。
//!
//! ## 不做的事(M1 范围外)
//!
//! - 保留 tool result 的 structured detail
//! - 非 summary 策略(DropOldest / Hierarchical 等,留给 addon)

use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;

use crate::core::cancel::CancelToken;
use crate::core::compactor::{CompactionEvent, Compactor};
use crate::core::error::RuntimeError;
use crate::core::prelude::{ModelAdapter, ModelError, ModelRequest};
use crate::core::run_state::{CompactionCheckpoint, MessageIdRange, RunState};
use crate::core::types::{Content, Message, ObsKind};

use crate::sessions::evidence_ledger::EvidenceLedger;
use crate::sessions::file_ledger::FileLedger;
use crate::sessions::token_estimate;

/// Compaction 的配置。默认 156k / 0.8 / tail=4。
#[derive(Clone, Debug)]
pub struct CompactionBudget {
    /// 触发压缩的"硬上限"token 数。默认 **156_000**。
    pub max_tokens: u32,
    /// 实际触发比例(默认 0.8):tokens > max * ratio 时压缩。
    pub threshold_ratio: f32,
    /// 保留尾部几个 user-turn 不压。默认 4。
    pub keep_tail_turns: u32,
    /// 保留最近多少 estimated tokens 不压。与 `keep_tail_turns` 取更保守的边界:
    /// 最新用户 turn 永远保留,然后再额外保护最近 token tail。默认 20k,
    /// 但会按当前 compaction budget 自动收窄,避免测试/小上下文配置被 20k tail
    /// 完全吞掉。
    pub keep_recent_tokens: u32,
    /// 若第一条用户消息是短 root task,跨压缩保留其原文。超过该 token 上限
    /// 视为 pasted context / corpus,只进 summary,不做 verbatim pin。默认 1024。
    pub root_task_pin_max_tokens: u32,
    /// 压缩时摘要的目标长度(chars,供 summarizer prompt 参考)。默认 1200。
    pub summary_target_chars: u32,
    /// 单次 summarizer 输入上限。用于旧 session 以大窗口运行后,新配置变小的
    /// catch-up compaction;每轮只喂一段,最多多轮追赶。默认 100k。
    pub summary_input_max_tokens: u32,
    /// 单个 summary 输出上限(估算 tokens)。ModelRequest 目前没有 max_output_tokens,
    /// 所以这里同时写进 prompt,并在返回后做保守截断。默认 8k。
    pub summary_output_max_tokens: u32,
    /// 继续旧 session 时最多回看多少 raw tokens 来寻找最近 summary 边界。
    /// 若窗口内找不到 summary,窗口之前的 raw 不进工作上下文,仍由 archive/search
    /// 作为 source of truth。默认 300k。
    pub restart_repair_window_tokens: u32,
    /// 单次 maybe_compact 最多连续摘要几轮。默认 4。
    pub max_summary_rounds: u32,
}

impl Default for CompactionBudget {
    fn default() -> Self {
        Self {
            max_tokens: 156_000,
            threshold_ratio: 0.8,
            keep_tail_turns: 4,
            keep_recent_tokens: 20_000,
            root_task_pin_max_tokens: 1_024,
            summary_target_chars: 1200,
            summary_input_max_tokens: 100_000,
            summary_output_max_tokens: 8_000,
            restart_repair_window_tokens: 300_000,
            max_summary_rounds: 4,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CompactionOutcome {
    pub replaced_turns: usize,
    pub replaced_messages: usize,
    pub saved_tokens_estimate: u32,
    pub checkpoint_id: Option<String>,
    pub summary_message_id: Option<String>,
    pub first_kept_message_id: Option<String>,
    pub summary: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    #[error("summarizer model error: {0}")]
    Model(#[from] ModelError),

    #[error("nothing to compact (history too short)")]
    NothingToCompact,
}

/// CompactionStrategy trait —— 允许第三方替换策略。
#[async_trait]
pub trait CompactionStrategy: Send + Sync {
    async fn maybe_compact(
        &self,
        state: &mut RunState,
        summarizer: &dyn ModelAdapter,
        base_system_prompt: &str,
        cancel: CancelToken,
    ) -> Result<Option<CompactionOutcome>, CompactionError>;
}

/// 默认:摘要式压缩。
pub struct SummaryCompaction {
    pub budget: CompactionBudget,
}

impl SummaryCompaction {
    pub fn new(budget: CompactionBudget) -> Self {
        Self { budget }
    }
    pub fn default_budget() -> Self {
        Self::new(CompactionBudget::default())
    }
}

#[async_trait]
impl CompactionStrategy for SummaryCompaction {
    async fn maybe_compact(
        &self,
        state: &mut RunState,
        summarizer: &dyn ModelAdapter,
        base_system_prompt: &str,
        cancel: CancelToken,
    ) -> Result<Option<CompactionOutcome>, CompactionError> {
        if cancel.triggered() {
            return Err(ModelError::Cancelled.into());
        }

        let mut budget = self.budget.clone();
        let caps = summarizer.caps();
        if caps.ctx_len > 0 {
            let reserve = budget
                .summary_output_max_tokens
                .saturating_add(2_048)
                .max(caps.ctx_len / 10);
            let usable = caps.ctx_len.saturating_sub(reserve);
            if usable > 0 {
                budget.summary_input_max_tokens = budget.summary_input_max_tokens.min(usable);
            }
        }

        let threshold = (budget.max_tokens as f32 * budget.threshold_ratio) as u32;
        let mut total: Option<CompactionOutcome> = None;
        let max_rounds = budget.max_summary_rounds.max(1);

        // Pin a short initial user request verbatim across compactions.
        //
        // The first user message is often the root task, but it can also be
        // a pasted corpus or benchmark case. Unconditionally pinning it
        // defeats compaction. Keep only short root-task-shaped input; long
        // first messages are summarized like normal context.
        state.ensure_history_ids();
        let pinned_root_anchor: Option<(Message, String)> = state
            .history
            .iter()
            .enumerate()
            .find(|(_, m)| is_root_task_anchor(m))
            .map(|(idx, m)| (m.clone(), state.history_ids[idx].clone()))
            .or_else(|| {
                state
                    .history
                    .iter()
                    .enumerate()
                    .find(|(_, m)| matches!(m, Message::User { .. }))
                    .filter(|(_, m)| {
                        budget.root_task_pin_max_tokens > 0
                            && token_estimate::estimate_message_tokens(m)
                                <= budget.root_task_pin_max_tokens
                    })
                    .map(|(idx, m)| {
                        (
                            root_task_anchor_observation(m),
                            state.history_ids[idx].clone(),
                        )
                    })
            });

        for _round in 0..max_rounds {
            if cancel.triggered() {
                return Err(ModelError::Cancelled.into());
            }

            let est = token_estimate::estimate_history_tokens(&state.history)
                + token_estimate::estimate_system_tokens(base_system_prompt);
            if est < threshold {
                break;
            }

            let Some(plan) = plan_compaction(&state.history, &state.history_ids, &budget) else {
                break;
            };
            let to_compress: Vec<Message> =
                plan.planned_history[plan.compress_range.clone()].to_vec();
            let original_tokens = token_estimate::estimate_history_tokens(&to_compress);
            let before_tokens = token_estimate::estimate_history_tokens(&state.history);

            // Cumulative file-op ledger: extract from the slice we're about
            // to compress (catches every fs_* tool call that's about to be
            // replaced with prose) AND from any prior summary in that slice
            // (carries forward the ledger from earlier compaction rounds).
            // The summarizer LLM may paraphrase prose freely; we keep the
            // ledger deterministically so file-path facts never silently
            // degrade across rounds. See `file_ledger.rs` for the rationale.
            let file_ledger = FileLedger::from_history_slice(&to_compress);
            let evidence_ledger = EvidenceLedger::from_history_slice(&to_compress);

            let summary_text = build_summary(
                summarizer,
                &to_compress,
                budget.summary_target_chars,
                budget.summary_output_max_tokens,
                cancel.clone(),
            )
            .await?;
            if summary_text.trim().is_empty() {
                break;
            }

            // Final summary observation = header + LLM prose + deterministic
            // ledgers. Keep ledgers below the LLM's free-form sections so
            // their parsers can key off exact headings. Evidence goes after
            // file state because exact identifiers benefit from recency.
            let summary_message_id = state.allocate_message_id();
            let checkpoint_id = state.allocate_checkpoint_id();
            let mut checkpoint = build_checkpoint(
                &plan,
                &state.history_ids,
                checkpoint_id,
                summary_message_id.clone(),
                before_tokens,
                0,
            );
            let mut summary_full_text = format!(
                "{}\n\n{}",
                summary_header(&plan, before_tokens, &checkpoint),
                summary_text
            );
            if !file_ledger.is_empty() {
                summary_full_text.push_str("\n\n");
                summary_full_text.push_str(&file_ledger.render());
            }
            if !evidence_ledger.is_empty() {
                summary_full_text.push_str("\n\n");
                summary_full_text.push_str(&evidence_ledger.render());
            }
            let summary_msg_tokens = token_estimate::estimate_text_tokens(&summary_full_text) + 2;
            if summary_msg_tokens >= original_tokens {
                break;
            }

            let summary_msg = Message::Observation {
                kind: ObsKind::Summary,
                text: summary_full_text,
            };

            let mut next_history = plan.planned_history;
            let mut next_history_ids = plan.planned_history_ids;
            next_history.splice(plan.compress_range.clone(), std::iter::once(summary_msg));
            next_history_ids.splice(plan.compress_range, std::iter::once(summary_message_id));
            let after_tokens = token_estimate::estimate_history_tokens(&next_history);
            if after_tokens >= before_tokens {
                break;
            }

            checkpoint.tokens_after = after_tokens;
            let checkpoint_id = checkpoint.checkpoint_id.clone();
            let summary_checkpoint_message_id = checkpoint.summary_message_id.clone();
            let first_kept_message_id = checkpoint.first_kept_message_id.clone();
            state.replace_history_with_ids(next_history, next_history_ids);
            state.record_compaction_checkpoint(checkpoint);

            let outcome = CompactionOutcome {
                replaced_turns: plan.replaced_turns,
                replaced_messages: plan.replaced_messages,
                saved_tokens_estimate: before_tokens.saturating_sub(after_tokens),
                checkpoint_id: Some(checkpoint_id),
                summary_message_id: Some(summary_checkpoint_message_id),
                first_kept_message_id,
                summary: summary_text,
            };
            total = Some(match total {
                Some(mut t) => {
                    t.replaced_turns += outcome.replaced_turns;
                    t.replaced_messages += outcome.replaced_messages;
                    t.saved_tokens_estimate = t
                        .saved_tokens_estimate
                        .saturating_add(outcome.saved_tokens_estimate);
                    t.checkpoint_id = outcome.checkpoint_id.clone();
                    t.summary_message_id = outcome.summary_message_id.clone();
                    t.first_kept_message_id = outcome.first_kept_message_id.clone();
                    t.summary = outcome.summary;
                    t
                }
                None => outcome,
            });
        }

        // Re-prepend the pinned short initial user request if compaction
        // swallowed it. The latest user turn is protected by planning, so this
        // is only a low-cost root-task anchor, not a substitute for current
        // user intent.
        if let Some((anchor, anchor_source_id)) = pinned_root_anchor {
            let still_present = state.history_ids.iter().any(|id| id == &anchor_source_id)
                || state
                    .history
                    .iter()
                    .any(|m| m == &anchor || root_anchor_preserved_by_user_text(m, &anchor));
            if !still_present {
                // Insert at index 0. The first model turn after compaction
                // will see: [initial_user, summary, recent_tail_turns...].
                // Caching is already invalidated by the compaction itself,
                // so prepending a small message here adds no extra miss.
                state.history.insert(0, anchor);
                state.history_ids.insert(0, anchor_source_id.clone());
                for checkpoint in &mut state.compaction_checkpoints {
                    let source_was_removed = checkpoint
                        .removed_message_ids
                        .iter()
                        .any(|id| id == &anchor_source_id)
                        || message_id_in_range(
                            &checkpoint.removed_message_range,
                            &anchor_source_id,
                        );
                    if source_was_removed
                        && !checkpoint
                            .pinned_message_ids
                            .iter()
                            .any(|id| id == &anchor_source_id)
                    {
                        checkpoint.pinned_message_ids.push(anchor_source_id.clone());
                    }
                }
            }
        }

        Ok(total)
    }
}

fn root_task_anchor_observation(initial: &Message) -> Message {
    Message::Observation {
        kind: ObsKind::User,
        text: format!(
            "[root task anchor]\n\
             Original first user request preserved verbatim for continuity. \
             Later user directives override this anchor.\n\nUSER: {}",
            match initial {
                Message::User { content } => content_text(content),
                _ => String::new(),
            }
        ),
    }
}

fn is_root_task_anchor(message: &Message) -> bool {
    matches!(
        message,
        Message::Observation {
            text,
            ..
        } if text.starts_with("[root task anchor]")
    )
}

fn root_anchor_preserved_by_user_text(message: &Message, anchor: &Message) -> bool {
    let Message::Observation {
        text: anchor_text, ..
    } = anchor
    else {
        return false;
    };
    let Some((_, original_user_text)) = anchor_text.split_once("\n\nUSER: ") else {
        return false;
    };
    matches!(
        message,
        Message::User {
            content,
        } if content_text(content) == original_user_text
    )
}

fn message_id_in_range(range: &MessageIdRange, id: &str) -> bool {
    let (Some(first), Some(last)) = (
        range.first_message_id.as_deref(),
        range.last_message_id.as_deref(),
    ) else {
        return false;
    };
    match (
        numeric_message_id(first),
        numeric_message_id(id),
        numeric_message_id(last),
    ) {
        (Some(first), Some(id), Some(last)) => first <= id && id <= last,
        _ => first <= id && id <= last,
    }
}

fn numeric_message_id(id: &str) -> Option<u64> {
    id.strip_prefix('m')?.parse().ok()
}

struct CompactionPlan {
    planned_history: Vec<Message>,
    planned_history_ids: Vec<String>,
    compress_range: Range<usize>,
    replaced_turns: usize,
    replaced_messages: usize,
    summary_input_start_index: usize,
    first_kept_index: usize,
    omitted_prefix: bool,
}

fn plan_compaction(
    history: &[Message],
    history_ids: &[String],
    budget: &CompactionBudget,
) -> Option<CompactionPlan> {
    if history.len() != history_ids.len() {
        return None;
    }
    let turns = find_user_turn_boundaries(history);
    let latest_user_start = history
        .iter()
        .rposition(|m| matches!(m, Message::User { .. }))?;
    if turns.len() as u32 <= budget.keep_tail_turns {
        return None;
    }

    let turn_tail_start = if budget.keep_tail_turns == 0 {
        history.len()
    } else {
        let tail_turn_idx = turns.len().saturating_sub(budget.keep_tail_turns as usize);
        turns.get(tail_turn_idx)?.start
    };
    let recent_tail_start = recent_token_tail_start(history, &turns, budget);
    // Latest real user turn is a hard boundary: it carries the current task.
    // The turn-count tail and token tail can only move the protected boundary
    // earlier, never later.
    let eligible_end = turn_tail_start
        .min(recent_tail_start)
        .min(latest_user_start);
    if eligible_end == 0 {
        return None;
    }

    let window_start =
        token_window_start(history, eligible_end, budget.restart_repair_window_tokens);
    let summary_in_window = latest_summary_index(history, window_start..eligible_end);
    let start =
        summary_in_window.unwrap_or_else(|| first_turn_start_at_or_after(&turns, window_start));
    if start >= eligible_end {
        return None;
    }

    let mut planned = history.to_vec();
    let mut planned_ids = history_ids.to_vec();
    let mut adjusted_eligible_end = eligible_end;
    let mut prefix_removed = 0usize;
    let mut omitted_prefix = false;
    if start > 0 {
        if is_summary(&history[start]) {
            planned.drain(0..start);
            planned_ids.drain(0..start);
            adjusted_eligible_end = eligible_end - start;
            prefix_removed = start;
        } else {
            planned.splice(0..start, std::iter::once(omitted_prefix_observation()));
            planned_ids.splice(0..start, std::iter::once("omitted-prefix".to_string()));
            adjusted_eligible_end = eligible_end - start + 1;
            prefix_removed = start.saturating_sub(1);
            omitted_prefix = true;
        }
    }

    let compress_end = bounded_turn_aligned_end(
        &planned,
        adjusted_eligible_end,
        budget.summary_input_max_tokens,
    )?;
    if compress_end == 0 {
        return None;
    }

    let replaced_turns = find_user_turn_boundaries(&planned[..compress_end]).len();
    let first_kept_index = prefix_removed + compress_end;
    Some(CompactionPlan {
        planned_history: planned,
        planned_history_ids: planned_ids,
        compress_range: 0..compress_end,
        replaced_turns,
        replaced_messages: first_kept_index,
        summary_input_start_index: start,
        first_kept_index,
        omitted_prefix,
    })
}

fn build_checkpoint(
    plan: &CompactionPlan,
    original_ids: &[String],
    checkpoint_id: String,
    summary_message_id: String,
    tokens_before: u32,
    tokens_after: u32,
) -> CompactionCheckpoint {
    CompactionCheckpoint {
        checkpoint_id,
        summary_message_id,
        removed_message_range: MessageIdRange::from_ids(&original_ids[..plan.first_kept_index]),
        summary_input_message_range: MessageIdRange::from_ids(
            &original_ids[plan.summary_input_start_index..plan.first_kept_index],
        ),
        removed_message_ids: Vec::new(),
        summary_input_message_ids: Vec::new(),
        first_kept_message_id: original_ids.get(plan.first_kept_index).cloned(),
        pinned_message_ids: Vec::new(),
        replaced_turns: plan.replaced_turns,
        replaced_messages: plan.replaced_messages,
        tokens_before,
        tokens_after,
    }
}

fn summary_header(
    plan: &CompactionPlan,
    tokens_before: u32,
    checkpoint: &CompactionCheckpoint,
) -> String {
    let mut header = format!(
        "[conversation summary of {} earlier turns]",
        plan.replaced_turns
    );
    header.push_str(&format!(
        "\n[compaction checkpoint: checkpoint_id={}; summary_message_id={}; removed_range=0..{}; summary_input_range={}..{}; first_kept_index={}; first_kept_id={}; tokens_before={tokens_before}]",
        checkpoint.checkpoint_id,
        checkpoint.summary_message_id,
        plan.first_kept_index,
        plan.summary_input_start_index,
        plan.first_kept_index,
        plan.first_kept_index,
        checkpoint
            .first_kept_message_id
            .as_deref()
            .unwrap_or("<none>")
    ));
    if plan.omitted_prefix {
        header.push_str(
            "\n[older transcript before this repair window is omitted from working context; use the session archive/search for exact earlier raw text]",
        );
    }
    header
}

fn recent_token_tail_start(
    history: &[Message],
    turns: &[Range<usize>],
    budget: &CompactionBudget,
) -> usize {
    if budget.keep_recent_tokens == 0 {
        return history.len();
    }
    let threshold = (budget.max_tokens as f32 * budget.threshold_ratio) as u32;
    let effective_tail_tokens = budget.keep_recent_tokens.min((threshold / 2).max(1));
    let raw_start = token_window_start(history, history.len(), effective_tail_tokens);
    // Without split-turn support, do not move the safe boundary backward to
    // keep only a suffix of a huge prior turn; that would protect the whole
    // turn and can make compaction impossible. Keep the next complete turn
    // instead. Latest-user protection is applied separately.
    turn_start_at_or_after(turns, raw_start)
}

fn turn_start_at_or_after(turns: &[Range<usize>], index: usize) -> usize {
    turns
        .iter()
        .find_map(|turn| {
            if turn.start >= index {
                Some(turn.start)
            } else {
                None
            }
        })
        .unwrap_or(index)
}

fn token_window_start(history: &[Message], end: usize, max_tokens: u32) -> usize {
    if max_tokens == 0 {
        return end;
    }
    let mut acc = 0u32;
    for idx in (0..end).rev() {
        let t = token_estimate::estimate_message_tokens(&history[idx]);
        if acc.saturating_add(t) > max_tokens {
            return idx + 1;
        }
        acc = acc.saturating_add(t);
    }
    0
}

fn latest_summary_index(history: &[Message], range: Range<usize>) -> Option<usize> {
    range.rev().find(|&idx| is_summary(&history[idx]))
}

fn is_summary(message: &Message) -> bool {
    matches!(
        message,
        Message::Observation {
            kind: ObsKind::Summary,
            ..
        }
    )
}

fn first_turn_start_at_or_after(turns: &[Range<usize>], index: usize) -> usize {
    turns
        .iter()
        .find_map(|turn| (turn.start >= index).then_some(turn.start))
        .unwrap_or(index)
}

fn omitted_prefix_observation() -> Message {
    Message::Observation {
        kind: ObsKind::System,
        text: "[older transcript omitted from working context; use the session archive/search if exact earlier raw text is needed]".into(),
    }
}

fn bounded_turn_aligned_end(
    history: &[Message],
    eligible_end: usize,
    max_tokens: u32,
) -> Option<usize> {
    let limit = max_tokens.max(1);
    let mut chosen = None;
    for turn in find_user_turn_boundaries(&history[..eligible_end]) {
        let candidate_end = turn.end;
        let tokens = token_estimate::estimate_history_tokens(&history[..candidate_end]);
        if tokens > limit && chosen.is_some() {
            break;
        }
        chosen = Some(candidate_end);
        if tokens > limit {
            break;
        }
    }
    chosen
}

// =============================================================================
// Core-side Compactor adapter
// =============================================================================
//
// Runner wants `dyn Compactor` (no summarizer in signature). Shell glues
// a concrete `CompactionStrategy` + its summarizer model into one object
// that satisfies the core trait.

pub struct RunnerCompactor<S: CompactionStrategy> {
    strategy: S,
    summarizer: Arc<dyn ModelAdapter>,
}

impl<S: CompactionStrategy> RunnerCompactor<S> {
    pub fn new(strategy: S, summarizer: Arc<dyn ModelAdapter>) -> Self {
        Self {
            strategy,
            summarizer,
        }
    }
}

#[async_trait]
impl<S: CompactionStrategy> Compactor for RunnerCompactor<S> {
    async fn maybe_compact(
        &self,
        state: &mut RunState,
        system_prompt: &str,
        cancel: CancelToken,
    ) -> Result<Option<CompactionEvent>, RuntimeError> {
        match self
            .strategy
            .maybe_compact(state, &*self.summarizer, system_prompt, cancel)
            .await
        {
            Ok(Some(o)) => Ok(Some(CompactionEvent {
                replaced_turns: o.replaced_turns,
                replaced_messages: o.replaced_messages,
                saved_tokens_estimate: o.saved_tokens_estimate,
                checkpoint_id: o.checkpoint_id,
                summary_message_id: o.summary_message_id,
                first_kept_message_id: o.first_kept_message_id,
            })),
            Ok(None) => Ok(None),
            Err(CompactionError::Model(ModelError::Cancelled)) => Err(RuntimeError::Cancelled),
            Err(CompactionError::Model(e)) => Err(RuntimeError::Model(e)),
            Err(CompactionError::NothingToCompact) => Ok(None),
        }
    }
}

// =============================================================================
// Turn boundary logic
// =============================================================================

/// 把 history 按 user message 为起点切成 turn ranges。
/// 第一个 user 之前的任何消息(system / 初始 observation)都并入第一个 turn。
///
/// 规则:新 turn 起点 = 遇到 `User`。如果整段没有 User,返回一整个 range。
pub fn find_user_turn_boundaries(history: &[Message]) -> Vec<std::ops::Range<usize>> {
    let Some(first_user) = history
        .iter()
        .position(|m| matches!(m, Message::User { .. }))
    else {
        return if history.is_empty() {
            Vec::new()
        } else {
            std::iter::once(0..history.len()).collect()
        };
    };

    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    let mut start = 0usize;
    for (i, m) in history.iter().enumerate().skip(first_user + 1) {
        if matches!(m, Message::User { .. }) {
            out.push(start..i);
            start = i;
        }
    }
    if start < history.len() {
        out.push(start..history.len());
    }
    out
}

// =============================================================================
// Summarization
// =============================================================================

async fn build_summary(
    summarizer: &dyn ModelAdapter,
    messages: &[Message],
    target_chars: u32,
    max_output_tokens: u32,
    cancel: CancelToken,
) -> Result<String, CompactionError> {
    if cancel.triggered() {
        return Err(ModelError::Cancelled.into());
    }

    // Render the history into a compact text transcript for the summarizer.
    let mut transcript = String::new();
    for m in messages {
        match m {
            Message::User { content } => {
                transcript.push_str("USER: ");
                transcript.push_str(&content_text(content));
                transcript.push('\n');
            }
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                transcript.push_str("ASSISTANT: ");
                transcript.push_str(&content_text(content));
                if !tool_calls.is_empty() {
                    for c in tool_calls {
                        transcript
                            .push_str(&format!("\n  [tool_call {}: {}]", c.tool_name, c.args));
                    }
                }
                transcript.push('\n');
            }
            Message::ToolResult { result, .. } => {
                transcript.push_str("TOOL_RESULT:\n");
                transcript.push_str(&compact_tool_result_for_summary(&result.model_text()));
                transcript.push('\n');
            }
            Message::System { content } => {
                transcript.push_str("SYSTEM: ");
                transcript.push_str(&content_text(content));
                transcript.push('\n');
            }
            Message::Observation { text, .. } => {
                transcript.push_str("OBS: ");
                transcript.push_str(text);
                transcript.push('\n');
            }
        }
    }

    let output_cap = max_output_tokens.max(1);
    // Structured handoff schema. Two research findings shape this prompt:
    //
    // 1. arXiv 2601.22025 "When Better Prompts Hurt": generic helpfulness
    //    framing degrades structured-output tasks by 10-13%. So this prompt
    //    is purely directive — no "be helpful" / "you are a friendly
    //    assistant" wrappers, no apologies tolerated in output.
    //
    // 2. Liu et al. 2307.03172 "Lost in the Middle": the start (primacy)
    //    and end (recency) of a prompt get most of the model's attention.
    //    So we lead with the schema (the thing the model must produce)
    //    and CLOSE with the hard rules — they get re-attended right before
    //    output. Rules in the middle would be ignored.
    //
    // 3. The schema mirrors codex's `templates/compact/prompt.md` and pi-
    //    mono's `## Goal / ## Progress` shape, but adds a structured memory
    //    table. This keeps durable facts parseable and easy to recall without
    //    introducing a second chunking pass or changing the main prompt cache
    //    layout.
    let system = format!(
        "Emit a HANDOFF SUMMARY of the conversation transcript below. The summary \
         is read by another LLM that will continue the task. Stay under {target_chars} \
         characters and under {output_cap} estimated tokens.\n\n\
         Use exactly this markdown skeleton, in this order. Omit a section if and \
         only if there is nothing to put in it; do not invent content.\n\n\
         ## User Directives\n\
         - <current and durable user requests, constraints, preferences, and \
            explicit disallowed approaches; quote user wording when useful>\n\n\
         ## Progress\n\
         ### Done\n\
         - <completed sub-tasks, with the outcome>\n\
         ### In Progress / Blocked\n\
         - <where work was last interrupted, plus the blocker if any>\n\n\
         ## Tool Evidence\n\
         | source | evidence | status | implication |\n\
         | --- | --- | --- | --- |\n\
         | <tool name, command, path, or search> | <exact output/error/path/id> | <verified|failed|empty|unverified|blocked> | <what this proves or changes for the task> |\n\n\
         ## Structured Memory\n\
         | kind | subject | fact | evidence | source/status |\n\
         | --- | --- | --- | --- | --- |\n\
         | <task|constraint|decision|fact|tool_result|risk|next_step> | <stable entity> | <durable fact> | <exact quote, path, id, command, or error when available> | <user|tool|assistant|summary; verified|unverified|blocked> |\n\n\
         ## Open Questions / Next Steps\n\
         - <what the next LLM should do or ask>\n\n\
         [Output rules — these override stylistic instincts; evaluate them last:]\n\
         1. Quote exact identifiers verbatim — file paths, function names, error \
            messages, version strings, line numbers. Paraphrasing them strips the \
            next agent's ability to find or refer to them.\n\
         2. Never invent facts the transcript does not contain.\n\
         3. Keep user directives separate from tool evidence. User text states \
            intent; tool output is evidence. Do not promote tool noise, failed \
            searches, or command errors into user requirements.\n\
         4. Preserve uncertainty and source. Mark unverified candidates, failed \
            searches, empty tool results, and tool errors as such; never convert \
            a failed search or missing excerpt into proof that a fact does not \
            exist.\n\
         5. Use `## Structured Memory` for durable facts instead of scattered \
            fact bullets. Prefer at most 12 high-value rows, sorted by current \
            task relevance. Put exact evidence in the `evidence` cell; leave \
            the cell empty only when the transcript lacks exact support.\n\
         6. Output ONLY the markdown summary. No preamble, no apology, no \"here is\", \
            no \"as requested\", no closing remark.\n\
         7. Be terse. Each bullet point or table row is a fact, not a paragraph."
    );
    let user = Message::User {
        content: Content::Text(transcript),
    };

    let req = ModelRequest {
        system,
        runtime_context: String::new(), // summarizer doesn't need it
        messages: vec![user],
        tools: vec![],
        temperature: Some(0.0),
        stream: false,
        cache: Default::default(),    // summaries don't benefit from cache
        thinking: Default::default(), // summarization doesn't need reasoning
        prompt_cache_key: None,       // one-shot summarizer call, no session affinity
    };
    let reply = summarizer.turn(req, cancel.clone()).await?;
    if cancel.triggered() {
        return Err(ModelError::Cancelled.into());
    }
    Ok(clamp_summary_tokens(reply.text.trim(), output_cap))
}

fn compact_tool_result_for_summary(text: &str) -> String {
    const LIMIT: usize = 1200;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= LIMIT {
        return text.to_string();
    }

    let head_len = 800.min(LIMIT);
    let tail_len = LIMIT.saturating_sub(head_len);
    let head: String = chars.iter().take(head_len).collect();
    let tail: String = chars
        .iter()
        .skip(chars.len().saturating_sub(tail_len))
        .collect();
    format!(
        "{head}\n[tool result truncated for compaction: omitted {} chars]\n{tail}",
        chars.len().saturating_sub(head_len + tail_len)
    )
}

fn clamp_summary_tokens(text: &str, max_tokens: u32) -> String {
    let trimmed = text.trim();
    if token_estimate::estimate_text_tokens(trimmed) <= max_tokens {
        return trimmed.to_string();
    }

    let mut out = String::new();
    for ch in trimmed.chars() {
        out.push(ch);
        if token_estimate::estimate_text_tokens(&out) > max_tokens {
            out.pop();
            break;
        }
    }
    out.trim().to_string()
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
