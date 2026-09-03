#![allow(unused_doc_comments)]
/// Agent tools system
/// This module provides the tool abstraction layer and built-in tools.
use crate::types::ToolDefinition;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::oneshot;

// ═══════════════════════════════════════════════════════════════════════════
// Capability traits — lightweight interfaces tools need, implemented by the
// composition root (oxicode-cli) bridging to SDK ports. oxicode-agent does NOT depend
// on oxicode-sdk, so these are defined here.
// ═══════════════════════════════════════════════════════════════════════════

/// A single memory item returned by [`MemoryBackend`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryItem {
    /// Unique identifier.
    pub id: String,
    /// Memory kind: "fact", "preference", "context", "summary".
    pub kind: String,
    /// The memory content text.
    pub content: String,
    /// Project/scope identifier.
    pub subject: String,
}

/// Memory backend for the `memory_*` tools. The composition root implements
/// this, bridging to `oxicode_sdk::ports::MemoryStore` + `EmbeddingProvider`.
pub trait MemoryBackend: Send + Sync + std::fmt::Debug + 'static {
    /// Store a memory item, returning its new ID.
    fn put<'a>(
        &'a self,
        content: &'a str,
        kind: &'a str,
        subject: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>>;
    /// Semantic-search stored memories, returning up to `k` matches.
    fn search<'a>(
        &'a self,
        query: &'a str,
        k: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<MemoryItem>, ToolError>> + Send + 'a>>;
    /// List memory items for the given subject.
    fn list<'a>(
        &'a self,
        subject: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<MemoryItem>, ToolError>> + Send + 'a>>;
    /// Delete the memory item with the given ID.
    fn delete<'a>(
        &'a self,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), ToolError>> + Send + 'a>>;

    /// Human-readable memory status (None if not supported by this backend).
    fn memory_info(&self) -> Option<String> {
        None
    }
    /// Trigger sleep consolidation, returning a status message.
    fn trigger_consolidation(&self) -> Option<String> {
        None
    }
    /// Trigger SHMR harmonization, returning a status message.
    fn trigger_harmonize(&self) -> Option<String> {
        None
    }

    /// Delete every memory item in the backend. Returns the number of
    /// items removed.
    ///
    /// The default implementation is **unsupported**: it returns an
    /// honest error rather than silently deleting zero rows. Backends
    /// that can perform a true bulk erase must override this method;
    /// backends without a list-all subject primitive must surface
    /// that limitation rather than guess.
    ///
    /// This is a destructive operation; the `/memory clear` slash
    /// command requires an explicit confirmation flag before invoking.
    fn clear_all<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<usize, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            Err(
                "clear_all not supported by this backend (no list-all subject primitive)"
                    .to_string(),
            )
        })
    }

    /// Force a consolidation (rebuild) job. Returns a status message
    /// describing what was dispatched, or `Err` when the backend has
    /// no in-process capability to enqueue work.
    ///
    /// Default: `Err` ("not supported"). Backends that expose
    /// consolidation via the engine (e.g. Mnemopi sleep) should
    /// override and run the real operation.
    fn enqueue_consolidation<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(
            async move { Err("enqueue_consolidation not supported by this backend".to_string()) },
        )
    }
}

/// Content resolved from an internal protocol URL (e.g. `skill://`, `issue://`).
pub struct ResolvedContent {
    /// The resolved text content.
    pub content: String,
    /// MIME type: "text/markdown", "application/json", "text/plain".
    pub content_type: String,
    /// True if the content is uneditable (suppresses hashline anchors).
    pub immutable: bool,
}

/// URL resolver for internal protocol schemes. The composition root
/// implements this, bridging to `oxicode_sdk::ports::InternalUrlRouter`.
pub trait UrlResolver: Send + Sync + std::fmt::Debug {
    /// Whether this resolver handles the given input URI.
    fn can_resolve(&self, input: &str) -> bool;
    /// Resolve an internal URI to its content, asynchronously.
    fn resolve<'a>(
        &'a self,
        uri: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ResolvedContent, ToolError>> + Send + 'a>>;
}

/// Todo state access capability. Implemented by the composition root
/// (oxicode-cli) bridging to the session-scoped todo state. Used by the
/// `todo` agent tool and the TUI sticky panel.
pub trait TodoStateProvider: Send + Sync + std::fmt::Debug {
    /// Return a snapshot of the current phase list (read-only, for TUI).
    fn get_phases(&self) -> Vec<crate::tools::todo::TodoPhase>;

    /// Synchronously replace the whole phase list. Used by the TUI's
    /// subagent auto-reconcile path, which runs on the sync frame loop and
    /// cannot `.await` `apply_ops`. Only meaningful for local in-process
    /// providers; a remote provider may return a `not supported` error.
    fn set_phases_sync(&self, phases: Vec<crate::tools::todo::TodoPhase>);

    /// Apply a sequence of todo ops, returning the updated state, the
    /// newly-completed transitions (for strikethrough animation), and
    /// any error messages from ambiguous op references.
    fn apply_ops<'a>(
        &'a self,
        ops: Vec<crate::tools::todo::TodoOp>,
    ) -> Pin<
        Box<dyn Future<Output = Result<crate::tools::todo::TodoUpdateResult, String>> + Send + 'a>,
    >;
}

// ── Agent Hub capability (⑥) ──────────────────────────────────────────

/// Agent kind for Hub display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    /// Main conversation agent.
    Main,
    /// Task-spawned sub-agent.
    Task,
    /// Observation-only advisor.
    Advisor,
}

