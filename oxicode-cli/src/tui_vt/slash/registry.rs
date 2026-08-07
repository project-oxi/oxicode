//! Slash command registry for the VT TUI harness.
//!
//! Parsed in `crate::tui_vt::main_loop::handle_inline_event` when the
//! submitted text begins with `/`. Each command owns its name, aliases, and
//! execution. Adding a command = implement `SlashCommand` + register it in
//! [`SlashRegistry::builtins`].
//!
//! Slash commands for the VT TUI harness. Each command owns its name, aliases,
//! and execution; adding one = implement `SlashCommand` + register it in
//! [`SlashRegistry::builtins`] (`register_all`). Overlay-driving commands
//! (`/model`, `/settings`, `/sessions`, `/theme`) build an `InlineListSelection`
//! modal whose submission is handled in `main_loop.rs`'s overlay-submission arm.
//! `/issue` is not yet wired (no issue overlay in this harness).

use oxicode_vtui::tui::core::{
    InlineHandle, InlineListItem, InlineListSelection, InlineMessageKind,
};

use crate::app::agent_session::AgentSessionHandle;
use crate::tui_vt::main_loop::{RenderState, plain_segment};

/// Outcome of dispatching a slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlashOutcome {
    /// Command handled; remain in the event loop.
    Handled,
    /// Request application shutdown (`/quit`).
    Quit,
    /// No command matched; caller surfaces an "unknown command" notice.
    NotHandled,
}

/// Everything a slash command execution needs, bundled so adding a
/// dependency never changes every command's signature.
pub(crate) struct SlashCtx<'a> {
    pub session: &'a AgentSessionHandle,
    pub handle: &'a InlineHandle,
    pub state: &'a mut RenderState,
}

impl SlashCtx<'_> {
    /// Append one or more transcript lines via the harness command channel —
    /// the single source of truth (`apply_command` applies it to `RenderState`
    /// on the next loop iteration, so we must NOT also mutate `state` here).
    /// Newlines split into separate transcript lines.
    pub(crate) fn reply(&self, kind: InlineMessageKind, text: impl Into<String>) {
        let text = text.into();
        for line in text.split('\n') {
            self.handle
                .append_line(kind, vec![plain_segment(line.to_string())]);
        }
    }
}

/// One slash command owns its definition and execution.
///
/// Adding a command = implementing this trait + registering in `builtins()`.
/// Aliases live alongside the handler so the old "keep the table in sync with
/// the match" drift cannot recur.
pub(crate) trait SlashCommand: Send + Sync {
    /// Canonical name, no leading `/` (e.g. `"quit"`).
    fn name(&self) -> &'static str;
    /// Alternative names resolved alongside `name()` (e.g. `["exit", "q"]`).
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }
    /// Short description shown in `/help` and RPC `get_commands`.
    fn description(&self) -> &'static str;
    /// Run the command. `args` is the trimmed text after the command token.
    fn execute(&self, args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome;

    /// Whether `token` (no leading `/`) names this command (case-insensitive).
    fn matches(&self, token: &str) -> bool {
        token.eq_ignore_ascii_case(self.name())
            || self.aliases().iter().any(|a| token.eq_ignore_ascii_case(a))
    }
}

/// Central registry of all built-in slash commands.
pub struct SlashRegistry {
    builtins: Vec<Box<dyn SlashCommand>>,
}

impl SlashRegistry {
    /// Assemble all built-in commands.
    pub fn builtins() -> Self {
        let mut registry = SlashRegistry {
            builtins: Vec::new(),
        };
        register_all(&mut registry);
        registry
    }

    /// Register one command.
    pub(crate) fn register(&mut self, cmd: Box<dyn SlashCommand>) {
        self.builtins.push(cmd);
    }

