//! Agent instruction files (`AGENT.md`, `AGENTS.md`, `CLAUDE.md`).
//!
//! This is CLI/shell policy, not core runtime behavior: the Runner should not
//! know about host filesystem conventions. We load these once at startup and
//! append them to the cacheable base system prompt as session-sticky context.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

const FILE_NAMES: &[&str] = &[
    "AGENT.md",
    "AGENTS.md",
    "agent.md",
    "agents.md",
    "CLAUDE.md",
    "claude.md",
];

const DEFAULT_AGENT_RULES: &str = "\
- You are not only a coding assistant. Treat file work, document cleanup, data extraction, project operations, troubleshooting, research over local materials, and software tasks as first-class work.
- The user usually wants execution, not a lecture. If the request is actionable and the next step is clear, act with the available tools.
- Keep scope proportional. If the user asks for a minimal check, do not turn it into a broad audit. If they say the project or style is minimal, keep the solution minimal.
- Ask only when the missing fact cannot be discovered locally and a guess would likely produce the wrong result.
- Inspect relevant context before acting: named files, nearby files, config, docs, tests, command output, prior tool results, or process state.
- Choose the smallest useful action that moves the task forward. When editing, make a coherent scoped change and avoid unrelated rewrites, broad formatting churn, and unnecessary abstractions.
- Use the filesystem directly when the task is about locating, reading, comparing, rewriting, renaming, organizing, extracting, or preparing files.
- Preserve the user's existing work. If unrelated changes are present, ignore them unless they affect the task; if they affect the task, work with them.
- Prefer documented project commands for build, test, benchmark, formatter, linter, export, import, or data processing. Otherwise inspect locally before choosing commands.
- For structured data such as JSON, YAML, CSV, TOML, markdown, config, and logs, preserve format and use structure-aware reasoning.
- Deleting, moving, renaming, overwriting files, or running destructive commands requires clear user intent or an explicit task need.
- Prefer deterministic commands that finish on their own. Avoid interactive programs unless the user requested an interactive workflow.
- Do not invent host restrictions. If a tool is unavailable, denied, times out, or returns a policy/error hint, report and adapt to that exact result.
- Verify with the narrowest meaningful check first. Use broader checks when the change touches shared behavior or an important workflow.
- If a build, test, lint, or benchmark fails, inspect the failure and distinguish your change from pre-existing problems.
- For code review, lead with concrete findings ordered by severity: bugs, regressions, missing tests, data loss risks, correctness issues, or user-visible failures.
- For local questions, verify with tools instead of relying on memory. For counts, dates, versions, and exact values, prefer deterministic inspection.
- Stop when the requested outcome is handled. Final answers should be concise and concrete: outcome, changed files, checks run, important measured results, and remaining blockers.
- If tests or verification were not run, say so directly and why. Do not claim a command, test, benchmark, or file operation succeeded unless it actually ran and output supports that claim.
- Use the user's language when practical. If the user asks for exact output or says \"return only\", return only the requested value.
";

