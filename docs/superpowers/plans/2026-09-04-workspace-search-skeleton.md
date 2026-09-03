# Workspace Search Skeleton (oxibrain document plane) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the opt-in `workspace_search` agent tool skeleton backed by the local oxibrain daemon's document plane — backend-trait abstraction, CLI implementation over a daemonless stdio child, settings gate, conditional registration, prompt fragment.

**Architecture:** `oxicode-agent` defines `WorkspaceSearchBackend` (the `MemoryBackend` precedent: sync trait methods returning boxed futures) plus `WorkspaceSearchTool` with a stable two-parameter schema (`query`, `limit ≤ 20`). `oxicode-cli` implements `BrainWorkspaceSearch` over `oxibrain-client` (renamed dep `oxibrain-client-brain = { package = "oxibrain-client", version = "0.12.1" }`): lazily spawns `<oxibrain> admin serve --stdio --dir <brain-dir>` (`kill_on_drop`), idempotently registers the workspace root, searches the documents plane with hybrid mode, over-fetches and post-filters by root alias (spaces have no root filter), and renders an Oxicode-owned envelope. The legacy 0.2 memory wiring is untouched.

**Tech Stack:** `oxibrain-client 0.12.1` (crates.io; verified: `admin serve --stdio --dir` argv, `register_document_root`, `search_planes`, `DocumentHitDto`), `tokio`, existing settings/persona infrastructure.

**Spec:** `docs/designs/2026-09-03-workspace-retrieval-zvec-grep.md` (D3/D4/D5 as amended; user decisions: default off, `{query, limit}` schema only, over-fetch + alias post-filter, typed unavailable errors naming `grep`/`read`).

## Global Constraints

- `cargo fmt` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; tests via `cargo nextest run -p <crate>`.
- Default state is OFF: no spawn, no probe, no registration, no prompt fragment unless `[workspace_search] enabled = true`.
- Tool schema is exactly `{ "query": string (required), "limit": integer default 8, capped at 20 }`. No `root`/`include`/`freshness`/`vector` parameters.
- The backend never exposes anything but read-only search. No ingest/extraction/redaction/root-removal call sites.
- Snippets capped at 500 chars (same truncate helper semantics as grep); per-call work bounded by the agent loop's tool timeout.
- All unavailable states return typed errors naming `grep` and `read` as alternatives — never an empty success.
- Do not touch the memory transport (`oxibrain-client 0.2` socket path, `BrainMemoryBackend`, launchd revival, TUI brain chip).

---

### Task 1: Backend trait + `WorkspaceSearchTool` in oxicode-agent

**Files:**
- Create: `oxicode-agent/src/tools/workspace_search.rs`
- Modify: `oxicode-agent/src/tools.rs` (module decl + `pub use`)

**Interfaces:**
- Produces (consumed by Task 3/4/5):

```rust
pub struct SearchPassage {
    pub locator: String,   // workspace-root-relative file path
    pub ordinal: u32,      // chunk index within the file
    pub revision: String,  // index revision the chunk was built from
    pub score: f64,
    pub snippet: String,   // chunk text, pre-truncated by the renderer
    pub modified_at_ms: i64,
}

pub struct WorkspaceSearchPage {
    pub passages: Vec<SearchPassage>,
    pub freshness: Option<String>, // human-readable reconcile report line
}

pub trait WorkspaceSearchBackend: Send + Sync + std::fmt::Debug + 'static {
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Result<WorkspaceSearchPage, ToolError>> + Send + 'a>>;
}
```

- [ ] **Step 1: Write the failing tests** — create `workspace_search.rs` with the types above and `#[cfg(test)] mod tests` containing a fake backend and tool assertions:

