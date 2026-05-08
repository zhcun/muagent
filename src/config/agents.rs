//! File-backed subagent definitions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::core::subagent::AgentDefinition;

use super::{expand_home, home_dir, split_list};

pub(super) fn load_agent_definitions(root: &Path) -> Vec<AgentDefinition> {
    let mut defs = BTreeMap::new();
    for dir in agent_dirs(root) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            match parse_agent_file(&path) {
                Ok(def) => {
                    defs.insert(def.name.clone(), def);
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "subagent file ignored");
                }
            }
        }
    }
    defs.into_values().collect()
}

fn agent_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home_dir() {
        dirs.push(home.join(".muagent").join("agents"));
    }
    dirs.push(expand_home(
        root.join(".muagent")
            .join("agents")
            .to_string_lossy()
            .as_ref(),
    ));
    dirs
}

fn parse_agent_file(path: &Path) -> Result<AgentDefinition, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("subagent")
        .to_string();
    parse_agent_text(&text, &stem)
}

pub(super) fn parse_agent_text(text: &str, fallback_name: &str) -> Result<AgentDefinition, String> {
    let (frontmatter, body) = split_frontmatter(text);
    let fields = parse_frontmatter(frontmatter)?;
    let name = fields
        .get("name")
        .cloned()
        .unwrap_or_else(|| fallback_name.to_string());
    validate_name(&name)?;
    let description = fields.get("description").cloned().unwrap_or_default();
    let instructions = fields
        .get("instructions")
        .cloned()
        .unwrap_or_else(|| body.trim().to_string());
    if instructions.trim().is_empty() {
        return Err("instructions/body cannot be empty".into());
    }
    let tools = fields.get("tools").map(|raw| split_list(raw));
    let skills = fields.get("skills").map(|raw| split_list(raw));
    let model = fields
        .get("model")
        .cloned()
        .filter(|s| !s.trim().is_empty());
    let max_steps = fields
        .get("max_steps")
        .map(|raw| {
            raw.parse::<usize>()
                .map(|n| n.max(1))
                .map_err(|_| "max_steps must be a positive integer".to_string())
        })
        .transpose()?;

    Ok(AgentDefinition {
        name,
        description,
        instructions,
        tools,
        skills,
        model,
        max_steps,
        context_mode: Default::default(),
    })
}

fn split_frontmatter(text: &str) -> (&str, &str) {
    let trimmed = text.strip_prefix('\u{feff}').unwrap_or(text);
    if !trimmed.starts_with("---\n") {
        return ("", trimmed);
    }
    let rest = &trimmed[4..];
    match rest.find("\n---") {
        Some(end) => (&rest[..end], rest[end + 4..].trim_start_matches('\n')),
        None => ("", trimmed),
    }
}

fn parse_frontmatter(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut fields = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!("invalid frontmatter line `{line}`"));
        };
        let key = key.trim().replace('-', "_").to_ascii_lowercase();
        let value = value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        fields.insert(key, value);
    }
    Ok(fields)
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("name cannot be empty".into());
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err("name may contain only ASCII letters, digits, `_`, or `-`".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_markdown_agent_definition() {
        let def = parse_agent_text(
            r#"---
name: reviewer
description: Review code changes
tools: fs_read, fs_list
model: openai/gpt-5.4-nano
max_steps: 42
---
Read the diff and find correctness issues.
"#,
            "fallback",
        )
        .unwrap();

        assert_eq!(def.name, "reviewer");
        assert_eq!(def.description, "Review code changes");
        assert_eq!(
            def.tools.as_deref(),
            Some(["fs_read".into(), "fs_list".into()].as_slice())
        );
        assert_eq!(def.model.as_deref(), Some("openai/gpt-5.4-nano"));
        assert_eq!(def.max_steps, Some(42));
        assert!(def.instructions.contains("Read the diff"));
    }
}
