//! M0 Shell 必过的 3 条验收测试:panic / guard deny / timeout。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use muagent::core::cancel::CancelToken;
use muagent::core::prelude::*;
use muagent::core::tool::Tool;
use muagent::prelude::*;
use serde_json::{json, Value};

fn descriptor(name: &str, timeout_ms: u64) -> ToolDescriptor {
    ToolDescriptor {
        name: name.into(),
        description: format!("test tool {name}"),
        schema_json: json!({}),
        timeout: Duration::from_millis(timeout_ms),
        max_out_tokens: 4096,
        concurrency: Default::default(),
        side_effects: Default::default(),
        idempotency: Default::default(),
    }
}

// ================ Test 6:panic → ToolResult ================

struct PanicTool {
    d: ToolDescriptor,
}

#[async_trait]
impl Tool for PanicTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.d
    }
    async fn run_ctxless(
        &self,
        _args: Value,
        _cancel: muagent::core::cancel::CancelToken,
    ) -> Result<ToolOk, ToolErr> {
        panic!("boom")
    }
}

#[tokio::test]
async fn t6_panic_becomes_tool_result() {
    let reg = Arc::new(CapabilityRegistry::new());
    reg.register(Arc::new(PanicTool {
        d: descriptor("panic_tool", 5000),
    }));

    let exec = DefaultToolExecutor::new(reg);
    let call = PendingCall::new("c1", "panic_tool", json!({}));
    let r = exec
        .execute(&call, &ToolContext::ephemeral(), CancelToken::never())
        .await
        .unwrap();

    assert!(!r.ok);
    assert!(r.retryable);
    assert!(r.text().contains("internal"));
    assert!(r.text().contains("boom"));
}

// ================ Test 7:guard deny → ToolResult ================

struct GuardDenyTool {
    d: ToolDescriptor,
}

#[async_trait]
impl Tool for GuardDenyTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.d
    }
    fn guard(&self, _args: &Value) -> GuardOutcome {
        GuardOutcome::Deny {
            reason: "path outside sandbox".into(),
            hint: Some("use sandbox:// root".into()),
        }
    }
    async fn run_ctxless(
        &self,
        _args: Value,
        _cancel: muagent::core::cancel::CancelToken,
    ) -> Result<ToolOk, ToolErr> {
        // Should never reach here
        Ok(ToolOk::text("unreachable"))
    }
}

struct LongGuardDenyTool {
    d: ToolDescriptor,
}

#[async_trait]
impl Tool for LongGuardDenyTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.d
    }
    fn guard(&self, _args: &Value) -> GuardOutcome {
        GuardOutcome::Deny {
            reason: "x".repeat(200),
            hint: Some("h".repeat(2000)),
        }
    }
}

#[tokio::test]
async fn t7_guard_deny_becomes_tool_result() {
    let reg = Arc::new(CapabilityRegistry::new());
    reg.register(Arc::new(GuardDenyTool {
        d: descriptor("deny_tool", 5000),
    }));

    let exec = DefaultToolExecutor::new(reg);
    let call = PendingCall::new("c1", "deny_tool", json!({"path":"/etc/passwd"}));
    let r = exec
        .execute(&call, &ToolContext::ephemeral(), CancelToken::never())
        .await
        .unwrap();

    assert!(!r.ok);
    assert!(!r.retryable); // guard deny 是硬错,不可重试
    assert!(r.text().contains("outside sandbox"));
    assert_eq!(r.hint.as_deref(), Some("use sandbox:// root"));
}

#[tokio::test]
async fn guard_deny_output_and_hint_are_capped() {
    let reg = Arc::new(CapabilityRegistry::new());
    let mut d = descriptor("long_deny_tool", 5000);
    d.max_out_tokens = 2;
    reg.register(Arc::new(LongGuardDenyTool { d }));

    let exec = DefaultToolExecutor::new(reg);
    let call = PendingCall::new("c1", "long_deny_tool", json!({}));
    let r = exec
        .execute(&call, &ToolContext::ephemeral(), CancelToken::never())
        .await
        .unwrap();

    assert!(!r.ok);
    assert!(r.text().contains("truncated to ~2 tokens"));
    assert!(
        r.text().len() < 180,
        "text should be capped: {}",
        r.text().len()
    );
    let hint = r.hint.as_deref().unwrap_or("");
    assert!(hint.contains("hint truncated"));
    assert!(hint.len() < 1100, "hint should be capped: {}", hint.len());
}

