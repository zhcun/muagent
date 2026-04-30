//! Offline: FilesystemSkillLoader + DefaultToolSetProvider allowlist.
//!
//! Per the Anthropic Skills protocol, skills are **description-only** —
//! they don't bundle tools. Tools are registered independently. These
//! tests reflect that:
//! - `skill_allowlist` affects ONLY prompt_augmentation, never the tool set.
//! - `tool_allowlist` affects ONLY the tool set.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use muagent::core::prelude::*;
use muagent::core::tool::{Tool, ToolErr, ToolOk};
use muagent::prelude::*;
use serde_json::{json, Value};
use uuid::Uuid;

fn tmpdir() -> PathBuf {
    let p = std::env::temp_dir().join(format!("muagent-skills-it-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct DummyTool {
    desc: ToolDescriptor,
}
impl DummyTool {
    fn new(name: &str) -> Self {
        Self {
            desc: ToolDescriptor {
                name: name.into(),
                description: format!("tool {name}"),
                schema_json: json!({"type":"object"}),
                timeout: std::time::Duration::from_secs(1),
                max_out_tokens: 50,
                concurrency: Concurrency::Parallel,
                side_effects: SideEffects::ReadOnly,
                idempotency: Idempotency::Idempotent,
            },
        }
    }
}
#[async_trait]
impl Tool for DummyTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.desc
    }
    async fn run_ctxless(
        &self,
        _args: Value,
        _cancel: muagent::core::cancel::CancelToken,
    ) -> Result<ToolOk, ToolErr> {
        Ok(ToolOk::text("ok"))
    }
}

struct CodeSkill;
impl Skill for CodeSkill {
    fn id(&self) -> &str {
        "code-writer"
    }
    fn description(&self) -> &str {
        "Write code in Rust or Python."
    }
}

struct MathSkill;
impl Skill for MathSkill {
    fn id(&self) -> &str {
        "math"
    }
    fn description(&self) -> &str {
        "Precise arithmetic and number theory."
    }
}

#[tokio::test]
async fn fs_loader_reads_description_and_sets_root_uri() {
    let root = tmpdir();
    let skill_dir = root.join("pdf-reader");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        r#"---
name: pdf-reader
description: Read and summarize PDFs. Use when the user shares a .pdf file.
---
# PDF Reader
Detailed instructions here. Read via fs_read when needed.
"#,
    )
    .unwrap();

    let mgr = SkillManager::new();
    let loaded = FilesystemSkillLoader::new()
        .with_root(&root)
        .load_into(&mgr)
        .unwrap();
    assert_eq!(loaded, vec!["pdf-reader".to_string()]);

    let skills = mgr.all_skills();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id(), "pdf-reader");
    assert!(skills[0].description().contains("Read and summarize PDFs"));
    let uri = skills[0].root_uri().expect("root_uri should be set");
    assert!(uri.starts_with("file://"));
    // Points at the DIRECTORY, not SKILL.md.
    assert!(!uri.ends_with("SKILL.md"));
    assert!(uri.ends_with("pdf-reader"));
}

#[tokio::test]
async fn fs_loader_does_not_scan_subfolders() {
    // Contract: the loader only parses SKILL.md. It must NOT list scripts/,
    // reference/, assets/ — agent uses fs_list / fs_read to discover those.
    let root = tmpdir();
    let skill_dir = root.join("analyzer");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        r#"---
name: analyzer
description: Analyze logs.
---
"#,
    )
    .unwrap();
    std::fs::create_dir(skill_dir.join("scripts")).unwrap();
    std::fs::write(skill_dir.join("scripts/parse.py"), "print('hi')").unwrap();
    std::fs::create_dir(skill_dir.join("reference")).unwrap();
    std::fs::write(skill_dir.join("reference/grok.md"), "## Patterns").unwrap();

    let mgr = SkillManager::new();
    FilesystemSkillLoader::new()
        .with_root(&root)
        .load_into(&mgr)
        .unwrap();

    let skill = mgr
        .all_skills()
        .into_iter()
        .find(|s| s.id() == "analyzer")
        .unwrap();
    assert_eq!(skill.id(), "analyzer");
    assert_eq!(skill.description(), "Analyze logs.");
    // root_uri points at the folder — agent will list/read it as needed.
    let root_uri = skill.root_uri().unwrap();
    assert!(root_uri.ends_with("analyzer"));
    assert!(!root_uri.ends_with("SKILL.md"));

    // Prompt mentions the folder and the Anthropic convention in ONE place
    // (not per-skill), so the agent knows to look at SKILL.md and explore.
    let prompt = mgr.prompt_augmentation(None, None);
    assert!(prompt.contains("analyzer"));
    assert!(prompt.contains(root_uri));
    assert!(prompt.contains("SKILL.md"));
    // Must NOT enumerate specific files/folders — that's the agent's job.
    assert!(!prompt.contains("scripts/parse.py"));
    assert!(!prompt.contains("reference/grok.md"));
}

