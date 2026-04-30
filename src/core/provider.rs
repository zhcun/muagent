//! `ActiveToolSetProvider` trait + `Fn` blanket impl。

use async_trait::async_trait;

use crate::core::prompt::PromptBlock;
use crate::core::run_state::RunState;
use crate::core::tool::ToolDescriptor;

#[derive(Clone, Debug, Default)]
pub struct ActiveToolSet {
    pub tools: Vec<ToolDescriptor>,
    /// Structured prompt contributions. Runner can place cacheable and dynamic
    /// blocks separately instead of treating host guidance as one opaque string.
    pub prompt_blocks: Vec<PromptBlock>,
    /// Backward-compatible prompt contribution. Prefer `prompt_blocks` for new
    /// providers; Runner only falls back to this when no structured blocks are
    /// supplied.
    pub prompt_augmentation: String,
    pub version: u64,
}

/// host / shell 每 step 前调此 trait 提供本 step 的 active tool set。
///
/// 带 `Fn(&RunState) -> ActiveToolSet` 的 blanket impl,host 可直接传闭包:
/// ```ignore
/// let provider = |_state: &RunState| ActiveToolSet::default();
/// Runner::builder().tools_provider(provider).build();
/// ```
#[async_trait]
pub trait ActiveToolSetProvider: Send + Sync {
    async fn provide(&self, state: &RunState) -> ActiveToolSet;
}

#[async_trait]
impl<F> ActiveToolSetProvider for F
where
    F: Fn(&RunState) -> ActiveToolSet + Send + Sync,
{
    async fn provide(&self, state: &RunState) -> ActiveToolSet {
        (self)(state)
    }
}
