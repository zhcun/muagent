//! Filesystem-backed skill discovery.
//!
//! Scans one or more root directories for `*/SKILL.md` files, parses a tiny
//! YAML frontmatter (`name:` + `description:`), and returns a `Skill` per
//! document. The body is **not** loaded into memory — the agent reads it
//! on demand via `fs_read` (progressive disclosure).
//!
//! Conventions:
//! - Directory name = skill id (must match YAML `name:` or we use the dir).
//! - Project-level roots win over user-level: if the same skill id appears
//!   in `./.muagent/skills/foo/` and `~/.muagent/skills/foo/`, the project
//!   version is registered and the user version is dropped.
//!
//! Frontmatter format (deliberately small; no full YAML parser):
//!
//! ```text
//! ---
//! name: pdf-reader
//! description: Read PDFs and summarize them. Use when the user shares a PDF.
//! ---
//! # PDF Reader
//! ... (body; not parsed)
//! ```
//!
//! `description:` may span multiple lines until the next `---` or a
//! top-level `key:` line. Values may also be `> |` block scalars.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::capabilities::skills::{Skill, SkillManager};

pub struct FilesystemSkill {
    id: String,
    description: String,
    /// `file://` URI of the skill's root directory. Agent explores its
    /// contents via `fs_list` / `fs_read` — the loader never peeks inside.
    root_uri: String,
}

impl Skill for FilesystemSkill {
    fn id(&self) -> &str {
        &self.id
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn root_uri(&self) -> Option<&str> {
        Some(&self.root_uri)
    }
}

#[derive(Clone, Debug, Default)]
pub struct FilesystemSkillLoader {
    /// Searched in order; earlier roots win on id conflict.
    pub roots: Vec<PathBuf>,
}

impl FilesystemSkillLoader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.roots.push(root.into());
        self
    }

    /// Default roots: `./.muagent/skills/` (project) then `~/.muagent/skills/` (user).
    pub fn default_roots() -> Self {
        let mut loader = Self::new();
        if let Ok(cwd) = std::env::current_dir() {
            loader.roots.push(cwd.join(".muagent").join("skills"));
        }
        if let Some(home) = home_dir() {
            loader.roots.push(home.join(".muagent").join("skills"));
        }
        loader
    }

    /// Discover + register all skills into `mgr`. Returns the ids loaded.
    /// Non-fatal: missing roots are skipped; malformed SKILL.md files are
    /// logged via `tracing::warn!` and skipped.
    pub fn load_into(&self, mgr: &SkillManager) -> std::io::Result<Vec<String>> {
        let mut loaded: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for root in &self.roots {
            if !root.is_dir() {
                continue;
            }
            let entries = std::fs::read_dir(root)?;
            for e in entries {
                let e = match e {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let path = e.path();
                if !path.is_dir() {
                    continue;
                }
                let skill_md = path.join("SKILL.md");
                if !skill_md.is_file() {
                    continue;
                }

                let dir_name = match path.file_name().and_then(|s| s.to_str()) {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                if seen.contains(&dir_name) {
                    // A higher-priority root already won.
                    continue;
                }

                match parse_skill_md(&skill_md, &dir_name) {
                    Ok(skill) => {
                        seen.insert(skill.id.clone());
                        loaded.push(skill.id.clone());
                        mgr.register(Arc::new(skill));
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %skill_md.display(),
                            error = %e,
                            "skipping malformed SKILL.md",
                        );
                    }
                }
            }
        }
        Ok(loaded)
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

/// Parse a SKILL.md file: read only the frontmatter block.
/// Returns a `FilesystemSkill` whose `root_uri` points at the skill's
/// directory (parent of SKILL.md). The loader never peeks inside the
/// directory beyond parsing SKILL.md — agent uses `fs_list` / `fs_read`
/// to explore scripts/reference/assets on demand.
fn parse_skill_md(path: &Path, fallback_id: &str) -> std::io::Result<FilesystemSkill> {
    let text = std::fs::read_to_string(path)?;
    let (front, _body) = split_frontmatter(&text);
    let (name, description) = extract_name_and_description(front);

    let id = name.unwrap_or_else(|| fallback_id.to_string());
    if description.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "SKILL.md missing `description:` in frontmatter",
        ));
    }

    // URI points at the DIRECTORY, not SKILL.md. Agent uses fs_read/fs_list
    // to explore — framework doesn't scan subfolders.
    let skill_md_abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let root = skill_md_abs
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| skill_md_abs.clone());

    Ok(FilesystemSkill {
        id,
        description,
        root_uri: format!("file://{}", root.display()),
    })
}

/// Split a markdown string at its leading `---\n...\n---\n` frontmatter.
/// If no frontmatter, returns ("", whole_text).
fn split_frontmatter(s: &str) -> (&str, &str) {
    let rest = match s.strip_prefix("---\n") {
        Some(r) => r,
        None => match s.strip_prefix("---\r\n") {
            Some(r) => r,
            None => return ("", s),
        },
    };
    // Find the closing '---' on its own line.
    let end_marker_idx = rest
        .find("\n---\n")
        .or_else(|| rest.find("\n---\r\n"))
        .or_else(|| {
            if rest.trim_end() == "---" {
                Some(rest.len())
            } else {
                None
            }
        });
    match end_marker_idx {
        Some(i) => {
            let front = &rest[..i];
            // Body starts after the closing ---\n
            let after = rest[i..]
                .trim_start_matches('\n')
                .trim_start_matches("---\n");
            (front, after)
        }
        None => ("", s),
    }
}

