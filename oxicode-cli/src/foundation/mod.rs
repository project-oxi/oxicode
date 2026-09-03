//! Oxi Foundation v1 host primitives for oxicode.
//!
//! Reads the versioned contract under `~/.oxi/foundation/v1/`. The
//! contract is the only interface across the host boundary: oxicode
//! does not import from `oxibrain` or `oxios` directly.
//!
//! # Layout
//!
//! ```text
//! ~/.oxi/foundation/v1/
//! ├── foundation.json
//! ├── profiles.json
//! ├── packages.lock
//! └── packages/<sha256>/
//! ```
//!
//! Override the root with `$OXI_FOUNDATION_HOME`. The contract is
//! documented in
//! `docs/superpowers/specs/2026-08-17-oxi-foundation-contract.md`.
//!
//! # Modules
//!
//! - [`compatibility`] — typed `foundation.json` parsing + schema /
//!   host-version negotiation.
//! - [`profiles`] — typed `profiles.json` parsing, role resolution, and
//!   the pure [`resolve_profile`](profiles::resolve_profile) decision
//!   function.
//! - [`packages`] — typed `packages.lock` parsing, digest verification,
//!   and capability mapping to oxicode's existing policy.
//! - [`credentials`] — Keychain-backed credential resolver (see
//!   `credentials.rs`).
//! - [`compat_import`] — one-time legacy compatibility import (gated by
//!   `OXICODE_FOUNDATION_MIGRATION=1`).
//! - [`fixtures`] — helpers that load the shared cross-host JSON
//!   fixtures from `tests/fixtures/oxi-foundation/v1/`.
//!
//! All errors are re-typeset through [`FoundationError`]; the
//! `Display`/`Debug` impls never expose a secret value.
//! - [`brain`] — BrainMemoryBackend, the only durable-memory authority
//!   under the Foundation host. Talks to `oxibrain` over a Unix-domain
//!   socket; surfaces `degraded` state on connection failure.

pub mod brain;
pub mod brain_control;
/// `workspace_search` backend — `BrainWorkspaceSearch` over the oxibrain
/// document plane (daemonless stdio child).
pub mod brain_workspace;
pub mod compat_import;
pub mod compatibility;
pub mod credentials;
pub mod fixtures;
pub mod packages;
pub mod profiles;

pub mod migrate;

use std::path::{Path, PathBuf};

/// Resolve the oxicode home directory through the unified Oxi home layout
/// (`$OXICODE_HOME`, else `<oxi_home>/oxicode`, else `~/.oxi/oxicode`).
/// The Foundation host is independent: `OXICODE_HOME`/`OXI_HOME` only
/// affect oxicode-local paths (legacy memory, migration checkpoints, etc.)
/// and do not change the Foundation root.
pub fn fetch_oxicode_home() -> Option<PathBuf> {
    oxicode_catalog::oxi_home::oxicode_home()
}

/// Canonical subdirectory name under the host's `$HOME` (or
/// `$OXI_FOUNDATION_HOME`).
pub const FOUNDATION_ROOT_SUFFIX: &str = "oxi/foundation/v1";

/// Filenames the contract requires at the foundation root.
pub mod files {
    /// `foundation.json` — schema version + host compatibility.
    pub const FOUNDATION: &str = "foundation.json";
    /// `profiles.json` — non-secret provider/model profiles.
    pub const PROFILES: &str = "profiles.json";
    /// `packages.lock` — immutable resolved package records.
    pub const PACKAGES_LOCK: &str = "packages.lock";
    /// `packages/<sha256>/` — verified immutable package content.
    pub const PACKAGES_DIR: &str = "packages";
}

/// Resolve the foundation root. Honors `$OXI_FOUNDATION_HOME`; falls
/// back to `$HOME/.oxi/foundation/v1`. Never reads secrets from this
/// path.
pub fn foundation_root() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("OXI_FOUNDATION_HOME") {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    dirs::home_dir().map(|h| h.join(FOUNDATION_ROOT_SUFFIX))
}

/// `true` when the foundation installation is present and looks
/// well-formed enough to attempt parsing. Reads only metadata; does
/// not validate schemas.
pub fn foundation_present(root: &Path) -> bool {
    root.is_dir() && root.join(files::FOUNDATION).is_file() && root.join(files::PROFILES).is_file()
}

/// Full filesystem discovery — parses `foundation.json`, `profiles.json`,
/// and `packages.lock` (when present). Returns a typed snapshot or a
/// typed error. All reads are best-effort for the lockfile: the
/// foundation is usable without installed packages.
pub fn discover(root: &Path) -> Result<FoundationSnapshot, FoundationError> {
    let compatibility = compatibility::read(&root.join(files::FOUNDATION))?;
    let profiles = profiles::read(&root.join(files::PROFILES))?;
    let packages = packages::read(
        &root.join(files::PACKAGES_LOCK),
        &root.join(files::PACKAGES_DIR),
    )?;
    Ok(FoundationSnapshot {
        root: root.to_path_buf(),
        compatibility,
        profiles,
        packages,
    })
}

/// In-memory snapshot of the foundation installation. Cheap to clone —
/// all fields are Arc-friendly.
#[derive(Debug, Clone)]
pub struct FoundationSnapshot {
    /// Root directory the snapshot was loaded from.
    pub root: PathBuf,
    /// `foundation.json` content.
    pub compatibility: compatibility::FoundationManifest,
    /// `profiles.json` content.
    pub profiles: profiles::ProfilesFile,
    /// `packages.lock` content (verified; `OK` on load).
    pub packages: packages::PackagesFile,
}