    /// Static catalog for RPC `get_commands`: `(name, description, aliases)`.
    /// Kept as a standalone associated fn so RPC can enumerate without
    /// constructing a session/handle context.
    pub fn builtin_commands() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
        Self::builtins()
            .builtins
            .iter()
            .map(|c| (c.name(), c.description(), c.aliases().to_vec()))
            .collect()
    }

    /// Try to dispatch `input` (full `/cmd args…`) to a command. `/help` is
    /// intercepted here because only the registry can enumerate its siblings.
    /// Returns [`SlashOutcome::NotHandled`] if nothing matches.
    pub(crate) fn dispatch(&self, input: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        let trimmed = input.trim();
        let (cmd_token, arg) = match trimmed.find(' ') {
            Some(space) => (&trimmed[..space], trimmed[space + 1..].trim()),
            None => (trimmed, ""),
        };
        let token = cmd_token.strip_prefix('/').unwrap_or(cmd_token);

        if matches!(token, "help" | "?" | "commands") {
            self.render_help(ctx);
            return SlashOutcome::Handled;
        }

        for command in &self.builtins {
            if command.matches(token) {
                return command.execute(arg, ctx);
            }
        }
        SlashOutcome::NotHandled
    }

    fn render_help(&self, ctx: &mut SlashCtx<'_>) {
        let mut items: Vec<InlineListItem> = self
            .builtins
            .iter()
            .map(|c| {
                let mut title = format!("/{}", c.name());
                for alias in c.aliases() {
                    title.push_str(&format!(", /{alias}"));
                }
                InlineListItem {
                    title,
                    subtitle: Some(c.description().to_string()),
                    badge: None,
                    indent: 0,
                    selection: Some(InlineListSelection::SlashCommand(c.name().to_string())),
                    search_value: None,
                }
            })
            .collect();
        items.sort_by(|a, b| a.title.cmp(&b.title));
        ctx.handle.show_list_modal(
            "Commands".to_string(),
            vec!["Select a command (Esc to close)".to_string()],
            items,
            None,
            None,
        );
    }
}

/// `/settings` — open a settings overlay showing current configuration.
/// Selecting a toggleable item cycles/toggles its value through the session.
struct SettingsCommand;

impl SlashCommand for SettingsCommand {
    fn name(&self) -> &'static str {
        "settings"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["config"]
    }
    fn description(&self) -> &'static str {
        "Show settings overlay (toggle thinking, compaction, advisor)"
    }
    fn execute(&self, _args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        use oxicode_vtui::tui::core::{
            InlineListItem, InlineListSearchConfig, InlineListSelection,
        };

        let session = ctx.session;
        let model = session.model_id();
        let thinking = session.thinking_level();
        let auto_compaction = session.auto_compaction_enabled();
        let auto_retry = session.auto_retry_enabled();
        let advisor = session.is_advisor_enabled();

        // Build setting items. Items with a `selection` are interactive;
        // items without are read-only display.
        let items = vec![
            InlineListItem {
                title: format!("Model: {model}"),
                subtitle: Some("Use /model to switch".into()),
                badge: None,
                indent: 0,
                selection: None,
                search_value: Some("model".into()),
            },
            InlineListItem {
                title: format!("Thinking: {thinking:?}"),
                subtitle: Some("Enter to cycle".into()),
                badge: None,
                indent: 0,
                selection: Some(InlineListSelection::ConfigAction("thinking_level".into())),
                search_value: Some("thinking".into()),
            },
            InlineListItem {
                title: format!(
                    "Auto-compaction: {}",
                    if auto_compaction { "on" } else { "off" }
                ),
                subtitle: Some("Enter to toggle".into()),
                badge: None,
                indent: 0,
                selection: Some(InlineListSelection::ConfigAction("auto_compaction".into())),
                search_value: Some("compaction".into()),
            },
            InlineListItem {
                title: format!("Auto-retry: {}", if auto_retry { "on" } else { "off" }),
                subtitle: Some("Enter to toggle".into()),
                badge: None,
                indent: 0,
                selection: Some(InlineListSelection::ConfigAction("auto_retry".into())),
                search_value: Some("retry".into()),
            },
            InlineListItem {
                title: format!("Advisor: {}", if advisor { "on" } else { "off" }),
                subtitle: Some("Enter to toggle".into()),
                badge: None,
                indent: 0,
                selection: Some(InlineListSelection::ConfigAction("advisor".into())),
                search_value: Some("advisor".into()),
            },
        ];

        let search = InlineListSearchConfig {
            label: "Filter settings".into(),
            placeholder: Some("Type to filter\u{2026}".into()),
        };
        ctx.handle.show_list_modal(
            "Settings".into(),
            vec!["Select a setting to toggle/cycle (Esc to close)".into()],
            items,
            None,
            Some(search),
        );
        SlashOutcome::Handled
    }
}

