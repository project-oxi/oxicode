//! Agent session runtime — service container and runtime factory.
//!
//! Originally inspired by pi-mono's agent-session-runtime and agent-session-services.
//!
//! This module provides:
//!
//! - **`AgentSessionServices`**: A coherent set of cwd-bound runtime services
//!   (auth storage, settings, model registry, resource loader).
//! - **`AgentSessionRuntime`**: Owns the current `AgentSession` plus its services,
//!   and provides session lifecycle methods (`new_session`, `switch_session`,
//!   `fork`, `import_from_jsonl`, `dispose`).
//! - **`create_agent_session_services`**: Factory that builds services for a given cwd.
//! - **`create_agent_session_from_services`**: Factory that creates an `AgentSession`
//!   from pre-built services.
//!
//! # Architecture
//!
//! ```text
//!   CLI / interactive / print / rpc
//!            │
//!            ▼
//!   AgentSessionRuntime       ← this module
//!     ├─ AgentSessionServices  (cwd-bound infra)
//!     └─ AgentSession          (session wrapper around Agent)
//!            │
//!            ▼
//!   oxicode_agent::Agent
//!            │
//!            ▼
//!   oxicode_sdk::Provider
//! ```

use crate::app::agent_session::{AgentSession, AgentSessionHandle, ScopedModel};
use crate::storage::resource_loader::ResourceLoader;
use crate::store::auth_storage::AuthStorage;
use crate::store::session::SessionManager;
use crate::store::session_cwd::{SessionCwdSource, assert_session_cwd_exists};
use crate::store::settings::{Settings, ThinkingLevel};
use anyhow::Result;
use oxicode_sdk::ModelRegistry;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ═══════════════════════════════════════════════════════════════════════════
// Diagnostics
// ═══════════════════════════════════════════════════════════════════════════
/// Map the CLI settings `TodoEagerMode` onto the agent-loop enum (the two
/// crates can't share a type, so this conversion mirrors the same pattern
/// used for `todo_reminders_*`).
fn convert_todo_eager_mode(
    mode: crate::store::settings::TodoEagerMode,
) -> oxicode_agent::agent_loop::todo_policy::TodoEagerMode {
    match mode {
        crate::store::settings::TodoEagerMode::Off => {
            oxicode_agent::agent_loop::todo_policy::TodoEagerMode::Off
        }
        crate::store::settings::TodoEagerMode::Preferred => {
            oxicode_agent::agent_loop::todo_policy::TodoEagerMode::Preferred
        }
        crate::store::settings::TodoEagerMode::Always => {
            oxicode_agent::agent_loop::todo_policy::TodoEagerMode::Always
        }
    }
}

/// Non-fatal issues collected while creating services or sessions.
///
/// Runtime creation returns diagnostics to the caller instead of printing
/// or exiting. The app layer decides whether warnings should be shown and
/// whether errors should abort startup.
#[derive(Debug, Clone)]
pub struct AgentSessionRuntimeDiagnostic {
    #[allow(dead_code)]
    pub severity: DiagnosticSeverity,
    #[allow(dead_code)]
    pub message: String,
}

/// Severity level for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DiagnosticSeverity {
    /// info variant.
    Info,
    /// warning variant.
    Warning,
    /// error variant.
    Error,
}

impl std::fmt::Display for DiagnosticSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiagnosticSeverity::Info => write!(f, "info"),
            DiagnosticSeverity::Warning => write!(f, "warning"),
            DiagnosticSeverity::Error => write!(f, "error"),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Session services
// ═══════════════════════════════════════════════════════════════════════════

/// Coherent cwd-bound runtime services for one effective session cwd.
///
/// This is infrastructure only. The `AgentSession` itself is created
/// separately so session options can be resolved against these services first.
pub struct AgentSessionServices {
    /// Current working directory (may change on session switch).
    pub cwd: PathBuf,
    /// Agent data directory (typically `~/.oxicode`).
    #[allow(dead_code)]
    pub agent_dir: PathBuf,
    /// Auth storage retained for overlay UI reads; provider credential
    /// resolution now flows through the SDK's wired AuthProvider port.
    #[allow(dead_code)]
    pub auth_storage: Arc<AuthStorage>,
    /// Settings (layered configuration).
    pub settings: Arc<Settings>,
    /// Model registry (built-in + custom models).
    #[allow(dead_code)]
    pub model_registry: Arc<ModelRegistry>,
    /// Resource loader (skills, extensions, themes, context files).
    #[allow(dead_code)]
    pub resource_loader: Arc<ResourceLoader>,
    /// Persona provider backing the TUI/session system prompt.
    ///
    /// The provider's `default` persona (if present) is resolved at
    /// session construction and threaded into the system prompt as a
    /// body fragment. `preferred_model` is applied to model resolution;
    /// `allowed_tools` is intentionally non-applied (no per-session
    /// allow-list hook on `ToolRegistry`).
    pub persona_provider: Arc<dyn oxicode_sdk::PersonaProvider>,
    /// Diagnostics collected during service creation.
    pub diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
    /// Cached hook runner (cloned from the engine) so `teardown_current`
    /// can fire `SessionEnd` without going back to the engine. None when
    /// no hooks are registered.
    #[allow(dead_code)]
    pub hook_runner: Option<Arc<dyn oxicode_sdk::ports::HookRunner>>,
}

impl AgentSessionServices {
    /// Get a clone of the cached hook runner (or `None`).
    pub fn hook_runner(&self) -> Option<Arc<dyn oxicode_sdk::ports::HookRunner>> {
        self.hook_runner.clone()
    }
}

/// Options for creating cwd-bound runtime services.
pub struct CreateAgentSessionServicesOptions {
    /// Working directory for the session.
    pub cwd: PathBuf,
    /// Override agent data directory (default: `~/.oxicode`).
    pub agent_dir: Option<PathBuf>,
    /// Override auth storage (default: auto-detected from agent_dir).
    pub auth_storage: Option<Arc<AuthStorage>>,
    /// Override settings (default: loaded from cwd + agent_dir).
    pub settings: Option<Arc<Settings>>,
    /// Override model registry (default: `agent_dir/models.json`).
    pub model_registry: Option<Arc<ModelRegistry>>,
    /// Override resource loader (default: created from cwd + agent_dir).
    pub resource_loader: Option<Arc<ResourceLoader>>,
    /// Override persona provider (default: `FilePersonaProvider` rooted
    /// at `<agent_dir>/personas`).
    pub persona_provider: Option<Arc<dyn oxicode_sdk::PersonaProvider>>,
}

