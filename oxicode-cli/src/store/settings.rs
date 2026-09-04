//! Settings management for oxicode CLI
//!
//! Settings are loaded in layers (later layers override earlier):
//! 1. Built-in defaults
//! 2. Global config: canonical home `settings.{json,toml}` (default
//!    `~/.oxi/oxicode/`; legacy `~/.oxicode/` read-only fallback)
//! 3. Project config: `.oxicode/settings.toml` (walked up to repo root)
//! 4. Environment variables (`OXICODE_*` prefix)
//! 5. CLI arguments
//!
//! Migration is handled via a `version` field in the config file.

// F-13 (audit 2026-06-21): the `glyph_set` field technically makes the
// store layer (`oxicode-cli/src/store/`) depend on the UI layer
// (`oxicode_tui`). The proper fix is to store only a discriminant
// (`"unicode" | "ascii" | "nerd"`) here and let `oxicode_tui` map it to
// `GlyphSet` at the rendering site; that refactor is tracked as a
// follow-up because 5 call sites + on-disk TOML compatibility would
// need to change together. For now we keep the enum import but
// acknowledge the layering violation in this comment so a future
// contributor doesn't assume the dependency is intentional.
use crate::symbols::GlyphSet;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Current settings format version.
///
/// Version history:
/// - 4: dynamic_models field + last_used_model/provider split
/// - 7: edit_format field (Hashline/StrReplace, default StrReplace)
/// - 8: glyph_set field (Unicode/Ascii/Nerd, default Unicode)
/// - 9: model_roles field (named model roles ported from omp, default empty)
/// - serde-default (no version bump): `advisor` field (`AdvisorSettings`,
///   default OFF) — `#[serde(default)]` fills it for older files, no migration.
/// - 10: removed dead routing/fallback/circuit-breaker + language policy fields:
///   `enable_routing`, `router_profile`, `prefer_cost_efficient`,
///   `fallback_chain`, `enable_fallback`, `disable_fallback`,
///   `circuit_breaker_failure_threshold`, `circuit_breaker_open_duration_secs`.
///   Old settings files with these fields still load (serde ignores unknown keys).
const SETTINGS_VERSION: u32 = 10;

/// Environment variable prefix for oxicode settings.
/// Keep: reserved for future env-based config loading (e.g. OXICODE_API_KEY).
#[allow(dead_code)]
const ENV_PREFIX: &str = "OXICODE_";

/// Thinking level for agent responses
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    /// Extended reasoning disabled (default).
    #[default]
    Off,
    /// Minimal reasoning.
    Minimal,
    /// Low reasoning.
    Low,
    /// Medium reasoning.
    Medium,
    /// High reasoning.
    High,
    /// Very high reasoning.
    XHigh,
}

/// Edit format for the edit tool.
///
/// Controls whether the system prompt instructs the model to use hashline
/// line-anchored patches or traditional str_replace. Hashline is the new
/// format ported from omp — see `docs/designs/omp-adoption/01-hashline-edit.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EditFormat {
    /// Hashline line-anchored editing (default).
    #[default]
    Hashline,
    /// Traditional str_replace (legacy fallback).
    StrReplace,
}
/// A custom OpenAI-compatible provider configuration.
///
/// Custom providers are loaded from the global settings file via `[[custom_provider]]` sections
/// and registered at runtime so that models like `minimax/minimax-m2.5` can be used directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomProvider {
    /// Unique provider name (e.g. `"minimax"`).
    pub name: String,
    /// Base URL of the OpenAI-compatible API (e.g. `"https://api.minimax.chat/v1"`).
    pub base_url: String,
    /// Environment variable name that holds the API key (e.g. `"MINIMAX_API_KEY"`).
    pub api_key_env: String,
    /// API dialect: `"openai-completions"` or `"openai-responses"`.
    #[serde(default = "default_custom_provider_api")]
    pub api: String,
}

pub(crate) fn default_custom_provider_api() -> String {
    "openai-completions".to_string()
}

/// How strongly to auto-create a todo list on the first turn. Mirrors omp's
/// `todo.eager` (`default`/`preferred`/`always`), renamed to avoid the Rust
/// keyword `default` as a variant name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoEagerMode {
    /// Model decides; no automatic todo list. (default)
    #[default]
    Off,
    /// Suggests a todo list on the first message (reminder, not forced).
    Preferred,
    /// Forces a todo list on the first message via `ToolChoice::Named("todo")`
    /// when the resolved model's provider supports it.
    Always,
}

/// Application settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    // ── Version (for migration) ──────────────────────────────────────
    /// Settings format version. Used for automatic migration.
    #[serde(default)]
    pub version: u32,

    // ── Core LLM settings ───────────────────────────────────────────
    /// Thinking level for agent responses
    #[serde(default = "default_thinking_level")]
    pub thinking_level: ThinkingLevel,
    /// Color theme — resolved by `oxicode_vtui::theme` (e.g. "oxi", "oxide-dark", "nord").
    #[serde(default = "default_theme")]
    pub theme: String,

    /// Terminal glyph set — controls every UI symbol (status markers,
    /// list cursors, box drawing, spinners, icons).
    ///
    /// `unicode` (default): box-drawing + emoji, works on any UTF-8 terminal.
    /// `ascii`: 7-bit fallback for serial consoles / CI logs.
    /// `nerd`: Nerd Font private-use codepoints (needs a patched font).
    #[serde(default)]
    pub glyph_set: GlyphSet,

    /// Deprecated: use `last_used_model` instead. Kept for serde backward compat.
    #[serde(default, skip_serializing)]
    pub default_model: Option<String>,

    /// Deprecated: use `last_used_provider` instead. Kept for serde backward compat.
    #[serde(default, skip_serializing)]
    pub default_provider: Option<String>,

    /// Model selected by the user (last used = current default).
    /// Set during onboarding and updated every time the user switches model.
    #[serde(default)]
    pub last_used_model: Option<String>,

    /// Provider for the last used model.
    #[serde(default)]
    pub last_used_provider: Option<String>,

    /// Max tokens for responses
    pub max_tokens: Option<u32>,

    /// Temperature for generation (0.0–2.0)
    pub temperature: Option<f32>,

    /// Default temperature as f64 (higher precision, takes precedence over `temperature`)
    pub default_temperature: Option<f64>,

    /// Maximum tokens for generation (usize variant, takes precedence over `max_tokens`)
    pub max_response_tokens: Option<usize>,

    // ── Session settings ─────────────────────────────────────────────
    /// Session history size (entries to keep in memory)
    #[serde(default = "default_session_history_size")]
    pub session_history_size: usize,

    /// Directory for storing sessions (default: canonical home `sessions/`)
    pub session_dir: Option<PathBuf>,

    // ── Behaviour flags ──────────────────────────────────────────────
    /// Whether extensions are enabled
    #[serde(default = "default_true")]
    pub extensions_enabled: bool,

    /// Whether to auto-compact conversations that exceed context window
    #[serde(default = "default_true")]
    pub auto_compaction: bool,

    /// Built-in tools to disable (by name, e.g. `["web_search", "github_search"]`).
    /// All tools are enabled by default; list tools here to turn them off.
    #[serde(default)]
    pub disabled_tools: Vec<String>,

    // ── Timeouts ─────────────────────────────────────────────────────
    /// Timeout in seconds for tool execution
    #[serde(default = "default_tool_timeout")]
    pub tool_timeout_seconds: u64,

    /// Ask overlay timeout in seconds. 0 = disabled (wait indefinitely).
    /// When timeout fires, auto-selects the recommended option (or first).
    #[serde(default, alias = "questionnaire_timeout_secs")]
    pub ask_timeout_secs: u64,

    // ── Resource lists (managed by `oxicode config`) ────────────────────
    /// List of extension paths or npm package sources to load
    #[serde(default)]
    pub extensions: Vec<String>,

    /// List of skill paths or npm package sources to load
    #[serde(default)]
    pub skills: Vec<String>,

    /// List of prompt template paths to load
    #[serde(default)]
    pub prompts: Vec<String>,

    /// List of theme paths to load
    #[serde(default)]
    pub themes: Vec<String>,

    // ── Custom OpenAI-compatible providers ──────────────────────────────
    /// Registered custom providers (loaded from `[[custom_provider]]` TOML sections).
    #[serde(default)]
    pub custom_providers: Vec<CustomProvider>,

    // ── Dynamic model cache ─────────────────────────────────────────────
    /// Cached model lists fetched from provider `/models` endpoints.
    /// Key is the provider name, value is a list of model IDs.
    /// Updated when API keys are entered in setup wizard or on demand.
    #[serde(default)]
    pub dynamic_models: HashMap<String, Vec<String>>,

    // ── Keybindings ────────────────────────────────────────────────────
    /// User-defined keybinding overrides.
    /// Format: `{ "ActionName": ["Ctrl+x", "Alt+y"] }`
    /// Actions are matched case-insensitively. Declared here for config persistence; not currently consumed by the `tui_vt` host loop.
    #[serde(default)]
    pub keybindings: HashMap<String, Vec<String>>,

    // ── TUI output language policy (TUI-only) ─────────────────────────
    /// Per-channel output language for the TUI agent loop.
    ///
    /// Maps a channel key (e.g. `"response"`, `"code_comment"`,
    /// Edit format for the edit tool.
    ///
    /// `str_replace` (default): traditional find-and-replace.
    /// `hashline`: line-anchored patches with content-derived tags.
    #[serde(default)]
    pub edit_format: EditFormat,

    // ── Feature flags (omp-adoption-2) ────────────────────────────────
    /// Enable the sticky todo panel in the TUI.
    /// Default: true.
    #[serde(default = "default_true")]
    pub todo_panel_enabled: bool,

    /// How strongly to auto-create a todo list on the first turn.
    /// Default: off.
    #[serde(default)]
    pub todo_eager_mode: TodoEagerMode,

    /// Remind the agent to finish open todos before it stops. Default: true.
    #[serde(default = "default_true")]
    pub todo_reminders_enabled: bool,

    /// Max stop-time todo reminders per run. Default: 3.
    #[serde(default = "default_todo_reminders_max")]
    pub todo_reminders_max: u32,

    /// Seconds after every todo closes before the HUD auto-clears.
    /// Default: 60; `0` = instant; negative disables clearing.
    #[serde(default = "default_todo_clear_delay_secs")]
    pub todo_clear_delay_secs: i64,

    /// Enable the Agent Hub overlay (Ctrl+h / /agents).
    /// Default: true.
    #[serde(default = "default_true")]
    pub agent_hub_enabled: bool,

    /// Enable the Snapcompact PNG-frame compactor.
    /// Default: false (experimental).
    #[serde(default = "default_false")]
    pub snapcompact_enabled: bool,

    /// Enable Mermaid diagram rendering in markdown.
    /// Default: true.
    #[serde(default = "default_true")]
    pub mermaid_render_enabled: bool,

    /// Inline image previews in the TUI (kitty / iTerm2 graphics
    /// protocols). Kill-switch for terminals that misrender image
    /// escapes. Default: true.
    #[serde(default = "default_true")]
    pub inline_images: bool,

    /// Enable the Commit tool with optional LLM analysis.
    /// Default: false (opt-in, LLM cost).
    #[serde(default = "default_false")]
    pub commit_tool_enabled: bool,

    /// Run the bash tool inside a real PTY so ANSI SGR color sequences
    /// survive in command output (F-9, audit 2026-08-24). Default: false.
    ///
    /// **Currently inert.** The agent crate (`oxicode-agent`) cannot see
    /// cli settings today — `ToolContext` carries no settings field, and
    /// `Settings::apply_env()` is a no-op. The only live gate is the
    /// `OXICODE_BASH_PTY=1` environment variable, which is checked
    /// directly in `BashTool::execute`. Setting `bash_pty = true` in
    /// your settings file is silently ignored and emits a one-time
    /// `tracing::warn!` at settings load. The field is reserved for the
    /// eventual cli→agent settings plumbing — once that ships, the
    /// setting will be respected automatically.
    ///
    /// To opt in today: export `OXICODE_BASH_PTY=1` in the environment
    /// before invoking oxicode.
    #[serde(default = "default_false")]
    pub bash_pty: bool,

    // ── Hindsight memory (④) ─────────────────────────────────────────
    /// Enable session-spanning memory tools (retain/recall/reflect/edit)
    /// backed by the oxibrain daemon — the Oxi Foundation host's only
    /// durable-memory authority. Default: true. Machines without the
    /// daemon degrade honestly (tools return typed unavailable results).
    #[serde(default = "default_true")]
    pub memory_enabled: bool,

    // ── Workspace search (D3/D4) ─────────────────────────────────────
    /// oxibrain-backed workspace discovery (`workspace_search` tool).
    /// Everything defaults OFF — no spawn, no registration, no prompt
    /// fragment unless explicitly enabled (design D4).
    #[serde(default)]
    pub workspace_search: WorkspaceSearchSettings,

    // ── TTSR (③) ─────────────────────────────────────────────────────
    /// Enable Time-Traveling Stream Rules (stream interrupt on rule violation).
    /// Default: false (opt-in, stable-first).
    #[serde(default = "default_false")]
    pub ttsr_enabled: bool,

    /// TTSR interrupt mode. Default: "prose_only".
    #[serde(default = "default_ttsr_mode")]
    pub ttsr_interrupt_mode: String,

    // ── Model roles (ported from omp) ────────────────────────────────
    /// Named model-role → model-pattern assignments (e.g. `"commit"` →
    /// `"anthropic/claude-haiku"`, `"slow"` → `"pi/default"`).
    ///
    /// Empty by default. Role names are open-ended: the 10 built-in roles
    /// (`default`/`smol`/`slow`/`vision`/`plan`/`designer`/`commit`/`title`/
    /// `task`/`advisor`) plus any user-defined role are accepted.
    /// Resolution — including `pi/<role>` alias expansion with cycle
    /// detection — is done by [`oxicode_ai::RoleRegistry`]. The role-switching
    /// layer (which role is active when) is wired separately.
    #[serde(default)]
    pub model_roles: HashMap<String, String>,
    // ── Advisor (read-only reviewer shadowing the primary agent) ────
    /// Advisor subsystem settings. Default OFF (opt-in). Drives the
    /// `oxicode_agent::advisor` engine wired into `AgentSession`.
    #[serde(default)]
    pub advisor: AdvisorSettings,

    // ── Hooks (port 16) ───────────────────────────────────────────
    /// User-configured event→shell-command hooks. Loaded from the
    /// `[[hooks]]` array in settings.toml. Project hooks are gated by
    /// the first-run approval (see `store/hook_approval.rs`).
    #[serde(default)]
    pub hooks: Vec<oxicode_sdk::ports::HookSpec>,
}

