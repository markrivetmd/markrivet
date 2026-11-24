//! Source-preserving structural engine over Markdown files.
#[cfg(not(target_os = "linux"))]
compile_error!(
    "Rivet supports Linux only; macOS is not supported yet and Windows is unsupported. See README.md: Platforms."
);
pub mod analytics;
pub mod api;
pub mod archive;
pub mod citations;
pub mod commit;
pub mod contract;
pub mod delete_receipt;
pub mod doctor;
pub mod document;
pub mod edit;
pub mod editor_lock;
pub mod find;
pub mod fs;
pub mod git;
mod git_object;
pub mod history;
pub mod markdown;
pub mod markers;
mod native;
pub mod prune_snapshot;
pub mod ready;
pub mod relocate;
pub mod schema;
pub mod status;
#[cfg(test)]
#[path = "../../test-support/mod.rs"]
mod test_support;
mod transaction;
pub mod yaml;

pub use api::{Request, Response, execute};
pub use contract::*;
pub use document::Document;
pub use markdown::inspect;
