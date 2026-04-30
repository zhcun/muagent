//! Live E2E: registered skill's tools are immediately callable.
//!
//! Skills no longer have activation states. Register a skill with a
//! CalculatorTool, and a real LLM should see it in the tool list and
//! use it to answer "12345 + 67890".

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use muagent::adapters::ReqwestEgress;
use muagent::core::prelude::*;
use muagent::core::step::Step;
use muagent::core::tool::{Tool, ToolErr, ToolOk};
use muagent::core::types::{Content, Message};
use muagent::prelude::*;
use muagent::providers::OpenAiAdapter;
use muagent::storage::MemorySessionStore;
use serde_json::{json, Value};
use uuid::Uuid;

fn load_env() -> (String, String, String) {
    for p in &[".env", "../.env", "../../.env", "../../../.env"] {
        if dotenvy::from_path(p).is_ok() {
            break;
        }
    }
    (
        std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY"),
        std::env::var("OPENROUTER_BASE_URL")
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1".into()),
        std::env::var("OPENROUTER_MODEL").unwrap_or_else(|_| "openai/gpt-5.4-nano".into()),
    )
}

fn build_real_model() -> Arc<dyn ModelAdapter> {
    let (key, base, model) = load_env();
    let net = Arc::new(ReqwestEgress::new().unwrap());
    Arc::new(OpenAiAdapter::new(net, &base, &model, Some(key)))
}

async fn drive_until_terminal(runner: &Runner, state: &mut RunState, max: usize) {
    for _ in 0..max {
        if matches!(
            state.step,
            Step::Done { .. } | Step::Failed { .. } | Step::Paused { .. }
        ) {
            return;
        }
        runner.step(state).await.expect("step");
    }
}

// ============ skill:calculator + calc_add tool ============

struct CalculatorTool {
    desc: ToolDescriptor,
}

impl CalculatorTool {
    fn new() -> Self {
        Self {
            desc: ToolDescriptor {
                name: "calc_add".into(),
                description: "Add two integers precisely.".into(),
                schema_json: json!({
                    "type":"object",
                    "properties":{ "a":{"type":"integer"}, "b":{"type":"integer"} },
                    "required":["a","b"],
                }),
                timeout: Duration::from_secs(1),
                max_out_tokens: 50,
                concurrency: Concurrency::Parallel,
                side_effects: SideEffects::ReadOnly,
                idempotency: Idempotency::Idempotent,
            },
        }
    }
}

#[async_trait]
impl Tool for CalculatorTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.desc
    }
    async fn run_ctxless(
        &self,
        args: Value,
        _cancel: muagent::core::cancel::CancelToken,
    ) -> Result<ToolOk, ToolErr> {
        let a = args["a"]
            .as_i64()
            .ok_or_else(|| ToolErr::deny("a not int"))?;
        let b = args["b"]
            .as_i64()
            .ok_or_else(|| ToolErr::deny("b not int"))?;
        Ok(ToolOk::text(format!("{}", a + b)))
    }
}

/// A description-only skill that tells the LLM when to use calc_add.
/// Tool and skill are decoupled per the Anthropic Skills protocol.
struct CalculatorSkill;

impl Skill for CalculatorSkill {
    fn id(&self) -> &str {
        "calculator"
    }
    fn description(&self) -> &str {
        "Precise math. For any arithmetic, call calc_add. Don't do math in your head."
    }
}

#[tokio::test]
async fn registered_tool_appears_in_active_tool_set() {
    // Tools are registered directly; skills only contribute prompt text.
    let registry = Arc::new(CapabilityRegistry::new());
    registry.register(Arc::new(CalculatorTool::new()));

    let skills = Arc::new(SkillManager::new());
    skills.register(Arc::new(CalculatorSkill));

    let provider = DefaultToolSetProvider::new(registry).with_skills(skills);
    let ats = provider
        .provide(&RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0))
        .await;
    let names: Vec<&str> = ats.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"calc_add"),
        "calc_add missing from active tool set: {names:?}"
    );
    assert!(ats.prompt_augmentation.contains("calculator"));
}

#[ignore = "hits real OpenRouter API"]
#[tokio::test]
async fn live_skill_tools_are_immediately_callable() {
    let model = build_real_model();
    let registry = Arc::new(CapabilityRegistry::new());
    registry.register(Arc::new(CalculatorTool::new()));

    let skills = Arc::new(SkillManager::new());
    skills.register(Arc::new(CalculatorSkill));

    let executor = Arc::new(DefaultToolExecutor::new(registry.clone()));
    let provider = DefaultToolSetProvider::new(registry).with_skills(skills);
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    let runner = Runner::builder()
        .model(model)
        .tools(executor)
        .store(store)
        .tools_provider(provider)
        .base_system_prompt("You are precise. Use the calc_add tool for arithmetic.")
        .build()
        .unwrap();

    let mut state = RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0);
    runner
        .submit_user_message(
            &mut state,
            Message::User {
                content: Content::text("What is 12345 plus 67890? Use calc_add."),
            },
        )
        .await
        .unwrap();
    drive_until_terminal(&runner, &mut state, 10).await;

    let final_text = match &state.step {
        Step::Done { final_text } => final_text.clone(),
        other => panic!("expected Done, got {other:?}"),
    };
    assert!(
        final_text.contains("80235"),
        "agent should compute 12345+67890=80235 via calc_add; got: {final_text}"
    );

    let used = state.history.iter().any(|m| match m {
        Message::Assistant { tool_calls, .. } => {
            tool_calls.iter().any(|c| c.tool_name == "calc_add")
        }
        _ => false,
    });
    assert!(used, "calc_add should have been called");
}