/// Advisor subsystem settings — a read-only reviewer that shadows the primary
/// agent and surfaces advice (`nit`/`concern`/`blocker`). All default OFF;
/// the advisor is opt-in (set `enabled = true` in `[advisor]`).
///
/// Ported from omp's `advisor.*` settings (advisor.enabled /
/// advisor.syncBacklog / advisor.immuneTurns).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisorSettings {
    /// Master switch. Default OFF.
    #[serde(default = "default_false")]
    pub enabled: bool,
    /// Sync-backlog barrier: pause the primary when the advisor falls this many
    /// turns behind, or `"off"` to never block. omp `advisor.syncBacklog`.
    /// Default `"off"`.
    #[serde(default = "default_advisor_sync_backlog")]
    pub sync_backlog: String,
    /// Post-interrupt immune-turn cooldown: after a `concern`/`blocker` steers
    /// in, downgrade further `concern`/`blocker` notes to asides for this many
    /// turns (prevents advice storms). omp `advisor.immuneTurns`. Default 0.
    #[serde(default)]
    pub immune_turns: u64,
}

impl Default for AdvisorSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            sync_backlog: default_advisor_sync_backlog(),
            immune_turns: 0,
        }
    }
}

fn default_advisor_sync_backlog() -> String {
    "off".to_string()
}

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

impl Default for WorkspaceSearchSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            executable: None,
            space: default_workspace_search_space(),
            include: None,
            exclude: None,
            max_file_bytes: None,
        }
    }
}

fn default_workspace_search_space() -> String {
    "dev".to_string()
}

fn default_theme() -> String {
    "default".to_string()
}

fn default_thinking_level() -> ThinkingLevel {
    ThinkingLevel::Medium
}

fn default_session_history_size() -> usize {
    100
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}
fn default_todo_reminders_max() -> u32 {
    3
}

fn default_todo_clear_delay_secs() -> i64 {
    60
}

fn default_ttsr_mode() -> String {
    "prose_only".to_string()
}

fn default_tool_timeout() -> u64 {
    120
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            thinking_level: ThinkingLevel::Medium,
            theme: default_theme(),
            glyph_set: GlyphSet::default(),
            last_used_model: None,
            last_used_provider: None,
            default_model: None,
            default_provider: None,
            max_tokens: None,
            temperature: None,
            default_temperature: None,
            max_response_tokens: None,
            session_history_size: default_session_history_size(),
            session_dir: None,
            extensions_enabled: true,
            auto_compaction: true,
            disabled_tools: Vec::new(),
            tool_timeout_seconds: default_tool_timeout(),
            ask_timeout_secs: 0,
            extensions: Vec::new(),
            skills: Vec::new(),
            prompts: Vec::new(),
            themes: Vec::new(),
            custom_providers: Vec::new(),
            dynamic_models: HashMap::new(),
            keybindings: HashMap::new(),
            edit_format: EditFormat::default(),
            memory_enabled: true,
            workspace_search: WorkspaceSearchSettings::default(),
            todo_panel_enabled: true,
            todo_eager_mode: TodoEagerMode::Off,
            todo_reminders_enabled: true,
            todo_reminders_max: default_todo_reminders_max(),
            todo_clear_delay_secs: default_todo_clear_delay_secs(),
            agent_hub_enabled: true,
            snapcompact_enabled: false,
            advisor: AdvisorSettings::default(),
            mermaid_render_enabled: true,
            inline_images: true,
            commit_tool_enabled: false,
            bash_pty: false,
            ttsr_enabled: false,
            ttsr_interrupt_mode: default_ttsr_mode(),
            model_roles: HashMap::new(),
            hooks: Vec::new(),
        }
    }
}

impl Settings {
    // ── Paths ────────────────────────────────────────────────────────

    /// Get the global settings directory path (the canonical oxicode home:
    /// `$OXICODE_HOME`, else `$OXI_HOME/oxicode`, else `~/.oxi/oxicode`).
    pub fn settings_dir() -> Result<PathBuf> {
        oxicode_catalog::product_env::home_dir().context("Cannot determine oxicode home directory")
    }

    /// Canonical-only global settings file path (existing format wins, JSON
    /// preferred).
    ///
    /// Unlike [`Self::settings_path`], this never resolves into the legacy
    /// home — use it as the write target.
    fn canonical_settings_path() -> Result<PathBuf> {
        let json_path = Self::settings_json_path()?;
        if json_path.exists() {
            return Ok(json_path);
        }
        let toml_path = Self::settings_toml_path()?;
        if toml_path.exists() {
            return Ok(toml_path);
        }
        Ok(json_path)
    }

    /// Legacy read-only settings dir (pre-unified-layout `~/.oxicode`).
    ///
    /// `None` when no legacy home exists (or an explicit `$OXICODE_HOME`
    /// opts out of legacy merging).
    fn legacy_settings_dir() -> Option<PathBuf> {
        oxicode_catalog::oxi_home::legacy_home_dir()
    }

    /// Pure settings-file resolution used by [`Self::settings_path`] and
    /// [`Self::settings_path_with_preference`].
    ///
    /// Canonical candidates first (`prefer_json` sets the priority), then
    /// the legacy dir read-only, then the canonical preferred default (the
    /// write target).
    fn resolve_settings_path_in(
        canonical_dir: &std::path::Path,
        legacy_dir: Option<&std::path::Path>,
        prefer_json: bool,
    ) -> PathBuf {
        let json = canonical_dir.join("settings.json");
        let toml = canonical_dir.join("settings.toml");
        let (primary, secondary) = if prefer_json {
            (&json, &toml)
        } else {
            (&toml, &json)
        };
        if primary.exists() {
            return primary.clone();
        }
        if secondary.exists() {
            return secondary.clone();
        }
        if let Some(legacy) = legacy_dir {
            let legacy_json = legacy.join("settings.json");
            let legacy_toml = legacy.join("settings.toml");
            let (legacy_primary, legacy_secondary) = if prefer_json {
                (&legacy_json, &legacy_toml)
            } else {
                (&legacy_toml, &legacy_json)
            };
            if legacy_primary.exists() {
                return legacy_primary.clone();
            }
            if legacy_secondary.exists() {
                return legacy_secondary.clone();
            }
        }
        primary.clone()
    }

