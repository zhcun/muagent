//! Offline: net_http tool calls NetEgress with the right shape and
//! routes denied/errored responses into ToolErr variants correctly.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use muagent::adapters::linux::LinuxFileSystem;
use muagent::adapters::AdapterBundle;
use muagent::core::cancel::CancelToken;
use muagent::core::net::{HttpReq, HttpResp, NetEgress, NetErr};
use muagent::core::prelude::*;
use muagent::prelude::*;
use serde_json::{json, Value};
use uuid::Uuid;

struct FakeNet {
    captured: Arc<Mutex<Option<HttpReq>>>,
    reply: Mutex<Result<HttpResp, NetErr>>,
}

#[async_trait]
impl NetEgress for FakeNet {
    async fn http(&self, req: HttpReq, _c: CancelToken) -> Result<HttpResp, NetErr> {
        *self.captured.lock().unwrap() = Some(req);
        // swap in a dummy so subsequent calls fail loud
        let mut guard = self.reply.lock().unwrap();
        std::mem::replace(&mut *guard, Err(NetErr::Io("already consumed".into())))
    }
}

fn wire(reply: Result<HttpResp, NetErr>) -> (Arc<CapabilityRegistry>, Arc<Mutex<Option<HttpReq>>>) {
    let captured = Arc::new(Mutex::new(None));
    let net: Arc<dyn NetEgress> = Arc::new(FakeNet {
        captured: captured.clone(),
        reply: Mutex::new(reply),
    });
    let bundle = Arc::new(
        AdapterBundle::builder()
            .fs(Arc::new(LinuxFileSystem::new(vec![std::env::temp_dir()])))
            .net(net)
            .build()
            .unwrap(),
    );
    let reg = Arc::new(CapabilityRegistry::new());
    muagent::capabilities::tools::register_defaults(&reg, bundle);
    (reg, captured)
}

async fn run(reg: Arc<CapabilityRegistry>, args: Value) -> ToolResult {
    let exec = DefaultToolExecutor::new(reg);
    let ctx = ToolContext {
        session_id: Uuid::nil(),
        run_id: Uuid::nil(),
        turn: 0,
    };
    let call = PendingCall::new(Uuid::new_v4().to_string(), "net_http", args);
    exec.execute(&call, &ctx, CancelToken::never())
        .await
        .expect("exec")
}

fn json_resp(body: &str) -> HttpResp {
    let mut h = HashMap::new();
    h.insert("content-type".into(), "application/json".into());
    HttpResp {
        status: 200,
        headers: h,
        body: body.as_bytes().to_vec(),
    }
}