/// Hub display status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentHubStatus {
    /// Currently executing.
    Running,
    /// Finished, idle.
    Idle,
    /// Parked (memory retained, not running).
    Parked,
    /// Abnormal termination.
    Aborted,
}

/// Read-only agent info for Hub display.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    /// Unique identifier.
    pub id: String,
    /// Display name.
    pub display_name: String,
    /// Agent kind.
    pub kind: AgentKind,
    /// Current status.
    pub status: AgentHubStatus,
    /// Current task description (if any).
    pub current_task: Option<String>,
}

/// Agent pool access capability. Implemented by the composition root
/// to expose live sub-agent info to the Hub overlay and todo matching.
pub trait AgentPoolProvider: Send + Sync + std::fmt::Debug {
    /// List all known agents (main + sub-agents).
    fn list_agents(&self) -> Vec<AgentInfo>;
    /// Get a specific agent by ID.
    fn get_agent(&self, id: &str) -> Option<AgentInfo>;
}

// ── LSP capability (⑧) ────────────────────────────────────────────────

/// Aggregated diagnostics across one or more files (returned by
/// [`LspProvider::drain_diagnostics`]). Counts severity buckets so callers
/// can surface a quick "0 errors / 3 warnings" summary without scanning
/// every diagnostic.
#[derive(Debug, Clone, Default)]
pub struct DiagnosticsSummary {
    /// Total number of fresh diagnostics after filtering.
    pub count: usize,
    /// Number of error-severity diagnostics.
    pub errors: usize,
    /// Number of warning-severity diagnostics.
    pub warnings: usize,
    /// Per-file entries; empty when no diagnostics have arrived yet.
    pub entries: Vec<FileDiagnosticEntry>,
}

/// Diagnostics for one file. Path is the LSP document URI as the server
/// reported it (may be `file://`-prefixed); `diagnostics` is the raw payload
/// from `textDocument/publishDiagnostics`.
#[derive(Debug, Clone)]
pub struct FileDiagnosticEntry {
    /// Document URI (typically `file://<absolute path>`).
    pub uri: String,
    /// Path-relative display of the file (best effort).
    pub path: String,
    /// Diagnostics reported by the server for this file.
    pub diagnostics: serde_json::Value,
}

/// LSP action enum — the operations the `lsp` tool supports.
#[derive(Debug, Clone)]
pub enum LspAction {
    /// Get diagnostics for a file.
    Diagnostics {
        /// Path to the file to inspect.
        file: String,
    },
    /// Go to definition.
    Definition {
        /// Path to the file containing the symbol.
        file: String,
        /// 1-based line number of the symbol.
        line: u32,
        /// Optional symbol text to resolve (for disambiguation).
        symbol: Option<String>,
    },
    /// Find references.
    References {
        /// Path to the file containing the symbol.
        file: String,
        /// 1-based line number of the symbol.
        line: u32,
        /// Optional symbol text to find references for.
        symbol: Option<String>,
    },
    /// Hover info.
    Hover {
        /// Path to the file containing the symbol.
        file: String,
        /// 1-based line number of the symbol.
        line: u32,
        /// Optional symbol text to hover.
        symbol: Option<String>,
    },
    /// Rename symbol.
    Rename {
        /// Path to the file containing the symbol.
        file: String,
        /// 1-based line number of the symbol.
        line: u32,
        /// Symbol text to rename.
        symbol: String,
        /// New name for the symbol.
        new_name: String,
        /// If true, apply the rename; otherwise just preview.
        apply: bool,
    },
    /// Get workspace/document symbols.
    Symbols {
        /// Path to the file to inspect (workspace symbols if query-only).
        file: String,
        /// Optional filter query for symbols.
        query: Option<String>,
    },
    /// Get server status.
    Status,
    /// Available code actions at a position.
    CodeActions {
        /// Path to the file containing the position.
        file: String,
        /// 1-based line number.
        line: u32,
        /// Optional symbol hint for disambiguation.
        symbol: Option<String>,
    },
    /// Go to type definition.
    TypeDefinition {
        /// Path to the file containing the symbol.
        file: String,
        /// 1-based line number.
        line: u32,
        /// Optional symbol hint.
        symbol: Option<String>,
    },
    /// Go to implementation.
    Implementation {
        /// Path to the file containing the symbol.
        file: String,
        /// 1-based line number.
        line: u32,
        /// Optional symbol hint.
        symbol: Option<String>,
    },
    /// Rename a file (workspace/willRenameFiles + applyWorkspaceEdit).
    FileRename {
        /// Current path on disk.
        old_path: String,
        /// Target path on disk.
        new_path: String,
        /// If true, apply the rename; otherwise just preview.
        apply: bool,
    },
    /// Reload the LSP server (e.g. rust-analyzer/reloadWorkspace).
    Reload,
    /// Dump the server's capabilities (from the initialize handshake).
    Capabilities,
    /// Send a raw LSP request (method name + optional JSON params).
    Request {
        /// LSP method name (e.g. "workspace/symbol").
        query: String,
        /// Optional JSON payload. If absent, empty params are sent.
        payload: Option<serde_json::Value>,
    },
}

/// LSP access capability. Implemented by an `oxicode-lsp` crate (feature-gated)
/// or stubbed with `None` when LSP is disabled.
pub trait LspProvider: Send + Sync + std::fmt::Debug {
    /// Kick off background initialisation (servers start but `ensure_ready`
    /// isn't awaited). Idempotent.
    fn ensure_started_background<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Block until at least the configured LSP servers have finished their
    /// `initialize` handshake (or the operation times out per the
    /// provider's internal budget).
    fn ensure_ready<'a>(&'a self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

