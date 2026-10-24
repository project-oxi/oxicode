/// Agent configuration
use oxicode_ai::CompactionStrategy;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

fn default_context_window() -> usize {
    128_000
}

// Agent autonomy mode — controls whether the agent may pause to ask the
// user questions or runs autonomously to completion. In [`Mode::Auto`] the
// `ask` tool short-circuits and a per-turn directive reinforces autonomous
// operation; [`Mode::Default`] is normal interactive behavior.
use std::sync::atomic::{AtomicU8, Ordering};

/// Agent autonomy mode.
///
/// - [`Mode::Default`]: normal interactive behavior — the agent may use the
///   `ask` tool to request user input.
/// - [`Mode::Auto`]: autonomous operation — the agent runs to completion
///   without asking the user questions. The `ask` tool is short-circuited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Normal interactive behavior (the default).
    #[default]
    Default,
    /// Autonomous operation — no user questions, run to completion.
    Auto,
}

impl Mode {
    /// Returns `true` in autonomous ([`Mode::Auto`]) mode.
    pub fn is_auto(self) -> bool {
        matches!(self, Mode::Auto)
    }

    /// Toggle between the two modes.
    pub fn toggle(self) -> Self {
        match self {
            Mode::Default => Mode::Auto,
            Mode::Auto => Mode::Default,
        }
    }

    /// Short display label (`"default"` / `"auto"`).
    pub fn label(self) -> &'static str {
        match self {
            Mode::Default => "default",
            Mode::Auto => "auto",
        }
    }

    /// Encode as a `u8` for storage in a shared atomic.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode from a `u8` (any value other than `1` maps to [`Mode::Default`]).
    pub fn from_u8(v: u8) -> Self {
        if v == Mode::Auto.as_u8() {
            Mode::Auto
        } else {
            Mode::Default
        }
    }

    /// Read the current mode from a shared atomic.
    pub fn load(atomic: &AtomicU8) -> Self {
        Mode::from_u8(atomic.load(Ordering::SeqCst))
    }
}
/// Hook context for `shouldStopAfterTurn`.
#[derive(Debug, Clone)]
pub struct ShouldStopAfterTurnContext {
    /// The assistant message that completed the turn.
    pub message: oxicode_ai::AssistantMessage,
    /// Tool result messages from this turn.
    pub tool_results: Vec<oxicode_ai::ToolResultMessage>,
    /// Current iteration number.
    pub iteration: usize,
}

/// Result of `beforeToolCall` hook.
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallResult {
    /// If `true`, the tool call is blocked and an error result is returned.
    pub block: bool,
    /// Human-readable reason for blocking.
    pub reason: Option<String>,
}

/// Result of `afterToolCall` hook.
#[derive(Debug, Clone, Default)]
pub struct AfterToolCallResult {
    /// Override content for the tool result.
    pub content: Option<String>,
    /// Override error status.
    pub is_error: Option<bool>,
    /// Signal that the agent should stop after this batch.
    pub terminate: Option<bool>,
    /// Arbitrary structured details returned by the hook.
    ///
    /// Consumers (e.g. telemetry, middleware) can use this to attach
    /// extra context without extending the struct.
    pub details: Option<serde_json::Value>,
}

/// Hook context for `beforeToolCall`.
#[derive(Debug, Clone)]
pub struct BeforeToolCallContext {
    /// The tool call being made.
    pub tool_call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// Validated arguments.
    pub args: serde_json::Value,
}

/// Hook context for `afterToolCall`.
#[derive(Debug, Clone)]
pub struct AfterToolCallContext {
    /// The tool call that was made.
    pub tool_call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// The tool result content.
    pub result: String,
    /// Whether the result is an error.
    pub is_error: bool,
    /// Arbitrary structured details provided to the hook.
    ///
    /// Set by the agent loop before invoking the hook so that consumers
    /// receive extra context (e.g. execution timing, tool-specific metadata).
    pub details: Option<serde_json::Value>,
}