// ================ Test 8:timeout → ToolResult ================

struct SlowTool {
    d: ToolDescriptor,
}

#[async_trait]
impl Tool for SlowTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.d
    }
    async fn run_ctxless(
        &self,
        _args: Value,
        _cancel: muagent::core::cancel::CancelToken,
    ) -> Result<ToolOk, ToolErr> {
        tokio::time::sleep(Duration::from_millis(5000)).await;
        Ok(ToolOk::text("too slow"))
    }
}

#[tokio::test]
async fn t8_timeout_becomes_tool_result() {
    let reg = Arc::new(CapabilityRegistry::new());
    reg.register(Arc::new(SlowTool {
        d: descriptor("slow_tool", 50),
    })); // 50ms timeout

    let exec = DefaultToolExecutor::new(reg);
    let call = PendingCall::new("c1", "slow_tool", json!({}));
    let r = exec
        .execute(&call, &ToolContext::ephemeral(), CancelToken::never())
        .await
        .unwrap();

    assert!(!r.ok);
    assert!(r.retryable);
    assert_eq!(r.text(), "timeout");
    assert!(r.hint.is_some());
}

struct LongOutputTool {
    d: ToolDescriptor,
}

#[async_trait]
impl Tool for LongOutputTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.d
    }
    async fn run_ctxless(
        &self,
        _args: Value,
        _cancel: muagent::core::cancel::CancelToken,
    ) -> Result<ToolOk, ToolErr> {
        Ok(ToolOk::text("abcdefghijklmnopqrstuvwxyz"))
    }
}

struct LongDetailTool {
    d: ToolDescriptor,
}

#[async_trait]
impl Tool for LongDetailTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.d
    }
    async fn run_ctxless(
        &self,
        _args: Value,
        _cancel: muagent::core::cancel::CancelToken,
    ) -> Result<ToolOk, ToolErr> {
        Ok(ToolOk::text("ok").with_detail(json!({
            "blob": "d".repeat(10_000),
        })))
    }
}

#[tokio::test]
async fn tool_output_respects_descriptor_max_out_tokens() {
    let reg = Arc::new(CapabilityRegistry::new());
    let mut d = descriptor("long_output_tool", 5000);
    d.max_out_tokens = 2;
    reg.register(Arc::new(LongOutputTool { d }));

    let exec = DefaultToolExecutor::new(reg);
    let call = PendingCall::new("c1", "long_output_tool", json!({}));
    let r = exec
        .execute(&call, &ToolContext::ephemeral(), CancelToken::never())
        .await
        .unwrap();

    assert!(r.ok);
    assert!(r.text().starts_with("abcdefgh\n"));
    assert!(r.text().contains("truncated to ~2 tokens"));
    assert!(r.text().contains("from_end=true/tail"));
    assert!(!r.text().contains("ijklmnopqrstuvwxyz"));
}

#[tokio::test]
async fn tool_detail_is_capped_before_history_and_model_wire() {
    let reg = Arc::new(CapabilityRegistry::new());
    reg.register(Arc::new(LongDetailTool {
        d: descriptor("long_detail_tool", 5000),
    }));

    let exec = DefaultToolExecutor::new(reg);
    let call = PendingCall::new("c1", "long_detail_tool", json!({}));
    let r = exec
        .execute(&call, &ToolContext::ephemeral(), CancelToken::never())
        .await
        .unwrap();

    assert!(r.ok);
    assert_eq!(r.text(), "ok");
    let detail = r.detail.unwrap();
    assert_eq!(detail["truncated"], true);
    assert!(detail["preview_json_prefix"].as_str().unwrap_or("").len() <= 4096);
}

#[tokio::test]
async fn protocol_error_message_and_detail_are_capped() {
    let reg = Arc::new(CapabilityRegistry::new());
    let exec = DefaultToolExecutor::new(reg);
    let call = PendingCall::new(
        "c1",
        TOOL_PROTOCOL_ERROR_TOOL,
        json!({
            "message": "m".repeat(30_000),
            "hint": "h".repeat(30_000),
            "errors": {"raw": "e".repeat(30_000)},
        }),
    );
    let r = exec
        .execute(&call, &ToolContext::ephemeral(), CancelToken::never())
        .await
        .unwrap();

    assert!(!r.ok);
    assert!(r.text().contains("truncated to ~4096 tokens"));
    assert!(r.hint.unwrap().contains("hint truncated"));
    assert_eq!(r.detail.unwrap()["truncated"], true);
}
