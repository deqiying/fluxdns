//! Versioned configuration loading, normalization and semantic validation.

pub(crate) mod doh_route;
pub mod load;
pub mod migrate;
pub mod model;
pub mod resolve;
pub(crate) mod source_edit;
pub(crate) mod store;
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
    BindEntry, BindPlan, BindProtocol, BindTransport, ConfigError, ConfigErrorKind,
    ConfigErrorReport, DohBindingRef,
};

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_PATH_ID: AtomicU64 = AtomicU64::new(1);

    pub(crate) fn absolute_path(label: &str) -> String {
        let id = NEXT_PATH_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("fluxdns-{label}-{}-{id}", std::process::id()));
        path.to_string_lossy().replace('\\', "/")
    }

    pub(crate) fn portable_example() -> (String, PathBuf) {
        let path = absolute_path("example");
        let source = include_str!("../../../config-example.yaml")
            .replace("path: /etc/fluxdns", &format!("path: {path}"));
        (source, PathBuf::from(path))
    }
}