/// Extract `name:` and `description:` from YAML-ish frontmatter. Supports
/// single-line values and simple multi-line (subsequent indented lines, or
/// `|` block style until the next `key:` at column 0).
fn extract_name_and_description(front: &str) -> (Option<String>, String) {
    let mut name: Option<String> = None;
    let mut desc_lines: Vec<String> = Vec::new();
    let mut in_desc = false;
    let mut desc_block_style = false;

    for line in front.lines() {
        // A new top-level key resets any multi-line capture.
        let is_top_key = line
            .chars()
            .next()
            .map(|c| !c.is_whitespace())
            .unwrap_or(false)
            && line.contains(':');

        if is_top_key {
            in_desc = false;
            desc_block_style = false;
            if let Some((k, v)) = line.split_once(':') {
                let k = k.trim().to_ascii_lowercase();
                let v = v.trim();
                match k.as_str() {
                    "name" => {
                        name = Some(strip_quotes(v).to_string());
                    }
                    "description" => {
                        if v == "|" || v == ">" || v == "|-" || v == ">-" {
                            in_desc = true;
                            desc_block_style = true;
                        } else if !v.is_empty() {
                            desc_lines.push(strip_quotes(v).to_string());
                            in_desc = true; // allow unindented continuation only via block style
                        } else {
                            in_desc = true;
                            desc_block_style = true;
                        }
                    }
                    _ => {}
                }
            }
        } else if in_desc {
            if desc_block_style {
                // Keep block-indented lines; trim the common-leading-whitespace loosely.
                let trimmed = line.trim_start();
                desc_lines.push(trimmed.to_string());
            } else {
                // Continuation of a single-line value — only accept indented lines.
                let trimmed = line.trim();
                if line.starts_with(' ') || line.starts_with('\t') {
                    if !trimmed.is_empty() {
                        desc_lines.push(trimmed.to_string());
                    }
                } else {
                    in_desc = false;
                }
            }
        }
    }

    let description = desc_lines.join(" ").trim().to_string();
    (name, description)
}

fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        if (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
        {
            return &s[1..s.len() - 1];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_frontmatter_basic() {
        let s = "---\nname: x\ndescription: y\n---\nbody";
        let (front, body) = split_frontmatter(s);
        assert!(front.contains("name: x"));
        assert!(body.starts_with("body"));
    }

    #[test]
    fn split_frontmatter_missing() {
        let s = "no frontmatter here\njust text";
        let (front, body) = split_frontmatter(s);
        assert_eq!(front, "");
        assert_eq!(body, s);
    }

    #[test]
    fn extract_single_line() {
        let front = "name: foo\ndescription: Short desc.\n";
        let (n, d) = extract_name_and_description(front);
        assert_eq!(n.as_deref(), Some("foo"));
        assert_eq!(d, "Short desc.");
    }

    #[test]
    fn extract_quoted() {
        let front = "name: \"pdf-reader\"\ndescription: 'Read PDFs'\n";
        let (n, d) = extract_name_and_description(front);
        assert_eq!(n.as_deref(), Some("pdf-reader"));
        assert_eq!(d, "Read PDFs");
    }

    #[test]
    fn extract_block_scalar() {
        let front = "name: multi\ndescription: |\n  Line one.\n  Line two.\n";
        let (_, d) = extract_name_and_description(front);
        assert!(d.contains("Line one."));
        assert!(d.contains("Line two."));
    }

    #[test]
    fn loader_picks_project_over_user_on_id_conflict() {
        let base = tempdir();
        let proj = base.join("proj");
        let user = base.join("user");
        std::fs::create_dir_all(proj.join("foo")).unwrap();
        std::fs::create_dir_all(user.join("foo")).unwrap();
        std::fs::write(
            proj.join("foo/SKILL.md"),
            "---\nname: foo\ndescription: PROJECT version\n---\nbody",
        )
        .unwrap();
        std::fs::write(
            user.join("foo/SKILL.md"),
            "---\nname: foo\ndescription: USER version\n---\nbody",
        )
        .unwrap();

        let mgr = SkillManager::new();
        let loader = FilesystemSkillLoader::new()
            .with_root(&proj)
            .with_root(&user);
        let ids = loader.load_into(&mgr).unwrap();
        assert_eq!(ids.len(), 1);
        let all = mgr.all_skills();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].description(), "PROJECT version");
    }

    #[test]
    fn loader_skips_missing_description() {
        let base = tempdir();
        std::fs::create_dir_all(base.join("bad")).unwrap();
        std::fs::write(base.join("bad/SKILL.md"), "---\nname: bad\n---\nbody only").unwrap();

        let mgr = SkillManager::new();
        let ids = FilesystemSkillLoader::new()
            .with_root(&base)
            .load_into(&mgr)
            .unwrap();
        assert!(ids.is_empty());
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("muagent-skills-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
