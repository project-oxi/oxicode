//! Extended slash commands for the VT TUI harness.
//!
//! These live in a sibling module to [`super::registry`] to keep the catalog-
//! dependent and introspection commands isolated from the core command
//! plumbing. They are registered alongside the built-ins by
//! [`register_extra`], which [`super::registry::register_all`] calls.
//!
//! Every command here is self-contained: it owns its definition + execution
//! and receives a [`SlashCtx`] (`session`, `handle`, `state`). The model
//! catalog is reached via `ctx.state.catalog` (captured once at TUI startup);
//! credentials via the process-global `shared_auth_storage()`.

use oxicode_vtui::tui::core::{
    InlineListItem, InlineListSearchConfig, InlineListSelection, InlineMessageKind,
};

use super::registry::{SlashCommand, SlashCtx, SlashOutcome, SlashRegistry};

/// Register every extended command. Called from `register_all`.
pub(crate) fn register_extra(registry: &mut SlashRegistry) {
    registry.register(Box::new(ModelsCommand));
    registry.register(Box::new(ProvidersCommand));
    registry.register(Box::new(ToolsCommand));
    registry.register(Box::new(McpCommand));
    registry.register(Box::new(InfoCommand));
    registry.register(Box::new(ExportCommand));
}

// ─────────────────────────────────────────────────────────────────────────
// Formatting helpers (pure, unit-tested)
// ─────────────────────────────────────────────────────────────────────────

/// Format a token count as a compact context-window label.
pub(super) fn fmt_ctx(tokens: u32) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M ctx", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1000 {
        format!("{}K ctx", tokens / 1000)
    } else {
        format!("{tokens} ctx")
    }
}

/// Format a USD-per-million-token price. `0.0` (free / undisclosed) → "free".
pub(super) fn fmt_cost(price: f64) -> String {
    if price <= 0.0 {
        "free".to_string()
    } else if price < 0.01 {
        "<$0.01/M".to_string()
    } else {
        format!("${price:.2}/M")
    }
}

/// Split a `provider/model` id into `(provider, model_id)` on the first `/`.
pub(super) fn split_model_id(model_id: &str) -> (&str, &str) {
    match model_id.find('/') {
        Some(i) => (&model_id[..i], &model_id[i + 1..]),
        None => (model_id, ""),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// /models — browse the FULL catalog
// ─────────────────────────────────────────────────────────────────────────

/// `/models [query]` — open a searchable list of every catalog model
/// (provider/model · context window · price). Selecting a row switches the
/// active model. Unlike `/model` (scoped models only), this browses the entire
/// models.dev catalog plus local/dynamic discovery.
struct ModelsCommand;

impl SlashCommand for ModelsCommand {
    fn name(&self) -> &'static str {
        "models"
    }
    fn description(&self) -> &'static str {
        "Browse the full model catalog (/models [query])"
    }
    fn execute(&self, args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        let query = args.trim();
        let Some(catalog) = ctx.state.catalog.as_ref() else {
            ctx.reply(
                InlineMessageKind::Warning,
                "Model catalog is unavailable in this session.",
            );
            return SlashOutcome::Handled;
        };

        // `search_sync("")` returns the full snapshot; we filter client-side
        // so the optional `query` narrows provider/model/name in one pass.
        let mut entries = catalog.search_sync("");
        entries.sort_by(|a, b| {
            a.provider
                .cmp(&b.provider)
                .then_with(|| a.model_id.cmp(&b.model_id))
        });

        let q = query.to_ascii_lowercase();
        let filtered: Vec<_> = if q.is_empty() {
            entries.iter().collect()
        } else {
            entries
                .iter()
                .filter(|e| {
                    e.provider.to_ascii_lowercase().contains(&q)
                        || e.model_id.to_ascii_lowercase().contains(&q)
                        || e.name.to_ascii_lowercase().contains(&q)
                })
                .collect()
        };

        if filtered.is_empty() {
            if entries.is_empty() {
                ctx.reply(
                    InlineMessageKind::Warning,
                    "Model catalog is empty (catalog may have failed to load).",
                );
            } else {
                ctx.reply(
                    InlineMessageKind::Warning,
                    format!("No models match '{query}'."),
                );
            }
            return SlashOutcome::Handled;
        }

        let total = filtered.len();
        // Record the (provider, model_id) pairs backing each row so the
        // overlay-submission handler can resolve a selection back to a model.
        ctx.state.overlay_catalog_models = filtered
            .iter()
            .map(|e| (e.provider.clone(), e.model_id.clone()))
            .collect();

        let current = ctx.session.model_id();
        let items: Vec<InlineListItem> = filtered
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let id = format!("{}/{}", e.provider, e.model_id);
                let mut sub = format!(
                    "{} · {} in / {} out",
                    fmt_ctx(e.context_window),
                    fmt_cost(e.cost_input),
                    fmt_cost(e.cost_output)
                );
                if e.reasoning {
                    sub.push_str(" · reasoning");
                }
                if e.supports_vision {
                    sub.push_str(" · vision");
                }
                InlineListItem {
                    title: id.clone(),
                    subtitle: Some(sub),
                    badge: if id == current {
                        Some("active".to_string())
                    } else {
                        None
                    },
                    indent: 0,
                    selection: Some(InlineListSelection::CatalogModel(i)),
                    search_value: Some(format!("{} {} {}", e.provider, e.model_id, e.name)),
                }
            })
            .collect();

        let search = InlineListSearchConfig {
            label: "Filter models".into(),
            placeholder: Some("Type to filter (provider / model / name)\u{2026}".into()),
        };
        ctx.handle.show_list_modal(
            format!("Models ({total})"),
            vec![format!(
                "{} model{} \u{2014} Enter to switch, Esc to close",
                total,
                if total == 1 { "" } else { "s" }
            )],
            items,
            None,
            Some(search),
        );
        SlashOutcome::Handled
    }
}

