//! `BrainWorkspaceSearch` — the `workspace_search` backend over the local
//! oxibrain daemon's document plane.
//!
//! Daemonless transport (two-plane design): the backend lazily spawns a
//! caller-owned `oxibrain admin serve --stdio --dir <brain-dir>` child via
//! [`oxibrain_client_brain::LocalProcessEndpoint`] (`kill_on_drop`), idempotently
//! registers the canonical workspace root into the configured brain space,
//! then searches the documents plane (hybrid mode). Spaces have no root
//! filter, so the backend over-fetches and post-filters hits by the root
//! alias. The legacy 0.2 memory transport (`oxibrain-client`) is untouched.

use crate::store::settings::{Settings, WorkspaceSearchSettings};
use oxicode_agent::tools::path_security::PathGuard;
use oxicode_agent::tools::{SearchPassage, ToolError, WorkspaceSearchBackend, WorkspaceSearchPage};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set iff `create_workspace_search_backend` constructed a backend for this
/// process and bootstrap registered the `workspace_search` tool on the live
/// tool registry (see `bootstrap::build_app`). This is a boot-time fact:
/// construction happens once per process, so prompt gating can rely on it
/// for the process lifetime. Defaults to `false`, which is the honest
/// answer for callers that never run bootstrap (tests, embedding hosts) —
/// the routing fragment must not name a tool that was never registered.
static WORKSPACE_SEARCH_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Whether the `workspace_search` tool was registered for this process.
/// The single gate (together with the settings flag) behind
/// `workspace_search_guidance`, so the initial session prompt and every
/// hot-apply rebuild derive the fragment from the same condition that
/// registered the tool.
pub(crate) fn workspace_search_registered() -> bool {
    WORKSPACE_SEARCH_REGISTERED.load(Ordering::Relaxed)
}

/// Record that the `workspace_search` tool was registered. Called by
/// bootstrap immediately after registering the tool.
pub(crate) fn mark_workspace_search_registered() {
    WORKSPACE_SEARCH_REGISTERED.store(true, Ordering::Relaxed);
}

/// Code-oriented include globs used when settings leave `include` unset.
const DEFAULT_INCLUDE: &[&str] = &[
    "**/*.rs",
    "**/*.ts",
    "**/*.tsx",
    "**/*.js",
    "**/*.jsx",
    "**/*.py",
    "**/*.go",
    "**/*.java",
    "**/*.kt",
    "**/*.swift",
    "**/*.c",
    "**/*.h",
    "**/*.cpp",
    "**/*.hpp",
    "**/*.md",
    "**/*.toml",
    "**/*.yaml",
    "**/*.yml",
    "**/*.json",
];

/// Code-oriented exclude globs used when settings leave `exclude` unset.
const DEFAULT_EXCLUDE: &[&str] = &[
    "**/target/**",
    "**/node_modules/**",
    "**/dist/**",
    "**/build/**",
    "**/__pycache__/**",
    "**/.git/**",
    "**/.venv/**",
    "**/venv/**",
];

/// Deterministic, collision-safe document-root alias for a workspace:
/// `ws-<dirname>-<hash8>` where the hash pins the canonical path, so two
/// same-named worktrees never collide and re-registrations stay idempotent.
fn root_alias(workspace_root: &Path) -> String {
    let name = workspace_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());
    let canonical = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let mut h = DefaultHasher::new();
    canonical.hash(&mut h);
    format!("ws-{}-{:08x}", name, h.finish() as u32)
}

/// Resolve the oxibrain binary: settings override → `$PATH` scan →
/// `~/.oxi/bin/oxibrain`. A configured override is used as-is (a missing
/// binary surfaces at spawn with the path in the message); the PATH scan
/// and home fallback require an existing file.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|c| c.is_file())
}

fn resolve_executable(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(exe) = explicit {
        return Some(PathBuf::from(exe));
    }
    if let Some(found) = find_on_path("oxibrain") {
        return Some(found);
    }
    let home = dirs::home_dir()?;
    let candidate = home.join(".oxi").join("bin").join("oxibrain");
    candidate.is_file().then_some(candidate)
}