/// Callback hooks for the agent loop.
///
/// These mirror pi-mono's `AgentLoopConfig` hooks, allowing callers to
/// inject custom logic at key points in the agentic loop.
#[derive(Default)]
#[allow(clippy::type_complexity)]
pub struct AgentHooks {
    /// Called after each turn completes. Return `true` to stop the agent loop.
    ///
    /// Wrapped in `Arc` so the hook can be invoked multiple times without
    /// being consumed (unlike `Box<dyn Fn>` which requires `take()`).
    pub should_stop_after_turn:
        Option<Arc<dyn Fn(&ShouldStopAfterTurnContext) -> bool + Send + Sync>>,

    /// Called before a tool is executed. Return a `BeforeToolCallResult` with
    /// `block: true` to prevent execution.
    #[allow(clippy::type_complexity)]
    pub before_tool_call:
        Option<Box<dyn Fn(&BeforeToolCallContext) -> BeforeToolCallResult + Send + Sync>>,

    /// Called after a tool execution completes. Can override the result.
    #[allow(clippy::type_complexity)]
    pub after_tool_call:
        Option<Box<dyn Fn(&AfterToolCallContext) -> AfterToolCallResult + Send + Sync>>,

    /// Returns steering messages to inject mid-run. Called after each turn
    /// (unless stopped).
    #[allow(clippy::type_complexity)]
    pub get_steering_messages: Option<Arc<dyn Fn() -> Vec<oxicode_ai::Message> + Send + Sync>>,

    /// Returns follow-up messages to process after the agent would stop.
    /// Called when the agent has no more tool calls and no steering messages.
    #[allow(clippy::type_complexity)]
    pub get_follow_up_messages: Option<Arc<dyn Fn() -> Vec<oxicode_ai::Message> + Send + Sync>>,

    /// Tool execution mode.
    pub tool_execution: ToolExecutionMode,
}

/// How tool calls are executed within a single assistant turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolExecutionMode {
    /// Execute tool calls sequentially, one at a time.
    Sequential,
    /// Execute tool calls concurrently (in parallel).
    #[default]
    Parallel,
}

