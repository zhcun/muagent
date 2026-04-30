//! `fs_rename`:重命名/移动文件或目录。两端 URI 都必须落在 allowed roots 内。

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
    from: String,
    to: String,
}

pub struct FsRename {
    bundle: Arc<AdapterBundle>,
    desc: ToolDescriptor,
}

impl FsRename {
    pub fn new(bundle: Arc<AdapterBundle>) -> Self {
        let desc = ToolDescriptor {
            name: "fs_rename".into(),
            description: "Rename or move a file/directory. Both `from` and `to` must be within \
                 allowed roots. Atomic on the same filesystem; may fail across mounts."
                .into(),
            schema_json: json!({
                "type": "object",
                "properties": {
                    "from": {"type":"string"},
                    "to":   {"type":"string"},
                },
                "required": ["from","to"],
            }),
            timeout: Duration::from_secs(5),
            max_out_tokens: 128,
            concurrency: Concurrency::Exclusive,
            side_effects: SideEffects::Mutating,
            idempotency: Idempotency::AtMostOnce,
        };
        Self { bundle, desc }
    }
}

#[async_trait]
impl Tool for FsRename {
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
        for (k, v) in [("from", &a.from), ("to", &a.to)] {
            if Uri::new(v).has_dotdot_escape() {
                return GuardOutcome::Deny {
                    reason: format!("`{k}` contains `..`"),
                    hint: None,
                };
            }
        }
        GuardOutcome::Allow
    }

    async fn run_ctxless(&self, args: Value, cancel: CancelToken) -> Result<ToolOk, ToolErr> {
        if cancel.triggered() {
            return Err(ToolErr::deny("cancelled"));
        }
        let a: Args = parse_args(&args)?;
        self.bundle
            .fs
            .rename(&Uri::new(&a.from), &Uri::new(&a.to))
            .await
            .map_err(super::map_fs_err)?;
        Ok(ToolOk::text(format!("renamed {} -> {}", a.from, a.to)))
    }
}