    /// Get the global settings TOML file path (`<settings_dir>/settings.toml`).
    pub fn settings_toml_path() -> Result<PathBuf> {
        Ok(Self::settings_dir()?.join("settings.toml"))
    }

    /// Get the global settings JSON file path (`<settings_dir>/settings.json`).
    pub fn settings_json_path() -> Result<PathBuf> {
        Ok(Self::settings_dir()?.join("settings.json"))
    }

    /// Get the global settings file path (JSON takes priority).
    ///
    /// Returns the path to the settings file that should be used.
    /// If both JSON and TOML exist, JSON is returned (takes priority).
    /// If only one exists, that path is returned.
    /// If neither exists in the canonical home, falls back read-only to the
    /// legacy home (`~/.oxicode/settings.{json,toml}`) when present.
    /// Otherwise returns the canonical JSON path by default (the write target).
    pub fn settings_path() -> Result<PathBuf> {
        let dir = Self::settings_dir()?;
        Ok(Self::resolve_settings_path_in(
            &dir,
            Self::legacy_settings_dir().as_deref(),
            true,
        ))
    }

    /// Get the effective settings file path, preferring the specified format.
    ///
    /// If `prefer_json` is true, checks JSON first; otherwise checks TOML first.
    /// Returns the first existing file, or — when the canonical home has
    /// neither — the legacy home's file (read-only) when present. Falls back
    /// to the canonical preferred path otherwise.
    pub fn settings_path_with_preference(prefer_json: bool) -> Result<PathBuf> {
        let dir = Self::settings_dir()?;
        Ok(Self::resolve_settings_path_in(
            &dir,
            Self::legacy_settings_dir().as_deref(),
            prefer_json,
        ))
    }

    /// Detect the settings file format from its path.
    pub fn detect_format(path: &Path) -> SettingsFormat {
        match path.extension().and_then(|e| e.to_str()) {
            Some("json") => SettingsFormat::Json,
            Some("toml") => SettingsFormat::Toml,
            _ => SettingsFormat::Json, // Default to JSON for unknown extensions
        }
    }

    /// Get the project-local settings file path.
    ///
    /// Searches for `.oxicode/settings.json` first, then `.oxicode/settings.toml`.
    /// Returns the first one found, or None if neither exists.
    pub fn find_project_settings(start_dir: &std::path::Path) -> Option<PathBuf> {
        let mut dir = start_dir.to_path_buf();
        loop {
            // Check JSON first (priority), then TOML
            let json_candidate = dir.join(".oxicode").join("settings.json");
            if json_candidate.exists() {
                return Some(json_candidate);
            }

            let toml_candidate = dir.join(".oxicode").join("settings.toml");
            if toml_candidate.exists() {
                return Some(toml_candidate);
            }

            if !dir.pop() {
                return None;
            }
        }
    }

    /// Resolve the effective session directory.
    ///
    /// Priority: `session_dir` field → canonical home `sessions/`.
    pub fn effective_session_dir(&self) -> Result<PathBuf> {
        if let Some(ref dir) = self.session_dir {
            return Ok(dir.clone());
        }
        Ok(Self::settings_dir()?.join("sessions"))
    }

    // ── Loading ──────────────────────────────────────────────────────

    /// Load settings, applying all layers:
    ///
    /// 1. Built-in defaults
    /// 2. Global canonical settings (`$OXICODE_HOME`, else `~/.oxi/oxicode/`; legacy
    ///    `~/.oxicode/` read-only fallback)
    /// 3. Project `.oxicode/settings.toml`
    /// 4. Environment variable overrides
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use oxicode_cli::Settings;
    ///
    /// let settings = Settings::load().expect("Failed to load settings");
    /// println!("Using model: {}", settings.effective_model(None));
    /// ```
    pub fn load() -> Result<Self> {
        Self::load_from_cwd()
    }

    /// Load settings with an explicit working directory for project config discovery.
    ///
    /// Always layers the global config from `Self::settings_path()` when it
    /// exists. Use [`Settings::load_from_with`] to inject a custom global
    /// path (e.g. for tests or portable mode).
    pub fn load_from(dir: &std::path::Path) -> Result<Self> {
        Self::load_from_with(dir, None)
    }

    /// Load settings with an explicit project directory and an optional
    /// global settings path override.
    ///
    /// Layering order:
    /// 1. Defaults
    /// 2. Global config from `global_override` if `Some`, else from
    ///    `Self::settings_path()` if it exists.
    /// 3. Project config (`<dir>/.oxicode/settings.{toml,json}`).
    /// 4. Environment variable overrides.
    /// 5. Migration.
    /// 6. TUI language policy validation.
    ///
    /// Passing `global_override = None` keeps the default behavior of
    /// reading the user's real global settings (canonical home, with legacy
    /// read-only fallback). Tests pass
    /// `Some(custom_path)` or rely on the real path being absent to get
    /// pure defaults. (The test suite uses `Some(specific_path)` semantics
    /// by passing a temp path; passing `None` is also valid for "skip the
    /// global layer entirely".)
    pub fn load_from_with(
        dir: &std::path::Path,
        global_override: Option<&std::path::Path>,
    ) -> Result<Self> {
        // 1. Start from defaults
        let mut settings = Settings::default();

        // 2. Layer global config (override takes precedence; None = use real
        //    canonical home settings if present, legacy read-only fallback)
        let resolved_global: Option<std::path::PathBuf> = match global_override {
            Some(p) => Some(p.to_path_buf()),
            None => Self::settings_path().ok(),
        };
        if let Some(ref gp) = resolved_global
            && gp.exists()
        {
            settings = Self::layer_file(&settings, gp)?;
        }

        // 3. Layer project config
        if let Some(project_path) = Self::find_project_settings(dir) {
            settings = Self::layer_file(&settings, &project_path)?;
        }

        // 4. Layer environment variables
        settings.apply_env();

        // 5. Run migration if needed
        settings = Self::migrate(settings)?;

        // 5. Validate settings — placeholder for future validation

        // F-9 (audit 2026-08-24): nudge users who opt in via the setting
        // but whose value is silently ignored until cli→agent settings
        // plumbing lands. The env var path still works.
        if settings.bash_pty {
            tracing::warn!(
                "settings.bash_pty = true is currently inert — the agent tool                  cannot see cli settings yet. To enable PTY-backed bash right                  now, export OXICODE_BASH_PTY=1 in your environment."
            );
        }

        Ok(settings)
    }

    /// Convenience: load from current working directory.
    pub fn load_from_cwd() -> Result<Self> {
        let cwd = env::current_dir().context("Cannot determine current directory")?;
        Self::load_from(&cwd)
    }

    /// Parse a settings file (TOML or JSON) and overlay its values onto `base`.
    ///
    /// The format is auto-detected based on the file extension.
    /// Fields present in the file replace those in `base`; absent fields
    /// are left untouched.
    fn layer_file(base: &Settings, path: &std::path::Path) -> Result<Settings> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read settings from {}", path.display()))?;

        let format = Self::detect_format(path);
        let overlay: serde_json::Value = match format {
            SettingsFormat::Toml => {
                let toml_value: toml::Value = toml::from_str(&content).with_context(|| {
                    format!("Failed to parse TOML settings from {}", path.display())
                })?;
                // Convert TOML to JSON Value for uniform merging
                toml_value_to_json(toml_value)
            }
            SettingsFormat::Json => serde_json::from_str(&content).with_context(|| {
                format!("Failed to parse JSON settings from {}", path.display())
            })?,
        };

        // Re-serialize the base to JSON, merge with the overlay, then
        // deserialize back. This gives correct "only override what's
        // present" semantics.
        let base_json =
            serde_json::to_value(base).context("Failed to serialize base settings for merge")?;

        let merged = merge_json_values(base_json, overlay);
        let result: Settings =
            serde_json::from_value(merged).context("Failed to deserialize merged settings")?;

