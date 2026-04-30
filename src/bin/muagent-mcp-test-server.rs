//! Stdio MCP test server: reads line-delimited JSON from stdin, writes
//! line-delimited JSON responses to stdout. All protocol logic lives in
//! `muagent::capabilities::mcp::test_support::handle_rpc`.

use std::io::{BufRead, Write};

use muagent::capabilities::mcp::test_support::handle_rpc;
use serde_json::Value;

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) if !l.trim().is_empty() => l,
            Ok(_) => continue,
            Err(_) => break,
        };
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(reply) = handle_rpc(&req) {
            let mut buf = serde_json::to_vec(&reply).unwrap();
            buf.push(b'\n');
            if out.write_all(&buf).is_err() {
                break;
            }
            if out.flush().is_err() {
                break;
            }
        }
    }
}
