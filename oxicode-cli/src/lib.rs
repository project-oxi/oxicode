// oxicode: CLI coding harness
// Migrating to oxicode-vtui: relaxed linting for vendored code compatibility.
#![allow(
    missing_docs,
    dead_code,
    clippy::field_reassign_with_default,
    clippy::unwrap_used,
    clippy::let_and_return,
    clippy::borrow_interior_mutable_const,
    clippy::derivable_impls,
    clippy::new_without_default,
    unknown_lints
)]
//! oxicode: CLI coding harness
//!
//! This crate provides the main application logic for the oxicode CLI.

// ─── Root-level entry modules ───────────────────────────────────────────────
// cli must be pub for main.rs binary
pub mod bootstrap;
pub mod cli;
pub mod internal_urls;
pub mod lsp;
pub mod main_dispatch;
pub mod mcp_credentials;
pub mod print_mode;
pub mod services;
pub mod setup_wizard;
pub mod store;

// ─── Directory groups ───────────────────────────────────────────────────────
pub(crate) mod app;
pub(crate) mod context;
pub mod discovery;
pub mod extensions; // public for main.rs
pub(crate) mod infra;
pub(crate) mod media;
pub(crate) mod prompt;
pub mod rpc_mode;
pub(crate) mod skills;
pub mod storage; // public for main.rs (packages)
// Re-exports from storage for main.rs
pub use storage::packages::PackageManager;
pub use storage::packages::ResourceKind;
pub mod tools;
pub(crate) mod ui;
pub(crate) mod util;

///
/// This is the **new entry point** for oxicode-cli run modes. It uses
/// `oxicode-fs` adapters and `OxicodeBuilder::with_port_*` to construct an
/// `Oxicode` with persistence, auth, config, and skills wired. The legacy
/// `App::new` path is still used by the interactive TUI during the
/// migration period.
///
/// ```
/// use oxicode::build_oxicode_engine;
/// # async fn _example() -> anyhow::Result<()> {
/// let oxicode = build_oxicode_engine(None, None).await?;
/// println!("providers: {}", oxicode.providers().names().len());
/// # Ok(()) }
/// ```
pub async fn build_oxicode_engine(
    embedding_provider: Option<std::sync::Arc<dyn oxicode_sdk::ports::EmbeddingProvider>>,
    hook_runner: Option<std::sync::Arc<dyn oxicode_sdk::ports::HookRunner>>,
) -> anyhow::Result<oxicode_sdk::Oxicode> {
    let paths = services::OxicodePaths::default_paths()?;
    services::build_oxicode(&paths, embedding_provider, hook_runner).await
}

/// Self-check the wired port implementations. Prints a one-line summary
/// per port and returns `Ok(())` if all are reachable.
///
/// Triggered by the `OXICODE_PORT_CHECK=1` environment variable from
/// `oxicode-cli/src/main.rs`. Useful for verifying the new composition root
/// without disturbing the legacy `App::new` path.
pub async fn run_port_check() -> anyhow::Result<()> {
    let oxicode = build_oxicode_engine(None, None).await?;
    let ports = oxicode.ports();

    let entries = ports.state.list("").await?;
    println!("[state]    entries: {}", entries.len());

    // Auth
    let providers = ports.auth.list_providers().await?;
    println!("[auth]     providers with credentials: {:?}", providers);

    // Config
    let keys = ports.config.list()?;
    println!("[config]   keys: {}", keys.len());

    // Skills
    let skills = ports.skills.list().await?;
    println!("[skills]   {} skill(s) discovered", skills.len());
    for s in &skills {
        println!("           - {}: {}", s.name, s.description);
    }

    // Event bus / memory / etc — all noop unless registered
    let _ = ports
        .event_bus
        .publish(&"port-check".to_string(), serde_json::json!({"ok": true}))
        .await;
    println!("[event-bus] publish ok (noop bus if not registered)");

    println!("\nport check: ok");
    Ok(())
}