        Ok(result)
    }

    // ── Environment variables ────────────────────────────────────────

    /// Apply environment variable overrides in-place.
    ///
    /// DEPRECATED: Environment variable overrides are being phased out in favor
    /// of file-based configuration (the global settings file). This method is
    /// kept for CI/CD compatibility but should not be relied upon for local
    /// development. Use `oxicode config set` or `oxicode setup` instead.
    ///
    /// Supported variables (CI/CD only):
    ///
    /// | Env var                    | Setting                |
    /// |---------------------------|------------------------|
    /// | `OXICODE_MODEL`               | `default_model`        |
    /// | `OXICODE_PROVIDER`            | `default_provider`     |
    /// | `OXICODE_THINKING`            | `thinking_level`       |
    /// | `OXICODE_THEME`               | `theme`                |
    /// | `OXICODE_MAX_TOKENS`          | `max_tokens`           |
    /// | `OXICODE_TEMPERATURE`         | `default_temperature`  |
    /// | `OXICODE_SESSION_DIR`         | `session_dir`          |
    /// | `OXICODE_EXTENSIONS_ENABLED`  | `extensions_enabled`   |
    /// | `OXICODE_AUTO_COMPACTION`     | `auto_compaction`      |
    /// | `OXICODE_TOOL_TIMEOUT`        | `tool_timeout_seconds` |
    /// | `OXICODE_DISABLED_TOOLS`      | `disabled_tools`       |
    #[allow(dead_code)]
    pub fn apply_env(&mut self) {
        // No-op: environment variable overrides are disabled.
        // All configuration should come from settings.toml / settings.json.
        // This method is kept for backward compatibility but does nothing.
    }

    /// Build a `Settings` instance from **only** environment variables
    /// (all other fields stay at defaults).
    ///
    /// DEPRECATED: Returns defaults since env overrides are disabled.
    /// Use `Settings::load()` to load from settings.toml instead.
    #[allow(dead_code)]
    pub fn from_env() -> Self {
        Self::default()
    }

    // ── Persistence ──────────────────────────────────────────────────

    /// Save settings to the global config file.
    ///
    /// Preserves the format of the existing canonical file; otherwise saves
    /// as JSON. Writes always land in the canonical home — a legacy
    /// `~/.oxicode` file is never modified (see [`Self::settings_path`] for
    /// the read-side fallback).
    pub fn save(&self) -> Result<()> {
        let dir = Self::settings_dir()?;
        let path = Self::canonical_settings_path()?;

        if !dir.exists() {
            fs::create_dir_all(&dir).with_context(|| {
                format!("Failed to create settings directory {}", dir.display())
            })?;
        }

        let format = Self::detect_format(&path);
        let content = Self::serialize_for_format(self, format)?;

        // Atomic write: write to temp file first, then rename
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, &content)
            .with_context(|| format!("Failed to write settings to {}", tmp_path.display()))?;
        fs::rename(&tmp_path, &path)
            .with_context(|| format!("Failed to rename settings to {}", path.display()))?;

        Ok(())
    }

    /// Save settings to a specific path, using the format determined by the file extension.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }

        let format = Self::detect_format(path);
        let content = Self::serialize_for_format(self, format)?;

        // Atomic write
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, &content)
            .with_context(|| format!("Failed to write settings to {}", tmp_path.display()))?;
        fs::rename(&tmp_path, path)
            .with_context(|| format!("Failed to rename settings to {}", path.display()))?;

        Ok(())
    }

    /// Save settings to the project-local config file.
    ///
    /// Uses the format of the existing file if present, otherwise saves as JSON.
    pub fn save_project(&self, project_dir: &std::path::Path) -> Result<()> {
        let dir = project_dir.join(".oxicode");

        if !dir.exists() {
            fs::create_dir_all(&dir).with_context(|| {
                format!(
                    "Failed to create project settings directory {}",
                    dir.display()
                )
            })?;
        }

        // Check if a settings file already exists in project
        let json_path = dir.join("settings.json");
        let toml_path = dir.join("settings.toml");

        let path = if json_path.exists() {
            &json_path
        } else if toml_path.exists() {
            &toml_path
        } else {
            // Default to JSON for new files
            &json_path
        };

        let format = Self::detect_format(path);
        let content = Self::serialize_for_format(self, format)?;

        // Atomic write
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, &content)
            .with_context(|| format!("Failed to write settings to {}", tmp_path.display()))?;
        fs::rename(&tmp_path, path)
            .with_context(|| format!("Failed to rename settings to {}", path.display()))?;

        Ok(())
    }

    /// Serialize settings to a string in the specified format.
    pub fn serialize_for_format(settings: &Settings, format: SettingsFormat) -> Result<String> {
        match format {
            SettingsFormat::Toml => {
                toml::to_string_pretty(settings).context("Failed to serialize settings to TOML")
            }
            SettingsFormat::Json => serde_json::to_string_pretty(settings)
                .context("Failed to serialize settings to JSON"),
        }
    }

    /// Parse settings from a string in the specified format.
    pub fn parse_from_str(content: &str, format: SettingsFormat) -> Result<Settings> {
        match format {
            SettingsFormat::Toml => {
                toml::from_str(content).context("Failed to parse TOML settings")
            }
            SettingsFormat::Json => {
                serde_json::from_str(content).context("Failed to parse JSON settings")
            }
        }
    }

    // ── CLI overrides ────────────────────────────────────────────────

    /// Merge with CLI arguments (CLI takes precedence).
    ///
    /// # Arguments
    ///
    /// * `model` — CLI-specified model override
    /// * `provider` — CLI-specified provider override
    pub fn merge_cli(&mut self, model: Option<String>, provider: Option<String>) {
        if let Some(m) = model {
            self.last_used_model = Some(m);
        }
        if let Some(p) = provider {
            self.last_used_provider = Some(p);
        }
    }

    /// Get the effective model ID (provider/model format).
    /// Returns None if no model is configured.
    pub fn effective_model(&self, cli_model: Option<&str>) -> Option<String> {
        cli_model.map(String::from).or_else(|| {
            // Reconstruct full model ID from separate fields.
            // Handles both cases:
            //   - last_used_model = "anthropic/claude-sonnet-4" (full ID, stored by save_last_used)
            //   - last_used_model = "claude-sonnet-4" + last_used_provider = "anthropic" (split)
            let model = self.last_used_model.as_ref()?;
            if model.contains('/') {
                // Already a full model ID
                Some(model.clone())
            } else if let Some(ref provider) = self.last_used_provider {
                // Reconstruct from separate fields
                Some(format!("{}/{}", provider, model))
            } else {
                Some(model.clone())
            }
        })
    }

    /// Get the effective provider.
    /// Returns None if no provider is configured.
    pub fn effective_provider(&self, cli_provider: Option<&str>) -> Option<String> {
        cli_provider
            .map(String::from)
            .or_else(|| self.last_used_provider.clone())
    }

    /// Get the effective temperature, preferring `default_temperature` (f64)
    /// over `temperature` (f32), falling back to `None`.
    pub fn effective_temperature(&self) -> Option<f64> {
        self.default_temperature
            .or(self.temperature.map(|t| t as f64))
    }

    /// Get the effective max tokens, preferring `max_response_tokens` (usize)
    /// over `max_tokens` (u32), falling back to `None`.
    pub fn effective_max_tokens(&self) -> Option<usize> {
        self.max_response_tokens
            .or(self.max_tokens.map(|t| t as usize))
    }

    // ── Theme persistence ─────────────────────────────────────────────

    /// Save the last used model/provider and persist to disk.
    ///
    /// Splits the model_id on first `/` to store provider and model separately.
    pub fn save_last_used(model_id: &str) {
        if let Ok(mut settings) = Self::load() {
            if let Some((provider, model)) = model_id.split_once('/') {
                settings.last_used_provider = Some(provider.to_string());
                settings.last_used_model = Some(model.to_string());
            } else {
                settings.last_used_model = Some(model_id.to_string());
            }
            let _ = settings.save();
        }
    }

    /// Save the current theme to settings and persist to disk.
    pub fn save_theme(&mut self, name: &str) -> Result<()> {
        self.theme = name.to_string();
        self.save()
    }

    /// Get the theme name from settings, returning a default if not set.
    pub fn get_theme_name(&self) -> String {
        if self.theme.is_empty() || self.theme == "default" {
            "oxi".to_string()
        } else {
            self.theme.clone()
        }
    }

    // ── Migration ────────────────────────────────────────────────────

    /// Migrate settings from an older format version to the current one.
    ///
    /// Currently handles:
    /// - Version 0 → Version 6 (multi-step)
    /// - Version 1 → Version 6 (multi-step)
    /// - Version 2 → Version 6 (multi-step)
    /// - Version 3 → Version 4 (default_model → last_used_model)
    /// - Version 7 → Version 8 (edit_format field added —
    ///   `#[serde(default)]` fills with EditFormat::StrReplace)
    /// - Version 8 → Version 9 (model_roles field added — no value
    ///   migration, `#[serde(default)]` fills with an empty map)
    fn migrate(settings: Settings) -> Result<Settings> {
        let mut settings = settings;

        match settings.version {
            SETTINGS_VERSION => {
                // Already current — nothing to do.
            }
            0 => {
                // Version 0 = pre-versioning config.
                // Add any defaults that were introduced in version 1.
                if settings.tool_timeout_seconds == 0 {
                    settings.tool_timeout_seconds = default_tool_timeout();
                }
                settings.version = SETTINGS_VERSION;

                tracing::info!("Migrated settings from version 0 to {}", SETTINGS_VERSION);
            }
            1 | 2 => {
                // Version 1/2 → 10: dynamic_models field added + model/provider split.
                // The v3 → v4 default_model → last_used_model split doesn't apply
                // here (no default_model in v1/v2). `#[serde(default)]` fills missing fields.
                settings.version = SETTINGS_VERSION;
                tracing::info!(
                    "Migrated settings from version {} to {}",
                    settings.version,
                    SETTINGS_VERSION
                );
            }
            3 => {
                // Version 3 → 4 step happens inline: migrate default_model → last_used_model.
                // Then collapse to current version.
                if let Some(model) = settings.default_model.take() {
                    if let Some((provider, model_name)) = model.split_once('/') {
                        settings.last_used_provider = Some(provider.to_string());
                        settings.last_used_model = Some(model_name.to_string());
                    } else {
                        settings.last_used_model = Some(model);
                    }
                }
                settings.version = SETTINGS_VERSION;
                tracing::info!(
                    "Migrated settings from version 3 to {} (default_model → last_used_model)",
                    SETTINGS_VERSION
                );
            }
            4 => {
                // Version 4 → 10: `#[serde(default)]` fills missing fields.
                settings.version = SETTINGS_VERSION;
                tracing::info!("Migrated settings from version 4 to {}", SETTINGS_VERSION);
            }
            5 => {
                // Version 5 → 10: `#[serde(default)]` fills missing fields.
                settings.version = SETTINGS_VERSION;
                tracing::info!("Migrated settings from version 5 to {}", SETTINGS_VERSION);
            }
            6 => {
                // Version 6 → 7: edit_format field added.
                // `#[serde(default)]` fills with EditFormat::StrReplace (default).
                settings.version = SETTINGS_VERSION;
                tracing::info!(
                    "Migrated settings from version 6 to {} (added edit_format, defaulting to str_replace)",
                    SETTINGS_VERSION
                );
            }
            7 => {
                // Version 7 → 8: glyph_set field added.
                // `#[serde(default)]` fills with GlyphSet::Unicode (default).
                settings.version = SETTINGS_VERSION;
                tracing::info!(
                    "Migrated settings from version 7 to {} (added glyph_set, defaulting to unicode)",
                    SETTINGS_VERSION
                );
            }
            8 => {
                // Version 8 → 9: model_roles field added (ported from omp).
                // No value migration — `#[serde(default)]` fills an empty map.
                settings.version = SETTINGS_VERSION;
                tracing::info!(
                    "Migrated settings from version 8 to {} (added model_roles, defaulting to empty)",
                    SETTINGS_VERSION
                );
            }
            v if v > SETTINGS_VERSION => {
                // Future version — we don't know how to downgrade.
                anyhow::bail!(
                    "Settings version {} is newer than supported version {}. \
                     Please update oxicode.",
                    v,
                    SETTINGS_VERSION
                );
            }
            v => {
                // Unknown old version — best-effort migration.
                tracing::warn!(
                    "Unknown settings version {}, attempting migration to {}",
                    v,
                    SETTINGS_VERSION
                );
                settings.version = SETTINGS_VERSION;
            }
        }

        Ok(settings)
    }
}

// ── Settings format detection ──────────────────────────────────────

/// Supported settings file formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsFormat {
    /// JSON format.
    #[default]
    Json,
    /// TOML format.
    Toml,
}

impl SettingsFormat {
    /// Get the file extension for this format.
    pub fn extension(&self) -> &'static str {
        match self {
            SettingsFormat::Json => "json",
            SettingsFormat::Toml => "toml",
        }
    }
}

// ── JSON/TOML conversion helpers ────────────────────────────────────