/// Agent runtime configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Agent name
    pub name: String,
    /// Agent description
    pub description: Option<String>,
    /// Model ID to use
    pub model_id: String,
    /// System prompt
    pub system_prompt: Option<String>,
    /// Timeout in seconds for the entire agent run
    pub timeout_seconds: u64,
    /// Temperature for generation (0.0 to 1.0)
    pub temperature: Option<f64>,
    /// Maximum tokens to generate
    pub max_tokens: Option<usize>,
    /// Compaction strategy for long conversations
    #[serde(default)]
    pub compaction_strategy: CompactionStrategy,
    /// Custom instruction passed to the compactor
    #[serde(default)]
    pub compaction_instruction: Option<String>,
    /// Model context window size (used for threshold-based compaction)
    #[serde(default = "default_context_window")]
    pub context_window: usize,
    /// Working directory for file tools. Defaults to current directory if None.
    #[serde(default)]
    pub workspace_dir: Option<std::path::PathBuf>,
    /// Output mode for agent responses.
    ///
    /// When set, the agent extracts structured output from the final response.
    /// See [`OutputMode`] for available modes.
    ///
    /// [`OutputMode`]: crate::structured_output::OutputMode
    #[serde(default)]
    pub output_mode: Option<String>,
    /// Session identity used by tools that gate behavior on liveness (e.g. the
    /// `issue` tool's `start`/`close` ownership checks). When `Some`, this value
    /// is threaded through to [`crate::tools::ToolContext::session_id`].
    /// `None` means the tool receives `session_id == None` and ownership-gated
    /// operations will reject the call (defensive default).
    #[serde(default)]
    pub session_id: Option<String>,

    /// Autonomy mode — [`Mode::Default`] (interactive) or [`Mode::Auto`]
    /// (autonomous; the `ask` tool is short-circuited and a directive
    /// reinforces autonomous operation). Default: [`Mode::Default`].
    #[serde(default)]
    pub mode: Mode,

    /// Per-provider options for fine-grained control.
    ///
    /// When set, these are passed through to [`oxicode_ai::StreamOptions::provider_options`]
    /// so the provider can read provider-specific settings (e.g. Anthropic adaptive
    /// thinking, OpenAI reasoning_effort, Google thinkingConfig).
    #[serde(default)]
    pub provider_options: Option<oxicode_ai::ProviderOptions>,

    /// TTSR engine for stream rule checking. When set, streaming output
    /// is checked against registered rules and violations trigger
    /// [`crate::agent_loop::StreamOutcome::RuleInterrupt`].
    #[serde(skip, default)]
    pub ttsr_engine: Option<std::sync::Arc<crate::agent_loop::ttsr::TtsrEngine>>,

    /// Memory backend for `memory_*` tools.
    #[serde(skip, default)]
    pub memory: Option<std::sync::Arc<dyn crate::tools::MemoryBackend>>,
    /// Todo state provider for the `todo` tool.
    #[serde(skip, default)]
    pub todo: Option<std::sync::Arc<dyn crate::tools::TodoStateProvider>>,
    /// Whether to inject stop-time reminders for incomplete todos.
    /// Default `true`. Threaded to `AgentLoopConfig::todo_reminders_enabled`.
    #[serde(default = "default_true_bool")]
    pub todo_reminders_enabled: bool,
    /// Max stop-time todo reminders per run. Default
    /// [`crate::tools::todo::MAX_TODO_STOP_REMINDERS`].
    #[serde(default = "default_todo_reminders_max")]
    pub todo_reminders_max: u32,
    /// Eager first-turn todo-list creation policy. Default `Off` preserves
    /// today's behavior. Threaded to `AgentLoopConfig::todo_eager_mode`.
    #[serde(default = "default_todo_eager_mode")]
    pub todo_eager_mode: crate::agent_loop::todo_policy::TodoEagerMode,
    /// Agent pool for Hub display and sub-agent matching.
    #[serde(skip, default)]
    pub agent_pool: Option<std::sync::Arc<dyn crate::tools::AgentPoolProvider>>,
    /// URL resolver for internal protocol schemes (`issue://`, `pr://`, etc.).
    /// Threaded through to [`crate::agent_loop::config::AgentLoopConfig::url_resolver`].
    /// When `None`, URL-prefixed paths are treated as regular file paths.
    #[serde(skip, default)]
    pub url_resolver: Option<std::sync::Arc<dyn crate::tools::UrlResolver>>,
    /// LSP provider for the `lsp` tool.
    /// Threaded through to [`crate::agent_loop::config::AgentLoopConfig::lsp`].
    /// When `None`, the `lsp` tool returns an error.
    #[serde(skip, default)]
    pub lsp: Option<std::sync::Arc<dyn crate::tools::LspProvider>>,

    /// Maximum bytes of a tool result's text content before truncation
    /// (#28 gap 1, surfaced as #32). Threaded through to
    /// [`crate::agent_loop::config::AgentLoopConfig::max_tool_result_bytes`].
    ///
    /// When set, tool results exceeding this limit are truncated and a
    /// `"... [truncated: N bytes omitted]"` marker is appended, preventing a
    /// single large tool output from consuming the context window.
    ///
    /// `None` (default) = no limit. Opt-in.
    #[serde(skip, default)]
    pub max_tool_result_bytes: Option<usize>,

    /// In-process sub-agent runner (#28 gap 3, surfaced as #32). When set,
    /// the `subagent` tool prefers an in-process isolated run over shelling
    /// out. Threaded through to
    /// [`crate::agent_loop::config::AgentLoopConfig::subagent_runner`].
    #[serde(skip, default)]
    pub subagent_runner: Option<std::sync::Arc<dyn crate::tools::SubagentRunner>>,

    /// Current sub-agent nesting depth (#28 gap 3, surfaced as #32). Default
    /// `0` (top-level). The `subagent` tool increments this when forking a
    /// child config to cap recursion.
    #[serde(skip, default)]
    pub subagent_depth: u8,
    /// Snapshot store for hashline line-anchored edit mode.
    ///
    /// When `Some`, the `read` tool records file snapshots and emits
    /// `[path#TAG]` headers, and the `edit` tool validates edits against
    /// them. When `None` (default), hashline anchoring is disabled and the
    /// edit tool falls back to plain text replacement.
    #[serde(skip, default)]
    pub snapshot_store: Option<std::sync::Arc<dyn oxicode_hashline::SnapshotStore>>,
    /// Delivery mode for steering messages queued mid-run (upstream pi
    /// `steeringMode`). Threaded to `AgentLoopConfig::steering_mode`.
    /// Default: [`QueueMode::All`].
    #[serde(default)]
    pub steering_mode: crate::agent_loop::config::QueueMode,
    /// Delivery mode for follow-up messages queued mid-run (upstream pi
    /// `followUpMode`). Threaded to `AgentLoopConfig::follow_up_mode`.
    /// Default: [`QueueMode::All`].
    #[serde(default)]
    pub follow_up_mode: crate::agent_loop::config::QueueMode,
}

