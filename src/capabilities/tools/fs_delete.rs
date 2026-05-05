//! `fs_delete`:删除文件或空目录。
//!
//! **Destructive + AtMostOnce**:Core 会在 execute 前 persist 意图,
//! 崩溃后不会重放(避免双删)。对目录只删空的;非空目录请先 list 确认。

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

pub struct FsDelete {
    bundle: Arc<AdapterBundle>,
    desc: ToolDescriptor,
}

impl FsDelete {
    pub fn new(bundle: Arc<AdapterBundle>) -> Self {
        let desc = ToolDescriptor {
            name: "fs_delete".into(),
            description: "Delete a file or empty directory. Destructive; will NOT retry on \
                 interruption. Use fs_list first if you need to remove a non-empty \
                 directory — this tool doesn't recurse."
                .into(),
            schema_json: json!({
                "type": "object",
                "properties": { "uri": {"type":"string","description":"Absolute file:// URI, e.g. file:///abs/path."} },
                "required": ["uri"],
            }),
            timeout: Duration::from_secs(5),
            max_out_tokens: 128,
            concurrency: Concurrency::Exclusive,
            side_effects: SideEffects::Destructive,
            idempotency: Idempotency::AtMostOnce,
        };
        Self { bundle, desc }
    }
}

#[async_trait]
impl Tool for FsDelete {
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
        self.bundle
            .fs
            .delete(&Uri::new(&a.uri))
            .await
            .map_err(super::map_fs_err)?;
        Ok(ToolOk::text(format!("deleted {}", a.uri)))
    }
}