    /// Drain the most recent batch of diagnostics that arrived via
    /// `textDocument/publishDiagnostics`. Returns `None` when nothing
    /// fresh has arrived within `timeout`.
    fn drain_diagnostics<'a>(
        &'a self,
        timeout: std::time::Duration,
    ) -> Pin<Box<dyn Future<Output = Option<DiagnosticsSummary>> + Send + 'a>>;

    /// Read the most recent cached diagnostics for the given file paths
    /// (zero-copy snapshot — no waiting). Paths that have no fresh
    /// diagnostics are omitted from the returned vec.
    fn read_diagnostics<'a>(
        &'a self,
        paths: &'a [std::path::PathBuf],
    ) -> Pin<Box<dyn Future<Output = Vec<FileDiagnosticEntry>> + Send + 'a>>;

    /// Notify the LSP manager that the contents of `path` changed. The
    /// manager is responsible for forwarding a `workspace/didChange` to
    /// every server that owns the file. Default implementation is a no-op
    /// so lightweight providers (e.g. test stubs) don't have to wire it.
    fn notify_file_changed<'a>(
        &'a self,
        _path: &'a std::path::Path,
        _content: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }

    /// Execute an LSP action and return formatted text output.
    fn execute_action<'a>(
        &'a self,
        action: &'a LspAction,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>>;
}

// ── Sub-agent delegation (issue #28 gap 3) ─────────────────────────────

/// Result of an in-process isolated sub-agent fork run.
///
/// Produced by [`SubagentRunner::run_isolated`]. The sub-agent runs
/// with a **fresh, empty context** — its conversation history is
/// completely isolated from the parent agent. Only the final text and
/// usage statistics are returned, keeping the parent's context small.
///
/// This is the library-native alternative to shelling out to the `oxicode`
/// CLI binary. Library consumers (e.g. Oxios) that embed `oxicode-agent`
/// without an `oxicode` subprocess implement this trait so the `subagent`
/// tool works in-process.
#[derive(Debug, Clone, Default)]
pub struct ForkResult {
    /// Final response text from the sub-agent.
    pub text: String,
    /// Input tokens consumed (last reported turn).
    pub input_tokens: usize,
    /// Output tokens consumed (last reported turn).
    pub output_tokens: usize,
    /// Number of agent turns executed.
    pub turns: u32,
    /// Model ID used by the sub-agent.
    pub model: Option<String>,
    /// Error message if the run failed.
    pub error: Option<String>,
}

/// In-process sub-agent runner — the library-native delegation backend.
///
/// When wired into [`ToolContext`] via
/// [`ToolContext::with_subagent_runner`], the `subagent` tool prefers
/// this in-process path over shelling out to the `oxicode` CLI binary.
/// This is essential for library consumers (Oxios) that embed
/// `oxicode-agent` as a kernel without an `oxicode` subprocess.
///
/// The SDK provides a ready-made implementation
/// (`oxicode_sdk::SdkSubagentRunner`) that wraps an `Oxicode` instance and
/// creates a fresh `Agent` for each invocation.
#[async_trait::async_trait]
#[allow(clippy::too_many_arguments)]
pub trait SubagentRunner: Send + Sync + std::fmt::Debug {
    /// Run a single agent task with an isolated (empty) context.
    ///
    /// # Arguments
    /// * `agent_name` — Agent definition name (for logging / display).
    /// * `task` — The task prompt to execute.
    /// * `system_prompt` — Optional system prompt override.
    /// * `model` — Optional model ID override (e.g. `"anthropic/claude-...`).
    /// * `tools` — Optional tool whitelist (empty = all registered tools).
    /// * `cwd` — Working directory for file tools.
    /// * `depth` — Current sub-agent nesting depth. The runner sets
    ///   the forked agent's `subagent_depth` to `depth + 1` so the
    ///   fork's own subagent tool can enforce a recursion cap without
    ///   env vars (issue #28 gap 3 — concurrent `set_var` is UB).
    async fn run_isolated(
        &self,
        agent_name: &str,
        task: &str,
        system_prompt: Option<&str>,
        model: Option<&str>,
        tools: &[String],
        cwd: &Path,
        depth: u8,
    ) -> anyhow::Result<ForkResult>;
}