/// Context for compaction operations, passed to extension hooks
#[derive(Debug, Clone)]
pub struct CompactionContext {
    /// Messages being compacted
    pub messages_count: usize,
    /// Estimated tokens before compaction
    pub tokens_before: usize,
    /// Target token count after compaction
    pub target_tokens: usize,
    /// Strategy being used
    pub strategy: String,
}

impl CompactionContext {
    /// Create a new compaction context
    pub fn new(
        messages_count: usize,
        tokens_before: usize,
        target_tokens: usize,
        strategy: impl Into<String>,
    ) -> Self {
        Self {
            messages_count,
            tokens_before,
            target_tokens,
            strategy: strategy.into(),
        }
    }

    /// Get expected compression ratio
    pub fn compression_ratio(&self) -> f32 {
        if self.tokens_before == 0 {
            return 1.0;
        }
        self.target_tokens as f32 / self.tokens_before as f32
    }
}

// ─── Module-level imports ────────────────────────────────────────────────────
use crate::store::settings::Settings;
use anyhow::{Error, Result};
use oxicode_agent::{Agent, AgentConfig, AgentEvent};
use parking_lot::RwLock;
use skills::SkillManager;
use std::collections::VecDeque;
use std::sync::Arc;

/// Pre-built session state threaded into the agent hook chain.
///
/// Constructed by the cli BEFORE `oxicode.agent(...).build()` so the
/// middleware pipeline (`with_port_hooks`) and session closures
/// (`with_session_hooks`) are composed into a single `AgentHooks`
/// instance — see the single-`set_hooks` invariant (only
/// [`AgentBuilder::build`](oxicode_sdk::AgentBuilder::build) calls
/// `set_hooks`). The same `Arc`s are cloned into `AgentSession` so the
/// runtime queues, stop flag, and agent hooks all observe the same
/// state across teardown/recreate cycles.
#[derive(Clone)]
pub struct SessionState {
    /// Set when the user (or `Ctrl+C` handler) requests the agent to
    /// stop after the current turn. Consulted by
    /// [`oxicode_sdk::agent_builder::SessionHookClosures::should_stop_after_turn`].
    pub should_stop: Arc<std::sync::atomic::AtomicBool>,
    /// Steering messages — drained at the start of each turn (until empty).
    pub steering: Arc<RwLock<VecDeque<oxicode_sdk::Message>>>,
    /// Follow-up messages — drained after the agent has stopped.
    pub follow_up: Arc<RwLock<VecDeque<oxicode_sdk::Message>>>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            should_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            steering: Arc::new(RwLock::new(VecDeque::new())),
            follow_up: Arc::new(RwLock::new(VecDeque::new())),
        }
    }
}

// ─── Application state ───────────────────────────────────────────────────────

