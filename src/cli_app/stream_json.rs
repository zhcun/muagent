//! NDJSON event emitter for `muagent exec --output-format stream-json`.
//!
//! Each call writes one JSON object to stdout, terminated by `\n`, and
//! flushes immediately. Stdout is reserved for events; logs and progress
//! go to stderr (handled by the tracing subscriber). Schema follows
//! `STREAM_JSON.md`.

use std::io::{self, Write};
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::core::run_state::Usage;

/// Sink for emitted NDJSON lines. Pulled behind a trait so tests can
/// capture output without poking at stdout.
pub trait LineSink: Send + Sync {
    fn write_line(&self, bytes: &[u8]);
}

struct StdoutSink {
    inner: Mutex<io::Stdout>,
}

impl LineSink for StdoutSink {
    fn write_line(&self, bytes: &[u8]) {
        let Ok(mut out) = self.inner.lock() else {
            return;
        };
        let _ = out.write_all(bytes);
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
}

/// Holds the session id used to tag every terminal event so the host can
/// reconcile streams across resume.
pub struct StreamEmitter {
    session_id: String,
    sink: Box<dyn LineSink>,
}

impl StreamEmitter {
    pub fn new(session_id: String) -> Self {
        Self::with_sink(
            session_id,
            Box::new(StdoutSink {
                inner: Mutex::new(io::stdout()),
            }),
        )
    }

    pub fn with_sink(session_id: String, sink: Box<dyn LineSink>) -> Self {
        Self { session_id, sink }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn emit_session_started(&self, resumed: bool) {
        self.write(json!({
            "type": "session_started",
            "session_id": self.session_id,
            "resumed": resumed,
        }));
    }

    pub fn emit_assistant_text(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.write(json!({
            "type": "assistant_text",
            "text": text,
        }));
    }

    pub fn emit_tool_call_start(&self, call_id: &str, tool_name: &str, input: &Value) {
        self.write(json!({
            "type": "tool_call_start",
            "tool_call_id": call_id,
            "tool_name": tool_name,
            "input": input,
        }));
    }

    pub fn emit_tool_call_result(
        &self,
        call_id: &str,
        ok: bool,
        output: &str,
        error: Option<&str>,
    ) {
        self.write(json!({
            "type": "tool_call_result",
            "tool_call_id": call_id,
            "ok": ok,
            "output": output,
            "error": error,
        }));
    }

    pub fn emit_result(&self, final_text: &str, is_error: bool, usage: &Usage) {
        self.write(json!({
            "type": "result",
            "final_text": final_text,
            "is_error": is_error,
            "session_id": self.session_id,
            "cost_usd": if usage.cost_usd > 0.0 { Some(usage.cost_usd) } else { None },
            "usage": usage_json(usage),
        }));
    }

    pub fn emit_error(&self, message: &str, stage: &str) {
        self.write(json!({
            "type": "error",
            "message": message,
            "stage": stage,
            "session_id": self.session_id,
        }));
    }

    fn write(&self, value: Value) {
        let line = match serde_json::to_string(&value) {
            Ok(s) => s,
            Err(_) => return,
        };
        self.sink.write_line(line.as_bytes());
    }
}

fn usage_json(u: &Usage) -> Value {
    json!({
        "tokens_prompt": u.tokens_prompt,
        "tokens_completion": u.tokens_completion,
        "tokens_cache_read": u.tokens_cache_read,
        "tokens_cache_write": u.tokens_cache_write,
        "tokens_thinking": u.tokens_thinking,
        "turns": u.turns,
        "tool_calls": u.tool_calls,
        "cost_usd": u.cost_usd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    struct CaptureSink {
        lines: Arc<Mutex<Vec<String>>>,
    }

    impl LineSink for CaptureSink {
        fn write_line(&self, bytes: &[u8]) {
            // Mirror StdoutSink semantics: a NDJSON line never contains the
            // trailing newline itself. Storing the JSON body alone keeps the
            // assertions readable.
            let line = std::str::from_utf8(bytes).unwrap_or("").to_string();
            self.lines.lock().unwrap().push(line);
        }
    }

    fn capture() -> (StreamEmitter, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = CaptureSink {
            lines: lines.clone(),
        };
        let emitter = StreamEmitter::with_sink("sess-123".into(), Box::new(sink));
        (emitter, lines)
    }

    fn parse(lines: &Arc<Mutex<Vec<String>>>) -> Vec<Value> {
        lines
            .lock()
            .unwrap()
            .iter()
            .map(|l| serde_json::from_str(l).expect("valid JSON line"))
            .collect()
    }

    #[test]
    fn session_started_event_has_required_fields() {
        let (emitter, lines) = capture();
        emitter.emit_session_started(false);
        let events = parse(&lines);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "session_started");
        assert_eq!(events[0]["session_id"], "sess-123");
        assert_eq!(events[0]["resumed"], false);
    }

    #[test]
    fn assistant_text_skips_empty_chunks() {
        let (emitter, lines) = capture();
        emitter.emit_assistant_text("");
        emitter.emit_assistant_text("hello");
        let events = parse(&lines);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "assistant_text");
        assert_eq!(events[0]["text"], "hello");
    }

    #[test]
    fn tool_call_pair_round_trips_input_and_output() {
        let (emitter, lines) = capture();
        emitter.emit_tool_call_start(
            "call_1",
            "sh_exec",
            &serde_json::json!({"bin": "echo", "args": ["hi"]}),
        );
        emitter.emit_tool_call_result("call_1", true, "hi\n", None);
        let events = parse(&lines);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "tool_call_start");
        assert_eq!(events[0]["tool_call_id"], "call_1");
        assert_eq!(events[0]["tool_name"], "sh_exec");
        assert_eq!(events[0]["input"]["bin"], "echo");
        assert_eq!(events[1]["type"], "tool_call_result");
        assert_eq!(events[1]["tool_call_id"], "call_1");
        assert_eq!(events[1]["ok"], true);
        assert_eq!(events[1]["output"], "hi\n");
        assert!(events[1]["error"].is_null());
    }

    #[test]
    fn result_event_carries_session_and_usage() {
        let (emitter, lines) = capture();
        let mut usage = Usage::default();
        usage.tokens_prompt = 12;
        usage.tokens_completion = 8;
        usage.cost_usd = 0.0125;
        usage.turns = 1;
        emitter.emit_result("done.", false, &usage);
        let events = parse(&lines);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "result");
        assert_eq!(events[0]["final_text"], "done.");
        assert_eq!(events[0]["is_error"], false);
        assert_eq!(events[0]["session_id"], "sess-123");
        assert_eq!(events[0]["cost_usd"], 0.0125);
        assert_eq!(events[0]["usage"]["tokens_prompt"], 12);
        assert_eq!(events[0]["usage"]["tokens_completion"], 8);
        assert_eq!(events[0]["usage"]["turns"], 1);
    }

    #[test]
    fn error_event_replaces_result_on_failure() {
        let (emitter, lines) = capture();
        emitter.emit_error("provider unreachable", "provider");
        let events = parse(&lines);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "error");
        assert_eq!(events[0]["message"], "provider unreachable");
        assert_eq!(events[0]["stage"], "provider");
        assert_eq!(events[0]["session_id"], "sess-123");
    }
}