impl CreateAgentSessionServicesOptions {
    /// Create options with minimal required fields.
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            agent_dir: None,
            auth_storage: None,
            settings: None,
            model_registry: None,
            resource_loader: None,
            persona_provider: None,
        }
    }
}

/// Get the default agent directory (the canonical oxicode home).
pub fn get_default_agent_dir() -> PathBuf {
    oxicode_catalog::oxi_home::oxicode_home().unwrap_or_else(|| PathBuf::from(".oxicode"))
}

/// Create cwd-bound runtime services.
///
/// Returns services plus diagnostics. It does **not** create an `AgentSession`.
pub fn create_agent_session_services(
    options: CreateAgentSessionServicesOptions,
    hook_runner: Option<Arc<dyn oxicode_sdk::ports::HookRunner>>,
) -> Result<AgentSessionServices> {
    let cwd = options.cwd;
    let agent_dir = options.agent_dir.unwrap_or_else(get_default_agent_dir);

    // Auth storage — both the service-level handle and the model registry
    // need auth access. Since AuthStorage is not Clone, we create two
    // instances (they both read from the same underlying file).
    let auth_storage = options
        .auth_storage
        .unwrap_or_else(crate::store::auth_storage::shared_auth_storage);

    // Settings — load from cwd + agent_dir
    let settings = options.settings.unwrap_or_else(|| {
        let s = Settings::load_from(&cwd).unwrap_or_default();
        Arc::new(s)
    });

    // Model registry — uses the SDK's static catalog. oxicode-cli does not
    // maintain its own model DB; the SDK has all built-in providers.
    let model_registry = options
        .model_registry
        .unwrap_or_else(|| Arc::new(oxicode_sdk::ModelRegistry::from_static()));

    // Resource loader
    let resource_loader = options
        .resource_loader
        .unwrap_or_else(|| Arc::new(ResourceLoader::with_paths(agent_dir.clone(), cwd.clone())));

    // Persona provider — file-based by default, rooted at
    // `<agent_dir>/personas`. The TUI/session system prompt
    // construction reads the `default` persona (when present) and
    // threads its body into the prompt builder.
    let persona_provider: Arc<dyn oxicode_sdk::PersonaProvider> =
        options.persona_provider.unwrap_or_else(|| {
            Arc::new(oxicode_sdk::fs::FilePersonaProvider::new(
                agent_dir.join("personas"),
            ))
        });

    Ok(AgentSessionServices {
        cwd,
        agent_dir,
        auth_storage,
        settings,
        model_registry,
        resource_loader,
        persona_provider,
        diagnostics: Vec::new(),
        hook_runner,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// Create session from services
// ═══════════════════════════════════════════════════════════════════════════

/// Options for creating an `AgentSession` from already-created services.
pub struct CreateAgentSessionFromServicesOptions {
    /// Pre-built services.
    pub services: Arc<AgentSessionServices>,
    /// Session manager (handles persistence).
    pub session_manager: SessionManager,
    /// Override model (e.g. from CLI `--model`).
    pub model_id: Option<String>,
    /// Override thinking level.
    pub thinking_level: Option<ThinkingLevel>,
    /// Scoped models for Ctrl+P cycling.
    pub scoped_models: Vec<ScopedModel>,
    /// Pre-configured tool registry to copy into the new agent.
    /// If None, builtin tools are registered automatically.
    pub tool_registry: Option<Arc<oxicode_agent::ToolRegistry>>,
    /// SDK engine that owns provider construction and live credential
    /// resolution. Interactive callers must supply this so credentials saved
    /// in the TUI are used both at session creation and on refresh.
    pub oxicode: Option<oxicode_sdk::Oxicode>,
    /// Pre-built session queues + stop flag. When `None`, fresh state is
    /// constructed (sufficient for the headless paths that don't share
    /// state with an `App`). The TUI/RPC entry points pass the shared
    /// `App::session_state()` so Ctrl+C and `/steer` survive the
    /// teardown/recreate cycle.
    pub session_state: Option<crate::SessionState>,
}

/// Result of creating an agent session.
pub struct CreateAgentSessionResult {
    /// pub.
    pub session: AgentSession,
    /// pub.
    pub model_fallback_message: Option<String>,
}

/// Create an `AgentSession` from previously created services.
///
/// This keeps session creation separate from service creation so callers
/// can resolve model, thinking, tools, and other session inputs against
/// the target cwd before constructing the session.
pub async fn create_agent_session_from_services(
    options: CreateAgentSessionFromServicesOptions,
) -> Result<CreateAgentSessionResult> {
    let services = &options.services;
    let settings = services.settings.as_ref();
    let cwd = services.cwd.to_string_lossy().to_string();

    // Resolve the default persona once. The body flows into the
    // system prompt; `preferred_model` overrides the settings default
    // when no explicit CLI/model-id override is supplied. We resolve
    // it before the model_id branch so both branches can share it.
    let persona = resolve_default_persona(services.persona_provider.as_ref()).await;

    // Resolve model — no hardcoded default, must be configured
    // unless the active persona declares one.
    let model_id = match options
        .model_id
        .or_else(|| persona.as_ref().and_then(|p| p.preferred_model.clone()))
        .or_else(|| settings.effective_model(None))
    {
        Some(id) if !id.is_empty() => {
            tracing::debug!(
                "Model resolved: {} (last_used={:?})",
                id,
                settings.last_used_model
            );
            id
        }
        other => {
            tracing::warn!(
                "No model configured: effective_model={:?}",
                settings.effective_model(None)
            );
            other.unwrap_or_default()
        }
    };

    // Resolve thinking level
    let thinking_level = options.thinking_level.unwrap_or(settings.thinking_level);

    // Optional workspace_search routing guidance (design D3/D4): rendered
    // from the shared helper so the initial prompt and every hot-apply
    // rebuild gate the fragment identically — present only when the user
    // enabled the tool AND it was actually registered at boot.
    let workspace_search_block = workspace_search_guidance(settings);

    // Get provider and model
    if model_id.is_empty() {
        // No model — return minimal session, TUI setup wizard will handle configuration
        let memory_block = if settings.memory_enabled {
            let backend = crate::services::create_memory_backend(settings);
            // Plan §5.h/§6.f: durable memory is the Brain backend as-is.
            // No extraction wrapper — that path was a local authority.
            if let Some(ref backend) = backend {
                crate::services::build_memory_recall(backend.as_ref(), &cwd).await
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        let memory_opt = if memory_block.is_empty() {
            None
        } else {
            Some(memory_block)
        };

        // TTSR engine
        let ttsr_engine: Option<Arc<oxicode_agent::agent_loop::ttsr::TtsrEngine>> =
            if settings.ttsr_enabled {
                let rules = crate::discovery::rules::discover_rules(&services.cwd);
                // Extract AST rules before moving rules into the registry.
                let ast_rules: Vec<oxicode_agent::agent_loop::ttsr::AstRule> = rules
                    .iter()
                    .filter_map(|r| {
                        r.ast_condition.as_ref().map(|pattern| {
                            oxicode_agent::agent_loop::ttsr::AstRule {
                                name: r.name.clone(),
                                pattern: pattern.clone(),
                                file_scope: r.globs.clone(),
                                interrupt_mode: r.interrupt_mode,
                            }
                        })
                    })
                    .collect();
                let registry: Arc<dyn oxicode_agent::agent_loop::ttsr::RuleRegistry> =
                    Arc::new(crate::discovery::rules::StaticRuleRegistry::new(rules));
                let engine =
                    oxicode_agent::agent_loop::ttsr::TtsrEngine::new(registry, Default::default());
                if !ast_rules.is_empty() {
                    engine.set_ast_matcher(oxicode_agent::agent_loop::ttsr::TtsrAstMatcher::new(
                        ast_rules,
                    ));
                }
                Some(Arc::new(engine))
            } else {
                None
            };

        let config = oxicode_agent::AgentConfig {
            name: "oxicode".to_string(),
            description: Some("oxicode CLI agent".to_string()),
            model_id: String::new(),
            system_prompt: Some(build_system_prompt_with_memory(
                thinking_level,
                memory_opt,
                None,
                workspace_search_block.clone(),
                persona.as_ref(),
            )),
            timeout_seconds: settings.tool_timeout_seconds,
            temperature: settings.effective_temperature(),
            max_tokens: settings.effective_max_tokens(),
            compaction_strategy: if settings.auto_compaction {
                oxicode_sdk::CompactionStrategy::Threshold(0.8)
            } else {
                oxicode_sdk::CompactionStrategy::Disabled
            },
            compaction_instruction: None,
            context_window: 128_000,
            workspace_dir: Some(services.cwd.clone()),
            output_mode: None,
            provider_options: None,
            session_id: None,
            ttsr_engine,
            memory: None,
            todo: Some(Arc::new(crate::store::todo_state::TodoState::new())),
            todo_reminders_enabled: settings.todo_reminders_enabled,
            todo_reminders_max: settings.todo_reminders_max,
            todo_eager_mode: convert_todo_eager_mode(settings.todo_eager_mode),
            agent_pool: None,
            ..Default::default()
        };
        // Use anthropic as a placeholder provider so the session can be created
        let provider = oxicode_sdk::get_provider("anthropic")
            .ok_or_else(|| anyhow::anyhow!("No provider available"))?;
        let agent = Arc::new(oxicode_agent::Agent::new(
            Arc::from(provider),
            config,
            Arc::new(oxicode_agent::ToolRegistry::new()),
        ));
        let session = AgentSession::new(
            agent,
            settings.clone(),
            options.session_manager,
            cwd,
            options.session_state.clone().unwrap_or_default(),
        );
        return Ok(CreateAgentSessionResult {
            session,
            model_fallback_message: None,
        });
    }

    let (provider_name, _model_name) = parse_model_id(&model_id);

    let provider: Arc<dyn oxicode_sdk::Provider> = if let Some(oxicode) = &options.oxicode {
        oxicode.create_provider(&provider_name)?
    } else {
        Arc::from(
            oxicode_sdk::get_provider(&provider_name)
                .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found", provider_name))?,
        )
    };
    let memory_backend: Option<Arc<dyn oxicode_agent::tools::MemoryBackend>> =
        crate::services::create_memory_backend(settings);

    let memory_block = if let Some(ref backend) = memory_backend {
        crate::services::build_memory_recall(backend.as_ref(), &cwd).await
    } else {
        String::new()
    };

    // ── TTSR engine (③) ──
    let ttsr_engine: Option<Arc<oxicode_agent::agent_loop::ttsr::TtsrEngine>> =
        if settings.ttsr_enabled {
            let rules = crate::discovery::rules::discover_rules(&services.cwd);
            // Extract AST rules before moving rules into the registry.
            let ast_rules: Vec<oxicode_agent::agent_loop::ttsr::AstRule> = rules
                .iter()
                .filter_map(|r| {
                    r.ast_condition.as_ref().map(|pattern| {
                        oxicode_agent::agent_loop::ttsr::AstRule {
                            name: r.name.clone(),
                            pattern: pattern.clone(),
                            file_scope: r.globs.clone(),
                            interrupt_mode: r.interrupt_mode,
                        }
                    })
                })
                .collect();
            let registry: Arc<dyn oxicode_agent::agent_loop::ttsr::RuleRegistry> =
                Arc::new(crate::discovery::rules::StaticRuleRegistry::new(rules));
            let engine = oxicode_agent::agent_loop::ttsr::TtsrEngine::new(
                registry,
                oxicode_agent::agent_loop::ttsr::TtsrSettings {
                    enabled: true,
                    builtin_rules: settings.ttsr_interrupt_mode != "never",
                    ..Default::default()
                },
            );
            if !ast_rules.is_empty() {
                engine.set_ast_matcher(oxicode_agent::agent_loop::ttsr::TtsrAstMatcher::new(
                    ast_rules,
                ));
            }
            Some(Arc::new(engine))
        } else {
            None
        };

    // Build agent config
    let memory_block_opt = if memory_block.is_empty() {
        None
    } else {
        Some(memory_block)
    };
    let system_prompt = build_system_prompt_with_memory(
        thinking_level,
        memory_block_opt,
        None,
        workspace_search_block,
        persona.as_ref(),
    );
    let compaction_strategy = if settings.auto_compaction {
        oxicode_sdk::CompactionStrategy::Threshold(0.8)
    } else {
        oxicode_sdk::CompactionStrategy::Disabled
    };
    // API key resolution is handled by the SDK resolver via the wired
    // AuthProvider port (sync fast-path in `Oxicode::create_provider`).

    let config = oxicode_agent::AgentConfig {
        name: "oxicode".to_string(),
        description: Some("oxicode CLI agent".to_string()),
        model_id: model_id.clone(),
        system_prompt: Some(system_prompt),
        timeout_seconds: settings.tool_timeout_seconds,
        temperature: settings.effective_temperature(),
        max_tokens: settings.effective_max_tokens(),
        compaction_strategy,
        compaction_instruction: None,
        workspace_dir: Some(services.cwd.clone()),
        output_mode: None,
        provider_options: None,
        session_id: None,
        ttsr_engine,
        memory: memory_backend,
        todo: Some(Arc::new(crate::store::todo_state::TodoState::new())),
        todo_reminders_enabled: settings.todo_reminders_enabled,
        todo_reminders_max: settings.todo_reminders_max,
        todo_eager_mode: convert_todo_eager_mode(settings.todo_eager_mode),
        agent_pool: None,
        ..Default::default()
    };

    // Always wrap with the role router so UI edits to `model_roles` apply live
    // (the wrapper passes through unchanged while the registry is empty).
    let role_registry = std::sync::Arc::new(parking_lot::RwLock::new(
        oxicode_sdk::RoleRegistry::from_map(settings.model_roles.clone()),
    ));
    oxicode_sdk::set_live_role_registry(std::sync::Arc::clone(&role_registry));
    let provider: Arc<dyn oxicode_sdk::Provider> = Arc::new(oxicode_sdk::RoleRoutingProvider::new(
        provider,
        role_registry,
    ));
    let agent = Arc::new(match options.oxicode.clone() {
        Some(oxicode) => oxicode_agent::Agent::new_with_resolver(
            provider,
            config,
            Arc::new(oxicode_agent::ToolRegistry::new()),
            Arc::new(oxicode),
        ),
        None => oxicode_agent::Agent::new(
            provider,
            config,
            Arc::new(oxicode_agent::ToolRegistry::new()),
        ),
    });

    // Register tools: use provided registry or fallback to builtins
    let registry = options.tool_registry.unwrap_or_else(|| {
        Arc::new(oxicode_agent::ToolRegistry::with_builtins_cwd(
            PathBuf::from(&cwd),
            &services.settings.disabled_tools,
        ))
    });
    let agent_tools = agent.tools();
    for name in registry.names() {
        if let Some(tool) = registry.get(&name) {
            agent_tools.register_arc(tool);
        }
    }
    // Propagate the MCP manager from the source registry to the agent's
    // registry. `register_arc` copies only `Arc<dyn AgentTool>`; the
    // manager lives in a separate field and would otherwise be `None`,
    // breaking the TUI's `/mcp dashboard|status|reauth` overlays (they
    // read `session.agent_ref().tools().mcp_manager()` and silently warn
    // "MCP runtime manager unavailable"). Mirrors bootstrap.rs:425-427.
    if let Some(mgr) = registry.mcp_manager() {
        agent_tools.set_mcp_manager(mgr);
    }

    // Create the session
    let session = AgentSession::new(
        agent,
        settings.clone(),
        options.session_manager,
        cwd,
        options.session_state.clone().unwrap_or_default(),
    );

    // Set scoped models if provided
    if !options.scoped_models.is_empty() {
        session.set_scoped_models(options.scoped_models);
    }

    // Honor `[advisor] enabled = true` from settings (best-effort: a failure
    // to resolve the advisor provider/model is logged, not fatal — the primary
    // session still starts). The user can also toggle via `/advisor`.
    if settings.advisor.enabled
        && let Err(e) = session.set_advisor_enabled(true)
    {
        tracing::warn!("advisor auto-enable failed: {e}");
    }

    Ok(CreateAgentSessionResult {
        session,
        model_fallback_message: None,
    })
}

/// Routing guidance fragment for the optional `workspace_search` tool
/// (design D3/D4) — the **single source of truth** for both the fragment
/// text and its gating.
///
/// Consumed by the initial session-construction path
/// ([`create_agent_session_from_services`]) and by both hot-apply rebuild
/// paths ([`crate::app::agent_session::AgentSession::rebuild_system_prompt`]
/// and `crate::App::rebuild_system_prompt`), so `/reload` and `/settings`
/// rebuilds can no longer drop the fragment.
///
/// The fragment is rendered only when the tool was actually registered:
/// bootstrap constructs the oxibrain backend only when
/// `[workspace_search].enabled` is set AND construction succeeds
/// (resolvable brain dir, `PathGuard` validation), so the boot-time
/// registration marker — not the settings flag alone — is what keeps the
/// prompt from naming a tool that is absent. The settings flag stays in
/// the gate as the runtime kill switch: disabling it drops the fragment on
/// the next reload even though the tool remains registered.
pub(crate) fn workspace_search_guidance(settings: &Settings) -> Option<String> {
    if !(settings.workspace_search.enabled
        && crate::foundation::brain_workspace::workspace_search_registered())
    {
        return None;
    }
    Some(
        "For a workspace-grounded question with unknown wording or location, \
         use one focused workspace_search query. Verify its source passages \
         with read or grep. Use grep for exact identifiers, paths, literals, \
         regular expressions, and exhaustive occurrence searches. Do not use \
         workspace_search to create, update, or delete an index."
            .to_string(),
    )
}

// ═══════════════════════════════════════════════════════════════════════════
// Runtime factory
// ═══════════════════════════════════════════════════════════════════════════

/// Result returned by runtime creation.
#[allow(dead_code)]
pub struct CreateAgentSessionRuntimeResult {
    pub session: AgentSession,
    pub services: Arc<AgentSessionServices>,
    pub diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
    pub model_fallback_message: Option<String>,
}

/// Factory closure type that creates a runtime for a given cwd + session manager.
#[allow(dead_code)]
pub type CreateRuntimeFactory =
    dyn Fn(CreateRuntimeOptions) -> Result<CreateAgentSessionRuntimeResult> + Send + Sync;

/// Options passed to the runtime factory.
#[allow(dead_code)]
pub struct CreateRuntimeOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub session_manager: SessionManager,
}

// ═══════════════════════════════════════════════════════════════════════════
// Session switch reason (for extension hooks / diagnostics)
// ═══════════════════════════════════════════════════════════════════════════

/// Why a session was replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SessionSwitchReason {
    /// Fresh session created via /new.
    New,
    /// Switched to an existing session via /resume or /switch.
    Resume,
    /// Forked from an existing conversation.
    Fork,
    /// Imported from an external JSONL file.
    Import,
    /// Application is shutting down.
    Quit,
}

// ═══════════════════════════════════════════════════════════════════════════
// Import error
// ═══════════════════════════════════════════════════════════════════════════

/// Error when `/import` references a JSONL file that does not exist.
#[derive(Debug)]
#[allow(dead_code)]
pub struct SessionImportFileNotFoundError {
    pub file_path: PathBuf,
}

impl std::fmt::Display for SessionImportFileNotFoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "File not found: {}", self.file_path.display())
    }
}

