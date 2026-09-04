//! Application bootstrap and run-mode dispatch.
//!
//! Owns: log init, app building (settings → custom providers → router →
//! tools → WASM), and run-mode dispatch (TUI / print / RPC).
//!
//! The helper functions below are moved verbatim from main.rs and
//! retain their original signatures.

use crate::cli::CliArgs;
use crate::print_mode;
use crate::store::settings::Settings;
use anyhow::Result;
use tracing;

/// Build a wired `App` from CLI args. All the wiring that used to be
/// inline in `main()` lives here.
pub async fn build_app(args: &CliArgs) -> Result<crate::App> {
    // Layer 2.5 / Catalog Port (v3): the `FileModelCatalog` wired in
    // `services::build_oxicode` performs its own init at `OxicodeBuilder::build`
    // time — it loads the embedded SNAP, applies overrides, and attempts
    // one refresh if the cache is stale. So we no longer call the legacy
    // `init_models_dev()` here. To skip network access during boot, set
    // `OXICODE_MODELS_DEV_DISABLE_FETCH=1`.

    // Load settings (global + project + env layers).
    let mut settings = Settings::load().unwrap_or_default();

    // Apply CLI overrides. Centralized in a closure so the post-wizard reload
    // re-applies the exact same overrides — adding a new flag can't silently
    // diverge between the two call sites.
    let apply_cli_overrides = |s: &mut Settings| {
        s.merge_cli(args.model.clone(), args.provider.clone());
    };
    apply_cli_overrides(&mut settings);

    // Pre-build the per-process liveness identity BEFORE the engine build so
    // we can include it in the SessionStart hook context (and pass it to
    // App::from_oxicode on the same path).
    let ownership_session_id = if is_tui_mode(args) {
        tui_ownership_id()
    } else {
        proc_ownership_id()
    };

    // Load hooks: global hooks (`~/.oxicode/settings.toml` -> `[[hooks]]`)
    // are always trusted. Project hooks (`.oxicode/settings.toml`) require
    // a first-run interactive Y/n approval — unless we're in a non-TUI
    // mode, in which case we skip with a warning to keep the boot path
    // non-interactive. See `store/hook_approval.rs` for the registry.
    let global_hooks = settings.hooks.clone();
    let cwd_now = std::env::current_dir().unwrap_or_default();
    let project_hooks_path = Settings::find_project_settings(&cwd_now);
    let project_hooks: Vec<oxicode_sdk::ports::HookSpec> = match &project_hooks_path {
        Some(path) => {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            let hash = crate::store::hook_approval::hash_settings(&content);
            let mut registry = crate::store::hook_approval::HookApprovalRegistry::load_or_default();
            if registry.is_approved(&cwd_now, &hash) {
                // Approved: re-parse the project file and extract its [[hooks]].
                match Settings::parse_from_str(&content, Settings::detect_format(path)) {
                    Ok(s) => s.hooks,
                    Err(e) => {
                        tracing::warn!(error = %e, "project hooks file failed to parse");
                        Vec::new()
                    }
                }
            } else {
                // First run or hash mismatch.
                let count = content.matches("[[hooks]]").count();
                if count > 0 {
                    if is_tui_mode(args) {
                        let ok = crate::store::hook_approval::prompt_for_approval(&cwd_now, count);
                        if ok {
                            registry.approve(&cwd_now, &hash);
                            let _ = registry.persist();
                            Settings::parse_from_str(&content, Settings::detect_format(path))
                                .map(|s| s.hooks)
                                .unwrap_or_default()
                        } else {
                            tracing::warn!("project hooks denied by user; skipping");
                            Vec::new()
                        }
                    } else {
                        tracing::warn!(
                            count,
                            "project hooks not approved; skipping (non-interactive mode)"
                        );
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            }
        }
        None => Vec::new(),
    };
    let mut all_hooks = global_hooks;
    all_hooks.extend(project_hooks);
    let hook_runner: std::sync::Arc<dyn oxicode_sdk::ports::HookRunner> =
        match oxicode_sdk::ports::fs::CommandHookRunner::new(all_hooks) {
            Ok(r) => std::sync::Arc::new(r),
            Err(e) => {
                tracing::warn!(error = %e, "hook runner construction failed; using empty runner");
                std::sync::Arc::new(
                    oxicode_sdk::ports::fs::CommandHookRunner::new(Vec::new())
                        .expect("empty spec list is always valid"),
                )
            }
        };

    if settings
        .effective_model(None)
        .unwrap_or_default()
        .is_empty()
    {
        // No model configured. In interactive (TUI) mode, drop the user
        // straight into the setup wizard instead of erroring out — this is
        // the common first-run experience. In non-interactive modes
        // (print / JSON / RPC / single-prompt) the caller explicitly wants a
        // one-shot run, so a hard error with guidance is correct.
        if is_tui_mode(args) {
            eprintln!("No model configured. Launching setup wizard...");
            crate::setup_wizard::run().await?;

            // Reload settings the wizard just persisted and re-apply the
            // CLI overrides, then re-check. If the user bailed out of the
            // wizard without selecting a model, fall through to the error.
            settings = Settings::load().unwrap_or_default();
            apply_cli_overrides(&mut settings);
        }

        if settings
            .effective_model(None)
            .unwrap_or_default()
            .is_empty()
        {
            eprintln!(
                "{}",
                print_mode::format_error("No model configured. Run `oxicode setup` to configure.")
            );
            std::process::exit(1);
        }
    }

    // Register custom OpenAI-compatible providers from settings.
    register_custom_providers(&settings);

    // Register model router (reads router_config file, opt-in).
    register_router_provider();

    // Apply thinking level if specified.
    if let Some(ref level_str) = args.thinking {
        if let Some(level) = crate::store::settings::parse_thinking_level(level_str) {
            settings.thinking_level = level;
        } else {
            anyhow::bail!(
                "Invalid thinking level: {}. Valid options: off, minimal, low, medium, high, xhigh",
                level_str
            );
        }
    }

    // Durable memory: oxibrain daemon is the sole authority (Foundation
    // plan §5). The backend is constructed per AgentSession via
    // services::create_memory_backend; no local pipeline is spawned.
    // Build the wired Oxicode engine + Agent via the SDK composition root.
    // (The SDK embeddings port stays at its Noop default — durable memory is
    // the oxibrain daemon, not an embedding pipeline.)
    let oxicode = crate::build_oxicode_engine(None, Some(hook_runner.clone())).await?;

    // Fire SessionStart (fail-open: a hook that errors must not block boot).
    {
        let hook_ctx = oxicode_sdk::ports::HookContext {
            event: oxicode_sdk::ports::HookEvent::SessionStart,
            session_id: Some(ownership_session_id.clone()),
            session_cwd: Some(cwd_now.clone()),
            ..Default::default()
        };
        let _ = oxicode
            .ports()
            .hooks
            .run(oxicode_sdk::ports::HookEvent::SessionStart, &hook_ctx)
            .await;
    }

    // Spawn the catalog event logger so refresh / override / local-discovery
    // events show up in the log file. UI hooks can subscribe to
    // `oxicode.catalog().subscribe()` separately for picker invalidation.
    let _catalog_logger =
        crate::services::spawn_catalog_event_logger(std::sync::Arc::clone(oxicode.catalog()));

    // Pre-build session state so the runtime (AgentSession) and the
    // agent's session-level closures (`with_session_hooks`) share the
    // SAME queues + stop flag. The single `set_hooks` invariant depends
    // on this state living across both ends.
    let session_state = crate::SessionState::default();

    // Register built-in tools + the coding-omp-v1 behavior pack on the
    // engine's SHARED tool registry BEFORE App/Agent construction so the
    // pack's AgentConfig patch (snapshot store, prompt layers) applies
    // when the AgentConfig is built.
    let tools = oxicode.tools();
    let cwd = cwd_now.clone();
    let behavior = register_builtin_tools(
        &tools,
        &cwd,
        args,
        &settings.disabled_tools,
        &settings.model_roles,
    );
    if let Some(b) = &behavior {
        tracing::info!(
            packs = %b
                .manifest
                .packs
                .iter()
                .map(|p| p.0.as_str())
                .collect::<Vec<_>>()
                .join("+"),
            tools = b.manifest.tools.len(),
            degraded = b.manifest.degraded.len(),
            level = ?b.manifest.compatibility_level(),
            "behavior pack installed"
        );
    }

    // Optional oxibrain-backed workspace discovery (design D3/D4): registered
    // only when the user enabled it; zero cost otherwise. The registration
    // marker feeds `workspace_search_guidance`, so every prompt path (initial
    // + hot-apply rebuilds) gates the routing fragment on the same condition
    // that registered the tool — never on the settings flag alone.
    if let Some(backend) =
        crate::foundation::brain_workspace::create_workspace_search_backend(&settings, &cwd)
    {
        tools.register_arc(std::sync::Arc::new(
            oxicode_agent::tools::WorkspaceSearchTool::new(backend),
        ));
        crate::foundation::brain_workspace::mark_workspace_search_registered();
        tracing::info!("workspace_search registered (oxibrain document plane)");
    }

    let mut app = crate::App::from_oxicode(
        oxicode,
        settings,
        ownership_session_id,
        Some(session_state),
        behavior,
    )
    .await?;

    // Fire-and-forget OAuth refresh: if the active provider has a stored
    // OAuth credential that is expired (or within 60 s of expiry), ask the
    // refresh module to rotate it in the background. A failed refresh must
    // never crash startup — the agent will surface a re-login prompt on
    // the first 401 if the refresh actually failed.
    if let Some(active_provider) = app.settings().effective_provider(args.provider.as_deref())
        && !active_provider.is_empty()
    {
        let p = active_provider.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::oauth_refresh::refresh_if_expired(&p).await {
                tracing::debug!(provider = %p, error = %e, "oauth refresh skipped");
            }
        });
    }

    // v2.2: wire the MCP credential provider (OAuth2 client_credentials).
    // Reads the same `mcp.json` files the agent uses, picks every server
    // with an `oauth` block, and gives the manager a provider that can
    // obtain + refresh access tokens on demand. No-op when no server
    // declares `oauth`.
    let mcp_cfg = oxicode_agent::mcp::config::load_mcp_config();
    let mut oauth_map: std::collections::HashMap<String, oxicode_agent::mcp::types::OAuthConfig> =
        std::collections::HashMap::new();
    for (name, entry) in &mcp_cfg.mcp_servers {
        if let Some(oc) = entry.oauth.clone() {
            oauth_map.insert(name.clone(), oc);
        }
    }
    if !oauth_map.is_empty()
        && let Some(manager) = app.agent_tools().mcp_manager()
    {
        let config_dir = dirs::config_dir()
            .map(|d| d.join("oxicode"))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        match crate::mcp_credentials::FileMcpCredentialProvider::new(oauth_map, config_dir) {
            Ok(provider) => {
                manager.set_credential_provider(provider);
            }
            Err(e) => {
                tracing::warn!("Failed to construct MCP credential provider: {}", e);
            }
        }
    }

    // Native headless browser (opt-in via the `native-browser` cargo feature).
    // Constructs the pure-Rust `oxibrowser-core` engine and registers the
    // browse tools (incl. `browse_session` with `observe`/`wait` actions) so
    // the agent can navigate/observe/extract — omp-parity browsing without a
    // Chrome dependency.
    #[cfg(feature = "native-browser")]
    {
        match oxicode_agent::tools::browse::OxicodeBrowserEngine::new().await {
            Ok(engine) => {
                let (provider, model) = match (app.agent().model_id().is_empty(), app.oxicode()) {
                    (false, oxicode) => {
                        let model_id = app.agent().model_id();
                        // model_id is the bare model id (Agent::model_id);
                        // resolve provider name from the agent's config or fall
                        // back to the Oxicode default.
                        let provider_name = "anthropic".to_string();
                        match (
                            oxicode.create_provider(&provider_name),
                            oxicode_ai::Model::new(
                                model_id.clone(),
                                model_id.clone(),
                                oxicode_ai::Api::AnthropicMessages,
                                provider_name.clone(),
                                String::new(),
                            ),
                        ) {
                            (Ok(p), m) => (Some(p), Some(m)),
                            _ => (None, None),
                        }
                    }
                    _ => (None, None),
                };
                let browser_registry = oxicode_agent::tools::browse::browsing_tools_with_session(
                    provider,
                    model,
                    std::sync::Arc::new(engine),
                );
                tools.extend_from(&browser_registry);
            }
            Err(e) => {
                tracing::warn!("native browser engine unavailable; browse tools disabled: {e}");
            }
        }
    }

    // Discover and load WASM extensions.
    let wasm_ext = load_wasm_extensions(&app, &cwd, &tools);
    app.set_wasm_ext(wasm_ext);

    // Handle --append-system-prompt.
    if let Some(ref prompt_path) = args.append_system_prompt {
        let content = std::fs::read_to_string(prompt_path)
            .map_err(|e| anyhow::anyhow!("Failed to read system prompt file: {}", e))?;
        app.agent().set_system_prompt(content);
    }

    Ok(app)
}

