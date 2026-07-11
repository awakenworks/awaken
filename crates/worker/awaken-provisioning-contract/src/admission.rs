//! Admission: the `EnvironmentSoundness` control-plane check (ported from the
//! oversight `G45` invariant). A declared [`super::EnvironmentKind`] is admitted
//! only when it is well-formed, so a broken config is rejected at publish time,
//! not at provision time.
//!
//! This works on a small `EnvironmentDecl` fact (not the full config type) so the
//! rule stays independent of how the config is stored; the control plane projects
//! its config into a decl and calls [`check_environment_soundness`].

use crate::vocab::RESERVED_ENV_KEYS;

/// The admission facts extracted from a declared environment config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentDecl {
    /// Operator-facing one-line summary (must be non-empty).
    pub summary: String,
    /// Stable kind name, for diagnostics.
    pub kind: String,
    /// A kind-specific required field: `(field_name, value)`. `Some` with an empty
    /// value is a violation (e.g. `Image` with a blank `reference`).
    pub required_field: Option<(&'static str, String)>,
    /// Whether the environment's base filesystem is writable.
    pub writable_base: bool,
    /// Declared pool capacity, if any.
    pub max_concurrency: Option<u32>,
    /// Env var names the declaration wants to set.
    pub env_keys: Vec<String>,
}

/// Why an environment declaration was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AdmissionError {
    #[error("environment summary must not be empty")]
    EmptySummary,
    #[error("required field {field:?} for kind {kind:?} must not be empty")]
    MissingRequiredField { kind: String, field: &'static str },
    #[error("env key {0:?} is reserved by the runtime and cannot be set")]
    ReservedEnvKey(String),
    #[error("writable_base requires max_concurrency <= 1 (got {0:?})")]
    WritableBaseConcurrency(Option<u32>),
}

/// Check `EnvironmentSoundness` (G45). Fail-closed: any violation rejects.
///
/// - `summary` non-empty;
/// - a declared `required_field` is non-empty;
/// - no `env_keys` collide with [`RESERVED_ENV_KEYS`];
/// - `writable_base` implies `max_concurrency <= 1` (no concurrent RWX base).
pub fn check_environment_soundness(decl: &EnvironmentDecl) -> Result<(), AdmissionError> {
    if decl.summary.trim().is_empty() {
        return Err(AdmissionError::EmptySummary);
    }
    if let Some((field, value)) = &decl.required_field
        && value.trim().is_empty()
    {
        return Err(AdmissionError::MissingRequiredField {
            kind: decl.kind.clone(),
            field,
        });
    }
    for key in &decl.env_keys {
        if RESERVED_ENV_KEYS.contains(&key.as_str()) {
            return Err(AdmissionError::ReservedEnvKey(key.clone()));
        }
    }
    if decl.writable_base && decl.max_concurrency.is_some_and(|n| n > 1) {
        return Err(AdmissionError::WritableBaseConcurrency(
            decl.max_concurrency,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sound() -> EnvironmentDecl {
        EnvironmentDecl {
            summary: "a coding sandbox".into(),
            kind: "sandbox".into(),
            required_field: None,
            writable_base: false,
            max_concurrency: Some(4),
            env_keys: vec!["TZ".into(), "NODE_ENV".into()],
        }
    }

    #[test]
    fn accepts_a_sound_declaration() {
        assert!(check_environment_soundness(&sound()).is_ok());
    }

    #[test]
    fn rejects_empty_summary() {
        let mut d = sound();
        d.summary = "   ".into();
        assert_eq!(
            check_environment_soundness(&d),
            Err(AdmissionError::EmptySummary)
        );
    }

    #[test]
    fn rejects_blank_required_field() {
        let mut d = sound();
        d.kind = "image".into();
        d.required_field = Some(("reference", String::new()));
        assert_eq!(
            check_environment_soundness(&d),
            Err(AdmissionError::MissingRequiredField {
                kind: "image".into(),
                field: "reference"
            })
        );
    }

    #[test]
    fn rejects_reserved_env_key() {
        let mut d = sound();
        d.env_keys = vec!["PATH".into()];
        assert_eq!(
            check_environment_soundness(&d),
            Err(AdmissionError::ReservedEnvKey("PATH".into()))
        );
    }

    #[test]
    fn writable_base_forbids_concurrency_above_one() {
        let mut d = sound();
        d.writable_base = true;
        d.max_concurrency = Some(2);
        assert_eq!(
            check_environment_soundness(&d),
            Err(AdmissionError::WritableBaseConcurrency(Some(2)))
        );
        d.max_concurrency = Some(1);
        assert!(check_environment_soundness(&d).is_ok());
    }
}
