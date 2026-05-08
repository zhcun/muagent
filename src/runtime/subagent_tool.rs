//! Subagent-as-tool runtime adapter.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

use crate::core::prelude::{
    parse_args, AgentDefinition, CancelToken, Concurrency, GuardOutcome, Idempotency, SideEffects,
    SubagentInvocation, SubagentResult, Tool, ToolContext, ToolDescriptor, ToolErr, ToolOk,
    SUBAGENT_TOOL_NAME,
};

/// Hard cap for concurrently running subagent invocations from one parent tool.
pub const MAX_PARALLEL_SUBAGENT_CALLS: usize = 8;

#[async_trait]
pub trait SubagentExecutor: Send + Sync {
    async fn invoke(
        &self,
        definition: AgentDefinition,
        invocation: SubagentInvocation,
        cancel: CancelToken,
    ) -> Result<SubagentResult, String>;
}

#[derive(Deserialize)]
struct Args {
    subagent: String,
    task: String,
}

pub struct SubagentTool {
    definitions: Arc<BTreeMap<String, AgentDefinition>>,
    executor: Arc<dyn SubagentExecutor>,
    permits: Arc<Semaphore>,
    desc: ToolDescriptor,
}

impl SubagentTool {
    pub fn new(definitions: Vec<AgentDefinition>, executor: Arc<dyn SubagentExecutor>) -> Self {
        let definitions = definitions
            .into_iter()
            .map(|def| (def.name.clone(), def))
            .collect::<BTreeMap<_, _>>();
        let desc = descriptor(&definitions);
        Self {
            definitions: Arc::new(definitions),
            executor,
            permits: Arc::new(Semaphore::new(MAX_PARALLEL_SUBAGENT_CALLS)),
            desc,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.definitions.is_empty()
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.desc
    }

    fn guard(&self, args: &Value) -> GuardOutcome {
        let parsed: Args = match parse_args(args) {
            Ok(args) => args,
            Err(e) => {
                return GuardOutcome::Deny {
                    reason: e.msg,
                    hint: e.hint,
                }
            }
        };
        if !self.definitions.contains_key(&parsed.subagent) {
            return GuardOutcome::Deny {
                reason: format!("unknown subagent `{}`", parsed.subagent),
                hint: Some(format!(
                    "Available subagents: {}",
                    self.definitions
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            };
        }
        if parsed.task.trim().is_empty() {
            return GuardOutcome::Deny {
                reason: "`task` cannot be empty".into(),
                hint: Some("Pass a concrete task for the selected subagent.".into()),
            };
        }
        GuardOutcome::Allow
    }

    async fn run(
        &self,
        args: Value,
        ctx: &ToolContext,
        cancel: CancelToken,
    ) -> Result<ToolOk, ToolErr> {
        if cancel.triggered() {
            return Err(ToolErr::retry("cancelled"));
        }
        let parsed: Args = parse_args(&args)?;
        let definition = self
            .definitions
            .get(&parsed.subagent)
            .cloned()
            .ok_or_else(|| ToolErr::deny(format!("unknown subagent `{}`", parsed.subagent)))?;
        let invocation = SubagentInvocation {
            agent_name: parsed.subagent.clone(),
            task: parsed.task,
            parent_run_id: ctx.run_id,
            parent_session_id: ctx.session_id,
            parent_call_id: None,
        };
        let _permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ToolErr::retry("subagent concurrency limiter closed"))?;
        let result = self
            .executor
            .invoke(definition, invocation, cancel)
            .await
            .map_err(|e| ToolErr::retry(format!("subagent `{}` failed: {e}", parsed.subagent)))?;
        let detail = serde_json::to_value(&result).unwrap_or(Value::Null);
        Ok(ToolOk::text(result.final_text).with_detail(detail))
    }
}

fn descriptor(definitions: &BTreeMap<String, AgentDefinition>) -> ToolDescriptor {
    let agents = definitions
        .values()
        .map(|def| {
            if def.description.trim().is_empty() {
                format!("- {}", def.name)
            } else {
                format!("- {}: {}", def.name, def.description)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let agent_names = definitions.keys().cloned().collect::<Vec<_>>();
    ToolDescriptor {
        name: SUBAGENT_TOOL_NAME.into(),
        description: format!(
            "Delegate a focused task to a configured subagent with its own \
             instructions, context, and tool policy. Use this when one of the \
             specialized agents is a better fit than the main agent.\n\n\
             Available subagents:\n{agents}"
        ),
        schema_json: json!({
            "type": "object",
            "properties": {
                "subagent": {
                    "type": "string",
                    "description": "Name of the subagent to invoke.",
                    "enum": agent_names,
                },
                "task": {
                    "type": "string",
                    "description": "Concrete task for the subagent. Include all context it needs.",
                },
            },
            "required": ["subagent", "task"],
            "additionalProperties": false,
        }),
        timeout: Duration::from_secs(300),
        max_out_tokens: 4096,
        concurrency: Concurrency::Parallel,
        side_effects: SideEffects::Mutating,
        idempotency: Idempotency::AtMostOnce,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::prelude::{SubagentContextMode, ToolContext};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn reviewer_definition() -> AgentDefinition {
        AgentDefinition {
            name: "reviewer".into(),
            description: "Review changes".into(),
            instructions: "Find bugs.".into(),
            tools: Some(vec!["fs_read".into()]),
            skills: None,
            model: None,
            max_steps: None,
            context_mode: SubagentContextMode::Fresh,
        }
    }

    struct FakeExecutor;

    #[async_trait]
    impl SubagentExecutor for FakeExecutor {
        async fn invoke(
            &self,
            definition: AgentDefinition,
            invocation: SubagentInvocation,
            _cancel: CancelToken,
        ) -> Result<SubagentResult, String> {
            Ok(SubagentResult {
                agent_name: definition.name,
                final_text: format!("handled: {}", invocation.task),
                run_id: uuid::Uuid::new_v4(),
                session_id: uuid::Uuid::new_v4(),
                usage: Default::default(),
            })
        }
    }

    #[tokio::test]
    async fn subagent_tool_invokes_named_agent() {
        let tool = SubagentTool::new(vec![reviewer_definition()], Arc::new(FakeExecutor));

        assert_eq!(tool.descriptor().name, "spawn_sub_agent");
        let out = tool
            .run(
                json!({"subagent":"reviewer","task":"check the patch"}),
                &ToolContext::ephemeral(),
                CancelToken::never(),
            )
            .await
            .unwrap();

        assert_eq!(
            out.content,
            crate::core::types::Content::Text("handled: check the patch".into())
        );
        assert!(out.detail.is_some());
    }

    #[derive(Default)]
    struct CountingExecutor {
        current: AtomicUsize,
        max_seen: AtomicUsize,
    }

    #[async_trait]
    impl SubagentExecutor for CountingExecutor {
        async fn invoke(
            &self,
            definition: AgentDefinition,
            invocation: SubagentInvocation,
            _cancel: CancelToken,
        ) -> Result<SubagentResult, String> {
            let current = self.current.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_seen.fetch_max(current, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(25)).await;
            self.current.fetch_sub(1, Ordering::SeqCst);
            Ok(SubagentResult {
                agent_name: definition.name,
                final_text: format!("handled: {}", invocation.task),
                run_id: uuid::Uuid::new_v4(),
                session_id: uuid::Uuid::new_v4(),
                usage: Default::default(),
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn subagent_tool_caps_parallel_invocations_at_eight() {
        let executor = Arc::new(CountingExecutor::default());
        let tool = Arc::new(SubagentTool::new(
            vec![reviewer_definition()],
            executor.clone(),
        ));

        let mut handles = Vec::new();
        for i in 0..(MAX_PARALLEL_SUBAGENT_CALLS * 2) {
            let tool = tool.clone();
            handles.push(tokio::spawn(async move {
                tool.run(
                    json!({"subagent":"reviewer","task":format!("task {i}")}),
                    &ToolContext::ephemeral(),
                    CancelToken::never(),
                )
                .await
                .unwrap();
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        let max_seen = executor.max_seen.load(Ordering::SeqCst);
        assert!(
            max_seen <= MAX_PARALLEL_SUBAGENT_CALLS,
            "expected at most {MAX_PARALLEL_SUBAGENT_CALLS} concurrent calls, saw {max_seen}"
        );
        assert!(max_seen > 1, "test did not exercise parallel execution");
    }
}