// ─────────────────────────────────────────────────────────────────────────
// /providers — credential status + key removal
// ─────────────────────────────────────────────────────────────────────────

/// `/providers` — list every known provider with its credential status.
/// `/providers remove <name>` — remove a stored API key (asks for
/// confirmation; `--yes` skips it). Key *entry* still needs `oxicode setup`
/// (the overlay has no free-text input for secrets).
struct ProvidersCommand;

impl SlashCommand for ProvidersCommand {
    fn name(&self) -> &'static str {
        "providers"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["keys"]
    }
    fn description(&self) -> &'static str {
        "Show provider key status; remove a key (/providers remove <name>)"
    }
    fn execute(&self, args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        let tokens: Vec<&str> = args.split_whitespace().collect();

        // `/providers remove <name> [--yes]`
        if tokens
            .first()
            .map(|t| *t == "remove" || *t == "rm")
            .unwrap_or(false)
        {
            return remove_provider_key(ctx, tokens.get(1).copied(), tokens.contains(&"--yes"));
        }

        // `/providers` — status overlay.
        let auth = crate::store::auth_storage::shared_auth_storage();
        let mut names: Vec<String> = ctx
            .state
            .catalog
            .as_ref()
            .map(|c| c.list_providers_sync())
            .unwrap_or_default();
        // Merge custom providers from settings that the catalog doesn't list.
        if let Ok(settings) = crate::store::settings::Settings::load() {
            for cp in &settings.custom_providers {
                if !names.iter().any(|n| n == &cp.name) {
                    names.push(cp.name.clone());
                }
            }
        }
        names.sort();

        if names.is_empty() {
            ctx.reply(
                InlineMessageKind::Info,
                "No providers configured. Run `oxicode setup` to add one.",
            );
            return SlashOutcome::Handled;
        }

        ctx.state.overlay_providers = names.clone();
        let catalog = ctx.state.catalog.as_ref();
        let items: Vec<InlineListItem> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let has_key = auth.has(name);
                let entry = catalog.and_then(|c| c.get_provider_sync(name));
                let base = entry.as_ref().and_then(|p| p.base_url.clone());
                let env_key = entry.as_ref().and_then(|p| p.env_key.clone());
                let subtitle = match (env_key.as_deref(), base.as_deref()) {
                    (Some(env), Some(url)) if !url.is_empty() => format!("{env} · {url}"),
                    (Some(env), _) => env.to_string(),
                    (_, Some(url)) => url.to_string(),
                    _ => "Enter to manage".to_string(),
                };
                InlineListItem {
                    title: name.clone(),
                    subtitle: Some(subtitle),
                    badge: Some(if has_key {
                        "key".to_string()
                    } else {
                        "\u{2014}".to_string()
                    }),
                    indent: 0,
                    selection: Some(InlineListSelection::ProviderRow(i)),
                    search_value: Some(name.clone()),
                }
            })
            .collect();

        let keyed = names.iter().filter(|n| auth.has(n)).count();
        let search = InlineListSearchConfig {
            label: "Filter providers".into(),
            placeholder: Some("Type to filter\u{2026}".into()),
        };
        ctx.handle.show_list_modal(
            "Providers".into(),
            vec![format!(
                "{keyed}/{} with keys \u{2014} Enter to manage, Esc to close",
                names.len()
            )],
            items,
            None,
            Some(search),
        );
        SlashOutcome::Handled
    }
}

