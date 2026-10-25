//! The domain and safety boundary for NovaGPT; no MCP or CLI dependencies.
mod command_ops;
mod context_ops;
mod fs_ops;
mod git_ops;
mod handoff_ops;
mod handoff_status;
mod image_ops;
mod import_ops;
mod inspect_ops;
mod patch_ops;
mod pro_context;
mod process_output;
mod workspace;
mod worktree_ops;

pub use command_ops::{BashMode, BashRequest, BashResult};
pub use context_ops::CodexInstructions;
pub use fs_ops::{EditRequest, FileDocument, FileReceipt, FileWriteRequest, SearchHit, TreeEntry};
pub use git_ops::{GitRequest, GitResult};
pub use handoff_ops::HandoffReceipt;
pub use handoff_status::HandoffStatus;
pub use image_ops::ImageDocument;
pub use import_ops::{AttachmentRef, ImportRequest, ImportResult};
pub use inspect_ops::WorkspaceInspection;
pub use patch_ops::PatchResult;
pub use pro_context::ProContextReceipt;
pub use workspace::{NovaError, NovaResult, Workspace, WorkspaceRegistry};
pub use worktree_ops::OwnedWorktree;
