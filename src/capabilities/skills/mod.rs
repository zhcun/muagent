//! Skill — 纯 Markdown 文档型能力包(Anthropic Skills 协议)。
//!
//! **Skill 只是文档,不提供 tools。** tools 来自:
//! - 内置工具(`tools_builtin::register_defaults` 等)
//! - MCP server(`crate::capabilities::mcp::register_mcp_tools`)
//! - host 自定义注册
//!
//! Skill 提供的是:`id + description`(永远在 system prompt 里),可选的
//! `root_uri`(skill 目录的 `file://` 路径,agent 需要详情时用
//! `fs_read` 自己拉)。
//!
//! ## 来源
//!
//! 1. **文件系统**:`FilesystemSkillLoader` 扫 `./.muagent/skills/` 和
//!    `~/.muagent/skills/` 下的 `*/SKILL.md`,解析 YAML frontmatter
//!    (`name:` + `description:`)自动注册。项目级优先。
//! 2. **代码注册**:host 自己 `impl Skill` 然后 `SkillManager::register`。
//!    代码型 skill 基本只在"host 想在 prompt 里塞一段说明但又不想写文件"
//!    的场景用;正常情况下请写 SKILL.md。

pub mod loader;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

pub use loader::{FilesystemSkill, FilesystemSkillLoader};

pub trait Skill: Send + Sync {
    /// 唯一 id。文件系统加载时 = 目录名(和 YAML frontmatter 的 `name` 一致)。
    fn id(&self) -> &str;

    fn version(&self) -> &str {
        "0.1.0"
    }

    /// 短描述(≤ ~1024 char),永远拼进 system prompt。**靠它让 LLM 判断
    /// 何时使用此 skill**;别塞整篇手册进来。
    fn description(&self) -> &str;

    /// 可选:skill 目录的 URI。prompt 里会列出来;agent 自己用 `fs_list` /
    /// `fs_read` / `sh_exec` 按 Anthropic 约定探索(SKILL.md / scripts/ /
    /// reference/ / assets/)。**框架不扫子目录,不替 agent 做任何决定。**
    /// 程序化注册的 skill(没有文件系统对应物)返回 `None`。
    fn root_uri(&self) -> Option<&str> {
        None
    }
}

// ============================================================================
// SkillManager — 纯注册表。没有 active/inactive,没有 tools 汇总。
// ============================================================================

#[derive(Default)]
pub struct SkillManager {
    skills: RwLock<HashMap<String, Arc<dyn Skill>>>,
}

impl SkillManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, skill: Arc<dyn Skill>) {
        let id = skill.id().to_string();
        self.skills.write().unwrap().insert(id, skill);
    }

    pub fn all_skills(&self) -> Vec<Arc<dyn Skill>> {
        self.skills.read().unwrap().values().cloned().collect()
    }

    /// 所有(或过滤后的)skill 拼成可注入 system prompt 的段落。
    /// 格式遵循 Anthropic Skills 的 progressive-disclosure 约定:prompt 里
    /// 只放 `name + 一行 description + SKILL.md 路径`;agent 自行决定何时
    /// `fs_read` 详情 + 何时运行 skill 自带的 scripts。
    /// `allowlist == None` = 全开;`Some(list)` = 仅 list 里的 id。
    /// `denylist` 总是优先,用于默认全开但排除某些 skill。
    pub fn prompt_augmentation(
        &self,
        allowlist: Option<&[String]>,
        denylist: Option<&[String]>,
    ) -> String {
        let skills = self.skills.read().unwrap();
        let mut list: Vec<_> = skills
            .values()
            .filter(|s| match allowlist {
                None => true,
                Some(xs) => xs.iter().any(|x| x == s.id()),
            })
            .filter(|s| !denylist.unwrap_or_default().iter().any(|x| x == s.id()))
            .collect();
        if list.is_empty() {
            return String::new();
        }
        list.sort_by_key(|s| s.id().to_string());

        let mut out = String::from(
            "## Skills\n\
             Each skill is a folder. When a skill looks relevant to the \
             current task, read `<folder>/SKILL.md` via fs_read for \
             instructions. SKILL.md may reference `scripts/` (run via \
             sh_exec), `reference/` (read via fs_read), or `assets/` — \
             use fs_list on the folder if you need to see what's there.\n\n",
        );
        for s in &list {
            let desc = s.description().trim();
            match s.root_uri() {
                Some(root) => out.push_str(&format!("- {}: {} ({})\n", s.id(), desc, root)),
                None => out.push_str(&format!("- {}: {}\n", s.id(), desc)),
            }
        }
        out
    }
}