/// Handler for `/providers remove <name> [--yes]`.
fn remove_provider_key(ctx: &mut SlashCtx<'_>, name: Option<&str>, yes: bool) -> SlashOutcome {
    let Some(name) = name else {
        ctx.reply(InlineMessageKind::Error, "Usage: /providers remove <name>");
        return SlashOutcome::Handled;
    };
    let auth = crate::store::auth_storage::shared_auth_storage();
    if !auth.has(name) {
        ctx.reply(
            InlineMessageKind::Warning,
            format!("No stored key for '{name}'."),
        );
        return SlashOutcome::Handled;
    }
    if !yes {
        ctx.state.confirmation = Some(crate::tui_vt::main_loop::ModalConfirmation {
            title: format!("Remove key for {name}?"),
            message: "  y \u{2014} remove key     n / x \u{2014} cancel".into(),
            action: crate::tui_vt::main_loop::ConfirmationAction::RemoveProviderKey(
                name.to_string(),
            ),
        });
        return SlashOutcome::Handled;
    }
    auth.remove(name);
    ctx.reply(
        InlineMessageKind::Info,
        format!("Removed key for '{name}'."),
    );
    SlashOutcome::Handled
}

// ─────────────────────────────────────────────────────────────────────────
// /tools — registered tool inventory
// ─────────────────────────────────────────────────────────────────────────

/// `/tools` — list every registered agent tool (built-in + extension) with its
/// description and whether it is essential (cannot be disabled). Read-only.
struct ToolsCommand;

impl SlashCommand for ToolsCommand {
    fn name(&self) -> &'static str {
        "tools"
    }
    fn description(&self) -> &'static str {
        "List registered agent tools"
    }
    fn execute(&self, _args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        let tools = ctx.session.agent_ref().tools();
        let mut tools = tools.get_tools();
        tools.sort_by(|a, b| a.name().cmp(b.name()));

        let items: Vec<InlineListItem> = tools
            .iter()
            .map(|t| InlineListItem {
                title: t.name().to_string(),
                subtitle: Some(t.description().to_string()),
                badge: if t.essential() {
                    Some("essential".to_string())
                } else {
                    None
                },
                indent: 0,
                selection: None,
                search_value: Some(format!("{} {}", t.name(), t.label())),
            })
            .collect();

        let count = items.len();
        let essential = items.iter().filter(|i| i.badge.is_some()).count();
        let search = InlineListSearchConfig {
            label: "Filter tools".into(),
            placeholder: Some("Type to filter\u{2026}".into()),
        };
        ctx.handle.show_list_modal(
            format!("Tools ({count})"),
            vec![format!("{essential} essential \u{2014} Esc to close")],
            items,
            None,
            Some(search),
        );
        SlashOutcome::Handled
    }
}

// ─────────────────────────────────────────────────────────────────────────
// /mcp — MCP server dashboard
// ─────────────────────────────────────────────────────────────────────────

/// `/mcp` — show the MCP server dashboard: connection state, tool counts, and
/// settings summary, sourced from the synchronous `dashboard_data()` snapshot.
struct McpCommand;

impl SlashCommand for McpCommand {
    fn name(&self) -> &'static str {
        "mcp"
    }
    fn description(&self) -> &'static str {
        "Show MCP server status"
    }
    fn execute(&self, _args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        let Some(mcp) = ctx.session.agent_ref().tools().mcp_manager() else {
            ctx.reply(InlineMessageKind::Info, "No MCP manager configured.");
            return SlashOutcome::Handled;
        };
        let dash = mcp.dashboard_data();
        let s = &dash.settings;

        if dash.servers.is_empty() {
            ctx.reply(
                InlineMessageKind::Info,
                format!(
                    "No MCP servers configured ({} servers, {} tools).",
                    s.total_servers, s.total_tools
                ),
            );
            return SlashOutcome::Handled;
        }

        let items: Vec<InlineListItem> = dash
            .servers
            .iter()
            .map(|srv| {
                use oxicode_agent::mcp::types::McpConnectionStatus;
                let status = match &srv.status {
                    McpConnectionStatus::Connected => "connected",
                    McpConnectionStatus::Disconnected => "disconnected",
                    McpConnectionStatus::Connecting => "connecting",
                    McpConnectionStatus::Error(_) => "error",
                };
                InlineListItem {
                    title: srv.name.clone(),
                    subtitle: Some(format!(
                        "{status} · {} tool{} · {}",
                        srv.tool_count,
                        if srv.tool_count == 1 { "" } else { "s" },
                        srv.lifecycle
                    )),
                    badge: Some(status.to_string()),
                    indent: 0,
                    selection: None,
                    search_value: Some(srv.name.clone()),
                }
            })
            .collect();

        ctx.handle.show_list_modal(
            "MCP Servers".into(),
            vec![format!(
                "{}/{} connected · {} tools · prefix: {}",
                s.connected_servers, s.total_servers, s.total_tools, s.tool_prefix
            )],
            items,
            None,
            None,
        );
        SlashOutcome::Handled
    }
}