/// Context passed to tools at execution time.
///
/// This allows tools to operate on a specific workspace without being
/// rebuilt. When `root_dir` is `Some`, tools use it as their base directory.
/// When `None`, tools should fall back to `workspace_dir`.
#[derive(Clone)]
pub struct ToolContext {
    /// Primary workspace directory (used when root_dir is None).
    pub workspace_dir: PathBuf,
    /// Optional explicit root directory for file tools.
    /// Takes priority over workspace_dir if present.
    pub root_dir: Option<PathBuf>,
    /// Session identifier for logging/tracing.
    pub session_id: Option<String>,
    /// Snapshot store for hashline tag emission/validation.
    /// When `None`, hashline edit mode is unavailable.
    pub snapshot_store: Option<Arc<dyn oxicode_hashline::SnapshotStore>>,
    /// Memory backend for `memory_*` tools.
    /// When `None`, memory tools return an error.
    pub memory: Option<Arc<dyn MemoryBackend>>,
    /// URL resolver for internal protocol schemes (`issue://`, `pr://`, etc.).
    /// When `None`, URL-prefixed paths are treated as regular file paths.
    pub url_resolver: Option<Arc<dyn UrlResolver>>,
    /// Todo state for the `todo` agent tool.
    /// When `None`, the `todo` tool returns an error.
    pub todo: Option<Arc<dyn TodoStateProvider>>,
    /// Agent pool for Hub display and todo sub-agent matching.
    pub agent_pool: Option<Arc<dyn AgentPoolProvider>>,
    /// LSP provider for the `lsp` tool.
    pub lsp: Option<Arc<dyn LspProvider>>,
    /// In-process sub-agent runner (issue #28 gap 3).
    /// When `Some`, the `subagent` tool prefers an in-process isolated
    /// run over shelling out to the CLI binary. Library consumers
    /// (e.g. Oxios) that embed `oxicode-agent` without an `oxicode` subprocess
    /// set this so delegation works. When `None`, the CLI backend is
    /// used (the default for `oxicode-cli`).
    pub subagent_runner: Option<Arc<dyn SubagentRunner>>,
    /// Current sub-agent nesting depth for the in-process path
    /// (issue #28 gap 3). The CLI path uses env vars instead.
    /// Default 0 (top-level agent).
    pub subagent_depth: u8,
    /// Intent trace for the current tool call.
    /// Set by the agent loop before executing each tool, read by tools
    /// that surface intent to users (e.g. `ask`).
    pub intent: Option<String>,
}

impl fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolContext")
            .field("workspace_dir", &self.workspace_dir)
            .field("root_dir", &self.root_dir)
            .field("session_id", &self.session_id)
            .field(
                "snapshot_store",
                &self.snapshot_store.as_ref().map(|_| "<dyn SnapshotStore>"),
            )
            .field(
                "memory",
                &self.memory.as_ref().map(|_| "<dyn MemoryBackend>"),
            )
            .field(
                "url_resolver",
                &self.url_resolver.as_ref().map(|_| "<dyn UrlResolver>"),
            )
            .finish()
    }
}

impl ToolContext {
    /// Create a new context with the given workspace.
    pub fn new(workspace_dir: impl Into<PathBuf>) -> Self {
        Self {
            workspace_dir: workspace_dir.into(),
            root_dir: None,
            session_id: None,
            snapshot_store: None,
            memory: None,
            url_resolver: None,
            todo: None,
            agent_pool: None,
            lsp: None,
            subagent_runner: None,
            subagent_depth: 0,
            intent: None,
        }
    }

    /// Get the effective root directory.
    /// Returns root_dir if set, otherwise workspace_dir.
    pub fn root(&self) -> &Path {
        self.root_dir.as_deref().unwrap_or(&self.workspace_dir)
    }

    /// Set a session ID.
    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Set an explicit root directory.
    pub fn with_root(mut self, root_dir: impl Into<PathBuf>) -> Self {
        self.root_dir = Some(root_dir.into());
        self
    }

    /// Set the snapshot store (enables hashline edit mode).
    pub fn with_snapshot_store(mut self, store: Arc<dyn oxicode_hashline::SnapshotStore>) -> Self {
        self.snapshot_store = Some(store);
        self
    }

    /// Set the memory backend (enables memory tools).
    pub fn with_memory(mut self, memory: Arc<dyn MemoryBackend>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Set the URL resolver (enables internal URL dispatch).
    pub fn with_url_resolver(mut self, resolver: Arc<dyn UrlResolver>) -> Self {
        self.url_resolver = Some(resolver);
        self
    }

    /// Set the todo state (enables the `todo` agent tool).
    pub fn with_todo(mut self, todo: Arc<dyn TodoStateProvider>) -> Self {
        self.todo = Some(todo);
        self
    }

    /// Set the in-process sub-agent runner (enables library-native
    /// delegation — issue #28 gap 3).
    pub fn with_subagent_runner(mut self, runner: Arc<dyn SubagentRunner>) -> Self {
        self.subagent_runner = Some(runner);
        self
    }

    /// Attach an intent trace to this context.
    pub fn with_intent(mut self, intent: impl Into<String>) -> Self {
        self.intent = Some(intent.into());
        self
    }
}

impl Default for ToolContext {
    fn default() -> Self {
        Self {
            workspace_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            root_dir: None,
            session_id: None,
            snapshot_store: None,
            memory: None,
            url_resolver: None,
            todo: None,
            agent_pool: None,
            lsp: None,
            subagent_runner: None,
            subagent_depth: 0,
            intent: None,
        }
    }
}

/// Result type for tool execution
pub type ToolError = String;

/// Result of tool execution
#[derive(Debug)]
pub struct AgentToolResult {
    /// pub.
    pub success: bool,
    /// pub.
    pub output: String,
    /// pub.
    pub metadata: Option<serde_json::Value>,
    /// Optional content blocks (e.g., image blocks) to include in the tool result message.
    /// When present, these are used as the content of the ToolResultMessage instead of
    /// wrapping `output` in a Text block.
    pub content_blocks: Option<Vec<oxicode_ai::ContentBlock>>,
    /// When `true`, signals that the agent loop should terminate after this batch
    /// of tool calls completes.  Defaults to `false` so that the loop continues
    /// unless a tool explicitly opts-in to termination.
    pub terminate: bool,
    /// Intent trace — a concise description of what this specific tool call did.
    /// Set by the agent loop from the tool's static `intent()` or by the tool
    /// itself for dynamic intent. Included in `ToolExecutionEnd` events.
    pub intent: Option<String>,
}

impl AgentToolResult {
    /// Creates a successful tool result with the given output text.
    pub fn success(output: impl Into<String>) -> Self {
        Self {
            success: true,
            output: output.into(),
            metadata: None,
            content_blocks: None,
            terminate: false,
            intent: None,
        }
    }

