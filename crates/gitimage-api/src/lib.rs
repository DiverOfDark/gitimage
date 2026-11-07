//! Shared API surface of GitImage: the `GitImage` CRD, the annotation and label keys,
//! and the controller `Config` read from the `gitimage-config` ConfigMap.

pub mod config;
pub mod gitimage;
pub mod keys;

pub use config::Config;
pub use gitimage::{
    BuildArg, BuildOutcome, BuildRecord, BuildSecret, BuildSpec, Condition, GitImage, GitImageSpec,
    GitImageStatus, OnNewCommit, Resources, SecretKeyRef, SecretRef,
};

/// API group and version of every GitImage resource.
pub const GROUP: &str = "gitimage.dev";
pub const VERSION: &str = "v1alpha1";