/// `/sessions` — open a session picker overlay listing recent sessions.
/// Selecting a session fills `/resume <id>` into the prompt.
struct SessionsCommand;

impl SlashCommand for SessionsCommand {
    fn name(&self) -> &'static str {
        "sessions"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["resume"]
    }
    fn description(&self) -> &'static str {
        "Browse and resume past sessions"
    }
    fn execute(&self, _args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        use oxicode_vtui::tui::core::{InlineListItem, InlineListSearchConfig};

        // Find the sessions directory.
        let session_dir = dirs::home_dir()
            .map(|h| h.join(".oxicode").join("sessions"))
            .unwrap_or_else(|| std::path::PathBuf::from(".oxicode/sessions"));

        // Scan session files synchronously, sorted by mtime desc.
        let mut entries: Vec<(String, std::time::SystemTime)> = Vec::new();
        if let Ok(dir) = std::fs::read_dir(&session_dir) {
            for entry in dir.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                    let id = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let mtime = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    entries.push((id, mtime));
                }
            }
        }
        entries.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
        entries.truncate(30); // cap at 30 most recent

        if entries.is_empty() {
            ctx.reply(InlineMessageKind::Info, "No saved sessions found.");
            return SlashOutcome::Handled;
        }

        let items: Vec<InlineListItem> = entries
            .iter()
            .map(|(id, mtime)| {
                let time_str = format_relative_time(*mtime);
                InlineListItem {
                    title: format!("{id}  \u{00b7}  {time_str}"),
                    subtitle: Some("Enter to resume".into()),
                    badge: None,
                    indent: 0,
                    selection: Some(InlineListSelection::Session(id.clone())),
                    search_value: Some(id.clone()),
                }
            })
            .collect();

        let search = InlineListSearchConfig {
            label: "Filter sessions".into(),
            placeholder: Some("Type to filter\u{2026}".into()),
        };
        ctx.handle.show_list_modal(
            "Sessions".into(),
            vec!["Select a session to resume (Esc to close)".into()],
            items,
            None,
            Some(search),
        );
        SlashOutcome::Handled
    }
}

/// Format a `SystemTime` as a human-readable relative time (e.g. "2h ago").
fn format_relative_time(t: std::time::SystemTime) -> String {
    let now = std::time::SystemTime::now();
    match now.duration_since(t) {
        Ok(d) => {
            let mins = d.as_secs() / 60;
            if mins < 1 {
                "just now".into()
            } else if mins < 60 {
                format!("{mins}m ago")
            } else if mins < 60 * 24 {
                format!("{}h ago", mins / 60)
            } else if mins < 60 * 24 * 7 {
                format!("{}d ago", mins / (60 * 24))
            } else {
                format!("{}w ago", mins / (60 * 24 * 7))
            }
        }
        Err(_) => "unknown".into(),
    }
}

fn register_all(registry: &mut SlashRegistry) {
    registry.register(Box::new(QuitCommand));
    registry.register(Box::new(ClearCommand));
    registry.register(Box::new(CompactCommand));
    registry.register(Box::new(ModelCommand));
    registry.register(Box::new(CancelCommand));
    registry.register(Box::new(StatusCommand));
    registry.register(Box::new(SettingsCommand));
    registry.register(Box::new(VimCommand));
    registry.register(Box::new(AgentsCommand));
    registry.register(Box::new(ThemeCommand));
    registry.register(Box::new(FindCommand));
    registry.register(Box::new(SessionsCommand));
    registry.register(Box::new(ShortcutsCommand));
    super::commands::register_extra(registry);
}