    /// Creates an error tool result with the given error message.
    pub fn error(output: impl Into<String>) -> Self {
        Self {
            success: false,
            output: output.into(),
            metadata: None,
            content_blocks: None,
            terminate: false,
            intent: None,
        }
    }

    /// Attaches structured metadata (JSON) to this result.
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Attaches rich content blocks (images, code, etc.) to this result.
    pub fn with_content_blocks(mut self, blocks: Vec<oxicode_ai::ContentBlock>) -> Self {
        self.content_blocks = Some(blocks);
        self
    }

    /// Mark this result as requesting agent-loop termination.
    pub fn with_terminate(mut self) -> Self {
        self.terminate = true;
        self
    }

    /// Attach an intent trace to this result.
    pub fn with_intent(mut self, intent: impl Into<String>) -> Self {
        self.intent = Some(intent.into());
        self
    }
}

impl fmt::Display for AgentToolResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.output)
    }
}

/// Callback type for progress updates
pub type ProgressCallback = Arc<dyn Fn(String) + Send + Sync>;

/// Tool execution mode for parallel safety.
#[derive(Debug, Clone)]
pub enum ToolExecutionMode {
    /// Safe to run in parallel with any other tool
    ParallelSafe,
    /// Must run sequentially — no parallel execution
    SequentialOnly,
    /// Mutates a specific file — file_mutation_queue serializes same-file access
    MutatesFile(std::path::PathBuf),
    /// Read-only — always parallel safe
    ReadOnly,
}

/// Render output for TUI visualization.
#[derive(Debug, Clone)]
pub struct RenderOutput {
    /// Rendered text content (markdown or plain)
    pub content: String,
    /// Whether to show collapsed by default
    pub collapsed: bool,
    /// Optional summary text for TUI footer
    pub summary: Option<String>,
}

/// Core trait for all agent tools
/// Risk tier for approval gating.
///
/// Determines which approval tiers gate a tool call.
/// - `Read`  — no side effects (lookup, search, inspection).
/// - `Write` — mutates data (creates, edits, commits).
/// - `Exec`  — arbitrary side effects (shell, eval, network).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolTier {
    /// Read-only — inspection, search, lookup.
    Read,
    /// Data mutation — create, edit, commit.
    Write,
    /// Arbitrary execution — shell, eval, network, subagent.
    #[default]
    Exec,
}

/// Core trait for all agent tools
#[async_trait]
pub trait AgentTool: Send + Sync {
    /// Tool name (used in function calls)
    fn name(&self) -> &str;

    /// Human-readable label
    fn label(&self) -> &str;

    /// Description for the model
    fn description(&self) -> &str;

    /// JSON Schema for parameters
    fn parameters_schema(&self) -> Value;

    /// Whether this tool is essential (cannot be disabled).
    /// Essential tools: read, write, edit, bash, grep, find, ls
    /// Optional tools: web_search, github, subagent, etc.
    fn essential(&self) -> bool {
        false
    }

    /// Execute the tool with the given tool call ID and parameters.
    ///
    /// The `ctx` parameter provides workspace information. File tools should
    /// use `ctx.root()` to get the effective directory. Custom tools can use
    /// `ctx.workspace_dir` for workspace-relative operations.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use oxicode_agent::{AgentTool, AgentToolResult, ToolContext};
    /// use serde_json::json;
    /// struct MyTool;
    ///
    /// #[async_trait]
    /// impl AgentTool for MyTool {
    ///     fn name(&self) -> &str { "my_tool" }
    ///     fn label(&self) -> &str { "My Tool" }
    ///     fn description(&self) -> &str { "A custom tool" }
    ///     fn parameters_schema(&self) -> Value { json!({
    ///         "type": "object",
    ///         "properties": {}
    ///     }) }
    ///
    ///     async fn execute(&self, tool_call_id: &str, params: Value, _signal: Option<oneshot::Receiver<()>>, ctx: &ToolContext) -> Result<AgentToolResult, String> {
    ///         println!("Tool '{}' called with params: {:?}, workspace: {:?}", tool_call_id, params, ctx.workspace_dir);
    ///         Ok(AgentToolResult::success("Done!"))
    ///     }
    /// }
    /// ```
    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        signal: Option<oneshot::Receiver<()>>,
        ctx: &ToolContext,
    ) -> Result<AgentToolResult, ToolError>;

    /// Called with progress updates during execution.
    /// Tools can override this to emit streaming updates.
    fn on_progress(&self, _callback: ProgressCallback) {
        // Default no-op
    }

    /// Structured browse progress callback for browser tool context enrichment.
    /// Default implementation is no-op. Only browse tools override this to
    /// register a callback that enriches `ToolCallContext` with structured
    /// data from `BrowseProgress` events.
    fn on_browse_progress(&self, _callback: crate::tools::browse::BrowseProgressCallback) {}

    /// Custom rendering for tool call (TUI visualization).
    /// Return None to use the default tool_renderer.rs formatter.
    fn render_call(&self, _params: &serde_json::Value) -> Option<RenderOutput> {
        None
    }

    /// Custom rendering for tool result (TUI visualization).
    /// Return None to use the default tool_renderer.rs formatter.
    fn render_result(&self, _result: &AgentToolResult) -> Option<RenderOutput> {
        None
    }

    /// Intent trace — a concise description of what this tool does.
    /// Returned value is included in `ToolExecutionStart` / `ToolExecutionEnd`
    /// events so the agent loop can surface intent to users or telemetry.
    /// Default `None` (no intent tracing).
    fn intent(&self) -> Option<&str> {
        None
    }

    /// Execution mode for parallel safety.
    /// Defaults to ParallelSafe. Override for file-mutating or sequential tools.
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::ParallelSafe
    }

    /// Risk tier for approval gating.
    ///
    /// - `Read`  — no side effects (lookup, search, inspection).
    /// - `Write` — mutates data (creates, edits, commits).
    /// - `Exec`  — arbitrary side effects (shell, eval, network).
    ///
    /// Default: `Exec` (safest default — requires explicit opt-down).
    fn tool_tier(&self) -> ToolTier {
        ToolTier::Exec
    }

    /// Return the current active tab ID, if this tool manages browser tabs.
    /// Defaults to `None`. Browser tools override this to return the tab ID
    /// of the currently-open tab during execution, so the agent loop can
    /// populate `ToolExecutionUpdate.tab_id`.
    fn current_tab_id(&self) -> Option<uuid::Uuid> {
        None
    }

    /// Receive a shared slot where the tool can write the current tab ID.
    /// The agent loop creates the slot and passes it before `on_progress`;
    /// the tool writes `Some(tab_id)` when it opens a tab and `None` when
    /// it closes it. Defaults to a no-op — only tab-aware tools override.
    fn set_tab_id_slot(&self, _slot: Arc<parking_lot::Mutex<Option<uuid::Uuid>>>) {}

    /// Convert to ToolDefinition
    fn to_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: serde_json::from_value(self.parameters_schema()).unwrap_or_default(),
        }
    }
}