impl std::error::Error for SessionImportFileNotFoundError {}

// ═══════════════════════════════════════════════════════════════════════════
// AgentSessionRuntime
// ═══════════════════════════════════════════════════════════════════════════

/// Owns the current `AgentSession` plus its cwd-bound services.
///
/// Session replacement methods tear down the current runtime first, then
/// create and apply the next runtime. If creation fails, the error is
/// propagated to the caller.
#[allow(dead_code)]
pub struct AgentSessionRuntime {
    session: AgentSessionHandle,
    services: Arc<AgentSessionServices>,
    diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
    model_fallback_message: Option<String>,
    create_runtime: Arc<CreateRuntimeFactory>,
}

#[allow(dead_code)]
impl AgentSessionRuntime {
    /// Create a new runtime wrapper.
    ///
    /// The same factory is stored and reused for later `/new`, `/resume`,
    /// `/fork`, and import flows.
    pub fn new(
        session: AgentSession,
        services: Arc<AgentSessionServices>,
        create_runtime: Arc<CreateRuntimeFactory>,
        diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
        model_fallback_message: Option<String>,
    ) -> Self {
        Self {
            session: session.clone_handle(),
            services,
            diagnostics,
            model_fallback_message,
            create_runtime,
        }
    }

    // ── Accessors ────────────────────────────────────────────────────

