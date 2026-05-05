//! `fs_stat`:读一个 URI 的元信息(size / is_dir / mtime_ms)。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::core::prelude::{
    parse_args, CancelToken, Concurrency, GuardOutcome, Idempotency, SideEffects, Tool,
    ToolDescriptor, ToolErr, ToolOk,
};

use crate::adapters::{AdapterBundle, Uri};

#[derive(Deserialize)]
struct Args {
    uri: String,
}

pub struct FsStat {
    bundle: Arc<AdapterBundle>,
    desc: ToolDescriptor,
}

impl FsStat {
    pub fn new(bundle: Arc<AdapterBundle>) -> Self {
        let desc = ToolDescriptor {
            name: "fs_stat".into(),
            description: "Get file/dir metadata. Returns JSON with `size`, `is_dir`, `mtime_ms`."
                .into(),
            schema_json: json!({
                "type": "object",
                "properties": { "uri": {"type":"string","description":"Absolute file:// URI, e.g. file:///abs/path."} },
                "required": ["uri"],
            }),
            timeout: Duration::from_secs(2),
            max_out_tokens: 256,
            concurrency: Concurrency::Parallel,
            side_effects: SideEffects::ReadOnly,
            idempotency: Idempotency::Idempotent,
        };
        Self { bundle, desc }
    }
}

#[async_trait]
impl Tool for FsStat {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.desc
    }

    fn guard(&self, args: &Value) -> GuardOutcome {
        let a: Args = match parse_args(args) {
            Ok(a) => a,
            Err(e) => {
                return GuardOutcome::Deny {
                    reason: e.msg,
                    hint: e.hint,
                }
            }
        };
        if Uri::new(&a.uri).has_dotdot_escape() {
            return GuardOutcome::Deny {
                reason: "path contains `..`".into(),
                hint: Some("use absolute file:// paths without `..`".into()),
            };
        }
        GuardOutcome::Allow
    }

    async fn run_ctxless(&self, args: Value, cancel: CancelToken) -> Result<ToolOk, ToolErr> {
        if cancel.triggered() {
            return Err(ToolErr::deny("cancelled"));
        }
        let a: Args = parse_args(&args)?;
        let meta = self
            .bundle
            .fs
            .stat(&Uri::new(&a.uri))
            .await
            .map_err(super::map_fs_err)?;
        Ok(ToolOk::text(
            serde_json::to_string(&json!({
                "uri": a.uri,
                "size": meta.size,
                "is_dir": meta.is_dir,
                "mtime_ms": meta.mtime_ms,
            }))
            .unwrap(),
        ))
    }
}