// Built-in tools
/// Ask tool — ask the user one or more clarifying questions via the TUI overlay.
pub mod ask;
/// AST-aware structural code rewriting tool (ast-grep backed).
pub mod ast_edit;
/// AST structural search tool (wraps the `sg` CLI).
pub mod ast_grep;
/// Bash shell execution tool.
pub mod bash;
/// Persistent-session bash tool (pack-routed `ShellSession` backend).
pub mod bash_session;
/// Browser tools (engine abstraction always compiled).
pub mod browse;
/// Checkpoint and Rewind tools — save/restore investigation state.
pub mod checkpoint_tool;
/// Conventional-commit tool (deterministic scope + LLM analysis).
pub mod commit;
/// Computer tool — computer control using Vision AI.
pub mod computer_tool;
/// Context7 documentation tools.
pub mod context7;
/// Debug tool — DAP-backed debugger integration (scaffold).
pub mod debug_tool;
/// In-place file edit tool.
pub mod edit;
/// Diff-based edit helpers.
pub mod edit_diff;
/// Eval tool — persistent-kernel code execution (scaffold).
pub mod eval_tool;
/// ExactSearchEngine — private streaming search backend behind `GrepTool` v2.
// allow(dead_code) is interim: removed when the v2 wiring consumes the engine.
#[allow(dead_code)]
mod exact_search;
/// Serialised file-mutation queue.
pub mod file_mutation_queue;
/// File-fsystem find tool.
pub mod find;
/// Image generation tool (OpenRouter API).
pub mod generate_image;
/// GitHub integration tool (gh CLI-based).
pub mod github;
/// GitHub repository search tool (legacy REST API).
pub mod github_search;
/// Goal tool — manage investigation goals with token budgets.
pub mod goal_tool;
/// Content search (grep) tool.
pub mod grep;
/// TokioHashlineFs — tokio::fs-backed HashlineFs implementation.
pub mod hashline_fs;
/// Shared HTTP client singleton.
pub mod http_client;
/// Hub tool — agent coordination for peer messaging and job management.
pub mod hub_tool;
/// Inspect Image tool — analyze images using Vision LLM capabilities.
pub mod inspect_image_tool;
/// Local issue management tool backed by the `.oxicode/issues/` store.
pub mod issue;
/// Learn tool — capture a reusable lesson to memory and optionally create a managed skill.
pub mod learn_tool;
/// Directory listing tool.
pub mod ls;
/// LSP tool (requires LspProvider capability).
pub mod lsp;
/// Manage Skill tool — create, update, or delete isolated managed SKILL.md files.
pub mod manage_skill_tool;
/// Memory edit tool — update or delete a memory item.
pub mod memory_edit;
/// Memory recall tool — semantic search over stored memories.
pub mod memory_recall;
/// Memory reflect tool — persist a session summary to memory.
pub mod memory_reflect;
/// Memory retain tool — persist a memory item to the backend.
pub mod memory_retain;
/// Path security (traversal protection).
pub mod path_security;
/// Path manipulation utilities.
pub mod path_utils;
/// File reading tool.
pub mod read;
pub(crate) mod read_http;
/// Rendering utilities for tool output.
pub mod render_utils;
/// Review tool — request code review with focus areas and priorities.
pub mod review_tool;
/// Search result cache and get_search_results tool.
pub mod search_cache;
/// Sub-agent delegation tool.
pub mod subagent;
/// Phased todo tool (init/start/done/drop/rm/append/view).
pub mod todo;
/// Tool definition wrapper helpers.
pub mod tool_definition_wrapper;
/// Output truncation helpers.
pub mod truncate;
/// TTS tool — text-to-speech synthesis.
pub mod tts_tool;
/// Vibe tool — manage persistent worker sessions.
pub mod vibe_tool;
/// Multi-engine web search tool (oxibrowser search module).
pub mod web_search;
/// File writing tool.
pub mod write;
/// Yield tool — subagent result submission.
pub mod yield_tool;

