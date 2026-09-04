//! Device type model, node resolution and OPC UA value to thin-edge payload conversion.
#![forbid(unsafe_code)]

pub mod constraints;
pub mod model;
pub mod namespace;
pub mod payload;
pub mod resolve;
pub mod source;
pub mod value;

pub use model::{
    AlarmCreation, ApplyConstraints, DeviceType, EventCreation, MappingEntry, MeasurementCreation,
    SubscriptionType,
};
pub use resolve::{Action, ResolvedMapping, ResolvedNode};

/// Errors raised while loading or resolving device types.
#[derive(Debug, thiserror::Error)]
pub enum MappingError {
    #[error("invalid node id `{0}`")]
    InvalidNodeId(String),
    #[error("namespace uri `{0}` is not in the server namespace table")]
    UnknownNamespace(String),
    #[error("failed to read mapping file {path}: {source}")]
    ReadFile {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse mapping file {path}: {source}")]
    ParseFile {
        path: std::path::PathBuf,
        #[source]
        source: serde_json::Error,
    },
}
