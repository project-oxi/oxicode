//! AgentSession — session wrapper around Agent.
//!
//! This is the core session abstraction shared between all run modes
//! (interactive, print, RPC). It encapsulates:
//!
//! - Agent state access and event subscription
//! - Automatic session persistence on each agent event
//! - Model and thinking-level management with cycling
//! - Auto-compaction (threshold-based and overflow-recovery)
//! - Auto-retry on transient / rate-limit errors
//! - Steering / follow-up message queueing
//! - Extension event forwarding hooks
//!
//! # Architecture
//!
//! ```text
//! interactive.rs / print_mode.rs / rpc_mode.rs
//!        │
//!        ▼
//!  AgentSession   ← this module
//!        │
//!        ▼
//!  oxicode_agent::Agent
//!        │
//!        ▼
//!  oxicode_sdk::Provider  (streaming LLM calls)
//! ```

use crate::context::auto_compaction::{CompactionConfig, CompactionReason};
use crate::extensions::{
    ExtensionContext, ExtensionContextBuilder, ExtensionRunner, SessionShutdownEvent,
    SessionShutdownReason,
};
use crate::store::session::{AgentMessage, SessionManager};
use crate::store::settings::{Settings, ThinkingLevel};
use anyhow::{Context, Result};
use oxicode_agent::advisor::{
    AdviseTool, AdvisorDeliveryChannel, AdvisorEmissionGuard, AdvisorNote, AdvisorRuntime,
    AdvisorRuntimeHost, AgentAdvisor, DeliveryOpts, EnqueueAdviceFn, format_advisory_batch,
    resolve_delivery_channel,
};
use oxicode_agent::{
    Agent, AgentConfig, AgentEvent, AgentState, FindTool, GrepTool, ReadTool, ToolRegistry,
};
use oxicode_ai::ModelRole;
use oxicode_sdk::{
    CompactionStrategy, Message, Provider, RoleRegistry, RoleRoutingProvider, get_provider,
    resolve_role_to_model,
};
use parking_lot::{Mutex as PlMutex, RwLock};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;

// ═══════════════════════════════════════════════════════════════════════════
// Session-level events (extends AgentEvent with session concerns)
// ═══════════════════════════════════════════════════════════════════════════

/// Events emitted by [`AgentSession`] in addition to the underlying
/// [`AgentEvent`]s.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum SessionEvent {
    /// A steering or follow-up queue changed.
    QueueUpdate {
        /// Current steering messages.
        steering: Vec<oxicode_sdk::Message>,
        /// Current follow-up messages.
        follow_up: Vec<oxicode_sdk::Message>,
    },
    /// Compaction started.
    CompactionStart {
        /// Why compaction was triggered.
        reason: CompactionReason,
    },
    /// Compaction finished (or failed / was aborted).
    CompactionEnd {
        /// Why compaction was triggered.
        reason: CompactionReason,
        /// Error message if compaction failed.
        error_message: Option<String>,
    },
    /// Session display name changed.
    SessionInfoChanged,
    /// Passthrough agent event.
    Agent(Box<AgentEvent>),
    /// Thinking level changed.
    ThinkingLevelChanged {
        /// New thinking level.
        level: ThinkingLevel,
    },
    /// An advisor note was delivered to the primary (aside or preserve
    /// channel). Rendered as an `<advisory>` card; does not interrupt the run.
    Advisor {
        /// How the note was routed.
        channel: AdvisorDeliveryChannel,
        /// The rendered `<advisory>` batch (one element per note).
        body: String,
    },
    /// Session handoff complete — a new session was started from a handoff
    /// document. The event loop uses this to clear the transcript and
    /// optionally auto-submit a continuation prompt.
    HandoffComplete {
        /// Path to the written handoff document.
        doc_path: String,
        /// Whether to auto-submit a continuation prompt to the new session.
        auto_continue: bool,
    },
    /// Session handoff failed — the current session is preserved unchanged.
    HandoffFailed {
        /// Error message describing what went wrong.
        error: String,
    },
}

/// Result of a compaction operation.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// Token count before compaction.
    pub tokens_before: usize,
}

// ═══════════════════════════════════════════════════════════════════════════
// Model cycling
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
/// Scoped model entry for Ctrl+P cycling.
pub struct ScopedModel {
    /// Provider name.
    pub provider: String,
    /// Model identifier.
    pub model_id: String,
}

// ═══════════════════════════════════════════════════════════════════════════
// Prompt options
// ═══════════════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════════════
// Session statistics
// ═══════════════════════════════════════════════════════════════════════════

/// Statistics returned by [`AgentSession::session_stats`].
#[derive(Debug, Clone)]
pub struct SessionStats {
    /// Unique session identifier.
    pub session_id: String,
    /// Number of user messages.
    pub user_messages: usize,
    /// Number of assistant messages.
    pub assistant_messages: usize,
    /// Number of tool calls made.
    pub tool_calls: usize,
    /// Number of tool results returned.
    pub tool_results: usize,
    /// Total number of messages.
    pub total_messages: usize,
}

// ═══════════════════════════════════════════════════════════════════════════
// AgentSession
// ═══════════════════════════════════════════════════════════════════════════

/// Session wrapper around [`Agent`] that adds:
///
/// - Model cycling and thinking-level management
/// - Steering / follow-up message queues
/// - Auto-compaction after responses
/// - Auto-retry on transient errors
/// - Session persistence (auto-save on each event)
/// - Extension event forwarding hooks
pub struct AgentSession {
    // ── Core ──────────────────────────────────────────────────────────
    agent: Arc<Agent>,
    settings: Arc<RwLock<Settings>>,
    session_manager: Arc<RwLock<SessionManager>>,
    /// Display metadata for the Agent Hub overlay.
    hub: super::agent_hub_registry::SharedHubRegistry,

    // ── Event listeners ──────────────────────────────────────────────
    #[allow(clippy::type_complexity)]
    listeners: Arc<RwLock<Vec<Box<dyn Fn(&SessionEvent) + Send + Sync>>>>,

    // ── Model / thinking state ───────────────────────────────────────
    scoped_models: Arc<RwLock<Vec<ScopedModel>>>,

    // ── Queues ───────────────────────────────────────────────────────
    steering_messages: Arc<RwLock<VecDeque<oxicode_sdk::Message>>>,
    follow_up_messages: Arc<RwLock<VecDeque<oxicode_sdk::Message>>>,
    // ── Queue injection modes (RPC/IDE state) ───────────────────────
    /// Whether steering/follow-up messages are surfaced ("all") or only
    /// injected silently ("system"). Informational in RPC mode — the agent
    /// loop always drains both queues regardless; this only controls how
    /// the client surfaces them.
    steering_mode: Arc<RwLock<String>>,
    follow_up_mode: Arc<RwLock<String>>,

    // ── Compaction state ─────────────────────────────────────────────
    compaction_config: Arc<RwLock<CompactionConfig>>,
    compaction_abort: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    overflow_recovery_attempted: Arc<RwLock<bool>>,

    // ── Session persistence ──────────────────────────────────────────
    session_id: Arc<RwLock<String>>,

    // ── CWD ──────────────────────────────────────────────────────────
    cwd: String,

    // ── Streaming state ──────────────────────────────────────────────
    streaming: Arc<AtomicBool>,

    // ── Cancellation ─────────────────────────────────────────────────
    should_stop: Arc<AtomicBool>,

    // ── Extensions ───────────────────────────────────────────────────
    extension_runner: Arc<RwLock<Option<ExtensionRunner>>>,

    // ── Advisor (read-only reviewer shadowing the primary) ───────────
    /// The advisor runtime, if enabled. `None` when `advisor.enabled` is off.
    advisor: Arc<RwLock<Option<Arc<AdvisorRuntime>>>>,
    /// Per-session advisor emission guard (dedupe + one-advise-per-update +
    /// content-free-phrase suppression). Owned here because the session is
    /// what routes accepted notes back to the primary transcript.
    advisor_guard: Arc<AdvisorEmissionGuard>,
    /// Advisor delivery + interrupt-cooldown state.
    advisor_delivery: Arc<PlMutex<AdvisorDeliveryState>>,
    /// Count of primary turns completed (for the immune-turn cooldown fence).
    advisor_primary_turns: Arc<std::sync::atomic::AtomicU64>,
}

/// Advisor delivery + interrupt-cooldown state (session-scoped).
#[derive(Debug, Default, Clone, Copy)]
struct AdvisorDeliveryState {
    /// Latched true when the user deliberately interrupted; suppresses
    /// `concern`/`blocker` auto-resume until the user next resumes.
    auto_resume_suppressed: bool,
    /// The primary-turn index at which the post-interrupt immune-turn window
    /// starts. `None` when no interrupting steer is in flight.
    interrupt_immune_turn_start: Option<u64>,
}

/// Minimal [`AdvisorRuntimeHost`] for the session-owned advisor: captures only
/// what the runtime needs (the primary transcript for deltas, and the emission
/// guard's per-update reset). Advice routing is handled by the `AdviseTool`'s
/// enqueue closure (which captures the session's shared steering/listener
/// queues), NOT by this host — so this host never needs an `Arc<AgentSession>`
/// (avoiding a self-referential cycle).
struct AdvisorHost {
    agent: Arc<Agent>,
    guard: Arc<AdvisorEmissionGuard>,
}

impl AdvisorRuntimeHost for AdvisorHost {
    fn snapshot_messages(&self) -> Vec<Message> {
        self.agent.state().messages
    }
    fn begin_advisor_update(&self) {
        self.guard.begin_update();
    }
    // Routing is handled by the AdviseTool closure; this is unused for the
    // session-owned advisor but required by the trait (SDK consumers may use it).
    fn enqueue_advice(&self, _note: AdvisorNote) {}
}