fn default_true_bool() -> bool {
    true
}

fn default_todo_reminders_max() -> u32 {
    crate::tools::todo::MAX_TODO_STOP_REMINDERS
}
fn default_todo_eager_mode() -> crate::agent_loop::todo_policy::TodoEagerMode {
    crate::agent_loop::todo_policy::TodoEagerMode::Off
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: "oxicode-agent".to_string(),
            todo_reminders_enabled: true,
            todo_reminders_max: crate::tools::todo::MAX_TODO_STOP_REMINDERS,
            todo_eager_mode: crate::agent_loop::todo_policy::TodoEagerMode::Off,
            steering_mode: crate::agent_loop::config::QueueMode::default(),
            follow_up_mode: crate::agent_loop::config::QueueMode::default(),
            description: None,
            model_id: "claude-sonnet-4-20250514".to_string(),
            system_prompt: None,
            timeout_seconds: 300,
            temperature: None,
            max_tokens: None,
            compaction_strategy: CompactionStrategy::default(),
            compaction_instruction: None,
            context_window: 128_000,
            workspace_dir: None,
            output_mode: None,
            provider_options: None,
            mode: Mode::Default,
            session_id: None,
            ttsr_engine: None,
            memory: None,
            todo: None,
            agent_pool: None,
            url_resolver: None,
            lsp: None,
            max_tool_result_bytes: None,
            subagent_runner: None,
            subagent_depth: 0,
            snapshot_store: None,
        }
    }
}

