//! Versioned configuration loading, normalization and semantic validation.

pub mod load;
pub mod migrate;
pub mod model;
pub mod resolve;
pub mod validate;

pub use load::{
    ConfigLoadError, ConfigLoadOutput, ConfigLoader, LoadOptions, SnapshotStatus, load_from_bytes,
    load_from_path, load_from_str,
};
pub use model::{ConfigDto, RawConfig};
pub use resolve::{
    ProxyScheme, ResolvedClientIp, ResolvedConfig, ResolvedRuleSetRef, ResolvedSecretRef,
    ResolvedSecretValue, SecretResolveError, SecretSourceKind, SecretValidationError,
    ValidatedConfig,
};
pub use validate::{
    BindEntry, BindPlan, BindProtocol, ConfigError, ConfigErrorKind, ConfigErrorReport,
};
