//! Versioned, deterministic configuration migrations.
//!
//! This module deliberately deals in raw document bytes rather than a config DTO.  The
//! loader can therefore parse the current version only after all explicit migrations have
//! completed, while migration steps remain independent of the eventual config model.

use std::{error::Error, fmt, sync::Arc};

/// The schema revision currently understood by the application.
pub const CURRENT_CONFIG_VERSION: u32 = 1;

/// A pure migration callback.
///
/// The callback must not perform I/O or depend on process state.  Returning a `String`
/// keeps parsing/conversion errors local to the step; the registry adds the step ID and
/// produces a structured [`MigrationError`].
pub type MigrationTransform =
    dyn Fn(&[u8]) -> Result<MigrationOutput, String> + Send + Sync + 'static;

/// The value returned by one migration step.
#[derive(Clone, Eq, PartialEq)]
pub struct MigrationOutput {
    /// Version represented by `document` after this step.
    pub version: u32,
    /// Transformed raw configuration document.
    pub document: Vec<u8>,
    /// Optional human-readable description of the change.
    pub summary: Option<String>,
    /// Non-fatal warnings that must remain visible to the caller.
    pub warnings: Vec<String>,
}

impl fmt::Debug for MigrationOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MigrationOutput")
            .field("version", &self.version)
            .field("document_len", &self.document.len())
            .field("summary", &self.summary)
            .field("warning_count", &self.warnings.len())
            .finish()
    }
}

impl MigrationOutput {
    pub fn new(version: u32, document: Vec<u8>) -> Self {
        Self {
            version,
            document,
            summary: None,
            warnings: Vec::new(),
        }
    }

    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    pub fn with_warning(mut self, warning: impl Into<String>) -> Self {
        self.warnings.push(warning.into());
        self
    }
}

/// One explicitly versioned migration edge.
pub struct MigrationStep {
    pub from: u32,
    pub to: u32,
    pub id: String,
    pub transform: Arc<MigrationTransform>,
}

impl MigrationStep {
    pub fn new<F>(from: u32, to: u32, id: impl Into<String>, transform: F) -> Self
    where
        F: Fn(&[u8]) -> Result<MigrationOutput, String> + Send + Sync + 'static,
    {
        Self {
            from,
            to,
            id: id.into(),
            transform: Arc::new(transform),
        }
    }

    pub fn from_fn(
        from: u32,
        to: u32,
        id: impl Into<String>,
        transform: fn(&[u8]) -> Result<MigrationOutput, String>,
    ) -> Self {
        Self::new(from, to, id, transform)
    }
}

impl fmt::Debug for MigrationStep {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MigrationStep")
            .field("from", &self.from)
            .field("to", &self.to)
            .field("id", &self.id)
            .field("transform", &"<pure transform>")
            .finish()
    }
}

/// Errors raised while validating or executing a migration chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationError {
    InvalidCurrentVersion {
        current: u32,
    },
    InvalidStep {
        id: String,
        from: u32,
        to: u32,
    },
    DuplicateStepId {
        id: String,
    },
    DuplicateTransition {
        from: u32,
        to: u32,
    },
    Fork {
        from: u32,
        first_to: u32,
        second_to: u32,
    },
    FutureVersion {
        input: u32,
        current: u32,
    },
    MissingStep {
        from: u32,
        target: u32,
    },
    TransformFailed {
        id: String,
        message: String,
    },
    ResultVersionMismatch {
        id: String,
        expected: u32,
        actual: u32,
    },
}

impl fmt::Display for MigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCurrentVersion { current } => {
                write!(formatter, "invalid current config version: {current}")
            }
            Self::InvalidStep { id, from, to } => {
                write!(formatter, "invalid migration step {id:?}: {from} -> {to}")
            }
            Self::DuplicateStepId { id } => {
                write!(formatter, "duplicate migration step id: {id:?}")
            }
            Self::DuplicateTransition { from, to } => {
                write!(formatter, "duplicate migration transition: {from} -> {to}")
            }
            Self::Fork {
                from,
                first_to,
                second_to,
            } => write!(
                formatter,
                "migration fork at version {from}: {first_to} and {second_to}"
            ),
            Self::FutureVersion { input, current } => write!(
                formatter,
                "config version {input} is newer than supported version {current}"
            ),
            Self::MissingStep { from, target } => {
                write!(
                    formatter,
                    "missing migration step from version {from} to {target}"
                )
            }
            Self::TransformFailed { id, message } => {
                write!(formatter, "migration step {id:?} failed: {message}")
            }
            Self::ResultVersionMismatch {
                id,
                expected,
                actual,
            } => write!(
                formatter,
                "migration step {id:?} returned version {actual}, expected {expected}"
            ),
        }
    }
}

impl Error for MigrationError {}