```rust
//! `workspace_search` — optional indexed discovery tool.
//!
//! Stable two-parameter schema; retrieval is delegated to a
//! composition-root-supplied [`WorkspaceSearchBackend`] (oxibrain document
//! plane in the CLI). The tool owns policy: parameter caps, snippet bounds,
//! the envelope, and typed unavailable errors naming `grep`/`read`.

use super::{AgentTool, AgentToolResult, ToolContext, ToolError};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub const DEFAULT_LIMIT: usize = 8;
pub const MAX_LIMIT: usize = 20;
const MAX_SNIPPET_CHARS: usize = 500;

#[derive(Debug, Clone)]
pub struct SearchPassage {
    pub locator: String,
    pub ordinal: u32,
    pub revision: String,
    pub score: f64,
    pub snippet: String,
    pub modified_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct WorkspaceSearchPage {
    pub passages: Vec<SearchPassage>,
    pub freshness: Option<String>,
}

pub trait WorkspaceSearchBackend: Send + Sync + std::fmt::Debug + 'static {
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Result<WorkspaceSearchPage, ToolError>> + Send + 'a>>;
}

pub struct WorkspaceSearchTool {
    backend: Arc<dyn WorkspaceSearchBackend>,
}

impl WorkspaceSearchTool {
    pub fn new(backend: Arc<dyn WorkspaceSearchBackend>) -> Self {
        Self { backend }
    }
}

fn truncate_snippet(text: &str) -> String {
    if text.len() <= MAX_SNIPPET_CHARS {
        return text.to_string();
    }
    let mut end = MAX_SNIPPET_CHARS;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

fn render(page: &WorkspaceSearchPage) -> String {
    if page.passages.is_empty() {
        return "No relevant passages found. Use grep for exact search or read \
                to inspect files directly."
            .to_string();
    }
    let mut out = format!("Found {} ranked passages:\n", page.passages.len());
    for (i, p) in page.passages.iter().enumerate() {
        out.push_str(&format!(
            "\n{}. {}#{} (score {:.3}, rev {})\n   {}\n",
            i + 1,
            p.locator,
            p.ordinal,
            p.score,
            p.revision,
            truncate_snippet(&p.snippet)
        ));
    }
    if let Some(f) = &page.freshness {
        out.push_str(&format!("\nfreshness: {f}\n"));
    }
    out.push_str("\nVerify each passage with read or grep before relying on it.");
    out
}

#[async_trait]
impl AgentTool for WorkspaceSearchTool {
    fn name(&self) -> &str {
        "workspace_search"
    }

    fn label(&self) -> &str {
        "Workspace Search"
    }

    fn essential(&self) -> bool {
        false
    }

    fn description(&self) -> &str {
        "Semantic discovery over the workspace index: find relevant source \
         passages when the wording or location is unknown. Results are ranked \
         chunk-level evidence (file + excerpt), not exact matches. Use grep \
         for exact identifiers, paths, or regular expressions."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language query describing what to find"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum passages to return (default 8, max 20)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: Option<tokio::sync::oneshot::Receiver<()>>,
        _ctx: &ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter: query".to_string())?;
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v.min(MAX_LIMIT as u64) as usize)
            .unwrap_or(DEFAULT_LIMIT);

        let page = self.backend.search(query, limit).await?;
        Ok(AgentToolResult::success(render(&page)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug)]
    struct FakeBackend {
        page: Mutex<Option<WorkspaceSearchPage>>,
        seen_limit: Mutex<Vec<usize>>,
        fail: bool,
    }

    impl WorkspaceSearchBackend for FakeBackend {
        fn search<'a>(
            &'a self,
            query: &'a str,
            limit: usize,
        ) -> Pin<Box<dyn Future<Output = Result<WorkspaceSearchPage, ToolError>> + Send + 'a>>
        {
            Box::pin(async move {
                if self.fail {
                    return Err("workspace_search is unavailable: backend offline. \
                                Use grep for exact search and read to inspect files."
                        .to_string());
                }
                self.seen_limit.lock().unwrap().push(limit);
                let _ = query;
                let page = self.page.lock().unwrap().clone().unwrap_or(WorkspaceSearchPage {
                    passages: vec![],
                    freshness: None,
                });
                Ok(page)
            })
        }
    }

    fn page() -> WorkspaceSearchPage {
        WorkspaceSearchPage {
            passages: vec![SearchPassage {
                locator: "src/auth.rs".into(),
                ordinal: 3,
                revision: "abc".into(),
                score: 0.912,
                snippet: "ownership_session_id".repeat(80), // long
                modified_at_ms: 1,
            }],
            freshness: Some("reconciled: ws-oxi (12 files)".into()),
        }
    }

    #[tokio::test]
    async fn limit_is_capped_at_max_and_defaults_to_eight() {
        let backend = Arc::new(FakeBackend {
            page: Mutex::new(None),
            seen_limit: Mutex::new(vec![]),
            fail: false,
        });
        let tool = WorkspaceSearchTool::new(backend.clone());
        let ctx = ToolContext::new("/tmp");
        tool.execute("t", json!({"query": "x", "limit": 500}), None, &ctx)
            .await
            .unwrap();
        tool.execute("t", json!({"query": "x"}), None, &ctx)
            .await
            .unwrap();
        let seen = backend.seen_limit.lock().unwrap().clone();
        assert_eq!(seen, vec![20, 8], "cap at 20, default 8");
    }

    #[tokio::test]
    async fn empty_page_names_alternatives() {
        let tool = WorkspaceSearchTool::new(Arc::new(FakeBackend {
            page: Mutex::new(Some(WorkspaceSearchPage { passages: vec![], freshness: None })),
            seen_limit: Mutex::new(vec![]),
            fail: false,
        }));
        let ctx = ToolContext::new("/tmp");
        let out = tool.execute("t", json!({"query": "x"}), None, &ctx).await.unwrap();
        let text = out.text_or_empty();
        assert!(text.contains("Use grep for exact search"), "{text}");
    }

    #[tokio::test]
    async fn backend_failure_surfaces_typed_error() {
        let tool = WorkspaceSearchTool::new(Arc::new(FakeBackend {
            page: Mutex::new(None),
            seen_limit: Mutex::new(vec![]),
            fail: true,
        }));
        let ctx = ToolContext::new("/tmp");
        let out = tool.execute("t", json!({"query": "x"}), None, &ctx).await.unwrap();
        let text = out.text_or_empty();
        assert!(text.contains("unavailable") && text.contains("grep"), "{text}");
    }
}
```