/// Dispatch the run mode: TUI / print / RPC, based on the CLI flags.
pub async fn dispatch_run_mode(args: &CliArgs, app: crate::App) -> Result<i32> {
    let prompt = args.prompt.join(" ");

    if args.mode.as_deref() == Some("json") || args.print {
        let mode = if args.mode.as_deref() == Some("json") {
            crate::print_mode::PrintMode::Json
        } else {
            crate::print_mode::PrintMode::Text
        };
        let options = crate::print_mode::PrintModeOptions {
            mode,
            initial_message: if prompt.is_empty() {
                None
            } else {
                Some(prompt)
            },
            messages: vec![],
            no_stdin: args.print,
            no_session: args.print || args.no_session,
            quiet: args.print,
            timeout: args.timeout,
        };
        return crate::print_mode::run_print_mode(&app, options).await;
    }

    if args.mode.as_deref() == Some("rpc") {
        crate::rpc_mode::run_rpc_mode(app).await?;
        return Ok(0);
    }

    if let Some(mode) = args.mode.as_deref() {
        anyhow::bail!("Unknown run mode: {mode}");
    }

    if prompt.is_empty() || args.interactive {
        crate::tui_vt::run_tui(app).await?;
        return Ok(0);
    }

    crate::main_dispatch::run_single_prompt(app, &prompt).await?;
    Ok(0)
}