/// A read-only description of one migration execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationReport {
    pub from_version: u32,
    pub to_version: u32,
    pub step_ids: Vec<String>,
    pub summaries: Vec<String>,
    pub warnings: Vec<String>,
    pub input_hash: String,
    pub output_hash: String,
}

impl MigrationReport {
    /// Returns all step summaries in execution order.
    pub fn summary(&self) -> String {
        self.summaries.join("; ")
    }

    pub fn is_changed(&self) -> bool {
        self.input_hash != self.output_hash || self.from_version != self.to_version
    }
}

/// Migrated document together with its audit report.
#[derive(Clone, Eq, PartialEq)]
pub struct MigrationOutcome {
    pub document: Vec<u8>,
    pub report: MigrationReport,
}

impl fmt::Debug for MigrationOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MigrationOutcome")
            .field("document_len", &self.document.len())
            .field("report", &self.report)
            .finish()
    }
}

/// A validated collection of migration edges targeting one current schema version.
#[derive(Debug)]
pub struct MigrationRegistry {
    current_version: u32,
    steps: Vec<MigrationStep>,
}

impl MigrationRegistry {
    pub fn new(current_version: u32) -> Self {
        Self {
            current_version,
            steps: Vec::new(),
        }
    }

    pub fn current() -> Self {
        Self::new(CURRENT_CONFIG_VERSION)
    }

    pub fn v1() -> Self {
        Self::current()
    }

    pub fn from_steps(current_version: u32, steps: Vec<MigrationStep>) -> Self {
        Self {
            current_version,
            steps,
        }
    }

    pub fn current_version(&self) -> u32 {
        self.current_version
    }

    pub fn steps(&self) -> &[MigrationStep] {
        &self.steps
    }

    /// Adds a step and leaves the registry unchanged if validation fails.
    pub fn register(&mut self, step: MigrationStep) -> Result<(), MigrationError> {
        self.steps.push(step);
        if let Err(error) = self.validate() {
            self.steps.pop();
            return Err(error);
        }
        Ok(())
    }

    /// Checks all structural invariants without executing transforms.
    pub fn validate(&self) -> Result<(), MigrationError> {
        if self.current_version == 0 {
            return Err(MigrationError::InvalidCurrentVersion {
                current: self.current_version,
            });
        }

        for (index, step) in self.steps.iter().enumerate() {
            if step.id.trim().is_empty() || step.from >= step.to || step.to > self.current_version {
                return Err(MigrationError::InvalidStep {
                    id: step.id.clone(),
                    from: step.from,
                    to: step.to,
                });
            }

            for previous in &self.steps[..index] {
                if previous.id == step.id {
                    return Err(MigrationError::DuplicateStepId {
                        id: step.id.clone(),
                    });
                }
                if previous.from == step.from {
                    if previous.to == step.to {
                        return Err(MigrationError::DuplicateTransition {
                            from: step.from,
                            to: step.to,
                        });
                    }
                    return Err(MigrationError::Fork {
                        from: step.from,
                        first_to: previous.to,
                        second_to: step.to,
                    });
                }
            }
        }

        Ok(())
    }

    /// Applies every edge from `input_version` to this registry's current version.
    pub fn migrate(
        &self,
        input_version: u32,
        input: &[u8],
    ) -> Result<MigrationOutcome, MigrationError> {
        self.validate()?;

        if input_version > self.current_version {
            return Err(MigrationError::FutureVersion {
                input: input_version,
                current: self.current_version,
            });
        }

        let input_hash = deterministic_hash(input);
        let mut version = input_version;
        let mut document = input.to_vec();
        let mut step_ids = Vec::new();
        let mut summaries = Vec::new();
        let mut warnings = Vec::new();

        while version < self.current_version {
            let step = self
                .steps
                .iter()
                .find(|candidate| candidate.from == version)
                .ok_or(MigrationError::MissingStep {
                    from: version,
                    target: self.current_version,
                })?;

            let output =
                (step.transform)(&document).map_err(|message| MigrationError::TransformFailed {
                    id: step.id.clone(),
                    message,
                })?;

            if output.version != step.to {
                return Err(MigrationError::ResultVersionMismatch {
                    id: step.id.clone(),
                    expected: step.to,
                    actual: output.version,
                });
            }

            version = output.version;
            document = output.document;
            step_ids.push(step.id.clone());
            if let Some(summary) = output.summary
                && !summary.is_empty()
            {
                summaries.push(summary);
            }
            warnings.extend(output.warnings);
        }

        Ok(MigrationOutcome {
            document: document.clone(),
            report: MigrationReport {
                from_version: input_version,
                to_version: version,
                step_ids,
                summaries,
                warnings,
                input_hash,
                output_hash: deterministic_hash(&document),
            },
        })
    }
}