/// Holds an `Oxicode` engine (composition root) and a single `Agent` built
/// from it. The legacy `App::new(settings)` constructor is **gone**;
/// use [`App::from_oxicode`] with a wired `Oxicode` from
/// [`build_oxicode_engine`].
pub struct App {
    oxicode: oxicode_sdk::Oxicode,
    agent: Arc<Agent>,
    settings: Settings,
    skills: RwLock<SkillManager>,
    active_skills: RwLock<Vec<String>>,
    wasm_ext: Option<std::sync::Arc<crate::extensions::WasmExtensionManager>>,
    ask_bridge: Option<std::sync::Arc<oxicode_agent::tools::ask::AskBridge>>,
    /// Shared local issue store (`.oxicode/issues/`). Cloned cheaply (inner `Arc`).
    /// Used by the agent `issue` tool, the TUI indicator, and the `oxicode issue`
    /// CLI subcommand.
    issue_store: Option<crate::store::issues::FileIssueStore>,
    /// Process-wide liveness identity used by every issue-ownership surface
    /// in this process (agent tool's `ToolContext.session_id`, TUI panel,
    /// slash-command `/issue` handlers). See
    /// [`crate::store::issues::liveness::TUI_OWNERSHIP_ID`] for the TUI value.
    ownership_session_id: String,
    /// Alive-lock held for the lifetime of `App`. Dropped with `App`, releasing
    /// the OS-held flock so any other process sees this session as dead once
    /// we exit (including `kill -9` / crash / normal exit). Only held when
    /// `issue_store` is available.
    #[allow(dead_code)]
    liveness_guard: Option<crate::store::issues::liveness::AliveGuard>,
    /// Cached `default` persona body, resolved once in `from_oxicode` so
    /// synchronous prompt rebuilds can reuse it without awaiting the port.
    persona_body: RwLock<Option<String>>,
    /// Pre-built session queues + stop flag. Cloned into `AgentSession` so
    /// the runtime and the agent's session-level closures share the SAME
    /// state (see [`SessionState`] doc).
    session_state: SessionState,
}
// ─── System prompt builder ───────────────────────────────────────────────────
fn build_system_prompt(
    thinking_level: crate::store::settings::ThinkingLevel,
    skill_contents: &[String],
    persona_body: Option<&str>,
) -> String {
    let skills: Vec<prompt::system_prompt::Skill> = skill_contents
        .iter()
        .enumerate()
        .map(|(i, content)| prompt::system_prompt::Skill {
            name: format!("skill-{}", i),
            content: content.clone(),
        })
        .collect();

    let options = prompt::system_prompt::BuildSystemPromptOptions {
        custom_prompt: prompt::system_prompt::thinking_level_prompt(thinking_level),
        skills,
        cwd: std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
        persona_prompt: persona_body.map(|s| s.to_string()),
        ..Default::default()
    };

    prompt::system_prompt::build_system_prompt(&options)
}

// ─── App implementation ─────────────────────────────────────────────────────

impl App {
    /// Build an `App` from a wired `Oxicode` engine and a settings object.
    ///
    /// The `Oxicode` should be created via [`build_oxicode_engine`] (or
    /// `services::build_oxicode`) so that all 11 ports are wired. The
    /// settings hold the user's runtime configuration (model, thinking
    /// level, etc.).
    ///
    /// `ownership_session_id` is the per-process liveness identity used by
    /// the agent's `issue` tool (`ToolContext.session_id`), the TUI panel,
    /// and the `/issue` slash command. In TUI mode this MUST equal
    /// [`crate::store::issues::liveness::TUI_OWNERSHIP_ID`] so the panel and
    /// agent see the same flock holder. In print / RPC mode, a stable
    /// process-scoped id (e.g. `proc-<pid>-<uuid>`) is appropriate.
    ///
    /// `session_state` is the pre-built [`SessionState`] passed into the
    /// agent's `with_session_hooks` call. When `None`, fresh state is
    /// constructed (the default for tests and most call sites — bootstrap
    /// constructs it explicitly so the runtime and AgentSession share the
    /// SAME queues + stop flag).
    pub async fn from_oxicode(
        oxicode: oxicode_sdk::Oxicode,
        settings: Settings,
        ownership_session_id: String,
        session_state: Option<SessionState>,
    ) -> Result<Self> {
        let session_state = session_state.unwrap_or_default();
        // Resolve the default persona once from the wired
        // PersonaProvider port. The body flows into the system prompt;
        // `preferred_model` overrides the settings default when no
        // other override exists.
        let persona = match oxicode.ports().personas.get("default").await {
            Ok(Some(p)) if !p.system_prompt.trim().is_empty() => Some(p),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(error = %e, "default persona lookup failed");
                None
            }
        };

        let model_id = persona
            .as_ref()
            .and_then(|p| p.preferred_model.clone())
            .or_else(|| settings.effective_model(None))
            .unwrap_or_default();
        // Provider-name and api_key lookups removed in 0.55.0 — the SDK
        // resolver consults the wired AuthProvider port directly.