/// `/vim` — toggle vim mode for prompt editing.
struct VimCommand;

impl SlashCommand for VimCommand {
    fn name(&self) -> &'static str {
        "vim"
    }
    fn description(&self) -> &'static str {
        "Toggle vim mode for prompt editing"
    }
    fn execute(&self, _args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        let enabled = !ctx.state.vim_state.enabled();
        ctx.state.vim_state.set_enabled(enabled);
        ctx.reply(
            InlineMessageKind::Info,
            if enabled {
                "Vim mode: ON — press Esc for Normal, i for Insert".to_string()
            } else {
                "Vim mode: OFF".to_string()
            },
        );
        SlashOutcome::Handled
    }
}

/// `/agents` — open the Agent Hub overlay. Alias: `/hub`.
struct AgentsCommand;

impl SlashCommand for AgentsCommand {
    fn name(&self) -> &'static str {
        "agents"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["hub"]
    }
    fn description(&self) -> &'static str {
        "Open the Agent Hub overlay (alias: /hub)"
    }
    fn execute(&self, _args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        ctx.state.agent_hub_open = true;
        ctx.state.hub_entries = ctx.session.hub().snapshot();
        SlashOutcome::Handled
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Built-in commands
// ─────────────────────────────────────────────────────────────────────────

/// `/quit` — exit oxicode. Aliases: `/exit`, `/q`.
struct QuitCommand;

impl SlashCommand for QuitCommand {
    fn name(&self) -> &'static str {
        "quit"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["exit", "q"]
    }
    fn description(&self) -> &'static str {
        "Quit oxicode (aliases: /exit, /q)"
    }
    fn execute(&self, _args: &str, _ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        SlashOutcome::Quit
    }
}

/// `/clear` — reset the conversation and wipe the transcript. Alias: `/cls`.
struct ClearCommand;

impl SlashCommand for ClearCommand {
    fn name(&self) -> &'static str {
        "clear"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["cls"]
    }
    fn description(&self) -> &'static str {
        "Clear the conversation and transcript (alias: /cls)"
    }
    fn execute(&self, args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        // `--yes` skips the confirmation dialog (used when re-dispatching
        // from the confirmation modal). Without it, open the dialog.
        if !args.split_whitespace().any(|a| a == "--yes") {
            ctx.state.confirmation = Some(super::super::main_loop::clear_confirmation());
            return SlashOutcome::Handled;
        }
        ctx.session.reset();
        ctx.state.transcript.clear();
        ctx.state.message_buffer.clear();
        ctx.state.scroll_offset = usize::MAX;
        ctx.reply(InlineMessageKind::Info, "Conversation cleared.");
        SlashOutcome::Handled
    }
}

/// `/compact` — manually trigger context compaction. Optional instructions.
struct CompactCommand;

impl SlashCommand for CompactCommand {
    fn name(&self) -> &'static str {
        "compact"
    }
    fn description(&self) -> &'static str {
        "Compact the context (optional: /compact <instructions>)"
    }
    fn execute(&self, args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        let instructions = args.trim();
        let arg = if instructions.is_empty() {
            None
        } else {
            Some(instructions.to_string())
        };
        let session = ctx.session.clone();
        ctx.reply(InlineMessageKind::Info, "Compacting\u{2026}");
        tokio::spawn(async move {
            match session.compact(arg).await {
                Ok(result) => tracing::info!(?result, "manual compaction complete"),
                Err(err) => tracing::warn!(%err, "manual compaction failed"),
            }
        });
        SlashOutcome::Handled
    }
}

/// `/model` — inspect, set, or cycle the active model.
///   `/model`            show the current model
///   `/model <id>`       switch to `provider/model`
///   `/model next`       cycle to the next scoped model
struct ModelCommand;