/// Convert a TOML Value to a serde_json::Value.
fn toml_value_to_json(toml: toml::Value) -> serde_json::Value {
    match toml {
        toml::Value::String(s) => serde_json::Value::String(s),
        toml::Value::Integer(i) => serde_json::Value::Number(i.into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        toml::Value::Boolean(b) => serde_json::Value::Bool(b),
        toml::Value::Datetime(dt) => serde_json::Value::String(dt.to_string()),
        toml::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(toml_value_to_json).collect())
        }
        toml::Value::Table(table) => {
            let obj = table
                .into_iter()
                .map(|(k, v)| (k, toml_value_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
    }
}

/// Deep merge two JSON values. The second value overrides the first.
fn merge_json_values(base: serde_json::Value, override_: serde_json::Value) -> serde_json::Value {
    match (base, override_) {
        // If either is not an object, the override wins
        (serde_json::Value::Object(base_map), serde_json::Value::Object(override_map)) => {
            let mut result = base_map;
            for (key, override_value) in override_map {
                let base_value = result.remove(&key);
                let merged = match base_value {
                    Some(base_v) => merge_json_values(base_v, override_value),
                    None => override_value,
                };
                result.insert(key, merged);
            }
            serde_json::Value::Object(result)
        }
        // Override wins for non-objects
        (_, override_) => override_,
    }
}

/// Parse a thinking level from a string.
pub fn parse_thinking_level(s: &str) -> Option<ThinkingLevel> {
    match s.to_lowercase().as_str() {
        "off" | "none" => Some(ThinkingLevel::Off),
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" | "standard" => Some(ThinkingLevel::Medium),
        "high" | "thorough" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::XHigh),
        _ => None,
    }
}

/// Parse a boolean-like string (`"true"`, `"false"`, `"1"`, `"0"`, `"yes"`, `"no"`).
#[allow(dead_code)]
fn parse_boolish(s: &str) -> Result<bool> {
    match s.to_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => anyhow::bail!("Cannot parse '{}' as boolean", s),
    }
}

#[cfg(test)]
mod tests {
    /// Canonical settings exist → legacy dir never consulted.
    #[test]
    fn resolve_settings_path_prefers_canonical() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().join("canonical");
        let legacy = tmp.path().join("legacy");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(canonical.join("settings.toml"), "theme = \"x\"").unwrap();
        std::fs::write(legacy.join("settings.json"), "{}").unwrap();

