//! `fs_edit`: precise file edits with exact search/replace.
//!
//! This is the small-edit counterpart to `fs_write`: it reads the target file,
//! verifies the requested text matches the current contents, then writes the
//! modified file only if every edit is unambiguous. Tool output intentionally
//! includes line-number diagnostics and a compact diff so the next model turn
//! can repair bad edits without guessing.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::core::prelude::{
    parse_args, CancelToken, Concurrency, GuardOutcome, Idempotency, SideEffects, Tool,
    ToolDescriptor, ToolErr, ToolOk,
};

use crate::adapters::{AdapterBundle, ReadOpts, Uri, WriteOpts};

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024; // whole-file edit cap
const MAX_DIFF_LINES: usize = 80;
const MAX_DIFF_CHARS: usize = 3500;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArg {
    old_text: String,
    new_text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    uri: String,
    #[serde(default)]
    edits: Vec<EditArg>,
    #[serde(default)]
    old_text: Option<String>,
    #[serde(default)]
    new_text: Option<String>,
    #[serde(default)]
    replace_all: bool,
    #[serde(default)]
    expected_replacements: Option<usize>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Clone, Debug)]
struct Replacement {
    edit_index: usize,
    start: usize,
    end: usize,
    new_text: String,
}

pub struct FsEdit {
    bundle: Arc<AdapterBundle>,
    desc: ToolDescriptor,
}

impl FsEdit {
    pub fn new(bundle: Arc<AdapterBundle>) -> Self {
        let desc = ToolDescriptor {
            name: "fs_edit".into(),
            description: "Edit one existing UTF-8 text file using exact search/replace. \
                 Prefer this over fs_write for small modifications. By default each \
                 old_text must match exactly once in the original file; duplicate \
                 matches fail with line-number diagnostics so you can add more \
                 surrounding context. Pass replace_all=true only when every \
                 occurrence should change, ideally with expected_replacements. \
                 Multiple edits are matched against the original file and must \
                 not overlap. Success returns changed_lines plus a compact \
                 diff preview; dry_run=true validates and returns the same \
                 diagnostics without writing."
                .into(),
            schema_json: json!({
                "type":"object",
                "properties": {
                    "uri": {"type":"string","description":"Complete URI, e.g. file:///abs/path"},
                    "old_text": {"type":"string","description":"Shortcut for a single replacement. Exact text to replace."},
                    "new_text": {"type":"string","description":"Shortcut for a single replacement. Replacement text."},
                    "edits": {
                        "type":"array",
                        "description":"One or more targeted replacements, each matched against the original file.",
                        "items": {
                            "type":"object",
                            "properties": {
                                "old_text": {"type":"string"},
                                "new_text": {"type":"string"}
                            },
                            "required":["old_text","new_text"],
                            "additionalProperties": false
                        }
                    },
                    "replace_all": {"type":"boolean","default":false,
                                    "description":"If true, replace every occurrence of each old_text. If false, each old_text must be unique."},
                    "expected_replacements": {"type":"integer","minimum":0,
                                              "description":"Optional total replacement count guard. Strongly recommended with replace_all=true."},
                    "dry_run": {"type":"boolean","default":false,
                                "description":"Validate and report the planned edit without writing the file."}
                },
                "required":["uri"],
                "additionalProperties": false
            }),
            timeout: Duration::from_secs(10),
            max_out_tokens: 1536,
            concurrency: Concurrency::Exclusive,
            side_effects: SideEffects::Mutating,
            idempotency: Idempotency::AtMostOnce,
        };
        Self { bundle, desc }
    }
}