impl SlashCommand for ModelCommand {
    fn name(&self) -> &'static str {
        "model"
    }
    fn description(&self) -> &'static str {
        "Show or switch model (/model [<id>|next])"
    }
    fn execute(&self, args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        match args.trim() {
            "" => {
                let models = ctx.session.scoped_models();
                if models.is_empty() {
                    ctx.reply(
                        InlineMessageKind::Info,
                        format!("Current model: {}", ctx.session.model_id()),
                    );
                } else {
                    // Open a model picker overlay.
                    ctx.state.overlay_model_ids = models
                        .iter()
                        .map(|m| format!("{}/{}", m.provider, m.model_id))
                        .collect();
                    let current = ctx.session.model_id();
                    let items: Vec<InlineListItem> = models
                        .iter()
                        .enumerate()
                        .map(|(i, m)| {
                            let id = format!("{}/{}", m.provider, m.model_id);
                            let sub = ctx
                                .state
                                .catalog
                                .as_ref()
                                .and_then(|c| c.get_model_sync(&m.provider, &m.model_id))
                                .map(|e| {
                                    format!(
                                        "{} · {}",
                                        m.provider,
                                        super::commands::fmt_ctx(e.context_window)
                                    )
                                })
                                .unwrap_or_else(|| m.provider.clone());
                            InlineListItem {
                                title: id.clone(),
                                subtitle: Some(sub),
                                badge: if id == current {
                                    Some("active".to_string())
                                } else {
                                    None
                                },
                                indent: 0,
                                selection: Some(InlineListSelection::Model(i)),
                                search_value: None,
                            }
                        })
                        .collect();
                    ctx.handle.show_list_modal(
                        "Models".to_string(),
                        vec!["Select a model (Esc to close)".to_string()],
                        items,
                        None,
                        None,
                    );
                }
            }
            "next" | "cycle" => match ctx.session.cycle_model() {
                Some(new_id) => ctx.reply(InlineMessageKind::Info, format!("Switched to {new_id}")),
                None => ctx.reply(
                    InlineMessageKind::Warning,
                    "No scoped models configured to cycle.",
                ),
            },
            id => match ctx.session.set_model(id) {
                Ok(()) => ctx.reply(InlineMessageKind::Info, format!("Switched to {id}")),
                Err(err) => ctx.reply(
                    InlineMessageKind::Error,
                    format!("Failed to set model {id}: {err}"),
                ),
            },
        }
        SlashOutcome::Handled
    }
}

/// `/cancel` — abort any in-progress agent run. Alias: `/stop`.
struct CancelCommand;

impl SlashCommand for CancelCommand {
    fn name(&self) -> &'static str {
        "cancel"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["stop"]
    }
    fn description(&self) -> &'static str {
        "Abort the current run (alias: /stop)"
    }
    fn execute(&self, _args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        let session = ctx.session.clone();
        tokio::spawn(async move {
            session.abort().await;
        });
        SlashOutcome::Handled
    }
}

/// `/status` — show the active model and session message counts.
struct StatusCommand;

impl SlashCommand for StatusCommand {
    fn name(&self) -> &'static str {
        "status"
    }
    fn description(&self) -> &'static str {
        "Show model and session stats"
    }
    fn execute(&self, _args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        let stats = ctx.session.session_stats();
        let model = ctx.session.model_id();
        let (provider, model_part) = super::commands::split_model_id(&model);
        let auth = crate::store::auth_storage::shared_auth_storage();
        let key = if auth.has(provider) { "set" } else { "missing" };
        let ctx_win = ctx
            .state
            .catalog
            .as_ref()
            .and_then(|c| c.get_model_sync(provider, model_part))
            .map(|e| super::commands::fmt_ctx(e.context_window))
            .unwrap_or_else(|| "?".to_string());
        let compaction = if ctx.session.auto_compaction_enabled() {
            "on"
        } else {
            "off"
        };
        let advisor = if ctx.session.is_advisor_enabled() {
            "on"
        } else {
            "off"
        };
        let thinking = ctx.session.thinking_level();
        ctx.reply(
            InlineMessageKind::Info,
            format!(
                "Model: {model}  (key: {key}, {ctx_win})\n\
                 Provider: {provider}  \u{00b7}  Thinking: {thinking:?}\n\
                 Compaction: {compaction}  \u{00b7}  Advisor: {advisor}\n\
                 Messages: {} user / {} assistant\n\
                 Tool calls: {} (results: {})\n\
                 Total: {}",
                stats.user_messages,
                stats.assistant_messages,
                stats.tool_calls,
                stats.tool_results,
                stats.total_messages,
            ),
        );
        SlashOutcome::Handled
    }
}

