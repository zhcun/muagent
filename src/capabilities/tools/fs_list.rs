//! `fs_list`:列出目录内容,一行一项,`<kind> <size> <uri>`。

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
    #[serde(default = "default_max_entries")]
    max_entries: u64,
}

fn default_max_entries() -> u64 {
    500
}

const MAX_ENTRIES_CAP: usize = 5000;

pub struct FsList {
    bundle: Arc<AdapterBundle>,
    desc: ToolDescriptor,
}

impl FsList {
    pub fn new(bundle: Arc<AdapterBundle>) -> Self {
        let desc = ToolDescriptor {
            name: "fs_list".into(),
            description: "List a directory. One line per entry: `<kind> <size> <uri>` \
                 where kind is `dir` or `file` and size is in bytes. \
                 Defaults: max_entries=500; hard cap 5000 entries per call. \
                 When the directory has more \
                 entries, the output appends a `… (truncated to N entries)` \
                 sentinel so you know to request a larger max_entries or \
                 drill in."
                .into(),
            schema_json: json!({
                "type": "object",
                "properties": {
                    "uri": {"type":"string", "description":"Directory URI."},
                    "max_entries": {"type":"integer", "minimum":1, "default":500},
                },
                "required": ["uri"],
            }),
            timeout: Duration::from_secs(5),
            max_out_tokens: 4096,
            concurrency: Concurrency::Parallel,
            side_effects: SideEffects::ReadOnly,
            idempotency: Idempotency::Idempotent,
        };
        Self { bundle, desc }
    }
}

#[async_trait]
impl Tool for FsList {
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
                hint: Some("use absolute paths within a root".into()),
            };
        }
        GuardOutcome::Allow
    }

    async fn run_ctxless(&self, args: Value, cancel: CancelToken) -> Result<ToolOk, ToolErr> {
        if cancel.triggered() {
            return Err(ToolErr::deny("cancelled"));
        }
        let a: Args = parse_args(&args)?;
        let requested_max_entries = usize::try_from(a.max_entries).unwrap_or(usize::MAX);
        let max_entries = requested_max_entries.min(MAX_ENTRIES_CAP);

        let uri = Uri::new(&a.uri);
        let mut entries = self.bundle.fs.list(&uri).await.map_err(super::map_fs_err)?;
        let truncated = entries.len() > max_entries;
        if truncated {
            entries.truncate(max_entries);
        }

        let mut out = String::new();
        for e in &entries {
            let kind = if e.is_dir { "dir " } else { "file" };
            out.push_str(&format!("{} {:>10} {}\n", kind, e.size, e.uri.0));
        }
        if truncated {
            out.push_str(&format!("… (truncated to {} entries)\n", max_entries));
        }
        if requested_max_entries > MAX_ENTRIES_CAP {
            out.push_str(&format!(
                "… (requested max_entries={} clamped to hard cap {})\n",
                requested_max_entries, MAX_ENTRIES_CAP
            ));
        }
        if out.is_empty() {
            out.push_str("(empty directory)\n");
        }
        Ok(ToolOk::text(out))
    }
}