#[derive(Clone, Debug)]
pub struct AgentInstructionFile {
    pub path: PathBuf,
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct AgentInstructionSet {
    pub files: Vec<AgentInstructionFile>,
    pub default_used: bool,
}

impl AgentInstructionSet {
    pub fn render(&self) -> String {
        let mut out = String::from("## Agent instructions\n\n");
        if self.default_used {
            out.push_str(
                "These default agent instructions are lower priority than system, developer, \
                 and current user instructions.\n",
            );
        } else {
            out.push_str(
                "These default and local agent instructions are lower priority than system, \
                 developer, and current user instructions. Local instruction files are \
                 workspace/user guidance; when files conflict, later and more specific entries \
                 should win.\n",
            );
        }

        out.push_str("\n### Default operating rules\n\n");
        out.push_str(DEFAULT_AGENT_RULES);

        if !self.default_used {
            for file in &self.files {
                out.push_str("\n### ");
                out.push_str(&file.path.display().to_string());
                if file.truncated {
                    out.push_str(" (truncated)");
                }
                out.push_str("\n\n");
                out.push_str(file.text.trim());
                out.push('\n');
            }
        }
        out
    }
}

pub fn load(root: &Path, max_bytes_per_file: usize) -> AgentInstructionSet {
    let home = home_dir();
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    load_with_home(&root, home.as_deref(), max_bytes_per_file)
}

fn load_with_home(
    root: &Path,
    home: Option<&Path>,
    max_bytes_per_file: usize,
) -> AgentInstructionSet {
    let max_bytes = max_bytes_per_file.max(1);
    let mut candidates = Vec::new();

    if let Some(home) = home {
        for dir in [
            home.to_path_buf(),
            home.join(".muagent"),
            home.join(".config").join("muagent"),
            home.join(".claude"),
        ] {
            add_dir_candidates(&mut candidates, &dir);
        }
    }

    let mut ancestors = root
        .ancestors()
        .map(Path::to_path_buf)
        .collect::<Vec<PathBuf>>();
    ancestors.reverse();
    for dir in ancestors {
        add_dir_candidates(&mut candidates, &dir);
    }

    let mut seen = HashSet::new();
    let mut files = Vec::new();

    for path in candidates {
        let key = path.clone();
        if !seen.insert(key) {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        match read_limited(&path, max_bytes) {
            Ok((text, truncated)) if !text.trim().is_empty() => {
                files.push(AgentInstructionFile {
                    path,
                    text,
                    truncated,
                });
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    target: "muagent::agent_instructions",
                    path = %path.display(),
                    error = %e,
                    "failed to load agent instruction file"
                );
            }
        }
    }

    AgentInstructionSet {
        default_used: files.is_empty(),
        files,
    }
}

fn add_dir_candidates(out: &mut Vec<PathBuf>, dir: &Path) {
    for name in FILE_NAMES {
        out.push(dir.join(name));
    }
}

fn read_limited(path: &Path, max_bytes: usize) -> std::io::Result<(String, bool)> {
    let bytes = std::fs::read(path)?;
    let truncated = bytes.len() > max_bytes;
    let mut cut = bytes.into_iter().take(max_bytes).collect::<Vec<u8>>();
    while std::str::from_utf8(&cut).is_err() && !cut.is_empty() {
        cut.pop();
    }
    let mut text = String::from_utf8_lossy(&cut).to_string();
    if truncated {
        text.push_str("\n\n[agent instruction file truncated by MUAGENT_AGENT_MD_MAX_BYTES]\n");
    }
    Ok((text, truncated))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("muagent-agent-md-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn default_rules_are_used_when_no_files_exist() {
        let dir = tempdir();
        let set = load_with_home(&dir, None, 4096);
        assert!(set.default_used);
        assert!(set.files.is_empty());
        let rendered = set.render();
        assert!(rendered.contains("Default operating rules"));
        assert!(!rendered.contains("No local AGENT.md"));
        assert!(!rendered.contains("### Local instruction files"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn loads_user_and_project_files_project_last() {
        let base = tempdir();
        let home = base.join("home");
        let project = base.join("repo").join("crates").join("app");
        std::fs::create_dir_all(home.join(".muagent")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(home.join(".muagent").join("AGENT.md"), "global rule").unwrap();
        std::fs::write(base.join("repo").join("AGENTS.md"), "repo rule").unwrap();
        std::fs::write(project.join("agent.md"), "nested rule").unwrap();

        let set = load_with_home(&project, Some(&home), 4096);
        let rendered = set.render();
        let default = rendered.find("Default operating rules").unwrap();
        let global = rendered.find("global rule").unwrap();
        let repo = rendered.find("repo rule").unwrap();
        let nested = rendered.find("nested rule").unwrap();
        assert!(!set.default_used);
        assert!(default < global);
        assert!(global < repo);
        assert!(repo < nested);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn supports_claude_md_and_truncates_large_files() {
        let dir = tempdir();
        std::fs::write(dir.join("CLAUDE.md"), "abcdef").unwrap();
        let set = load_with_home(&dir, None, 3);
        let rendered = set.render();
        assert!(rendered.contains("abc"));
        assert!(rendered.contains("truncated"));
        assert!(!rendered.contains("abcdef"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