    /// Current services.
    pub fn services(&self) -> &AgentSessionServices {
        &self.services
    }

    /// Current session handle.
    pub fn session(&self) -> &AgentSessionHandle {
        &self.session
    }

    /// Current working directory.
    pub fn cwd(&self) -> &Path {
        &self.services.cwd
    }

    /// Diagnostics from the last runtime creation.
    pub fn diagnostics(&self) -> &[AgentSessionRuntimeDiagnostic] {
        &self.diagnostics
    }

    /// Model fallback message (set when the requested model was unavailable).
    pub fn model_fallback_message(&self) -> Option<&str> {
        self.model_fallback_message.as_deref()
    }

    // ── Session lifecycle ────────────────────────────────────────────

    /// Switch to an existing session file.
    ///
    /// Validates that the session's cwd matches (or can be overridden),
    /// tears down the current session, and creates a new runtime.
    pub fn switch_session(&mut self, session_path: &str, cwd_override: Option<&str>) -> Result<()> {
        // Open the target session
        let session_manager = SessionManager::open(session_path, None, cwd_override);

        // Validate CWD
        let cwd = session_manager.get_cwd();
        let adapter = SessionManagerCwdAdapter(&session_manager);
        assert_session_cwd_exists(&adapter, &cwd).map_err(|e| anyhow::anyhow!("{}", e))?;

        self.teardown_current(SessionSwitchReason::Resume);

        let result = (self.create_runtime)(CreateRuntimeOptions {
            cwd: PathBuf::from(&cwd),
            agent_dir: self.services.agent_dir.clone(),
            session_manager,
        })?;

        self.apply(result);
        Ok(())
    }