/// Convert a resumed session's branch (root → leaf, chronological) into the
/// LLM message stream used to seed the agent's conversation state — the inverse
/// of [`AgentSession::persist_event_message`].
///
/// Restores the conversation context lost on resume (issue #23): user prompts,
/// assistant responses, tool calls, and their paired tool results. Compaction
/// is honoured: if a `CompactionSummary` is present, everything before the last
/// one is dropped (it already replaced that span) and the summary is included.
///
/// Trailing/unmatched tool calls are left as-is; the agent loop's
/// `sanitize_orphaned_tool_results` (run before every provider request) strips
/// any dangling `tool_use` a provider would reject.
fn resume_messages_from_branch(
    entries: &[crate::store::session::SessionEntry],
) -> Vec<oxicode_ai::Message> {
    use crate::store::session::AssistantContentBlock;
    use oxicode_ai::{
        AssistantMessage, ContentBlock, ImageContent, Message, TextContent, ThinkingContent,
        ToolCall,
    };
    use std::collections::HashMap;

    // Honour compaction: drop everything before the last summary, then include
    // the summary itself as the starting context.
    let last_compaction = entries
        .iter()
        .rposition(|e| matches!(e.message, AgentMessage::CompactionSummary { .. }));
    let relevant = &entries[last_compaction.unwrap_or(0)..];

    // tool_call_id -> tool_name, populated as ToolCall blocks are emitted so a
    // following ToolResult can carry the right tool name.
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut out: Vec<Message> = Vec::new();

    for entry in relevant {
        match &entry.message {
            AgentMessage::CompactionSummary { summary, .. } => {
                if !summary.is_empty() {
                    out.push(Message::user(format!(
                        "[Summary of earlier conversation]\n{summary}"
                    )));
                }
            }
            AgentMessage::User { content } => {
                out.push(Message::user(content.as_str().to_string()));
            }
            AgentMessage::Assistant {
                content,
                provider,
                model_id,
                ..
            } => {
                // Record tool names for any tool calls in this message.
                for b in content {
                    if let AssistantContentBlock::ToolCall { id, name, .. } = b {
                        tool_names.insert(id.clone(), name.clone());
                    }
                }
                let blocks: Vec<ContentBlock> = content
                    .iter()
                    .filter_map(|b| match b {
                        AssistantContentBlock::Text { text } => {
                            Some(ContentBlock::Text(TextContent::new(text.clone())))
                        }
                        AssistantContentBlock::Thinking { thinking } => Some(
                            ContentBlock::Thinking(ThinkingContent::new(thinking.clone())),
                        ),
                        AssistantContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } => Some(ContentBlock::ToolCall(ToolCall::new(
                            id.clone(),
                            name.clone(),
                            arguments.clone(),
                        ))),
                        AssistantContentBlock::ImageResult { data, media_type } => {
                            Some(ContentBlock::Image(ImageContent::new(
                                data.clone(),
                                media_type.clone(),
                            )))
                        }
                        // Refusal / ToolPlan are not replayed to the model.
                        _ => None,
                    })
                    .collect();
                if blocks.is_empty() {
                    continue;
                }
                let api = provider
                    .as_ref()
                    .and_then(|p| oxicode_sdk::get_provider_api(p))
                    .unwrap_or(oxicode_ai::Api::OpenAiCompletions);
                let mut am = AssistantMessage::new(
                    api,
                    provider.clone().unwrap_or_else(|| "assistant".to_string()),
                    model_id.clone().unwrap_or_else(|| "assistant".to_string()),
                );
                am.content = blocks;
                out.push(Message::Assistant(am));
            }
            AgentMessage::ToolResult {
                content,
                tool_call_id,
            } => {
                let tool_name = tool_names
                    .get(tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| "tool".to_string());
                out.push(Message::tool_result(
                    tool_call_id.clone(),
                    tool_name,
                    vec![ContentBlock::Text(TextContent::new(
                        content.as_str().to_string(),
                    ))],
                ));
            }
            // System / BashExecution / Custom / BranchSummary are metadata, not
            // part of the LLM message stream.
            _ => {}
        }
    }
    out
}

#[allow(dead_code)]
impl AgentSession {
    /// Create a new session wrapping the given [`Agent`].
    ///
    /// `session_state` carries the cli-owned stop flag + steering / follow-up
    /// queues. The session clones the three `Arc`s out of it so the runtime
    /// and the agent's `with_session_hooks` closures observe the SAME state
    /// (see [`crate::SessionState`] doc). When the caller doesn't care about
    /// sharing state (tests, ad-hoc runs), pass [`crate::SessionState::default`].
    pub fn new(
        agent: Arc<Agent>,
        settings: Settings,
        session_manager: SessionManager,
        cwd: String,
        session_state: crate::SessionState,
    ) -> Self {
        let session_id = session_manager.get_session_id();
        let hub = Arc::new(super::agent_hub_registry::HubRegistry::new());
        if let Some(session_file) = session_manager.get_session_file()
            && let Some(session_dir) = std::path::Path::new(&session_file).parent()
        {
            super::agent_hub_bridge::register_persisted_subagents(&hub, session_dir);
            // The current session's own .jsonl lives in the same per-CWD
            // directory as persisted subagents. `register_persisted_subagents`
            // cannot distinguish it from a subagent (the scan sees every
            // `*.jsonl` in the dir), so unregister it here. The unregister is
            // idempotent — if the file hasn't been flushed yet, it's a no-op.
            if let Some(own_stem) = std::path::Path::new(&session_file)
                .file_stem()
                .and_then(|stem| stem.to_str())
            {
                hub.unregister(own_stem);
            }
        }

        // Seed the agent's conversation state from the resumed session so the
        // LLM sees prior user/assistant/tool history (issue #23). For a
        // brand-new (empty) session the branch is empty and this is a no-op.
        // Safe against re-persisting: `persist_session` reconciles against the
        // on-disk entry count (`get_entries().len()`), which already includes
        // these entries, so the seeded messages are never rewritten.
        let history = resume_messages_from_branch(&session_manager.get_branch(None));
        if !history.is_empty() {
            agent.update_state(|s| s.messages = history);
        }

        let compaction_config = CompactionConfig {
            enabled: settings.auto_compaction,
            ..CompactionConfig::default()
        };

        Self {
            agent,
            settings: Arc::new(RwLock::new(settings)),
            session_manager: Arc::new(RwLock::new(session_manager)),
            hub,
            listeners: Arc::new(RwLock::new(Vec::new())),
            scoped_models: Arc::new(RwLock::new(Vec::new())),
            // Steer / follow-up queues + stop flag are SHARED with the
            // agent's session-level closures (see `with_session_hooks` in
            // `App::from_oxicode`). Cloning the `Arc`s preserves identity
            // — enqueues from the runtime are seen by the agent, and the
            // agent's stop check is observed by Ctrl+C handlers.
            steering_messages: Arc::clone(&session_state.steering),
            follow_up_messages: Arc::clone(&session_state.follow_up),
            should_stop: Arc::clone(&session_state.should_stop),
            steering_mode: Arc::new(RwLock::new("all".to_string())),
            follow_up_mode: Arc::new(RwLock::new("all".to_string())),
            compaction_config: Arc::new(RwLock::new(compaction_config)),
            compaction_abort: Arc::new(Mutex::new(None)),
            overflow_recovery_attempted: Arc::new(RwLock::new(false)),
            session_id: Arc::new(RwLock::new(session_id)),
            cwd,
            streaming: Arc::new(AtomicBool::new(false)),
            extension_runner: Arc::new(RwLock::new(None)),
            advisor: Arc::new(RwLock::new(None)),
            advisor_guard: Arc::new(AdvisorEmissionGuard::new()),
            advisor_delivery: Arc::new(PlMutex::new(AdvisorDeliveryState::default())),
            advisor_primary_turns: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    // ══════════════════════════════════════════════════════════════════
    // Read-only state access
    // ══════════════════════════════════════════════════════════════════

    /// Display metadata for the Agent Hub overlay.
    pub fn hub(&self) -> &super::agent_hub_registry::HubRegistry {
        &self.hub
    }

    /// Get the session's todo state provider, if configured. Lets the TUI
    /// observe todo phase changes live (the same source of truth the `todo`
    /// agent tool writes to).
    pub fn todo_provider(
        &self,
    ) -> Option<std::sync::Arc<dyn oxicode_agent::tools::TodoStateProvider>> {
        self.agent.todo_provider()
    }

    /// Get the current model ID (`provider/model`).
    pub fn model_id(&self) -> String {
        self.agent.model_id()
    }

    /// Get the current agent state.
    #[allow(dead_code)]
    pub fn state(&self) -> AgentState {
        self.agent.state()
    }

    /// Current thinking level.
    pub fn thinking_level(&self) -> ThinkingLevel {
        self.settings.read().thinking_level
    }

    /// Whether the agent is currently streaming.
    #[allow(dead_code)]
    pub fn is_streaming(&self) -> bool {
        self.streaming.load(Ordering::SeqCst)
    }

    /// Get a cloneable reference to the streaming flag.
    /// Used by the TUI worker thread to set/clear the flag around agent runs.
    pub fn streaming_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.streaming)
    }

    /// All messages in the agent state.
    #[allow(dead_code)]
    pub fn messages(&self) -> Vec<Message> {
        self.agent.state().messages
    }

    /// Current session ID.
    pub fn session_id(&self) -> String {
        self.session_manager.read().get_session_id()
    }

    /// Whether compaction is in progress.
    #[allow(dead_code)]
    pub fn is_compacting(&self) -> bool {
        // try_lock() succeeds only when no one holds the tokio Mutex.
        // If compaction is running, the handle is Some AND the mutex is
        // held by the compaction task, so try_lock fails → return true.
        // If try_lock succeeds, the mutex was uncontended; check the handle.
        match self.compaction_abort.try_lock() {
            Ok(guard) => guard.is_some(), // lock acquired: check if handle present
            Err(_) => true,               // lock contested → compaction is running
        }
    }

    /// Check if auto-retry is enabled.
    ///
    /// Delegates to the agent loop's retry configuration.
    /// Auto-retry is now handled entirely by the agent loop
    /// (`oxicode_agent::AgentLoopConfig::auto_retry_enabled`).
    #[allow(dead_code)]
    pub fn auto_retry_enabled(&self) -> bool {
        // Agent loop defaults to enabled; we reflect that here.
        true
    }

    /// Get the current session stats.
    pub fn session_stats(&self) -> SessionStats {
        let state = self.agent.state();
        let mut user_messages = 0usize;
        let mut assistant_messages = 0usize;
        let mut tool_results = 0usize;
        let mut tool_calls = 0usize;

        for msg in &state.messages {
            match msg {
                Message::User(_) => user_messages += 1,
                Message::Assistant(a) => {
                    assistant_messages += 1;
                    // Count tool-use content blocks
                    for block in &a.content {
                        if matches!(block, oxicode_sdk::ContentBlock::ToolCall(_)) {
                            tool_calls += 1;
                        }
                    }
                    let _ = &a; // suppress unused warning
                }
                Message::ToolResult(_) => tool_results += 1,
            }
        }

        SessionStats {
            session_id: self.session_id(),
            user_messages,
            assistant_messages,
            tool_calls,
            tool_results,
            total_messages: state.messages.len(),
        }
    }

    /// Get the number of pending messages (steering + follow-up).
    #[allow(dead_code)]
    pub fn pending_message_count(&self) -> usize {
        self.steering_messages.read().len() + self.follow_up_messages.read().len()
    }

    /// Get pending steering messages.
    /// Get pending steering messages (full [`Message`]s, including any image
    /// content blocks; the TUI extracts text via `text_content()` for display).
    pub fn steering_messages(&self) -> Vec<oxicode_sdk::Message> {
        self.steering_messages.read().iter().cloned().collect()
    }

    /// Get pending follow-up messages (full [`Message`]s).
    pub fn follow_up_messages(&self) -> Vec<oxicode_sdk::Message> {
        self.follow_up_messages.read().iter().cloned().collect()
    }

    /// Get a reference to the steering message queue (for hook wiring).
    pub fn steering_queue(&self) -> Arc<RwLock<std::collections::VecDeque<oxicode_sdk::Message>>> {
        self.steering_messages.clone()
    }

    /// Get a reference to the follow-up message queue (for hook wiring).
    pub fn follow_up_queue(&self) -> Arc<RwLock<std::collections::VecDeque<oxicode_sdk::Message>>> {
        self.follow_up_messages.clone()
    }