// Re-export for convenience
pub use bash::BashTool;
pub use debug_tool::DebugTool;
pub use edit::EditTool;
pub use eval_tool::EvalTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
// pub use search_cache;

pub use crate::mcp::McpTool;
pub use ask::{AskBridge, AskTool};
pub use ast_edit::AstEditTool;
pub use ast_grep::AstGrepTool;
pub use commit::CommitTool;
pub use context7::{Context7QueryDocsTool, Context7ResolveLibraryIdTool};
pub use memory_edit::MemoryEditTool;
pub use memory_recall::MemoryRecallTool;
pub use memory_reflect::MemoryReflectTool;
pub use memory_retain::MemoryRetainTool;
pub use subagent::SubagentTool;
pub use write::WriteTool;

/// Tool registry for managing available tools
#[derive(Clone)]
pub struct ToolRegistry {
    tools: Arc<parking_lot::RwLock<std::collections::HashMap<String, Arc<dyn AgentTool>>>>,
    /// Optional MCP manager, set by `with_builtins_cwd()` so the TUI and
    /// other consumers can reach the live MCP state (Phase 2+).
    mcp_manager: Arc<parking_lot::RwLock<Option<Arc<crate::mcp::McpManager>>>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    /// Creates an empty tool registry.
    pub fn new() -> Self {
        Self {
            tools: Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
            mcp_manager: Arc::new(parking_lot::RwLock::new(None)),
        }
    }

    /// Attach an `McpManager` to this registry. Replaces any previous one.
    pub fn set_mcp_manager(&self, mgr: Arc<crate::mcp::McpManager>) {
        *self.mcp_manager.write() = Some(mgr);
    }

    /// Get the attached `McpManager`, if any.
    pub fn mcp_manager(&self) -> Option<Arc<crate::mcp::McpManager>> {
        self.mcp_manager.read().clone()
    }

    /// Register a tool
    pub fn register(&self, tool: impl AgentTool + 'static) {
        let name = tool.name().to_string();
        self.tools.write().insert(name, Arc::new(tool));
    }

    /// Register a tool that is already wrapped in an `Arc`.
    /// This is the primary path for extensions that produce `Arc<dyn AgentTool>`.
    pub fn register_arc(&self, tool: Arc<dyn AgentTool>) {
        let name = tool.name().to_string();
        self.tools.write().insert(name, tool);
    }

    /// Get a tool by name
    pub fn get(&self, name: &str) -> Option<Arc<dyn AgentTool>> {
        self.tools.read().get(name).cloned()
    }

    /// Unregister a tool by name.
    /// Returns `true` if the tool was present and removed.
    pub fn unregister(&self, name: &str) -> bool {
        self.tools.write().remove(name).is_some()
    }

    /// List all registered tool names
    pub fn names(&self) -> Vec<String> {
        self.tools.read().keys().cloned().collect()
    }