    /// Create a new empty session.
    pub fn new_session(&mut self) -> Result<()> {
        let session_dir = get_default_session_dir();
        let session_manager =
            SessionManager::create(&self.services.cwd.to_string_lossy(), Some(&session_dir));

        self.teardown_current(SessionSwitchReason::New);

        let result = (self.create_runtime)(CreateRuntimeOptions {
            cwd: self.services.cwd.clone(),
            agent_dir: self.services.agent_dir.clone(),
            session_manager,
        })?;

        self.apply(result);
        Ok(())
    }

    /// Fork from a specific entry.
    ///
    /// Creates a new session that branches from the given entry.
    /// Uses `SessionManager::fork_from` to create the forked session file,
    /// then branches to the specified entry within it.
    pub fn fork(&mut self, entry_id: &str, _position: ForkPosition) -> Result<()> {
        let session_dir = get_default_session_dir();
        let cwd_str = self.services.cwd.to_string_lossy().to_string();

        // Create a forked session from the current session file
        let mut session_manager = {
            // For an in-memory session, just create a new one

            SessionManager::create(&cwd_str, Some(&session_dir))
        };

        // Branch to the specified entry within the new session
        if let Err(e) = session_manager.branch(entry_id) {
            tracing::warn!("Branch to entry {} failed: {}", entry_id, e);
        }

        self.teardown_current(SessionSwitchReason::Fork);

        let result = (self.create_runtime)(CreateRuntimeOptions {
            cwd: self.services.cwd.clone(),
            agent_dir: self.services.agent_dir.clone(),
            session_manager,
        })?;

        self.apply(result);
        Ok(())
    }

