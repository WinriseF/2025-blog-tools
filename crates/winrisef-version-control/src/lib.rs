mod export;
mod models;
mod repository;

pub use export::{ExportFormat, ExportLayout, ExportOptions};
pub use models::{
    ConflictPerspective, DiffFile, DiffSession, DiffSummary, GitRef, GitRefKind, GraphCommit,
    PreviewContent, RepositoryOverview, RevisionRef, WorkingTreeGroup,
};
pub use repository::{RepositoryReader, VcsError};

pub const PREVIEW_SIDE_LIMIT: usize = 2 * 1024 * 1024;
pub const EXPORT_SIDE_LIMIT: usize = 32 * 1024 * 1024;

#[cfg(test)]
mod tests;