    /// Get all tool definitions
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .read()
            .values()
            .map(|t| t.to_definition())
            .collect()
    }

    /// Get all tools as a slice
    pub fn get_tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.tools.read().values().cloned().collect()
    }

    /// Check whether all tools in `required` are registered.
    ///
    /// Useful for validating program/module dependencies before execution.
    ///
    /// # Example
    ///
    /// ```
    /// use oxicode_agent::ToolRegistry;
    /// let registry = ToolRegistry::new();
    /// assert!(!registry.has_all(&["read", "write"]));
    /// ```
    pub fn has_all(&self, required: &[&str]) -> bool {
        let tools = self.tools.read();
        required.iter().all(|name| tools.contains_key(*name))
    }

    /// Return the subset of `required` tool names that are **not** registered.
    ///
    /// # Example
    ///
    /// ```
    /// use oxicode_agent::ToolRegistry;
    /// let registry = ToolRegistry::new();
    /// let missing = registry.missing(&["read", "exec", "nonexistent"]);
    /// assert_eq!(missing, vec!["read", "exec", "nonexistent"]);
    /// ```
    pub fn missing<'a>(&self, required: &[&'a str]) -> Vec<&'a str> {
        let tools = self.tools.read();
        required
            .iter()
            .filter(|name| !tools.contains_key(**name))
            .copied()
            .collect()
    }

    /// Create a registry with all built-in tools
    ///
    /// # Examples
    ///
    /// ```
    /// use oxicode_agent::ToolRegistry;
    /// let registry = ToolRegistry::with_builtins();
    /// let tools = registry.names();
    /// assert!(tools.contains(&"read".to_string()));
    /// assert!(tools.contains(&"write".to_string()));
    /// assert!(tools.contains(&"bash".to_string()));
    /// ```
    pub fn with_builtins() -> Self {
        Self::with_builtins_cwd(PathBuf::from("."), &[])
    }

    /// Create a registry with all built-in tools, using the given cwd.
    ///
    /// Pass `disabled_tools` to selectively disable built-in tools
    /// (e.g. `["web_search", "github_search"]` for a minimal setup).
    pub fn with_builtins_cwd(cwd: PathBuf, disabled_tools: &[String]) -> Self {
        let registry = Self::new();
        let disabled: std::collections::HashSet<&str> =
            disabled_tools.iter().map(|s| s.as_str()).collect();

        // Helper to create shared cache on demand
        let cache_once: std::cell::OnceCell<Arc<search_cache::SearchCache>> =
            std::cell::OnceCell::new();

        // MCP: use OnceCell to avoid re-creating McpManager on repeated calls
        let mcp_once: std::cell::OnceCell<Arc<crate::mcp::McpManager>> = std::cell::OnceCell::new();
        let mcp_manager = mcp_once.get_or_init(crate::mcp::McpManager::spawn).clone();

        // Register all builtin tools — essential ones ignore disabled list
        let mut all_tools: Vec<Box<dyn AgentTool>> = vec![
            Box::new(ReadTool::with_cwd(cwd.clone())),
            Box::new(WriteTool::with_cwd(cwd.clone())),
            Box::new(AstGrepTool::with_cwd(cwd.clone())),
            Box::new(BashTool::with_cwd(cwd.clone())),
            Box::new(EditTool::with_cwd(cwd.clone())),
            Box::new(GrepTool::with_cwd(cwd.clone())),
            Box::new(FindTool::with_cwd(cwd.clone())),
            Box::new(LsTool::with_cwd(cwd.clone())),
            Box::new(web_search::WebSearchTool::new(
                cache_once
                    .get_or_init(|| Arc::new(search_cache::SearchCache::new()))
                    .clone(),
            )),
            Box::new(search_cache::GetSearchResultsTool::new(
                cache_once
                    .get_or_init(|| Arc::new(search_cache::SearchCache::new()))
                    .clone(),
            )),
            Box::new(github::GitHubTool::new(
                cache_once
                    .get_or_init(|| Arc::new(search_cache::SearchCache::new()))
                    .clone(),
            )),
            Box::new(SubagentTool::with_cwd(cwd.clone())),
            Box::new(todo::TodoTool),
            Box::new(memory_recall::MemoryRecallTool),
            Box::new(memory_reflect::MemoryReflectTool),
            Box::new(memory_retain::MemoryRetainTool),
            Box::new(memory_edit::MemoryEditTool),
        ];

        all_tools.push(Box::new(crate::mcp::McpTool::new(mcp_manager.clone())));

        // Phase 3: register direct MCP tools from the metadata cache.
        for def in mcp_manager.direct_tools_from_cache() {
            all_tools.push(Box::new(crate::mcp::McpDirectTool::new(
                mcp_manager.clone(),
                def,
            )));
        }

        // Remember the manager on the registry so the TUI can reach it.
        registry.set_mcp_manager(mcp_manager);

        all_tools.push(Box::new(context7::Context7ResolveLibraryIdTool::new()));
        all_tools.push(Box::new(context7::Context7QueryDocsTool::new()));
        all_tools.push(Box::new(generate_image::GenerateImageTool::new()));
        all_tools.push(Box::new(commit::CommitTool::unconfigured()));
        all_tools.push(Box::new(ast_edit::AstEditTool::new()));
        all_tools.push(Box::new(lsp::LspTool));
        all_tools.push(Box::new(eval_tool::EvalTool));
        all_tools.push(Box::new(checkpoint_tool::CheckpointTool));
        all_tools.push(Box::new(checkpoint_tool::RewindTool));
        all_tools.push(Box::new(hub_tool::HubTool));
        all_tools.push(Box::new(yield_tool::YieldTool));
        all_tools.push(Box::new(goal_tool::GoalTool));
        all_tools.push(Box::new(review_tool::ReviewTool));
        all_tools.push(Box::new(learn_tool::LearnTool));
        all_tools.push(Box::new(manage_skill_tool::ManageSkillTool));
        all_tools.push(Box::new(inspect_image_tool::InspectImageTool));
        all_tools.push(Box::new(computer_tool::ComputerTool));
        all_tools.push(Box::new(tts_tool::TtsTool));
        all_tools.push(Box::new(vibe_tool::VibeTool));
        // debug_tool — DAP-backed debugger integration.
        // Most actions are validated scaffolds (route through xd://debug);
        // real launch/attach/breakpoint control is wired via the harness device.
        all_tools.push(Box::new(debug_tool::DebugTool));

        for tool in all_tools {
            if tool.essential() || !disabled.contains(tool.name()) {
                // web_search ↔ get_search_results coupling
                if tool.name() == "get_search_results" && disabled.contains("web_search") {
                    continue;
                }
                registry.register_arc(Arc::from(tool));
            }
        }

        registry
    }

    /// Extend this registry with all tools from another registry.
    ///
    /// Useful for composing tool sets from multiple sources
    /// (e.g., coding tools + kernel tools + browser tools).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let base = ToolRegistry::new();
    /// base.extend_from(&other_registry);
    /// ```
    pub fn extend_from(&self, other: &ToolRegistry) {
        for name in other.names() {
            if let Some(tool) = other.get(&name) {
                self.register_arc(tool);
            }
        }
    }

    /// Create registry with selected builtins only.
    pub fn with_selected_tools(cwd: PathBuf, names: &[&str]) -> Self {
        let full = Self::with_builtins_cwd(cwd, &[]);
        let registry = Self::new();
        let set: std::collections::HashSet<&str> = names.iter().copied().collect();
        for name in full.names() {
            if set.contains(name.as_str())
                && let Some(tool) = full.get(&name)
            {
                registry.register_arc(tool);
            }
        }
        registry
    }
}
