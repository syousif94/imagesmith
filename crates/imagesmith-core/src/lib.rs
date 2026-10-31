//! Headless core for ImageSmith. UI-free so the same build path backs the
//! gpui app, the `imagesmith` CLI, and tests.

pub mod app_dirs;
pub mod build;
pub mod events;
pub mod flash;
pub mod hashing;
pub mod host;
pub mod probe;
pub mod project;
pub mod recipe;
pub mod resolve;
pub mod runner;
pub mod settings;
pub mod size;
pub mod store;
pub mod tracker;
pub mod validate;

/// Bumped whenever the builder image (Containerfile, build.sh, fsctl) changes;
/// part of the rootfs cache key and the store container's identity.
pub const BUILDER_VERSION: &str = "7";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("recipe parse error at line {line}: {message}")]
    RecipeParse { line: usize, message: String },
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("container runtime: {0}")]
    Runtime(String),
    #[error("recipe has {0} blocking error(s)")]
    Invalid(usize),
    #[error("build cancelled")]
    Cancelled,
    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