#[tokio::test]
async fn default_provider_lists_tools_and_skill_descriptions() {
    let registry = Arc::new(CapabilityRegistry::new());
    registry.register(Arc::new(DummyTool::new("code_format")));
    registry.register(Arc::new(DummyTool::new("calc_add")));

    let skills = Arc::new(SkillManager::new());
    skills.register(Arc::new(CodeSkill));
    skills.register(Arc::new(MathSkill));

    let provider = DefaultToolSetProvider::new(registry).with_skills(skills);
    let ats = provider
        .provide(&RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0))
        .await;

    // Tools come from the registry, not from skills.
    let names: Vec<&str> = ats.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"code_format"));
    assert!(names.contains(&"calc_add"));

    // Skill descriptions appear in prompt_augmentation.
    assert!(ats.prompt_augmentation.contains("code-writer"));
    assert!(ats.prompt_augmentation.contains("math"));
}

#[tokio::test]
async fn skill_allowlist_filters_only_prompt_not_tools() {
    let registry = Arc::new(CapabilityRegistry::new());
    registry.register(Arc::new(DummyTool::new("code_format")));
    registry.register(Arc::new(DummyTool::new("calc_add")));

    let skills = Arc::new(SkillManager::new());
    skills.register(Arc::new(CodeSkill));
    skills.register(Arc::new(MathSkill));

    let provider = DefaultToolSetProvider::new(registry)
        .with_skills(skills)
        .with_skill_allowlist(["math".to_string()]);

    let ats = provider
        .provide(&RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0))
        .await;

    // Skill allowlist ONLY trims prompt_augmentation — tools are independent.
    let names: Vec<&str> = ats.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"code_format"));
    assert!(names.contains(&"calc_add"));

    assert!(ats.prompt_augmentation.contains("math"));
    assert!(!ats.prompt_augmentation.contains("code-writer"));
}

#[tokio::test]
async fn tool_allowlist_filters_by_tool_name() {
    let registry = Arc::new(CapabilityRegistry::new());
    registry.register(Arc::new(DummyTool::new("keep_me")));
    registry.register(Arc::new(DummyTool::new("drop_me")));

    let provider = DefaultToolSetProvider::new(registry).with_tool_allowlist(["keep_me".into()]);

    let ats = provider
        .provide(&RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0))
        .await;
    let names: Vec<&str> = ats.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"keep_me"));
    assert!(!names.contains(&"drop_me"));
}

#[tokio::test]
async fn tool_denylist_wins_over_allowlist() {
    let registry = Arc::new(CapabilityRegistry::new());
    registry.register(Arc::new(DummyTool::new("keep_me")));
    registry.register(Arc::new(DummyTool::new("drop_me")));

    let provider = DefaultToolSetProvider::new(registry)
        .with_tool_allowlist(["keep_me".into(), "drop_me".into()])
        .with_tool_denylist(["drop_me".into()]);

    let ats = provider
        .provide(&RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0))
        .await;
    let names: Vec<&str> = ats.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"keep_me"));
    assert!(!names.contains(&"drop_me"));
}

#[tokio::test]
async fn empty_skill_allowlist_hides_all_skill_descriptions() {
    let registry = Arc::new(CapabilityRegistry::new());
    let skills = Arc::new(SkillManager::new());
    skills.register(Arc::new(MathSkill));

    let provider = DefaultToolSetProvider::new(registry)
        .with_skills(skills)
        .with_skill_allowlist(std::iter::empty::<String>());

    let ats = provider
        .provide(&RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0))
        .await;
    assert!(!ats.prompt_augmentation.contains("math"));
}

#[tokio::test]
async fn skill_denylist_wins_over_allowlist() {
    let registry = Arc::new(CapabilityRegistry::new());
    let skills = Arc::new(SkillManager::new());
    skills.register(Arc::new(CodeSkill));
    skills.register(Arc::new(MathSkill));

    let provider = DefaultToolSetProvider::new(registry)
        .with_skills(skills)
        .with_skill_allowlist(["code-writer".into(), "math".into()])
        .with_skill_denylist(["code-writer".into()]);

    let ats = provider
        .provide(&RunState::new(Uuid::new_v4(), Uuid::new_v4(), 0))
        .await;
    assert!(ats.prompt_augmentation.contains("math"));
    assert!(!ats.prompt_augmentation.contains("code-writer"));
}