/// Parse args, build the app, dispatch.
pub async fn run_with_args(args: CliArgs) -> Result<i32> {
    let app = build_app(&args).await?;
    dispatch_run_mode(&args, app).await
}

// ─── Helpers (moved verbatim from main.rs) ─────────────────────────────

/// Initialize file-based logging to `~/.cache/oxicode/oxicode.log`.
///
/// Reads `RUST_LOG` for filter (default: `debug`). Builds a
/// `tracing_subscriber::EnvFilter` and writes to a `Mutex<File>` writer.
pub fn init_logging() {
    let log_dir = dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("oxicode");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("oxicode.log");

    let log_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "debug".to_string());
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&log_filter));

    // Logging is non-critical infrastructure: if the log file can't be
    // created (permissions, read-only fs, …), degrade to stderr instead of
    // aborting the process. Previously this `.expect()`-panicked on init,
    // which under `panic = "abort"` killed the app before it could start.
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .with_thread_ids(true)
        .with_ansi(false);
    match std::fs::File::create(&log_path) {
        Ok(file) => {
            subscriber.with_writer(std::sync::Mutex::new(file)).init();
        }
        Err(e) => {
            eprintln!(
                "oxicode: could not open log file {log_path:?} ({e}); falling back to stderr"
            );
            subscriber.with_writer(std::io::stderr).init();
        }
    }

    tracing::info!("Logging initialized, log file: {:?}", log_path);
}