        let skills_dir = SkillManager::skills_dir().unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".oxicode")
                .join("skills")
        });
        let skills = SkillManager::load_from_dir(&skills_dir).unwrap_or_else(|e| {
            tracing::debug!("Skills not loaded: {}", e);
            SkillManager::new()
        });

        let body_str = persona.as_ref().map(|p| p.system_prompt.clone());
        let system_prompt = build_system_prompt(settings.thinking_level, &[], body_str.as_deref());
        let compaction_strategy = if settings.auto_compaction {
            oxicode_sdk::CompactionStrategy::Threshold(0.8)
        } else {
            oxicode_sdk::CompactionStrategy::Disabled
        };

        let config = AgentConfig {
            name: "oxicode".to_string(),
            description: Some("oxicode CLI agent".to_string()),
            model_id: model_id.clone(),
            system_prompt: Some(system_prompt),
            timeout_seconds: settings.tool_timeout_seconds,
            temperature: settings.effective_temperature(),
            max_tokens: settings.effective_max_tokens(),
            compaction_strategy,
            compaction_instruction: None,
            context_window: 128_000,
            workspace_dir: Some(
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            ),
            output_mode: None,
            provider_options: None,
            session_id: Some(ownership_session_id.clone()),
            ttsr_engine: None,
            memory: None,
            todo: None,
            agent_pool: None,
            url_resolver: Some(Arc::new(oxicode_sdk::SdkUrlResolver::new(
                oxicode.ports().url_router.clone(),
            ))),
            // LSP: lazy-spawn rust-analyzer (or other configured
            // servers) on first request. When no servers are
            // configured for the workspace, the field stays `None`
            // and AgentBuilder.build() drops the `lsp` tool from
            // the registry (see agent_builder.rs::build).
            lsp: if crate::lsp::manager::default_servers().is_empty() {
                None
            } else {
                Some(Arc::new(crate::lsp::CliLspProvider::with_defaults(
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                )))
            },
            ..Default::default()
        };

        // Build the agent via the SDK's AgentBuilder — no manual wiring.
        //
        // Single `set_hooks` invariant: the agent's hook chain is built
        // EXACTLY ONCE here, composing the port-backed middleware pipeline
        // (`with_port_hooks`) and the cli-owned session closures
        // (`with_session_hooks`) into one `AgentHooks` value. NEVER call
        // `agent.set_hooks(...)` elsewhere (it would wipe the
        // before/after_tool_call slots the middleware populated).
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

        // Clone the shared session state into the closures. Each closure
        // owns a fresh `Arc` clone — cheap, but essential so the agent
        // and the runtime (which owns the `SessionState`) see mutations
        // from either side.
        let stop_flag = Arc::clone(&session_state.should_stop);
        let steering = Arc::clone(&session_state.steering);
        let follow_up = Arc::clone(&session_state.follow_up);
        let session_hooks = oxicode_sdk::agent_builder::SessionHookClosures {
            should_stop_after_turn: Arc::new(move |_| {
                stop_flag.load(std::sync::atomic::Ordering::SeqCst)
            }),
            get_steering_messages: Arc::new(move || steering.write().drain(..).collect()),
            get_follow_up_messages: Arc::new(move || follow_up.write().drain(..).collect()),
            tool_execution: oxicode_agent::config::ToolExecutionMode::Sequential,
        };

        let agent = oxicode
            .agent(config)
            .workspace(cwd)
            .with_port_hooks()
            .with_session_hooks(session_hooks)
            .build()
            .map_err(|e| Error::msg(format!("agent build failed: {e}")))?;
        let agent = Arc::new(agent);

        let ask_timeout = if settings.ask_timeout_secs > 0 {
            Some(std::time::Duration::from_secs(settings.ask_timeout_secs))
        } else {
            None
        };
        let bridge = std::sync::Arc::new(oxicode_agent::tools::ask::AskBridge::with_timeout(
            ask_timeout,
        ));
        let ask_tool = oxicode_agent::tools::ask::AskTool::new(bridge.clone());
        agent.tools().register_arc(std::sync::Arc::new(ask_tool));
        // Open the local issue store rooted at the project (`.oxicode/issues/`).
        // Best-effort: if the directory cannot be resolved, issues are simply
        // unavailable — the app still works without them. The `/issue` slash
        // command surfaces a clear error in that case.
        let issue_store = std::env::current_dir()
            .ok()
            .map(|cwd| crate::store::issues::FileIssueStore::open_from_cwd(&cwd))
            .and_then(|r| {
                r.map_err(|e| tracing::warn!("issue store unavailable: {e}"))
                    .ok()
            });

        // Register the `issue` agent tool when the store is available.
        if let Some(store) = issue_store.clone() {
            let tool = std::sync::Arc::new(crate::tools::IssueTool::new(store));
            agent.tools().register_arc(tool);
        }

        Ok(Self {
            oxicode,
            agent,
            settings,
            skills: RwLock::new(skills),
            active_skills: RwLock::new(Vec::new()),
            wasm_ext: None,
            ask_bridge: Some(bridge),
            issue_store,
            ownership_session_id,
            liveness_guard: None, // set below once issue_store is known
            persona_body: RwLock::new(persona.as_ref().map(|p| p.system_prompt.clone())),
            session_state,
        })
        .map(|mut app| {
            // Acquire the process-wide liveness flock now that issue_store exists.
            // Best-effort: another live process already holds the lock is non-fatal;
            // we still expose ownership_session_id so callers can detect the conflict.
            app.liveness_guard =
                acquire_ownership_guard(app.issue_store.as_ref(), &app.ownership_session_id);
            app
        })
    }

    /// Per-process liveness identity. Used by the agent's `issue` tool and any
    /// other surface that gates on `is_session_alive`.
    pub fn ownership_session_id(&self) -> &str {
        &self.ownership_session_id
    }

    /// True iff `App` holds a live liveness flock under `ownership_session_id`.
    /// False when there is no `issue_store` (e.g. headless test) or when another
    /// live process already holds the lock (the assignment feature will surface
    /// `Assigned` errors in that case — by design).
    pub fn has_liveness_lock(&self) -> bool {
        self.liveness_guard.is_some()
    }

    /// Get the current settings
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Set the WASM extension manager
    pub fn set_wasm_ext(
        &mut self,
        ext: Option<std::sync::Arc<crate::extensions::WasmExtensionManager>>,
    ) {
        self.wasm_ext = ext;
    }

    /// Get the WASM extension manager
    pub fn wasm_ext(&self) -> Option<&std::sync::Arc<crate::extensions::WasmExtensionManager>> {
        self.wasm_ext.as_ref()
    }

    /// Get a clone of the local issue store, if one was opened successfully.
    pub fn issue_store(&self) -> Option<crate::store::issues::FileIssueStore> {
        self.issue_store.clone()
    }

    /// Get a reference to the underlying `Oxicode` engine. The catalog port and
    /// other ports are accessible through it.
    pub fn oxicode(&self) -> &oxicode_sdk::Oxicode {
        &self.oxicode
    }

    /// Get a clone of the model catalog port (the canonical provider/model
    /// metadata source). Used by the TUI to browse the full catalog in-TUI.
    pub fn catalog(&self) -> std::sync::Arc<dyn oxicode_sdk::ports::catalog::ModelCatalog> {
        std::sync::Arc::clone(self.oxicode.catalog())
    }

    /// Get a reference to the underlying agent.
    pub fn agent(&self) -> Arc<Agent> {
        Arc::clone(&self.agent)
    }

    /// Get the tool registry (for registering extension tools)
    pub fn agent_tools(&self) -> Arc<oxicode_agent::ToolRegistry> {
        self.agent.tools()
    }

    /// Get the ask bridge, if initialized.
    pub fn ask_bridge(&self) -> Option<&std::sync::Arc<oxicode_agent::tools::ask::AskBridge>> {
        self.ask_bridge.as_ref()
    }

    /// Get a reference to the skill manager
    pub fn skills(&self) -> parking_lot::RwLockReadGuard<'_, SkillManager> {
        self.skills.read()
    }

    /// Activate a skill by name. Returns an error string if not found.
    pub fn activate_skill(&self, name: &str) -> Result<(), String> {
        {
            let skills = self.skills.read();
            if skills.get(name).is_none() {
                return Err(format!("Skill '{}' not found", name));
            }
        }
        let name_lower = name.to_lowercase();
        {
            let mut active = self.active_skills.write();
            if !active.contains(&name_lower) {
                active.push(name_lower);
            }
        }
        self.rebuild_system_prompt();
        Ok(())
    }

    /// Deactivate a skill by name.
    pub fn deactivate_skill(&self, name: &str) {
        let name_lower = name.to_lowercase();
        {
            let mut active = self.active_skills.write();
            active.retain(|n| n != &name_lower);
        }
        self.rebuild_system_prompt();
    }

    /// List currently active skill names
    pub fn active_skills(&self) -> Vec<String> {
        self.active_skills.read().clone()
    }

    /// Rebuild the system prompt with current active skills
    fn rebuild_system_prompt(&self) {
        let active = self.active_skills.read();
        let skills = self.skills.read();
        let contents: Vec<String> = active
            .iter()
            .filter_map(|name| skills.get(name).map(|s| s.content.clone()))
            .collect();
        // The persona body was resolved once in `from_oxicode` and cached
        // on `self.persona_body` so this sync rebuild can include it
        // without re-awaiting the async PersonaProvider port.
        let persona = self.persona_body.read().clone();
        let prompt =
            build_system_prompt(self.settings.thinking_level, &contents, persona.as_deref());
        self.agent.set_system_prompt(prompt);
    }

    /// Get a clone of the current state
    pub fn agent_state(&self) -> oxicode_agent::AgentState {
        self.agent.state()
    }

    /// Run a single prompt and return the response
    pub async fn run_prompt(&self, prompt: String) -> Result<String> {
        let (response, _events) = self.agent.run(prompt).await?;
        Ok(response.content)
    }

    /// Run a prompt with event callback
    pub async fn run_prompt_with_events<F>(&self, prompt: String, on_event: F) -> Result<String>
    where
        F: FnMut(AgentEvent) + Send + 'static,
    {
        self.agent.run_streaming(prompt, on_event).await?;
        let state = self.agent_state();
        for msg in state.messages.iter().rev() {
            if let oxicode_sdk::Message::Assistant(a) = msg {
                return Ok(a.text_content());
            }
        }
        Ok(String::new())
    }

    /// Reset the conversation
    pub fn reset(&self) {
        self.agent.reset();
    }

    /// Switch the model used for future LLM calls.
    ///
    /// The new provider is re-credentialed by the SDK resolver via the
    /// wired AuthProvider port; the `api_key` parameter was removed in
    /// 0.55.0 (issues #39/#40).
    pub async fn switch_model(&self, model_id: &str) -> anyhow::Result<()> {
        let _ = self.agent.switch_model(model_id);
        Ok(())
    }

    /// Get the current model ID
    pub fn model_id(&self) -> String {
        self.agent.model_id()
    }

    /// Borrow the pre-built [`SessionState`] (stop flag + steering + follow-up
    /// queues). The runtime clones the `Arc`s it needs into `AgentSession`
    /// so the runtime and the agent's session-level closures share the SAME
    /// state — required for Ctrl+C and `/steer` to take effect mid-run.
    pub fn session_state(&self) -> &SessionState {
        &self.session_state
    }

    /// Clone the shared stop flag. Cheap (single Arc bump).
    pub fn should_stop_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.session_state.should_stop)
    }

    /// Clone the shared steering queue.
    pub fn steering_queue(&self) -> Arc<RwLock<VecDeque<oxicode_sdk::Message>>> {
        Arc::clone(&self.session_state.steering)
    }

    /// Clone the shared follow-up queue.
    pub fn follow_up_queue(&self) -> Arc<RwLock<VecDeque<oxicode_sdk::Message>>> {
        Arc::clone(&self.session_state.follow_up)
    }
}