#[async_trait]
impl Tool for FsEdit {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.desc
    }

    fn idempotency_for_args(&self, args: &Value) -> Idempotency {
        match args.get("dry_run").and_then(|v| v.as_bool()) {
            Some(true) => Idempotency::Idempotent,
            _ => self.desc.idempotency,
        }
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
        let uri = Uri::new(&a.uri);
        if uri.has_dotdot_escape() {
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
        let uri = Uri::new(&a.uri);
        let edits = prepare_edits(&a)?;

        let meta = self.bundle.fs.stat(&uri).await.map_err(super::map_fs_err)?;
        if meta.is_dir {
            return Err(ToolErr::deny(format!("`{}` is a directory", uri.as_str()))
                .with_hint("use fs_list to enumerate a directory"));
        }
        if meta.size > MAX_FILE_BYTES {
            return Err(ToolErr::deny(format!(
                "file is {} bytes; fs_edit cap is {} bytes (4 MiB)",
                meta.size, MAX_FILE_BYTES
            ))
            .with_hint("use a narrower tool or split the file before editing"));
        }

        if cancel.triggered() {
            return Err(ToolErr::deny("cancelled"));
        }
        let bytes = self
            .bundle
            .fs
            .read(&uri, ReadOpts::range(0, meta.size as usize))
            .await
            .map_err(super::map_fs_err)?;
        let raw = String::from_utf8(bytes).map_err(|_| {
            ToolErr::deny("file is not valid UTF-8").with_hint("fs_edit only edits text files")
        })?;

        let (bom, body) = strip_bom(&raw);
        let line_ending = detect_line_ending(body);
        let base = normalize_newlines(body);
        let replacements =
            plan_replacements(&base, &edits, a.replace_all, a.expected_replacements)?;
        let new_body = apply_replacements(&base, &replacements);
        if new_body == base {
            return Err(ToolErr::deny(format!(
                "no changes made to {}; replacement produced identical content",
                uri.as_str()
            ))
            .with_hint("check whether old_text and new_text are the same"));
        }

        let changed_lines = changed_lines(&base, &replacements);
        let first_changed_line = changed_lines.iter().next().copied();
        let final_text = format!("{}{}", bom, restore_line_endings(&new_body, line_ending));

        if !a.dry_run {
            if cancel.triggered() {
                return Err(ToolErr::deny("cancelled"));
            }
            self.bundle
                .fs
                .write(
                    &uri,
                    final_text.as_bytes(),
                    WriteOpts {
                        append: false,
                        create_dirs: false,
                    },
                )
                .await
                .map_err(super::map_fs_err)?;
        }

        let action = if a.dry_run {
            "fs_edit dry_run"
        } else {
            "fs_edit ok"
        };
        let changed_line_text = render_number_set(&changed_lines);
        let first = first_changed_line.unwrap_or(0);
        let mut summary = format!(
            "{action}: {} replacement(s) in {} (bytes {} -> {}, first_changed_line={first}, changed_lines={changed_line_text})",
            replacements.len(),
            uri.as_str(),
            raw.len(),
            final_text.len(),
        );
        let diff = render_diff_preview(&base, &replacements);
        if !diff.is_empty() {
            summary.push_str("\ndiff:\n");
            summary.push_str(&diff);
        }

        Ok(ToolOk::text(summary).with_detail(json!({
            "uri": uri.as_str(),
            "dry_run": a.dry_run,
            "replacements": replacements.len(),
            "bytes_before": raw.len(),
            "bytes_after": final_text.len(),
            "first_changed_line": first_changed_line,
            "changed_lines": changed_lines.iter().copied().collect::<Vec<_>>(),
            "diff_preview": diff,
        })))
    }
}

fn prepare_edits(a: &Args) -> Result<Vec<EditArg>, ToolErr> {
    let has_shortcut = a.old_text.is_some() || a.new_text.is_some();
    if has_shortcut && !a.edits.is_empty() {
        return Err(ToolErr::deny(
            "pass either edits[] or old_text/new_text, not both",
        ));
    }

    let edits = if has_shortcut {
        let old_text = a
            .old_text
            .clone()
            .ok_or_else(|| ToolErr::deny("old_text is required when new_text is provided"))?;
        let new_text = a
            .new_text
            .clone()
            .ok_or_else(|| ToolErr::deny("new_text is required when old_text is provided"))?;
        vec![EditArg { old_text, new_text }]
    } else {
        a.edits.clone()
    };

    if edits.is_empty() {
        return Err(ToolErr::deny(
            "provide old_text/new_text or at least one edits[] entry",
        ));
    }
    for (i, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(ToolErr::deny(format!(
                "edits[{i}].old_text must not be empty"
            )));
        }
    }
    Ok(edits)
}

fn plan_replacements(
    base: &str,
    edits: &[EditArg],
    replace_all: bool,
    expected_replacements: Option<usize>,
) -> Result<Vec<Replacement>, ToolErr> {
    let mut out = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        let old_text = normalize_newlines(&edit.old_text);
        let new_text = normalize_newlines(&edit.new_text);
        let hits = occurrences(base, &old_text);
        if hits.is_empty() {
            let line_count = old_text.lines().count().max(1);
            return Err(
                ToolErr::deny(format!(
                    "could not find edits[{i}].old_text in the file (old_text_bytes={}, old_text_lines={line_count})",
                    old_text.len(),
                ))
                .with_hint(
                    "reflect: the file content likely differs from your old_text; call fs_read on the target region, then retry with a small exact unique block",
                ),
            );
        }
        if !replace_all && hits.len() != 1 {
            let lines = line_numbers_for_offsets(base, &hits);
            let line_text = render_number_set(&lines);
            return Err(ToolErr::deny(format!(
                "found {} occurrences of edits[{i}].old_text at lines {line_text}; expected exactly 1",
                hits.len(),
            ))
            .with_hint("add surrounding context copied from one target occurrence, or set replace_all=true with expected_replacements if all occurrences should change"));
        }
        for start in hits {
            out.push(Replacement {
                edit_index: i,
                start,
                end: start + old_text.len(),
                new_text: new_text.clone(),
            });
        }
    }

    if let Some(expected) = expected_replacements {
        if out.len() != expected {
            let starts: Vec<usize> = out.iter().map(|r| r.start).collect();
            let lines = line_numbers_for_offsets(base, &starts);
            let line_text = render_number_set(&lines);
            return Err(ToolErr::deny(format!(
                "planned {} replacement(s) at lines {line_text}, but expected_replacements={expected}",
                out.len()
            ))
            .with_hint("adjust expected_replacements or make old_text more specific"));
        }
    }

    out.sort_by_key(|r| (r.start, r.end));
    for pair in out.windows(2) {
        let prev = &pair[0];
        let next = &pair[1];
        if prev.end > next.start {
            return Err(ToolErr::deny(format!(
                "edits[{}] overlaps edits[{}] near line {}; merge nearby changes into one old_text/new_text block",
                prev.edit_index,
                next.edit_index,
                line_number_at(base, next.start),
            )));
        }
    }
    Ok(out)
}