    /// Import a session JSONL file and switch runtime state.
    ///
    /// # Errors
    ///
    /// Returns [`SessionImportFileNotFoundError`] when the input path does not
    /// exist. Returns other errors when the imported session's CWD cannot be
    /// resolved.
    pub fn import_from_jsonl(
        &mut self,
        input_path: &Path,
        cwd_override: Option<&str>,
    ) -> Result<()> {
        let resolved = if input_path.is_absolute() {
            input_path.to_path_buf()
        } else {
            std::env::current_dir()?.join(input_path)
        };

        if !resolved.exists() {
            return Err(SessionImportFileNotFoundError {
                file_path: resolved,
            }
            .into());
        }

        // Copy to session dir if needed
        let session_dir = get_default_session_dir();
        let dest_dir = Path::new(&session_dir);
        if !dest_dir.exists() {
            std::fs::create_dir_all(dest_dir)?;
        }

        let file_name = resolved.file_name().unwrap_or_default();
        let destination = dest_dir.join(file_name);

        if destination != resolved {
            std::fs::copy(&resolved, &destination)?;
        }

        let session_manager = SessionManager::open(
            &destination.to_string_lossy(),
            Some(&session_dir),
            cwd_override,
        );

        let cwd = session_manager.get_cwd();
        let adapter = SessionManagerCwdAdapter(&session_manager);
        assert_session_cwd_exists(&adapter, &cwd).map_err(|e| anyhow::anyhow!("{}", e))?;

        self.teardown_current(SessionSwitchReason::Import);

        let result = (self.create_runtime)(CreateRuntimeOptions {
            cwd: PathBuf::from(&cwd),
            agent_dir: self.services.agent_dir.clone(),
            session_manager,
        })?;

        self.apply(result);
        Ok(())
    }

    /// Teardown the current session.
    fn teardown_current(&mut self, _reason: SessionSwitchReason) {
        // Capture session data before reset for memory reflection
        let session_id = self.session.session_id();
        let messages = self.session.messages();
        let memory = self.session.agent_ref().get_config().memory.clone();

        self.session.reset();

        // Fire SessionEnd hook (best-effort, fire-and-forget).
        if let Some(runner) = self.services.hook_runner() {
            let hook_ctx = oxicode_sdk::ports::HookContext {
                event: oxicode_sdk::ports::HookEvent::SessionEnd,
                session_id: Some(session_id.clone()),
                ..Default::default()
            };
            tokio::spawn(async move {
                let _ = runner
                    .run(oxicode_sdk::ports::HookEvent::SessionEnd, &hook_ctx)
                    .await;
            });
        }

        // Fire-and-forget: store a session summary into the memory backend.
        // Non-blocking — teardown does not wait for the write to complete.
        if let Some(backend) = memory
            && messages.len() >= 3
        {
            let sid = session_id.clone();
            let msg_count = messages.len();
            tokio::spawn(async move {
                let summary = format!("Session {}: {} messages", sid, msg_count);
                crate::services::session_reflect(&*backend, &sid, &summary).await;
            });
        }
    }

    /// Apply a new runtime result.
    fn apply(&mut self, result: CreateAgentSessionRuntimeResult) {
        self.session = result.session.clone_handle();
        self.services = result.services;
        self.diagnostics = result.diagnostics;
        self.model_fallback_message = result.model_fallback_message;
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Fork position
// ═══════════════════════════════════════════════════════════════════════════

/// Where to fork relative to a session entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ForkPosition {
    /// Fork at the specified entry (includes it).
    At,
    /// Fork before the specified entry (excludes it).
    Before,
}

// ═══════════════════════════════════════════════════════════════════════════
// Session CWD adapter for SessionManager
// ═══════════════════════════════════════════════════════════════════════════

/// Adapter to use `SessionManager` with `assert_session_cwd_exists`.
pub(crate) struct SessionManagerCwdAdapter<'a>(pub(crate) &'a SessionManager);

impl SessionCwdSource for SessionManagerCwdAdapter<'_> {
    fn get_cwd(&self) -> Option<String> {
        let cwd = self.0.get_cwd();
        if cwd.is_empty() { None } else { Some(cwd) }
    }

    fn get_session_file(&self) -> Option<String> {
        self.0.get_session_file()
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Top-level factory
// ═══════════════════════════════════════════════════════════════════════════

/// Create the initial runtime from a runtime factory and initial session target.
///
/// The same factory is stored on the returned `AgentSessionRuntime` and reused
/// for later `/new`, `/resume`, `/fork`, and import flows.
#[allow(dead_code)]
pub fn create_agent_session_runtime(
    create_runtime: Arc<CreateRuntimeFactory>,
    options: CreateRuntimeOptions,
) -> Result<AgentSessionRuntime> {
    // Validate CWD
    let adapter = SessionManagerCwdAdapter(&options.session_manager);
    let cwd = options.session_manager.get_cwd();
    assert_session_cwd_exists(&adapter, &cwd).map_err(|e| anyhow::anyhow!("{}", e))?;

    let result = create_runtime(options)?;

    Ok(AgentSessionRuntime::new(
        result.session,
        result.services,
        create_runtime,
        result.diagnostics,
        result.model_fallback_message,
    ))
}

// ═══════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Parse a model ID string like `"anthropic/claude-sonnet-4-20250514"` into
/// `(provider, model)` parts.
fn parse_model_id(model_id: &str) -> (String, String) {
    let parts: Vec<&str> = model_id.splitn(2, '/').collect();
    if parts.len() >= 2 {
        (parts[0].to_string(), parts[1].to_string())
    } else {
        ("anthropic".to_string(), model_id.to_string())
    }
}

/// Resolve the `default` persona from a [`oxicode_sdk::PersonaProvider`].
///
/// Returns `None` when the provider has no `default.md`, when the file
/// exists but the body is blank, or when the provider errors out — in
/// every case the session prompt falls back to the no-persona default.
/// Errors are logged at `warn`; persona resolution is non-fatal.
///
/// bridge the registered `PersonaProvider` port to the
/// [`build_system_prompt_with_memory`] builder. Callers that already
/// hold a `Persona` (e.g. tests, or non-default selection paths) can
/// pass it directly.
pub(crate) async fn resolve_default_persona(
    provider: &dyn oxicode_sdk::PersonaProvider,
) -> Option<oxicode_sdk::Persona> {
    match provider.get("default").await {
        Ok(Some(p)) if !p.system_prompt.trim().is_empty() => Some(p),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, "default persona lookup failed");
            None
        }
    }
}