/// Acquire the process-wide liveness flock for `ownership_id` under the issue
/// store's `.alive/` directory.
///
/// Returns `None` (no lock) when there is no issue store or when another live
/// process already holds the lock — both non-fatal; the caller can still read
/// `ownership_session_id` and the assignment feature will surface `Assigned`
/// errors if contention actually occurs.
///
/// Extracted from `App::from_oxicode` so the single-lock invariant (defect #13 fix)
/// can be unit-tested without standing up a full `Oxicode` engine.
pub(crate) fn acquire_ownership_guard(
    issue_store: Option<&crate::store::issues::FileIssueStore>,
    ownership_id: &str,
) -> Option<crate::store::issues::liveness::AliveGuard> {
    let store = issue_store?;
    if ownership_id.is_empty() {
        // Defensive: never hold a lock under the empty string — that was the
        // #13 bug shape (empty owner is never alive, so ownership was bypassed).
        return None;
    }
    crate::store::issues::liveness::acquire(&store.issues_dir(), ownership_id).ok()
}

#[cfg(test)]
mod tests {
    //! P0 regression: `App` must hold exactly one liveness flock under its
    //! ownership identity. We test the extracted `acquire_ownership_guard`
    //! helper (the single chokepoint `from_oxicode` delegates to) rather than
    //! standing up a full `Oxicode` engine.
    use super::*;
    use crate::store::issues::FileIssueStore;
    use crate::store::issues::liveness;