/// Register custom OpenAI-compatible providers from settings and auto-fetch their models.
fn register_custom_providers(settings: &Settings) {
    let auth_storage = crate::store::auth_storage::shared_auth_storage();
    for cp in &settings.custom_providers {
        let api_key = auth_storage.get_api_key(&cp.name);
        let api = cp.api.to_lowercase();

        match api.as_str() {
            "openai-completions" | "openai" => {
                let provider = oxicode_ai::OpenAiProvider::with_base_url_and_key(
                    &cp.base_url,
                    api_key.clone(),
                );
                oxicode_sdk::register_provider(&cp.name, provider);
                tracing::info!(
                    "Registered custom provider '{}' (openai-completions) -> {}",
                    cp.name,
                    cp.base_url
                );
            }
            "openai-responses" | "responses" => {
                let provider = oxicode_sdk::OpenAiResponsesProvider::with_base_url_and_key(
                    &cp.base_url,
                    api_key.clone(),
                );
                oxicode_sdk::register_provider(&cp.name, provider);
                tracing::info!(
                    "Registered custom provider '{}' (openai-responses) -> {}",
                    cp.name,
                    cp.base_url
                );
            }
            _ => {
                tracing::warn!(
                    "Unknown API type '{}' for custom provider '{}'. Supported: openai-completions, openai-responses",
                    cp.api,
                    cp.name
                );
            }
        }

        fetch_and_register_models(cp, &api, &api_key);
    }
}