// ─────────────────────────────────────────────────────────────────────────
// /info — diagnostics
// ─────────────────────────────────────────────────────────────────────────

/// `/info` — a diagnostics snapshot: version, paths, model, provider, key
/// status, and catalog size. Useful for bug reports and "why isn't X working".
struct InfoCommand;

impl SlashCommand for InfoCommand {
    fn name(&self) -> &'static str {
        "info"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["diagnostics", "debug"]
    }
    fn description(&self) -> &'static str {
        "Show diagnostics: version, paths, model, catalog (alias: /diagnostics)"
    }
    fn execute(&self, _args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        let home = dirs::home_dir().unwrap_or_default();
        let model = ctx.session.model_id();
        let (provider, _) = split_model_id(&model);
        let auth = crate::store::auth_storage::shared_auth_storage();
        let key_status = if auth.has(provider) { "set" } else { "missing" };
        let catalog_count = ctx
            .state
            .catalog
            .as_ref()
            .map(|c| c.model_count_sync())
            .unwrap_or(0);
        let session_file = ctx
            .session
            .session_file()
            .unwrap_or_else(|| "(none)".into());

        let lines = vec![
            "".into(),
            format!("  oxicode   v{}", env!("CARGO_PKG_VERSION")),
            format!("  cwd       {}", ctx.state.cwd.display()),
            format!("  session   {}", ctx.session.session_id()),
            format!("  file      {session_file}"),
            "".into(),
            format!("  model     {model}"),
            format!("  provider  {provider}  (key: {key_status})"),
            format!("  catalog   {catalog_count} models"),
            "".into(),
            "  Paths".into(),
            format!(
                "  config    {}",
                home.join(".oxicode/settings.toml").display()
            ),
            format!("  auth      {}", home.join(".oxicode/auth.json").display()),
            format!("  sessions  {}", home.join(".oxicode/sessions").display()),
            format!("  logs      {}", home.join(".oxicode/logs").display()),
            "".into(),
        ];
        ctx.handle.show_modal("Diagnostics".into(), lines, None);
        SlashOutcome::Handled
    }
}

// ─────────────────────────────────────────────────────────────────────────
// /export — conversation → HTML
// ─────────────────────────────────────────────────────────────────────────

/// `/export` — render the current conversation as a self-contained HTML file
/// in the working directory and reply with the path.
struct ExportCommand;

impl SlashCommand for ExportCommand {
    fn name(&self) -> &'static str {
        "export"
    }
    fn description(&self) -> &'static str {
        "Export the conversation to HTML"
    }
    fn execute(&self, _args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        match ctx.session.export_html() {
            Ok(html) => {
                let id = ctx.session.session_id();
                let stem: String = id.chars().take(12).collect();
                let path = ctx.state.cwd.join(format!("oxicode-export-{stem}.html"));
                match std::fs::write(&path, html) {
                    Ok(()) => ctx.reply(
                        InlineMessageKind::Info,
                        format!("Exported to {}", path.display()),
                    ),
                    Err(e) => ctx.reply(
                        InlineMessageKind::Error,
                        format!("Failed to write export: {e}"),
                    ),
                }
            }
            Err(e) => ctx.reply(
                InlineMessageKind::Error,
                format!("Failed to export conversation: {e}"),
            ),
        }
        SlashOutcome::Handled
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_ctx_compact() {
        assert_eq!(fmt_ctx(0), "0 ctx");
        assert_eq!(fmt_ctx(500), "500 ctx");
        assert_eq!(fmt_ctx(8192), "8K ctx");
        assert_eq!(fmt_ctx(128_000), "128K ctx");
        assert_eq!(fmt_ctx(1_000_000), "1.0M ctx");
        assert_eq!(fmt_ctx(2_000_000), "2.0M ctx");
    }

    #[test]
    fn fmt_cost_edges() {
        assert_eq!(fmt_cost(0.0), "free");
        assert_eq!(fmt_cost(0.001), "<$0.01/M");
        assert_eq!(fmt_cost(3.0), "$3.00/M");
        assert_eq!(fmt_cost(15.0), "$15.00/M");
    }

    #[test]
    fn split_model_id_basic() {
        assert_eq!(
            split_model_id("anthropic/claude-3"),
            ("anthropic", "claude-3")
        );
        assert_eq!(split_model_id("bare"), ("bare", ""));
        // Only the first slash splits.
        assert_eq!(split_model_id("oai/gpt-4/vision"), ("oai", "gpt-4/vision"));
    }
}