Implementer notes: (a) check how neighboring tools read `AgentToolResult` text in `oxicode-agent` tests (`tools.rs` grep tests) and mirror that accessor instead of `text_or_empty()` — define a tiny local helper if none exists. (b) the `limit_is_capped_at_max` test must assert the fake recorded `seen_limit == [20]` for input 500 and `[8]` for a missing limit — write it that way directly. (c) keep `WorkspaceSearchTool` OUT of `with_builtins_cwd` — it is host-registered only.

- [ ] **Step 2: Wire module decl** — in `tools.rs`: `pub mod workspace_search;` next to `pub mod grep;` (or `mod workspace_search;` matching the crate's privacy convention for non-exported tools; the CLI imports via `oxicode_agent::tools::workspace_search::…`, so it must be `pub mod`) plus `pub use workspace_search::{SearchPassage, WorkspaceSearchBackend, WorkspaceSearchPage, WorkspaceSearchTool};` alongside the `GrepTool` re-export.

- [ ] **Step 3: Run** — `cargo nextest run -p oxicode-agent --lib tools::workspace_search`
Expected: ALL PASS (4 tests).

- [ ] **Step 4: Commit**

```bash
git add oxicode-agent/src/tools/workspace_search.rs oxicode-agent/src/tools.rs
git commit -m "feat(agent): workspace_search tool with backend abstraction and bounded envelope"
```

### Task 2: Settings — `[workspace_search]` section

**Files:**
- Modify: `oxicode-cli/src/store/settings.rs`

**Interfaces:**
- Produces: `settings.workspace_search: WorkspaceSearchSettings` consumed by Task 3/5.

```rust
/// oxibrain-backed workspace discovery (`workspace_search` tool).
/// Everything defaults OFF — no spawn, no registration, no prompt fragment
/// unless explicitly enabled (design D4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSearchSettings {
    /// Master switch. False by default.
    #[serde(default = "default_false")]
    pub enabled: bool,
    /// Path to the oxibrain binary. Default: probe PATH, then ~/.oxi/bin.
    #[serde(default)]
    pub executable: Option<String>,
    /// Brain space the workspace root registers into.
    #[serde(default = "default_workspace_search_space")]
    pub space: String,
    /// Include globs for the document root (code-oriented defaults when unset).
    #[serde(default)]
    pub include: Option<Vec<String>>,
    /// Exclude globs for the document root.
    #[serde(default)]
    pub exclude: Option<Vec<String>>,
    /// Per-file size cap for indexing.
    #[serde(default)]
    pub max_file_bytes: Option<u64>,
}

impl Default for WorkspaceSearchSettings { /* enabled: false, space: "dev", rest None */ }

fn default_workspace_search_space() -> String { "dev".to_string() }
```

And on `Settings`: `#[serde(default)] pub workspace_search: WorkspaceSearchSettings,` (place next to `memory_enabled`) plus the field in `Settings::default()` construction if that impl enumerates fields (check the existing `Default` impl — `memory_enabled: true` at settings.rs:468 shows it does).

- [ ] **Step 1: Write the failing test** (settings.rs tests module):

```rust
    #[test]
    fn workspace_search_defaults_to_disabled() {
        let s = Settings::default();
        assert!(!s.workspace_search.enabled);
        assert_eq!(s.workspace_search.space, "dev");
        assert!(s.workspace_search.executable.is_none());
    }

    #[test]
    fn workspace_search_parses_toml_section() {
        let raw = r#"
[workspace_search]
enabled = true
space = "dev"
executable = "/opt/oxibrain/bin/oxibrain"
"#;
        let s: Settings = toml::from_str(raw).unwrap();
        assert!(s.workspace_search.enabled);
        assert_eq!(s.workspace_search.executable.as_deref(), Some("/opt/oxibrain/bin/oxibrain"));
    }
```

(Mirror the existing TOML round-trip test style in the file — `toml` is already a dependency.)

- [ ] **Step 2: Run** — `cargo nextest run -p oxicode-cli --lib store::settings`
Expected: new tests FAIL, then implement, then PASS.

- [ ] **Step 3: Commit**

```bash
git add oxicode-cli/src/store/settings.rs
git commit -m "feat(cli): [workspace_search] settings section (default off)"
```

### Task 3: `BrainWorkspaceSearch` backend in oxicode-cli

**Files:**
- Modify: `oxicode-cli/Cargo.toml` (`[target.'cfg(unix)'.dependencies]`: add `oxibrain-client-brain = { package = "oxibrain-client", version = "0.12.1" }`; leave `oxibrain-client = "0.2"` untouched)
- Create: `oxicode-cli/src/foundation/brain_workspace.rs`
- Modify: `oxicode-cli/src/foundation/mod.rs` (module decl, `#[cfg(unix)]`-gated like brain.rs)

**Interfaces:**
- Consumes: `WorkspaceSearchBackend`, `SearchPassage`, `WorkspaceSearchPage` from Task 1; `WorkspaceSearchSettings` from Task 2; `oxibrain_client_brain::{BrainClient, LocalProcessEndpoint, RegisterDocumentRootRequest, SearchResponseDto}`.
- Produces:

```rust
pub struct BrainWorkspaceSearch { /* client+registration state behind tokio::sync::Mutex */ }

impl BrainWorkspaceSearch {
    pub fn new(workspace_root: PathBuf, settings: &WorkspaceSearchSettings) -> Result<Self, String>;
}

pub fn create_workspace_search_backend(
    settings: &Settings,
    workspace_root: &Path,
) -> Option<Arc<dyn WorkspaceSearchBackend>>;
```

- [ ] **Step 1: Implement** — full module:

Key logic (write as real code in the plan-checked file):

1. **Alias derivation** (deterministic, collision-safe):

```rust
fn root_alias(workspace_root: &Path) -> String {
    let name = workspace_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());
    let canonical = workspace_root.canonicalize().unwrap_or_else(|_| workspace_root.to_path_buf());
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut h);
    format!("ws-{}-{:08x}", name, h.finish() as u32)
}
```

2. **Executable resolution**: settings override → `which oxibrain` (scan `$PATH` manually — the crate has no `which` dep; a 10-line helper over `std::env::split_paths(&env::var_os("PATH"))`) → `dirs::home_dir()/.oxi/bin/oxibrain`. Resolution failure is deferred to first `search` (typed unavailable), not construction.
3. **Brain dir**: `$OXI_BRAIN_DIR` → `dirs::home_dir()/.oxi/brain`. Passed as `--dir` so the child never touches an ambient store (two-plane invariant).
4. **State**:

```rust
#[derive(Debug)]
struct BrainState {
    client: Option<oxibrain_client_brain::BrainClient>,
    registered: bool,
    space_ok: bool,
}

pub struct BrainWorkspaceSearch {
    root: PathBuf,          // canonical workspace
    alias: String,
    endpoint_executable: Option<String>,
    brain_dir: PathBuf,
    space: String,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    max_file_bytes: Option<u64>,
    state: tokio::sync::Mutex<BrainState>,
}
```

5. **`ensure_ready(&self, state: &mut BrainState)`** (async, called under the mutex):

```rust
async fn ensure_ready(&self, st: &mut BrainState) -> Result<(), String> {
    if st.registered && st.space_ok && st.client.is_some() {
        return Ok(());
    }
    let exe = self.resolve_executable().ok_or_else(|| {
        "workspace_search is unavailable: the oxibrain executable was not found \
         (set workspace_search.executable in settings). Use grep for exact search \
         and read to inspect files."
            .to_string()
    })?;
    if st.client.is_none() {
        let endpoint = LocalProcessEndpoint::new(exe, self.brain_dir.clone());
        let client = BrainClient::spawn_local(endpoint)
            .await
            .map_err(|e| format!("workspace_search is unavailable: cannot spawn the \
                                  oxibrain child ({e}). Use grep/read."))?;
        st.client = Some(client);
    }
    if !st.space_ok {
        let client = st.client.as_mut().unwrap();
        let spaces = client.list_spaces().await
            .map_err(|e| format!("workspace_search is unavailable: {e}. Use grep/read."))?;
        if !spaces.iter().any(|s| s.id == self.space || s.name == self.space) {
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
            .register_document_root(RegisterDocumentRootRequest {
                space: self.space.clone(),
                alias: self.alias.clone(),
                path: self.root.to_string_lossy().to_string(),
                include: self.include.clone(),
                exclude: self.exclude.clone(),
                max_file_bytes: self.max_file_bytes,
            })
            .map_err(|e| format!("workspace_search is unavailable: root registration failed: {e}. Use grep/read."))?;
        st.registered = true;
    }
    Ok(())
}
```

6. **Default code include/exclude** when settings leave them unset:

```rust
const DEFAULT_INCLUDE: &[&str] = &[
    "**/*.rs", "**/*.ts", "**/*.tsx", "**/*.js", "**/*.jsx", "**/*.py",
    "**/*.go", "**/*.java", "**/*.kt", "**/*.swift", "**/*.c", "**/*.h",
    "**/*.cpp", "**/*.hpp", "**/*.md", "**/*.toml", "**/*.yaml", "**/*.yml",
    "**/*.json",
];
const DEFAULT_EXCLUDE: &[&str] = &[
    "**/target/**", "**/node_modules/**", "**/dist/**", "**/build/**",
    "**/__pycache__/**", "**/.git/**", "**/.venv/**", "**/venv/**",
];
```

7. **Trait impl** (registered-root over-fetch + post-filter — spaces have no root filter):

```rust
impl WorkspaceSearchBackend for BrainWorkspaceSearch {
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Result<WorkspaceSearchPage, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let mut st = self.state.lock().await;
            if let Err(e) = self.ensure_ready(&mut st).await {
                return Err(e);
            }
            let client = st.client.as_mut().unwrap();
            let fetch = (limit * 3).clamp(12, 48);
            let resp: SearchResponseDto = client
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
            Ok(WorkspaceSearchPage { passages, freshness: Some(freshness_line(&resp.freshness)) })
        })
    }
}

fn freshness_line(f: &oxibrain_client_brain::DocumentFreshnessDto) -> String {
    let mut parts = vec![format!("reconciled: {}", f.reconciled_roots.join(", "))];
    if !f.skipped_roots.is_empty() {
        parts.push(format!(
            "skipped: {}",
            f.skipped_roots.iter().map(|(a, r)| format!("{a} ({r})")).collect::<Vec<_>>().join(", ")
        ));
    }
    if let Some(cov) = f.dense_coverage {
        parts.push(format!("dense coverage: {cov:.0}%"));
    }
    parts.join("; ")
}
```

8. **`create_workspace_search_backend`** (in the same module, mirroring `services.rs::create_memory_backend`): returns `None` when `!settings.workspace_search.enabled` (silent — the zero-cost default-off contract); `Some(Arc::new(BrainWorkspaceSearch::new(...)))` otherwise. `PathGuard::new(workspace_root).validate(&workspace_root)` on construction per D5 (strict canonical validation; construction error → `None` + `tracing::warn!`).

- [ ] **Step 2: Unit tests** (in `brain_workspace.rs` `#[cfg(test)]`):

```rust
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
```

- [ ] **Step 3: Run** — `cargo nextest run -p oxicode-cli --lib foundation::brain_workspace`
Expected: PASS.

- [ ] **Step 4: Gates** — `cargo fmt --all && cargo clippy -p oxicode-cli --all-targets -- -D warnings`
Expected: clean. (`oxibrain-client-brain` is unix-gated; mirror brain.rs's `#[cfg(unix)]` module gating so non-unix builds stay green.)

- [ ] **Step 5: Commit**

```bash
git add oxicode-cli/Cargo.toml Cargo.lock oxicode-cli/src/foundation/brain_workspace.rs oxicode-cli/src/foundation/mod.rs
git commit -m "feat(cli): BrainWorkspaceSearch backend over oxibrain document plane (daemonless stdio)"
```

### Task 4: Live integration test (env-gated, `#[ignore]`)

**Files:**
- Create: `oxicode-cli/tests/workspace_search_live.rs`

**Interfaces:**
- Consumes: `BrainWorkspaceSearch` (Task 3) directly as a backend + a real pinned oxibrain binary.

- [ ] **Step 1: Write the test**:

```rust
//! Live `workspace_search` integration against a real oxibrain binary.
//! Gated: needs OXICODE_BRAIN_WS_BIN=<path-to-oxibrain> and runs only under
//! `cargo nextest run -- --ignored` (excluded from normal offline CI).
#![cfg(unix)]

use oxicode_agent::tools::{ToolContext, WorkspaceSearchBackend, WorkspaceSearchTool};
use std::sync::Arc;

fn bin() -> Option<std::path::PathBuf> {
    std::env::var_os("OXICODE_BRAIN_WS_BIN").map(std::path::PathBuf::from)
}

#[tokio::test]
#[ignore = "requires OXICODE_BRAIN_WS_BIN and a local oxibrain store"]
async fn registers_root_and_returns_ranked_hits() {
    let Some(bin) = bin() else {
        panic!("set OXICODE_BRAIN_WS_BIN to run this test");
    };
    // Isolated store + workspace so the user's real brain is untouched.
    let brain = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(
        ws.path().join("auth.rs"),
        "// session ownership is validated here via flock\nfn require_owner() {}\n",
    )
    .unwrap();

    let mut settings = oxicode_cli::store::settings::Settings::default();
    settings.workspace_search.enabled = true;
    settings.workspace_search.executable = Some(bin.display().to_string());
    settings.workspace_search.space = "dev".into(); // test store has default spaces? create via oxibrain init on temp dir:
    // The spawned child owns `--dir <brain>`; `oxibrain init --dir <brain>` is
    // expected to have been run by this test beforehand (see below).

    // NOTE: the child requires an initialized store. Run first:
    //   <bin> init --dir <brain-dir>
    // via std::process::Command and assert success before spawning.
    let init = std::process::Command::new(&bin)
        .args(["init", "--dir", brain.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(init.success(), "oxibrain init on the temp store failed");

    let backend = oxicode_cli::foundation::brain_workspace::create_workspace_search_backend(
        &settings,
        ws.path(),
    )
    .expect("backend constructed");

    let tool = WorkspaceSearchTool::new(backend);
    let ctx = ToolContext::new(ws.path());
    let out = tool
        .execute(
            "t",
            serde_json::json!({"query": "where is session ownership validated"}),
            None,
            &ctx,
        )
        .await
        .unwrap();
    let text = out.text_or_empty(); // mirror the accessor from Task 1
    assert!(text.contains("auth.rs"), "expected a hit in auth.rs: {text}");
    assert!(text.contains("Verify each passage"), "{text}");
}
```

Implementer notes: (a) if the temp-store `init` flow or default spaces differ (space "dev" missing → the typed unavailable path), assert THAT behavior instead and document the store prerequisite — the test must never touch `~/.oxi/brain`. (b) first query may index/embed — give the tool call a generous budget and keep the workspace tiny (one file). (c) verify the exact CLI subcommand (`init` vs `admin init`) against the pinned binary — the test asserts on observable behavior, adjusting the subcommand is in-scope.

- [ ] **Step 2: Run** — `cargo nextest run -p oxicode-cli --test workspace_search_live` (skipped without the env var), then build oxibrain once (`cd ../oxibrain && cargo build --release`) and run `OXICODE_BRAIN_WS_BIN=../oxibrain/target/release/oxibrain cargo nextest run -p oxicode-cli --test workspace_search_live -- --ignored`.
Expected: PASS locally; skip in CI.

- [ ] **Step 3: Commit**

```bash
git add oxicode-cli/tests/workspace_search_live.rs
git commit -m "test(cli): env-gated live workspace_search integration against real oxibrain"
```

### Task 5: Conditional registration + prompt fragment

**Files:**
- Modify: `oxicode-cli/src/bootstrap.rs` (registration after the shared-tool-registry block, ~line 199-236)
- Modify: `oxicode-cli/src/app/agent_session_runtime.rs` (prompt fragment next to the `memory_block` precedent, ~line 334)

**Interfaces:**
- Consumes: `create_workspace_search_backend` (Task 3), `WorkspaceSearchTool` (Task 1).

- [ ] **Step 1: Register the tool** — in `bootstrap.rs::build_app`, directly after the behavior-pack install block that already touches `let tools = oxicode.tools();`:

```rust
    // Optional oxibrain-backed workspace discovery (design D3/D4): registered
    // only when the user enabled it; zero cost otherwise.
    if let Some(backend) = crate::foundation::brain_workspace::create_workspace_search_backend(
        app.settings(),
        &cwd,
    ) {
        tools.register_arc(Arc::new(oxicode_agent::tools::WorkspaceSearchTool::new(backend)));
        tracing::info!("workspace_search registered (oxibrain document plane)");
    }
```

(Mirror how `CommitTool` is conditionally registered at bootstrap.rs:577 for the `register_arc` import style; `cwd` here is the canonical workspace root already used by PathGuard tool wiring.)

- [ ] **Step 2: Prompt fragment** — in `agent_session_runtime.rs`, next to the existing `memory_block` construction (`let memory_block = if settings.memory_enabled { … }` at ~line 334), add the parallel block:

```rust
        let workspace_search_block = if settings.workspace_search.enabled {
            Some(
                "For a workspace-grounded question with unknown wording or location, \
                 use one focused workspace_search query. Verify its source passages \
                 with read or grep. Use grep for exact identifiers, paths, literals, \
                 regular expressions, and exhaustive occurrence searches. Do not use \
                 workspace_search to create, update, or delete an index."
                    .to_string(),
            )
        } else {
            None
        };
```

…then thread it into the system-prompt assembly exactly where `memory_block` is consumed (same concat), keeping the fragment absent when disabled.

- [ ] **Step 3: Verify by run (TUI smoke)** — `cargo run -p oxicode-cli -- --print "what tools do you have"` with `[workspace_search] enabled = true` in settings and the oxibrain binary present: the agent lists `workspace_search`; with `enabled` absent/false, it does not and startup shows no probe logs. (This is the manual behavioral check; the automated gate is Task 4's live test.)

- [ ] **Step 4: Gates** — `cargo fmt --all && cargo clippy -p oxicode-cli --all-targets -- -D warnings && cargo nextest run -p oxicode-cli`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add oxicode-cli/src/bootstrap.rs oxicode-cli/src/app/agent_session_runtime.rs
git commit -m "feat(cli): conditional workspace_search registration and routing prompt fragment"
```

### Task 6: Docs

**Files:**
- Modify: `docs/designs/2026-09-03-workspace-retrieval-zvec-grep.md` (Status → Phase 2 skeleton shipped)
- Modify: `CHANGELOG.md`

- [ ] **Step 1: CHANGELOG** — under `[Unreleased]` / `### Added`: "`workspace_search` (opt-in, `[workspace_search] enabled = true`): semantic workspace discovery over the local oxibrain document plane via a daemonless `admin serve --stdio` child. Registers the workspace as an idempotent document root, searches the documents plane (hybrid), post-filters by root alias, and returns ranked chunk-level passages with a freshness report and a verify-with-read/grep reminder. Zero startup cost when disabled."
- [ ] **Step 2: Design doc** — flip the Phase 2 rollout bullets already marked done; note the live-test command from Task 4 as the reproduction path.
- [ ] **Step 3: Commit**

```bash
git add docs/designs/2026-09-03-workspace-retrieval-zvec-grep.md CHANGELOG.md
git commit -m "docs: workspace_search skeleton adoption notes and changelog"
```