/// The brain store directory the child serves: `$OXI_BRAIN_DIR`, else
/// `~/.oxi/brain`. Passed as `--dir` so the child never touches an ambient
/// store (two-plane invariant).
fn brain_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("OXI_BRAIN_DIR") {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|h| h.join(".oxi").join("brain"))
}

fn default_globs(globs: &[&str]) -> Vec<String> {
    globs.iter().map(|g| (*g).to_string()).collect()
}

/// Lazily-initialized client + registration state, guarded by the backend's
/// async mutex so concurrent tool calls serialize on setup.
#[derive(Debug)]
struct BrainState {
    client: Option<oxibrain_client_brain::BrainClient>,
    registered: bool,
    space_ok: bool,
}

/// oxibrain document-plane backend for the `workspace_search` tool.
#[derive(Debug)]
pub struct BrainWorkspaceSearch {
    root: PathBuf, // canonical workspace
    alias: String,
    endpoint_executable: Option<String>,
    brain_dir: PathBuf,
    space: String,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    max_file_bytes: Option<u64>,
    state: tokio::sync::Mutex<BrainState>,
}

impl BrainWorkspaceSearch {
    /// Construct the backend for a workspace root. Cheap and side-effect
    /// free: the child spawn, space check, and root registration are all
    /// deferred to the first search (resolution failure surfaces there as
    /// a typed unavailable error, never at construction).
    pub fn new(
        workspace_root: PathBuf,
        settings: &WorkspaceSearchSettings,
    ) -> Result<Self, String> {
        let root = workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.clone());
        let brain_dir = brain_dir().ok_or_else(|| {
            "workspace_search is unavailable: cannot resolve the brain store \
             directory (set OXI_BRAIN_DIR). Use grep for exact search and \
             read to inspect files."
                .to_string()
        })?;
        Ok(Self {
            alias: root_alias(&root),
            endpoint_executable: settings.executable.clone(),
            brain_dir,
            space: settings.space.clone(),
            // Code-oriented defaults when settings leave the rules unset.
            include: Some(
                settings
                    .include
                    .clone()
                    .unwrap_or_else(|| default_globs(DEFAULT_INCLUDE)),
            ),
            exclude: Some(
                settings
                    .exclude
                    .clone()
                    .unwrap_or_else(|| default_globs(DEFAULT_EXCLUDE)),
            ),
            max_file_bytes: settings.max_file_bytes,
            root,
            state: tokio::sync::Mutex::new(BrainState {
                client: None,
                registered: false,
                space_ok: false,
            }),
        })
    }

    /// Spawn the child (once), verify the space exists, and idempotently
    /// register the workspace root. Called under the state mutex.
    async fn ensure_ready(&self, st: &mut BrainState) -> Result<(), String> {
        if st.registered && st.space_ok && st.client.is_some() {
            return Ok(());
        }
        let exe = resolve_executable(self.endpoint_executable.as_deref()).ok_or_else(|| {
            "workspace_search is unavailable: the oxibrain executable was not found \
             (set workspace_search.executable in settings). Use grep for exact search \
             and read to inspect files."
                .to_string()
        })?;
        if st.client.is_none() {
            let endpoint =
                oxibrain_client_brain::LocalProcessEndpoint::new(exe, self.brain_dir.clone());
            let client = oxibrain_client_brain::BrainClient::spawn_local(endpoint)
                .await
                .map_err(|e| {
                    format!(
                        "workspace_search is unavailable: cannot spawn the \
                         oxibrain child ({e}). Use grep/read."
                    )
                })?;
            st.client = Some(client);
        }
        if !st.space_ok {
            let client = st.client.as_mut().unwrap();
            let spaces = client
                .list_spaces()
                .await
                .map_err(|e| format!("workspace_search is unavailable: {e}. Use grep/read."))?;
            if !spaces
                .iter()
                .any(|s| s.id == self.space || s.name == self.space)
            {
                return Err(format!(
                    "workspace_search is unavailable: brain space '{}' does not exist \
                     (create it with oxibrain tooling). Use grep/read.",
                    self.space
                ));
            }
            st.space_ok = true;
        }
        if !st.registered {
            let client = st.client.as_mut().unwrap();
            client
                .register_document_root(oxibrain_client_brain::RegisterDocumentRootRequest {
                    space: self.space.clone(),
                    alias: self.alias.clone(),
                    path: self.root.to_string_lossy().to_string(),
                    include: self.include.clone(),
                    exclude: self.exclude.clone(),
                    max_file_bytes: self.max_file_bytes,
                })
                .await
                .map_err(|e| {
                    format!(
                        "workspace_search is unavailable: root registration failed: \
                         {e}. Use grep/read."
                    )
                })?;
            st.registered = true;
        }
        Ok(())
    }
}