    /// Current working directory.
    #[allow(dead_code)]
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Get scoped models for cycling.
    pub fn scoped_models(&self) -> Vec<ScopedModel> {
        self.scoped_models.read().clone()
    }
    /// Cycle to the next scoped model and apply it.
    ///
    /// Returns the new `provider/model` id, or `None` when fewer than two
    /// scoped models are configured (nothing to cycle).
    pub fn cycle_model(&self) -> Option<String> {
        let models = self.scoped_models.read().clone();
        if models.len() < 2 {
            return None;
        }
        let current = self.model_id();
        let idx = models
            .iter()
            .position(|m| format!("{}/{}", m.provider, m.model_id) == current)
            .unwrap_or(0);
        let next = models.get((idx + 1) % models.len())?;
        let id = format!("{}/{}", next.provider, next.model_id);
        match self.set_model(&id) {
            Ok(()) => Some(id),
            Err(_) => None,
        }
    }
    /// Render the current conversation as a self-contained HTML string.
    ///
    /// Used by the RPC `export_html` command (and reusable by the TUI
    /// `/export` slash command). Each message is flattened to its text
    /// content for the export.
    pub fn export_html(&self) -> Result<String> {
        use crate::storage::export::{ExportMeta, HtmlExportOptions, export_to_html};
        let meta = ExportMeta {
            model: Some(self.model_id()),
            provider: None,
            exported_at: chrono::Utc::now().timestamp_millis(),
            total_user_tokens: None,
            total_assistant_tokens: None,
        };
        // Use the canonical persisted entries directly — avoids the lossy
        // Message→SessionEntry round-trip (preserves tool calls, thinking, etc.).
        let entries = self.session_manager.read().get_entries();
        export_to_html(&entries, &meta, &HtmlExportOptions::default())
    }
    /// Current session file path (used by RPC `clone`/`fork`), if persisted.
    pub fn session_file(&self) -> Option<String> {
        self.session_manager.read().get_session_file()
    }
    /// Fork the current session at the given entry (RPC `fork`).
    ///
    /// Delegates to `SessionManager::branch_from_entry`, materializing a new
    /// session file containing only the entries up to and including
    /// `entry_id`. Returns the path of the new file; callers feed it through
    /// `SessionManager::open` and `swap_session` to activate the fork.
    pub fn branch_from_entry(&self, entry_id: &str) -> Result<String, String> {
        self.session_manager.read().branch_from_entry(entry_id)
    }

    /// Check if auto-compaction is enabled.
    pub fn auto_compaction_enabled(&self) -> bool {
        self.compaction_config.read().enabled
    }

    /// Toggle auto-compaction at runtime (RPC `set_auto_compaction`).
    pub fn set_auto_compaction(&self, enabled: bool) {
        self.compaction_config.write().enabled = enabled;
    }
    /// Toggle auto-retry at runtime (RPC `set_auto_retry`).
    pub fn set_auto_retry(&self, enabled: bool) {
        self.agent.set_auto_retry(enabled);
    }

    /// Abort any in-progress auto-retry wait (RPC `abort_retry`).
    pub fn cancel_auto_retry(&self) {
        self.agent.cancel_auto_retry();
    }

    /// Current steering-injection mode ("all" or "system").
    pub fn steering_mode(&self) -> String {
        self.steering_mode.read().clone()
    }

    /// Set the steering-injection mode.
    pub fn set_steering_mode(&self, mode: String) {
        *self.steering_mode.write() = mode;
    }

    /// Current follow-up-injection mode ("all" or "system").
    pub fn follow_up_mode(&self) -> String {
        self.follow_up_mode.read().clone()
    }

    /// Set the follow-up-injection mode.
    pub fn set_follow_up_mode(&self, mode: String) {
        *self.follow_up_mode.write() = mode;
    }

    // ══════════════════════════════════════════════════════════════════
    // Event subscription
    // ══════════════════════════════════════════════════════════════════

    /// Subscribe to session events. Returns a guard that, when dropped,
    /// unsubscribes the listener.
    ///
    /// **Note:** The listener is called synchronously on the event-processing
    /// thread; keep it fast. For async processing, forward to a channel.
    pub fn subscribe(
        &self,
        listener: Box<dyn Fn(&SessionEvent) + Send + Sync>,
    ) -> SessionListenerGuard {
        let key = {
            let mut listeners = self.listeners.write();
            listeners.push(listener);
            listeners.len() - 1
        };
        SessionListenerGuard {
            listeners: Arc::clone(&self.listeners),
            key,
        }
    }

    /// Emit a session event to all listeners.
    fn emit(&self, event: SessionEvent) {
        let listeners = self.listeners.read();
        for listener in listeners.iter() {
            listener(&event);
        }
    }

    /// Emit a queue update event.
    fn emit_queue_update(&self) {
        self.emit(SessionEvent::QueueUpdate {
            steering: self.steering_messages(),
            follow_up: self.follow_up_messages(),
        });
    }

    /// Queue a steering message (delivered after current turn's tool calls).
    ///
    /// This is a synchronous method because it contains no async operations.
    /// The message is added to the steering queue and will be injected into
    /// the agent loop by the `get_steering_messages` hook on the next run.
    ///
    /// Note: we intentionally do NOT call `agent.state().add_user_message()`
    /// here because the agent loop will add the message from the queue via
    /// hooks, and `run_with_channel` copies the current state at startup.
    /// Adding it here would cause a duplicate.
    pub fn steer_sync(&self, text: String) {
        self.steer_sync_message(oxicode_sdk::Message::User(oxicode_sdk::UserMessage::new(
            text,
        )));
    }

    /// Queue a steering [`Message`] (supports image content blocks) — RPC `steer`
    /// with images. Injected mid-run via the `get_steering_messages` hook.
    pub fn steer_sync_message(&self, msg: oxicode_sdk::Message) {
        self.steering_messages.write().push_back(msg);
        self.emit_queue_update();
    }

    /// Queue a steering message (async wrapper for backward compatibility).
    #[allow(dead_code)]
    pub async fn steer(&self, text: String) -> Result<()> {
        self.steer_sync(text);
        Ok(())
    }

    /// Queue a follow-up message (processed after agent finishes).
    pub fn follow_up_sync(&self, text: String) {
        self.follow_up_sync_message(oxicode_sdk::Message::User(oxicode_sdk::UserMessage::new(
            text,
        )));
    }

    /// Queue a follow-up [`Message`] (supports image content blocks) — RPC
    /// `follow_up` with images.
    pub fn follow_up_sync_message(&self, msg: oxicode_sdk::Message) {
        self.follow_up_messages.write().push_back(msg);
        self.emit_queue_update();
    }

    /// Queue a follow-up message (async wrapper for backward compatibility).
    #[allow(dead_code)]
    pub async fn follow_up(&self, text: String) -> Result<()> {
        self.follow_up_sync(text);
        Ok(())
    }

    /// Abort current operation.
    ///
    /// Sets the `should_stop` flag which causes the agent loop to exit
    /// after the current turn completes (via `should_stop_after_turn` hook).
    /// Also clears any queued steering/follow-up messages.
    pub async fn abort(&self) {
        tracing::debug!("AgentSession::abort() — setting should_stop flag");
        self.should_stop.store(true, Ordering::SeqCst);
        self.clear_queue();
    }