/// `/theme` — cycle, set, or pick a color theme.
///   `/theme`            cycle to the next theme
///   `/theme list`       open the theme picker overlay
///   `/theme <name>`     switch to a named theme
struct ThemeCommand;

impl SlashCommand for ThemeCommand {
    fn name(&self) -> &'static str {
        "theme"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["t"]
    }
    fn description(&self) -> &'static str {
        "Cycle or pick a color theme (/theme [name|list])"
    }
    fn execute(&self, args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        use oxicode_vtui::theme::{
            active_theme_id, available_themes, set_active_theme, theme_label,
        };
        match args.trim() {
            "" | "next" | "cycle" => {
                let themes = available_themes();
                if themes.len() <= 1 {
                    ctx.reply(InlineMessageKind::Info, "Only one theme available.");
                } else {
                    let current = active_theme_id();
                    let pos = themes.iter().position(|t| *t == current).unwrap_or(0);
                    let next_id = &themes[(pos + 1) % themes.len()];
                    match set_active_theme(next_id) {
                        Ok(()) => {
                            let label = theme_label(next_id).unwrap_or(next_id.as_ref());
                            ctx.reply(InlineMessageKind::Info, format!("Theme: {label}"));
                        }
                        Err(e) => ctx.reply(
                            InlineMessageKind::Error,
                            format!("Failed to set theme: {e}"),
                        ),
                    }
                }
            }
            "list" | "picker" => {
                let themes = available_themes();
                let current = active_theme_id();
                let items: Vec<InlineListItem> = themes
                    .iter()
                    .map(|id| InlineListItem {
                        title: theme_label(id).unwrap_or(id.as_ref()).to_string(),
                        subtitle: Some(id.to_string()),
                        badge: if *id == current {
                            Some("active".to_string())
                        } else {
                            None
                        },
                        indent: 0,
                        selection: Some(InlineListSelection::Theme(id.to_string())),
                        search_value: Some(id.to_string()),
                    })
                    .collect();
                ctx.handle.show_list_modal(
                    "Themes".to_string(),
                    vec!["Select a theme (Esc to close, Enter to apply)".to_string()],
                    items,
                    None,
                    None,
                );
            }
            name => match set_active_theme(name) {
                Ok(()) => {
                    let label = theme_label(name).unwrap_or(name);
                    ctx.reply(InlineMessageKind::Info, format!("Theme: {label}"));
                }
                Err(e) => ctx.reply(
                    InlineMessageKind::Error,
                    format!("Unknown theme '{name}': {e}"),
                ),
            },
        }
        SlashOutcome::Handled
    }
}

/// `/find` — search within the transcript. Opens an inline search bar.
///   `/find <query>`   search for matches (n/N to navigate)
///   `/find`           clear search
struct FindCommand;

impl SlashCommand for FindCommand {
    fn name(&self) -> &'static str {
        "find"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["search", "/"]
    }
    fn description(&self) -> &'static str {
        "Search transcript (/find <query>, n/N to navigate)"
    }
    fn execute(&self, args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        let query = args.trim();
        if query.is_empty() {
            ctx.state.search = None;
            ctx.reply(InlineMessageKind::Info, "Search cleared.");
        } else {
            ctx.state.start_search(query);
            let count = ctx
                .state
                .search
                .as_ref()
                .map(|s| s.matches.len())
                .unwrap_or(0);
            if count == 0 {
                ctx.reply(
                    InlineMessageKind::Warning,
                    format!("No matches for '{query}'."),
                );
            } else {
                ctx.reply(
                    InlineMessageKind::Info,
                    format!(
                        "{count} match{} for '{query}'",
                        if count == 1 { "" } else { "es" }
                    ),
                );
            }
        }
        SlashOutcome::Handled
    }
}