/// Error type for every foundation operation. Carries no secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoundationError {
    /// `schema_version` is not `1` or is missing.
    UnsupportedSchema(u32),
    /// `host_compatibility.oxicode` does not include this build.
    IncompatibleHost(String),
    /// The file is malformed JSON or missing required fields.
    Parse(String),
    /// A profile contains a known secret-shaped field.
    SecretNotAllowed(String),
    /// Two profiles share the same id.
    DuplicateProfileId(String),
    /// A package requires an unknown abstract capability.
    UnsupportedRequirement(String),
    /// A package's on-disk content does not match its declared digest.
    DigestMismatch {
        package: String,
        expected: String,
        actual: String,
    },
    /// The package's `targets` list does not include `oxicode`.
    TargetMismatch {
        package: String,
        targets: Vec<String>,
    },
    /// The explicit profile id does not match any record.
    UnknownProfile(String),
    /// The requested role matched zero profiles.
    UnknownRole(String),
    /// The requested role matched more than one profile.
    AmbiguousRole(String),
    /// Keychain is unreachable (no keychain daemon, etc.).
    KeychainUnavailable(String),
    /// Keychain prompt was cancelled by the user.
    KeychainLocked(String),
    /// The credential locator has no entry.
    KeychainNotFound { service: String, account: String },
    /// Brain daemon is unreachable.
    BrainUnavailable(String),
    /// I/O failures.
    Io(String),
}

impl std::fmt::Display for FoundationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never include a credential value, account name that looks like
        // a secret, or any token. Account names are accepted because
        // they are public profile ids.
        match self {
            Self::UnsupportedSchema(v) => write!(f, "unsupported foundation schema_version {v}"),
            Self::IncompatibleHost(s) => write!(f, "host compatibility check failed: {s}"),
            Self::Parse(s) => write!(f, "foundation parse error: {s}"),
            Self::SecretNotAllowed(s) => write!(f, "secret not allowed in foundation file: {s}"),
            Self::DuplicateProfileId(id) => write!(f, "duplicate profile id: {id}"),
            Self::UnsupportedRequirement(req) => {
                write!(f, "unsupported package requirement: {req}")
            }
            Self::DigestMismatch {
                package,
                expected,
                actual,
            } => write!(
                f,
                "package {package} digest mismatch: expected {expected}, got {actual}"
            ),
            Self::TargetMismatch { package, targets } => write!(
                f,
                "package {package} targets do not include `oxicode`: {targets:?}"
            ),
            Self::UnknownProfile(id) => write!(f, "unknown profile id: {id}"),
            Self::UnknownRole(r) => write!(f, "no profile matches requested role: {r}"),
            Self::AmbiguousRole(r) => write!(f, "multiple profiles match role {r}"),
            Self::KeychainUnavailable(s) => write!(f, "keychain unavailable: {s}"),
            Self::KeychainLocked(s) => write!(f, "keychain locked: {s}"),
            Self::KeychainNotFound { service, account } => {
                write!(f, "keychain entry not found for {service}:{account}")
            }
            Self::BrainUnavailable(s) => write!(f, "brain daemon unavailable: {s}"),
            Self::Io(s) => write!(f, "foundation I/O error: {s}"),
        }
    }
}

impl std::error::Error for FoundationError {}

impl From<std::io::Error> for FoundationError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<serde_json::Error> for FoundationError {
    fn from(e: serde_json::Error) -> Self {
        Self::Parse(e.to_string())
    }
}

/// Source of the resolved provider/model. Used in logs and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// `$OXICODE_PROVIDER` / `$OXICODE_MODEL` — non-persistent.
    Environment,
    /// Profile selected via `--profile` / `OXICODE_PROFILE`.
    Profile,
    /// Role-compatible profile resolution.
    Role,
    /// One-time legacy compatibility import.
    CompatibilityImport,
    /// No provider resolved.
    Unavailable,
}

impl std::fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Environment => f.write_str("environment"),
            Self::Profile => f.write_str("profile"),
            Self::Role => f.write_str("role"),
            Self::CompatibilityImport => f.write_str("compatibility_import"),
            Self::Unavailable => f.write_str("unavailable"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foundation_root_honors_env_override() {
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: we set the env var only for this test; the test runner
        // is single-threaded for unit tests by default.
        // SAFETY: tests run on a single thread when using #[test] + cargo
        // test by default; rust 2024 still warns about process-wide env.
        // We use a scoped approach instead.
        let original = std::env::var("OXI_FOUNDATION_HOME").ok();
        unsafe {
            std::env::set_var("OXI_FOUNDATION_HOME", tmp.path());
        }
        let root = foundation_root().unwrap();
        unsafe {
            std::env::remove_var("OXI_FOUNDATION_HOME");
        }
        if let Some(value) = original {
            unsafe {
                std::env::set_var("OXI_FOUNDATION_HOME", value);
            }
        }
        assert_eq!(root, tmp.path());
    }

    #[test]
    fn foundation_present_detects_layout() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!foundation_present(tmp.path()));
        std::fs::write(tmp.path().join(files::FOUNDATION), "{}").unwrap();
        std::fs::write(tmp.path().join(files::PROFILES), "{}").unwrap();
        assert!(foundation_present(tmp.path()));
    }

    #[test]
    fn credential_source_display_roundtrip() {
        assert_eq!(CredentialSource::Environment.to_string(), "environment");
        assert_eq!(CredentialSource::Profile.to_string(), "profile");
        assert_eq!(CredentialSource::Role.to_string(), "role");
        assert_eq!(
            CredentialSource::CompatibilityImport.to_string(),
            "compatibility_import"
        );
        assert_eq!(CredentialSource::Unavailable.to_string(), "unavailable");
    }
}