/// Returns the empty migration registry for the current v1 schema.
pub fn current_registry() -> MigrationRegistry {
    MigrationRegistry::current()
}

/// Stable, dependency-free FNV-1a hash rendered as lowercase hexadecimal.
pub fn deterministic_hash(input: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in input {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3_u64);
    }
    format!("{hash:016x}")
}

pub fn stable_hash(input: &[u8]) -> String {
    deterministic_hash(input)
}

#[cfg(test)]
mod tests {
    use super::{
        CURRENT_CONFIG_VERSION, MigrationError, MigrationOutput, MigrationRegistry, MigrationStep,
        current_registry, deterministic_hash,
    };

    #[test]
    fn current_v1_empty_chain_is_identity() {
        let input = b"version: 1\n";
        let outcome = current_registry()
            .migrate(CURRENT_CONFIG_VERSION, input)
            .unwrap();

        assert_eq!(outcome.document, input);
        assert_eq!(outcome.report.from_version, 1);
        assert_eq!(outcome.report.to_version, 1);
        assert!(outcome.report.step_ids.is_empty());
        assert!(outcome.report.summaries.is_empty());
        assert!(outcome.report.warnings.is_empty());
        assert_eq!(outcome.report.input_hash, outcome.report.output_hash);
        assert!(!outcome.report.is_changed());
    }

    #[test]
    fn future_versions_are_rejected() {
        let error = current_registry().migrate(2, b"version: 2").unwrap_err();
        assert_eq!(
            error,
            MigrationError::FutureVersion {
                input: 2,
                current: 1,
            }
        );
    }

    #[test]
    fn missing_step_is_rejected() {
        let registry = MigrationRegistry::new(3);
        let error = registry.migrate(1, b"old").unwrap_err();
        assert_eq!(error, MigrationError::MissingStep { from: 1, target: 3 });
    }

    #[test]
    fn duplicate_and_forked_edges_are_rejected() {
        let duplicate = MigrationRegistry::from_steps(
            2,
            vec![
                MigrationStep::new(1, 2, "v1-to-v2", |_| {
                    Ok(MigrationOutput::new(2, Vec::new()))
                }),
                MigrationStep::new(1, 2, "v1-to-v2-again", |_| {
                    Ok(MigrationOutput::new(2, Vec::new()))
                }),
            ],
        );
        assert_eq!(
            duplicate.validate().unwrap_err(),
            MigrationError::DuplicateTransition { from: 1, to: 2 }
        );

        let duplicate_id = MigrationRegistry::from_steps(
            3,
            vec![
                MigrationStep::new(1, 2, "same-id", |_| Ok(MigrationOutput::new(2, Vec::new()))),
                MigrationStep::new(2, 3, "same-id", |_| Ok(MigrationOutput::new(3, Vec::new()))),
            ],
        );
        assert_eq!(
            duplicate_id.validate().unwrap_err(),
            MigrationError::DuplicateStepId {
                id: "same-id".to_owned(),
            }
        );

        let fork = MigrationRegistry::from_steps(
            3,
            vec![
                MigrationStep::new(1, 2, "v1-to-v2", |_| {
                    Ok(MigrationOutput::new(2, Vec::new()))
                }),
                MigrationStep::new(1, 3, "v1-to-v3", |_| {
                    Ok(MigrationOutput::new(3, Vec::new()))
                }),
            ],
        );
        assert_eq!(
            fork.validate().unwrap_err(),
            MigrationError::Fork {
                from: 1,
                first_to: 2,
                second_to: 3,
            }
        );
    }

    #[test]
    fn reports_are_stable_and_preserve_step_order() {
        let registry = MigrationRegistry::from_steps(
            3,
            vec![
                MigrationStep::new(1, 2, "rename-field", |input| {
                    let mut document = input.to_vec();
                    document.extend_from_slice(b"-v2");
                    Ok(MigrationOutput::new(2, document).with_summary("rename field"))
                }),
                MigrationStep::new(2, 3, "add-default", |input| {
                    let mut document = input.to_vec();
                    document.extend_from_slice(b"-v3");
                    Ok(MigrationOutput::new(3, document)
                        .with_summary("add default")
                        .with_warning("default was inserted"))
                }),
            ],
        );

        let first = registry.migrate(1, b"input").unwrap();
        let second = registry.migrate(1, b"input").unwrap();

        assert_eq!(first, second);
        assert_eq!(first.report.step_ids, ["rename-field", "add-default"]);
        assert_eq!(first.report.summaries, ["rename field", "add default"]);
        assert_eq!(first.report.warnings, ["default was inserted"]);
        assert_eq!(first.report.summary(), "rename field; add default");
        assert!(first.report.is_changed());
        assert_eq!(deterministic_hash(b"input"), first.report.input_hash);
    }
}
