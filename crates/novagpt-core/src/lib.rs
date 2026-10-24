//! The domain and safety boundary for NovaGPT; no MCP or CLI dependencies.
mod fs_ops;
mod workspace;

pub use fs_ops::{EditRequest, FileDocument, FileReceipt, FileWriteRequest, SearchHit, TreeEntry};
pub use workspace::{NovaError, NovaResult, Workspace, WorkspaceRegistry};