impl WorkspaceSearchBackend for BrainWorkspaceSearch {
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Result<WorkspaceSearchPage, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let mut st = self.state.lock().await;
            self.ensure_ready(&mut st).await?;
            let client = st.client.as_mut().unwrap();
            let fetch = (limit * 3).clamp(12, 48);
            let resp: oxibrain_client_brain::SearchResponseDto = client
                .search_planes(query, &self.space, "hybrid", fetch, &["documents"])
                .await
                .map_err(|e| format!("workspace_search failed: {e}. Use grep/read."))?;
            let passages = resp
                .documents
                .iter()
                .filter(|h| h.root == self.alias)
                .take(limit)
                .map(|h| SearchPassage {
                    locator: h.locator.clone(),
                    ordinal: h.ordinal,
                    revision: h.revision.clone(),
                    score: h.score,
                    snippet: h.text.text.clone(),
                    modified_at_ms: h.modified_at_ms,
                })
                .collect();
            Ok(WorkspaceSearchPage {
                passages,
                freshness: Some(freshness_line(&resp.freshness)),
            })
        })
    }
}

fn freshness_line(f: &oxibrain_client_brain::DocumentFreshnessDto) -> String {
    let mut parts = vec![format!("reconciled: {}", f.reconciled_roots.join(", "))];
    if !f.skipped_roots.is_empty() {
        parts.push(format!(
            "skipped: {}",
            f.skipped_roots
                .iter()
                .map(|(a, r)| format!("{a} ({r})"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(cov) = f.dense_coverage {
        parts.push(format!("dense coverage: {cov:.0}%"));
    }
    parts.join("; ")
}

/// Create the `workspace_search` backend when the user enabled it.
///
/// Mirrors [`crate::services::create_memory_backend`]: returns `None`
/// silently when `workspace_search.enabled` is false (the zero-cost
/// default-off contract — no spawn, no probe, no registration). With the
/// feature enabled, the workspace root is validated through the strict
/// [`PathGuard::validate`] boundary (D5) before any backend exists; a
/// construction failure logs a warning and returns `None`.
pub fn create_workspace_search_backend(
    settings: &Settings,
    workspace_root: &Path,
) -> Option<Arc<dyn WorkspaceSearchBackend>> {
    if !settings.workspace_search.enabled {
        return None;
    }
    let guard = PathGuard::new(workspace_root);
    let canonical = match guard.validate(workspace_root) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                "workspace_search enabled but the workspace root failed \
                 PathGuard validation: {e}"
            );
            return None;
        }
    };
    match BrainWorkspaceSearch::new(canonical, &settings.workspace_search) {
        Ok(backend) => Some(Arc::new(backend)),
        Err(e) => {
            tracing::warn!("workspace_search unavailable at construction: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::settings::Settings;

    #[test]
    fn alias_is_deterministic_and_name_bearing() {
        let a = root_alias(Path::new("/tmp/proj-x"));
        let b = root_alias(Path::new("/tmp/proj-x"));
        let c = root_alias(Path::new("/tmp/proj-y"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("ws-proj-x-"));
    }

    #[test]
    fn disabled_settings_yield_no_backend() {
        let mut s = Settings::default();
        assert!(create_workspace_search_backend(&s, Path::new("/tmp")).is_none());
        s.workspace_search.enabled = true;
        // construction succeeds even without the binary (resolution is lazy)
        assert!(create_workspace_search_backend(&s, Path::new("/tmp")).is_some());
    }
}