/// Combine memory block with tool usage guidance for system prompt.
fn append_memory_and_tool_guidance(memory_block: Option<String>) -> Option<String> {
    let tool_guidance = "\n\n## Task Management\n\
        When working on multi-step tasks (3+ steps), use the `todo` tool to plan, start, and complete \
        phases before writing code. Tasks are 5-10 words describing WHAT not HOW.\n\
        \n## Project Memory\n\
        Use `memory_retain` to save project facts/preferences across sessions. \
        Use `memory_recall` to search past knowledge. Use `memory_reflect` at session end.\n\
        \n## Commit Messages\n\
        Use `commit` with `--dry-run` first to preview conventional commit messages. \
        Handles scope detection, validation, and changelog updates.\n\
        \n## Diagrams\n\
        Use ` ```mermaid ` code blocks to explain architecture and flow. \
        They render as ASCII diagrams in the terminal.";

    match memory_block {
        Some(mem) => Some(format!("{}{}", mem, tool_guidance)),
        None => Some(tool_guidance.to_string()),
    }
}

/// Build the system prompt with optional project-memory blocks: a
/// raw project-recall block (`memory_block`) and an autonomous
/// read-path block (`read_path_block`) generated from
/// `<memory-root>/memory_summary.md` (omp `read-path.md` port), and the
/// optional `workspace_search_block` routing fragment (present only when
/// the user enabled `[workspace_search]`). All are appended after the
/// standard system prompt body.
///
/// `persona`, when `Some`, contributes its `system_prompt` body to
/// the rendered prompt via `BuildSystemPromptOptions::persona_prompt`.
/// `preferred_model` is **not** consumed here — it's applied at the
/// composition root (`create_agent_session_from_services`) before
/// the agent is built. `allowed_tools` is intentionally non-applied
/// (no per-session allow-list hook on `ToolRegistry`).
pub(crate) fn build_system_prompt_with_memory(
    thinking_level: ThinkingLevel,
    memory_block: Option<String>,
    read_path_block: Option<String>,
    workspace_search_block: Option<String>,
    persona: Option<&oxicode_sdk::Persona>,
) -> String {
    // Concatenate the optional blocks in order (raw recall first, then
    // the autonomous read-path guidance, then the workspace_search
    // routing fragment).
    let combined = match (memory_block, read_path_block) {
        (Some(m), Some(r)) => Some(format!("{}{}", m, r)),
        (Some(m), None) => Some(m),
        (None, Some(r)) => Some(r),
        (None, None) => None,
    };
    let combined = match (combined, workspace_search_block) {
        (Some(existing), Some(w)) => Some(format!("{existing}\n\n{w}")),
        (None, Some(w)) => Some(w),
        (existing, None) => existing,
    };
    let options = crate::prompt::system_prompt::BuildSystemPromptOptions {
        custom_prompt: crate::prompt::system_prompt::thinking_level_prompt(thinking_level),
        cwd: std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
        selected_tools: crate::prompt::system_prompt::default_tool_names(),
        tool_snippets: crate::prompt::system_prompt::default_tool_snippets(),
        append_system_prompt: append_memory_and_tool_guidance(combined),
        persona_prompt: persona.map(|p| p.system_prompt.clone()),
        ..Default::default()
    };

    crate::prompt::system_prompt::build_system_prompt(&options)
}

/// Get the default sessions directory.
fn get_default_session_dir() -> String {
    format!("{}/sessions", get_default_agent_dir().to_string_lossy())
}

// ═══════════════════════════════════════════════════════════════════════════
// Default runtime factory
// ═══════════════════════════════════════════════════════════════════════════

