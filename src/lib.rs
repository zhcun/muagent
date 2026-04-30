pub mod adapters;
pub mod agent_instructions;
pub mod capabilities;
pub mod cli;
pub mod config;
pub mod core;
pub mod oauth;
pub mod providers;
pub mod runtime;
pub mod sessions;
pub mod setup;
pub mod storage;
pub mod tui;

pub mod prelude {
    pub use crate::adapters::{AdapterBundle, FileSystem, ProcessExec, ReqwestEgress, Root, Uri};
    pub use crate::capabilities::skills::{
        loader::{FilesystemSkill, FilesystemSkillLoader},
        Skill, SkillManager,
    };
    pub use crate::capabilities::tools::register_defaults;
    pub use crate::core::prelude::*;
    pub use crate::runtime::{DefaultToolExecutor, DefaultToolSetProvider};
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
    pub use crate::storage::{JsonlSessionStore, MemorySessionStore};
}