/// Fetch models from a custom provider's /v1/models endpoint and register them.
fn fetch_and_register_models(
    cp: &crate::store::settings::CustomProvider,
    api: &str,
    api_key: &Option<String>,
) {
    if let Some(key) = api_key {
        match oxicode_sdk::fetch_models_blocking(&cp.base_url, key.as_str()) {
            Ok(model_ids) => {
                let count = model_ids.len();
                for model_id in &model_ids {
                    let api_type = match api {
                        "openai-responses" | "responses" => oxicode_sdk::Api::OpenAiResponses,
                        _ => oxicode_sdk::Api::OpenAiCompletions,
                    };
                    // `/v1/models` reports bare ids with no metadata.
                    // Custom providers usually proxy upstream models
                    // (claude-*, gemini-*, …), so cross-fill real limits
                    // from the models.dev catalog before falling back to
                    // the conservative placeholder.
                    let known = oxicode_sdk::find_entry_by_model_id(model_id);
                    let model = oxicode_sdk::Model {
                        id: model_id.clone(),
                        name: model_id.clone(),
                        api: api_type,
                        provider: cp.name.clone(),
                        base_url: cp.base_url.clone(),
                        reasoning: known.map(|e| e.reasoning).unwrap_or(false),
                        input: vec![oxicode_sdk::InputModality::Text],
                        cost: known
                            .map(|e| oxicode_sdk::Cost {
                                input: e.cost_input.max(0.0),
                                output: e.cost_output.max(0.0),
                                cache_read: e.cost_cache_read.max(0.0),
                                cache_write: e.cost_cache_write.max(0.0),
                            })
                            .unwrap_or_default(),
                        context_window: known.map(|e| e.context_window as usize).unwrap_or(128_000),
                        max_tokens: known.map(|e| e.max_tokens as usize).unwrap_or(8_192),
                        headers: Default::default(),
                        compat: None,
                    };
                    oxicode_sdk::register_model(model);
                }
                tracing::info!(
                    "[oxicode] auto-fetched {} models from '{}' ({})",
                    count,
                    cp.name,
                    cp.base_url
                );
            }
            Err(e) => {
                tracing::warn!(
                    "[oxicode] warning: failed to resolve models for {}: {}",
                    cp.name,
                    e
                );
            }
        }
    }
}

/// Register builtin tools with the agent, respecting --tools filter and disabled_tools.
///
/// Also transfers the [`McpManager`](oxicode_agent::mcp::McpManager) reference from
/// the built-in registry to the live agent registry. This matters because
/// `register_arc` only copies the `Arc<dyn AgentTool>` — the manager field is
/// stored separately and would otherwise be `None`, making `/mcp` show a
/// "MCP is not configured" warning even though the `McpTool` is registered.
fn register_builtin_tools(
    tools: &oxicode_agent::ToolRegistry,
    cwd: &std::path::Path,
    args: &CliArgs,
    disabled_tools: &[String],
    model_roles: &std::collections::HashMap<String, String>,
) -> Option<crate::behavior::BehaviorComposition> {
    let builtin_registry = if let Some(ref tools_str) = args.tools {
        let names: Vec<&str> = tools_str.split(',').map(|s| s.trim()).collect();
        oxicode_agent::ToolRegistry::with_selected_tools(cwd.to_path_buf(), &names)
    } else {
        oxicode_agent::ToolRegistry::with_builtins_cwd(cwd.to_path_buf(), disabled_tools)
    };
    for name in builtin_registry.names() {
        if let Some(tool) = builtin_registry.get(&name) {
            tools.register_arc(tool);
        }
    }
    // Propagate the MCP manager so the TUI's `/mcp` overlay can hot-reload
    // configs, render live connection status, and so on.
    if let Some(mgr) = builtin_registry.mcp_manager() {
        tools.set_mcp_manager(mgr);
    }

    // Role-based commit model: if a `commit` role is configured, upgrade the
    // deterministic (no-LLM) CommitTool to one backed by that model. Tools
    // register by name, so this overwrites the unconfigured instance safely.
    let role_registry = oxicode_sdk::RoleRegistry::from_map(model_roles.clone());
    if let Some(model) =
        oxicode_sdk::resolve_role_to_model(oxicode_sdk::ModelRole::Commit, &role_registry)
    {
        let commit: std::sync::Arc<dyn oxicode_agent::AgentTool> =
            std::sync::Arc::new(oxicode_agent::CommitTool::new(model));
        tools.register_arc(commit);
        tracing::debug!("CommitTool upgraded to commit-role model");
    }

    // coding-omp-v1 behavior pack: canonical coding tools installed through
    // the SDK's host installer interception point, overwriting the legacy
    // instances of the same names. The returned manifest + config patch feed
    // App::from_oxicode (snapshot store, prompt layers) and /info surfacing.
    let allow: Option<Vec<String>> = args
        .tools
        .as_deref()
        .map(|s| s.split(',').map(|t| t.trim().to_string()).collect());
    crate::behavior::install_coding_omp_v1(tools, cwd, allow.as_deref(), disabled_tools)
}