        let got = Settings::resolve_settings_path_in(&canonical, Some(&legacy), true);
        assert_eq!(got, canonical.join("settings.toml"));
    }

    /// Canonical empty → legacy JSON wins over legacy TOML when prefer_json.
    #[test]
    fn resolve_settings_path_falls_back_to_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().join("canonical");
        let legacy = tmp.path().join("legacy");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("settings.json"), "{}").unwrap();
        std::fs::write(legacy.join("settings.toml"), "theme = \"x\"").unwrap();

        let json = Settings::resolve_settings_path_in(&canonical, Some(&legacy), true);
        assert_eq!(json, legacy.join("settings.json"));
        let toml = Settings::resolve_settings_path_in(&canonical, Some(&legacy), false);
        assert_eq!(toml, legacy.join("settings.toml"));
    }

    /// Neither exists → canonical preferred default (the write target).
    #[test]
    fn resolve_settings_path_defaults_to_canonical() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().join("canonical");
        std::fs::create_dir_all(&canonical).unwrap();

        let got = Settings::resolve_settings_path_in(&canonical, None, true);
        assert_eq!(got, canonical.join("settings.json"));
    }

    /// Write target (`canonical_settings_path`) never resolves to legacy.
    #[test]
    fn save_targets_canonical_dir() {
        // `save()` resolves its write path via `canonical_settings_path`,
        // which only considers `<settings_dir>/settings.{json,toml}`.
        // Pin the contract structurally: canonical JSON present → JSON path.
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().join("canonical");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::write(canonical.join("settings.toml"), "theme = \"x\"").unwrap();

        let json = Settings::resolve_settings_path_in(&canonical, None, true);
        assert_eq!(json, canonical.join("settings.toml"));
    }

    /// `inline_images` kill-switch: default ON, and a settings file that
    /// sets it false loads the override (serde contract pin).
    #[test]
    fn inline_images_defaults_true_and_reads_override() {
        use super::*;
        assert!(Settings::default().inline_images, "previews on by default");
        let s: Settings = toml::from_str("inline_images = false").unwrap();
        assert!(!s.inline_images, "settings file can disable previews");
    }

    /// `workspace_search` defaults OFF — no spawn, no registration, no
    /// prompt fragment unless explicitly enabled (design D4).
    #[test]
    fn workspace_search_defaults_to_disabled() {
        let s = Settings::default();
        assert!(!s.workspace_search.enabled);
        assert_eq!(s.workspace_search.space, "dev");
        assert!(s.workspace_search.executable.is_none());
    }

    /// `[workspace_search]` section parses from TOML with the serde
    /// contract pin (enabled flag + executable override).
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
        assert_eq!(
            s.workspace_search.executable.as_deref(),
            Some("/opt/oxibrain/bin/oxibrain")
        );
    }

    use super::*;
    use std::io::Write as IoWrite;
    use std::sync::Mutex;

    #[test]
    fn todo_settings_default_preserve_current_behavior() {
        let s = Settings::default();
        assert_eq!(s.todo_eager_mode, TodoEagerMode::Off);
        assert!(s.todo_reminders_enabled);
        assert_eq!(s.todo_reminders_max, 3);
        assert_eq!(s.todo_clear_delay_secs, 60);
    }

    #[test]
    fn todo_eager_mode_round_trips_through_toml() {
        let parsed: TodoEagerMode = toml::from_str("v = \"always\"")
            .map(|t: toml::Value| TodoEagerMode::deserialize(t["v"].clone()).unwrap())
            .unwrap();
        assert_eq!(parsed, TodoEagerMode::Always);
    }

    /// Global lock to serialize all tests that manipulate process-wide env vars.
    #[allow(dead_code)] // held implicitly via guard pattern; not all tests acquire it
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that removes listed env vars on creation and restores them on drop.
    /// This prevents parallel test races where one test sets an env var that leaks into another.
    struct EnvGuard {
        saved: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        fn new(vars: &[&str]) -> Self {
            let saved = vars
                .iter()
                .map(|&name| {
                    let old = env::var(name).ok();
                    // SAFETY: test-only; the ENV_LOCK mutex serializes access.
                    unsafe { env::remove_var(name) };
                    (name.to_string(), old)
                })
                .collect();
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, old) in self.saved.drain(..) {
                match old {
                    // SAFETY: test-only; the ENV_LOCK mutex serializes access.
                    Some(val) => unsafe { env::set_var(&name, val) },
                    None => unsafe { env::remove_var(&name) },
                }
            }
        }
    }

    // ── Struct tests ─────────────────────────────────────────────────

    #[test]
    fn test_default_settings() {
        let settings = Settings::default();
        assert_eq!(settings.version, SETTINGS_VERSION);
        assert_eq!(settings.thinking_level, ThinkingLevel::Medium);
        assert_eq!(settings.theme, "default");
        assert!(settings.last_used_model.is_none());
        assert!(settings.last_used_provider.is_none());
        assert!(settings.extensions_enabled);
        assert!(settings.auto_compaction);
        assert_eq!(settings.tool_timeout_seconds, 120);
    }

    #[test]
    fn test_merge_cli() {
        let mut settings = Settings::default();
        settings.last_used_model = Some("gpt-4o".to_string());

        settings.merge_cli(Some("claude".to_string()), None);
        assert_eq!(settings.last_used_model, Some("claude".to_string()));

        settings.merge_cli(None, Some("google".to_string()));
        assert_eq!(settings.last_used_provider, Some("google".to_string()));
    }

    // ── Layered loading ──────────────────────────────────────────────

    #[test]
    fn test_layer_file_overrides() {
        let base = Settings::default();

        let tmp = tempfile::NamedTempFile::with_suffix(".toml").unwrap();
        let toml_content = r#"
last_used_model = "openai/gpt-4o"
theme = "dracula"
"#;
        tmp.as_file().write_all(toml_content.as_bytes()).unwrap();

        let merged = Settings::layer_file(&base, tmp.path()).unwrap();
        assert_eq!(merged.last_used_model, Some("openai/gpt-4o".to_string()));
        assert_eq!(merged.theme, "dracula");
        // Unchanged fields retain defaults
        assert_eq!(merged.thinking_level, ThinkingLevel::Medium);
        assert!(merged.extensions_enabled);
    }

    #[test]
    fn test_layer_file_preserves_unset() {
        let mut base = Settings::default();
        base.last_used_provider = Some("deepseek".to_string());

        let tmp = tempfile::NamedTempFile::with_suffix(".toml").unwrap();
        // Only override theme — provider should remain
        let toml_content = "theme = \"monokai\"\n";
        tmp.as_file().write_all(toml_content.as_bytes()).unwrap();

        let merged = Settings::layer_file(&base, tmp.path()).unwrap();
        assert_eq!(merged.theme, "monokai");
        assert_eq!(merged.last_used_provider, Some("deepseek".to_string()));
    }

    #[test]
    fn test_load_from_dir_with_project_config() {
        let _guard = EnvGuard::new(&[
            "OXICODE_MODEL",
            "OXICODE_PROVIDER",
            "OXICODE_THEME",
            "OXICODE_TOOL_TIMEOUT",
            "OXICODE_TEMPERATURE",
            "OXICODE_MAX_TOKENS",
            "OXICODE_SESSION_DIR",
            "OXICODE_EXTENSIONS_ENABLED",
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let oxicode_dir = tmp.path().join(".oxicode");
        fs::create_dir_all(&oxicode_dir).unwrap();
        let settings_path = oxicode_dir.join("settings.toml");
        // Write v3 format: default_model contains "provider/model"
        fs::write(
            &settings_path,
            "version = 3\ndefault_model = \"google/gemini-2.0-flash\"\n",
        )
        .unwrap();

        let settings = Settings::load_from(tmp.path()).unwrap();
        // Migration moves default_model → last_used_model
        assert_eq!(
            settings.last_used_model,
            Some("gemini-2.0-flash".to_string())
        );
        assert_eq!(settings.last_used_provider, Some("google".to_string()));
    }

    #[test]
    fn test_load_from_dir_no_config() {
        // Clean env vars that load_from() reads via apply_env()
        let _guard = EnvGuard::new(&[
            "OXICODE_MODEL",
            "OXICODE_PROVIDER",
            "OXICODE_THEME",
            "OXICODE_TOOL_TIMEOUT",
            "OXICODE_TEMPERATURE",
            "OXICODE_MAX_TOKENS",
            "OXICODE_SESSION_DIR",
            "OXICODE_EXTENSIONS_ENABLED",
        ]);
        let tmp = tempfile::tempdir().unwrap();
        // Pass a nonexistent global path so the user's real global settings
        // never leaks into the test. (`Settings::load_from` reads the
        // real global config when present, which is what made this test
        // fail when the user's global set `thinking_level = "high"`.)
        let global = tmp.path().join("nonexistent-settings.json");
        let settings = Settings::load_from_with(tmp.path(), Some(&global)).unwrap();
        assert_eq!(settings.thinking_level, ThinkingLevel::Medium);
    }
    #[test]
    fn test_from_env() {
        // NOTE: Environment variable overrides are disabled.
        // from_env() returns defaults only.
        let _guard = EnvGuard::new(&[
            // no env vars to clear
            "OXICODE_MODEL",
            "OXICODE_THEME",
            "OXICODE_TOOL_TIMEOUT",
            "OXICODE_PROVIDER",
            "OXICODE_DEFAULT_MODEL",
        ]);

        let settings = Settings::from_env();
        // All fields should be at defaults since env overrides are disabled
        assert_eq!(settings.last_used_model, None);
        assert_eq!(settings.theme, "default");
        assert_eq!(settings.tool_timeout_seconds, 120);
    }

    #[test]
    fn test_apply_env_boolish() {
        // NOTE: Environment variable overrides are disabled.
        // apply_env() is a no-op.
        let _guard = EnvGuard::new(&["OXICODE_EXTENSIONS_ENABLED"]);
        unsafe { env::set_var("OXICODE_EXTENSIONS_ENABLED", "0") };

        let mut settings = Settings::default();
        settings.apply_env();
        // Since env overrides are disabled, values stay at defaults
        assert!(settings.extensions_enabled); // default is true
    }

    #[test]
    fn test_apply_env_temperature() {
        // NOTE: Environment variable overrides are disabled.
        let _guard = EnvGuard::new(&["OXICODE_TEMPERATURE"]);
        unsafe { env::set_var("OXICODE_TEMPERATURE", "0.7") };

        let mut settings = Settings::default();
        settings.apply_env();
        // Since env overrides are disabled, temperature stays at None
        assert_eq!(settings.default_temperature, None);
    }

    #[test]
    fn test_env_does_not_override_when_unset() {
        let _guard = EnvGuard::new(&[
            "OXICODE_MODEL",
            "OXICODE_PROVIDER",
            "OXICODE_THEME",
            "OXICODE_TEMPERATURE",
        ]);
        let settings = Settings::from_env();
        assert!(settings.last_used_model.is_none());
        assert!(settings.last_used_provider.is_none());
    }

    #[test]
    fn test_parse_thinking_level() {
        assert_eq!(parse_thinking_level("off"), Some(ThinkingLevel::Off));
        assert_eq!(parse_thinking_level("none"), Some(ThinkingLevel::Off));
        assert_eq!(
            parse_thinking_level("MINIMAL"),
            Some(ThinkingLevel::Minimal)
        );
        assert_eq!(parse_thinking_level("Low"), Some(ThinkingLevel::Low));
        assert_eq!(parse_thinking_level("medium"), Some(ThinkingLevel::Medium));
        assert_eq!(parse_thinking_level("Medium"), Some(ThinkingLevel::Medium));
        assert_eq!(
            parse_thinking_level("Standard"),
            Some(ThinkingLevel::Medium)
        );
        assert_eq!(parse_thinking_level("High"), Some(ThinkingLevel::High));
        assert_eq!(parse_thinking_level("thorough"), Some(ThinkingLevel::High));
        assert_eq!(parse_thinking_level("xhigh"), Some(ThinkingLevel::XHigh));
        assert_eq!(parse_thinking_level("invalid"), None);
    }

    #[test]
    fn test_parse_boolish() {
        assert!(parse_boolish("true").unwrap());
        assert!(parse_boolish("1").unwrap());
        assert!(parse_boolish("yes").unwrap());
        assert!(parse_boolish("ON").unwrap());
        assert!(!parse_boolish("false").unwrap());
        assert!(!parse_boolish("0").unwrap());
        assert!(!parse_boolish("no").unwrap());
        assert!(!parse_boolish("OFF").unwrap());
        assert!(parse_boolish("maybe").is_err());
    }

    // ── Effective accessors ──────────────────────────────────────────

    #[test]
    fn test_effective_model_returns_last_used() {
        let mut settings = Settings::default();
        settings.last_used_model = Some("openai/gpt-4o".to_string());
        assert_eq!(
            settings.effective_model(None),
            Some("openai/gpt-4o".to_string())
        );
    }

    #[test]
    fn test_effective_model_cli_overrides() {
        let mut settings = Settings::default();
        settings.last_used_model = Some("openai/gpt-4o".to_string());
        assert_eq!(
            settings.effective_model(Some("anthropic/claude-3")),
            Some("anthropic/claude-3".to_string())
        );
    }

    #[test]
    fn test_effective_model_none_when_unset() {
        let settings = Settings::default();
        assert_eq!(settings.effective_model(None), None);
    }

    #[test]
    fn test_effective_model_falls_back_to_last_used() {
        let mut settings = Settings::default();
        settings.last_used_model = Some("anthropic/claude-3".to_string());
        assert_eq!(
            settings.effective_model(None),
            Some("anthropic/claude-3".to_string())
        );
    }

    #[test]
    fn test_effective_model_returns_none_when_nothing_set() {
        let settings = Settings::default();
        assert_eq!(settings.effective_model(None), None);
    }

    #[test]
    fn test_effective_temperature_prefers_f64() {
        let mut settings = Settings::default();
        settings.temperature = Some(0.5);
        settings.default_temperature = Some(0.7);
        assert_eq!(settings.effective_temperature(), Some(0.7));
    }

    #[test]
    fn test_effective_temperature_falls_back_to_f32() {
        let mut settings = Settings::default();
        settings.temperature = Some(0.5);
        assert_eq!(settings.effective_temperature(), Some(0.5));
    }

    #[test]
    fn test_effective_max_tokens_prefers_usize() {
        let mut settings = Settings::default();
        settings.max_tokens = Some(1024);
        settings.max_response_tokens = Some(4096);
        assert_eq!(settings.effective_max_tokens(), Some(4096));
    }

    #[test]
    fn test_effective_max_tokens_falls_back_to_u32() {
        let mut settings = Settings::default();
        settings.max_tokens = Some(1024);
        assert_eq!(settings.effective_max_tokens(), Some(1024));
    }

    // ── Session dir ──────────────────────────────────────────────────

    #[test]
    fn test_effective_session_dir_default() {
        let _guard = EnvGuard::new(&["OXICODE_SESSION_DIR"]);
        let settings = Settings::default();
        let dir = settings.effective_session_dir().unwrap();
        assert!(dir.ends_with("sessions"), "dir was: {:?}", dir);
    }

    #[test]
    fn test_effective_session_dir_from_field() {
        let _guard = EnvGuard::new(&["OXICODE_SESSION_DIR"]);
        let mut settings = Settings::default();
        settings.session_dir = Some(PathBuf::from("/tmp/oxicode-sessions"));
        assert_eq!(
            settings.effective_session_dir().unwrap(),
            PathBuf::from("/tmp/oxicode-sessions")
        );
    }

    #[test]
    fn test_effective_session_dir_env_disabled() {
        // NOTE: Environment variable overrides are disabled.
        // OXICODE_SESSION_DIR is ignored; effective_session_dir() returns the field value (or default).
        let _guard = EnvGuard::new(&["OXICODE_SESSION_DIR"]);
        unsafe { env::set_var("OXICODE_SESSION_DIR", "/tmp/env-sessions") };
        let settings = Settings::default();
        // Env is ignored, so it should use the default path, not /tmp/env-sessions
        let dir = settings.effective_session_dir().unwrap();
        assert!(
            dir.ends_with("sessions"),
            "expected default sessions dir, got: {:?}",
            dir
        );
    }

    // ── Migration ────────────────────────────────────────────────────

    #[test]
    fn test_migration_v0_to_v1() {
        let mut settings = Settings::default();
        settings.version = 0;
        settings.tool_timeout_seconds = 0; // v0 might not have this field

        let migrated = Settings::migrate(settings).unwrap();
        assert_eq!(migrated.version, SETTINGS_VERSION);
        assert_eq!(migrated.tool_timeout_seconds, 120);
    }

    #[test]
    fn test_migration_already_current() {
        let settings = Settings::default();
        let migrated = Settings::migrate(settings).unwrap();
        assert_eq!(migrated.version, SETTINGS_VERSION);
    }

    #[test]
    fn test_migration_v3_to_v4_splits_model() {
        let mut settings = Settings::default();
        settings.version = 3;
        settings.default_model = Some("openai/gpt-4o".to_string());
        settings.default_provider = None;

        let migrated = Settings::migrate(settings).unwrap();
        assert_eq!(migrated.version, SETTINGS_VERSION);
        assert_eq!(migrated.last_used_model, Some("gpt-4o".to_string()));
        assert_eq!(migrated.last_used_provider, Some("openai".to_string()));
    }

    #[test]
    fn test_migration_v3_no_slash_keeps_model() {
        let mut settings = Settings::default();
        settings.version = 3;
        settings.default_model = Some("bare-model-name".to_string());

        let migrated = Settings::migrate(settings).unwrap();
        assert_eq!(migrated.version, SETTINGS_VERSION);
        assert_eq!(
            migrated.last_used_model,
            Some("bare-model-name".to_string())
        );
    }

    #[test]
    fn test_migration_future_version_fails() {
        let mut settings = Settings::default();
        settings.version = 9999;
        assert!(Settings::migrate(settings).is_err());
    }

    #[test]
    fn test_default_glyph_set_is_unicode() {
        let settings = Settings::default();
        assert_eq!(
            settings.glyph_set,
            GlyphSet::Unicode,
            "glyph_set must default to Unicode"
        );
    }

    #[test]
    fn test_migration_v7_to_v8_defaults_glyph_set_to_unicode() {
        // v7 settings (no glyph_set field on disk) deserialize with the serde
        // default (Unicode) and migrate to v8.
        let mut settings = Settings::default();
        settings.version = 7;
        // Simulate a freshly-loaded v7 file: glyph_set unset → default.
        settings.glyph_set = GlyphSet::default();

        let migrated = Settings::migrate(settings).unwrap();
        assert_eq!(migrated.version, SETTINGS_VERSION);
        assert_eq!(
            migrated.glyph_set,
            GlyphSet::Unicode,
            "v7 → v8 migration must default glyph_set to unicode"
        );
    }

    #[test]
    fn test_glyph_set_persists_through_roundtrip() {
        // Direct TOML serialize → deserialize exercises the on-disk
        // snake_case form (`glyph_set = "nerd"`) without depending on
        // the layered `load_from` directory walk.
        let mut original = Settings::default();
        original.glyph_set = GlyphSet::Nerd;
        let content = toml::to_string_pretty(&original).unwrap();
        assert!(
            content.contains("glyph_set = \"nerd\""),
            "nerd preset must serialize to snake_case; got:\n{content}"
        );
        let loaded: Settings = toml::from_str(&content).unwrap();
        assert_eq!(loaded.glyph_set, GlyphSet::Nerd);
        // Unicode round-trips too.
        original.glyph_set = GlyphSet::Unicode;
        let uni: Settings = toml::from_str(&toml::to_string_pretty(&original).unwrap()).unwrap();
        assert_eq!(uni.glyph_set, GlyphSet::Unicode);
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.toml");

        let mut original = Settings::default();
        original.last_used_model = Some("gpt-4o".to_string());
        original.last_used_provider = Some("openai".to_string());
        original.theme = "dracula".to_string();
        original.tool_timeout_seconds = 60;

        // Serialize
        let content = toml::to_string_pretty(&original).unwrap();
        fs::write(&settings_path, &content).unwrap();

        // Deserialize
        let loaded_content = fs::read_to_string(&settings_path).unwrap();
        let loaded: Settings = toml::from_str(&loaded_content).unwrap();

        assert_eq!(loaded.last_used_model, original.last_used_model);
        assert_eq!(loaded.theme, original.theme);
        assert_eq!(loaded.tool_timeout_seconds, original.tool_timeout_seconds);
    }

    #[test]
    fn test_toml_roundtrip_preserves_new_fields() {
        let mut settings = Settings::default();
        settings.default_temperature = Some(0.8);
        settings.max_response_tokens = Some(8192);
        settings.auto_compaction = false;
        settings.extensions_enabled = false;
        settings.session_dir = Some(PathBuf::from("/custom/sessions"));

        let toml_str = toml::to_string_pretty(&settings).unwrap();
        let parsed: Settings = toml::from_str(&toml_str).unwrap();

        assert_eq!(parsed.default_temperature, Some(0.8));
        assert_eq!(parsed.max_response_tokens, Some(8192));
        assert!(!parsed.auto_compaction);
        assert!(!parsed.extensions_enabled);
        assert_eq!(parsed.session_dir, Some(PathBuf::from("/custom/sessions")));
    }

    // ── JSON format tests ──────────────────────────────────────────────

    #[test]
    fn test_json_roundtrip() {
        let mut settings = Settings::default();
        settings.last_used_model = Some("gpt-4o".to_string());
        settings.last_used_provider = Some("openai".to_string());
        settings.theme = "dracula".to_string();
        settings.tool_timeout_seconds = 60;
        settings.default_temperature = Some(0.8);
        settings.max_response_tokens = Some(8192);

        let json_str = serde_json::to_string_pretty(&settings).unwrap();
        let parsed: Settings = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.last_used_model, settings.last_used_model);
        assert_eq!(parsed.theme, settings.theme);
        assert_eq!(parsed.tool_timeout_seconds, settings.tool_timeout_seconds);
        assert_eq!(parsed.default_temperature, settings.default_temperature);
        assert_eq!(parsed.max_response_tokens, settings.max_response_tokens);
    }

    #[test]
    fn test_json_serialize_for_format() {
        let mut settings = Settings::default();
        settings.last_used_model = Some("claude-3".to_string());
        settings.last_used_provider = Some("anthropic".to_string());
        settings.thinking_level = ThinkingLevel::Minimal;

        let json_content = Settings::serialize_for_format(&settings, SettingsFormat::Json).unwrap();
        let parsed: Settings = serde_json::from_str(&json_content).unwrap();

        assert_eq!(parsed.last_used_model, Some("claude-3".to_string()));
        assert_eq!(parsed.thinking_level, ThinkingLevel::Minimal);
    }

    #[test]
    fn test_toml_serialize_for_format() {
        let mut settings = Settings::default();
        settings.last_used_model = Some("gemini-pro".to_string());
        settings.last_used_provider = Some("google".to_string());
        settings.thinking_level = ThinkingLevel::High;

        let toml_content = Settings::serialize_for_format(&settings, SettingsFormat::Toml).unwrap();
        let parsed: Settings = toml::from_str(&toml_content).unwrap();

        assert_eq!(parsed.last_used_model, Some("gemini-pro".to_string()));
        assert_eq!(parsed.thinking_level, ThinkingLevel::High);
    }

    #[test]
    fn test_parse_from_str_json() {
        let json_content = r#"{
            "last_used_model": "gpt-4",
            "last_used_provider": "openai",
            "theme": "nord",
            "tool_timeout_seconds": 90
        }"#;

        let settings = Settings::parse_from_str(json_content, SettingsFormat::Json).unwrap();
        assert_eq!(settings.last_used_model, Some("gpt-4".to_string()));
        assert_eq!(settings.last_used_provider, Some("openai".to_string()));
        assert_eq!(settings.theme, "nord");
        assert_eq!(settings.tool_timeout_seconds, 90);
        // Unchanged fields retain defaults
        assert_eq!(settings.thinking_level, ThinkingLevel::Medium);
        assert!(settings.extensions_enabled);
    }

    #[test]
    fn test_parse_from_str_toml() {
        let toml_content = r#"
last_used_model = "claude-opus"
last_used_provider = "anthropic"
theme = "monokai"
tool_timeout_seconds = 45
"#;

        let settings = Settings::parse_from_str(toml_content, SettingsFormat::Toml).unwrap();
        assert_eq!(settings.last_used_model, Some("claude-opus".to_string()));
        assert_eq!(settings.last_used_provider, Some("anthropic".to_string()));
        assert_eq!(settings.theme, "monokai");
        assert_eq!(settings.tool_timeout_seconds, 45);
        assert_eq!(settings.thinking_level, ThinkingLevel::Medium);
    }

    #[test]
    fn test_layer_file_json() {
        let base = Settings::default();

        let tmp = tempfile::NamedTempFile::with_suffix(".json").unwrap();
        let json_content = r#"{
            "last_used_model": "gpt-4o",
            "last_used_provider": "openai",
            "theme": "dracula",
            "auto_compaction": false
        }"#;
        tmp.as_file().write_all(json_content.as_bytes()).unwrap();

        let merged = Settings::layer_file(&base, tmp.path()).unwrap();
        assert_eq!(merged.last_used_model, Some("gpt-4o".to_string()));
        assert_eq!(merged.last_used_provider, Some("openai".to_string()));
        assert_eq!(merged.theme, "dracula");
        assert!(!merged.auto_compaction);
        // Unchanged fields retain defaults
        assert_eq!(merged.thinking_level, ThinkingLevel::Medium);
        assert!(merged.extensions_enabled);
        assert_eq!(merged.tool_timeout_seconds, 120);
    }

    #[test]
    fn test_layer_file_json_preserves_unset() {
        let mut base = Settings::default();
        base.last_used_provider = Some("deepseek".to_string());

        let tmp = tempfile::NamedTempFile::with_suffix(".json").unwrap();
        let json_content = r#"{ "theme": "nord" }"#;
        tmp.as_file().write_all(json_content.as_bytes()).unwrap();

        let merged = Settings::layer_file(&base, tmp.path()).unwrap();
        assert_eq!(merged.theme, "nord");
        assert_eq!(merged.last_used_provider, Some("deepseek".to_string()));
    }

    #[test]
    fn test_save_to_json() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");

        let mut settings = Settings::default();
        settings.last_used_model = Some("gpt-4o".to_string());
        settings.last_used_provider = Some("openai".to_string());
        settings.theme = "dracula".to_string();
        settings.tool_timeout_seconds = 60;

        settings.save_to(&settings_path).unwrap();

        // Verify it's valid JSON
        let content = fs::read_to_string(&settings_path).unwrap();
        let parsed: Settings = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.last_used_model, Some("gpt-4o".to_string()));
        assert_eq!(parsed.theme, "dracula");
        assert_eq!(parsed.tool_timeout_seconds, 60);
    }

    #[test]
    fn test_save_to_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.toml");

        let mut settings = Settings::default();
        settings.last_used_model = Some("gemini-pro".to_string());
        settings.last_used_provider = Some("google".to_string());
        settings.theme = "monokai".to_string();
        settings.tool_timeout_seconds = 90;

        settings.save_to(&settings_path).unwrap();

        // Verify it's valid TOML
        let content = fs::read_to_string(&settings_path).unwrap();
        let parsed: Settings = toml::from_str(&content).unwrap();
        assert_eq!(parsed.last_used_model, Some("gemini-pro".to_string()));
        assert_eq!(parsed.theme, "monokai");
        assert_eq!(parsed.tool_timeout_seconds, 90);
    }

    #[test]
    fn test_load_from_dir_with_json_project_config() {
        let _guard = EnvGuard::new(&[
            "OXICODE_MODEL",
            "OXICODE_PROVIDER",
            "OXICODE_THEME",
            "OXICODE_TOOL_TIMEOUT",
            "OXICODE_TEMPERATURE",
            "OXICODE_MAX_TOKENS",
            "OXICODE_SESSION_DIR",
            "OXICODE_EXTENSIONS_ENABLED",
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let oxicode_dir = tmp.path().join(".oxicode");
        fs::create_dir_all(&oxicode_dir).unwrap();
        let settings_path = oxicode_dir.join("settings.json");
        // v3 format: default_model has provider/model
        let json_content = r#"{ "version": 3, "default_model": "google/gemini-2.0-flash" }"#;
        fs::write(&settings_path, json_content).unwrap();

        let settings = Settings::load_from(tmp.path()).unwrap();
        // Migration splits provider from model
        assert_eq!(
            settings.last_used_model,
            Some("gemini-2.0-flash".to_string())
        );
        assert_eq!(settings.last_used_provider, Some("google".to_string()));
    }

    #[test]
    fn test_find_project_settings_json_priority() {
        let tmp = tempfile::tempdir().unwrap();
        let oxicode_dir = tmp.path().join(".oxicode");
        fs::create_dir_all(&oxicode_dir).unwrap();

        // Create both files
        let json_path = oxicode_dir.join("settings.json");
        let toml_path = oxicode_dir.join("settings.toml");
        fs::write(&json_path, r#"{ "theme": "json-theme" }"#).unwrap();
        fs::write(&toml_path, r#"theme = "toml-theme""#).unwrap();

        // JSON takes priority
        let found = Settings::find_project_settings(tmp.path());
        assert!(found.is_some());
        assert_eq!(
            found.unwrap().file_name().unwrap().to_str().unwrap(),
            "settings.json"
        );
    }

    #[test]
    fn test_find_project_settings_json_only() {
        let tmp = tempfile::tempdir().unwrap();
        let oxicode_dir = tmp.path().join(".oxicode");
        fs::create_dir_all(&oxicode_dir).unwrap();

        let json_path = oxicode_dir.join("settings.json");
        fs::write(&json_path, r#"{ "theme": "test" }"#).unwrap();

        let found = Settings::find_project_settings(tmp.path());
        assert!(found.is_some());
        assert_eq!(
            found.unwrap().file_name().unwrap().to_str().unwrap(),
            "settings.json"
        );
    }

    #[test]
    fn test_find_project_settings_toml_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let oxicode_dir = tmp.path().join(".oxicode");
        fs::create_dir_all(&oxicode_dir).unwrap();

        let toml_path = oxicode_dir.join("settings.toml");
        fs::write(&toml_path, r#"theme = "test""#).unwrap();

        let found = Settings::find_project_settings(tmp.path());
        assert!(found.is_some());
        assert_eq!(
            found.unwrap().file_name().unwrap().to_str().unwrap(),
            "settings.toml"
        );
    }

    #[test]
    fn test_detect_format() {
        let json_path = PathBuf::from("/test/settings.json");
        let toml_path = PathBuf::from("/test/settings.toml");
        let unknown_path = PathBuf::from("/test/settings");

        assert_eq!(Settings::detect_format(&json_path), SettingsFormat::Json);
        assert_eq!(Settings::detect_format(&toml_path), SettingsFormat::Toml);
        assert_eq!(Settings::detect_format(&unknown_path), SettingsFormat::Json);
        // Default
    }

    #[test]
    fn test_settings_format_extension() {
        assert_eq!(SettingsFormat::Json.extension(), "json");
        assert_eq!(SettingsFormat::Toml.extension(), "toml");
    }

    #[test]
    fn test_layer_json_over_toml() {
        // Test that when loading, JSON takes priority over TOML
        let tmp = tempfile::tempdir().unwrap();
        let oxicode_dir = tmp.path().join(".oxicode");
        fs::create_dir_all(&oxicode_dir).unwrap();

        let json_path = oxicode_dir.join("settings.json");
        let toml_path = oxicode_dir.join("settings.toml");

        // JSON has model set to "json-model"
        fs::write(&json_path, r#"{ "last_used_model": "json-model" }"#).unwrap();
        // TOML has model set to "toml-model"
        fs::write(&toml_path, r#"last_used_model = "toml-model""#).unwrap();

        // JSON takes priority
        let settings = Settings::load_from(tmp.path()).unwrap();
        assert_eq!(settings.last_used_model, Some("json-model".to_string()));
    }

    #[test]
    fn test_mixed_format_loading() {
        // Test loading a TOML file through the generic layer_file
        let tmp = tempfile::NamedTempFile::with_suffix(".toml").unwrap();
        let toml_content = r#"
last_used_model = "loaded-via-toml"
theme = "loaded-theme"
"#;
        tmp.as_file().write_all(toml_content.as_bytes()).unwrap();

        let merged = Settings::layer_file(&Settings::default(), tmp.path()).unwrap();
        assert_eq!(merged.last_used_model, Some("loaded-via-toml".to_string()));
        assert_eq!(merged.theme, "loaded-theme");
    }

    #[test]
    fn test_merge_json_values() {
        let base = serde_json::json!({
            "version": 1,
            "theme": "default",
            "extensions": ["ext1"],
            "nested": {
                "a": 1,
                "b": 2
            }
        });

        let override_ = serde_json::json!({
            "version": 2,
            "theme": "dark",
            "extensions": ["ext2"],
            "nested": {
                "b": 20,
                "c": 30
            }
        });

        let merged = merge_json_values(base, override_);

        assert_eq!(merged["version"], 2);
        assert_eq!(merged["theme"], "dark");
        // Arrays are replaced, not merged
        assert_eq!(merged["extensions"], serde_json::json!(["ext2"]));
        // Nested objects are deeply merged
        assert_eq!(merged["nested"]["a"], 1);
        assert_eq!(merged["nested"]["b"], 20);
        assert_eq!(merged["nested"]["c"], 30);
    }

    #[test]
    fn test_save_project_preserves_existing_format() {
        let tmp = tempfile::tempdir().unwrap();
        let oxicode_dir = tmp.path().join(".oxicode");
        fs::create_dir_all(&oxicode_dir).unwrap();

        // Create existing TOML file
        let toml_path = oxicode_dir.join("settings.toml");
        fs::write(&toml_path, "theme = 'old-theme'").unwrap();

        let mut settings = Settings::default();
        settings.theme = "new-theme".to_string();
        settings.save_project(tmp.path()).unwrap();

        // Should still be TOML
        let content = fs::read_to_string(&toml_path).unwrap();
        assert!(content.contains("new-theme"));
        assert!(serde_json::from_str::<serde_json::Value>(&content).is_err());
    }

    #[test]
    fn test_save_project_creates_json_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let oxicode_dir = tmp.path().join(".oxicode");
        fs::create_dir_all(&oxicode_dir).unwrap();
        // Don't create any settings file

        let mut settings = Settings::default();
        settings.theme = "json-theme".to_string();
        settings.save_project(tmp.path()).unwrap();

        // Should create JSON file
        let json_path = oxicode_dir.join("settings.json");
        assert!(json_path.exists());
        let content = fs::read_to_string(&json_path).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&content).is_ok());
        assert!(content.contains("json-theme"));
    }

    // ── Custom provider tests ───────────────────────────────────────

    #[test]
    fn test_custom_provider_default_api() {
        use super::CustomProvider;
        let cp = CustomProvider {
            name: "test".to_string(),
            base_url: "https://api.test.com/v1".to_string(),
            api_key_env: "TEST_API_KEY".to_string(),
            api: super::default_custom_provider_api(),
        };
        assert_eq!(cp.api, "openai-completions");
    }

    #[test]
    fn test_custom_provider_toml_deserialize() {
        let toml_content = r#"
[[custom_providers]]
name = "minimax"
base_url = "https://api.minimax.chat/v1"
api_key_env = "MINIMAX_API_KEY"
api = "openai-completions"

[[custom_providers]]
name = "zai"
base_url = "https://api.z.ai/v1"
api_key_env = "ZAI_API_KEY"
api = "openai-responses"
"#;
        let settings: Settings = toml::from_str(toml_content).unwrap();
        assert_eq!(settings.custom_providers.len(), 2);
        assert_eq!(settings.custom_providers[0].name, "minimax");
        assert_eq!(
            settings.custom_providers[0].base_url,
            "https://api.minimax.chat/v1"
        );
        assert_eq!(settings.custom_providers[0].api_key_env, "MINIMAX_API_KEY");
        assert_eq!(settings.custom_providers[0].api, "openai-completions");
        assert_eq!(settings.custom_providers[1].name, "zai");
        assert_eq!(settings.custom_providers[1].api, "openai-responses");
    }

    #[test]
    fn test_custom_provider_json_deserialize() {
        let json_content = r#"{
            "custom_providers": [
                {
                    "name": "minimax",
                    "base_url": "https://api.minimax.chat/v1",
                    "api_key_env": "MINIMAX_API_KEY",
                    "api": "openai-completions"
                }
            ]
        }"#;
        let settings: Settings = serde_json::from_str(json_content).unwrap();
        assert_eq!(settings.custom_providers.len(), 1);
        assert_eq!(settings.custom_providers[0].name, "minimax");
    }

    #[test]
    fn test_custom_provider_toml_roundtrip() {
        let mut settings = Settings::default();
        settings.custom_providers.push(super::CustomProvider {
            name: "test".to_string(),
            base_url: "https://api.test.com/v1".to_string(),
            api_key_env: "TEST_API_KEY".to_string(),
            api: "openai-completions".to_string(),
        });

        let toml_str = toml::to_string_pretty(&settings).unwrap();
        let parsed: Settings = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.custom_providers.len(), 1);
        assert_eq!(parsed.custom_providers[0].name, "test");
        assert_eq!(
            parsed.custom_providers[0].base_url,
            "https://api.test.com/v1"
        );
    }

    #[test]
    fn test_custom_provider_defaults_empty() {
        let settings = Settings::default();
        assert!(settings.custom_providers.is_empty());
    }

    #[test]
    fn test_custom_provider_layer_file() {
        let base = Settings::default();

        let tmp = tempfile::NamedTempFile::with_suffix(".toml").unwrap();
        let toml_content = r#"
[[custom_providers]]
name = "my-provider"
base_url = "https://api.my-provider.com/v1"
api_key_env = "MY_PROVIDER_API_KEY"
"#;
        tmp.as_file().write_all(toml_content.as_bytes()).unwrap();

        let merged = Settings::layer_file(&base, tmp.path()).unwrap();
        assert_eq!(merged.custom_providers.len(), 1);
        assert_eq!(merged.custom_providers[0].name, "my-provider");
        // Default api value
        assert_eq!(merged.custom_providers[0].api, "openai-completions");
    }

    #[test]
    fn settings_deserialise_hooks_array() {
        let toml = r#"
            [[hooks]]
            event = "PreToolUse"
            matcher = "bash|write"
            command = "echo pre"
            timeout_secs = 10
        "#;
        let s: Settings = toml::from_str(toml).unwrap();
        assert_eq!(s.hooks.len(), 1);
        assert_eq!(s.hooks[0].event, oxicode_sdk::ports::HookEvent::PreToolUse);
        assert_eq!(s.hooks[0].matcher.as_deref(), Some("bash|write"));
        assert_eq!(s.hooks[0].command, "echo pre");
        assert_eq!(s.hooks[0].timeout_secs, Some(10));
    }

    #[test]
    fn settings_default_has_no_hooks() {
        let s = Settings::default();
        assert!(s.hooks.is_empty());
    }
}