#[tokio::test]
async fn get_with_json_response_returns_text_body() {
    let (reg, captured) = wire(Ok(json_resp(r#"{"hello":"world"}"#)));
    let r = run(
        reg,
        json!({
            "method":"GET",
            "url":"https://api.example.com/v1/things",
            "headers":{"x-key":"abc"},
        }),
    )
    .await;
    assert!(r.ok, "{:?}", r);

    // Request was shaped correctly.
    let req = captured.lock().unwrap().take().unwrap();
    assert_eq!(req.url, "https://api.example.com/v1/things");
    assert_eq!(req.headers.get("x-key").map(|s| s.as_str()), Some("abc"));

    // Response parsed into JSON envelope with text body.
    let v: Value = serde_json::from_str(&r.text()).unwrap();
    assert_eq!(v["status"], 200);
    assert_eq!(v["body"], r#"{"hello":"world"}"#);
    assert_eq!(v["headers"]["content-type"], "application/json");
    assert_eq!(v["body_truncated"], false);
    assert_eq!(v["body_preview_kind"], "head");
    assert_eq!(v["body_start_byte"], 0);
}

#[tokio::test]
async fn binary_body_returns_base64() {
    let mut h = HashMap::new();
    h.insert("content-type".into(), "application/octet-stream".into());
    let resp = HttpResp {
        status: 200,
        headers: h,
        body: vec![0u8, 1, 2, 3],
    };
    let (reg, _) = wire(Ok(resp));
    let r = run(reg, json!({"method":"GET","url":"https://x.test/bin"})).await;
    assert!(r.ok);
    let v: Value = serde_json::from_str(&r.text()).unwrap();
    assert!(v["body"]["base64"].is_string());
}

#[tokio::test]
async fn large_text_body_returns_bounded_parseable_preview() {
    let large = "x".repeat(100_000);
    let (reg, _) = wire(Ok(json_resp(&large)));
    let r = run(reg, json!({"method":"GET","url":"https://x.test/large"})).await;
    assert!(r.ok, "{:?}", r);
    assert!(
        !r.text().contains("tool output truncated"),
        "net_http should bound the body before generic executor truncation"
    );
    let v: Value = serde_json::from_str(&r.text()).unwrap();
    assert_eq!(v["body_bytes"], 100_000);
    assert_eq!(v["body_returned_bytes"], 32 * 1024);
    assert_eq!(v["body_truncated"], true);
    assert_eq!(v["body_preview_kind"], "head");
    assert_eq!(v["body_end_byte_exclusive"], 32 * 1024);
    assert!(v["body_next"]["note"]
        .as_str()
        .unwrap()
        .contains("Range: bytes=-32768"));
    assert_eq!(v["body"].as_str().unwrap().len(), 32 * 1024);
}

#[tokio::test]
async fn denied_maps_to_non_retryable_err() {
    let (reg, _) = wire(Err(NetErr::Denied("scheme not allowed".into())));
    let r = run(reg, json!({"method":"GET","url":"http://internal.local/"})).await;
    assert!(!r.ok);
    assert!(!r.retryable);
    assert!(r.text().contains("denied"));
}

#[tokio::test]
async fn transient_status_429_is_retryable() {
    let (reg, _) = wire(Err(NetErr::HttpStatus {
        status: 429,
        reason: "slow down".into(),
    }));
    let r = run(
        reg,
        json!({"method":"POST","url":"https://x.test/","body":"{}"}),
    )
    .await;
    assert!(!r.ok);
    assert!(r.retryable);
}

#[tokio::test]
async fn post_body_is_forwarded() {
    let (reg, captured) = wire(Ok(json_resp(r#"{"ok":true}"#)));
    let r = run(
        reg,
        json!({
            "method":"POST",
            "url":"https://x.test/api",
            "headers":{"content-type":"application/json"},
            "body": r#"{"a":1}"#,
        }),
    )
    .await;
    assert!(r.ok, "{:?}", r);
    let req = captured.lock().unwrap().take().unwrap();
    assert_eq!(req.body.as_deref(), Some(br#"{"a":1}"#.as_ref()));
}

#[tokio::test]
async fn options_method_is_forwarded() {
    let (reg, captured) = wire(Ok(json_resp("{}")));
    let r = run(reg, json!({"method":"OPTIONS","url":"https://x.test/api"})).await;
    assert!(r.ok, "{:?}", r);
    let req = captured.lock().unwrap().take().unwrap();
    assert_eq!(req.method, muagent::core::net::HttpMethod::Options);
}

#[tokio::test]
async fn guard_rejects_missing_scheme() {
    let (reg, _) = wire(Ok(json_resp("{}")));
    let r = run(reg, json!({"method":"GET","url":"example.com"})).await;
    assert!(!r.ok);
    assert!(r.text().contains("scheme"));
}

#[tokio::test]
async fn guard_rejects_unknown_method() {
    let (reg, _) = wire(Ok(json_resp("{}")));
    let r = run(reg, json!({"method":"TRACE","url":"https://x.test/"})).await;
    assert!(!r.ok);
}

#[tokio::test]
async fn idempotency_depends_on_method() {
    use muagent::capabilities::tools::net_http::NetHttp;
    // We construct a minimal tool to test the override; we don't need a
    // real registry for this — just the descriptor logic.
    let net: Arc<dyn NetEgress> = Arc::new(FakeNet {
        captured: Arc::new(Mutex::new(None)),
        reply: Mutex::new(Ok(json_resp("{}"))),
    });
    let t = NetHttp::new(net);
    let get = t.idempotency_for_args(&json!({"method":"GET","url":"https://x/"}));
    let post = t.idempotency_for_args(&json!({"method":"POST","url":"https://x/"}));
    assert!(matches!(get, Idempotency::Idempotent));
    assert!(matches!(post, Idempotency::AtMostOnce));
}