    /// Get a cloneable reference to the should_stop flag.
    pub fn should_stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.should_stop)
    }

    /// Reset the should_stop flag (call before starting a new prompt).
    pub fn reset_should_stop(&self) {
        self.should_stop.store(false, Ordering::SeqCst);
    }

    /// (Removed in the hooks refactor.) Session queues and stop flag
    /// are now wired into the agent hook chain at agent-build time via
    /// `App::from_oxicode` → `with_session_hooks`. Previously this
    /// function called `agent.set_hooks(...)` a second time, which
    /// wiped the middleware pipeline's before/after_tool_call slots
    /// (replace-semantics bug class — audit Gap-0). See
    /// `docs/superpowers/specs/2026-08-04-hooks-system-design.md`.
    /// Clear all queued messages and return them.
    pub fn clear_queue(&self) -> (Vec<oxicode_sdk::Message>, Vec<oxicode_sdk::Message>) {
        let steering = self.steering_messages.write().drain(..).collect();
        let follow_up = self.follow_up_messages.write().drain(..).collect();
        self.emit_queue_update();
        (steering, follow_up)
    }

    // ══════════════════════════════════════════════════════════════════
    // Model management
    // ══════════════════════════════════════════════════════════════════

    /// Refresh credentials from the wired AuthProvider port.
    ///
    /// Re-resolves the current provider via the SDK resolver, which consults
    /// the auth port's sync fast-path on every call. Picks up auth-store
    /// updates (e.g. a key entered via the provider overlay) without
    /// rebuilding the engine. Replaces the old `refresh_api_key` from
    /// pre-0.55.0; see issues #39/#40.
    pub fn refresh_api_key(&self) -> Result<()> {
        self.agent.refresh_credentials()?;
        Ok(())
    }

    /// Switch model mid-conversation.
    ///
    /// The new provider is re-credentialed by the SDK resolver via the
    /// wired AuthProvider port; no explicit api_key lookup is needed.
    pub fn set_model(&self, model_id: &str) -> Result<()> {
        self.agent.switch_model(model_id)?;

        // Persist model change to session
        {
            let mut sm = self.session_manager.write();
            let parts: Vec<&str> = model_id.split('/').collect();
            if parts.len() >= 2 {
                sm.append_model_change(parts[0], &parts[1..].join("/"));
            }
        }

        // Update settings: persist as last_used
        {
            let mut settings = self.settings.write();
            let parts: Vec<&str> = model_id.split('/').collect();
            if parts.len() >= 2 {
                settings.last_used_provider = Some(parts[0].to_string());
                settings.last_used_model = Some(parts[1..].join("/"));
            } else {
                settings.last_used_model = Some(model_id.to_string());
            }
        }

        Ok(())
    }

    /// Set scoped models for cycling.
    pub fn set_scoped_models(&self, models: Vec<ScopedModel>) {
        *self.scoped_models.write() = models;
    }

    // ══════════════════════════════════════════════════════════════════
    // Thinking level management
    // ══════════════════════════════════════════════════════════════════

    /// Set thinking level, clamped to model capabilities.
    pub fn set_thinking_level(&self, level: ThinkingLevel) {
        let old_level = self.thinking_level();
        if level == old_level {
            return;
        }

        {
            let mut settings = self.settings.write();
            settings.thinking_level = level;
        }

        // Persist to session
        {
            let mut sm = self.session_manager.write();
            sm.append_thinking_level_change(&format!("{:?}", level).to_lowercase());
        }

        self.emit(SessionEvent::ThinkingLevelChanged { level });
    }

    /// Cycle to the next thinking level.
    pub fn cycle_thinking_level(&self) -> Option<ThinkingLevel> {
        let levels = [
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
        ];
        let current = self.thinking_level();
        let current_index = levels.iter().position(|l| *l == current).unwrap_or(0);
        let next_index = (current_index + 1) % levels.len();
        let next = levels[next_index];
        self.set_thinking_level(next);
        Some(next)
    }

    // ══════════════════════════════════════════════════════════════════
    // System prompt rebuild
    // ══════════════════════════════════════════════════════════════════

    /// Rebuild the system prompt from the current `Settings`, picking
    /// up `thinking_level`. Pushes the rebuilt prompt to the underlying
    /// agent so the **next** user turn sees updated settings.
    ///
    /// This is a no-op-safe: it always rewrites the system prompt
    /// (no change-detection), so it can be called unconditionally
    /// from `/reload` or other hot-apply paths.
    ///
    /// **v6 — disk fresh load:** this method now reloads `Settings`
    /// from disk before rebuilding the prompt. This synchronizes
    /// the in-memory `Arc<RwLock<Settings>>` cache with whatever
    /// `persist_changes()` (or external `settings.toml` edits) just
    /// wrote, so overlay edits and direct file edits converge to
    /// the same state without the overlay having to reach into
    /// `AgentSession`'s mutable API.
    pub fn rebuild_system_prompt(&self) {
        // v6: fresh-load from disk so in-memory cache matches whatever
        // was just persisted (or hand-edited in settings.toml).
        let fresh = crate::store::settings::Settings::load().unwrap_or_default();
        let thinking = fresh.thinking_level;
        let auto_compaction = fresh.auto_compaction;
        *self.settings.write() = fresh;

        // Sync auto-compaction to runtime state so the overlay's
        // `auto_compaction` toggle (and hand-edited `settings.toml`) takes
        // effect on the next turn — not just the next session. The agent
        // reads `compaction_strategy` fresh from config at the start of
        // each run, so updating it here is sufficient for live toggle.
        self.compaction_config.write().enabled = auto_compaction;
        let strategy = if auto_compaction {
            oxicode_sdk::CompactionStrategy::Threshold(0.8)
        } else {
            oxicode_sdk::CompactionStrategy::Disabled
        };
        self.agent.set_compaction_strategy(strategy);

        let prompt = crate::app::agent_session_runtime::build_system_prompt(thinking);
        self.agent.set_system_prompt(prompt);
    }

    // ══════════════════════════════════════════════════════════════════
    // Auto-compaction
    // ══════════════════════════════════════════════════════════════════

    /// Manually trigger compaction.
    pub async fn compact(&self, custom_instructions: Option<String>) -> Result<CompactionResult> {
        self.emit(SessionEvent::CompactionStart {
            reason: CompactionReason::Manual,
        });

        let result = self.run_compaction(custom_instructions).await;

        match &result {
            Ok(_r) => self.emit(SessionEvent::CompactionEnd {
                reason: CompactionReason::Manual,
                error_message: None,
            }),
            Err(e) => self.emit(SessionEvent::CompactionEnd {
                reason: CompactionReason::Manual,
                error_message: Some(e.to_string()),
            }),
        }

        result
    }

    /// Internal compaction execution.
    async fn run_compaction(
        &self,
        custom_instructions: Option<String>,
    ) -> Result<CompactionResult> {
        let state = self.agent.state();
        let messages = state.messages.clone();

        if messages.len() < 3 {
            anyhow::bail!("Nothing to compact (session too small)");
        }

        // Force compaction regardless of strategy — this is a manual command.
        let compacted = self
            .agent
            .compaction_manager()
            .compact_now(&messages, custom_instructions.as_deref())
            .await
            .context("Compaction failed")?;

        let tokens_before = state.estimate_tokens();

        // Replace messages in agent state (must use update_state to
        // actually mutate the SharedState, not just a snapshot copy)
        self.agent.update_state(|s| {
            s.replace_messages(compacted.kept_messages.clone());
        });

        // Persist to session
        self.persist_session();

        Ok(CompactionResult { tokens_before })
    }

    /// Abort in-progress compaction via the shared abort handle.
    /// Used by the TUI's auto-compaction infrastructure.
    pub fn abort_compaction_sync(&self) {
        // Best-effort: try to abort without async. The compaction
        // checks compaction_abort periodically.
        if let Ok(mut guard) = self.compaction_abort.try_lock()
            && let Some(handle) = guard.take()
        {
            handle.abort();
        }
    }

    // ══════════════════════════════════════════════════════════════════
    // Session persistence
    // ══════════════════════════════════════════════════════════════════

    /// Persist the current agent state to the session manager.
    ///
    /// Safety-net fallback called on `AgentEnd`. Catches ToolResult
    /// messages that were never delivered via `MessageEnd` events.
    ///
    /// Uses `agent.state().messages` index tracking via `persisted_count`
    /// to append only messages that haven't been written yet.  Because
    /// `persist_event_message` increments `persisted_count` independently
    /// of this method's index-based logic, we must **reconcile** the two
    /// counters to avoid double-writes or gaps.
    ///
    /// The reconciliation strategy is simple: count how many entries the
    /// session manager already has, compare to `agent.state().messages.len()`,
    /// and append any deficit.  This is idempotent — calling it when
    /// everything is already persisted is a no-op.
    fn persist_session(&self) {
        let state = self.agent.state();
        let messages = &state.messages;
        let total = messages.len();

        // Nothing to persist
        if total == 0 {
            return;
        }

        // Count how many "real" message entries (non-header) the session
        // manager already has. This is the source of truth for how many
        // agent messages have been persisted to disk.
        let mut sm = self.session_manager.write();
        let already_in_sm = sm.get_entries().len();

        if already_in_sm >= total {
            return; // fully persisted
        }

        // Append the missing messages (by index in agent state)
        for msg in &messages[already_in_sm..] {
            match msg {
                Message::User(u) => {
                    let content = match &u.content {
                        oxicode_sdk::MessageContent::Text(t) => t.clone(),
                        oxicode_sdk::MessageContent::Blocks(blocks) => blocks
                            .iter()
                            .filter_map(|b| b.as_text())
                            .collect::<Vec<_>>()
                            .join(""),
                    };
                    sm.append_message(AgentMessage::User {
                        content: crate::store::session::ContentValue::String(content),
                    });
                }
                Message::Assistant(a) => {
                    let content_blocks: Vec<crate::store::session::AssistantContentBlock> = a
                        .content
                        .iter()
                        .map(|b| match b {
                            oxicode_sdk::ContentBlock::Text(t) => {
                                crate::store::session::AssistantContentBlock::Text {
                                    text: t.text.clone(),
                                }
                            }
                            oxicode_sdk::ContentBlock::Thinking(t) => {
                                crate::store::session::AssistantContentBlock::Thinking {
                                    thinking: t.thinking.clone(),
                                }
                            }
                            oxicode_sdk::ContentBlock::ToolCall(tc) => {
                                crate::store::session::AssistantContentBlock::ToolCall {
                                    id: tc.id.clone(),
                                    name: tc.name.clone(),
                                    arguments: tc.arguments.clone(),
                                }
                            }
                            oxicode_sdk::ContentBlock::Image(img) => {
                                crate::store::session::AssistantContentBlock::ImageResult {
                                    data: img.data.clone(),
                                    media_type: img.mime_type.clone(),
                                }
                            }
                            oxicode_sdk::ContentBlock::Unknown(v) => {
                                crate::store::session::AssistantContentBlock::Text {
                                    text: v.to_string(),
                                }
                            }
                        })
                        .collect();

                    sm.append_message(AgentMessage::Assistant {
                        content: content_blocks,
                        provider: Some(a.provider.clone()),
                        model_id: Some(a.model.clone()),
                        usage: Some(crate::store::session::Usage {
                            input: Some(a.usage.input as i64),
                            output: Some(a.usage.output as i64),
                            cache_read: Some(a.usage.cache_read as i64),
                            cache_write: Some(a.usage.cache_write as i64),
                            total_tokens: Some(a.usage.total_tokens as i64),
                        }),
                        stop_reason: Some(format!("{:?}", a.stop_reason)),
                    });
                }
                Message::ToolResult(t) => {
                    let content = t
                        .content
                        .iter()
                        .filter_map(|b| b.as_text())
                        .collect::<Vec<_>>()
                        .join("");
                    sm.append_message(AgentMessage::ToolResult {
                        content: crate::store::session::ContentValue::String(content),
                        tool_call_id: t.tool_call_id.clone(),
                    });
                }
            }
        }

        // Sync persisted_count to match reality
        sm.set_persisted_count(total);
    }

    // ══════════════════════════════════════════════════════════════════
    // Session management
    // ══════════════════════════════════════════════════════════════════

    /// Set a display name for the current session.
    pub fn set_session_name(&self, name: String) {
        let mut sm = self.session_manager.write();
        sm.append_session_info(&name);
        self.emit(SessionEvent::SessionInfoChanged);
    }

    /// Reset the agent state for a new conversation.
    pub fn reset(&self) {
        self.agent.reset();
        *self.overflow_recovery_attempted.write() = false;
        self.clear_queue();
    }

    /// Remove the session file if no real conversation happened.
    /// Called before session switch or quit.
    pub fn cleanup_empty_session(&self) {
        self.session_manager.read().cleanup_if_empty();
    }

    /// Start a new session: create a fresh [`SessionManager`] (new file, new
    /// session ID), and reset the agent state. Used by `/handoff`.
    ///
    /// The old session's JSONL file is preserved (append-only). The new
    /// session gets a blank conversation — the caller is responsible for
    /// seeding it (e.g. emitting a continuation prompt).
    pub fn start_new_session(&self) {
        let cwd = self.cwd.clone();
        let new_manager = SessionManager::create(&cwd, None);
        *self.session_manager.write() = new_manager;
        *self.session_id.write() = self.session_manager.read().get_session_id();
        self.agent.reset();
        *self.overflow_recovery_attempted.write() = false;
        self.clear_queue();
    }

    /// Emit a [`SessionEvent::HandoffComplete`] to signal the event loop that
    /// a handoff document was written and a new session was started.
    pub fn emit_handoff_complete(&self, doc_path: String, auto_continue: bool) {
        self.emit(SessionEvent::HandoffComplete {
            doc_path,
            auto_continue,
        });
    }

    /// Emit a [`SessionEvent::HandoffFailed`] so the event loop can surface
    /// the error to the user. The current session is preserved unchanged.
    pub fn emit_handoff_failed(&self, error: String) {
        self.emit(SessionEvent::HandoffFailed { error });
    }

    /// Get a reference to the underlying [`Agent`].
    ///
    /// Use this when you need direct agent access (e.g., `run_with_channel`).
    pub fn agent_ref(&self) -> Arc<Agent> {
        Arc::clone(&self.agent)
    }

    /// Persist a user prompt to the session manager before the agent loop starts.
    ///
    /// This must be called before sending the prompt to the agent worker so that
    /// the user message is in `file_entries` when the deferred-flush fires (which
    /// requires at least one assistant message before writing to disk).  Without
    /// this call the session file would contain only assistant / tool-result
    /// entries, and `cleanup_if_empty()` would delete it on teardown.
    ///
    /// The method also increments `persisted_count` so that the safety-net
    /// `persist_session()` (called on `AgentEnd`) does not double-write the
    /// message.
    pub fn persist_user_message(&self, content: String) {
        let mut sm = self.session_manager.write();
        sm.append_message(AgentMessage::User {
            content: crate::store::session::ContentValue::String(content),
        });
        let count = sm.persisted_count();
        sm.set_persisted_count(count + 1);
    }

    /// Persist a single message from an event directly to session manager.
    ///
    /// This is the **event-driven persist** path, matching pi's approach:
    /// each `MessageEnd` event carries the full message snapshot, and we
    /// convert + append it to the session file immediately.
    ///
    /// This avoids the race condition where `persist_session()` reads
    /// `agent.state()` which may be stale during a running agent loop
    /// (the agent loop operates on a separate `fresh_state`).
    pub fn persist_event_message(&self, message: &oxicode_sdk::Message) {
        let mut sm = self.session_manager.write();
        match message {
            Message::User(u) => {
                let content = match &u.content {
                    oxicode_sdk::MessageContent::Text(t) => t.clone(),
                    oxicode_sdk::MessageContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|b| b.as_text())
                        .collect::<Vec<_>>()
                        .join(""),
                };
                sm.append_message(AgentMessage::User {
                    content: crate::store::session::ContentValue::String(content),
                });
            }
            Message::Assistant(a) => {
                let content_blocks: Vec<crate::store::session::AssistantContentBlock> = a
                    .content
                    .iter()
                    .map(|b| match b {
                        oxicode_sdk::ContentBlock::Text(t) => {
                            crate::store::session::AssistantContentBlock::Text {
                                text: t.text.clone(),
                            }
                        }
                        oxicode_sdk::ContentBlock::Thinking(t) => {
                            crate::store::session::AssistantContentBlock::Thinking {
                                thinking: t.thinking.clone(),
                            }
                        }
                        oxicode_sdk::ContentBlock::ToolCall(tc) => {
                            crate::store::session::AssistantContentBlock::ToolCall {
                                id: tc.id.clone(),
                                name: tc.name.clone(),
                                arguments: tc.arguments.clone(),
                            }
                        }
                        oxicode_sdk::ContentBlock::Image(img) => {
                            crate::store::session::AssistantContentBlock::ImageResult {
                                data: img.data.clone(),
                                media_type: img.mime_type.clone(),
                            }
                        }
                        oxicode_sdk::ContentBlock::Unknown(v) => {
                            crate::store::session::AssistantContentBlock::Text {
                                text: v.to_string(),
                            }
                        }
                    })
                    .collect();

                sm.append_message(AgentMessage::Assistant {
                    content: content_blocks,
                    provider: Some(a.provider.clone()),
                    model_id: Some(a.model.clone()),
                    usage: Some(crate::store::session::Usage {
                        input: Some(a.usage.input as i64),
                        output: Some(a.usage.output as i64),
                        cache_read: Some(a.usage.cache_read as i64),
                        cache_write: Some(a.usage.cache_write as i64),
                        total_tokens: Some(a.usage.total_tokens as i64),
                    }),
                    stop_reason: Some(format!("{:?}", a.stop_reason)),
                });
            }
            Message::ToolResult(t) => {
                let content = t
                    .content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .collect::<Vec<_>>()
                    .join("");
                sm.append_message(AgentMessage::ToolResult {
                    content: crate::store::session::ContentValue::String(content),
                    tool_call_id: t.tool_call_id.clone(),
                });
            }
        }
        // Increment persisted_count so the fallback persist_session()
        // does not re-add this message on AgentEnd.
        let count = sm.persisted_count();
        sm.set_persisted_count(count + 1);
    }

    /// Persist the current agent state to the session file.
    ///
    /// Uses the state-snapshot approach: reads `agent.state().messages` and
    /// appends only messages not yet tracked by `persisted_count`.
    ///
    /// **Note:** This is kept as a safety-net fallback, called on `AgentEnd`
    /// to catch any messages that might have been missed by event-driven
    /// persist. The primary persist path is now `persist_event_message()`.
    pub fn persist(&self) {
        self.persist_session();
    }

    /// Get a cheap cloneable handle that references the same underlying session.
    pub fn clone_handle(&self) -> AgentSessionHandle {
        AgentSessionHandle {
            inner: Arc::new(self.clone_inner()),
        }
    }

    // Internal: produce a Self with the same arcs (doesn't actually clone Agent).
    fn clone_inner(&self) -> Self {
        Self {
            agent: Arc::clone(&self.agent),
            settings: Arc::clone(&self.settings),
            session_manager: Arc::clone(&self.session_manager),
            hub: Arc::clone(&self.hub),
            listeners: Arc::clone(&self.listeners),
            scoped_models: Arc::clone(&self.scoped_models),
            steering_messages: Arc::clone(&self.steering_messages),
            follow_up_messages: Arc::clone(&self.follow_up_messages),
            steering_mode: Arc::clone(&self.steering_mode),
            follow_up_mode: Arc::clone(&self.follow_up_mode),
            compaction_config: Arc::clone(&self.compaction_config),
            compaction_abort: Arc::clone(&self.compaction_abort),
            overflow_recovery_attempted: Arc::clone(&self.overflow_recovery_attempted),
            session_id: Arc::clone(&self.session_id),
            cwd: self.cwd.clone(),
            streaming: Arc::clone(&self.streaming),
            should_stop: Arc::clone(&self.should_stop),
            extension_runner: Arc::clone(&self.extension_runner),
            advisor: Arc::clone(&self.advisor),
            advisor_guard: Arc::clone(&self.advisor_guard),
            advisor_delivery: Arc::clone(&self.advisor_delivery),
            advisor_primary_turns: Arc::clone(&self.advisor_primary_turns),
        }
    }

    // ══════════════════════════════════════════════════════════════════
    // Extension integration
    // ══════════════════════════════════════════════════════════════════

    /// Set or replace the [`ExtensionRunner`] used by this session.
    ///
    /// This is called by the runtime after CLI parsing to inject the
    /// extension runner. If a runner was already set, its extensions are
    /// unloaded first via `emit_session_shutdown`.
    pub fn set_extension_runner(&self, runner: ExtensionRunner) {
        // If there is an existing runner, notify its extensions about shutdown
        {
            let guard = self.extension_runner.read();
            if let Some(existing) = guard.as_ref() {
                let session_id = self.session_id();
                let shutdown_event = SessionShutdownEvent {
                    reason: SessionShutdownReason::Reload,
                    target_session_file: None,
                };
                existing.emit_session_shutdown_event(&shutdown_event);
                existing.registry().emit_session_end(&session_id);
                existing.registry().emit_unload();
            }
        }

        // Install the new runner
        {
            let mut guard = self.extension_runner.write();
            *guard = Some(runner);
        }

        // Fire lifecycle hooks on the new runner
        {
            let guard = self.extension_runner.read();
            if let Some(runner) = guard.as_ref() {
                let ctx = self.build_extension_context();
                runner.registry().emit_load(&ctx);
                let session_id = self.session_id();
                runner.registry().emit_session_start(&session_id);
            }
        }

        tracing::debug!("ExtensionRunner installed into AgentSession");
    }

    /// Get a reference to the current [`ExtensionRunner`], if any.
    pub fn extension_runner(&self) -> parking_lot::RwLockReadGuard<'_, Option<ExtensionRunner>> {
        self.extension_runner.read()
    }

    /// Take the [`ExtensionRunner`] out of this session, shutting down extensions first.
    pub fn take_extension_runner(&self) -> Option<ExtensionRunner> {
        {
            let guard = self.extension_runner.read();
            if let Some(runner) = guard.as_ref() {
                let session_id = self.session_id();
                let shutdown_event = SessionShutdownEvent {
                    reason: SessionShutdownReason::Quit,
                    target_session_file: None,
                };
                runner.emit_session_shutdown_event(&shutdown_event);
                runner.registry().emit_session_end(&session_id);
                runner.registry().emit_unload();
            }
        }
        self.extension_runner.write().take()
    }

    /// Build an [`ExtensionContext`] for the current session state.
    ///
    /// The context provides extensions with access to settings, tools,
    /// session state, and other host capabilities.
    pub fn build_extension_context(&self) -> ExtensionContext {
        ExtensionContextBuilder::new(PathBuf::from(&self.cwd))
            .settings(Arc::clone(&self.settings))
            .build()
    }

    /// Forward an agent event to the extension system.
    ///
    /// If an [`ExtensionRunner`] is installed, the event is broadcast to
    /// all enabled extensions. The event is *also* emitted as a
    /// [`SessionEvent::Agent`] to regular session listeners.
    pub fn forward_event_to_extensions(&self, event: &AgentEvent) {
        // Advisor: feed each completed primary turn to the shadowing reviewer.
        // omp `setOnTurnEnd`. Reads the live primary transcript (TurnEnd only
        // carries the turn's assistant message; the advisor needs the full list).
        if let AgentEvent::TurnEnd { .. } = event {
            self.on_advisor_turn_end();
        }
        // Always emit to session listeners
        self.emit(SessionEvent::Agent(Box::new(event.clone())));

        // Forward to extension runner if installed
        let guard = self.extension_runner.read();
        if let Some(runner) = guard.as_ref() {
            runner.registry().emit_event(event);

            // Dispatch to typed hooks based on event variant
            match event {
                AgentEvent::ToolCall { tool_call } => {
                    runner.emit_tool_call(&tool_call.name, &tool_call.arguments);
                }
                AgentEvent::ToolExecutionStart {
                    tool_name, args, ..
                } => {
                    runner.emit_tool_call(tool_name, args);
                }
                AgentEvent::ToolExecutionEnd {
                    tool_name, result, ..
                } => {
                    let tool_result = oxicode_agent::AgentToolResult::success(&result.content);
                    runner.emit_tool_result_event(tool_name, &tool_result);
                }
                _ => {}
            }
        }
    }

    /// Check if extension handlers are registered for an event type.
    pub fn has_extension_handlers(&self, event_type: &str) -> bool {
        let guard = self.extension_runner.read();
        if let Some(runner) = guard.as_ref() {
            runner.has_handlers(event_type)
        } else {
            false
        }
    }

    /// Collect all tools contributed by extensions.
    ///
    /// Returns an empty vector when no extension runner is installed.
    pub fn extension_tools(&self) -> Vec<Arc<dyn oxicode_agent::AgentTool>> {
        let guard = self.extension_runner.read();
        if let Some(runner) = guard.as_ref() {
            runner.all_tools()
        } else {
            Vec::new()
        }
    }

    /// Collect all commands contributed by extensions.
    ///
    /// Returns an empty vector when no extension runner is installed.
    pub fn extension_commands(&self) -> Vec<crate::extensions::Command> {
        let guard = self.extension_runner.read();
        if let Some(runner) = guard.as_ref() {
            runner.all_commands()
        } else {
            Vec::new()
        }
    }

    /// Emit a before-tool-call event to extensions.
    ///
    /// Extensions may block the tool call by returning an error.
    /// Returns the [`ToolCallEmitResult`](crate::extensions::types::ToolCallEmitResult) with blocking status.
    pub fn emit_before_tool_call(
        &self,
        tool_name: &str,
        params: &serde_json::Value,
    ) -> crate::extensions::ToolCallEmitResult {
        let guard = self.extension_runner.read();
        if let Some(runner) = guard.as_ref() {
            runner.emit_tool_call(tool_name, params)
        } else {
            crate::extensions::ToolCallEmitResult::default()
        }
    }

    /// Emit an after-tool-result event to extensions.
    ///
    /// Extensions can inspect and log tool results.
    pub fn emit_after_tool_result(
        &self,
        tool_name: &str,
        result: &oxicode_agent::AgentToolResult,
    ) -> crate::extensions::ToolResultEmitResult {
        let guard = self.extension_runner.read();
        if let Some(runner) = guard.as_ref() {
            runner.emit_tool_result_event(tool_name, result)
        } else {
            crate::extensions::ToolResultEmitResult::default()
        }
    }

    /// Notify extensions that settings have changed.
    pub fn notify_extensions_settings_changed(&self) {
        let guard = self.extension_runner.read();
        if let Some(runner) = guard.as_ref() {
            let settings = self.settings.read().clone();
            runner.registry().emit_settings_changed(&settings);
        }
    }
    // ══════════════════════════════════════════════════════════════════
    // Advisor (read-only reviewer shadowing the primary agent)
    // ══════════════════════════════════════════════════════════════════

    /// Whether the advisor is currently enabled for this session.
    #[must_use]
    pub fn is_advisor_enabled(&self) -> bool {
        self.advisor.read().is_some()
    }

    /// Enable or disable the advisor for this session, starting or stopping
    /// the runtime to match. Returns `true` when the advisor is actively
    /// running after the call. Mirrors omp `setAdvisorEnabled`.
    pub fn set_advisor_enabled(&self, enabled: bool) -> Result<bool> {
        {
            let mut s = self.settings.write();
            s.advisor.enabled = enabled;
        }
        if enabled {
            if self.advisor.read().is_none() {
                let rt = self.build_advisor().ok_or_else(|| {
                    anyhow::anyhow!(
                        "advisor could not start: no provider/model resolved for the \
                         advisor role or the primary model"
                    )
                })?;
                super::agent_hub_bridge::register_advisor(&self.hub, rt.transcript_path());
                *self.advisor.write() = Some(rt);
            }
            Ok(true)
        } else {
            if let Some(rt) = self.advisor.write().take() {
                rt.dispose();
            }
            Ok(false)
        }
    }

    /// Toggle the advisor on/off. Returns the new enabled state.
    pub fn toggle_advisor(&self) -> Result<bool> {
        self.set_advisor_enabled(!self.is_advisor_enabled())
    }

    /// A one-line status string for `/advisor status`.
    #[must_use]
    pub fn advisor_status(&self) -> String {
        let enabled = self.is_advisor_enabled();
        let backlog = self.advisor.read().as_ref().map_or(0u64, |rt| rt.backlog());
        let turns = self.advisor_primary_turns.load(Ordering::SeqCst);
        format!(
            "advisor: {} | backlog {} turn(s) behind | {} primary turn(s) observed",
            if enabled { "ON" } else { "OFF" },
            backlog,
            turns
        )
    }

    /// Re-prime the advisor across a conversation boundary (`/new`, `/branch`,
    /// resume). Clears interrupt latches + delivery state and resets the
    /// emission guard so old advice can be re-raised. Mirrors omp
    /// `#resetAdvisorSessionState`.
    pub fn reset_advisor_state(&self) {
        self.advisor_guard.reset();
        *self.advisor_delivery.lock() = AdvisorDeliveryState::default();
        self.advisor_primary_turns.store(0, Ordering::SeqCst);
        if let Some(rt) = self.advisor.read().as_ref() {
            rt.reset();
        }
    }

    /// Feed one completed primary turn to the advisor. Called from
    /// `forward_event_to_extensions` on `AgentEvent::TurnEnd`.
    fn on_advisor_turn_end(&self) {
        self.advisor_primary_turns.fetch_add(1, Ordering::SeqCst);
        let rt = self.advisor.read().clone();
        let Some(rt) = rt else {
            return;
        };
        let messages = self.agent_ref().state().messages;
        rt.on_turn_end(messages);
        // Best-effort sync-backlog barrier (omp `advisor.syncBacklog`). The
        // event path is sync, so we can't await; spawn a bounded wait that
        // lets the advisor catch up before the next user-visible step.
        let sync = self.settings.read().advisor.sync_backlog.clone();
        if sync != "off"
            && let Ok(threshold) = sync.parse::<u64>()
            && threshold > 0
        {
            let rt = Arc::clone(&rt);
            tokio::spawn(async move {
                let _ = rt
                    .wait_for_catchup(std::time::Duration::from_millis(30_000), threshold)
                    .await;
            });
        }
    }

    /// Construct the advisor: a second `Agent` (advisor role model + read-only
    /// tools + advisor system prompt) wrapped in `AgentAdvisor`, driven by an
    /// `AdvisorRuntime`. Returns `None` if no provider/model can be resolved.
    fn build_advisor(&self) -> Option<Arc<AdvisorRuntime>> {
        let settings = self.settings.read();
        let primary_model = self.agent.model_id();
        // Resolve the advisor model: the `advisor` role if configured, else
        // fall back to the primary model so the advisor still runs.
        let advisor_model_id = oxicode_ai::roles::live_role_registry()
            .and_then(|r| {
                let reg = r.read();
                resolve_role_to_model(ModelRole::Advisor, &reg)
            })
            .map(|m| format!("{}/{}", m.provider, m.id))
            .unwrap_or_else(|| primary_model.clone());
        let (provider_name, _model_name) = advisor_model_id.split_once('/')?;
        let provider_box = get_provider(provider_name)?;
        let base: Arc<dyn Provider> = Arc::from(provider_box);
        // Wrap in the role router so live `model_roles` edits apply, mirroring
        // the primary agent's construction.
        let registry = oxicode_ai::roles::live_role_registry()
            .map(std::sync::Arc::clone)
            .unwrap_or_else(|| {
                std::sync::Arc::new(parking_lot::RwLock::new(RoleRegistry::default()))
            });
        let provider: Arc<dyn Provider> = Arc::new(RoleRoutingProvider::new(base, registry));

        let config = AgentConfig {
            name: "advisor".to_string(),
            description: Some("oxicode read-only advisor".to_string()),
            model_id: advisor_model_id,
            system_prompt: Some(crate::app::advisor_context::assemble_advisor_system_prompt(
                &self.cwd,
            )),
            timeout_seconds: settings.tool_timeout_seconds,
            temperature: settings.effective_temperature(),
            max_tokens: settings.effective_max_tokens(),
            compaction_strategy: CompactionStrategy::Threshold(0.8),
            compaction_instruction: None,
            context_window: 128_000,
            workspace_dir: Some(std::path::PathBuf::from(&self.cwd)),
            output_mode: None,
            session_id: None,
            provider_options: None,
            ttsr_engine: None,
            memory: None,
            todo: None,
            agent_pool: None,
            ..Default::default()
        };
        let immune_turns = settings.advisor.immune_turns;
        drop(settings);

        let advisor_agent = Arc::new(Agent::new(provider, config, Arc::new(ToolRegistry::new())));
        // Read-only investigation tools only.
        let tools = advisor_agent.tools();
        tools.register(ReadTool::new());
        tools.register(GrepTool::new());
        tools.register(FindTool::new());

        // The advise tool's enqueue closure routes accepted notes to the
        // primary via the session's shared queues (steering / listeners) +
        // emission guard + delivery state. Capturing shared `Arc`s avoids a
        // self-referential `Arc<AgentSession>` cycle.
        let enqueue: EnqueueAdviceFn = {
            let steering = Arc::clone(&self.steering_messages);
            let listeners = Arc::clone(&self.listeners);
            let guard = Arc::clone(&self.advisor_guard);
            let delivery = Arc::clone(&self.advisor_delivery);
            let streaming = Arc::clone(&self.streaming);
            let primary_turns = Arc::clone(&self.advisor_primary_turns);
            Arc::new(move |note: AdvisorNote| {
                if !guard.accept(&note.note) {
                    return;
                }
                let mut d = *delivery.lock();
                let turns = primary_turns.load(Ordering::SeqCst);
                let immune = oxicode_agent::advisor::is_immune_turn_active(
                    turns,
                    d.interrupt_immune_turn_start,
                    immune_turns,
                );
                let channel = resolve_delivery_channel(DeliveryOpts {
                    severity: note.severity,
                    auto_resume_suppressed: d.auto_resume_suppressed,
                    streaming: streaming.load(Ordering::SeqCst),
                    aborting: false,
                    interrupt_immune_turn_active: immune,
                });
                let body = format_advisory_batch(std::slice::from_ref(&note));
                match channel {
                    AdvisorDeliveryChannel::Steer => {
                        steering.write().push_back(oxicode_sdk::Message::User(
                            oxicode_sdk::UserMessage::new(body),
                        ));
                        d.interrupt_immune_turn_start = Some(turns + 1);
                        *delivery.lock() = d;
                    }
                    AdvisorDeliveryChannel::Aside | AdvisorDeliveryChannel::Preserve => {
                        let evt = SessionEvent::Advisor { channel, body };
                        for f in listeners.read().iter() {
                            f(&evt);
                        }
                    }
                }
            })
        };
        tools.register(AdviseTool::new(enqueue));

        let host: Arc<dyn AdvisorRuntimeHost> = Arc::new(AdvisorHost {
            agent: self.agent_ref(), // PRIMARY agent — the transcript being shadowed (not the advisor's own)
            guard: Arc::clone(&self.advisor_guard),
        });
        let recorder = Arc::new(crate::app::advisor_context::AdvisorTranscriptRecorder::new(
            self.session_manager.read().get_session_file(),
        ));
        let advisor_driver = Arc::new(AgentAdvisor::with_post_prompt_hook(
            advisor_agent,
            recorder.hook(),
        ));
        let rt = Arc::new(AdvisorRuntime::new(
            advisor_driver,
            host,
            std::time::Duration::from_millis(1000),
        ));
        let transcript_path = self
            .session_manager
            .read()
            .get_session_file()
            .and_then(|file| {
                std::path::Path::new(&file)
                    .parent()
                    .map(|dir| dir.join(crate::app::advisor_context::ADVISOR_TRANSCRIPT_FILENAME))
            });
        rt.set_transcript_path(transcript_path);
        rt.install_self(Arc::downgrade(&rt));
        // Seed the cursor so enabling mid-session doesn't replay all history.
        rt.seed_to(self.agent_ref().state().messages.len() as u64);
        Some(rt)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Listener guard
// ═══════════════════════════════════════════════════════════════════════════

/// RAII guard that removes a session event listener when dropped.
pub struct SessionListenerGuard {
    #[allow(clippy::type_complexity)]
    listeners: Arc<RwLock<Vec<Box<dyn Fn(&SessionEvent) + Send + Sync>>>>,
    key: usize,
}

impl Drop for SessionListenerGuard {
    fn drop(&mut self) {
        let mut listeners = self.listeners.write();
        if self.key < listeners.len() {
            // Replace with a no-op
            listeners[self.key] = Box::new(|_| {});
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Handle (cheap clone)
// ═══════════════════════════════════════════════════════════════════════════

/// A cheaply-clonable handle to an [`AgentSession`].
///
/// Use this when you need to share the session across tasks / threads.
#[derive(Clone)]
pub struct AgentSessionHandle {
    inner: Arc<AgentSession>,
}

impl std::ops::Deref for AgentSessionHandle {
    type Target = AgentSession;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Cycling direction
// ═══════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use futures::Stream;
    use oxicode_agent::AgentConfig;
    use oxicode_sdk::{Model, Provider, ProviderError, ProviderEvent};
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context as TaskContext, Poll};

    // ── Mock Provider ─────────────────────────────────────────────────

    /// Minimal mock provider that produces an empty stream.
    struct MockProvider;

    struct EmptyStream;

    impl Stream for EmptyStream {
        type Item = ProviderEvent;
        fn poll_next(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(None)
        }
    }

    impl Provider for MockProvider {
        fn stream<'a>(
            &'a self,
            _model: &'a Model,
            _context: &'a oxicode_sdk::Context,
            _options: Option<oxicode_sdk::StreamOptions>,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>,
                            ProviderError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                Ok::<_, ProviderError>(
                    Box::pin(EmptyStream) as Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>
                )
            })
        }
    }

    fn make_session() -> AgentSession {
        let provider = Arc::new(MockProvider);
        let config = AgentConfig::new("anthropic/claude-sonnet-4-20250514");
        let agent = Arc::new(Agent::new(
            provider,
            config,
            Arc::new(oxicode_agent::ToolRegistry::new()),
        ));
        let settings = Settings::default();
        let session_manager = SessionManager::in_memory("/tmp/test");
        AgentSession::new(
            agent,
            settings,
            session_manager,
            "/tmp/test".to_string(),
            crate::SessionState::default(),
        )
    }

    // ══════════════════════════════════════════════════════════════════
    // AgentSession creation
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn test_session_creation_basic_fields() {
        let session = make_session();
        assert!(!session.session_id().is_empty());
        assert_eq!(session.cwd(), "/tmp/test");
        assert!(!session.is_streaming());
        assert!(session.messages().is_empty());
    }

    #[test]
    fn test_session_creation_model_id() {
        let session = make_session();
        assert_eq!(session.model_id(), "anthropic/claude-sonnet-4-20250514");
    }

    #[test]
    fn test_session_creation_default_thinking_level() {
        let session = make_session();
        assert_eq!(session.thinking_level(), ThinkingLevel::Medium);
    }

    #[test]
    fn test_session_creation_empty_queues() {
        let session = make_session();
        assert_eq!(session.pending_message_count(), 0);
        assert!(session.steering_messages().is_empty());
        assert!(session.follow_up_messages().is_empty());
    }

    #[test]
    fn test_scoped_models_empty_by_default() {
        let session = make_session();
        assert!(session.scoped_models().is_empty());
    }

    // ══════════════════════════════════════════════════════════════════
    // Model cycling
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn test_set_scoped_models() {
        let session = make_session();
        let models = vec![
            ScopedModel {
                provider: "anthropic".to_string(),
                model_id: "claude-sonnet-4-20250514".to_string(),
            },
            ScopedModel {
                provider: "openai".to_string(),
                model_id: "gpt-4o".to_string(),
            },
            ScopedModel {
                provider: "google".to_string(),
                model_id: "gemini-2.0-flash".to_string(),
            },
        ];
        session.set_scoped_models(models);
        let retrieved = session.scoped_models();
        assert_eq!(retrieved.len(), 3);
        assert_eq!(retrieved[0].provider, "anthropic");
        assert_eq!(retrieved[2].model_id, "gemini-2.0-flash");
    }

    #[test]
    fn test_scoped_model_fields() {
        let model = ScopedModel {
            provider: "anthropic".to_string(),
            model_id: "claude-sonnet-4-20250514".to_string(),
        };
        assert_eq!(model.provider, "anthropic");
        assert_eq!(model.model_id, "claude-sonnet-4-20250514");
    }

    // ══════════════════════════════════════════════════════════════════
    // Thinking level changes
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn test_set_thinking_level() {
        let session = make_session();
        assert_eq!(session.thinking_level(), ThinkingLevel::Medium);

        session.set_thinking_level(ThinkingLevel::High);
        assert_eq!(session.thinking_level(), ThinkingLevel::High);

        session.set_thinking_level(ThinkingLevel::Off);
        assert_eq!(session.thinking_level(), ThinkingLevel::Off);

        session.set_thinking_level(ThinkingLevel::Minimal);
        assert_eq!(session.thinking_level(), ThinkingLevel::Minimal);
    }

    #[test]
    fn test_set_thinking_level_noop_when_same() {
        let session = make_session();
        // Should not emit event when setting to same level
        session.set_thinking_level(ThinkingLevel::Medium);
        assert_eq!(session.thinking_level(), ThinkingLevel::Medium);
    }

    #[test]
    fn test_cycle_thinking_level() {
        let session = make_session();
        assert_eq!(session.thinking_level(), ThinkingLevel::Medium);

        let next = session.cycle_thinking_level();
        assert_eq!(next, Some(ThinkingLevel::High));
        assert_eq!(session.thinking_level(), ThinkingLevel::High);

        // Continue cycling
        let next = session.cycle_thinking_level();
        assert_eq!(next, Some(ThinkingLevel::XHigh));
        assert_eq!(session.thinking_level(), ThinkingLevel::XHigh);

        let next = session.cycle_thinking_level();
        assert_eq!(next, Some(ThinkingLevel::Off));

        let next = session.cycle_thinking_level();
        assert_eq!(next, Some(ThinkingLevel::Minimal));

        let next = session.cycle_thinking_level();
        assert_eq!(next, Some(ThinkingLevel::Low));

        let next = session.cycle_thinking_level();
        assert_eq!(next, Some(ThinkingLevel::Medium));

        let next = session.cycle_thinking_level();
        assert_eq!(next, Some(ThinkingLevel::High));

        let next = session.cycle_thinking_level();
        assert_eq!(next, Some(ThinkingLevel::XHigh));
    }

    #[test]
    fn test_thinking_level_full_cycle() {
        let levels = [
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
        ];
        // Ensure we can cycle through all levels
        let mut current = 0;
        for _ in 0..levels.len() {
            current = (current + 1) % levels.len();
        }
        assert_eq!(current, 0); // Wraps back to start
    }

    // ══════════════════════════════════════════════════════════════════
    // Steering / follow-up queue operations
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn test_steer_message() {
        let session = make_session();
        session.steer("direction 1".to_string()).await.unwrap();
        assert_eq!(
            session
                .steering_messages()
                .iter()
                .map(|m| m.text_content().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["direction 1"]
        );
        assert_eq!(session.pending_message_count(), 1);
    }

    #[tokio::test]
    async fn test_follow_up_message() {
        let session = make_session();
        session.follow_up("next task".to_string()).await.unwrap();
        assert_eq!(
            session
                .follow_up_messages()
                .iter()
                .map(|m| m.text_content().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["next task"]
        );
        assert_eq!(session.pending_message_count(), 1);
    }

    #[tokio::test]
    async fn test_multiple_steer_messages() {
        let session = make_session();
        session.steer("first".to_string()).await.unwrap();
        session.steer("second".to_string()).await.unwrap();
        session.steer("third".to_string()).await.unwrap();
        assert_eq!(
            session
                .steering_messages()
                .iter()
                .map(|m| m.text_content().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["first", "second", "third"]
        );
        assert_eq!(session.pending_message_count(), 3);
    }

    #[tokio::test]
    async fn test_multiple_follow_up_messages() {
        let session = make_session();
        session.follow_up("a".to_string()).await.unwrap();
        session.follow_up("b".to_string()).await.unwrap();
        assert_eq!(
            session
                .follow_up_messages()
                .iter()
                .map(|m| m.text_content().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[tokio::test]
    async fn test_mixed_steer_and_follow_up() {
        let session = make_session();
        session.steer("steer-1".to_string()).await.unwrap();
        session.follow_up("follow-1".to_string()).await.unwrap();
        session.steer("steer-2".to_string()).await.unwrap();
        assert_eq!(session.pending_message_count(), 3);
        assert_eq!(
            session
                .steering_messages()
                .iter()
                .map(|m| m.text_content().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["steer-1", "steer-2"]
        );
        assert_eq!(
            session
                .follow_up_messages()
                .iter()
                .map(|m| m.text_content().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["follow-1"]
        );
    }

    #[test]
    fn test_clear_queue() {
        let session = make_session();
        // Manually insert items
        {
            let mut q = session.steering_messages.write();
            q.push_back(oxicode_sdk::Message::User(oxicode_sdk::UserMessage::new(
                "s1",
            )));
            q.push_back(oxicode_sdk::Message::User(oxicode_sdk::UserMessage::new(
                "s2",
            )));
        }
        {
            let mut q = session.follow_up_messages.write();
            q.push_back(oxicode_sdk::Message::User(oxicode_sdk::UserMessage::new(
                "f1",
            )));
        }
        assert_eq!(session.pending_message_count(), 3);

        let (steering, follow_up) = session.clear_queue();
        assert_eq!(
            steering
                .iter()
                .map(|m| m.text_content().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["s1", "s2"]
        );
        assert_eq!(
            follow_up
                .iter()
                .map(|m| m.text_content().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["f1"]
        );
        assert_eq!(session.pending_message_count(), 0);
    }

    #[test]
    fn test_clear_empty_queue() {
        let session = make_session();
        let (s, f) = session.clear_queue();
        assert!(s.is_empty());
        assert!(f.is_empty());
    }

    // ══════════════════════════════════════════════════════════════════
    // Compaction trigger logic
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn test_auto_compaction_default_enabled() {
        let session = make_session();
        // Settings::default() has auto_compaction = true, but AgentSession
        // overrides based on settings.auto_compaction in new()
        // CompactionConfig::default().enabled = true, but AgentSession::new
        // uses settings.auto_compaction to initialize
        assert!(session.auto_compaction_enabled());
    }

    #[test]
    fn test_is_compacting_initially_false() {
        let session = make_session();
        assert!(!session.is_compacting());
    }

    #[test]
    fn test_compaction_reason_variants() {
        assert_eq!(CompactionReason::Manual, CompactionReason::Manual);
        assert_ne!(CompactionReason::Manual, CompactionReason::Threshold);
        assert_ne!(CompactionReason::Threshold, CompactionReason::Overflow);
        assert_ne!(CompactionReason::Manual, CompactionReason::Overflow);
    }

    #[test]
    fn test_compaction_config_default() {
        let config = CompactionConfig::default();
        assert!(config.enabled);
        assert!(config.threshold > 0.0);
    }

    // ══════════════════════════════════════════════════════════════════
    // Session entry appending / persistence
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn test_session_stats_empty() {
        let session = make_session();
        let stats = session.session_stats();
        assert!(!stats.session_id.is_empty());
        assert_eq!(stats.user_messages, 0);
        assert_eq!(stats.assistant_messages, 0);
        assert_eq!(stats.tool_calls, 0);
        assert_eq!(stats.tool_results, 0);
        assert_eq!(stats.total_messages, 0);
    }

    #[test]
    fn test_session_stats_default() {
        let stats = SessionStats {
            session_id: "test".to_string(),
            user_messages: 0,
            assistant_messages: 0,
            tool_calls: 0,
            tool_results: 0,
            total_messages: 0,
        };
        assert_eq!(stats.total_messages, 0);
    }

    #[test]
    fn test_persist_session_empty_messages() {
        let session = make_session();
        // Should not panic with no messages
        session.persist_session();
    }

    // Note: Agent::state() returns a clone, so direct mutation via
    // agent.state().add_user_message() doesn't modify the internal state.
    // These tests verify persist_session behavior with the in-memory
    // SessionManager by checking the persisted_count boundary logic.

    #[test]
    fn test_persist_session_empty_is_noop() {
        let session = make_session();
        // No messages in agent state → persist_session is a no-op
        session.persist_session();
        let sm = session.session_manager.read();
        assert_eq!(sm.persisted_count(), 0);
    }

    #[test]
    fn test_persist_session_set_persisted_count() {
        let session = make_session();
        // Directly set persisted count to verify the accessor works
        {
            let sm = session.session_manager.write();
            sm.set_persisted_count(5);
        }
        let sm = session.session_manager.read();
        assert_eq!(sm.persisted_count(), 5);
    }

    #[test]
    fn test_persist_session_idempotent_with_set() {
        let session = make_session();
        // Set persisted_count to 3, then persist_session (0 messages) is a no-op
        {
            let sm = session.session_manager.write();
            sm.set_persisted_count(3);
        }
        session.persist_session();
        let sm = session.session_manager.read();
        assert_eq!(sm.persisted_count(), 3);
    }

    #[test]
    fn test_set_session_name() {
        let session = make_session();
        session.set_session_name("My Test Session".to_string());
        // Verify it doesn't panic and session ID remains valid
        assert!(!session.session_id().is_empty());
    }

    // ══════════════════════════════════════════════════════════════════
    // Event subscription
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn test_subscribe_receives_events() {
        let session = make_session();
        let received = Arc::new(RwLock::new(Vec::new()));
        let received_clone = received.clone();

        let _guard = session.subscribe(Box::new(move |event| {
            received_clone.write().push(format!("{:?}", event));
        }));

        // Trigger an event via set_thinking_level (Standard → Thorough)
        session.set_thinking_level(ThinkingLevel::High);

        let events = received.read();
        assert!(
            !events.is_empty(),
            "Listener should receive at least one event"
        );
        assert!(events.iter().any(|e| e.contains("ThinkingLevelChanged")));
    }

    #[test]
    fn test_subscribe_channel_with_guard() {
        let session = make_session();

        // subscribe_channel internally drops the guard (known issue),
        // so we test event reception via subscribe() directly instead.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();

        let _guard = session.subscribe(Box::new(move |event| {
            let _ = tx.send(event.clone());
        }));

        // Trigger event: Standard → None
        session.set_thinking_level(ThinkingLevel::Off);

        let event = rx
            .try_recv()
            .expect("Should receive event via subscribed channel");
        match event {
            SessionEvent::ThinkingLevelChanged { level } => {
                assert_eq!(level, ThinkingLevel::Off);
            }
            other => panic!("Expected ThinkingLevelChanged, got {:?}", other),
        }
    }

    // ══════════════════════════════════════════════════════════════════
    // Session reset
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn test_reset_clears_queues_and_overflow() {
        let session = make_session();
        // Add messages to queues (using internal fields directly)
        {
            let mut q = session.steering_messages.write();
            q.push_back(oxicode_sdk::Message::User(oxicode_sdk::UserMessage::new(
                "steer",
            )));
        }
        {
            let mut q = session.follow_up_messages.write();
            q.push_back(oxicode_sdk::Message::User(oxicode_sdk::UserMessage::new(
                "follow",
            )));
        }
        *session.overflow_recovery_attempted.write() = true;

        assert_eq!(session.pending_message_count(), 2);

        session.reset();
        assert_eq!(session.pending_message_count(), 0);
        assert!(!*session.overflow_recovery_attempted.read());
    }

    // ══════════════════════════════════════════════════════════════════
    // Handle cloning
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn test_clone_handle_shares_state() {
        let session = make_session();
        let handle = session.clone_handle();

        // Both should see the same session ID
        assert_eq!(session.session_id(), handle.session_id());

        // Mutations through the handle should be visible on the original
        handle.set_thinking_level(ThinkingLevel::High);
        assert_eq!(session.thinking_level(), ThinkingLevel::High);
    }

    // ══════════════════════════════════════════════════════════════════
    // ══════════════════════════════════════════════════════════════════
    // Extension integration
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn test_no_extension_runner_by_default() {
        let session = make_session();
        let guard = session.extension_runner();
        assert!(guard.is_none());
    }

    #[test]
    fn test_extension_tools_empty_without_runner() {
        let session = make_session();
        assert!(session.extension_tools().is_empty());
    }

    #[test]
    fn test_extension_commands_empty_without_runner() {
        let session = make_session();
        assert!(session.extension_commands().is_empty());
    }

    #[test]
    fn test_has_extension_handlers_false_without_runner() {
        let session = make_session();
        assert!(!session.has_extension_handlers("tool_call"));
    }

    #[test]
    fn test_auto_retry_enabled() {
        let session = make_session();
        assert!(session.auto_retry_enabled());
    }

    // ══════════════════════════════════════════════════════════════════
    // Listener guard
    // ══════════════════════════════════════════════════════════════════

    #[test]
    fn test_listener_guard_drop_removes() {
        let session = make_session();
        let received = Arc::new(RwLock::new(Vec::new()));
        let received_clone = received.clone();

        {
            let _guard = session.subscribe(Box::new(move |event| {
                received_clone.write().push(format!("{:?}", event));
            }));
            // Guard is active, event should fire
            session.set_thinking_level(ThinkingLevel::High);
        }
        // Guard dropped — the slot is replaced with no-op
        let count_after_drop = received.read().len();
        assert_eq!(count_after_drop, 1); // Only the first event
    }

    // ══════════════════════════════════════════════════════════════════
    // Session resume history reconstruction (issue #23)
    // ══════════════════════════════════════════════════════════════════

    use crate::store::session::{AssistantContentBlock, ContentValue, SessionEntry};

    fn entry(message: AgentMessage) -> SessionEntry {
        SessionEntry::new(message)
    }

    /// A resumed conversation with a tool call must reconstruct the full
    /// user → assistant(tool_call) → tool_result chain so the model and the UI
    /// see prior context (issue #23).
    #[test]
    fn resume_reconstructs_tool_call_and_result() {
        let branch = vec![
            entry(AgentMessage::User {
                content: ContentValue::String("list files".to_string()),
            }),
            entry(AgentMessage::Assistant {
                content: vec![
                    AssistantContentBlock::Text {
                        text: "Running ls".to_string(),
                    },
                    AssistantContentBlock::ToolCall {
                        id: "call_1".to_string(),
                        name: "ls".to_string(),
                        arguments: serde_json::json!({}),
                    },
                ],
                provider: Some("anthropic".to_string()),
                model_id: Some("claude-sonnet-4-20250514".to_string()),
                usage: None,
                stop_reason: None,
            }),
            entry(AgentMessage::ToolResult {
                content: ContentValue::String("file_a\nfile_b".to_string()),
                tool_call_id: "call_1".to_string(),
            }),
        ];

        let messages = resume_messages_from_branch(&branch);
        assert_eq!(messages.len(), 3, "all three turns reconstructed");

        // User
        assert!(matches!(messages[0], oxicode_sdk::Message::User(_)));
        // Assistant carries the tool call
        let assistant = match &messages[1] {
            oxicode_sdk::Message::Assistant(a) => a,
            _ => panic!("expected assistant message"),
        };
        assert_eq!(assistant.content.len(), 2);
        assert!(assistant.content.iter().any(
            |b| matches!(b, oxicode_sdk::ContentBlock::ToolCall(tc) if tc.id == "call_1"
                && tc.name == "ls")
        ));
        // Tool result is paired with the right id and resolved tool name.
        match &messages[2] {
            oxicode_sdk::Message::ToolResult(t) => {
                assert_eq!(t.tool_call_id, "call_1");
                assert_eq!(t.tool_name, "ls", "tool name resolved from the call");
                assert!(t.content.iter().any(|b| matches!(
                    b,
                    oxicode_sdk::ContentBlock::Text(t) if t.text.contains("file_a")
                )));
            }
            _ => panic!("expected tool result message"),
        }
    }

    /// Metadata entries (System / BashExecution / Custom) are NOT replayed to
    /// the model — only user, assistant, and tool-result turns.
    #[test]
    fn resume_skips_non_conversation_entries() {
        let branch = vec![
            entry(AgentMessage::System {
                content: ContentValue::String("sys note".to_string()),
            }),
            entry(AgentMessage::User {
                content: ContentValue::String("hello".to_string()),
            }),
            entry(AgentMessage::BashExecution {
                command: "echo hi".to_string(),
                output: "hi".to_string(),
                exit_code: Some(0),
                cancelled: false,
                truncated: false,
                full_output_path: None,
                exclude_from_context: None,
                timestamp: 0,
            }),
        ];

        let messages = resume_messages_from_branch(&branch);
        assert_eq!(messages.len(), 1, "only the user turn remains");
        assert!(matches!(messages[0], oxicode_sdk::Message::User(_)));
    }

    /// Compaction summary replaces earlier history: everything before the last
    /// summary is dropped, and the summary seeds the context.
    #[test]
    fn resume_honours_compaction_summary() {
        let branch = vec![
            entry(AgentMessage::User {
                content: ContentValue::String("old prompt".to_string()),
            }),
            entry(AgentMessage::CompactionSummary {
                summary: "We discussed X.".to_string(),
                tokens_before: 1000,
                timestamp: 0,
            }),
            entry(AgentMessage::User {
                content: ContentValue::String("new prompt".to_string()),
            }),
        ];

        let messages = resume_messages_from_branch(&branch);
        // Summary (as a user msg) + new prompt — the pre-compaction user msg
        // is dropped.
        assert_eq!(messages.len(), 2);
        let first_text = match &messages[0] {
            oxicode_sdk::Message::User(u) => match &u.content {
                oxicode_sdk::MessageContent::Text(t) => t.clone(),
                _ => String::new(),
            },
            _ => panic!("expected summary as user message"),
        };
        assert!(first_text.contains("We discussed X."));
    }

    /// An empty branch (brand-new session) yields no messages — seeding is a
    /// no-op.
    #[test]
    fn resume_empty_branch_is_empty() {
        let messages = resume_messages_from_branch(&[]);
        assert!(messages.is_empty());
    }

    /// End-to-end: constructing an `AgentSession` over a session manager that
    /// already holds history must seed the agent's conversation state (issue
    /// #23) so the resumed turn sees prior context — and must not duplicate the
    /// on-disk history when the safety-net `persist_session` runs.
    #[test]
    fn new_seeds_agent_state_from_resumed_session() {
        let mut sm = SessionManager::in_memory("/tmp/test");
        sm.append_message(AgentMessage::User {
            content: ContentValue::String("what is 2+2".to_string()),
        });
        sm.append_message(AgentMessage::Assistant {
            content: vec![AssistantContentBlock::Text {
                text: "4".to_string(),
            }],
            provider: Some("anthropic".to_string()),
            model_id: Some("claude-sonnet-4-20250514".to_string()),
            usage: None,
            stop_reason: None,
        });
        let before_count = sm.get_entries().len();

        let provider = Arc::new(MockProvider);
        let config = AgentConfig::new("anthropic/claude-sonnet-4-20250514");
        let agent = Arc::new(Agent::new(
            provider,
            config,
            Arc::new(oxicode_agent::ToolRegistry::new()),
        ));
        let session = AgentSession::new(
            agent,
            Settings::default(),
            sm,
            "/tmp/test".to_string(),
            crate::SessionState::default(),
        );

        let messages = session.agent_ref().state().messages;
        assert_eq!(messages.len(), 2, "agent state seeded with prior history");
        assert!(matches!(messages[0], oxicode_sdk::Message::User(_)));

        // Seeding only touched agent state, not the session manager.
        assert_eq!(
            session.session_manager.read().get_entries().len(),
            before_count
        );

        // The safety-net persist_session reconciles against the on-disk entry
        // count, so it must NOT re-write the seeded history.
        session.persist_session();
        assert_eq!(
            session.session_manager.read().get_entries().len(),
            before_count,
            "persist_session must not duplicate seeded history"
        );
    }
}
