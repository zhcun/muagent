pub mod executor;
pub mod provider;

pub mod token_estimate {
    pub use crate::sessions::token_estimate::*;
}

pub use executor::DefaultToolExecutor;
pub use provider::DefaultToolSetProvider;

pub mod prelude {
    pub use super::{DefaultToolExecutor, DefaultToolSetProvider};
    pub use crate::adapters::{AdapterBundle, FileSystem, ProcessExec, Root, Uri};
    pub use crate::capabilities::skills::{
        loader::{FilesystemSkill, FilesystemSkillLoader},
        Skill, SkillManager,
    };
    pub use crate::capabilities::tools::register_defaults;
    pub use crate::core::net::NetEgress;
    pub use crate::core::tool::{CapabilityRegistry, GuardOutcome, Tool, ToolErr, ToolOk};
    pub use crate::sessions::{
        archive::{
            ArchiveConfig, ArchiveRotation, PartInfo, SessionArchive, SessionMeta, SummaryInfo,
        },
        compaction::{
            CompactionBudget, CompactionError, CompactionOutcome, CompactionStrategy,
            RunnerCompactor, SummaryCompaction,
        },
        manager::{SearchHit, SessionInfo, SessionManager},
    };
}