impl AgentConfig {
    /// Create a new config with the given model ID.
    pub fn new(model_id: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            ..Default::default()
        }
    }

    /// Set the agent name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Set the system prompt.
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Set the timeout in seconds for the entire agent run.
    pub fn with_timeout(mut self, seconds: u64) -> Self {
        self.timeout_seconds = seconds;
        self
    }

    /// Set the compaction strategy for long conversations.
    pub fn with_compaction_strategy(mut self, strategy: CompactionStrategy) -> Self {
        self.compaction_strategy = strategy;
        self
    }

    /// Set a custom instruction passed to the compactor.
    pub fn with_compaction_instruction(mut self, instruction: impl Into<String>) -> Self {
        self.compaction_instruction = Some(instruction.into());
        self
    }

    /// Set the session identity threaded into [`crate::tools::ToolContext::session_id`].
    ///
    /// Tools that gate behavior on liveness (e.g. an `issue` tool's
    /// `start`/`close` ownership checks) use this to identify the caller.
    /// Leaving it `None` causes those tools to see an empty caller id and
    /// reject ownership-gated operations (defensive default).
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Set the hashline snapshot store — enables line-anchored edit mode in
    /// the `read`/`edit` tools (emits `[path#TAG]` headers, validates edits).
    pub fn with_snapshot_store(
        mut self,
        store: std::sync::Arc<dyn oxicode_hashline::SnapshotStore>,
    ) -> Self {
        self.snapshot_store = Some(store);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_defaults_to_none() {
        let c = AgentConfig::default();
        assert!(c.session_id.is_none(), "default session_id must be None");
    }

    #[test]
    fn with_session_id_sets_the_field() {
        let c = AgentConfig::new("m").with_session_id("proc-42");
        assert_eq!(c.session_id.as_deref(), Some("proc-42"));
    }

    #[test]
    fn session_id_round_trips_through_serde() {
        // Forward-compat: a serialized config with the new field deserializes back.
        let with = AgentConfig::new("m").with_session_id("proc-7");
        let json = serde_json::to_string(&with).unwrap();
        assert!(json.contains("\"session_id\":"));
        let back: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id.as_deref(), Some("proc-7"));

        // Backward-compat: a payload WITHOUT the session_id key must still
        // deserialize and default the field to None. We build that payload by
        // serializing a config, then stripping the key with serde_json::Value.
        let mut v: serde_json::Value =
            serde_json::from_str(&json).expect("config serializes to valid JSON");
        if let Some(obj) = v.as_object_mut() {
            obj.remove("session_id");
        }
        let stripped = serde_json::to_string(&v).unwrap();
        let legacy: AgentConfig = serde_json::from_str(&stripped).unwrap();
        assert!(
            legacy.session_id.is_none(),
            "payload missing session_id must default to None"
        );
    }

    #[test]
    fn loop_passthrough_fields_default() {
        // issue #32: the three AgentLoopConfig passthrough fields default to
        // their no-op values, preserving pre-#32 behavior for consumers that
        // don't set them.
        let c = AgentConfig::default();
        assert!(c.max_tool_result_bytes.is_none());
        assert!(c.subagent_runner.is_none());
        assert_eq!(c.subagent_depth, 0);
    }

    #[test]
    fn loop_passthrough_fields_are_serde_skipped() {
        // issue #32: the passthrough fields are #[serde(skip, default)].
        // (1) They must NOT appear in serialized output — this is what lets
        //     the non-serializable `Arc<dyn SubagentRunner>` coexist with
        //     `#[derive(Serialize)]` on AgentConfig.
        // (2) Legacy payloads missing the keys must deserialize to defaults,
        //     so existing serialized configs are unaffected.
        let c = AgentConfig::new("m");
        let json = serde_json::to_string(&c).expect("serializes");
        assert!(!json.contains("max_tool_result_bytes"));
        assert!(!json.contains("subagent_runner"));
        assert!(!json.contains("subagent_depth"));

        let legacy: AgentConfig =
            serde_json::from_str(r#"{"name":"x","model_id":"m","timeout_seconds":300}"#)
                .expect("deserializes");
        assert!(legacy.max_tool_result_bytes.is_none());
        assert!(legacy.subagent_runner.is_none());
        assert_eq!(legacy.subagent_depth, 0);
    }

    #[test]
    fn loop_passthrough_fields_set_and_clone() {
        // issue #32 verification: consumers can set the passthrough fields
        // and they survive Clone (AgentConfig derives Clone).
        let c = AgentConfig {
            max_tool_result_bytes: Some(8192),
            subagent_depth: 3,
            ..AgentConfig::new("m")
        };
        let cloned = c.clone();
        assert_eq!(cloned.max_tool_result_bytes, Some(8192));
        assert_eq!(cloned.subagent_depth, 3);
        assert!(cloned.subagent_runner.is_none());
    }
}
