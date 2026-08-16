pub mod architecture;
pub mod cdi;
pub mod json_patch;
pub mod oci_patch;
pub mod reference;
pub mod repo_command;
pub mod repo_lock;
pub mod repository;
pub mod runtime_config;
pub mod runtime_paths;
pub mod tls;
pub mod version;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const VERSION_FULL: &str = concat!(env!("CARGO_PKG_VERSION"), "-dev");

pub use architecture::Architecture;
pub use json_patch::apply_json_patch;
pub use oci_patch::apply_oci_configuration_patches;
pub use reference::{FuzzyReference, Reference};
pub use repo_command::{
    RepoCommandError, RepoOperation, RepoOperationResult, apply_repo_operation,
};
pub use version::{FallbackVersion, ParseOptions, Version, VersionV1, VersionV2};
