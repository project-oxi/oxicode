/// Core agent implementation
use crate::config::AgentConfig;
use crate::config::ShouldStopAfterTurnContext;
use crate::events::AgentEvent;
use crate::state::{AgentState, SharedState};
use crate::tools::{AgentTool, ToolRegistry};
use crate::types::{Response, StopReason};
use anyhow::{Error, Result};
use oxicode_ai::{
    CompactionManager, CompactionStrategy, Compactor, LlmCompactor, Model, Provider,
    transform_for_provider,
};
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// ── ProviderResolver trait ────────────────────────────────────────

/// Trait for resolving providers and models within an Agent.
///
/// This abstracts away global static registries, allowing SDK users
/// to provide isolated provider/model lookups.
///
/// When using the SDK (`oxicode-sdk`), the `Oxicode` engine implements this trait.
/// When using `Agent::new()` directly, a global fallback is used.
pub trait ProviderResolver: Send + Sync + 'static {
    /// Resolve a provider by name, returning an Arc handle.
    fn resolve_provider(&self, name: &str) -> Option<Arc<dyn Provider>>;

    /// Resolve a model ID ("provider/model" or bare "model") to a Model.
    fn resolve_model(&self, model_id: &str) -> Option<Model>;
}

/// Global provider resolver — uses `oxicode_ai` global functions.
///
/// This is the default resolver when using `Agent::new()`, preserving
/// backward compatibility with existing CLI usage.
pub(crate) struct GlobalProviderResolver;

impl ProviderResolver for GlobalProviderResolver {
    fn resolve_provider(&self, name: &str) -> Option<Arc<dyn Provider>> {
        oxicode_ai::get_provider(name).map(Arc::from)
    }

    fn resolve_model(&self, model_id: &str) -> Option<Model> {
        crate::model_id::resolve_model_from_id(model_id)
    }
}

// ── AgentInner ────────────────────────────────────────────────────
/// Mutable agent internals protected by a read-write lock.
struct AgentInner {
    config: AgentConfig,
    provider: Arc<dyn Provider>,
    /// Side-dispatch closures invoked for every `AgentEvent` emitted by
    /// the agent run methods. Used by `oxicode-sdk` to bridge observability
    /// types (Tracer, CostTracker, ...) into the agent loop without
    /// leaking SDK types into `oxicode-agent`.
    ///
    /// Lock-mutex rather than `RwLock`: dispatch lists mutate rarely
    /// (only on `add_observability_dispatch`), but reads happen on every
    /// event (high frequency), so a `Mutex` with cheap poison-free
    /// acquisition is the right shape.
    observability_dispatch: parking_lot::Mutex<Vec<EventDispatchFn>>,
}

/// Type alias for an observability dispatch handler. Each entry is a
/// closure registered via [`Agent::add_observability_dispatch`] and
/// invoked on every emitted `AgentEvent`. Named to keep the
/// [`AgentInner`] field readable without an inline `dyn` route.
type EventDispatchFn = Arc<dyn Fn(AgentEvent) + Send + Sync>;

impl Clone for AgentInner {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            provider: Arc::clone(&self.provider),
            // The dispatch list is *not* cloned: each `Agent` instance has
            // its own observers. Cloning the AgentInner (rare; happens in
            // `run_with_channel_inner` when sharing config across loops)
            // gives the new loop an empty observer set, which is correct:
            // the *Agent* retains the original dispatch list, and the
            // temporary inner clone is discarded after the run.
            observability_dispatch: parking_lot::Mutex::new(Vec::new()),
        }
    }
}
///
/// Manages provider, tool registry, state, and compaction, providing an
/// agentic loop for prompt execution, model switching, tool calls, and fallback.
///
/// Supports session continuation via [`continue_with`] and tokio-native
/// event streaming via [`run_tokio_stream`].
///
/// [`continue_with`]: Agent::continue_with
/// [`run_tokio_stream`]: Agent::run_tokio_stream
/// Deferred model switch request, stored when the agent is running.
struct PendingModelSwitch {
    model_id: String,
    provider: Arc<dyn Provider>,
    /// Whether messages need cross-provider transformation.
    needs_transform: bool,
    old_api: oxicode_ai::Api,
    new_api: oxicode_ai::Api,
}

/// Agent runtime.
///
/// Manages provider, tool registry, state, and compaction, providing an
/// agentic loop for prompt execution, model switching, tool calls, and fallback.
///
/// Supports session continuation, tokio-native event streaming, and deferred
/// model switching (changes are queued while a loop is running and applied
/// after it completes).
#[allow(missing_docs)]
pub struct Agent {
    inner: RwLock<AgentInner>,
    tools: Arc<ToolRegistry>,
    state: SharedState,
    compaction_manager: CompactionManager,
    /// Custom compactor injected at construction (via `new_with_compactor`).
    ///
    /// Replaces the default `LlmCompactor` in the per-run `AgentLoop`
    /// (threaded into `AgentLoopConfig.compactor`). `None` preserves the
    /// existing default-LLM-compactor behavior.
    custom_compactor: Option<Arc<dyn Compactor>>,
    hooks: parking_lot::RwLock<crate::config::AgentHooks>,
    /// Guard: true while a run is in progress. Prevents concurrent runs.
    is_running: Arc<AtomicBool>,
    /// Provider/model resolver. Uses global functions by default,
    /// or a custom resolver when created via `new_with_resolver()`.
    resolver: Arc<dyn ProviderResolver>,
    /// Shared cancellation flag. Set by `cancel()` (e.g. on Ctrl+C),
    /// propagated to AgentLoop's `external_stop` during each run.
    cancel_flag: Arc<AtomicBool>,
    /// Shared auto-retry enabled flag — runtime-toggleable via `set_auto_retry`,
    /// injected into each ephemeral AgentLoop via `set_auto_retry_state`.
    auto_retry_enabled: Arc<AtomicBool>,
    /// Shared auto-retry cancel flag (RPC `abort_retry`).
    auto_retry_cancel: Arc<AtomicBool>,
    /// Shared auto-retry notify for immediate retry-sleep wake-up.
    auto_retry_notify: Arc<tokio::sync::Notify>,
    /// Notified whenever a run finishes (normal exit or panic) — drives
    /// [`Agent::wait_for_idle`].
    idle_notify: Arc<tokio::sync::Notify>,
    /// Pending model switch — stored when the agent is running,
    /// applied after the current loop completes.
    pending_model_switch: RwLock<Option<PendingModelSwitch>>,
}