    fn tmp_store() -> (tempfile::TempDir, FileIssueStore) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".oxicode").join("issues");
        std::fs::create_dir_all(&dir).unwrap();
        (tmp, FileIssueStore::open(dir).unwrap())
    }

    #[test]
    fn app_holds_single_liveness_lock() {
        // The #13 invariant: acquiring the ownership guard makes the session
        // live under that identity, and a second acquire under the SAME id
        // fails (one flock per identity — single lock).
        let (_tmp, store) = tmp_store();
        let dir = store.issues_dir();
        let id = "proc-test-app";

        let guard = acquire_ownership_guard(Some(&store), id);
        assert!(
            guard.is_some(),
            "App must acquire the liveness lock for its ownership id"
        );
        assert!(
            liveness::is_session_alive(&dir, id),
            "after acquire, the session must be live"
        );

        // While held, the same identity cannot be acquired again — single lock.
        let second = liveness::acquire(&dir, id);
        assert!(second.is_err(), "second acquire under same id must fail");

        drop(guard);
        assert!(
            !liveness::is_session_alive(&dir, id),
            "dropping App's guard releases the lock"
        );
    }

    #[test]
    fn acquire_returns_none_without_store() {
        // No issue store (headless/test) → no lock. Not an error.
        let dir = tempfile::tempdir().unwrap();
        let id = "proc-x";
        assert!(acquire_ownership_guard(None, id).is_none());
        let _ = dir; // no store created
    }

    #[test]
    fn acquire_rejects_empty_ownership_id() {
        // Defensive guard against the #13 bug shape: never hold a lock under
        // the empty string (it's never alive, so ownership would be bypassed).
        let (_tmp, store) = tmp_store();
        assert!(
            acquire_ownership_guard(Some(&store), "").is_none(),
            "empty ownership id must never acquire a lock (#13 guard)"
        );
    }
}
pub mod symbols;
pub mod tui_vt;