/// Discover and load WASM extensions, registering their tools.
fn load_wasm_extensions(
    app: &crate::App,
    cwd: &std::path::Path,
    tools: &oxicode_agent::ToolRegistry,
) -> Option<std::sync::Arc<crate::extensions::WasmExtensionManager>> {
    if !app.settings().extensions_enabled {
        return None;
    }

    let wasm_paths = crate::extensions::WasmExtensionManager::discover(cwd);
    if wasm_paths.is_empty() {
        return None;
    }

    let mut wasm_mgr = crate::extensions::WasmExtensionManager::new();
    let (loaded, errors) = wasm_mgr.load_all(&wasm_paths);
    for info in &loaded {
        tracing::info!("WASM extension loaded: {} v{}", info.name, info.version);
    }
    for err in &errors {
        tracing::warn!("WASM extension error: {}", err);
    }

    if wasm_mgr.is_empty() {
        return None;
    }

    let mgr = std::sync::Arc::new(wasm_mgr);
    for tool_def in mgr.all_tool_defs() {
        let wasm_tool = crate::extensions::WasmTool::new(
            mgr.clone(),
            tool_def.name.clone(),
            tool_def.description.clone(),
            tool_def.schema.clone(),
        );
        tools.register(wasm_tool);
    }
    Some(mgr)
}

/// Register the model auto-router if configured in router_config.
fn register_router_provider() {
    let global_dir = dirs::config_dir().unwrap_or_default().join("oxicode");
    let project_dir = std::env::current_dir().unwrap_or_default();

    let store_cfg = match crate::store::router_config::load_router_config(&global_dir, &project_dir)
    {
        Some(cfg) => cfg,
        None => {
            tracing::debug!("No router config found — router/auto will not appear in model list");
            return;
        }
    };

    // Register router models only when configured.
    oxicode_sdk::register_model(oxicode_sdk::Model::new(
        "auto",
        "Router (auto)".to_string(),
        oxicode_sdk::Api::AnthropicMessages,
        "router",
        "router://local",
    ));

    // Convert store config to AI config.
    let mut ai_profiles = std::collections::HashMap::new();
    for (name, sp) in store_cfg.profiles() {
        fn parse_thinking(s: &Option<String>) -> Option<oxicode_sdk::ThinkingLevel> {
            s.as_ref().and_then(|s| match s.as_str() {
                "off" => Some(oxicode_sdk::ThinkingLevel::Off),
                "minimal" => Some(oxicode_sdk::ThinkingLevel::Minimal),
                "low" => Some(oxicode_sdk::ThinkingLevel::Low),
                "medium" => Some(oxicode_sdk::ThinkingLevel::Medium),
                "high" => Some(oxicode_sdk::ThinkingLevel::High),
                "xhigh" => Some(oxicode_sdk::ThinkingLevel::XHigh),
                _ => None,
            })
        }
        ai_profiles.insert(
            name.clone(),
            oxicode_sdk::router::RouterProfile {
                high: oxicode_sdk::router::RoutedTierConfig {
                    model: sp.high.model.clone(),
                    thinking: parse_thinking(&sp.high.thinking),
                    fallbacks: sp.high.fallbacks.clone(),
                },
                medium: oxicode_sdk::router::RoutedTierConfig {
                    model: sp.medium.model.clone(),
                    thinking: parse_thinking(&sp.medium.thinking),
                    fallbacks: sp.medium.fallbacks.clone(),
                },
                low: oxicode_sdk::router::RoutedTierConfig {
                    model: sp.low.model.clone(),
                    thinking: parse_thinking(&sp.low.thinking),
                    fallbacks: sp.low.fallbacks.clone(),
                },
            },
        );
    }
    let ai_cfg = oxicode_sdk::router::RouterConfig::with_pinning(
        store_cfg.default_profile().to_string(),
        store_cfg.classifier_model().map(String::from),
        store_cfg.context_upgrade_threshold(),
        store_cfg.max_session_budget(),
        ai_profiles,
        oxicode_sdk::router::ScoringWeights {
            structural: store_cfg.weights().structural,
            behavioral: store_cfg.weights().behavioral,
            context_budget: store_cfg.weights().context_budget,
            vision: store_cfg.weights().vision,
            message: store_cfg.weights().message,
        },
        store_cfg.pin_tier().and_then(|s| match s {
            "high" => Some(oxicode_sdk::router::RouterTier::High),
            "medium" => Some(oxicode_sdk::router::RouterTier::Medium),
            "low" => Some(oxicode_sdk::router::RouterTier::Low),
            _ => None,
        }),
        store_cfg.phase_bias(),
    );

    oxicode_sdk::router::register_router(&ai_cfg);
}