impl Agent {
    /// Create a new agent with the given provider, config, and tool registry.
    ///
    /// Uses the global `oxicode_ai::get_provider()` / `resolve_model_from_id()`
    /// for model switching. For isolated instances, use [`new_with_resolver`].
    ///
    /// [`new_with_resolver`]: Agent::new_with_resolver
    pub fn new(provider: Arc<dyn Provider>, config: AgentConfig, tools: Arc<ToolRegistry>) -> Self {
        let resolver = Arc::new(GlobalProviderResolver);
        Self::build_inner(provider, config, tools, resolver, None)
    }

    /// Create an agent with a custom provider/model resolver.
    ///
    /// This is the preferred constructor for SDK usage where provider
    /// and model registries must be isolated from global state.
    pub fn new_with_resolver(
        provider: Arc<dyn Provider>,
        config: AgentConfig,
        tools: Arc<ToolRegistry>,
        resolver: Arc<dyn ProviderResolver>,
    ) -> Self {
        Self::build_inner(provider, config, tools, resolver, None)
    }

    /// Create an agent with a custom provider/model resolver and a custom
    /// compactor that replaces the default LLM compactor.
    ///
    /// The compactor is threaded into every per-run `AgentLoop` (via
    /// `AgentLoopConfig.compactor`) — see
    /// [`crate::agent_loop::config::AgentLoopConfig::compactor`] for the
    /// replace semantics. `oxicode-sdk`'s `AgentBuilder::with_compactor`
    /// uses this constructor.
    pub fn new_with_compactor(
        provider: Arc<dyn Provider>,
        config: AgentConfig,
        tools: Arc<ToolRegistry>,
        resolver: Arc<dyn ProviderResolver>,
        custom_compactor: Option<Arc<dyn Compactor>>,
    ) -> Self {
        Self::build_inner(provider, config, tools, resolver, custom_compactor)
    }

    /// Create an agent with an empty tool registry.
    pub fn new_empty(provider: Arc<dyn Provider>, config: AgentConfig) -> Self {
        Self::new(provider, config, Arc::new(ToolRegistry::new()))
    }