fn apply_replacements(base: &str, replacements: &[Replacement]) -> String {
    let mut out = base.to_string();
    for r in replacements.iter().rev() {
        out.replace_range(r.start..r.end, &r.new_text);
    }
    out
}

fn occurrences(haystack: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut offset = 0;
    while let Some(rel) = haystack[offset..].find(needle) {
        let idx = offset + rel;
        out.push(idx);
        offset = idx + needle.len();
    }
    out
}

fn changed_lines(base: &str, replacements: &[Replacement]) -> BTreeSet<usize> {
    let mut lines = BTreeSet::new();
    for r in replacements {
        let start = line_number_at(base, r.start);
        let end = line_number_at(base, r.end);
        for line in start..=end {
            lines.insert(line);
        }
    }
    lines
}

fn line_number_at(text: &str, byte_idx: usize) -> usize {
    text[..byte_idx].bytes().filter(|b| *b == b'\n').count() + 1
}

fn line_numbers_for_offsets(text: &str, offsets: &[usize]) -> BTreeSet<usize> {
    offsets
        .iter()
        .map(|idx| line_number_at(text, *idx))
        .collect()
}

fn render_number_set(lines: &BTreeSet<usize>) -> String {
    const MAX_LINES: usize = 12;
    let mut rendered: Vec<String> = lines
        .iter()
        .take(MAX_LINES)
        .map(|n| n.to_string())
        .collect();
    if lines.len() > MAX_LINES {
        rendered.push(format!("...(+{})", lines.len() - MAX_LINES));
    }
    rendered.join(",")
}

fn render_diff_preview(base: &str, replacements: &[Replacement]) -> String {
    let base_lines: Vec<&str> = base.lines().collect();
    let mut out = Vec::new();
    let mut chars = 0_usize;

    for r in replacements {
        let old_start = line_number_at(base, r.start);
        let old_end = line_number_at(base, r.end).max(old_start);
        let mut block = Vec::new();
        block.push(format!(
            "@@ edit {} old_lines={old_start}-{old_end} @@",
            r.edit_index
        ));

        if old_start > 1 {
            if let Some(line) = base_lines.get(old_start.saturating_sub(2)) {
                block.push(format!(" {} {}", old_start - 1, line));
            }
        }

        let old_text = &base[r.start..r.end];
        for (idx, line) in split_preview_lines(old_text).into_iter().enumerate() {
            block.push(format!("-{} {}", old_start + idx, line));
        }
        for (idx, line) in split_preview_lines(&r.new_text).into_iter().enumerate() {
            block.push(format!("+{} {}", old_start + idx, line));
        }

        if let Some(line) = base_lines.get(old_end) {
            block.push(format!(" {} {}", old_end + 1, line));
        }

        for line in block {
            chars = chars.saturating_add(line.len()).saturating_add(1);
            if out.len() >= MAX_DIFF_LINES || chars > MAX_DIFF_CHARS {
                out.push(format!(
                    "... (diff preview truncated; {} replacement(s) total)",
                    replacements.len()
                ));
                return out.join("\n");
            }
            out.push(line);
        }
    }

    out.join("\n")
}

fn split_preview_lines(s: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = s.lines().collect();
    if lines.is_empty() {
        lines.push("");
    }
    lines
}

fn strip_bom(s: &str) -> (&str, &str) {
    s.strip_prefix('\u{feff}')
        .map(|rest| ("\u{feff}", rest))
        .unwrap_or(("", s))
}

fn detect_line_ending(s: &str) -> &'static str {
    match (s.find("\r\n"), s.find('\n')) {
        (Some(crlf), Some(lf)) if crlf <= lf => "\r\n",
        (Some(_), None) => "\r\n",
        _ => "\n",
    }
}

fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn restore_line_endings(s: &str, ending: &str) -> String {
    if ending == "\r\n" {
        s.replace('\n', "\r\n")
    } else {
        s.to_string()
    }
}