/// Unique liveness identity for THIS TUI process: `tui-<pid>-<uuid>`.
///
/// Historically every TUI shared the constant id `"tui"`. With parallel
/// interactive sessions that collapsed into a single identity — the second
/// TUI's flock acquisition failed silently and both sessions passed the
/// `require_owner` check (both *were* "tui"), so ownership exclusivity was
/// unenforceable between them. A unique id per process restores it: each
/// TUI holds its own flock, and a foreign assignment fails liveness the way
/// `proc-*` assignments always did.
pub(crate) fn tui_ownership_id() -> String {
    format!(
        "tui-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Unique liveness identity for a headless (print / RPC / single-prompt) run.
pub(crate) fn proc_ownership_id() -> String {
    format!(
        "proc-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Decide whether this run is the TUI (interactive) mode. Mirrors the
/// dispatch in [`dispatch_run_mode`]: print / RPC / single-prompt are
/// non-TUI. Used by [`build_app`] to pick the per-process liveness identity.
fn is_tui_mode(args: &CliArgs) -> bool {
    if matches!(args.mode.as_deref(), Some("json" | "rpc")) || args.print {
        return false;
    }
    // prompt-only (no `--interactive` and a non-empty prompt) is non-TUI too;
    // dispatch_run_mode sends it through main_dispatch::run_single_prompt.
    // NOTE: must join the prompt Vec — clap's `default_value = ""` on the
    // positional makes bare `oxicode` yield `prompt == vec![""]` (non-empty Vec,
    // empty join). Comparing the Vec directly would mis-classify the bare
    // interactive launch as a single-prompt run.
    let prompt = args.prompt.join(" ");
    if !args.interactive && !prompt.is_empty() {
        return false;
    }
    true
}
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn rpc_mode_is_headless() {
        let args = CliArgs::try_parse_from(["oxicode", "--mode", "rpc"]).unwrap();
        assert!(!is_tui_mode(&args));
    }

    #[test]
    fn empty_hooks_does_not_block() {
        use crate::store::settings::Settings;
        let s = Settings::default();
        assert!(s.hooks.is_empty());
    }

    #[test]
    fn tui_ownership_ids_are_unique_and_prefixed() {
        // Per-process TUI identity: two TUIs on the same machine must never
        // share one flock name — that collision silently broke issue
        // ownership exclusivity between parallel interactive sessions.
        let a = tui_ownership_id();
        let b = tui_ownership_id();
        assert!(a.starts_with("tui-"), "prefix missing: {a}");
        assert!(b.starts_with("tui-"), "prefix missing: {b}");
        assert_ne!(a, b, "ids must be unique per call (pid + uuid)");
        assert!(!a.contains(' '), "no whitespace: {a}");
    }
}