    /// Get the agent configuration (read guard)
    fn config(&self) -> parking_lot::RwLockReadGuard<'_, AgentInner> {
        self.inner.read()
    }

    /// Get a write guard for the agent inner state
    fn inner_mut(&self) -> parking_lot::RwLockWriteGuard<'_, AgentInner> {
        self.inner.write()
    }

    /// Get the current model ID
    pub fn model_id(&self) -> String {
        self.config().config.model_id.clone()
    }

    /// Get the active model's context window (tokens).
    ///
    /// Synced from the resolved model at construction and on every model
    /// switch — the authoritative value the per-run loop computes
    /// compaction thresholds against. Hosts use it for CTX displays.
    pub fn context_window(&self) -> usize {
        self.config().config.context_window
    }

    /// Get the agent configuration (full clone)
    pub fn get_config(&self) -> AgentConfig {
        self.config().config.clone()
    }

    /// Get a cheap clone of the configured todo state provider, if any.
    /// Used by hosts (e.g. the TUI) to observe todo phase changes without
    /// cloning the full [`AgentConfig`].
    pub fn todo_provider(&self) -> Option<std::sync::Arc<dyn crate::tools::TodoStateProvider>> {
        self.config().config.todo.clone()
    }

    /// Internal constructor shared by `new()`, `new_with_resolver()` and
    /// `new_with_compactor()`.
    fn build_inner(
        provider: Arc<dyn Provider>,
        config: AgentConfig,
        tools: Arc<ToolRegistry>,
        resolver: Arc<dyn ProviderResolver>,
        custom_compactor: Option<Arc<dyn Compactor>>,
    ) -> Self {
        // Resolve the initial model once: its real context window is the
        // source of truth for compaction thresholds (construction sites
        // pass a 128_000 placeholder). Overwrite only when positive —
        // resolvers that cannot resolve leave the configured value alone.
        let mut config = config;
        let resolved_model = resolver.resolve_model(&config.model_id);
        if let Some(model) = &resolved_model
            && model.context_window > 0
        {
            config.context_window = model.context_window;
        }

        let mut compaction_manager =
            CompactionManager::new(config.compaction_strategy.clone(), config.context_window);

        // Pre-initialize the LLM compactor if compaction is enabled
        // (unless a custom compactor replaces it — the Agent's own
        // manager follows the same replace semantics as the loop).
        if let Some(compactor) = &custom_compactor {
            compaction_manager.set_compactor(Arc::clone(compactor));
        } else if config.compaction_strategy != CompactionStrategy::Disabled
            && let Some(model) = resolved_model
        {
            let llm_compactor = Arc::new(LlmCompactor::new(model.clone(), Arc::clone(&provider)));
            compaction_manager.set_compactor(llm_compactor);
        }

        Self {
            inner: RwLock::new(AgentInner {
                config,
                provider,
                observability_dispatch: parking_lot::Mutex::new(Vec::new()),
            }),
            tools,
            state: SharedState::new(),
            compaction_manager,
            custom_compactor,
            hooks: parking_lot::RwLock::new(crate::config::AgentHooks::default()),
            is_running: Arc::new(AtomicBool::new(false)),
            resolver,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            auto_retry_enabled: Arc::new(AtomicBool::new(true)),
            auto_retry_cancel: Arc::new(AtomicBool::new(false)),
            auto_retry_notify: Arc::new(tokio::sync::Notify::new()),
            idle_notify: Arc::new(tokio::sync::Notify::new()),
            pending_model_switch: RwLock::new(None),
        }
    }

    /// Sync the agent's context window from a resolved model.
    ///
    /// The per-run loop reads `inner.config.context_window` fresh each run
    /// (the authoritative value — compaction thresholds, CTX display).
    /// Construction sites pass a 128_000 placeholder; the resolved model's
    /// real capacity overwrites it here. Zero-window models (LOCAL
    /// discovery placeholders) keep the previous value.
    ///
    /// The construction-time [`CompactionManager`] field is not updated:
    /// it only serves manual compaction (`compact_now`, window-independent)
    /// and is built with the already-synced window at construction.
    fn apply_context_window(&self, window: usize) {
        if window == 0 {
            return;
        }
        self.inner_mut().config.context_window = window;
    }

    /// Get a reference to the provider resolver.
    pub fn resolver(&self) -> &Arc<dyn ProviderResolver> {
        &self.resolver
    }
    /// Switch the model used for future LLM calls.
    ///
    /// Switch model mid-conversation.
    ///
    /// If the agent is currently running, the switch is deferred: the new
    /// model and provider are stored in `pending_model_switch` and applied
    /// automatically when the current loop finishes. This ensures the
    /// running loop completes with a consistent provider/model without
    /// interruption.
    ///
    /// If the agent is idle, the switch takes effect immediately.
    ///
    /// If the new model uses a different provider API, the conversation
    /// history is automatically transformed for cross-provider compatibility
    /// (e.g. thinking blocks are converted to `<thinking>` tags).
    ///
    /// # Arguments
    /// * `model_id` - New model ID in `provider/model` format
    ///
    /// # Returns
    /// `Ok(())` on success, or an error if the model/provider is unknown
    ///
    /// # Credentials
    /// The new provider is constructed via [`ProviderResolver::resolve_provider`],
    /// which is the single credential authority — the wired `AuthProvider`
    /// port (sync fast-path) supplies the API key. The old `api_key` parameter
    /// was removed in 0.55.0; see issues #39 and #40.
    pub fn switch_model(&self, model_id: &str) -> Result<()> {
        let new_model = self
            .resolver
            .resolve_model(model_id)
            .ok_or_else(|| Error::msg(format!("Model '{}' not found", model_id)))?;

        // Create the new provider via resolver
        let new_provider = self
            .resolver
            .resolve_provider(&new_model.provider)
            .ok_or_else(|| Error::msg(format!("Provider '{}' not found", new_model.provider)))?;

        // Detect API change
        let (old_api, needs_transform) = {
            let inner = self.config();
            let old_api = self
                .resolver
                .resolve_model(&inner.config.model_id)
                .map(|m| m.api)
                .unwrap_or(oxicode_ai::Api::AnthropicMessages);
            (old_api, old_api != new_model.api)
        };

        // If the agent is currently running, defer the switch.
        if self.is_running.load(Ordering::SeqCst) {
            tracing::info!(
                "[AGENT] Agent running, deferring model switch to '{}' until loop completes",
                model_id
            );
            *self.pending_model_switch.write() = Some(PendingModelSwitch {
                model_id: model_id.to_string(),
                provider: new_provider,
                needs_transform,
                old_api,
                new_api: new_model.api,
            });
            // Update config immediately so model_id() returns the new value,
            // but leave provider unchanged so the running loop keeps its provider.
            {
                let mut inner = self.inner_mut();
                inner.config.model_id = model_id.to_string();
            }
            self.apply_context_window(new_model.context_window);
            return Ok(());
        }

        // Agent is idle — apply immediately.
        if needs_transform {
            let messages = self.state.get_state().messages.clone();
            let transformed = transform_for_provider(&messages, &old_api, &new_model.api);
            self.state.update(|s| {
                s.replace_messages(transformed);
            });
        }

        {
            let mut inner = self.inner_mut();
            inner.config.model_id = model_id.to_string();
            inner.provider = new_provider;
        }
        self.apply_context_window(new_model.context_window);

        Ok(())
    }

    /// Switch the model using a pre-resolved `Model` object.
    ///
    /// This is useful when the caller has already looked up the model
    /// and optionally created the provider.
    ///
    /// Like [`switch_model`], if the agent is currently running, the switch
    /// is deferred until the current loop completes.
    ///
    /// # Credentials
    /// The new provider is constructed via [`ProviderResolver::resolve_provider`],
    /// the single credential authority (sync `AuthProvider` fast-path).
    /// The old `api_key` parameter was removed in 0.55.0; see issues #39/#40.
    ///
    /// [`switch_model`]: Agent::switch_model
    pub fn switch_to_model(&self, model: &oxicode_ai::Model) -> Result<()> {
        let model_id = format!("{}/{}", model.provider, model.id);
        let new_provider = self
            .resolver
            .resolve_provider(&model.provider)
            .ok_or_else(|| Error::msg(format!("Provider '{}' not found", model.provider)))?;

        // Detect API change
        let (old_api, needs_transform) = {
            let inner = self.config();
            let old_api = self
                .resolver
                .resolve_model(&inner.config.model_id)
                .map(|m| m.api)
                .unwrap_or(oxicode_ai::Api::AnthropicMessages);
            (old_api, old_api != model.api)
        };

        // If the agent is currently running, defer the switch.
        if self.is_running.load(Ordering::SeqCst) {
            tracing::info!(
                "[AGENT] Agent running, deferring model switch to '{}' until loop completes",
                model_id
            );
            *self.pending_model_switch.write() = Some(PendingModelSwitch {
                model_id: model_id.clone(),
                provider: new_provider,
                needs_transform,
                old_api,
                new_api: model.api,
            });
            {
                let mut inner = self.inner_mut();
                inner.config.model_id = model_id;
            }
            self.apply_context_window(model.context_window);
            return Ok(());
        }

        // Agent is idle — apply immediately.
        if needs_transform {
            let messages = self.state.get_state().messages.clone();
            let transformed = transform_for_provider(&messages, &old_api, &model.api);
            self.state.update(|s| {
                s.replace_messages(transformed);
            });
        }

        {
            let mut inner = self.inner_mut();
            inner.config.model_id = model_id;
            inner.provider = new_provider;
        }
        self.apply_context_window(model.context_window);

        Ok(())
    }

    /// Refresh credentials by re-resolving the current provider via the resolver.
    ///
    /// After the resolver-centric credential model (0.55.0), the provider
    /// instance is the single source of truth for API keys. To pick up
    /// credential changes — e.g. the user updated their auth store via the
    /// TUI overlay — call this to re-resolve the current provider and swap
    /// it in. The resolver consults the wired `AuthProvider` port on every
    /// call, so updates are reflected without rebuilding the engine.
    ///
    /// Returns `Ok(())` if a fresh provider was resolved and swapped, or an
    /// error if the resolver could not produce a provider (the existing
    /// provider is left untouched on error). Replaces the deprecated
    /// `refresh_api_key(&self, api_key)` from pre-0.55.0; see issues #39/#40.
    pub fn refresh_credentials(&self) -> Result<()> {
        let provider_name = {
            let inner = self.config();
            inner.config.model_id.split('/').next().map(str::to_string)
        };
        let name = provider_name.as_deref().unwrap_or("anthropic");
        let new_provider = self
            .resolver
            .resolve_provider(name)
            .ok_or_else(|| Error::msg(format!("Provider '{}' not found", name)))?;
        let mut inner = self.inner_mut();
        inner.provider = new_provider;
        Ok(())
    }

    /// Get a handle to the tool registry.
    pub fn tools(&self) -> Arc<ToolRegistry> {
        Arc::clone(&self.tools)
    }

    /// Get a snapshot of the current agent state.
    pub fn state(&self) -> AgentState {
        self.state.get_state()
    }

    /// Update agent state in-place. Used by compaction to replace messages.
    pub fn update_state(&self, f: impl FnOnce(&mut AgentState)) {
        self.state.update(f);
    }

    /// Reset agent state for a new conversation
    pub fn reset(&self) {
        self.state.reset();
    }

    /// Register a tool that the agent can invoke during a run.
    pub fn add_tool<T: AgentTool + 'static>(&self, tool: T) {
        self.tools.register(tool);
    }

    /// Update the system prompt for future interactions.
    pub fn set_system_prompt(&self, prompt: String) {
        self.inner_mut().config.system_prompt = Some(prompt);
    }

    /// Get the compaction manager
    pub fn compaction_manager(&self) -> &CompactionManager {
        &self.compaction_manager
    }
    /// Update the compaction strategy for future runs.
    ///
    /// The strategy is read fresh from the config at the start of each run
    /// (see `run_with_channel_inner`), so this takes effect on the next
    /// agent turn — never mid-run. Pair with `compaction_manager()` for
    /// manual compaction, which is unaffected by the strategy.
    pub fn set_compaction_strategy(&self, strategy: oxicode_ai::CompactionStrategy) {
        self.inner.write().config.compaction_strategy = strategy;
    }
    /// Get the compaction strategy that will be used on the next run.
    ///
    /// This reads from `inner.config` (mutable via `set_compaction_strategy`),
    /// **not** from the `compaction_manager` field (which retains its
    /// construction-time strategy). The agent loop reads from config fresh
    /// each run, so this is the authoritative value.
    pub fn compaction_strategy(&self) -> oxicode_ai::CompactionStrategy {
        self.inner.read().config.compaction_strategy.clone()
    }

    /// Run the agent with a prompt, collecting all events into a vector.
    ///
    /// Convenience wrapper around [`run_with_channel`](Self::run_with_channel) that gathers every
    /// [`AgentEvent`] produced during the run.
    pub async fn run(&self, prompt: String) -> Result<(Response, Vec<AgentEvent>)> {
        let mut events = Vec::new();
        let (tx, rx) = std::sync::mpsc::channel::<AgentEvent>();
        let result = self.run_with_channel(prompt, tx).await;
        while let Ok(event) = rx.recv() {
            events.push(event);
        }
        result.map(|r| (r, events))
    }

    /// Run the agent, delivering events through the provided channel.
    ///
    /// Delegates to the agent loop which implements the same 2-level agentic
    /// loop matching pi-mono's architecture:
    ///
    /// ```text
    /// AgentLoop.run_messages()
    ///   Outer loop (follow-up messages):
    ///     Inner loop (tool calls + steering):
    ///       1. Inject pending messages (steering)
    ///       2. Compaction check
    ///       3. Stream LLM response (with accumulated partial messages)
    ///       4. Execute tool calls if any
    ///       5. Emit turn_end
    ///       6. Check shouldStopAfterTurn
    ///       7. Poll steering messages
    ///     Check follow-up messages
    ///     Exit
    /// ```
    pub async fn run_with_channel(
        &self,
        prompt: String,
        tx: std::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<Response> {
        self.run_with_channel_message(
            oxicode_ai::Message::User(oxicode_ai::UserMessage::new(prompt)),
            tx,
        )
        .await
    }

    /// Run with an explicit user `Message` (supports image content blocks).
    /// Used by RPC `prompt` with images. The running-guard logic lives here;
    /// [`run_with_channel`](Self::run_with_channel) delegates after converting
    /// its String prompt into a text-only user message.
    pub async fn run_with_channel_message(
        &self,
        prompt: oxicode_ai::Message,
        tx: std::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<Response> {
        // pi-mono: Agent.prompt() throws if activeRun exists.
        // Prevent concurrent runs that would corrupt shared state.
        if self
            .is_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(Error::msg("Agent is already running"));
        }

        // Drop guard ensures is_running is cleared even on panic.
        struct RunningGuard<'a>(&'a AtomicBool, &'a tokio::sync::Notify);
        impl Drop for RunningGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
                self.1.notify_waiters();
            }
        }
        let _guard = RunningGuard(&self.is_running, &self.idle_notify);
        self.reset_cancel();

        self.run_with_channel_inner(prompt, tx).await
    }

    /// Inner implementation of run_with_channel, called after the running guard is set.
    async fn run_with_channel_inner(
        &self,
        prompt: oxicode_ai::Message,
        tx: std::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<Response> {
        use crate::agent_loop::AgentLoop;

        let (
            provider,
            system_prompt,
            temperature,
            max_tokens,
            compaction_strategy,
            context_window,
            workspace_dir,
        ) = {
            let inner = self.inner.read();
            (
                Arc::clone(&inner.provider) as Arc<dyn Provider>,
                inner.config.system_prompt.clone(),
                inner.config.temperature,
                inner.config.max_tokens,
                inner.config.compaction_strategy.clone(),
                inner.config.context_window,
                inner.config.workspace_dir.clone(),
            )
        }; // release read lock

        // Build AgentLoopConfig from Agent's config
        let loop_config = crate::agent_loop::config::AgentLoopConfig {
            model_id: self.model_id(),
            system_prompt,
            temperature: temperature.unwrap_or(1.0) as f32,
            max_tokens: max_tokens.unwrap_or(4096) as u32,
            tool_execution: crate::config::ToolExecutionMode::Sequential,
            compaction_strategy,
            compaction_instruction: None,
            compactor: self.custom_compactor.clone(),
            context_window,
            session_id: self.config().config.session_id.clone(),
            transport: None,
            compact_on_start: false,
            max_retry_delay_ms: None,
            auto_retry_enabled: true,
            auto_retry_max_attempts: 3,
            auto_retry_base_delay_ms: 1000,
            workspace_dir,
            provider_options: self.config().config.provider_options.clone(),
            on_compaction: None,
            ttsr_engine: self.config().config.ttsr_engine.clone(),
            memory: self.config().config.memory.clone(),
            todo: self.config().config.todo.clone(),
            todo_reminders_enabled: self.config().config.todo_reminders_enabled,
            todo_reminders_max: self.config().config.todo_reminders_max,
            todo_eager_mode: self.config().config.todo_eager_mode,
            agent_pool: self.config().config.agent_pool.clone(),
            url_resolver: self.config().config.url_resolver.clone(),
            lsp: self.config().config.lsp.clone(),
            snapshot_store: self.config().config.snapshot_store.clone(),
            max_tool_result_bytes: self.config().config.max_tool_result_bytes,
            subagent_runner: self.config().config.subagent_runner.clone(),
            subagent_depth: self.config().config.subagent_depth,
            mode: self.config().config.mode,
            steering_mode: self.config().config.steering_mode,
            follow_up_mode: self.config().config.follow_up_mode,
            ..Default::default()
        };

        // Create AgentLoop. We give it a NEW SharedState and sync back after.
        // (SharedState is not Clone, so we create a fresh one from current state)
        let fresh_state = crate::state::SharedState::new();
        let current = self.state.get_state();
        fresh_state.update(|s| {
            *s = current;
        });

        let mut agent_loop = AgentLoop::new_with_resolver(
            provider,
            loop_config,
            Arc::clone(&self.tools),
            fresh_state,
            Arc::clone(&self.resolver),
        );

        // Add the user prompt to Agent.state() AFTER fresh_state is created.
        // fresh_state got a copy of the pre-prompt state, so run_loop will
        // add the prompt to fresh_state independently via initial_prompts.
        // But persist_session() reads Agent.state() (not fresh_state), so it
        // needs the user prompt there to write it to the session file.
        // Sync happens at AgentEnd (after run_loop completes), where
        // Agent.state is overwritten with fresh_state (which has all messages).
        self.state.update(|s| {
            s.messages.push(prompt.clone());
        });

        // Pre-populate steering/follow-up from hooks
        {
            let hooks = self.hooks.read();
            if let Some(ref get_steering) = hooks.get_steering_messages {
                for msg in get_steering() {
                    agent_loop.steer(msg);
                }
            }
            if let Some(ref get_follow_up) = hooks.get_follow_up_messages {
                for msg in get_follow_up() {
                    agent_loop.follow_up(msg);
                }
            }

            // Store hooks on AgentLoop so they can be polled each turn
            // to pick up new messages injected during the run.
            if let Some(ref get_steering) = hooks.get_steering_messages {
                agent_loop.set_steering_hook(Arc::clone(get_steering));
            }
            if let Some(ref get_follow_up) = hooks.get_follow_up_messages {
                agent_loop.set_follow_up_hook(Arc::clone(get_follow_up));
            }
        }
        let mut al = agent_loop;

        // Wire should_stop_after_turn hook: share AgentLoop's external_stop
        // Arc with the emit callback. When the hook fires (Ctrl+C detected),
        // it sets ext_stop. AgentLoop checks this in should_stop_after_turn()
        // AND during streaming (streaming.rs checks external_stop each event).
        //
        // Arc<dyn Fn> can be cloned, so we read it without consuming.
        let maybe_hook = {
            let hooks_r = self.hooks.read();
            hooks_r.should_stop_after_turn.clone()
        };
        let ext_stop = al.external_stop().clone();
        let cancel_flag = self.cancel_flag.clone();

        // Share cancel_flag with AgentLoop so the streaming loop can check
        // it directly in the periodic timer — no emit callback required.
        // This closes the gap where cancel() was ineffective when the
        // provider stream produced no events.
        al.set_cancel_signal(self.cancel_flag.clone());
        let (ar_enabled, ar_cancel, ar_notify) = self.auto_retry_state();
        al.set_auto_retry_state(ar_enabled, ar_cancel, ar_notify);

        // Create emit callback that sends through the channel.
        // AgentLoop calls this synchronously. UnboundedSender::send() is
        // non-blocking and never drops events (unlike try_send on bounded).
        let tx_emit = tx.clone();

        // Snapshot the observability_dispatch list once per run. This avoids
        // holding an Agent lock on the emit-fn hot path while still letting
        // SDK consumers register new dispatchers at any time (registers after
        // this snapshot will fire on the next run).
        let dispatch_handlers: Vec<EventDispatchFn> =
            { self.inner.read().observability_dispatch.lock().clone() };
        tracing::info!("[AGENT] Starting agent run with channel");
        let result = al
            .run_message(prompt.clone(), move |event: AgentEvent| {
                // Forward event to channel (std::sync::mpsc — send from sync context)
                tracing::info!("[AGENT-EMIT] Event: {:?}", std::mem::discriminant(&event));
                if let Err(e) = tx_emit.send(event.clone()) {
                    tracing::error!(
                        "[AGENT-EMIT] Failed to send agent event to channel: {:?}",
                        e
                    );
                } else {
                    tracing::info!("[AGENT-EMIT] Successfully sent event");
                }

                // Propagate cancellation from Agent::cancel() → external_stop.
                // This runs on every event, ensuring the streaming loop detects
                // cancellation promptly.
                if cancel_flag.load(Ordering::SeqCst) {
                    ext_stop.store(true, Ordering::SeqCst);
                }

                // Fan out to SDK-side observability handlers (Tracer,
                // CostTracker, ...). The dispatch list is snapshotted at
                // run-start so we hold Arc clones, not a lock. This means
                // handlers added mid-run do not fire until the next run.
                for handler in dispatch_handlers.iter() {
                    handler(event.clone());
                }
                // Propagate should_stop → external_stop on every event, not
                // just TurnEnd. The TUI hook only checks should_stop_flag.load(),
                // so the context contents are irrelevant for non-TurnEnd events.
                // This ensures streaming.rs detects cancellation immediately
                // when the user presses Ctrl+C mid-stream.
                if let Some(ref hook) = maybe_hook {
                    let ctx = ShouldStopAfterTurnContext {
                        message: match &event {
                            AgentEvent::TurnEnd {
                                assistant_message: oxicode_ai::Message::Assistant(a),
                                ..
                            } => a.clone(),
                            _ => oxicode_ai::AssistantMessage::new(
                                oxicode_ai::Api::OpenAiCompletions,
                                "agent",
                                "agent-model",
                            ),
                        },
                        tool_results: match &event {
                            AgentEvent::TurnEnd { tool_results, .. } => tool_results.clone(),
                            _ => Vec::new(),
                        },
                        iteration: 0,
                    };
                    if hook(&ctx) {
                        ext_stop.store(true, Ordering::SeqCst);
                    }
                }
            })
            .await;

        match result {
            Ok(_events) => {
                // Sync state back from AgentLoop
                let loop_state = al.state().get_state();
                self.state.update(|s| {
                    *s = loop_state;
                });

                // Apply any pending model switch that was deferred during the run.
                // This transforms messages (if cross-provider) and swaps the provider
                // so the next run uses the new model.
                self.apply_pending_model_switch();

                // Extract final response text from state
                let state = self.state.get_state();
                let final_text = state
                    .messages
                    .iter()
                    .rev()
                    .find_map(|m| match m {
                        oxicode_ai::Message::Assistant(a) => {
                            a.content.iter().find_map(|b| match b {
                                oxicode_ai::ContentBlock::Text(t) => Some(t.text.clone()),
                                _ => None,
                            })
                        }
                        _ => None,
                    })
                    .unwrap_or_default();

                let stop_reason = state.stop_reason.unwrap_or(StopReason::Stop);

                Ok(Response {
                    content: final_text,
                    stop_reason,
                })
            }
            Err(e) => {
                // Apply pending model switch even on error so the next run
                // uses the new model.
                self.apply_pending_model_switch();
                Err(e)
            }
        }
    }

    // ── Helper methods for the agentic loop ────────────────────────

    /// Set hooks for the agent loop.
    pub fn set_hooks(&self, hooks: crate::config::AgentHooks) {
        let mut h = self.hooks.write();
        *h = hooks;
    }

    /// Register a side-dispatch closure called for every `AgentEvent`
    /// emitted by `run`, `run_with_channel`, `run_streaming`,
    /// `run_tokio_stream`, and `continue_with`.
    ///
    /// Multiple calls stack: every registered closure is invoked on
    /// every event. Closures run synchronously on the agent-loop emit
    /// thread, so they must be cheap and non-blocking. Long work
    /// should be spawned off (e.g. `tokio::spawn`) by the closure
    /// itself.
    ///
    /// Used by `oxicode-sdk` to bridge observability types
    /// (`Tracer`, `CostTracker`, `AuditLog`, `Authorizer` /
    /// `AccessGate`) into the runtime without leaking those types
    /// into `oxicode-agent`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// agent.add_observability_dispatch(|event| match event {
    ///     AgentEvent::TurnStart { turn_number } => {
    ///         // open a span
    ///     }
    ///     AgentEvent::Usage { input_tokens, output_tokens } => {
    ///         // record cost
    ///     }
    ///     _ => {}
    /// });
    /// ```
    pub fn add_observability_dispatch(&self, f: impl Fn(AgentEvent) + Send + Sync + 'static) {
        let guard = self.inner.write();
        let mut slot = guard.observability_dispatch.lock();
        slot.push(Arc::new(f));
    }

    /// Request cancellation of the current agent run.
    ///
    /// Sets a shared `cancel_flag` that is propagated to the `AgentLoop`'s
    /// `external_stop` on every event AND polled every ~500ms by the
    /// streaming loop's periodic check. This ensures cancellation is
    /// detected quickly even when the provider stream is completely hung
    /// (no events arriving).
    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
    }

    /// Whether a run is currently in flight (upstream pi: the agent's
    /// streaming state).
    pub fn is_streaming(&self) -> bool {
        self.is_running.load(Ordering::SeqCst)
    }

    /// Resolves once no run is in flight (upstream pi: `Agent.waitForIdle`).
    ///
    /// Returns immediately when the agent is idle. The wakeup is armed
    /// before the flag is re-checked, so a run finishing concurrently
    /// cannot be missed.
    pub async fn wait_for_idle(&self) {
        loop {
            let notified = self.idle_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.is_running.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    /// Toggle auto-retry at runtime (affects the next retry decision in an
    /// active run; does not interrupt an in-progress retry sleep — use
    /// [`Self::cancel_auto_retry`] for that).
    pub fn set_auto_retry(&self, enabled: bool) {
        self.auto_retry_enabled.store(enabled, Ordering::SeqCst);
    }

    /// Abort any in-progress auto-retry wait immediately. The running turn
    /// ends without retrying the error.
    pub fn cancel_auto_retry(&self) {
        self.auto_retry_cancel.store(true, Ordering::SeqCst);
        self.auto_retry_notify.notify_waiters();
    }

    /// Shared auto-retry state (enabled + cancel + notify) for injection
    /// into an ephemeral `AgentLoop` at run-start.
    pub(crate) fn auto_retry_state(
        &self,
    ) -> (Arc<AtomicBool>, Arc<AtomicBool>, Arc<tokio::sync::Notify>) {
        (
            Arc::clone(&self.auto_retry_enabled),
            Arc::clone(&self.auto_retry_cancel),
            Arc::clone(&self.auto_retry_notify),
        )
    }

    /// Reset the cancellation flag before starting a new run.
    pub fn reset_cancel(&self) {
        self.cancel_flag.store(false, Ordering::SeqCst);
    }

    /// Apply any pending model switch that was deferred during a running loop.
    ///
    /// Called after `run_with_channel_inner` completes (success or error).
    /// Transforms messages for cross-provider switches and swaps the provider
    /// so the next run uses the new model.
    fn apply_pending_model_switch(&self) {
        let pending = self.pending_model_switch.write().take();
        if let Some(pending) = pending {
            tracing::info!(
                "[AGENT] Applying deferred model switch to '{}' (transform={})",
                pending.model_id,
                pending.needs_transform
            );

            // Transform messages if cross-provider
            if pending.needs_transform {
                let messages = self.state.get_state().messages.clone();
                let transformed =
                    transform_for_provider(&messages, &pending.old_api, &pending.new_api);
                self.state.update(|s| {
                    s.replace_messages(transformed);
                });
            }

            // Swap the provider
            let mut inner = self.inner_mut();
            inner.provider = pending.provider;
            // model_id was already updated in switch_model()
        }
    }

    /// Run the agent, invoking `on_event` for each [`AgentEvent`] produced.
    ///
    /// Blocking convenience wrapper suitable for callers that prefer a
    /// callback-based API over a channel.
    pub async fn run_streaming<F>(&self, prompt: String, mut on_event: F) -> Result<Response>
    where
        F: FnMut(AgentEvent) + Send,
    {
        let (tx, rx) = std::sync::mpsc::channel::<AgentEvent>();
        let result = self.run_with_channel(prompt, tx).await;
        while let Ok(event) = rx.recv() {
            on_event(event);
        }
        result
    }

    // ── Session persistence ────────────────────────────────────────

    /// Export the agent state as a JSON value.
    ///
    /// The serialized state includes conversation messages, token counts,
    /// iteration progress, and stop reason. Use [`import_state`] to restore.
    ///
    /// [`import_state`]: Agent::import_state
    pub fn export_state(&self) -> Result<serde_json::Value> {
        let state = self.state.get_state();
        serde_json::to_value(&state).map_err(|e| Error::msg(format!("State export failed: {}", e)))
    }

    /// Import agent state from a JSON value.
    ///
    /// Restores conversation history, token counts, and iteration progress.
    /// Typically used together with [`export_state`] for session persistence.
    ///
    /// [`export_state`]: Agent::export_state
    pub fn import_state(&self, value: serde_json::Value) -> Result<()> {
        let state: AgentState = serde_json::from_value(value)
            .map_err(|e| Error::msg(format!("State import failed: {}", e)))?;
        self.state.update(|s| *s = state);
        Ok(())
    }

    // ── Session continuation ───────────────────────────────────────

    /// Continue the current session with a new prompt.
    ///
    /// Unlike `run()`, which can be used on a fresh agent, `continue_with`
    /// preserves the existing conversation state and appends the new prompt.
    /// This enables multi-turn interactions within the same session.
    pub async fn continue_with(&self, prompt: String) -> Result<(Response, Vec<AgentEvent>)> {
        let mut events = Vec::new();
        let (tx, rx) = std::sync::mpsc::channel::<AgentEvent>();
        let result = self.run_with_channel(prompt, tx).await;
        while let Ok(event) = rx.recv() {
            events.push(event);
        }
        result.map(|r| (r, events))
    }

    // ── Tokio-native streaming ─────────────────────────────────────

    /// Run the agent with tokio-native event streaming.
    ///
    /// Returns a `tokio::sync::mpsc::Receiver` for events and a
    /// `JoinHandle` for the response. This is the preferred API for
    /// async runtimes (WebSocket/SSE gateways, tokio-based servers).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (rx, handle) = agent.run_tokio_stream("Explain Rust".into()).await?;
    /// while let Some(event) = rx.recv().await {
    ///     println!("Event: {:?}", event.type_name());
    /// }
    /// let response = handle.await??;
    /// ```
    pub async fn run_tokio_stream(
        &self,
        prompt: String,
    ) -> Result<(
        tokio::sync::mpsc::Receiver<AgentEvent>,
        tokio::task::JoinHandle<Result<Response>>,
    )> {
        let (tx, rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);

        if self
            .is_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(Error::msg("Agent is already running"));
        }

        let should_stop_hook = self.hooks.read().should_stop_after_turn.clone();

        let inner = self.inner.read().clone();
        let tools = Arc::clone(&self.tools);
        let resolver = Arc::clone(&self.resolver);

        // Build AgentLoopConfig
        let loop_config = crate::agent_loop::config::AgentLoopConfig {
            model_id: inner.config.model_id.clone(),
            system_prompt: inner.config.system_prompt.clone(),
            temperature: inner.config.temperature.unwrap_or(1.0) as f32,
            max_tokens: inner.config.max_tokens.unwrap_or(4096) as u32,
            tool_execution: crate::config::ToolExecutionMode::Sequential,
            compaction_strategy: inner.config.compaction_strategy.clone(),
            compaction_instruction: None,
            compactor: self.custom_compactor.clone(),
            context_window: inner.config.context_window,
            session_id: inner.config.session_id.clone(),
            transport: None,
            compact_on_start: false,
            max_retry_delay_ms: None,
            auto_retry_enabled: true,
            auto_retry_max_attempts: 3,
            auto_retry_base_delay_ms: 1000,
            workspace_dir: inner.config.workspace_dir.clone(),
            provider_options: inner.config.provider_options.clone(),
            on_compaction: None,
            ttsr_engine: inner.config.ttsr_engine.clone(),
            max_tool_result_bytes: inner.config.max_tool_result_bytes,
            subagent_runner: inner.config.subagent_runner.clone(),
            subagent_depth: inner.config.subagent_depth,
            memory: inner.config.memory.clone(),
            todo: inner.config.todo.clone(),
            todo_reminders_enabled: inner.config.todo_reminders_enabled,
            todo_eager_mode: inner.config.todo_eager_mode,
            todo_reminders_max: inner.config.todo_reminders_max,
            agent_pool: inner.config.agent_pool.clone(),
            url_resolver: inner.config.url_resolver.clone(),
            lsp: inner.config.lsp.clone(),
            mode: inner.config.mode,
            snapshot_store: inner.config.snapshot_store.clone(),
            steering_mode: inner.config.steering_mode,
            follow_up_mode: inner.config.follow_up_mode,
            ..Default::default()
        };

        let provider: Arc<dyn Provider> = Arc::clone(&inner.provider);

        // Share the SAME SharedState (Arc<RwLock<AgentState>>) with the
        // agent loop so that state mutations inside the spawned task are
        // visible through self.state() without an explicit sync step.
        //
        // Unlike run_with_channel_inner which creates a fresh SharedState
        // and syncs back on completion, the tokio streaming API cannot
        // access `self` inside the `'static` spawned task, so we share
        // the underlying Arc instead.
        //
        // Pre-load current state into the shared Arc (in case it was
        // modified by a previous run that used a different SharedState).
        let shared_state = self.state.clone();

        let mut agent_loop = crate::agent_loop::AgentLoop::new_with_resolver(
            provider,
            loop_config,
            tools,
            shared_state.clone(),
            resolver,
        );

        let maybe_hook = should_stop_hook;
        let ext_stop = agent_loop.external_stop().clone();
        let (ar_enabled, ar_cancel, ar_notify) = self.auto_retry_state();
        agent_loop.set_auto_retry_state(ar_enabled, ar_cancel, ar_notify);

        // Clone the is_running Arc so the spawned task can clear it.
        let is_running_flag = Arc::clone(&self.is_running);
        let idle_notify = Arc::clone(&self.idle_notify);

        // Snapshot the observability_dispatch list before the spawned
        // task. The future is `'static` and cannot borrow `&self`,
        // so we take the snapshot at run-start on the regular borrow
        // stack and move the resulting Arc-clones into the task.
        let dispatch_handlers: Vec<EventDispatchFn> = {
            let guard = self.inner.read();
            guard.observability_dispatch.lock().clone()
        };

        let handle = tokio::task::spawn(async move {
            // Guard ensures is_running is cleared even if the task panics.
            // Without this, a panic mid-stream leaves is_running=true and
            // blocks all future runs (the compare_exchange at entry fails).
            struct RunningGuard(Arc<AtomicBool>, Arc<tokio::sync::Notify>);
            impl Drop for RunningGuard {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                    self.1.notify_waiters();
                }
            }
            let _guard = RunningGuard(is_running_flag, idle_notify);

            let result = agent_loop
                .run(prompt, move |event: AgentEvent| {
                    // Forward to tokio channel (non-blocking)
                    let _ = tx.try_send(event.clone());

                    // Fan out to SDK-side observability handlers
                    // (Tracer, CostTracker, ...).
                    for handler in dispatch_handlers.iter() {
                        handler(event.clone());
                    }
                    // Propagate should_stop → external_stop on every event,
                    // not just TurnEnd. See run_with_channel_inner for rationale.
                    if let Some(hook) = &maybe_hook {
                        let ctx = ShouldStopAfterTurnContext {
                            message: match &event {
                                AgentEvent::TurnEnd {
                                    assistant_message: oxicode_ai::Message::Assistant(a),
                                    ..
                                } => a.clone(),
                                _ => oxicode_ai::AssistantMessage::new(
                                    oxicode_ai::Api::OpenAiCompletions,
                                    "agent",
                                    "agent-model",
                                ),
                            },
                            tool_results: match &event {
                                AgentEvent::TurnEnd { tool_results, .. } => tool_results.clone(),
                                _ => Vec::new(),
                            },
                            iteration: 0,
                        };
                        if hook(&ctx) {
                            ext_stop.store(true, Ordering::SeqCst);
                        }
                    }
                })
                .await;

            // _guard dropped here: clears is_running on normal exit or panic.

            match result {
                Ok(_events) => {
                    // State is already shared via the same SharedState Arc,
                    // so self.state() will reflect all mutations.
                    Ok(Response {
                        content: String::new(),
                        stop_reason: StopReason::Stop,
                    })
                }
                Err(e) => Err(e),
            }
        });

        Ok((rx, handle))
    }
}