/// Build a default runtime factory that creates services and sessions
/// using standard defaults.
///
/// This is the main entry point for callers that don't need custom
/// service injection.
#[allow(dead_code)]
pub fn default_create_runtime_factory() -> Arc<CreateRuntimeFactory> {
    Arc::new(|options: CreateRuntimeOptions| {
        // No hooks available in the default factory path — None disables
        // SessionEnd firing here. (Production callers go through the App
        // which wires the runner from the engine.)
        let services = create_agent_session_services(
            CreateAgentSessionServicesOptions {
                cwd: options.cwd.clone(),
                agent_dir: Some(options.agent_dir.clone()),
                auth_storage: None,
                settings: None,
                model_registry: None,
                resource_loader: None,
                persona_provider: None,
            },
            None,
        )?;
        let services = Arc::new(services);

        let handle = tokio::runtime::Handle::current();
        let result = handle.block_on(create_agent_session_from_services(
            CreateAgentSessionFromServicesOptions {
                services: services.clone(),
                session_manager: options.session_manager,
                model_id: None,
                thinking_level: None,
                scoped_models: Vec::new(),
                tool_registry: None,
                oxicode: None,
                // The default factory runs without an App, so fresh
                // state is constructed — fine for tests / RPC paths
                // that don't need to share queues with another surface.
                session_state: None,
            },
        ))?;

        Ok(CreateAgentSessionRuntimeResult {
            session: result.session,
            services,
            diagnostics: Vec::new(),
            model_fallback_message: result.model_fallback_message,
        })
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_todo_eager_mode_converts_to_loop_enum() {
        assert_eq!(
            convert_todo_eager_mode(crate::store::settings::TodoEagerMode::Always),
            oxicode_agent::agent_loop::todo_policy::TodoEagerMode::Always
        );
        assert_eq!(
            convert_todo_eager_mode(crate::store::settings::TodoEagerMode::Preferred),
            oxicode_agent::agent_loop::todo_policy::TodoEagerMode::Preferred
        );
        assert_eq!(
            convert_todo_eager_mode(crate::store::settings::TodoEagerMode::Off),
            oxicode_agent::agent_loop::todo_policy::TodoEagerMode::Off
        );
    }

    #[test]
    fn test_parse_model_id_with_provider() {
        let (provider, model) = parse_model_id("anthropic/claude-sonnet-4-20250514");
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn test_parse_model_id_without_provider() {
        let (provider, model) = parse_model_id("claude-sonnet-4-20250514");
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn test_parse_model_id_nested() {
        let (provider, model) = parse_model_id("openai/gpt-4o");
        assert_eq!(provider, "openai");
        assert_eq!(model, "gpt-4o");
    }

    #[test]
    fn test_build_system_prompt() {
        let prompt = build_system_prompt_with_memory(ThinkingLevel::Off, None, None, None, None);
        assert!(prompt.contains("concise"));

        let prompt = build_system_prompt_with_memory(ThinkingLevel::Medium, None, None, None, None);
        assert!(prompt.contains("coding"));

        let prompt = build_system_prompt_with_memory(ThinkingLevel::High, None, None, None, None);
        assert!(prompt.contains("comprehensive"));
    }

    #[test]
    fn test_diagnostic_severity_display() {
        assert_eq!(format!("{}", DiagnosticSeverity::Info), "info");
        assert_eq!(format!("{}", DiagnosticSeverity::Warning), "warning");
        assert_eq!(format!("{}", DiagnosticSeverity::Error), "error");
    }

    #[test]
    fn test_session_import_file_not_found_error() {
        let err = SessionImportFileNotFoundError {
            file_path: PathBuf::from("/tmp/nonexistent.jsonl"),
        };
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_get_default_agent_dir() {
        // Canonical-first: the default agent dir is the resolved oxicode
        // home, whatever override is active (env-independent assertion).
        assert_eq!(
            get_default_agent_dir(),
            oxicode_catalog::oxi_home::oxicode_home().unwrap()
        );
    }

    #[test]
    fn test_fork_position() {
        assert_eq!(ForkPosition::At, ForkPosition::At);
        assert_ne!(ForkPosition::At, ForkPosition::Before);
    }

    #[test]
    fn test_session_switch_reason() {
        assert_eq!(SessionSwitchReason::New, SessionSwitchReason::New);
        assert_ne!(SessionSwitchReason::New, SessionSwitchReason::Resume);
        assert_ne!(SessionSwitchReason::Fork, SessionSwitchReason::Import);
        assert_ne!(SessionSwitchReason::Import, SessionSwitchReason::Quit);
    }

    #[test]
    fn test_create_agent_session_services_options() {
        let opts = CreateAgentSessionServicesOptions::new(PathBuf::from("/tmp"));
        assert_eq!(opts.cwd, PathBuf::from("/tmp"));
        assert!(opts.agent_dir.is_none());
        assert!(opts.auth_storage.is_none());
    }
    /// Regression: `register_arc` copies only `Arc<dyn AgentTool>` and NOT
    /// the `mcp_manager`. The TUI session builder in
    /// `create_agent_session_from_services` must therefore call
    /// `set_mcp_manager` after its copy loop (mirroring bootstrap.rs), or
    /// `session.agent_ref().tools().mcp_manager()` returns `None` and
    /// `/mcp dashboard|status|reauth` warn "MCP runtime manager
    /// unavailable" even though the `McpTool` is registered.
    #[test]
    fn register_arc_copies_tools_not_mcp_manager() {
        let src =
            oxicode_agent::ToolRegistry::with_builtins_cwd(std::path::PathBuf::from("/tmp"), &[]);
        assert!(
            src.mcp_manager().is_some(),
            "with_builtins_cwd attaches the MCP manager"
        );

        // Mirror the session builder's copy loop exactly.
        let dst = oxicode_agent::ToolRegistry::new();
        for name in src.names() {
            if let Some(tool) = src.get(&name) {
                dst.register_arc(tool);
            }
        }
        assert!(
            dst.mcp_manager().is_none(),
            "register_arc copies tools only, never the manager — the trap"
        );

        // The required explicit propagation.
        if let Some(mgr) = src.mcp_manager() {
            dst.set_mcp_manager(mgr);
        }
        assert!(
            dst.mcp_manager().is_some(),
            "set_mcp_manager propagates the live manager"
        );
    }

    /// Persona is loaded from a temp `FilePersonaProvider` and the
    /// resolved body reaches `build_system_prompt_with_memory` —
    /// the deterministic end-to-end proof required by the persona
    /// integration acceptance.
    #[tokio::test]
    async fn default_persona_body_reaches_session_prompt() {
        use oxicode_sdk::fs::FilePersonaProvider;
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("default.md"),
            "You are a reviewer focused on security.",
        )
        .unwrap();

        let provider: Arc<dyn oxicode_sdk::PersonaProvider> =
            Arc::new(FilePersonaProvider::new(tmp.path()));
        let resolved = resolve_default_persona(provider.as_ref())
            .await
            .expect("default.md should resolve");
        assert!(resolved.system_prompt.contains("security"));
        assert!(resolved.preferred_model.is_none());

        let prompt = build_system_prompt_with_memory(
            ThinkingLevel::Medium,
            None,
            None,
            None,
            Some(&resolved),
        );
        assert!(
            prompt.contains("# Persona"),
            "persona block must be rendered when persona is provided"
        );
        assert!(
            prompt.contains("You are a reviewer focused on security."),
            "persona body must reach the session prompt end-to-end"
        );
        // Persona metadata is NOT echoed as model-visible prose.
        assert!(!prompt.contains("Preferred model:"));
        assert!(!prompt.contains("Allowed tools:"));

        // Negative case: no persona → no persona block.
        let no_persona =
            build_system_prompt_with_memory(ThinkingLevel::Medium, None, None, None, None);
        assert!(!no_persona.contains("# Persona"));
    }
}
