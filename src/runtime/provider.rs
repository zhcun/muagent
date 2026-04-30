//! DefaultToolSetProvider:每步把 `CapabilityRegistry` 里的 tools 汇成
//! `ActiveToolSet`,再把 `SkillManager` 里的 skill description 作为
//! session-sticky `PromptBlock` 交给 Runner。
//!
//! **Skills 不贡献 tools**(Anthropic Skills 协议)—— 它们只提供描述。
//! 所有 tools 必须事先注册到 registry(built-in / MCP / host 自定义)。
//!
//! Host 可以 `with_tool_allowlist` / `with_tool_denylist` / skill filters 做过滤 ——
//! 这是唯一的运行期控制手段(不给 agent 暴露 meta-tool)。

use std::sync::Arc;

use async_trait::async_trait;

use crate::core::prelude::{
    ActiveToolSet, ActiveToolSetProvider, CapabilityRegistry, PromptBlock, RunState,
};

use crate::capabilities::skills::SkillManager;

pub struct DefaultToolSetProvider {
    registry: Arc<CapabilityRegistry>,
    skills: Option<Arc<SkillManager>>,
    tool_allowlist: Option<Vec<String>>,
    tool_denylist: Vec<String>,
    skill_allowlist: Option<Vec<String>>,
    skill_denylist: Vec<String>,
    version: std::sync::atomic::AtomicU64,
}

impl DefaultToolSetProvider {
    pub fn new(registry: Arc<CapabilityRegistry>) -> Self {
        Self {
            registry,
            skills: None,
            tool_allowlist: None,
            tool_denylist: Vec::new(),
            skill_allowlist: None,
            skill_denylist: Vec::new(),
            version: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 挂 SkillManager。所有 registered skill 的 description + root_uri
    /// 进 session-sticky prompt block(如果 skill_allowlist 没过滤掉)。
    pub fn with_skills(mut self, skills: Arc<SkillManager>) -> Self {
        self.skills = Some(skills);
        self
    }

    /// 只让 list 里的 tool 名字进 ActiveToolSet。`None`(默认) = 全开。
    pub fn with_tool_allowlist(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.tool_allowlist = Some(names.into_iter().collect());
        self
    }

    /// 不让 list 里的 tool 进 ActiveToolSet。denylist 优先于 allowlist。
    pub fn with_tool_denylist(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.tool_denylist = names.into_iter().collect();
        self
    }

    /// 只让 list 里的 skill id 的 description 进 prompt。`None` = 全开。
    pub fn with_skill_allowlist(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.skill_allowlist = Some(ids.into_iter().collect());
        self
    }

    /// 不让 list 里的 skill id 进 prompt。denylist 优先于 allowlist。
    pub fn with_skill_denylist(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.skill_denylist = ids.into_iter().collect();
        self
    }
}

fn allowed(allowlist: &Option<Vec<String>>, denylist: &[String], name: &str) -> bool {
    if denylist.iter().any(|x| x == name) {
        return false;
    }
    match allowlist {
        None => true,
        Some(xs) => xs.iter().any(|x| x == name),
    }
}

#[async_trait]
impl ActiveToolSetProvider for DefaultToolSetProvider {
    async fn provide(&self, _state: &RunState) -> ActiveToolSet {
        // All tools from the registry, filtered by tool_allowlist.
        let tools: Vec<_> = self
            .registry
            .list()
            .into_iter()
            .filter(|t| {
                allowed(
                    &self.tool_allowlist,
                    &self.tool_denylist,
                    &t.descriptor().name,
                )
            })
            .map(|t| t.descriptor().clone())
            .collect();

        // Skills contribute prompt context only (description + root_uri).
        let augmentation = self
            .skills
            .as_ref()
            .map(|sm| {
                sm.prompt_augmentation(
                    self.skill_allowlist.as_deref(),
                    Some(self.skill_denylist.as_slice()),
                )
            })
            .unwrap_or_default();

        let v = self
            .version
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ActiveToolSet {
            tools,
            prompt_blocks: if augmentation.trim().is_empty() {
                Vec::new()
            } else {
                vec![PromptBlock::session_sticky(
                    "shell.skills.prompt_augmentation",
                    augmentation.clone(),
                )]
            },
            prompt_augmentation: augmentation,
            version: v,
        }
    }
}