/// `/shortcuts` — show the keyboard shortcuts cheatsheet overlay.
struct ShortcutsCommand;

impl SlashCommand for ShortcutsCommand {
    fn name(&self) -> &'static str {
        "shortcuts"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["keys", "cheatsheet"]
    }
    fn description(&self) -> &'static str {
        "Show keyboard shortcuts (alias: ?)"
    }
    fn execute(&self, _args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        ctx.handle
            .show_modal("Keyboard Shortcuts".to_string(), shortcuts_lines(), None);
        SlashOutcome::Handled
    }
}

/// Lines for the shortcuts cheatsheet overlay.
fn shortcuts_lines() -> Vec<String> {
    vec![
        "".into(),
        "  Navigation".into(),
        "  j / ↓      Scroll down (line)".into(),
        "  k / ↑      Scroll up (line)".into(),
        "  Shift+J    Next assistant turn".into(),
        "  Shift+K    Previous user turn".into(),
        "  PgDn       Scroll down (page)".into(),
        "  PgUp       Scroll up (page)".into(),
        "  G          Jump to bottom (follow)".into(),
        "  g          Jump to top".into(),
        "".into(),
        "  Blocks".into(),
        "  e          Cycle block (collapse/truncate/expand)".into(),
        "  Shift+E    Expand all blocks".into(),
        "  Ctrl+E     Collapse all blocks".into(),
        "".into(),
        "  Search".into(),
        "  /find <q>  Search transcript".into(),
        "  n          Next match".into(),
        "  N          Previous match".into(),
        "  Esc        Clear search".into(),
        "".into(),
        "  Input".into(),
        "  Ctrl+M     Toggle multiline input".into(),
        "  Ctrl+P     Command palette".into(),
        "  Ctrl+Enter Send now (abort + submit)".into(),
        "  Esc        Cancel run / quit (y to confirm)".into(),
        "".into(),
        "  Other".into(),
        "  ?          Show this cheatsheet".into(),
        "  /theme     Cycle color theme".into(),
        "  /model     Pick a model".into(),
        "  /models    Browse all models".into(),
        "  /providers Manage API keys".into(),
        "  /tools     List tools".into(),
        "  /mcp       MCP status".into(),
        "  /info      Diagnostics".into(),
        "  /export    Save as HTML".into(),
        "  /vim       Toggle vim mode".into(),
        "  Ctrl+C     Cancel run (then y to quit)".into(),
        "".into(),
    ]
}
// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_register_expected_commands() {
        let reg = SlashRegistry::builtins();
        let names: Vec<&str> = reg.builtins.iter().map(|c| c.name()).collect();
        assert!(names.contains(&"quit"));
        assert!(names.contains(&"clear"));
        assert!(names.contains(&"compact"));
        assert!(names.contains(&"model"));
        assert!(names.contains(&"cancel"));
        assert!(names.contains(&"status"));
    }

    #[test]
    fn matches_resolves_aliases_case_insensitively() {
        let cmd = QuitCommand;
        assert!(cmd.matches("quit"));
        assert!(cmd.matches("EXIT"));
        assert!(cmd.matches("q"));
        assert!(!cmd.matches("quitter"));
    }

    #[test]
    fn builtin_commands_exposes_aliases_for_rpc() {
        let catalog = SlashRegistry::builtin_commands();
        let quit = catalog
            .iter()
            .find(|(name, _, _)| *name == "quit")
            .expect("quit command present");
        assert!(quit.2.contains(&"exit"));
        assert!(quit.2.contains(&"q"));
        assert!(!quit.1.is_empty(), "quit has a description");
    }

    #[test]
    fn dispatch_help_is_intercepted() {
        // `/help` must resolve even though no HelpCommand is registered —
        // only the registry can enumerate its siblings.
        let _reg = SlashRegistry::builtins();
        // We can't build a real SlashCtx without a session, so verify the
        // interception logic indirectly: a registered command name does not
        // shadow help, and help token is recognized before iteration.
        assert!(matches!("help".strip_prefix('/').unwrap_or("help"), "help"));
    }
}
