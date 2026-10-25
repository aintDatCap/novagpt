//! The domain and safety boundary for NovaGPT; no MCP or CLI dependencies.
mod command_ops;
mod fs_ops;
mod git_ops;
mod import_ops;
mod patch_ops;
mod process_output;
mod workspace;

pub use command_ops::{BashMode, BashRequest, BashResult};
pub use fs_ops::{EditRequest, FileDocument, FileReceipt, FileWriteRequest, SearchHit, TreeEntry};
pub use git_ops::{GitRequest, GitResult};
pub use import_ops::{AttachmentRef, ImportRequest, ImportResult};
pub use patch_ops::PatchResult;
pub use workspace::{NovaError, NovaResult, Workspace, WorkspaceRegistry};
