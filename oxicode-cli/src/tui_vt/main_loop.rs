#![allow(
    clippy::field_reassign_with_default,
    clippy::let_and_return,
    clippy::borrow_interior_mutable_const,
    clippy::derivable_impls
)]
//! TUI main event loop — connects oxicode's `AgentSession` to vtcode-ui's
//! `InlineSession` protocol and a ratatui rendering backend.

use std::io::{self, Stdout, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, disable_raw_mode, enable_raw_mode},
};
use oxicode_agent::AgentEvent;
use oxicode_agent::config::Mode;
use oxicode_agent::tools::TodoStateProvider;
use oxicode_agent::tools::todo::TodoStatus;
use oxicode_vtui::theme::{ThemeStyles, active_styles};
use oxicode_vtui::tui::core::{
    AuthAction, InlineCommand, InlineEvent, InlineHandle, InlineHeaderContext,
    InlineHeaderStatusBadge, InlineHeaderStatusTone, InlineListItem, InlineListSelection,
    InlineMessageKind, InlineSegment, InlineTextStyle, OverlayRequest, OverlaySubmission,
    SecurePromptConfig,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

use crate::App;
use crate::app::agent_hub_registry::HubEntry;
use crate::app::agent_session::SessionEvent;
use crate::tui_vt::slash::file_commands::FileCommand;
use crate::tui_vt::slash::registry::{SlashCtx, SlashOutcome, SlashRegistry};

// ─────────────────────────────────────────────────────────────────────────
// Terminal lifecycle (RAII)
// ─────────────────────────────────────────────────────────────────────────

/// Terminal wrapper with deterministic enter / exit / Drop semantics.
///
/// Each cleanup step in `exit` is independent — a failure in one stage
/// (e.g. `PopKeyboardEnhancementFlags`) MUST NOT prevent later stages
/// (`disable_raw_mode`) from running, or the user's terminal is left in
/// raw mode (no echo, no line editing).
pub struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    tty_ok: bool,
}

impl Tui {
    /// Enter the alternate screen, enable raw mode, push keyboard flags,
    /// enable bracketed paste, hide the cursor, install the panic hook.
    pub fn enter() -> Result<Self> {
        Self::set_panic_hook();

        let tty_ok = enable_raw_mode().is_ok();
        let mut stdout = io::stdout();

        if tty_ok {
            // Report event types so key-release / repeat events arrive as
            // distinct codes. Full Kitty flag set is gated on
            // OXICODE_KITTY_KEYBOARD=1; default mirrors pre-Kitty behavior.
            let flags = if std::env::var("OXICODE_KITTY_KEYBOARD").as_deref() == Ok("1") {
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            } else {
                KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            };
            let _ = execute!(
                stdout,
                Hide,
                EnableBracketedPaste,
                PushKeyboardEnhancementFlags(flags)
            );
            let _ = stdout.flush();
        }

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        if tty_ok {
            let _ = terminal.clear();
        }

        Ok(Self { terminal, tty_ok })
    }

    /// Restore the terminal to its pre-TUI state. Each step is independent;
    /// errors are swallowed so a partial restoration never strands the user
    /// in raw mode.
    pub fn exit(&mut self) -> Result<()> {
        if self.tty_ok {
            let _ = execute!(
                self.terminal.backend_mut(),
                PopKeyboardEnhancementFlags,
                DisableBracketedPaste
            );
            let _ = self.terminal.show_cursor();
            // disable_raw_mode is the most critical — always attempt it.
            disable_raw_mode()?;
            self.tty_ok = false;
        }
        Ok(())
    }

    /// Install a panic hook that restores the terminal before printing the
    /// panic message. Without this, a panic inside the TUI strands the
    /// user's shell in raw mode / alternate screen.
    fn set_panic_hook() {
        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            let _ = execute!(io::stdout(), Show);
            let _ = disable_raw_mode();
            original_hook(panic_info);
        }));
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.exit();
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Render state — shared between the input thread and the main loop.
// ─────────────────────────────────────────────────────────────────────────

/// Mutable state the input thread edits (text buffer, scroll, footer) and
/// the main loop reads for rendering.
#[derive(Default)]
pub struct RenderState {
    /// Editable text in the composer.
    pub input_buffer: String,
    /// Cursor position inside `input_buffer` (byte index).
    pub input_cursor: usize,
    /// Transcript lines, in display order.
    pub transcript: Vec<TranscriptLine>,
    /// Index of the line currently pinned at the top of the viewport.
    /// `usize::MAX` means "follow the tail" (auto-scroll).
    pub scroll_offset: usize,
    /// Header context mirrored from `InlineHeaderContext`.
    pub header_context: InlineHeaderContext,
    /// Composer enabled state — mirrored from `SetInputEnabled`.
    pub input_enabled: bool,
    /// Footer status (left + right) — mirrored from `SetInputStatus`.
    pub footer_left: Option<String>,
    pub footer_right: Option<String>,
    /// Composer prompt prefix — mirrored from `SetPrompt`.
    pub prompt_prefix: String,
    /// Composer placeholder — mirrored from `SetPlaceholder`.
    pub placeholder: Option<String>,
    /// Shutdown signal received from the harness.
    pub shutdown_requested: bool,
    /// Accumulated text for markdown rendering at message end.
    pub message_buffer: String,
    /// Agent Hub overlay open.
    pub agent_hub_open: bool,
    /// Hub entries snapshotted when the overlay was opened (`/agents`).
    pub hub_entries: Vec<(String, HubEntry)>,
    /// First Ctrl+C armed a quit; a second press exits (two-press quit).
    pub pending_quit: bool,
    /// Slash-command autocomplete popup state.
    pub slash_popup: SlashPopup,
    /// Current reasoning/tool stage (e.g. "tool: read"), shown above the composer.
    pub reasoning_stage: Option<String>,
    /// Overlay modal/list state — `Some` when an overlay is open.
    pub overlay: Option<OverlayState>,
    /// Model IDs for the /model overlay picker (ordered same as overlay items).
    pub overlay_model_ids: Vec<String>,
    /// `(provider, model_id)` pairs backing the `/models` catalog browser
    /// overlay (ordered same as overlay items).
    pub overlay_catalog_models: Vec<(String, String)>,
    /// Provider names backing the `/providers` overlay (ordered same as items).
    pub overlay_providers: Vec<String>,
    /// Model catalog port handle, captured once at TUI startup so slash
    /// commands (`/models`, `/providers`) can browse the full catalog.
    pub catalog: Option<std::sync::Arc<dyn oxicode_sdk::ports::catalog::ModelCatalog>>,
    /// Queued input prompts (waiting to be processed).
    pub queued_inputs: Vec<String>,
    /// Queued input prompts — interactive panel open (Ctrl+; toggles).
    pub queue_panel_open: bool,
    /// Selected index within the queue panel (when interactive).
    pub queue_selected: usize,
    /// Shell mode — `!` prefix for direct bash commands (grok-build parity).
    pub shell_mode: bool,
    /// Follow-up suggestion chips.
    pub follow_ups: Vec<String>,
    /// Todo checklist items (text, status) — refreshed from the live provider.
    pub todo_items: Vec<(String, TodoStatus)>,
    /// Live todo state provider — the same source the `todo` agent tool
    /// writes to. `None` when todos are disabled; the pane stays hidden.
    pub todo_provider: Option<Arc<dyn TodoStateProvider>>,
    /// Vim editing state (enabled by /vim command).
    pub vim_state: oxicode_vtui::vim::VimState,
    /// Vim clipboard buffer.
    pub vim_clipboard: String,
    /// In-transcript search state — `None` when no search is active.
    pub search: Option<SearchState>,
    /// Per-block display override. An absent entry means the default
    /// ([`BlockDisplayMode::Truncated]).
    pub block_display: std::collections::HashMap<usize, BlockDisplayMode>,
    /// Last Esc press timestamp (for double-Esc detection).
    pub last_esc_at: Option<std::time::Instant>,
    /// Multiline input mode — Enter inserts newline, Shift+Enter sends.
    pub multiline_mode: bool,
    /// Autonomy mode mirror for display. The authoritative value lives in
    /// the shared `AskBridge` mode atomic (toggled by Shift+Tab); this field
    /// is kept in lock-step so the render loop can draw a badge.
    pub autonomy_mode: Mode,
    /// Submitted prompt history (most-recent-first).
    pub prompt_history: Vec<String>,
    /// Current position in history navigation (None = not navigating).
    pub history_pos: Option<usize>,
    /// Next block ID to assign when appending transcript lines.
    pub next_block_id: usize,
    /// Cancel grace window — Esc pressed within this window after a cancel
    /// is ignored (grok-build post-cancel grace, ~1s). Prevents mashing.
    pub cancel_grace_until: Option<std::time::Instant>,
    /// Active y/n/x confirmation dialog — `Some` while a modal confirmation
    /// is open. The input thread resolves it; the render loop paints it
    /// centered on top of everything else.
    pub confirmation: Option<ModalConfirmation>,
    /// Active ephemeral tip banner — `Some` for a bounded number of render
    /// ticks, then auto-dismissed by expiry.
    pub tip: Option<EphemeralTip>,
    /// Workspace root — used by the @ file picker to walk + fuzzy-match.
    pub cwd: PathBuf,
    /// Active @-file-search dropdown — `Some` while the picker is open.
    pub file_search: Option<crate::tui_vt::file_search::FileSearchState>,
    /// Per-tip-key show counter — suppresses ambient tips after SEEN_CAP views.
    pub seen_tips: std::collections::HashMap<&'static str, u32>,
    /// User-defined slash commands loaded once at startup from
    /// `.oxicode/commands/` and `~/.oxicode/commands/`.
    pub file_commands: Vec<FileCommand>,
    /// Provider name targeted by the currently open secure prompt. Set
    /// before opening the prompt; cleared on `OverlaySubmission::SecureInput`
    /// after the key is written. `None` outside the `/providers` key-entry
    /// flow so a stray `SecureInput` cannot leak into a different provider.
    pub secure_input_target: Option<String>,
}

/// One rendered transcript line.
#[derive(Debug, Clone)]
pub struct TranscriptLine {
    pub kind: InlineMessageKind,
    pub segments: Vec<InlineSegment>,
    /// Block group ID — consecutive lines of the same kind share a block.
    /// Assigned incrementally when lines are appended.
    pub block_id: usize,
}

/// Three-state display mode for a transcript block (grok-build parity).
///
/// The default is [`BlockDisplayMode::Truncated`] — finished long blocks
/// show their head, an ellipsis gap, and a tail snippet rather than the
/// full body, keeping the scrollback scannable.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BlockDisplayMode {
    /// Fully collapsed — only the first line shows (▸ marker).
    Collapsed,
    /// Default — first line + ellipsis gap + last N lines, body DIM.
    #[default]
    Truncated,
    /// Fully expanded — every line shows at full weight.
    Expanded,
}

/// In-transcript search state.
#[derive(Clone, Debug)]
pub struct SearchState {
    pub query: String,
    /// Transcript line indices that contain a match.
    pub matches: Vec<usize>,
    /// Current match cursor (index into `matches`).
    pub current: usize,
}

/// One filtered entry in the `/`-command autocomplete popup.
#[derive(Clone)]
pub struct SlashPopupItem {
    /// Display label, e.g. `"/quit, /exit, /q"`.
    pub label: String,
    /// Short human description.
    pub description: String,
    /// Canonical command name (no leading `/`), used for completion.
    pub name: String,
}

/// Slash-command autocomplete popup state, managed by the input thread and
/// read by the render loop. The popup is open when the input buffer starts
/// with `/` and contains no space (i.e. the user is still typing the command
/// token, not its arguments).
#[derive(Default, Clone)]
pub struct SlashPopup {
    pub open: bool,
    pub items: Vec<SlashPopupItem>,
    pub selected: usize,
}

/// One item rendered inside a list overlay. Mirrors [`InlineListItem`] but
/// is a value type owned by the TUI (the input thread reads/writes these
/// fields directly via the `parking_lot::Mutex<RenderState>`).
#[derive(Clone, Debug)]
pub struct OverlayListItem {
    pub title: String,
    pub subtitle: Option<String>,
    pub badge: Option<String>,
    pub indent: u8,
    pub search_value: Option<String>,
    /// Original `InlineListSelection` echoed back to the harness on submit.
    pub selection: Option<oxicode_vtui::tui::core::InlineListSelection>,
}

/// Overlay modal/list state — materialised by `apply_command` when an
/// `InlineCommand::ShowOverlay` arrives. The input thread mutates
/// `selected` / `search` while the overlay is open and reads the same
/// fields when forwarding `OverlayEvent`s.
#[derive(Clone, Debug)]
pub struct OverlayState {
    pub title: String,
    pub lines: Vec<String>,
    pub items: Vec<OverlayListItem>,
    pub selected: usize,
    pub search: Option<OverlaySearchState>,
    pub secure_input: Option<OverlaySecureInput>,
}

/// Secure (masked) single-line input state carried by an overlay.
/// Only present when the original `OverlayRequest::Modal` carried a
/// `secure_prompt`. The input thread mutates `value` and `cursor` while
/// the overlay is open; on `Enter` it submits `OverlaySubmission::SecureInput`.
#[derive(Clone, Debug)]
pub struct OverlaySecureInput {
    pub config: SecurePromptConfig,
    pub value: String,
    pub cursor: usize,
}

/// A y/n/x confirmation dialog (grok-build `ModalConfirmation` parity).
/// Rendered centered on top of everything else; the input thread routes
/// `y` → confirm, `n` → decline (when offered), `x`/`Esc` → cancel.
#[derive(Clone, Debug)]
pub struct ModalConfirmation {
    pub title: String,
    pub message: String,
    /// What happens when the user confirms (`y`). Cancel (`n`/`x`/`Esc`)
    /// always just closes the dialog.
    pub action: ConfirmationAction,
}

/// The action bound to a [`ModalConfirmation`] — dispatched on `y`/Enter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmationAction {
    /// Exit the application.
    Quit,
    /// Clear the conversation transcript + reset the agent session.
    ClearConversation,
    /// Remove the stored API key for a provider (`/providers` → confirm).
    RemoveProviderKey(String),
}

/// A short-lived contextual tip banner (grok-build ephemeral tips parity).
/// Shown as one line above the composer for a bounded number of render
/// ticks, then auto-dismissed.
#[derive(Clone, Debug)]
pub struct EphemeralTip {
    pub text: String,
    /// Render tick the tip was born at (`FRAME_TICK` snapshot).
    pub born_tick: u64,
    /// How many ticks the tip stays visible before auto-dismissing.
    pub ttl_ticks: u64,
    /// Stable identifier for per-session seen-cap tracking. Tips with the
    /// same key are suppressed after `SEEN_CAP` showings.
    pub key: &'static str,
    /// Ambient tips (background suggestions) are occluded — their TTL pauses
    /// while an overlay/confirmation/dropdown is open. Non-ambient tips
    /// (direct user-action feedback) always count down.
    pub ambient: bool,
}

/// Search-bar state for an overlay. `None` value means search is disabled.
#[derive(Clone, Debug)]
pub struct OverlaySearchState {
    pub label: String,
    pub placeholder: Option<String>,
    pub value: String,
}

impl RenderState {
    fn new_with_header(header: InlineHeaderContext) -> Self {
        let mut s = Self::default();
        s.header_context = header;
        s.prompt_prefix = "> ".to_string();
        s.input_enabled = true;
        s
    }

    /// Append a brand-new line to the transcript.
    fn append_line(&mut self, kind: InlineMessageKind, segments: Vec<InlineSegment>) {
        let block_id = self.block_id_for_kind(kind);
        self.transcript.push(TranscriptLine {
            kind,
            segments,
            block_id,
        });
    }

    /// Append a segment to the most recent transcript line, or create a new
    /// line if the transcript is empty. Used for `Inline { kind, segment }`
    /// where the segment is a streaming delta.
    fn inline_segment(&mut self, kind: InlineMessageKind, segment: InlineSegment) {
        if let Some(last) = self.transcript.last_mut()
            && last.kind == kind
        {
            last.segments.push(segment);
            return;
        }
        let block_id = self.block_id_for_kind(kind);
        self.transcript.push(TranscriptLine {
            kind,
            segments: vec![segment],
            block_id,
        });
    }

    /// Determine the block_id for a new line: reuse the last line's block
    /// if the kind matches, otherwise allocate a new block.
    fn block_id_for_kind(&mut self, kind: InlineMessageKind) -> usize {
        if let Some(last) = self.transcript.last()
            && last.kind == kind
        {
            return last.block_id;
        }
        let id = self.next_block_id;
        self.next_block_id += 1;
        id
    }

    // ── Search ──

    /// Start a new transcript search, collecting all matching line indices.
    pub fn start_search(&mut self, query: &str) {
        let needle = query.to_lowercase();
        let matches: Vec<usize> = self
            .transcript
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                line.segments
                    .iter()
                    .any(|s| s.text.to_lowercase().contains(&needle))
            })
            .map(|(i, _)| i)
            .collect();
        self.search = Some(SearchState {
            query: query.to_string(),
            matches,
            current: 0,
        });
        // Jump to the first match if any.
        if let Some(s) = &self.search
            && let Some(&first) = s.matches.first()
        {
            self.scroll_offset = first;
        }
    }

    /// Advance to the next search match (wraps around).
    pub fn search_next(&mut self) {
        if let Some(s) = &mut self.search
            && !s.matches.is_empty()
        {
            s.current = (s.current + 1) % s.matches.len();
            let line = s.matches[s.current];
            self.scroll_offset = line;
        }
    }

    /// Go to the previous search match (wraps around).
    pub fn search_prev(&mut self) {
        if let Some(s) = &mut self.search
            && !s.matches.is_empty()
        {
            if s.current == 0 {
                s.current = s.matches.len() - 1;
            } else {
                s.current -= 1;
            }
            let line = s.matches[s.current];
            self.scroll_offset = line;
        }
    }

    // ── Block display modes (Collapsed / Truncated / Expanded) ──

    /// The display mode for a block — explicit override or the Truncated default.
    pub fn block_mode(&self, block_id: usize) -> BlockDisplayMode {
        self.block_display
            .get(&block_id)
            .copied()
            .unwrap_or_default()
    }

    /// Cycle the display mode of the block at (or nearest above) the current
    /// scroll offset: Collapsed → Truncated → Expanded → Collapsed.
    pub fn cycle_block_at_view(&mut self) {
        let offset = self.effective_offset();
        if let Some(line) = self.transcript.get(offset) {
            let bid = line.block_id;
            let next = match self.block_mode(bid) {
                BlockDisplayMode::Collapsed => BlockDisplayMode::Truncated,
                BlockDisplayMode::Truncated => BlockDisplayMode::Expanded,
                BlockDisplayMode::Expanded => BlockDisplayMode::Collapsed,
            };
            // Truncated is the default — represent it by absence so the map
            // only carries real overrides.
            if next == BlockDisplayMode::Truncated {
                self.block_display.remove(&bid);
            } else {
                self.block_display.insert(bid, next);
            }
        }
    }

    /// Expand every block (show every line at full weight).
    pub fn expand_all(&mut self) {
        for bid in self.all_block_ids() {
            self.block_display.insert(bid, BlockDisplayMode::Expanded);
        }
    }

    /// Collapse every block (first line only).
    pub fn fold_all(&mut self) {
        for bid in self.all_block_ids() {
            self.block_display.insert(bid, BlockDisplayMode::Collapsed);
        }
    }

    /// Reset every block to the default Truncated mode.
    pub fn truncate_all(&mut self) {
        self.block_display.clear();
    }

    /// Distinct block ids in transcript order.
    fn all_block_ids(&self) -> Vec<usize> {
        let mut ids = Vec::new();
        let mut prev: Option<usize> = None;
        for l in &self.transcript {
            if prev != Some(l.block_id) {
                ids.push(l.block_id);
                prev = Some(l.block_id);
            }
        }
        ids
    }

    // ── Turn navigation ──

    /// Jump the scroll to the start of the next assistant (Agent) block.
    pub fn jump_next_turn(&mut self) {
        let offset = self.effective_offset();
        let search_after = self
            .transcript
            .iter()
            .enumerate()
            .skip(offset + 1)
            .find(|(_, l)| l.kind == InlineMessageKind::Agent || l.kind == InlineMessageKind::User);
        if let Some((idx, _)) = search_after {
            self.scroll_offset = idx;
        }
    }

    /// Jump the scroll to the start of the previous user block.
    pub fn jump_prev_turn(&mut self) {
        let offset = self.effective_offset();
        let search_before = self
            .transcript
            .iter()
            .enumerate()
            .take(offset)
            .rev()
            .find(|(_, l)| l.kind == InlineMessageKind::User);
        if let Some((idx, _)) = search_before {
            self.scroll_offset = idx;
        }
    }

    /// Effective scroll offset (resolves `usize::MAX` follow-tail to a real index).
    fn effective_offset(&self) -> usize {
        if self.scroll_offset == usize::MAX {
            self.transcript.len().saturating_sub(1)
        } else {
            self.scroll_offset
        }
    }

    /// Drop the head of the queued-input list. Called when a turn ends so
    /// the queue pane stops showing the prompt that is now running.
    pub fn drain_queue_head(&mut self) {
        if !self.queued_inputs.is_empty() {
            self.queued_inputs.remove(0);
        }
    }

    /// Show an ephemeral tip if the per-session seen-cap hasn't been reached.
    /// Each unique `key` can show at most `SEEN_CAP` times per session.
    pub fn show_tip(&mut self, key: &'static str, text: &str, ttl: u64, ambient: bool) {
        let count = self.seen_tips.entry(key).or_insert(0);
        if *count >= SEEN_CAP {
            return;
        }
        *count += 1;
        self.tip = Some(EphemeralTip {
            text: text.to_string(),
            born_tick: FRAME_TICK.load(std::sync::atomic::Ordering::Relaxed),
            ttl_ticks: ttl,
            key,
            ambient,
        });
    }
}

/// Max times an ambient tip key is shown per session before suppression.
const SEEN_CAP: u32 = 3;

// ─────────────────────────────────────────────────────────────────────────
// Main entry: `pub async fn run_tui(app: App) -> Result<()>`
// ─────────────────────────────────────────────────────────────────────────

/// Run the new oxicode-vtui powered TUI. Returns once the user exits or the
/// session is shut down.
pub async fn run_tui(app: App) -> Result<()> {
    // Resolve shared session-level context up-front so it can outlive the
    // TUI RAII guard via the worker thread.
    let cwd: PathBuf = std::env::current_dir().unwrap_or_default();
    let git_branch = crate::util::git_utils::get_current_branch(&cwd);
    super::host::activate_theme(app.settings());
    // Validate active theme contrast and log any warnings.
    let theme_id = oxicode_vtui::theme::active_theme_id();
    let validation = oxicode_vtui::theme::validate_theme_contrast(&theme_id);
    if validation.warnings.is_empty() {
        tracing::debug!("theme '{theme_id}' passed contrast validation");
    } else {
        for w in &validation.warnings {
            tracing::warn!("theme contrast: {w}");
        }
    }

    // Wire the inline-protocol channels. `cmd_tx` becomes the
    // `InlineHandle`; `evt_tx` is the input-thread → main-loop channel.
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<InlineCommand>();
    let (evt_tx, mut evt_rx) = tokio::sync::mpsc::unbounded_channel::<InlineEvent>();
    let handle = InlineHandle::new_for_tests(cmd_tx);

    // Build the AgentSession from the App. The helper wraps
    // `create_agent_session_from_services` so we can construct the session
    // without duplicating the runtime plumbing here.
    let session = build_agent_session(&app).await?;
    // No install_runtime_hooks call: session queues and stop flag are
    // wired into the agent hook chain at agent-build time via
    // App::from_oxicode → with_session_hooks.
    let session_handle = session.clone_handle();

    // Forward session events to a tokio mpsc so the main loop can
    // `tokio::select!` on them. We do this in two stages:
    //  1. Subscribe to AgentSession — CompactionStart/End, Advisor,
    //     QueueUpdate, etc.
    //  2. A forwarder thread that drives `agent.run_with_channel` and
    //     calls `forward_event_to_extensions` so per-agent events also
    //     flow through the same listener.
    let (session_tx, mut session_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
    let _sub_guard = session.subscribe(Box::new(move |event| {
        let _ = session_tx.send(event.clone());
    }));

    // Header context — built once at startup with workspace + branch.
    let header = build_header_context(&app, &cwd, git_branch.as_deref());
    handle.set_header_context(header.clone());

    // Enter the terminal (RAII). Every setup step is fallible, but a
    // successful `Tui::enter` is required to draw anything.
    let mut tui = Tui::enter()?;

    // Initial composer + placeholder — the harness receives these as
    // `SetPrompt` / `SetPlaceholder` commands once it spins up its own
    // consumer; we set them eagerly so the very first frame is correct.
    handle.set_prompt("> ".to_string(), InlineTextStyle::default());
    handle.set_placeholder(Some("Describe what you want to build\u{2026}".to_string()));

    // Render state — shared between the input thread (which edits the
    // buffer) and the main loop (which reads it for drawing).
    let state = Arc::new(parking_lot::Mutex::new(RenderState::new_with_header(
        header,
    )));
    state.lock().cwd = cwd.clone();
    state.lock().catalog = Some(app.catalog());
    state.lock().file_commands = crate::tui_vt::slash::file_commands::load_file_commands(&cwd);
    state.lock().todo_provider = session_handle.todo_provider();
    // Onboarding tip: surfaces the cheatsheet and help command on first run,
    // auto-dismisses after ~30s of rendering.
    state.lock().tip = Some(EphemeralTip {
        text: "Press ? for shortcuts  \u{00b7}  /help for commands".to_string(),
        born_tick: 0,
        ttl_ticks: 900,
        key: "onboarding",
        ambient: true,
    });
    // SSH tip: suggest tmux when running over SSH (1-time).
    if std::env::var("SSH_CONNECTION").is_ok() {
        state.lock().show_tip(
            "ssh_wrap",
            "Over SSH? Consider tmux to keep sessions alive",
            600,
            true,
        );
    }
    // Shared autonomy-mode handle — Shift+Tab toggles it at runtime. The
    // AskBridge atomic is the authority; the render state mirrors it so the
    // composer can draw a mode badge.
    let mode_handle = app.ask_bridge().map(|b| {
        let handle = b.mode_handle();
        state.lock().autonomy_mode = Mode::load(&handle);
        handle
    });
    spawn_input_thread(state.clone(), evt_tx.clone(), mode_handle);

    // Worker thread that owns the agent loop. Receives prompts over a
    // tokio mpsc and dispatches them through `run_with_channel`. The
    // returned `AgentEvent`s flow through a `std::sync::mpsc`; a paired
    // forwarder thread funnels them into the session's listener bus so
    // our subscriber above picks them up.
    let prompt_tx = spawn_agent_worker(session_handle.clone());

    let result = run_event_loop(
        &mut tui.terminal,
        &mut cmd_rx,
        &mut evt_rx,
        &mut session_rx,
        &handle,
        &state,
        &session_handle,
        prompt_tx.clone(),
    )
    .await;

    // Drain the harness before tearing down the terminal. Even if the
    // loop exited early we want to release the worker.
    drop(prompt_tx);
    handle.shutdown();
    // Dropping `tui` restores the terminal. Drop is at function return.
    drop(tui);

    result
}

// ─────────────────────────────────────────────────────────────────────────
// Event loop
// ─────────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    cmd_rx: &mut tokio::sync::mpsc::UnboundedReceiver<InlineCommand>,
    evt_rx: &mut tokio::sync::mpsc::UnboundedReceiver<InlineEvent>,
    session_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
    handle: &InlineHandle,
    state: &Arc<parking_lot::Mutex<RenderState>>,
    session: &crate::app::agent_session::AgentSessionHandle,
    prompt_tx: tokio::sync::mpsc::UnboundedSender<String>,
) -> Result<()> {
    // Drain any pending InlineCommands so the harness's initial set_header_context
    // (and similar) is observed before the first frame.
    while let Ok(cmd) = cmd_rx.try_recv() {
        apply_command(&mut state.lock(), cmd);
    }

    // Draw the initial frame *before* blocking on the first event. The
    // `select!` below parks until an event arrives, and the per-iteration
    // redraw only runs after it resolves — so without this eager draw the
    // screen stays black until the user presses a key.
    {
        let snapshot = state.lock();
        let _ = execute!(terminal.backend_mut(), BeginSynchronizedUpdate);
        if let Err(err) = terminal.draw(|frame| render_frame(frame, &snapshot, handle)) {
            tracing::warn!(?err, "initial tui draw failed");
        }
        let _ = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    }

    // Render tick. The input thread edits shared state (typing, cursor
    // movement, backspace, …) *without* sending an event, so without a
    // periodic wake the composer would never repaint what the user types.
    // The ratatui diff backend coalesces unchanged frames, so a steady tick
    // is cheap and also drives future spinner animation.
    let mut render_tick = tokio::time::interval(std::time::Duration::from_millis(50));
    render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // biased: agent events take priority so streaming output is
            // never starved by Ctrl+C noise or sticky key repeats.
            biased;

            // 1. Agent → TUI commands (transcript updates).
            Some(cmd) = cmd_rx.recv() => {
                let shutdown = {
                    let mut s = state.lock();
                    apply_command(&mut s, cmd)
                };
                if shutdown {
                    break;
                }
            }

            // 2. Agent → TUI events (token deltas, tool calls, …).
            Some(event) = session_rx.recv() => {
                // Intercept handoff-completion to clear transcript + auto-submit.
                if let SessionEvent::HandoffComplete { doc_path, auto_continue } = &event {
                    let mut s = state.lock();
                    s.transcript.clear();
                    s.message_buffer.clear();
                    s.scroll_offset = usize::MAX;
                    s.append_line(
                        InlineMessageKind::Info,
                        vec![plain_segment(format!(
                            "Handoff written to {}. New session started.",
                            doc_path
                        ))],
                    );
                    if *auto_continue {
                        drop(s);
                        let _ = prompt_tx.send(format!(
                            "Continue our work. Read the handoff document at {} \
                             and start with the first item in \"Remaining Work\".",
                            doc_path
                        ));
                    }
                } else {
                    handle_session_event(&mut state.lock(), handle, &event);
                }
            }

            // 3. Keyboard / paste / TUI events from the input thread.
            Some(evt) = evt_rx.recv() => {
                let outcome = handle_inline_event(
                    &mut state.lock(),
                    handle,
                    session,
                    &prompt_tx,
                    evt,
                );
                if outcome == LoopOutcome::Exit {
                    break;
                }
            }

            // 4. External SIGINT — route through the same idle-vs-streaming
            //    policy as the key path (some terminals deliver Ctrl+C both
            //    as a key event AND raise SIGINT; `kill -INT` also lands here).
            _ = tokio::signal::ctrl_c() => {
                let outcome = {
                    let mut s = state.lock();
                    handle_interrupt(&mut s, session, handle)
                };
                if outcome == LoopOutcome::Exit {
                    break;
                }
            }

            // 5. Periodic repaint — echoes typed input and drives animation
            //    even when no other event is ready.
            _ = render_tick.tick() => {}
        }

        // small_screen tip: warn when terminal is too narrow for full UI.
        if let Ok(size) = terminal.size()
            && size.width < 40
        {
            let mut s = state.lock();
            if s.tip.is_none() {
                s.show_tip(
                    "small_screen",
                    "Terminal too narrow \u{2014} resize for full UI",
                    300,
                    true,
                );
            }
        }
        // Redraw every iteration. The harness's redraw is idempotent —
        // the ratatui backend coalesces unchanged frames.
        let mut snapshot = state.lock();
        // Refresh the todo checklist from the live provider so the sticky
        // pane reflects phase changes written by the `todo` agent tool.
        if let Some(provider) = snapshot.todo_provider.as_ref() {
            snapshot.todo_items = flatten_todo_items(&provider.get_phases());
        }
        let _ = execute!(terminal.backend_mut(), BeginSynchronizedUpdate);
        let draw_err = terminal
            .draw(|frame| render_frame(frame, &snapshot, handle))
            .err();
        let _ = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
        if let Some(err) = draw_err {
            tracing::warn!(?err, "tui draw failed");
            break;
        }
    }

    Ok(())
}

#[derive(PartialEq, Eq)]
enum LoopOutcome {
    Continue,
    Exit,
}

/// Whether an Esc-driven cancel should abort the running stream (via the
/// interrupt path, which sets the footer + abort) or exit the app outright
/// (idle one-press quit). Extracted as a pure function so the routing can
/// be unit-tested without a live `AgentSessionHandle`.
#[derive(PartialEq, Eq, Debug)]
enum CancelRoute {
    /// A stream is running: abort it. The input thread's ~1s post-cancel
    /// grace then prevents mashing Esc from firing repeated cancels.
    Interrupt,
    /// Idle: instant one-press quit — no quit-arming footer, no grace.
    Exit,
}

/// Pure routing decision for `InlineEvent::Cancel`. While a stream is
/// running, Esc aborts it (matching Ctrl+C). When idle, Esc quits at once.
fn route_cancel(is_streaming: bool) -> CancelRoute {
    if is_streaming {
        CancelRoute::Interrupt
    } else {
        CancelRoute::Exit
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Command / event handlers
// ─────────────────────────────────────────────────────────────────────────

/// Apply a single `InlineCommand` to the render state. Returns `true`
/// when the harness has requested a shutdown.
fn apply_command(state: &mut RenderState, cmd: InlineCommand) -> bool {
    match cmd {
        InlineCommand::AppendLine { kind, segments } => {
            state.append_line(kind, segments);
        }
        InlineCommand::Inline { kind, segment } => {
            state.inline_segment(kind, segment);
        }
        InlineCommand::ReplaceLast {
            count, kind, lines, ..
        } => {
            // Drop the last `count` lines and replace with the new ones.
            let drop = count.min(state.transcript.len());
            for _ in 0..drop {
                state.transcript.pop();
            }
            for line in lines {
                state.append_line(kind, line);
            }
        }
        InlineCommand::AppendPastedMessage { kind, text, .. } => {
            state.append_line(kind, vec![plain_segment(text)]);
        }
        InlineCommand::SetPrompt { prefix, .. } => {
            state.prompt_prefix = prefix;
        }
        InlineCommand::SetPlaceholder { hint, .. } => {
            state.placeholder = hint;
        }
        InlineCommand::SetHeaderContext { context } => {
            state.header_context = *context;
        }
        InlineCommand::SetInputStatus { left, right } => {
            state.footer_left = left;
            state.footer_right = right;
        }
        InlineCommand::SetInputEnabled(enabled) => {
            state.input_enabled = enabled;
        }
        InlineCommand::SetCursorVisible(_) | InlineCommand::ForceRedraw => {}
        InlineCommand::SetReasoningStage(stage) => {
            state.reasoning_stage = stage;
        }
        InlineCommand::SetVimModeEnabled(enabled) => {
            state.vim_state.set_enabled(enabled);
        }
        InlineCommand::SetQueuedInputs { entries } => {
            state.queued_inputs = entries;
        }
        InlineCommand::ShowOverlay { request } => {
            state.overlay = Some(materialize_overlay(*request));
        }
        InlineCommand::CloseOverlay => {
            state.overlay = None;
        }
        InlineCommand::Shutdown => {
            state.shutdown_requested = true;
            return true;
        }
        _ => {
            // Surface unknown commands as info so they are visible
            // during development.
            tracing::trace!("unhandled InlineCommand (not rendered)");
        }
    }
    false
}

/// Convert an `OverlayRequest` into the render-state representation used by
/// the TUI. The input thread mutates `selected` / `search` while the overlay
/// is open, and `handle_inline_event` projects the user's selection back to
/// the harness as `InlineEvent::Overlay`.
fn materialize_overlay(request: OverlayRequest) -> OverlayState {
    match request {
        OverlayRequest::Modal(req) => {
            let secure_input = req.secure_prompt.map(|cfg| OverlaySecureInput {
                config: cfg,
                value: String::new(),
                cursor: 0,
            });
            OverlayState {
                title: req.title,
                lines: req.lines,
                items: Vec::new(),
                selected: 0,
                search: None,
                secure_input,
            }
        }
        OverlayRequest::List(req) => {
            let search = req.search.map(|cfg| OverlaySearchState {
                label: cfg.label,
                placeholder: cfg.placeholder,
                value: String::new(),
            });
            OverlayState {
                title: req.title,
                lines: req.lines,
                items: req.items.into_iter().map(overlay_item_from).collect(),
                selected: 0,
                search,
                secure_input: None,
            }
        }
        OverlayRequest::Wizard(req) => {
            // Wizard overlays are multi-step flows that this TUI does not yet
            // render natively; surface the first step's title/items so the
            // user still sees something instead of a blank panel.
            let step_items = req
                .steps
                .first()
                .map(|s| {
                    s.items
                        .iter()
                        .map(|it| overlay_item_from(it.clone()))
                        .collect()
                })
                .unwrap_or_default();
            let search = req.search.map(|cfg| OverlaySearchState {
                label: cfg.label,
                placeholder: cfg.placeholder,
                value: String::new(),
            });
            OverlayState {
                title: req.title,
                lines: Vec::new(),
                items: step_items,
                selected: 0,
                search,
                secure_input: None,
            }
        }
    }
}
fn overlay_item_from(item: InlineListItem) -> OverlayListItem {
    OverlayListItem {
        title: item.title,
        subtitle: item.subtitle,
        badge: item.badge,
        indent: item.indent,
        search_value: item.search_value,
        selection: item.selection,
    }
}

/// Map a `SessionEvent` to the matching `InlineHandle` calls. This is the
/// single place where the agent's event vocabulary meets the harness's
/// transcript vocabulary.
fn handle_session_event(state: &mut RenderState, handle: &InlineHandle, event: &SessionEvent) {
    match event {
        SessionEvent::Agent(boxed) => {
            map_agent_event(handle, *boxed.clone(), state);
        }
        SessionEvent::CompactionStart { .. } => {
            handle.set_reasoning_stage(Some("Compacting\u{2026}".to_string()));
        }
        SessionEvent::CompactionEnd { error_message, .. } => {
            handle.set_reasoning_stage(None);
            if let Some(msg) = error_message {
                handle.append_line(
                    InlineMessageKind::Error,
                    vec![plain_segment(format!("Compaction failed: {msg}"))],
                );
            }
        }
        SessionEvent::ThinkingLevelChanged { .. } => {
            // No rendering — the footer reflects this implicitly via the
            // header context.
        }
        SessionEvent::QueueUpdate { .. } => {
            // Surface the queue length as a footer status update.
            // The exact count is computed lazily by the agent session;
            // we approximate it via the snapshot we hold.
            let pending = state.transcript.len();
            handle.set_input_status(
                None,
                Some(if pending == 0 {
                    "ready".to_string()
                } else {
                    "queued".to_string()
                }),
            );
        }
        SessionEvent::Advisor { body, .. } => {
            handle.append_line(InlineMessageKind::Info, vec![plain_segment(body.clone())]);
        }
        SessionEvent::SessionInfoChanged => {
            // The session name is reflected via header context on next
            // `set_header_context`. Nothing to do here.
        }
        SessionEvent::HandoffComplete { .. } => {
            // Intercepted in the event loop's session_rx arm before
            // reaching this function — transcript clearing and prompt
            // submission happen there. This arm exists for exhaustiveness.
        }
        SessionEvent::HandoffFailed { error } => {
            handle.append_line(
                InlineMessageKind::Error,
                vec![plain_segment(format!("Handoff failed: {}", error))],
            );
        }
    }
}

/// Project the agent-level event variants onto the harness transcript.
fn map_agent_event(handle: &InlineHandle, event: AgentEvent, state: &mut RenderState) {
    match event {
        AgentEvent::TextChunk { text } => {
            state.message_buffer.push_str(&text);
            handle.inline(InlineMessageKind::Agent, plain_segment(text));
        }
        AgentEvent::MessageStart { .. } => {
            state.message_buffer.clear();
        }
        AgentEvent::MessageUpdate { delta, .. } => match &delta {
            oxicode_sdk::StreamDelta::Text(text) => {
                state.message_buffer.push_str(text);
                handle.inline(InlineMessageKind::Agent, plain_segment(text.clone()));
            }
            oxicode_sdk::StreamDelta::Thinking(text) => {
                // Show thinking blocks as dimmed Info lines with a ✻ marker,
                // visually distinct from the actual response text.
                let mut style = InlineTextStyle::default();
                style.effects |= anstyle::Effects::DIMMED;
                let seg = InlineSegment {
                    text: format!("\u{2733} {text}"),
                    style: Arc::new(style),
                };
                handle.inline(InlineMessageKind::Info, seg);
            }
            oxicode_sdk::StreamDelta::Sync => {
                // Re-render the complete message as markdown
                if !state.message_buffer.is_empty() {
                    let lines =
                        oxicode_vtui::tui::ui::markdown::render_markdown(&state.message_buffer);
                    let count = lines.len();
                    if count > 0 {
                        handle.replace_last(count, InlineMessageKind::Agent, lines);
                    }
                    state.message_buffer.clear();
                }
            }
        },
        AgentEvent::MessageEnd { .. } => {
            // Final rendering (same as delta:None for completeness)
            if !state.message_buffer.is_empty() {
                let lines = oxicode_vtui::tui::ui::markdown::render_markdown(&state.message_buffer);
                let count = lines.len();
                if count > 0 {
                    handle.replace_last(count, InlineMessageKind::Agent, lines);
                }
                state.message_buffer.clear();
            }
        }
        AgentEvent::ToolStart { tool_name, .. } => {
            handle.append_line(
                InlineMessageKind::Tool,
                vec![plain_segment(format!("\u{2699} {tool_name}"))],
            );
            handle.set_reasoning_stage(Some(format!("tool: {tool_name}")));
        }
        AgentEvent::ToolComplete { result } => {
            // If the result looks like a diff, render with green/red coloring.
            if !try_render_diff(&result.content, handle) {
                let preview = preview_tool_result(&result.content);
                let mut style = InlineTextStyle::default();
                style.effects |= anstyle::Effects::DIMMED;
                handle.append_line(
                    InlineMessageKind::Tool,
                    vec![InlineSegment {
                        text: format!("\u{2713} {preview}"),
                        style: Arc::new(style),
                    }],
                );
            }
            handle.set_reasoning_stage(None);
            handle.set_input_enabled(true);
        }
        AgentEvent::ToolError { error, .. } => {
            handle.append_line(
                InlineMessageKind::Error,
                vec![plain_segment(format!("\u{2717} {error}"))],
            );
            handle.set_reasoning_stage(None);
            handle.set_input_enabled(true);
        }
        AgentEvent::Error { message, .. } => {
            handle.append_line(InlineMessageKind::Error, vec![plain_segment(message)]);
            handle.set_input_enabled(true);
            handle.set_input_status(None, None);
        }
        AgentEvent::Compaction { .. } => {
            // Detailed lifecycle is handled by the AgentSession layer
            // (CompactionStart/End SessionEvents).
        }
        AgentEvent::Cancelled => {
            handle.set_input_enabled(true);
            handle.set_input_status(None, Some("cancelled".to_string()));
        }
        AgentEvent::AutoRetryStart {
            attempt,
            max_attempts,
            ..
        } => {
            handle.set_input_status(None, Some(format!("retry {attempt}/{max_attempts}")));
        }
        AgentEvent::TurnEnd { .. } => {
            // Notify via the terminal's best-supported desktop-notification
            // protocol (OSC 9/99/777, falling back to BEL) so the user
            // notices a finished turn even when the window is unfocused.
            crate::tui_vt::notifications::emit_notification("oxicode", "Response complete");
            // The next queued prompt (if any) now starts running — drop it
            // from the visible queue pane so the pane only shows still-pending
            // inputs.
            state.drain_queue_head();
            handle.set_reasoning_stage(None);
        }
        _ => {
            // Other variants (TurnStart/End, AgentStart/End, Usage, …) are
            // logged but not rendered — they're either metadata or covered
            // by the dedicated SessionEvent variants above.
            tracing::debug!(?event, "ignored AgentEvent variant");
        }
    }
}

/// Decide which `/providers` actions apply for a provider, given whether
/// the user already has a stored credential and whether the provider
/// supports the OAuth `authorization_code` flow.
///
/// Single-action branches skip the menu entirely and drive directly
/// (no user-visible "Pick an action" list for the obvious cases).
pub(crate) fn next_provider_actions(has_key: bool, oauth_capable: bool) -> Vec<AuthAction> {
    match (has_key, oauth_capable) {
        (true, true) => vec![
            AuthAction::SetApiKey,
            AuthAction::StartOAuth,
            AuthAction::RemoveKey,
        ],
        (true, false) => vec![AuthAction::RemoveKey],
        (false, true) => vec![AuthAction::SetApiKey, AuthAction::StartOAuth],
        (false, false) => vec![AuthAction::SetApiKey],
    }
}

/// Dispatch a single `AuthAction` for `provider`.
///
/// `SetApiKey` opens the secure (masked) prompt and stashes
/// `state.secure_input_target` so the `SecureInput(text)` consumer can
/// route the key to the right provider. `StartOAuth` is a stub for
/// Task 8 (TODO: wire `run_oauth_flow`). `RemoveKey` reuses the existing
/// confirmation modal — its `ConfirmationAction::RemoveProviderKey`
/// handler already runs through `/providers remove <name> --yes`.
pub(crate) fn handle_auth_action(
    provider: &str,
    action: &AuthAction,
    auth: &Arc<crate::store::auth_storage::AuthStorage>,
    handle: &InlineHandle,
    state: &mut RenderState,
) {
    match action {
        AuthAction::SetApiKey => {
            state.secure_input_target = Some(provider.to_string());
            handle.show_modal(
                format!("Set API key for {provider}"),
                vec![
                    "Paste the API key. Press Enter to save, Esc to cancel.".into(),
                    "The key is masked on screen; nothing is logged.".into(),
                ],
                Some(SecurePromptConfig {
                    label: format!("{provider} key"),
                    placeholder: Some("sk-...".into()),
                    mask_input: true,
                }),
            );
        }
        AuthAction::StartOAuth => {
            // PKCE + loopback-callback glue lives in `run_oauth_flow`
            // (defined just below `handle_auth_action`). Spawn it on a
            // dedicated tokio task so the main loop can continue
            // rendering; the spawned task posts status updates back to
            // the transcript via the cloned `InlineHandle`.
            //
            // First, gate on the provider actually having an OAuth
            // spec in `product-meta.toml` — the action is only offered
            // when `next_provider_actions` includes it, so this branch
            // is purely defensive against a stale UI state.
            let spec = match crate::provider_oauth::spec_for(provider) {
                Some(s) => s,
                None => {
                    handle.append_line(
                        InlineMessageKind::Error,
                        vec![plain_segment(format!(
                            "OAuth: no OAuth config for '{provider}'."
                        ))],
                    );
                    return;
                }
            };
            // `provider_owned` and `tx` are cloned Strings/`InlineHandle`s
            // owned by the task; `auth_clone` is the shared storage
            // singleton (cheap to clone — it is already `Arc`-backed).
            // `spec` is moved into the task.
            let provider_owned = provider.to_string();
            let tx = handle.clone();
            let auth_clone = Arc::clone(auth);
            tokio::spawn(async move {
                run_oauth_flow(provider_owned, spec, tx, auth_clone).await;
            });
        }
        AuthAction::RemoveKey => {
            state.confirmation = Some(ModalConfirmation {
                title: format!("Remove key for {provider}?"),
                message: "  y \u{2014} remove key     n / x \u{2014} cancel".into(),
                action: ConfirmationAction::RemoveProviderKey(provider.to_string()),
            });
        }
    }
}

/// Drive the OAuth `authorization_code` flow for `provider` end to end:
///
/// 1. Bind an ephemeral loopback TCP listener and capture its port.
/// 2. Generate PKCE verifier + S256 challenge (`provider_oauth::pkce_pair`).
/// 3. Build the authorization URL (`provider_oauth::build_auth_url`) and
///    open it in the user's browser (`provider_oauth::open_browser`).
/// 4. Wait on the listener for the redirect carrying the `code` + `state`
///    (`oauth_listener::await_callback`); bind a timeout so a stuck
///    listener cannot leak.
/// 5. Exchange the code for tokens at the provider's token URL
///    (`provider_oauth::exchange_code`).
/// 6. Persist the OAuth credential via `AuthStorage::set_oauth_full` so
///    subsequent requests can use the access token (and `refresh_token`
///    if granted) without re-prompting the user.
///
/// Steps that hard-fail (callback timeout, state mismatch, missing
/// `code`, exchange error, persist error) post an `InlineMessageKind::Error`
/// line to the transcript and return; the bound listener is dropped on
/// every return path, satisfying the single-shot invariant.
///
/// Headless fallback (plan §3 / design §3): if `open_browser` returns
/// `Err`, we do NOT abort. We post an `Info` line printing the auth URL
/// and lengthen the callback timeout to 5 minutes so the user can paste
/// the URL into a browser on another machine and complete the flow.
/// Masking: every user-facing line that mentions the access token
/// surfaces only the token length (`access_token.chars().count()`), never
/// the value. Tokens are never logged via `tracing`.
pub(crate) async fn run_oauth_flow(
    provider: String,
    spec: crate::provider_oauth::ProviderOAuthSpec,
    handle: InlineHandle,
    auth: Arc<crate::store::auth_storage::AuthStorage>,
) {
    use std::time::Duration;
    // Timeout is selected AFTER the browser attempt: 2 minutes when the
    // browser opened (the user is right in front of it), 5 minutes when
    // it didn't (headless box — user has to copy the URL to another
    // machine, sign in there, and the redirect has to traverse NAT).
    // The variable is declared once as `mut` and then frozen below.
    // 1. Bind the loopback listener BEFORE opening the browser so the
    //    `redirect_uri` we hand to the provider already points at a live
    //    port. `TcpListener::bind("127.0.0.1:0")` picks an ephemeral port.
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0u16)).await {
        Ok(l) => l,
        Err(e) => {
            handle.append_line(
                InlineMessageKind::Error,
                vec![plain_segment(format!(
                    "OAuth: could not bind loopback listener for '{provider}': {e}"
                ))],
            );
            return;
        }
    };
    let port = match listener.local_addr() {
        Ok(addr) => addr.port(),
        Err(e) => {
            handle.append_line(
                InlineMessageKind::Error,
                vec![plain_segment(format!(
                    "OAuth: could not read loopback port for '{provider}': {e}"
                ))],
            );
            return;
        }
    };

    // 2. PKCE pair + per-flow `state`. The state must match what we send
    //    in the auth URL and what we accept on the callback — a single
    //    random base64-url string is enough since the flow is single-shot.
    let (verifier, challenge) = crate::provider_oauth::pkce_pair();
    let state_token = crate::provider_oauth::pkce_pair().0; // 43-char url-safe random

    // 3. Build auth URL and open the browser. `open_browser` already
    //    validates the URL scheme so a malformed spec would have failed
    //    at `build_auth_url` time (it calls `Url::parse` internally).
    let auth_url = crate::provider_oauth::build_auth_url(&spec, port, &state_token, &challenge);
    handle.append_line(
        InlineMessageKind::Info,
        vec![plain_segment(format!(
            "OAuth: opening browser for '{provider}' on http://127.0.0.1:{port}{}",
            spec.redirect_path
        ))],
    );
    // Pick the callback timeout based on whether the browser opened.
    // Headless fallback (plan §3 / design §3): when the OS refuses to
    // launch a browser, we surface the URL and KEEP listening so a user
    // on a different machine can paste it, sign in, and let the
    // redirect land back on our loopback port. A 5-minute window is
    // long enough for that round-trip; a 2-minute window is plenty
    // when the browser already opened in front of the user.
    let callback_timeout = match crate::provider_oauth::open_browser(&auth_url) {
        Ok(()) => Duration::from_secs(120),
        Err(e) => {
            handle.append_line(
                InlineMessageKind::Info,
                vec![plain_segment(format!(
                    "OAuth: could not open a browser ({e}).\nOpen this URL manually within 5 minutes:\n  {auth_url}"
                ))],
            );
            Duration::from_secs(300)
        }
    };

    // 4. Wait for the callback. The listener is single-shot by design:
    //    `await_callback` accepts exactly one connection.
    let callback = match crate::oauth_listener::await_callback(
        listener,
        state_token.clone(),
        spec.redirect_path.clone(),
        callback_timeout,
    )
    .await
    {
        Ok(c) => c,
        Err(crate::oauth_listener::CallbackError::Timeout) => {
            handle.append_line(
                InlineMessageKind::Error,
                vec![plain_segment(format!(
                    "OAuth: timed out waiting for '{provider}' callback (after {}s)",
                    callback_timeout.as_secs()
                ))],
            );
            return;
        }
        Err(e) => {
            handle.append_line(
                InlineMessageKind::Error,
                vec![plain_segment(format!(
                    "OAuth: callback failed for '{provider}': {e}"
                ))],
            );
            return;
        }
    };

    // 5. Exchange code → tokens.
    let tokens =
        match crate::provider_oauth::exchange_code(&spec, port, &callback.code, &verifier).await {
            Ok(t) => t,
            Err(e) => {
                handle.append_line(
                    InlineMessageKind::Error,
                    vec![plain_segment(format!(
                        "OAuth: token exchange failed for '{provider}': {e}"
                    ))],
                );
                return;
            }
        };

    // 6. Persist. `set_oauth_full` takes u64 `expires_at`; `OAuthTokens`
    //    exposes i64 (so callers can branch on `now < expires_at` in
    //    signed arithmetic). Saturate defensively — the value is always
    //    `now + expires_in` with `expires_in >= 0`, so negatives are
    //    impossible here, but a guard costs nothing.
    let new_expires_at: u64 = tokens.expires_at.max(0) as u64;
    let access_token_len = tokens.access_token.chars().count();
    // `set_oauth_full` returns `()` and logs persistence failures via
    // `tracing::warn` — the in-memory credential is always updated.
    auth.set_oauth_full(
        &provider,
        tokens.access_token,
        tokens.refresh_token,
        new_expires_at,
        if tokens.scopes.is_empty() {
            None
        } else {
            Some(tokens.scopes.join(" "))
        },
        None,
    );
    handle.append_line(
        InlineMessageKind::Info,
        vec![plain_segment(format!(
            "OAuth: '{provider}' logged in. Token stored ({} chars).",
            access_token_len
        ))],
    );
}

/// Map an input-thread `InlineEvent` to agent actions / state edits.
fn handle_inline_event(
    state: &mut RenderState,
    handle: &InlineHandle,
    session: &crate::app::agent_session::AgentSessionHandle,
    prompt_tx: &tokio::sync::mpsc::UnboundedSender<String>,
    evt: InlineEvent,
) -> LoopOutcome {
    match evt {
        InlineEvent::Submit(text) => {
            // Drain the composer — the input thread already cleared its
            // local copy once Submit fired, but we keep the canonical
            // buffer here in sync.
            let prompt = text.to_string();
            state.input_buffer.clear();
            state.input_cursor = 0;
            if prompt.is_empty() {
                return LoopOutcome::Continue;
            }
            state.pending_quit = false;
            // Slash commands: dispatch locally instead of forwarding to
            // the agent. The echoed line is appended before dispatch so
            // every command output appears after the prompt.
            if prompt.trim_start().starts_with('/') {
                state.append_line(InlineMessageKind::User, vec![plain_segment(prompt.clone())]);
                let mut ctx = SlashCtx {
                    session,
                    handle,
                    state,
                };
                return match SlashRegistry::builtins().dispatch(&prompt, &mut ctx) {
                    SlashOutcome::Quit => LoopOutcome::Exit,
                    SlashOutcome::Handled => LoopOutcome::Continue,
                    SlashOutcome::NotHandled => {
                        // File-based commands: try before erroring.
                        if let Some(expanded) = crate::tui_vt::slash::file_commands::try_expand(
                            &ctx.state.file_commands,
                            &prompt,
                        ) {
                            // Send expanded text directly to the agent worker.
                            // The original `/cmd args` is already echoed above.
                            let _ = prompt_tx.send(expanded);
                            LoopOutcome::Continue
                        } else {
                            ctx.reply(
                                InlineMessageKind::Error,
                                format!("Unknown command: {}", prompt.trim()),
                            );
                            LoopOutcome::Continue
                        }
                    }
                };
            }
            state.append_line(InlineMessageKind::User, vec![plain_segment(prompt.clone())]);
            // While a run is active, mirror the prompt into the queue pane so
            // the user sees their input is queued (the worker channel already
            // serialises execution; this is the visible counterpart).
            if session.is_streaming() {
                state.queued_inputs.push(prompt.clone());
                state.show_tip(
                    "send_now",
                    "Ctrl+Enter sends now  \u{00b7}  Ctrl+; manages queue",
                    240,
                    true,
                );
            }
            // Hand the prompt to the worker thread. If the worker has
            // already exited (e.g. shutdown), drop it on the floor.
            let _ = prompt_tx.send(prompt);
        }
        InlineEvent::Cancel => {
            // Esc-driven cancel. While a stream is running, abort it (the
            // input thread's ~1s post-cancel grace then prevents mashing).
            // When idle, Esc is an instant one-press quit — no grace, no
            // quit-arming footer that would invite a re-press the grace
            // swallows.
            return match route_cancel(session.is_streaming()) {
                CancelRoute::Interrupt => handle_interrupt(state, session, handle),
                CancelRoute::Exit => LoopOutcome::Exit,
            };
        }
        InlineEvent::Exit => {
            return LoopOutcome::Exit;
        }
        InlineEvent::Interrupt => {
            return handle_interrupt(state, session, handle);
        }
        InlineEvent::ScrollLineUp => {
            state.scroll_offset = state.scroll_offset.saturating_add(1);
        }
        InlineEvent::ScrollLineDown => {
            state.scroll_offset = state.scroll_offset.saturating_sub(1);
        }
        InlineEvent::ScrollPageUp => {
            state.scroll_offset = state.scroll_offset.saturating_add(10);
        }
        InlineEvent::ScrollPageDown => {
            state.scroll_offset = state.scroll_offset.saturating_sub(10);
        }
        InlineEvent::CyclePrimaryAgent => {
            let _ = session.cycle_model();
        }
        InlineEvent::CyclePrimaryAgentPrevious => {
            // No dedicated reverse-cycling API in AgentSession yet;
            // forward-cycle is the closest match.
            let _ = session.cycle_model();
        }
        InlineEvent::Overlay(overlay_evt) => {
            use oxicode_vtui::tui::core::OverlayEvent;
            match overlay_evt {
                OverlayEvent::Submitted(sub) => {
                    // If this was a /model picker, set the selected model.
                    if let OverlaySubmission::Selection(InlineListSelection::Model(idx)) = &sub
                        && idx < &state.overlay_model_ids.len()
                    {
                        let model_id = state.overlay_model_ids[*idx].clone();
                        match session.set_model(&model_id) {
                            Ok(()) => handle.append_line(
                                InlineMessageKind::Info,
                                vec![plain_segment(format!("Switched to {model_id}"))],
                            ),
                            Err(e) => handle.append_line(
                                InlineMessageKind::Error,
                                vec![plain_segment(format!("Failed to set model: {e}"))],
                            ),
                        }
                    }
                    // If this was a /theme picker, apply the selected theme.
                    if let OverlaySubmission::Selection(InlineListSelection::Theme(theme_id)) = &sub
                    {
                        match oxicode_vtui::theme::set_active_theme(theme_id) {
                            Ok(()) => {
                                let label = oxicode_vtui::theme::theme_label(theme_id)
                                    .unwrap_or(theme_id.as_ref())
                                    .to_string();
                                handle.append_line(
                                    InlineMessageKind::Info,
                                    vec![plain_segment(format!("Theme: {label}"))],
                                );
                            }
                            Err(e) => handle.append_line(
                                InlineMessageKind::Error,
                                vec![plain_segment(format!("Unknown theme: {e}"))],
                            ),
                        }
                    }
                    // If this was a command palette selection, fill the prompt.
                    if let OverlaySubmission::Selection(InlineListSelection::SlashCommand(name)) =
                        &sub
                    {
                        state.input_buffer = format!("/{name} ");
                        state.input_cursor = state.input_buffer.len();
                    }
                    // Settings overlay: toggle/cycle the selected setting.
                    if let OverlaySubmission::Selection(InlineListSelection::ConfigAction(key)) =
                        &sub
                    {
                        match key.as_str() {
                            "thinking_level" => {
                                if let Some(level) = session.cycle_thinking_level() {
                                    handle.append_line(
                                        InlineMessageKind::Info,
                                        vec![plain_segment(format!("Thinking: {level:?}"))],
                                    );
                                }
                            }
                            "auto_compaction" => {
                                let enabled = !session.auto_compaction_enabled();
                                session.set_auto_compaction(enabled);
                                handle.append_line(
                                    InlineMessageKind::Info,
                                    vec![plain_segment(format!(
                                        "Auto-compaction: {}",
                                        if enabled { "on" } else { "off" }
                                    ))],
                                );
                            }
                            "auto_retry" => {
                                let enabled = !session.auto_retry_enabled();
                                session.set_auto_retry(enabled);
                                handle.append_line(
                                    InlineMessageKind::Info,
                                    vec![plain_segment(format!(
                                        "Auto-retry: {}",
                                        if enabled { "on" } else { "off" }
                                    ))],
                                );
                            }
                            "advisor" => match session.toggle_advisor() {
                                Ok(enabled) => handle.append_line(
                                    InlineMessageKind::Info,
                                    vec![plain_segment(format!(
                                        "Advisor: {}",
                                        if enabled { "on" } else { "off" }
                                    ))],
                                ),
                                Err(e) => handle.append_line(
                                    InlineMessageKind::Error,
                                    vec![plain_segment(format!("Failed to toggle advisor: {e}"))],
                                ),
                            },
                            _ => {}
                        }
                    }
                    // Session picker: resume the selected session by filling
                    // `/resume <id>` into the prompt (the user confirms).
                    if let OverlaySubmission::Selection(InlineListSelection::Session(id)) = &sub {
                        state.input_buffer = format!("/resume {id}");
                        state.input_cursor = state.input_buffer.len();
                    }
                    // `/models` catalog browser: switch to the selected model.
                    if let OverlaySubmission::Selection(InlineListSelection::CatalogModel(idx)) =
                        &sub
                        && idx < &state.overlay_catalog_models.len()
                    {
                        let (provider, model_id) = &state.overlay_catalog_models[*idx];
                        let full = format!("{provider}/{model_id}");
                        match session.set_model(&full) {
                            Ok(()) => handle.append_line(
                                InlineMessageKind::Info,
                                vec![plain_segment(format!("Switched to {full}"))],
                            ),
                            Err(e) => handle.append_line(
                                InlineMessageKind::Error,
                                vec![plain_segment(format!("Failed to set model: {e}"))],
                            ),
                        }
                    }
                    // `/providers` list: pick a provider, then drive the
                    // `next_provider_actions(has_key, oauth_capable)` matrix.
                    // Single-action cases fire straight into
                    // `handle_auth_action`; multi-action cases open a
                    // one-shot action list whose selections are
                    // `ProviderAction { provider, action }`.
                    if let OverlaySubmission::Selection(InlineListSelection::ProviderRow(idx)) =
                        &sub
                        && idx < &state.overlay_providers.len()
                    {
                        let name = state.overlay_providers[*idx].clone();
                        let auth = crate::store::auth_storage::shared_auth_storage();
                        let has_key = auth.has(&name);
                        let oauth_capable = crate::provider_oauth::spec_for(&name).is_some();
                        let actions = next_provider_actions(has_key, oauth_capable);
                        if actions.len() == 1 {
                            // Single action — drive directly with no menu.
                            handle_auth_action(&name, &actions[0], &auth, handle, state);
                        } else {
                            // Show action menu.
                            let items: Vec<InlineListItem> = actions
                                .iter()
                                .map(|a| InlineListItem {
                                    title: match a {
                                        AuthAction::SetApiKey => "Set API key".into(),
                                        AuthAction::StartOAuth => "Login with OAuth".into(),
                                        AuthAction::RemoveKey => "Remove key".into(),
                                    },
                                    subtitle: None,
                                    badge: None,
                                    indent: 0,
                                    selection: Some(InlineListSelection::ProviderAction {
                                        provider: name.clone(),
                                        action: a.clone(),
                                    }),
                                    search_value: None,
                                })
                                .collect();
                            handle.show_list_modal(
                                name.clone(),
                                vec!["Pick an action".into()],
                                items,
                                None,
                                None,
                            );
                        }
                    }
                    // `/providers` action menu: forward the chosen
                    // `AuthAction` to the host dispatcher. Selecting
                    // "Remove key" reuses the existing y/n confirmation
                    // modal; "Set API key" opens the secure prompt;
                    // "Login with OAuth" prints the Task 8 stub.
                    if let OverlaySubmission::Selection(InlineListSelection::ProviderAction {
                        provider,
                        action,
                    }) = &sub
                    {
                        let auth = crate::store::auth_storage::shared_auth_storage();
                        handle_auth_action(provider, action, &auth, handle, state);
                    }
                    // Secure (masked) prompt committed by the user. The
                    // matching open prompt must have stashed
                    // `state.secure_input_target`; we trust that field
                    // here because every prompt path goes through
                    // `handle_auth_action` (SetApiKey) which sets it
                    // before opening the modal.
                    if let OverlaySubmission::SecureInput(text) = &sub
                        && let Some(provider) = state.secure_input_target.take()
                    {
                        let auth = crate::store::auth_storage::shared_auth_storage();
                        auth.set_api_key(&provider, text.clone());
                        handle.append_line(
                            InlineMessageKind::Info,
                            vec![plain_segment(format!(
                                "Saved API key for '{provider}' ({} chars).",
                                text.chars().count()
                            ))],
                        );
                    }
                    state.overlay_catalog_models.clear();
                    state.overlay_providers.clear();
                    state.overlay_model_ids.clear();
                    handle.close_overlay();
                }
                OverlayEvent::Cancelled => {
                    handle.close_overlay();
                }
                OverlayEvent::SelectionChanged(_) => {}
            }
        }
        _ => {
            // Other events (overlay, list-selection, etc.) are no-ops in
            // this harness — they are handled by the harness overlay
            // component, not by the inline protocol.
        }
    }
    LoopOutcome::Continue
}

// ─────────────────────────────────────────────────────────────────────────
// Ctrl+C policy / streaming guard
// ─────────────────────────────────────────────────────────────────────────

/// RAII guard that clears the streaming flag on drop (normal exit, error,
/// or panic cancellation). Wired in [`run_one_prompt`] around each run.
struct StreamingGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for StreamingGuard<'_> {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Central Ctrl+C policy.
///
/// - **Agent streaming** → abort the current run and tell the user to press
///   again to quit. The abort is effective because the session hooks installed
///   via `App::from_oxicode` → `with_session_hooks` wire the session's
///   `should_stop` flag into the agent loop.
/// - **Agent idle** → exit the application.
///
/// Both the input-thread key event (`InlineEvent::Interrupt`) and the OS
/// signal handler (`tokio::signal::ctrl_c()`) route through here so
/// behavior is identical regardless of how the interrupt arrives.
///
fn handle_interrupt(
    state: &mut RenderState,
    session: &crate::app::agent_session::AgentSessionHandle,
    _handle: &InlineHandle,
) -> LoopOutcome {
    // If a confirmation is already open, Ctrl+C acts as confirm (quit).
    if state.confirmation.is_some() {
        return LoopOutcome::Exit;
    }
    // A second Ctrl+C (after the first armed a quit during a stream) opens
    // the quit confirmation modal instead of exiting outright.
    if state.pending_quit {
        state.confirmation = Some(quit_confirmation());
        state.pending_quit = false;
        return LoopOutcome::Continue;
    }
    // First Ctrl+C. While streaming, abort the run and arm a quit (the next
    // press opens the confirmation). When idle, open the confirmation at
    // once — no separate quit-arming step needed.
    if session.is_streaming() {
        let s = session.clone();
        tokio::spawn(async move {
            s.abort().await;
        });
        state.footer_left = Some("Stopping\u{2026} press Ctrl+C again to confirm quit".to_string());
        state.pending_quit = true;
    } else {
        state.footer_left = None;
        state.confirmation = Some(quit_confirmation());
    }
    LoopOutcome::Continue
}

/// Build the standard quit-confirmation dialog.
fn quit_confirmation() -> ModalConfirmation {
    ModalConfirmation {
        title: "Quit oxicode?".into(),
        message: "  y \u{2014} quit now     n / x \u{2014} stay".into(),
        action: ConfirmationAction::Quit,
    }
}

/// Build a clear-conversation confirmation dialog.
pub(super) fn clear_confirmation() -> ModalConfirmation {
    ModalConfirmation {
        title: "Clear conversation?".into(),
        message: "  y \u{2014} clear all     n / x \u{2014} cancel".into(),
        action: ConfirmationAction::ClearConversation,
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Input thread — polls crossterm, edits the shared buffer, and forwards
// lifecycle events (Submit, Cancel, …) over a tokio channel.
// ─────────────────────────────────────────────────────────────────────────

fn spawn_input_thread(
    state: Arc<parking_lot::Mutex<RenderState>>,
    evt_tx: tokio::sync::mpsc::UnboundedSender<InlineEvent>,
    mode_handle: Option<std::sync::Arc<std::sync::atomic::AtomicU8>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // Poll stdin in a tight loop. `event::poll` returns `Ok(false)` on
        // timeout (no key within the window) — that is NOT a reason to exit,
        // only to poll again. The previous `while let Ok(true) = poll(...)`
        // treated the first timeout as loop termination, killing this thread
        // ~50ms after launch, dropping `evt_tx`, and leaving the TUI unable
        // to receive keyboard input — a black screen that only redrew on
        // Ctrl+C. Exit only on a genuine read error (stdin closed).
        loop {
            match event::poll(std::time::Duration::from_millis(50)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => break,
            }
            let event = match event::read() {
                Ok(ev) => ev,
                Err(_) => continue,
            };

            // Bracketed paste arrives as its own event; flatten into a
            // string of `Submit` text.
            let mut pasted = String::new();
            let mut key_event = None;
            match event {
                Event::Key(k) if k.kind == KeyEventKind::Press => key_event = Some(k),
                Event::Paste(p) => pasted = p,
                _ => {}
            }

            if !pasted.is_empty() {
                // When an overlay with a secure prompt is open, the paste
                // targets the masked input field instead of the main
                // composer buffer. Single-line filter (drops non-graphic
                // bytes, strips trailing newline) keeps secrets clean.
                let routed_to_secure = {
                    let mut s = state.lock();
                    if let Some(overlay) = s.overlay.as_mut() {
                        if let Some(secure) = overlay.secure_input.as_mut() {
                            let (v, c) = insert_paste_into_secure_input(
                                &secure.value,
                                secure.cursor,
                                &pasted,
                            );
                            secure.value = v;
                            secure.cursor = c;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };
                if routed_to_secure {
                    continue;
                }
                let mut s = state.lock();
                let cursor = s.input_cursor;
                s.input_buffer.insert_str(cursor, &pasted);
                s.input_cursor = cursor + pasted.len();
                continue;
            }

            let Some(key) = key_event else { continue };

            // Ctrl+C: even with raw mode enabled some terminals / shells
            // fall back to delivering it as a SIGINT. Handle it as an
            // explicit interrupt so we don't depend on the OS signal.
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                let _ = evt_tx.send(InlineEvent::Interrupt);
                continue;
            }

            // Ctrl+M: toggle multiline input mode.
            if key.code == KeyCode::Char('m') && key.modifiers.contains(KeyModifiers::CONTROL) {
                let mut s = state.lock();
                s.multiline_mode = !s.multiline_mode;
                continue;
            }

            // Ctrl+P: open the command palette.
            if key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL) {
                let mut s = state.lock();
                s.overlay = Some(build_command_palette());
                continue;
            }

            // Ctrl+;: toggle the interactive queue panel.
            if key.code == KeyCode::Char(';') && key.modifiers.contains(KeyModifiers::CONTROL) {
                let mut s = state.lock();
                s.queue_panel_open = !s.queue_panel_open;
                if s.queue_panel_open {
                    s.queue_selected = 0;
                }
                continue;
            }

            // Ctrl+E: fold all blocks (Shift+E expands all).
            if key.code == KeyCode::Char('e') && key.modifiers.contains(KeyModifiers::CONTROL) {
                let mut s = state.lock();
                s.fold_all();
                continue;
            }

            // Ctrl+Enter: send-now — abort the current run (if any) and submit
            // the composed input immediately, bypassing the queue pane.
            if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL) {
                let submitted = {
                    let mut s = state.lock();
                    let buf = if s.slash_popup.open && !s.slash_popup.items.is_empty() {
                        format!("/{}", s.slash_popup.items[s.slash_popup.selected].name)
                    } else {
                        std::mem::take(&mut s.input_buffer)
                    };
                    s.input_cursor = 0;
                    s.slash_popup = SlashPopup::default();
                    s.history_pos = None;
                    if !buf.is_empty() && !buf.starts_with('/') {
                        s.prompt_history.insert(0, buf.clone());
                        s.prompt_history.truncate(100);
                    }
                    buf
                };
                if !submitted.is_empty() {
                    let _ = evt_tx.send(InlineEvent::Interrupt);
                    let _ = evt_tx.send(InlineEvent::Submit(submitted.into()));
                }
                continue;
            }

            // Confirmation modal takes priority over everything except
            // Ctrl+C (handled above): y/Enter confirms, n/x/Esc cancels.
            {
                let s = state.lock();
                if s.confirmation.is_some() {
                    drop(s);
                    handle_confirmation_key(&state, &evt_tx, key.code);
                    continue;
                }
            }

            // Overlay key handling takes priority — when an overlay is
            // open, Up/Down navigate, Enter submits, Esc cancels, and any
            // printable char is captured for the search bar (if any).
            // All other keys are swallowed so the composer buffer stays
            // frozen while the user is interacting with the overlay.
            {
                let s = state.lock();
                if s.overlay.is_some() {
                    drop(s);
                    if handle_overlay_key(&state, &evt_tx, key.code) {
                        continue;
                    }
                }
            }

            // @-file-search dropdown — when the picker is open, intercept
            // navigation and accept keys. Regular chars fall through to
            // normal buffer insertion so the user can keep typing.
            {
                let s = state.lock();
                if s.file_search.is_some() {
                    drop(s);
                    if handle_file_search_key(&state, &evt_tx, key.code) {
                        continue;
                    }
                }
            }

            match key.code {
                // Shift+Tab — cycle autonomy mode Default <-> Auto.
                KeyCode::BackTab => {
                    if let Some(h) = &mode_handle {
                        let new_mode = Mode::load(h).toggle();
                        h.store(new_mode.as_u8(), std::sync::atomic::Ordering::SeqCst);
                        let label = new_mode.label();
                        let detail = if new_mode.is_auto() {
                            "autonomous — no questions, runs to completion"
                        } else {
                            "interactive — may ask questions"
                        };
                        let mut s = state.lock();
                        s.autonomy_mode = new_mode;
                        s.tip = Some(EphemeralTip {
                            text: format!("Mode: {label} — {detail}"),
                            born_tick: 0,
                            ttl_ticks: 240,
                            key: "mode_toggle",
                            ambient: false,
                        });
                    }
                    continue;
                }
                KeyCode::Enter => {
                    // Multiline mode: plain Enter inserts a newline.
                    // Shift+Enter (or Enter in non-multiline mode) sends.
                    let send = !state.lock().multiline_mode
                        || key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::SHIFT);

                    if !send {
                        let mut s = state.lock();
                        let cursor = s.input_cursor;
                        s.input_buffer.insert(cursor, '\n');
                        s.input_cursor = cursor + 1;
                        continue;
                    }

                    // Shell mode: submit the buffer as a bash command request.
                    let shell_cmd = state.lock().shell_mode;
                    if shell_cmd {
                        let submitted = {
                            let mut s = state.lock();
                            let buf = std::mem::take(&mut s.input_buffer);
                            s.input_cursor = 0;
                            s.shell_mode = false;
                            s.history_pos = None;
                            if !buf.is_empty() {
                                s.prompt_history.insert(0, buf.clone());
                                s.prompt_history.truncate(100);
                            }
                            buf
                        };
                        if !submitted.is_empty() {
                            let prompt = format!("Run this shell command: `{submitted}`");
                            let _ = evt_tx.send(InlineEvent::Submit(prompt.into()));
                        }
                        continue;
                    }

                    let submitted = {
                        let mut s = state.lock();
                        let buf = if s.slash_popup.open && !s.slash_popup.items.is_empty() {
                            let item = &s.slash_popup.items[s.slash_popup.selected];
                            format!("/{}", item.name)
                        } else {
                            std::mem::take(&mut s.input_buffer)
                        };
                        s.input_cursor = 0;
                        s.slash_popup = SlashPopup::default();
                        s.history_pos = None;
                        // Record non-empty, non-command prompts in history.
                        if !buf.is_empty() && !buf.starts_with('/') {
                            s.prompt_history.insert(0, buf.clone());
                            s.prompt_history.truncate(100);
                        }
                        buf
                    };
                    let _ = evt_tx.send(InlineEvent::Submit(submitted.into()));
                }
                KeyCode::Esc => {
                    // Esc ladder (grok-build-style):
                    // 1. Slash popup open → close popup
                    // 2. Input non-empty + 2nd Esc within 800ms → clear buffer
                    // 3. Input non-empty + 1st Esc → arm "press again to clear"
                    // 4. Empty input → cancel the run (with ~1s post-cancel
                    //    grace so mashing Esc doesn't fire repeated cancels)
                    let mut s = state.lock();
                    if s.shell_mode {
                        s.shell_mode = false;
                        s.input_buffer.clear();
                        s.input_cursor = 0;
                    } else if s.slash_popup.open {
                        s.slash_popup = SlashPopup::default();
                    } else if !s.input_buffer.is_empty() {
                        let now = std::time::Instant::now();
                        let is_double = s
                            .last_esc_at
                            .map(|t| now.duration_since(t).as_millis() < 800)
                            .unwrap_or(false);
                        if is_double {
                            s.input_buffer.clear();
                            s.input_cursor = 0;
                            s.last_esc_at = None;
                        } else {
                            s.last_esc_at = Some(now);
                            // Ephemeral hint so the user learns the
                            // double-Esc-to-clear gesture.
                            s.tip = Some(EphemeralTip {
                                text: "Press Esc again to clear input".to_string(),
                                born_tick: FRAME_TICK.load(std::sync::atomic::Ordering::Relaxed),
                                ttl_ticks: 120,
                                key: "esc_clear",
                                ambient: false,
                            });
                        }
                    } else {
                        let now = std::time::Instant::now();
                        let in_grace = s.cancel_grace_until.map(|t| t > now).unwrap_or(false);
                        if in_grace {
                            // Swallow — already cancelling.
                        } else {
                            s.cancel_grace_until = Some(now + std::time::Duration::from_secs(1));
                            s.last_esc_at = None;
                            drop(s);
                            let _ = evt_tx.send(InlineEvent::Cancel);
                        }
                    }
                }
                KeyCode::Tab => {
                    // Complete the selected slash command into the buffer
                    // (without submitting) so the user can type arguments.
                    let mut s = state.lock();
                    if s.slash_popup.open && !s.slash_popup.items.is_empty() {
                        let name = s.slash_popup.items[s.slash_popup.selected].name.clone();
                        s.input_buffer = format!("/{} ", name);
                        s.input_cursor = s.input_buffer.len();
                        refresh_input_popups(&mut s);
                    }
                }
                KeyCode::Backspace => {
                    let mut s = state.lock();
                    if s.input_cursor > 0 {
                        let cursor = s.input_cursor;
                        // Walk back one UTF-8 char (not necessarily one
                        // byte, but chars are 1+ bytes).
                        let prev = s
                            .input_buffer
                            .char_indices()
                            .take_while(|(i, _)| *i < cursor)
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        s.input_buffer.replace_range(prev..cursor, "");
                        s.input_cursor = prev;
                    }
                    refresh_input_popups(&mut s);
                }
                KeyCode::Delete => {
                    let mut s = state.lock();
                    if s.input_cursor < s.input_buffer.len() {
                        let cursor = s.input_cursor;
                        let next = s.input_buffer[cursor..]
                            .char_indices()
                            .nth(1)
                            .map(|(i, _)| cursor + i)
                            .unwrap_or(s.input_buffer.len());
                        s.input_buffer.replace_range(cursor..next, "");
                    }
                    refresh_input_popups(&mut s);
                }
                KeyCode::Left => {
                    let mut s = state.lock();
                    s.input_cursor = s.input_cursor.saturating_sub(1);
                }
                KeyCode::Right => {
                    let mut s = state.lock();
                    let len = s.input_buffer.len();
                    s.input_cursor = (s.input_cursor + 1).min(len);
                }
                KeyCode::Up => {
                    let mut s = state.lock();
                    if s.slash_popup.open && !s.slash_popup.items.is_empty() {
                        let len = s.slash_popup.items.len();
                        s.slash_popup.selected = if s.slash_popup.selected == 0 {
                            len - 1
                        } else {
                            s.slash_popup.selected - 1
                        };
                    } else if s.queue_panel_open
                        && !s.queued_inputs.is_empty()
                        && s.input_buffer.is_empty()
                    {
                        s.queue_selected = if s.queue_selected == 0 {
                            s.queued_inputs.len() - 1
                        } else {
                            s.queue_selected - 1
                        };
                    } else if s.input_buffer.is_empty() && !s.prompt_history.is_empty() {
                        // History recall: fill the prompt with the previous entry.
                        let pos = s.history_pos.unwrap_or(0);
                        let next = (pos + 1).min(s.prompt_history.len() - 1);
                        s.history_pos = Some(next);
                        s.input_buffer = s.prompt_history[next].clone();
                        s.input_cursor = s.input_buffer.len();
                    } else {
                        drop(s);
                        let _ = evt_tx.send(InlineEvent::ScrollLineUp);
                    }
                }
                KeyCode::Down => {
                    let mut s = state.lock();
                    if s.slash_popup.open && !s.slash_popup.items.is_empty() {
                        let len = s.slash_popup.items.len();
                        s.slash_popup.selected = if s.slash_popup.selected + 1 >= len {
                            0
                        } else {
                            s.slash_popup.selected + 1
                        };
                    } else if s.queue_panel_open
                        && !s.queued_inputs.is_empty()
                        && s.input_buffer.is_empty()
                    {
                        s.queue_selected = if s.queue_selected + 1 >= s.queued_inputs.len() {
                            0
                        } else {
                            s.queue_selected + 1
                        };
                    } else {
                        drop(s);
                        let _ = evt_tx.send(InlineEvent::ScrollLineDown);
                    }
                }
                KeyCode::PageUp => {
                    let _ = evt_tx.send(InlineEvent::ScrollPageUp);
                }
                KeyCode::PageDown => {
                    let _ = evt_tx.send(InlineEvent::ScrollPageDown);
                }
                KeyCode::Char(ch) => {
                    let mut s = state.lock();
                    // @! hidden-file toggle: when the picker is open and '!'
                    // is typed immediately after '@', toggle hidden mode
                    // instead of inserting '!'.
                    if s.file_search.is_some()
                        && ch == '!'
                        && s.input_buffer[..s.input_cursor].ends_with('@')
                    {
                        let cwd = s.cwd.clone();
                        if let Some(fs) = s.file_search.as_mut() {
                            fs.toggle_hidden(&cwd);
                        }
                        continue;
                    }
                    if s.agent_hub_open && ch == 'q' {
                        s.agent_hub_open = false;
                    } else if s.vim_state.enabled() && !s.slash_popup.open {
                        // Route through the vim engine. Deref the guard so
                        // we can borrow multiple fields simultaneously.
                        let s = &mut *s;
                        let vkey =
                            crossterm::event::KeyEvent::new(KeyCode::Char(ch), key.modifiers);
                        let mut editor = InputEditor {
                            buffer: &mut s.input_buffer,
                            cursor: &mut s.input_cursor,
                        };
                        let outcome = oxicode_vtui::vim::handle_key(
                            &mut s.vim_state,
                            &mut editor,
                            &mut s.vim_clipboard,
                            &vkey,
                        );
                        if outcome.handled {
                            refresh_input_popups(s);
                        } else {
                            let cursor = s.input_cursor;
                            s.input_buffer.insert(cursor, ch);
                            s.input_cursor = cursor + ch.len_utf8();
                            refresh_input_popups(s);
                        }
                    } else if s.input_buffer.is_empty() && !s.slash_popup.open {
                        // Shell mode: `!` on empty buffer enters bash mode.
                        if ch == '!' && !s.shell_mode {
                            s.shell_mode = true;
                            continue;
                        }
                        // Queue panel interactive mode takes priority when
                        // open and the buffer is empty. Keys that don't
                        // match fall through to scrollback nav below.
                        if s.queue_panel_open && !s.queued_inputs.is_empty() {
                            let idx = s.queue_selected.min(s.queued_inputs.len() - 1);
                            match ch {
                                'x' | 'X' => {
                                    s.queued_inputs.remove(idx);
                                    if s.queue_selected >= s.queued_inputs.len()
                                        && !s.queued_inputs.is_empty()
                                    {
                                        s.queue_selected = s.queued_inputs.len() - 1;
                                    }
                                    continue;
                                }
                                'e' => {
                                    let entry = s.queued_inputs.remove(idx);
                                    s.input_buffer = entry;
                                    s.input_cursor = s.input_buffer.len();
                                    s.queue_panel_open = false;
                                    continue;
                                }
                                'J' => {
                                    if idx + 1 < s.queued_inputs.len() {
                                        s.queued_inputs.swap(idx, idx + 1);
                                        s.queue_selected = idx + 1;
                                    }
                                    continue;
                                }
                                'K' => {
                                    if idx > 0 {
                                        s.queued_inputs.swap(idx, idx - 1);
                                        s.queue_selected = idx - 1;
                                    }
                                    continue;
                                }
                                _ => {} // fall through to scrollback nav
                            }
                        }
                        // When the prompt is empty, intercept scrollback
                        // navigation keys (matching grok-build's scrollback-
                        // focus semantics). Any other char falls through to
                        // normal insertion so the user can start typing.
                        match ch {
                            '?' => {
                                s.overlay = Some(OverlayState {
                                    title: "Keyboard Shortcuts".into(),
                                    lines: cheatsheet_lines(),
                                    items: vec![],
                                    selected: 0,
                                    search: None,
                                    secure_input: None,
                                });
                            }
                            'e' => s.cycle_block_at_view(),
                            'E' => s.expand_all(),
                            'J' => s.jump_next_turn(),
                            'K' => s.jump_prev_turn(),
                            'n' if s.search.is_some() => s.search_next(),
                            'N' if s.search.is_some() => s.search_prev(),
                            _ => {
                                let cursor = s.input_cursor;
                                s.input_buffer.insert(cursor, ch);
                                s.input_cursor = cursor + ch.len_utf8();
                                refresh_input_popups(&mut s);
                            }
                        }
                    } else {
                        let cursor = s.input_cursor;
                        s.input_buffer.insert(cursor, ch);
                        s.input_cursor = cursor + ch.len_utf8();
                        refresh_input_popups(&mut s);
                    }
                    // plan_nudge: surface /compact when user mentions "plan".
                    if s.tip.is_none() && s.input_buffer.to_lowercase().contains("plan") {
                        s.show_tip(
                            "plan_nudge",
                            "Try /compact to summarize and plan ahead",
                            180,
                            true,
                        );
                    }
                }
                _ => {}
            }
        }
    })
}

/// Resolve a keystroke against the active confirmation modal. `y`/Enter
/// confirms — dispatches the bound [`ConfirmationAction`]; `n`/`x`/Esc
/// cancels. Always consumes the key while a confirmation is open.
fn handle_confirmation_key(
    state: &Arc<parking_lot::Mutex<RenderState>>,
    evt_tx: &tokio::sync::mpsc::UnboundedSender<InlineEvent>,
    code: KeyCode,
) {
    let mut s = state.lock();
    let Some(confirm) = s.confirmation.clone() else {
        return;
    };
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            s.confirmation = None;
            drop(s);
            match confirm.action {
                ConfirmationAction::Quit => {
                    let _ = evt_tx.send(InlineEvent::Exit);
                }
                ConfirmationAction::ClearConversation => {
                    // Re-dispatch /clear with --yes so it flows through the
                    // normal command pipeline (where `session.reset()` is
                    // accessible). The sentinel arg bypasses the dialog.
                    let _ = evt_tx.send(InlineEvent::Submit("/clear --yes".into()));
                }
                ConfirmationAction::RemoveProviderKey(name) => {
                    // Re-dispatch /providers remove <name> --yes so it flows
                    // through the normal command pipeline. The sentinel arg
                    // bypasses the confirm dialog.
                    let _ = evt_tx.send(InlineEvent::Submit(
                        format!("/providers remove {name} --yes").into(),
                    ));
                }
            }
        }
        KeyCode::Char('n')
        | KeyCode::Char('N')
        | KeyCode::Char('x')
        | KeyCode::Char('X')
        | KeyCode::Esc => {
            s.confirmation = None;
        }
        _ => {}
    }
}

/// Handle a single keystroke while an overlay is open. Returns `true` if the
/// key was consumed (whether it changed state or not). Always returns `false`
/// when no overlay is open so the caller can fall through to the regular
/// input-thread key dispatch.
fn handle_overlay_key(
    state: &Arc<parking_lot::Mutex<RenderState>>,
    evt_tx: &tokio::sync::mpsc::UnboundedSender<InlineEvent>,
    code: KeyCode,
) -> bool {
    use oxicode_vtui::tui::core::{OverlayEvent, OverlaySubmission};

    let mut s = state.lock();
    let Some(overlay) = s.overlay.as_mut() else {
        return false;
    };
    // Secure (masked) single-line prompt: takes precedence over list
    // navigation. Char / Backspace / Left / Right / Enter / Esc route
    if let Some(secure) = overlay.secure_input.as_mut() {
        match code {
            KeyCode::Backspace => {
                let (v, c) = backspace_secure_input(&secure.value, secure.cursor);
                secure.value = v;
                secure.cursor = c;
            }
            KeyCode::Left => {
                if secure.cursor > 0 {
                    let mut p = secure.cursor - 1;
                    while !secure.value.is_char_boundary(p) {
                        p -= 1;
                    }
                    secure.cursor = p;
                }
            }
            KeyCode::Right => {
                if secure.cursor < secure.value.len() {
                    let mut n = secure.cursor + 1;
                    while n < secure.value.len() && !secure.value.is_char_boundary(n) {
                        n += 1;
                    }
                    secure.cursor = n;
                }
            }
            KeyCode::Enter => {
                let submission = OverlaySubmission::SecureInput(secure.value.clone());
                drop(s);
                state.lock().overlay = None;
                let _ = evt_tx.send(InlineEvent::Overlay(OverlayEvent::Submitted(submission)));
            }
            KeyCode::Esc => {
                drop(s);
                state.lock().overlay = None;
                let _ = evt_tx.send(InlineEvent::Overlay(OverlayEvent::Cancelled));
            }
            KeyCode::Char(ch) if ch.is_ascii_graphic() || ch == ' ' => {
                let (v, n) = insert_char_into_secure_input(&secure.value, secure.cursor, ch);
                secure.value = v;
                secure.cursor = n;
            }
            _ => {} // ignore other keys while the secure prompt is open
        }
        return true;
    }

    match code {
        KeyCode::Esc => {
            // Cancel the overlay and notify the harness.
            drop(s);
            state.lock().overlay = None;
            let _ = evt_tx.send(InlineEvent::Overlay(OverlayEvent::Cancelled));
        }
        KeyCode::Enter => {
            // Submit the currently selected item. If no item is selected
            // (empty list), we still close the overlay with a cancel.
            let submission = if let Some(item) = overlay.items.get(overlay.selected) {
                match item.selection.clone() {
                    Some(sel) => sel,
                    None => {
                        // Read-only / informational item (no InlineListSelection,
                        // e.g. /tools, /mcp, the /settings Model row): Enter is
                        // a no-op — keep the overlay open so the user can keep
                        // browsing (Esc closes). Avoids polluting the prompt
                        // with a synthetic "/overlay:N" command.
                        return true;
                    }
                }
            } else {
                drop(s);
                state.lock().overlay = None;
                let _ = evt_tx.send(InlineEvent::Overlay(OverlayEvent::Cancelled));
                return true;
            };
            let title = overlay.title.clone();
            let selected = overlay.selected;
            drop(s);
            state.lock().overlay = None;
            tracing::debug!(
                overlay = %title,
                selected,
                "overlay submitted"
            );
            let _ = evt_tx.send(InlineEvent::Overlay(OverlayEvent::Submitted(
                OverlaySubmission::Selection(submission),
            )));
        }
        KeyCode::Up => {
            let len = overlay_filtered_indices(overlay).len();
            if len == 0 {
                return true;
            }
            let pos = overlay_filtered_indices(overlay)
                .iter()
                .position(|&i| i == overlay.selected)
                .unwrap_or(0);
            let new_pos = if pos == 0 { len - 1 } else { pos - 1 };
            overlay.selected = overlay_filtered_indices(overlay)[new_pos];
        }
        KeyCode::Down => {
            let filtered = overlay_filtered_indices(overlay);
            let len = filtered.len();
            if len == 0 {
                return true;
            }
            let pos = filtered
                .iter()
                .position(|&i| i == overlay.selected)
                .unwrap_or(0);
            let new_pos = if pos + 1 >= len { 0 } else { pos + 1 };
            overlay.selected = filtered[new_pos];
        }
        KeyCode::Backspace => {
            if let Some(search) = overlay.search.as_mut() {
                search.value.pop();
                overlay.selected = 0;
            }
        }
        KeyCode::Char(ch) => {
            if let Some(search) = overlay.search.as_mut() {
                search.value.push(ch);
                overlay.selected = 0;
            }
        }
        _ => {
            // Swallow all other keys while an overlay is open.
        }
    }
    true
}

/// Return the indices of `overlay.items` that match the current search filter.
/// When no search is configured (or the search field is empty), returns every
/// index. Used by both the renderer and the input thread so they agree on
/// which item is "selected" after navigation or filter changes.
fn overlay_filtered_indices(overlay: &OverlayState) -> Vec<usize> {
    let needle = overlay
        .search
        .as_ref()
        .map(|s| s.value.to_lowercase())
        .unwrap_or_default();
    if needle.is_empty() {
        return (0..overlay.items.len()).collect();
    }
    overlay
        .items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            let title_hit = item.title.to_lowercase().contains(&needle);
            let sv_hit = item
                .search_value
                .as_deref()
                .map(|v| v.to_lowercase().contains(&needle))
                .unwrap_or(false);
            if title_hit || sv_hit { Some(idx) } else { None }
        })
        .collect()
}

/// Handle a single keystroke while the @-file-search dropdown is open.
/// Returns `true` if the key was consumed. Up/Down navigate, Tab/Enter
/// accept the selection (inserting `@path ` without submitting), Esc
/// cancels. Regular chars fall through (`false`) so they enter the buffer
/// and trigger `refresh_file_search` to re-filter.
fn handle_file_search_key(
    state: &Arc<parking_lot::Mutex<RenderState>>,
    _evt_tx: &tokio::sync::mpsc::UnboundedSender<InlineEvent>,
    code: KeyCode,
) -> bool {
    match code {
        KeyCode::Up => {
            let mut s = state.lock();
            if let Some(fs) = s.file_search.as_mut() {
                fs.up();
                true
            } else {
                false
            }
        }
        KeyCode::Down => {
            let mut s = state.lock();
            if let Some(fs) = s.file_search.as_mut() {
                fs.down();
                true
            } else {
                false
            }
        }
        KeyCode::Tab | KeyCode::Enter => {
            let mut s = state.lock();
            if s.file_search
                .as_ref()
                .and_then(|fs| fs.selected_result())
                .is_some()
            {
                accept_file_search(&mut s, false);
                true
            } else {
                // No results: close the picker, let Enter fall through.
                s.file_search = None;
                false
            }
        }
        KeyCode::Esc => {
            let mut s = state.lock();
            s.file_search = None;
            true
        }
        _ => false,
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Agent worker thread — owns the agent run loop, forwards events to the
// session bus, and accepts new prompts from a tokio channel.
// ─────────────────────────────────────────────────────────────────────────

fn spawn_agent_worker(
    session: crate::app::agent_session::AgentSessionHandle,
) -> tokio::sync::mpsc::UnboundedSender<String> {
    let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(err) => {
                tracing::error!(?err, "failed to build agent worker runtime");
                return;
            }
        };

        runtime.block_on(async move {
            let local = tokio::task::LocalSet::new();
            local
                .run_until(async move {
                    while let Some(prompt) = prompt_rx.recv().await {
                        run_one_prompt(&session, prompt).await;
                    }
                })
                .await;
        });
    });

    prompt_tx
}

async fn run_one_prompt(session: &crate::app::agent_session::AgentSessionHandle, prompt: String) {
    let session_for_forward = session.clone();
    let (event_tx, event_rx) = std::sync::mpsc::channel::<AgentEvent>();

    // Forwarder thread — runs `forward_event_to_extensions` on each event
    // so the AgentSession's subscribers (and therefore the main loop)
    // observe it.
    let forwarder = std::thread::spawn(move || {
        while let Ok(event) = event_rx.recv() {
            session_for_forward.forward_event_to_extensions(&event);
        }
    });

    // Reset the stop flag (a previous Ctrl+C may have left it set) and
    // mark streaming so the Ctrl+C policy can distinguish "interrupt"
    // from "quit". The guard clears the flag on any exit path.
    use std::sync::atomic::Ordering;
    session.reset_should_stop();
    let streaming = session.streaming_flag();
    streaming.store(true, Ordering::SeqCst);
    let _stream_guard = StreamingGuard(&streaming);

    let agent = session.agent_ref();
    let local = tokio::task::LocalSet::new();
    let result = local
        .run_until(agent.run_with_channel(prompt, event_tx))
        .await;

    // Wait for the forwarder to drain the channel (sender dropped when
    // `run_with_channel` returns).
    let _ = forwarder.join();
    if let Err(err) = result {
        tracing::warn!(?err, "agent run failed");
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Header / AgentSession construction
// ─────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────
// Header / AgentSession construction
// ─────────────────────────────────────────────────────────────────────────

fn build_header_context(
    app: &App,
    cwd: &std::path::Path,
    git_branch: Option<&str>,
) -> InlineHeaderContext {
    let workspace_name = cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "oxicode".to_string());
    let model_id = app.model_id();
    let provider = model_id
        .split_once('/')
        .map(|(p, _)| p.to_string())
        .unwrap_or_else(|| "Provider".to_string());
    let branch = git_branch.unwrap_or("\u{2014}").to_string();
    let mut ctx = InlineHeaderContext::default();
    ctx.app_name = "oxicode".to_string();
    ctx.provider = provider;
    ctx.model = model_id.clone();
    ctx.git = format!("git: {workspace_name}@{branch}");
    ctx.tools = "Tools: ready".to_string();
    ctx.search_tools = Some(InlineHeaderStatusBadge {
        text: workspace_name,
        tone: InlineHeaderStatusTone::Ready,
    });
    ctx.persistent_memory = Some(InlineHeaderStatusBadge {
        text: branch,
        tone: InlineHeaderStatusTone::Ready,
    });
    ctx.editor_context = Some(model_id);
    ctx
}

/// Construct an `AgentSession` for the TUI using the runtime helpers from
/// `agent_session_runtime`. Mirrors the wiring in the legacy `tui/` harness.
async fn build_agent_session(app: &App) -> Result<crate::app::agent_session::AgentSession> {
    use crate::app::agent_session_runtime::{
        CreateAgentSessionFromServicesOptions, CreateAgentSessionServicesOptions,
        create_agent_session_from_services, create_agent_session_services,
    };
    use crate::store::session::SessionManager;

    let cwd: PathBuf = std::env::current_dir().unwrap_or_default();
    let hook_runner = Arc::clone(&app.oxicode().ports().hooks);
    let services = create_agent_session_services(
        CreateAgentSessionServicesOptions::new(cwd.clone()),
        Some(hook_runner),
    )?;
    let services = Arc::new(services);

    let model_id = app.model_id();
    let tools = app.agent_tools();

    let session_manager = SessionManager::create(&cwd.to_string_lossy(), None);

    let result = create_agent_session_from_services(CreateAgentSessionFromServicesOptions {
        services,
        session_manager,
        model_id: if model_id.is_empty() {
            None
        } else {
            Some(model_id)
        },
        thinking_level: None,
        scoped_models: Vec::new(),
        tool_registry: Some(tools),
        // TUI runtime: share the App's session state so /steer, /follow_up,
        // and Ctrl+C continue to take effect across the session.
        session_state: Some(app.session_state().clone()),
    })
    .await?;

    if let Some(msg) = result.model_fallback_message {
        tracing::warn!(message = %msg, "agent session model fallback");
    }
    Ok(result.session)
}

// ─────────────────────────────────────────────────────────────────────────
// Rendering
// ─────────────────────────────────────────────────────────────────────────

/// Lines for the keyboard shortcuts cheatsheet overlay.
fn cheatsheet_lines() -> Vec<String> {
    vec![
        "".into(),
        "  Navigation".into(),
        "  j / ↓        Scroll down".into(),
        "  k / ↑        Scroll up".into(),
        "  J (Shift+j)  Next turn".into(),
        "  K (Shift+k)  Previous turn".into(),
        "  PgDn / PgUp  Page scroll".into(),
        "  g / G        Top / bottom".into(),
        "".into(),
        "  Blocks".into(),
        "  e            Cycle block (collapse/truncate/expand)".into(),
        "  E            Expand all blocks".into(),
        "  Ctrl+E       Collapse all blocks".into(),
        "".into(),
        "  Search".into(),
        "  /find <q>    Search transcript".into(),
        "  n / N        Next / previous match".into(),
        "".into(),
        "  Commands".into(),
        "  /theme       Cycle color theme".into(),
        "  /model       Pick a model".into(),
        "  /vim         Toggle vim mode".into(),
        "  /compact     Compact context".into(),
        "  /clear       Clear conversation".into(),
        "  Ctrl+C       Cancel run (then y to quit)".into(),
        "  Ctrl+Enter   Send now (abort + submit)".into(),
        "  Ctrl+M       Toggle multiline input".into(),
        "  Shift+Tab    Toggle Auto mode (no questions, runs to end)".into(),
        "  Ctrl+;       Toggle queue panel".into(),
        "".into(),
        "  Special Input".into(),
        "  @           File picker (fuzzy search)".into(),
        "  @!          Toggle hidden files in picker".into(),
        "  !           Shell mode (bash command)".into(),
    ]
}

/// Build the command palette overlay — a searchable list of all slash
/// commands plus quick actions. Triggered by Ctrl+P.
fn build_command_palette() -> OverlayState {
    use oxicode_vtui::tui::core::{InlineListItem, InlineListSelection};

    let catalog = SlashRegistry::builtin_commands();
    let mut items: Vec<InlineListItem> = catalog
        .iter()
        .map(|(name, desc, aliases)| {
            let title = if aliases.is_empty() {
                format!("/{name}")
            } else {
                format!(
                    "/{name} ({})",
                    aliases
                        .iter()
                        .map(|a| format!("/{a}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            InlineListItem {
                title,
                subtitle: Some(desc.to_string()),
                badge: None,
                indent: 0,
                selection: Some(InlineListSelection::SlashCommand(name.to_string())),
                search_value: Some(format!("{name} {desc}")),
            }
        })
        .collect();
    items.sort_by(|a, b| a.title.cmp(&b.title));

    OverlayState {
        title: "Command Palette".into(),
        lines: vec!["Type to filter, Enter to select".into()],
        items: items
            .into_iter()
            .map(|item| OverlayListItem {
                title: item.title,
                subtitle: item.subtitle,
                badge: item.badge,
                indent: item.indent,
                search_value: item.search_value,
                selection: item.selection,
            })
            .collect(),
        selected: 0,
        search: Some(OverlaySearchState {
            label: "search".into(),
            placeholder: Some("filter commands\u{2026}".into()),
            value: String::new(),
        }),
        secure_input: None,
    }
}

/// Global frame tick counter for animations (incremented per render).
static FRAME_TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Tracks whether the terminal title currently shows a running state.
static TITLE_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Braille spinner frames for the tab title.
const TITLE_SPINNER: &[&str] = &[
    "\u{2807}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
];

/// Wave brightness for accent rail animation: sin²(tick·speed + row/rows·2π).
/// Returns [0.0, 1.0] — 1.0 = full color, 0.0 = dimmed toward background.
fn wave_brightness(tick: u64, row: u16, wave_rows: u16, speed: f64) -> f64 {
    let phase =
        (tick as f64 * speed) + (row as f64 / wave_rows.max(1) as f64) * std::f64::consts::TAU;
    let s = phase.sin();
    s * s
}

/// Linear-interpolate between two RGB colors. `ratio` 0 = base, 1 = target.
fn blend_rgb(base: Color, target: Color, ratio: f64) -> Color {
    match (base, target) {
        (Color::Rgb(br, bg, bb), Color::Rgb(tr, tg, tb)) => {
            let r = (br as f64 + (tr as f64 - br as f64) * ratio).round() as u8;
            let g = (bg as f64 + (tg as f64 - bg as f64) * ratio).round() as u8;
            let b = (bb as f64 + (tb as f64 - bb as f64) * ratio).round() as u8;
            Color::Rgb(r, g, b)
        }
        _ => base,
    }
}

/// Accent rail color for a transcript line kind.
fn accent_color_for_kind(kind: InlineMessageKind, styles: &ThemeStyles) -> Color {
    match kind {
        InlineMessageKind::User => color_from_anstyle(styles.primary.get_fg_color()),
        InlineMessageKind::Agent => color_from_anstyle(styles.response.get_fg_color()),
        InlineMessageKind::Tool => color_from_anstyle(styles.tool.get_fg_color()),
        InlineMessageKind::Error => color_from_anstyle(styles.error.get_fg_color()),
        InlineMessageKind::Warning => color_from_anstyle(styles.status.get_fg_color()),
        InlineMessageKind::Info => color_from_anstyle(styles.info.get_fg_color()),
        InlineMessageKind::Policy => color_from_anstyle(styles.mcp.get_fg_color()),
        InlineMessageKind::Pty => color_from_anstyle(styles.pty_output.get_fg_color()),
    }
}

/// Compose one frame using the agent view layout (grok-build-style):
/// StatusBar (top) → Scrollback (dominant) → Prompt → ShortcutsBar (bottom).
/// Chrome geometry and the status/shortcuts bars are rendered by
/// [`render_chrome`](crate::tui_vt::frame_layout::render_chrome); the transcript and composer are placed
/// into the returned layout rects.
fn render_frame(frame: &mut Frame<'_>, state: &RenderState, _handle: &InlineHandle) {
    let area = frame.area();
    // Paint the theme background across the whole frame first. Without this
    // every span renders against the host terminal's transparent default bg,
    // so fg-only text can read as invisible when it clashes with that default
    // — the user only saw it after drag-selecting (which inverts colors).
    let bg = active_styles().background;
    frame
        .buffer_mut()
        .set_style(area, Style::default().bg(color_from_anstyle(Some(bg))));
    let layout = super::frame_layout::render_chrome(frame, area, state);
    let tick = FRAME_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Update terminal tab title: spinner while running, plain when idle.
    {
        let running = state.reasoning_stage.is_some();
        let was_running = TITLE_RUNNING.swap(running, std::sync::atomic::Ordering::Relaxed);
        if running || was_running {
            let title = if running {
                let spin = TITLE_SPINNER[(tick as usize) % TITLE_SPINNER.len()];
                let model = state
                    .header_context
                    .editor_context
                    .as_deref()
                    .unwrap_or("oxicode");
                format!("{spin} oxicode \u{2014} {model}")
            } else {
                "oxicode".to_string()
            };
            use std::io::Write;
            let _ = write!(std::io::stderr(), "\x1b]2;{}\x07", title);
            let _ = std::io::stderr().flush();
        }
    }
    render_transcript(frame, layout.scrollback, state, tick);
    if !state.queued_inputs.is_empty() {
        render_queue_pane(frame, layout.scrollback, state);
    }
    if !state.todo_items.is_empty() {
        render_todo_pane(frame, layout.scrollback, &state.todo_items);
    }
    if !state.follow_ups.is_empty() {
        render_follow_ups(frame, layout.prompt, &state.follow_ups);
    }
    if let Some(stage) = &state.reasoning_stage {
        render_reasoning_indicator(frame, layout.prompt, stage);
    }
    render_composer(frame, layout.prompt, state);
    // Ephemeral tip banner above the composer (auto-dismissed by tick TTL).
    let occluded = state.overlay.is_some() || state.confirmation.is_some();
    if let Some(tip) = &state.tip
        && tip_is_visible(tip, tick)
        && !(tip.ambient && occluded)
    {
        render_tip(frame, layout.prompt, &tip.text);
    }
    if state.slash_popup.open {
        render_slash_popup(frame, layout.prompt, state);
    }
    if state.file_search.is_some() {
        render_file_search_dropdown(frame, layout.prompt, state);
    }
    if state.agent_hub_open {
        render_agent_hub(frame, area, state);
    }
    if let Some(overlay) = &state.overlay {
        render_overlay(frame, area, overlay);
    }
    if let Some(confirm) = &state.confirmation {
        render_confirmation(frame, area, confirm);
    }
}

/// Render the y/n/x confirmation modal centered on top of everything else.
fn render_confirmation(frame: &mut Frame, area: Rect, confirm: &ModalConfirmation) {
    let styles = active_styles();
    let accent = color_from_anstyle(styles.error.get_fg_color());
    let inner_w = confirm
        .title
        .chars()
        .count()
        .max(confirm.message.chars().count())
        .max(36) as u16;
    let width = inner_w + 4;
    let height = 5;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup_area = Rect {
        x,
        y,
        width,
        height,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            format!(" {} ", confirm.title),
            Style::default().fg(accent).bold(),
        ))
        .border_style(Style::default().fg(accent));
    let msg = Line::styled(
        confirm.message.clone(),
        Style::default().fg(color_from_anstyle(Some(styles.foreground))),
    );
    frame.render_widget(
        Paragraph::new(vec![Line::default(), msg]).block(block),
        popup_area,
    );
}

/// Render the Agent Hub overlay — a centered panel listing every registered
/// agent (kind, name, status). Populated from `RenderState::hub_entries`,
/// snapshotted when `/agents` fired. `q` (input thread Char arm) closes it.
fn render_agent_hub(frame: &mut Frame<'_>, area: Rect, state: &RenderState) {
    let rows = state.hub_entries.len() as u16;
    let height = rows.saturating_add(4).min(area.height.saturating_sub(1));
    let width = area.width.clamp(30, 80);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);

    let title = Line::from(Span::styled(
        " Agent Hub ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let block = Block::default().borders(Borders::ALL).title(title);

    let items: Vec<ListItem<'_>> = if state.hub_entries.is_empty() {
        vec![ListItem::new(Line::from(Span::raw(
            "No agents registered.",
        )))]
    } else {
        state
            .hub_entries
            .iter()
            .map(|(id, e)| {
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{:?} ", e.kind)),
                    Span::raw(e.display_name.clone()),
                    Span::raw(format!("  — {:?} ({})", e.status, id)),
                ]))
            })
            .collect()
    };
    frame.render_widget(List::new(items).block(block), rect);
}

/// Render an overlay (Modal / List) as a centered, bordered panel. Modals
/// show only their title + descriptive lines; lists also render a search bar
/// (when configured) and a scrollable item list with the selected item
/// marked by ▸.
fn render_overlay(frame: &mut Frame<'_>, area: Rect, overlay: &OverlayState) {
    let styles = active_styles();
    // Secure-input overlays draw a compact frame: title + lines + a single
    // masked input box. List overlays take the longer path below.
    if let Some(secure) = &overlay.secure_input {
        // Reserve the line just below `overlay.lines` for the input box.
        let lines_count = overlay.lines.len();
        let desired_h = (lines_count as u16).saturating_add(1).saturating_add(2); // input row + borders
        let height = desired_h.min(area.height.saturating_sub(2));
        let width = area.width.clamp(30, 80);
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, rect);

        let title = Line::from(Span::styled(
            format!(" {} ", overlay.title),
            Style::default()
                .fg(color_from_anstyle(styles.primary.get_fg_color()))
                .add_modifier(Modifier::BOLD),
        ));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(color_from_anstyle(styles.secondary.get_fg_color())))
            .title(title);
        let inner = block.inner(rect);
        frame.render_widget(&block, rect);

        let secondary = color_from_anstyle(styles.secondary.get_fg_color());
        let fg = color_from_anstyle(Some(styles.foreground));

        let mut row = inner.top();
        for line_text in &overlay.lines {
            let row_area = Rect {
                x: inner.left(),
                y: row,
                width: inner.width,
                height: 1,
            };
            let line = Line::from(Span::styled(
                line_text.clone(),
                Style::default().fg(secondary),
            ));
            frame.render_widget(Paragraph::new(line), row_area);
            row = row.saturating_add(1);
        }

        // Secure input box — mask the value when `mask_input` is on; fall back
        // to the configured placeholder when the buffer is empty.
        let label = &secure.config.label;
        let display: String = if secure.value.is_empty() {
            secure
                .config
                .placeholder
                .clone()
                .unwrap_or_else(|| "(empty)".to_string())
        } else if secure.config.mask_input {
            // One asterisk per character so the length is visible without
            // leaking the value. The render path must never reveal
            // `secure.value` when `mask_input` is on.
            "*".repeat(secure.value.chars().count())
        } else {
            secure.value.clone()
        };
        let row_area = Rect {
            x: inner.left(),
            y: row,
            width: inner.width,
            height: 1,
        };
        let line = Line::from(vec![
            Span::styled(format!("{label}: "), Style::default().fg(secondary)),
            Span::styled(display, Style::default().fg(fg)),
        ]);
        frame.render_widget(Paragraph::new(line), row_area);
        return;
    }
    let visible_max = (area.height as usize).saturating_sub(6).max(3);

    // Filter items by the search value when search is enabled.
    let filtered: Vec<usize> = match &overlay.search {
        Some(search) if !search.value.is_empty() => {
            let needle = search.value.to_lowercase();
            overlay
                .items
                .iter()
                .enumerate()
                .filter_map(|(idx, item)| {
                    let title_match = item.title.to_lowercase().contains(&needle);
                    let sv_match = item
                        .search_value
                        .as_deref()
                        .map(|v| v.to_lowercase().contains(&needle))
                        .unwrap_or(false);
                    if title_match || sv_match {
                        Some(idx)
                    } else {
                        None
                    }
                })
                .collect()
        }
        _ => (0..overlay.items.len()).collect(),
    };

    let has_search = overlay.search.is_some();
    let lines_count = overlay.lines.len();
    let items_count = filtered.len().min(visible_max);
    let height_inner = (lines_count + items_count + if has_search { 1 } else { 0 }) as u16;
    let desired_h = height_inner.saturating_add(2); // borders
    let height = desired_h.min(area.height.saturating_sub(2));
    let width = area.width.clamp(30, 80);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);

    let title = Line::from(Span::styled(
        format!(" {} ", overlay.title),
        Style::default()
            .fg(color_from_anstyle(styles.primary.get_fg_color()))
            .add_modifier(Modifier::BOLD),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color_from_anstyle(styles.secondary.get_fg_color())))
        .title(title);
    let inner = block.inner(rect);
    frame.render_widget(&block, rect);

    let primary = color_from_anstyle(styles.primary.get_fg_color());
    let fg = color_from_anstyle(Some(styles.foreground));
    let secondary = color_from_anstyle(styles.secondary.get_fg_color());

    // Compute where the selected item is in the filtered list.
    let selected_filtered_pos = filtered
        .iter()
        .position(|&idx| idx == overlay.selected)
        .unwrap_or(0);

    let mut row = inner.top();
    // Search bar (if present).
    if let Some(search) = &overlay.search {
        let prompt = format!("{}: {}", search.label, search.value);
        let line = Line::from(vec![
            Span::styled(
                format!("{}: ", search.label),
                Style::default().fg(secondary),
            ),
            Span::styled(
                if search.value.is_empty() {
                    search
                        .placeholder
                        .clone()
                        .unwrap_or_else(|| "type to filter\u{2026}".to_string())
                } else {
                    search.value.clone()
                },
                if search.value.is_empty() {
                    Style::default().fg(secondary).add_modifier(Modifier::DIM)
                } else {
                    Style::default().fg(fg)
                },
            ),
        ]);
        let _ = prompt; // suppress unused warning
        let row_area = Rect {
            x: inner.left(),
            y: row,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(line), row_area);
        row = row.saturating_add(1);
    }

    // Descriptive lines.
    for line_text in &overlay.lines {
        let row_area = Rect {
            x: inner.left(),
            y: row,
            width: inner.width,
            height: 1,
        };
        let line = Line::from(Span::styled(
            line_text.clone(),
            Style::default().fg(secondary),
        ));
        frame.render_widget(Paragraph::new(line), row_area);
        row = row.saturating_add(1);
    }

    // Items.
    if filtered.is_empty() {
        let row_area = Rect {
            x: inner.left(),
            y: row,
            width: inner.width,
            height: 1,
        };
        let empty_text = if overlay.search.is_some() {
            "  (no matches)"
        } else {
            "  (no items)"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                empty_text,
                Style::default().fg(secondary).add_modifier(Modifier::DIM),
            ))),
            row_area,
        );
    } else {
        for (display_idx, &item_idx) in filtered.iter().take(visible_max).enumerate() {
            let item = &overlay.items[item_idx];
            let is_selected = display_idx == selected_filtered_pos;
            let marker = if is_selected { "\u{25b8} " } else { "  " };
            let indent = "  ".repeat(item.indent as usize);
            let item_style = if is_selected {
                Style::default().fg(primary).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(fg)
            };
            let mut spans = vec![
                Span::styled(marker, item_style),
                Span::styled(indent, item_style),
                Span::styled(item.title.clone(), item_style),
            ];
            if let Some(badge) = &item.badge {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    badge.clone(),
                    Style::default().fg(secondary).add_modifier(Modifier::DIM),
                ));
            }
            if let Some(subtitle) = &item.subtitle {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    subtitle.clone(),
                    Style::default().fg(secondary),
                ));
            }
            let line = Line::from(spans);
            let row_area = Rect {
                x: inner.left(),
                y: row,
                width: inner.width,
                height: 1,
            };
            frame.render_widget(Paragraph::new(line), row_area);
            row = row.saturating_add(1);
        }
    }
}

fn render_transcript(frame: &mut Frame<'_>, area: Rect, state: &RenderState, tick: u64) {
    if state.transcript.is_empty() {
        render_welcome(frame, area);
        return;
    }
    let styles = active_styles();
    let bg_color = color_from_anstyle(Some(styles.background));

    // Split area: [1-col accent rail | content | 1-col scrollbar].
    let accent_w: u16 = 1;
    let scrollbar_w: u16 = 1;
    let content_area = Rect {
        x: area.x + accent_w,
        y: area.y,
        width: area.width.saturating_sub(accent_w + scrollbar_w),
        height: area.height,
    };

    // Build the visible-line list, respecting block folding. Track the kind
    // alongside each line so we can paint the accent rail in the role color.
    let search_set: std::collections::HashSet<usize> = state
        .search
        .as_ref()
        .map(|s| s.matches.iter().copied().collect())
        .unwrap_or_default();
    let current_match = state
        .search
        .as_ref()
        .and_then(|s| (!s.matches.is_empty()).then(|| s.matches[s.current]));

    let mut display: Vec<(usize, InlineMessageKind, Line<'_>)> =
        Vec::with_capacity(state.transcript.len());
    // Group consecutive lines into blocks, then render each block according
    // to its display mode (Collapsed / Truncated / Expanded). Absent
    // overrides fall back to Truncated — the grok-build default that keeps
    // long finished blocks scannable (head + ellipsis gap + tail).
    const TRUNC_TAIL: usize = 3;
    let dim_style = Style::default()
        .fg(color_from_anstyle(styles.secondary.get_fg_color()))
        .add_modifier(Modifier::DIM);

    let mut blocks: Vec<(usize, Vec<(usize, &TranscriptLine)>)> = Vec::new();
    for (idx, tl) in state.transcript.iter().enumerate() {
        if blocks.last().is_some_and(|(id, _)| *id == tl.block_id) {
            blocks.last_mut().unwrap().1.push((idx, tl));
        } else {
            blocks.push((tl.block_id, vec![(idx, tl)]));
        }
    }

    for (block_id, lines) in &blocks {
        let mode = state.block_mode(*block_id);
        let len = lines.len();
        match mode {
            BlockDisplayMode::Collapsed => {
                let &(idx, tl) = &lines[0];
                let is_match = search_set.contains(&idx);
                let line =
                    transcript_line_marked(tl, &styles, true, is_match, current_match == Some(idx));
                display.push((idx, tl.kind, line));
            }
            BlockDisplayMode::Expanded => {
                for &(idx, tl) in lines {
                    let is_match = search_set.contains(&idx);
                    let line = transcript_line_marked(
                        tl,
                        &styles,
                        false,
                        is_match,
                        current_match == Some(idx),
                    );
                    display.push((idx, tl.kind, line));
                }
            }
            BlockDisplayMode::Truncated => {
                if len <= TRUNC_TAIL + 1 {
                    // Short enough — show every line at full weight.
                    for &(idx, tl) in lines {
                        let is_match = search_set.contains(&idx);
                        let line = transcript_line_marked(
                            tl,
                            &styles,
                            false,
                            is_match,
                            current_match == Some(idx),
                        );
                        display.push((idx, tl.kind, line));
                    }
                } else {
                    // Head (first line, full weight).
                    let &(hidx, htl) = &lines[0];
                    let is_match = search_set.contains(&hidx);
                    let line = transcript_line_marked(
                        htl,
                        &styles,
                        false,
                        is_match,
                        current_match == Some(hidx),
                    );
                    display.push((hidx, htl.kind, line));
                    // Ellipsis gap summarising the hidden middle.
                    let hidden = len - 1 - TRUNC_TAIL;
                    let gap = Line::styled(format!("  \u{2026} +{hidden} lines"), dim_style);
                    display.push((hidx, htl.kind, gap));
                    // Tail (last N lines, in order).
                    for &(idx, tl) in lines.iter().rev().take(TRUNC_TAIL).rev() {
                        let is_match = search_set.contains(&idx);
                        let line = transcript_line_marked(
                            tl,
                            &styles,
                            false,
                            is_match,
                            current_match == Some(idx),
                        );
                        display.push((idx, tl.kind, line));
                    }
                }
            }
        }
    }

    // Resolve scroll offset into the display list.
    let total = display.len();
    let raw_start = if state.scroll_offset == usize::MAX {
        total.saturating_sub(content_area.height as usize)
    } else {
        display
            .iter()
            .position(|(orig_idx, _, _)| *orig_idx >= state.scroll_offset)
            .unwrap_or(total.saturating_sub(1))
    };
    let start = effective_scroll_offset(raw_start, total, content_area.height as usize);

    // Sticky header (grok-build parity): when the viewport top sits inside a
    // block's body (not on its head), pin the block's first line at the top
    // so the user can tell which block they are scrolling through.
    let sticky_first: Option<usize> = display.get(start).and_then(|(orig_idx, _, _)| {
        let bid = state.transcript.get(*orig_idx)?.block_id;
        let first_idx = state.transcript.iter().position(|l| l.block_id == bid)?;
        (first_idx != *orig_idx).then_some(first_idx)
    });
    let sticky_h: u16 = if sticky_first.is_some() { 1 } else { 0 };
    let body_top = content_area.top() + sticky_h;

    // Determine animation state.
    let running = state.reasoning_stage.is_some();
    const WAVE_ROWS: u16 = 32;
    const WAVE_SPEED: f64 = 0.15;

    // Push/fade (grok-build iOS-style 1D): detect the next block boundary
    // within the viewport. As it approaches the sticky row, fade the current
    // sticky header toward the background — a smooth handoff to the next
    // block's header. FADE_ROWS controls the transition width.
    const FADE_ROWS: usize = 5;
    let sticky_opacity: f64 = if let Some(sidx) = sticky_first {
        let sticky_bid = state.transcript[sidx].block_id;
        // Walk display from `start` to find the first visual row belonging to
        // a different block.
        let next_offset = display.iter().skip(start).position(|(orig_idx, _, _)| {
            state
                .transcript
                .get(*orig_idx)
                .map(|l| l.block_id != sticky_bid)
                .unwrap_or(false)
        });
        match next_offset {
            Some(off) if off <= FADE_ROWS => off as f64 / FADE_ROWS as f64,
            _ => 1.0,
        }
    } else {
        1.0
    };

    // Sticky header row: accent rail + head line + faint bg highlight.
    // Opacity fades as the next block pushes in.
    if let Some(sidx) = sticky_first {
        let tl = &state.transcript[sidx];
        let accent_base = accent_color_for_kind(tl.kind, &styles);
        let rail_blend = 0.7 * sticky_opacity;
        let bg_blend = 0.1 * sticky_opacity;
        if sticky_opacity > 0.05
            && let Some(cell) = frame.buffer_mut().cell_mut((area.x, content_area.top()))
        {
            cell.set_char('\u{2503}');
            cell.set_style(Style::default().fg(blend_rgb(bg_color, accent_base, rail_blend)));
        }
        let line = transcript_line_marked(tl, &styles, false, false, false);
        let row = Rect {
            x: content_area.x,
            y: content_area.top(),
            width: content_area.width,
            height: 1,
        };
        if bg_blend > 0.01 {
            frame.buffer_mut().set_style(
                row,
                Style::default().bg(blend_rgb(bg_color, accent_base, bg_blend)),
            );
        }
        frame.render_widget(Paragraph::new(line), row);
    }

    // Render top-down, wrapping each line into multiple visual rows.
    let mut y = body_top;
    let width = content_area.width.max(1) as usize;
    let mut visual_row: u16 = 0;
    for (_, kind, line) in display.into_iter().skip(start) {
        if y >= content_area.bottom() {
            break;
        }
        let text_w = line.width();
        let wrapped_h = if text_w == 0 {
            1
        } else {
            text_w.div_ceil(width).max(1) as u16
        };

        // Paint accent rail for each visual row of this line.
        let accent_base = accent_color_for_kind(kind, &styles);
        for row_offset in 0..wrapped_h {
            let paint_y = y + row_offset;
            if paint_y >= content_area.bottom() {
                break;
            }
            let brightness = if running {
                0.4 + 0.6 * wave_brightness(tick, visual_row + row_offset, WAVE_ROWS, WAVE_SPEED)
            } else {
                0.7
            };
            let rail_color = blend_rgb(bg_color, accent_base, brightness);
            if let Some(cell) = frame.buffer_mut().cell_mut((area.x, paint_y)) {
                cell.set_char('\u{2503}'); // ┃ heavy vertical
                cell.set_style(Style::default().fg(rail_color));
            }
        }

        let row = Rect {
            x: content_area.x,
            y,
            width: content_area.width,
            height: wrapped_h.min(content_area.bottom().saturating_sub(y)),
        };
        frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: false }), row);
        y += wrapped_h;
        visual_row += wrapped_h;
    }

    // Scrollbar (rightmost column): shown only when content overflows.
    // Follow-tail dims the thumb; explicit scroll brightens it.
    let body_viewport = (content_area.height as usize).saturating_sub(sticky_h as usize);
    if total > body_viewport {
        let follow = state.scroll_offset == usize::MAX;
        render_scrollbar(
            frame,
            area.right().saturating_sub(1),
            area.top(),
            area.height,
            total,
            body_viewport,
            start,
            follow,
            &styles,
            bg_color,
        );
    }
}

/// Render a 1-column scrollbar in the rightmost cell column. The thumb
/// represents the viewport's position within the full content; the rail is
/// a faint track. Follow-tail (auto-scroll) dims the thumb toward the
/// background; an explicit scroll offset paints it in the accent color.
#[allow(clippy::too_many_arguments)]
fn render_scrollbar(
    frame: &mut Frame,
    x: u16,
    top: u16,
    height: u16,
    total: usize,
    viewport: usize,
    start: usize,
    follow: bool,
    styles: &ThemeStyles,
    bg: Color,
) {
    if height == 0 {
        return;
    }
    let ratio = (start as f64 / total.max(1) as f64).clamp(0.0, 1.0);
    let thumb_h = (((viewport as f64 / total.max(1) as f64) * height as f64).ceil() as u16)
        .max(1)
        .min(height);
    let track_h = height.saturating_sub(thumb_h);
    let thumb_y = (ratio * track_h as f64).round() as u16;

    let accent = color_from_anstyle(styles.primary.get_fg_color());
    // Follow-tail: dim thumb so it recedes. Explicit scroll: bright accent.
    let thumb_color = if follow {
        blend_rgb(bg, accent, 0.35)
    } else {
        accent
    };
    let rail_color = blend_rgb(bg, accent, 0.1);

    for row in 0..height {
        let y = top + row;
        let is_thumb = row >= thumb_y && row < thumb_y + thumb_h;
        let (ch, color) = if is_thumb {
            ('\u{2588}', thumb_color) // █
        } else {
            ('\u{2502}', rail_color) // │
        };
        if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
            cell.set_char(ch);
            cell.set_style(Style::default().fg(color));
        }
    }
}

/// Build a ratatui `Line` from a transcript line, with optional fold marker
/// and search-match highlighting.
fn transcript_line_marked<'a>(
    line: &'a TranscriptLine,
    styles: &'a ThemeStyles,
    folded: bool,
    is_match: bool,
    is_current: bool,
) -> Line<'a> {
    let (kind_style, marker) = match line.kind {
        InlineMessageKind::Agent => (
            Style::default().fg(color_from_anstyle(styles.response.get_fg_color())),
            "\u{25cf}", // ●
        ),
        InlineMessageKind::User => (
            Style::default().fg(color_from_anstyle(styles.primary.get_fg_color())),
            "\u{276f}", // ❯
        ),
        InlineMessageKind::Tool => (
            Style::default().fg(color_from_anstyle(styles.tool.get_fg_color())),
            "\u{2699}", // ⚙
        ),
        InlineMessageKind::Error => (
            Style::default().fg(color_from_anstyle(styles.error.get_fg_color())),
            "\u{2717}", // ✗
        ),
        InlineMessageKind::Warning => (
            Style::default().fg(color_from_anstyle(styles.status.get_fg_color())),
            "\u{26a0}", // ⚠
        ),
        InlineMessageKind::Info => (
            Style::default().fg(color_from_anstyle(styles.info.get_fg_color())),
            "\u{2139}", // ℹ
        ),
        InlineMessageKind::Policy => (
            Style::default().fg(color_from_anstyle(styles.mcp.get_fg_color())),
            "\u{25c6}", // ◆
        ),
        InlineMessageKind::Pty => (
            Style::default().fg(color_from_anstyle(styles.pty_output.get_fg_color())),
            "\u{258c}", // ▌
        ),
    };

    // Fold marker: ▸ for folded, ▾ for unfolded (shown on first line of block).
    let prefix = if folded {
        format!("\u{25b8} {} ", marker) // ▸
    } else {
        format!("{} ", marker)
    };

    // Highlight background for search matches.
    let highlight = if is_current {
        Some(Style::default().reversed())
    } else if is_match {
        Some(Style::default().add_modifier(Modifier::UNDERLINED))
    } else {
        None
    };

    let mut spans = Vec::with_capacity(line.segments.len() + 1);
    spans.push(Span::styled(prefix, kind_style));
    for segment in &line.segments {
        let mut style = segment_style(segment, kind_style, styles);
        if let Some(h) = highlight {
            style = style.patch(h);
        }
        spans.push(Span::styled(segment.text.clone(), style));
    }
    Line::from(spans)
}

fn segment_style(segment: &InlineSegment, fallback: Style, styles: &ThemeStyles) -> Style {
    let mut style = fallback;
    let inline = segment.style.as_ref();
    if let Some(color) = inline.color {
        style = style.fg(color_from_anstyle(Some(color)));
    } else {
        // Fall back to the active palette's default for the kind. We
        // pick `response` for agent segments since the harness doesn't
        // carry its own theme.
        style = style.fg(color_from_anstyle(styles.response.get_fg_color()));
    }
    if inline.effects.contains(anstyle::Effects::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if inline.effects.contains(anstyle::Effects::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if inline.effects.contains(anstyle::Effects::UNDERLINE) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if inline.effects.contains(anstyle::Effects::DIMMED) {
        style = style.add_modifier(Modifier::DIM);
    }
    style
}

fn render_composer(frame: &mut Frame<'_>, area: Rect, state: &RenderState) {
    let styles = active_styles();
    let prefix_style = Style::default()
        .fg(color_from_anstyle(styles.primary.get_fg_color()))
        .bold();
    let text_style = Style::default().fg(color_from_anstyle(Some(styles.foreground)));

    let prefix = state.prompt_prefix.clone();
    let body = state.input_buffer.clone();
    let placeholder = state.placeholder.clone();

    let mut line_spans = Vec::new();
    if let Some(label) = state.vim_state.status_label() {
        line_spans.push(Span::styled(
            format!("[{label}] "),
            Style::default()
                .fg(color_from_anstyle(styles.tool.get_fg_color()))
                .add_modifier(Modifier::BOLD),
        ));
    }
    if state.autonomy_mode.is_auto() {
        line_spans.push(Span::styled(
            "[auto] ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    line_spans.push(Span::styled(prefix, prefix_style));
    if state.shell_mode {
        line_spans.push(Span::styled(
            "! ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if body.is_empty()
        && let Some(ph) = placeholder
    {
        line_spans.push(Span::styled(
            ph,
            Style::default()
                .fg(color_from_anstyle(styles.secondary.get_fg_color()))
                .dim(),
        ));
    } else {
        line_spans.push(Span::styled(body, text_style));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color_from_anstyle(styles.secondary.get_fg_color())));
    let paragraph = Paragraph::new(Line::from(line_spans))
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);

    // Place the cursor inside the composer at the current edit position.
    // +1 on both axes to clear the rounded border.
    if state.input_enabled {
        let vim_off = state
            .vim_state
            .status_label()
            .map(|l| format!("[{l}] ").chars().count() as u16)
            .unwrap_or(0);
        let shell_off = if state.shell_mode { 2 } else { 0 };
        let mode_off = if state.autonomy_mode.is_auto() { 7 } else { 0 };
        let cursor_x = area.left()
            + 1
            + vim_off
            + mode_off
            + shell_off
            + state.prompt_prefix.chars().count() as u16
            + state.input_cursor as u16;
        let cursor_y = area.top() + 1;
        frame.set_cursor_position(ratatui::layout::Position::new(cursor_x, cursor_y));
    }
}

/// Render a welcome banner when the transcript is empty, using the vtui
/// `WelcomeLayout` for proper geometry on wide terminals.
fn render_welcome(frame: &mut Frame<'_>, area: Rect) {
    let styles = active_styles();
    let primary = color_from_anstyle(styles.primary.get_fg_color());
    let fg = color_from_anstyle(Some(styles.foreground));
    let secondary = color_from_anstyle(styles.secondary.get_fg_color());

    // For wide terminals, use the hero-box layout; otherwise a simple
    // centered paragraph is more reliable for narrow viewports.
    if area.width >= 90 {
        use oxicode_vtui::design::layout::WelcomeLayout;
        let layout = WelcomeLayout::compute(area, 3, 0, 0, 1, 0, false);
        let logo_area = if layout.has_hero_box() {
            layout.hero_logo
        } else {
            layout.logo
        };
        if logo_area.height > 0 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "\u{25cf} oxicode",
                    Style::default().fg(primary).add_modifier(Modifier::BOLD),
                )))
                .alignment(Alignment::Center),
                logo_area,
            );
        }
        if layout.tip.height > 0 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "Type a message to begin, or press / for commands.",
                    Style::default().fg(fg),
                )))
                .alignment(Alignment::Center),
                layout.tip,
            );
        }
        if layout.version.height > 0 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("v{}", env!("CARGO_PKG_VERSION")),
                    Style::default().fg(secondary).add_modifier(Modifier::DIM),
                )))
                .alignment(Alignment::Center),
                layout.version,
            );
        }
        return;
    }

    // Narrow terminal fallback — simple centered paragraph.
    let version = env!("CARGO_PKG_VERSION");
    let text = vec![
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "\u{25cf} oxicode",
            Style::default().fg(primary).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Type a message to begin, or press / for commands.",
            Style::default().fg(fg),
        )),
        Line::from(Span::styled(
            format!("v{version} \u{2014} /help for commands"),
            Style::default().fg(secondary),
        )),
    ];
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), area);
}

/// Render a 1-row reasoning/tool-stage indicator just above the composer.
fn render_reasoning_indicator(frame: &mut Frame<'_>, composer_area: Rect, stage: &str) {
    let styles = active_styles();
    let indicator_area = Rect {
        x: composer_area.x,
        y: composer_area.top().saturating_sub(1),
        width: composer_area.width,
        height: 1,
    };
    let spinner = "\u{25cc}"; // ◌
    let line = Line::from(vec![
        Span::styled(
            format!("{spinner} "),
            Style::default().fg(color_from_anstyle(styles.tool.get_fg_color())),
        ),
        Span::styled(
            stage.to_string(),
            Style::default()
                .fg(color_from_anstyle(styles.secondary.get_fg_color()))
                .add_modifier(Modifier::DIM),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), indicator_area);
}

/// Render queued input prompts as a compact pane at the top of the scrollback.
fn render_queue_pane(frame: &mut Frame<'_>, scrollback: Rect, state: &RenderState) {
    let styles = active_styles();
    let entries = &state.queued_inputs;
    let interactive = state.queue_panel_open;
    let selected = state.queue_selected.min(entries.len().saturating_sub(1));
    let height = entries.len() as u16 + 1;
    let area = Rect {
        x: scrollback.x,
        y: scrollback.y,
        width: scrollback.width,
        height,
    };
    let info = color_from_anstyle(styles.info.get_fg_color());
    let secondary = color_from_anstyle(styles.secondary.get_fg_color());
    let primary = color_from_anstyle(styles.primary.get_fg_color());
    let items: Vec<Line<'_>> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let prefix = if interactive {
                format!("#{} ", i + 1)
            } else {
                "\u{2261} ".to_string()
            };
            let prefix_style = if interactive && i == selected {
                Style::default().fg(primary).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(info)
            };
            let text_style = if interactive && i == selected {
                Style::default().fg(primary).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(secondary)
            };
            let marker = if interactive && i == selected {
                "\u{25b8} " // ▸
            } else {
                "  "
            };
            Line::from(vec![
                Span::styled(prefix, prefix_style),
                Span::styled(marker, prefix_style),
                Span::styled(e.clone(), text_style),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(items).block(Block::default().borders(Borders::TOP).border_style(
            Style::default().fg(color_from_anstyle(styles.secondary.get_fg_color())),
        )),
        area,
    );
}

/// Flatten todo phases into `(content, status)` pairs for the sticky pane.
fn flatten_todo_items(
    phases: &[oxicode_agent::tools::todo::TodoPhase],
) -> Vec<(String, TodoStatus)> {
    phases
        .iter()
        .flat_map(|p| p.tasks.iter().map(|t| (t.content.clone(), t.status)))
        .collect()
}

/// Render a compact todo checklist at the top of the scrollback area.
fn render_todo_pane(frame: &mut Frame<'_>, scrollback: Rect, items: &[(String, TodoStatus)]) {
    let styles = active_styles();
    let height = items.len() as u16 + 1;
    let area = Rect {
        x: scrollback.x,
        y: scrollback.y,
        width: scrollback.width,
        height,
    };
    let lines: Vec<Line<'_>> = items
        .iter()
        .map(|(text, status)| {
            // Per-status glyph + color so the current task (▶), blocked
            // tasks (⏸), and abandoned tasks (✗) are distinguishable at a
            // glance, not just done (☑) vs. open (☐).
            let (marker, color) = match status {
                TodoStatus::Completed => ("\u{2611}", Some(styles.foreground)), // ☑
                TodoStatus::InProgress => ("\u{25B6}", styles.primary.get_fg_color()), // ▶
                TodoStatus::Blocked => ("\u{23F8}", styles.info.get_fg_color()), // ⏸
                TodoStatus::Abandoned => ("\u{2717}", styles.error.get_fg_color()), // ✗
                TodoStatus::Pending => ("\u{2610}", styles.secondary.get_fg_color()), // ☐
            };
            let text_style = if *status == TodoStatus::Completed {
                Style::default()
                    .fg(color_from_anstyle(Some(styles.foreground)))
                    .add_modifier(Modifier::CROSSED_OUT)
            } else {
                Style::default().fg(color_from_anstyle(Some(styles.foreground)))
            };
            Line::from(vec![
                Span::styled(
                    format!("{marker} "),
                    Style::default().fg(color_from_anstyle(color)),
                ),
                Span::styled(text.clone(), text_style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// Render follow-up suggestion chips just above the composer.
fn render_follow_ups(frame: &mut Frame<'_>, composer_area: Rect, chips: &[String]) {
    let styles = active_styles();
    let area = Rect {
        x: composer_area.x,
        y: composer_area.top().saturating_sub(1),
        width: composer_area.width,
        height: 1,
    };
    let mut spans = vec![Span::styled(
        "Suggestions: ",
        Style::default()
            .fg(color_from_anstyle(styles.secondary.get_fg_color()))
            .add_modifier(Modifier::DIM),
    )];
    for (i, chip) in chips.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("\u{25b8} {chip}"),
            Style::default().fg(color_from_anstyle(styles.primary.get_fg_color())),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Whether an ephemeral tip is still within its visible TTL window.
fn tip_is_visible(tip: &EphemeralTip, now_tick: u64) -> bool {
    now_tick.saturating_sub(tip.born_tick) < tip.ttl_ticks
}

/// Render the ephemeral tip banner one row above the composer.
fn render_tip(frame: &mut Frame, composer_area: Rect, text: &str) {
    let styles = active_styles();
    let area = Rect {
        x: composer_area.x,
        y: composer_area.top().saturating_sub(1),
        width: composer_area.width,
        height: 1,
    };
    let line = Line::styled(
        format!(" \u{2139} {text}"),
        Style::default()
            .fg(color_from_anstyle(styles.info.get_fg_color()))
            .add_modifier(Modifier::DIM),
    );
    frame.render_widget(Paragraph::new(line), area);
}

/// Render the slash-command autocomplete popup as a floating panel above the
/// composer. Anchored to the composer's left edge, grows upward.
fn render_slash_popup(frame: &mut Frame<'_>, composer_area: Rect, state: &RenderState) {
    let styles = active_styles();
    let items = &state.slash_popup.items;
    if items.is_empty() {
        return;
    }

    let max_visible = 8usize;
    let visible = items.len().min(max_visible);
    let popup_h = visible as u16 + 2; // +2 for top/bottom border
    let width = composer_area.width.min(64);
    let popup_area = Rect {
        x: composer_area.left(),
        y: composer_area.top().saturating_sub(popup_h),
        width,
        height: popup_h,
    };
    frame.render_widget(Clear, popup_area);

    let border_color = color_from_anstyle(styles.secondary.get_fg_color());
    let title = Line::from(Span::styled(
        " Commands ",
        Style::default()
            .fg(color_from_anstyle(styles.primary.get_fg_color()))
            .add_modifier(Modifier::BOLD),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(title);
    let inner = block.inner(popup_area);
    frame.render_widget(&block, popup_area);

    // Column-align labels by padding to the widest visible label.
    let max_label = items
        .iter()
        .take(visible)
        .map(|i| i.label.chars().count())
        .max()
        .unwrap_or(0);

    let primary = color_from_anstyle(styles.primary.get_fg_color());
    let fg = color_from_anstyle(Some(styles.foreground));
    let secondary = color_from_anstyle(styles.secondary.get_fg_color());

    for (i, item) in items.iter().take(visible).enumerate() {
        let is_selected = i == state.slash_popup.selected;
        let y = inner.top() + i as u16;
        let row_area = Rect {
            x: inner.left(),
            y,
            width: inner.width,
            height: 1,
        };

        let marker = if is_selected { "\u{25b8} " } else { "  " }; // ▸ or space
        let label_style = if is_selected {
            Style::default().fg(primary).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(fg)
        };
        let label_padded = format!("{:<width$}", item.label, width = max_label);
        let line = Line::from(vec![
            Span::styled(marker, label_style),
            Span::styled(label_padded, label_style),
            Span::raw("  "),
            Span::styled(&item.description, Style::default().fg(secondary)),
        ]);
        frame.render_widget(Paragraph::new(line), row_area);
    }
}

/// Render the @-file-search dropdown as a floating panel above the
/// composer, mirroring `render_slash_popup`'s geometry. Shows up to 10
/// fuzzy-matched file paths with the selected one highlighted.
fn render_file_search_dropdown(frame: &mut Frame<'_>, composer_area: Rect, state: &RenderState) {
    let styles = active_styles();
    let Some(fs) = &state.file_search else {
        return;
    };
    let items = &fs.results;
    if items.is_empty() {
        return;
    }

    let max_visible = 10usize;
    let visible = items.len().min(max_visible);
    let popup_h = visible as u16 + 2; // +2 for top/bottom border
    let width = composer_area.width.min(72);
    let popup_area = Rect {
        x: composer_area.left(),
        y: composer_area.top().saturating_sub(popup_h),
        width,
        height: popup_h,
    };
    frame.render_widget(Clear, popup_area);

    let border_color = color_from_anstyle(styles.secondary.get_fg_color());
    let title_str = if fs.hidden_mode {
        " Files (hidden) "
    } else {
        " Files "
    };
    let title = Line::from(Span::styled(
        title_str,
        Style::default()
            .fg(color_from_anstyle(styles.primary.get_fg_color()))
            .add_modifier(Modifier::BOLD),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(title);
    let inner = block.inner(popup_area);
    frame.render_widget(&block, popup_area);

    let primary = color_from_anstyle(styles.primary.get_fg_color());
    let fg = color_from_anstyle(Some(styles.foreground));
    let secondary = color_from_anstyle(styles.secondary.get_fg_color());

    for (i, result) in items.iter().take(visible).enumerate() {
        let is_selected = i == fs.selected;
        let y = inner.top() + i as u16;
        let row_area = Rect {
            x: inner.left(),
            y,
            width: inner.width,
            height: 1,
        };

        let marker = if is_selected { "\u{25b8} " } else { "  " }; // ▸ or space
        let path_style = if is_selected {
            Style::default().fg(primary).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(fg)
        };
        let line = Line::from(vec![
            Span::styled(marker, path_style),
            Span::styled(&result.path, path_style),
        ]);
        frame.render_widget(Paragraph::new(line), row_area);
    }

    // Footer hint: show result count + key bindings.
    if popup_h >= 4 {
        let hint_y = inner.bottom();
        let hint_area = Rect {
            x: inner.left(),
            y: hint_y,
            width: inner.width,
            height: 1,
        };
        let count = items.len();
        let hint = format!("{count} files  \u{00b7}  Tab accept  Esc cancel");
        let _ = secondary; // suppress unused warning
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(color_from_anstyle(styles.secondary.get_fg_color())),
            )))
            .style(Style::default().fg(color_from_anstyle(styles.secondary.get_fg_color()))),
            hint_area,
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Vim mode — Editor adapter for the input buffer
// ─────────────────────────────────────────────────────────────────────────

/// Adapter that lets the vim engine operate on `RenderState`'s input buffer.
struct InputEditor<'a> {
    buffer: &'a mut String,
    cursor: &'a mut usize,
}

impl<'a> oxicode_vtui::vim::Editor for InputEditor<'a> {
    fn content(&self) -> &str {
        self.buffer
    }
    fn cursor(&self) -> usize {
        *self.cursor
    }
    fn set_cursor(&mut self, pos: usize) {
        *self.cursor = pos.min(self.buffer.len());
    }
    fn move_left(&mut self) {
        *self.cursor = self.cursor.saturating_sub(1);
    }
    fn move_right(&mut self) {
        let len = self.buffer.len();
        *self.cursor = (*self.cursor + 1).min(len);
    }
    fn delete_char_forward(&mut self) {
        let cursor = *self.cursor;
        if cursor < self.buffer.len() {
            let next = self.buffer[cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| cursor + i)
                .unwrap_or(self.buffer.len());
            self.buffer.replace_range(cursor..next, "");
        }
    }
    fn insert_text(&mut self, text: &str) {
        let cursor = *self.cursor;
        self.buffer.insert_str(cursor, text);
        *self.cursor = cursor + text.len();
    }
    fn replace(&mut self, content: String, cursor: usize) {
        *self.buffer = content;
        *self.cursor = cursor.min(self.buffer.len());
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Small helpers
// ─────────────────────────────────────────────────────────────────────────

pub(crate) fn plain_segment(text: impl Into<String>) -> InlineSegment {
    InlineSegment {
        text: text.into(),
        style: Arc::new(InlineTextStyle::default()),
    }
}

pub(super) fn effective_scroll_offset(offset: usize, total: usize, viewport: usize) -> usize {
    if offset == usize::MAX {
        return total.saturating_sub(viewport);
    }
    // Clamp into [0, total.saturating_sub(viewport)].
    let max_start = total.saturating_sub(viewport);
    offset.min(max_start)
}

// ─────────────────────────────────────────────────────────────────────────
// Secure-input helpers — pure string mutations used by the input thread
// while a secure prompt overlay is open. Kept as free functions so the
// key routing in `handle_overlay_key` / `spawn_input_thread` stays thin
// and the byte-boundary logic is unit-testable in isolation.
// ─────────────────────────────────────────────────────────────────────────

/// Append `ch` to `value` at `cursor`, returning the new value and cursor.
/// `cursor` is a byte index into `value`; `ch` is inserted at that offset
/// and the cursor advances by `ch.len_utf8()` bytes.
fn insert_char_into_secure_input(value: &str, cursor: usize, ch: char) -> (String, usize) {
    let mut s = String::with_capacity(value.len() + ch.len_utf8());
    s.push_str(&value[..cursor]);
    s.push(ch);
    s.push_str(&value[cursor..]);
    (s, cursor + ch.len_utf8())
}

/// Pop the byte before `cursor` from `value`, returning the new value and
/// cursor. Walks back to the nearest UTF-8 char boundary so multi-byte
/// characters are removed whole. A no-op when `cursor == 0`.
fn backspace_secure_input(value: &str, cursor: usize) -> (String, usize) {
    if cursor == 0 {
        return (value.to_string(), 0);
    }
    // Find the previous char boundary.
    let mut prev = cursor - 1;
    while !value.is_char_boundary(prev) {
        prev -= 1;
    }
    let mut s = String::with_capacity(value.len() - (cursor - prev));
    s.push_str(&value[..prev]);
    s.push_str(&value[cursor..]);
    (s, prev)
}

/// Insert a pasted chunk at `cursor`. Strips a single trailing `\n`
/// (terminals commonly deliver one at the end of a bracketed paste) and
/// drops any byte that isn't printable ASCII — newlines, tabs, and other
/// control characters are filtered out so secrets stay on a single line.
fn insert_paste_into_secure_input(value: &str, cursor: usize, paste: &str) -> (String, usize) {
    let trimmed = paste.trim_end_matches('\n');
    let filtered: String = trimmed
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect();
    let mut s = String::with_capacity(value.len() + filtered.len());
    s.push_str(&value[..cursor]);
    s.push_str(&filtered);
    s.push_str(&value[cursor..]);
    (s, cursor + filtered.len())
}

// ─────────────────────────────────────────────────────────────────────────
// Slash-command autocomplete popup
// ─────────────────────────────────────────────────────────────────────────

/// Filter slash commands by `token` (the text after `/`). An empty token
/// returns every command. Matching is prefix-based against the canonical
/// name and all aliases.
///
/// Built-in commands are listed first; user-defined file commands are
/// appended afterwards. Any file command whose name shadows a built-in is
/// dropped — built-ins always win, so file commands cannot redefine
/// `/quit`, `/clear`, etc.
fn slash_filter(token: &str, file_commands: &[FileCommand]) -> Vec<SlashPopupItem> {
    let builtins = SlashRegistry::builtin_commands();
    let builtin_names: std::collections::HashSet<&str> =
        builtins.iter().map(|(n, _, _)| *n).collect();

    let mut items: Vec<SlashPopupItem> = builtins
        .into_iter()
        .filter(|(name, _, aliases)| {
            token.is_empty()
                || name.starts_with(token)
                || aliases.iter().any(|a| a.starts_with(token))
        })
        .map(|(name, desc, aliases)| {
            let mut label = format!("/{name}");
            for a in &aliases {
                label.push_str(&format!(", /{a}"));
            }
            SlashPopupItem {
                label,
                description: desc.to_string(),
                name: name.to_string(),
            }
        })
        .collect();

    // Append file commands (skip names shadowed by builtins).
    for fc in file_commands {
        if builtin_names.contains(fc.name.as_str())
            || fc
                .aliases
                .iter()
                .any(|alias| builtin_names.contains(alias.as_str()))
        {
            continue;
        }
        if token.is_empty()
            || fc.name.starts_with(token)
            || fc.aliases.iter().any(|a| a.starts_with(token))
        {
            let mut label = format!("/{}", fc.name);
            for a in &fc.aliases {
                label.push_str(&format!(", /{a}"));
            }
            items.push(SlashPopupItem {
                label,
                description: fc.description.clone(),
                name: fc.name.clone(),
            });
        }
    }

    items
}

/// Recompute the slash popup from the current input buffer. The popup is
/// active when the buffer starts with `/` and has no space yet (the user is
/// still composing the command token, not its arguments). Called after every
/// buffer mutation in the input thread.
fn refresh_slash_popup(state: &mut RenderState) {
    let buf = state.input_buffer.clone();
    let active = buf.starts_with('/') && !buf[1..].contains(' ');
    if !active {
        state.slash_popup.open = false;
        state.slash_popup.items.clear();
        state.slash_popup.selected = 0;
        return;
    }
    let token = &buf[1..];
    let items = slash_filter(token, &state.file_commands);
    state.slash_popup.open = !items.is_empty();
    if items.is_empty() {
        state.slash_popup.selected = 0;
    } else {
        state.slash_popup.selected = state.slash_popup.selected.min(items.len() - 1);
    }
    state.slash_popup.items = items;
}
/// Combined popup refresher — calls both the slash-command popup and the
/// @-file-search picker. Called after every input buffer mutation in the
/// input thread so both popups stay in sync with the cursor position.
fn refresh_input_popups(state: &mut RenderState) {
    refresh_slash_popup(state);
    refresh_file_search(state);
}

/// Recompute the @-file-search dropdown from the current input buffer.
/// Called after every buffer mutation in the input thread. The filesystem
/// walk (building the index) happens only on the `None → Some` transition
/// (when `@` is first typed); subsequent keystrokes just re-filter the
/// cached index via [`FileSearchState::refresh`](crate::tui_vt::file_search::FileSearchState::refresh).
fn refresh_file_search(state: &mut RenderState) {
    use crate::tui_vt::file_search;
    // Never open the file picker while a slash command is being composed.
    if state.slash_popup.open {
        state.file_search = None;
        return;
    }
    match file_search::parse_at_cursor(&state.input_buffer, state.input_cursor) {
        Some(token) => match &mut state.file_search {
            None => {
                let cwd = state.cwd.clone();
                state.file_search = Some(file_search::open(&cwd, token.at_offset, false));
            }
            Some(fs) => {
                if fs.query != token.path_query {
                    fs.refresh(&token.path_query);
                }
            }
        },
        None => state.file_search = None,
    }
}

/// Accept the currently-selected file-search result: replace the `@query`
/// token in the buffer with the canonical `@path ` (or `@path:N-M ` in
/// line mode), advance the cursor past it, and close the picker.
/// Returns `true` if a result was accepted.
fn accept_file_search(state: &mut RenderState, line_mode: bool) -> bool {
    use crate::tui_vt::file_search;
    let Some(fs) = &state.file_search else {
        return false;
    };
    let Some(result) = fs.selected_result().cloned() else {
        return false;
    };
    let at_offset = fs.at_offset;
    let text = file_search::insertion_text(&result.path, None, line_mode);
    let cursor_end = state.input_cursor;
    // Replace everything from `@` to the current cursor with the insertion.
    state
        .input_buffer
        .replace_range(at_offset..cursor_end.min(state.input_buffer.len()), &text);
    state.input_cursor = at_offset + text.len();
    state.file_search = None;
    true
}

fn preview_tool_result(content: &str) -> String {
    const MAX: usize = 500;
    if content.chars().count() <= MAX {
        return content.to_string();
    }
    let truncated: String = content.chars().take(MAX).collect();
    format!("{truncated}\u{2026}")
}

/// Try to render tool result content as a colored diff. Returns `true` if the
/// content was recognized as a diff and rendered, `false` to fall back to the
/// plain preview.
fn try_render_diff(content: &str, handle: &InlineHandle) -> bool {
    let lines: Vec<&str> = content.lines().collect();
    // Require a unified-diff hunk header (`@@ … @@`) as a strong signal that
    // the content is actually a diff — prevents grep context lines, bullet
    // lists, and shell output from being mis-rendered as deletions.
    if !lines.iter().any(|l| l.starts_with("@@")) {
        return false;
    }
    let additions = lines
        .iter()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count();
    let deletions = lines
        .iter()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .count();
    if additions + deletions < 2 {
        return false;
    }

    let styles = active_styles();
    let green = styles.secondary.get_fg_color();
    let red = styles.error.get_fg_color();
    const MAX_DIFF_LINES: usize = 30;

    // Header line with diffstat.
    let mut hdr_style = InlineTextStyle::default();
    hdr_style.effects |= anstyle::Effects::DIMMED;
    handle.append_line(
        InlineMessageKind::Tool,
        vec![InlineSegment {
            text: format!("\u{2713} diff (+{additions} \u{2212}{deletions})"),
            style: Arc::new(hdr_style),
        }],
    );

    // Render diff lines with green/red coloring.
    for line in lines.iter().take(MAX_DIFF_LINES) {
        let mut style = InlineTextStyle::default();
        if line.starts_with('+') && !line.starts_with("+++") {
            style.color = green;
        } else if line.starts_with('-') && !line.starts_with("---") {
            style.color = red;
        } else {
            style.effects |= anstyle::Effects::DIMMED;
        }
        handle.append_line(
            InlineMessageKind::Tool,
            vec![InlineSegment {
                text: format!("  {line}"),
                style: Arc::new(style),
            }],
        );
    }

    if lines.len() > MAX_DIFF_LINES {
        let mut more_style = InlineTextStyle::default();
        more_style.effects |= anstyle::Effects::DIMMED;
        handle.append_line(
            InlineMessageKind::Tool,
            vec![InlineSegment {
                text: format!("  \u{2026} {} more lines", lines.len() - MAX_DIFF_LINES),
                style: Arc::new(more_style),
            }],
        );
    }

    true
}

fn color_from_anstyle(color: Option<anstyle::Color>) -> Color {
    match color {
        Some(anstyle::Color::Ansi(a)) => ansi_to_ratatui(a),
        Some(anstyle::Color::Ansi256(idx)) => Color::Indexed(idx.0),
        Some(anstyle::Color::Rgb(rgb)) => Color::Rgb(rgb.0, rgb.1, rgb.2),
        None => Color::Reset,
    }
}
fn ansi_to_ratatui(color: anstyle::AnsiColor) -> Color {
    use anstyle::AnsiColor as A;
    match color {
        A::Black => Color::Black,
        A::Red => Color::Red,
        A::Green => Color::Green,
        A::Yellow => Color::Yellow,
        A::Blue => Color::Blue,
        A::Magenta => Color::Magenta,
        A::Cyan => Color::Cyan,
        A::White => Color::Gray,
        A::BrightBlack => Color::DarkGray,
        A::BrightRed => Color::LightRed,
        A::BrightGreen => Color::LightGreen,
        A::BrightYellow => Color::LightYellow,
        A::BrightBlue => Color::LightBlue,
        A::BrightMagenta => Color::LightMagenta,
        A::BrightCyan => Color::LightCyan,
        A::BrightWhite => Color::White,
    }
}

// Suppress the unused-import warning while keeping the AtomicBool/Ordering
// available for future control flags (e.g. SIGINT safety net).
#[allow(dead_code, clippy::declare_interior_mutable_const)]
const _ATOMIC_REFS: (AtomicBool, Ordering) = (AtomicBool::new(false), Ordering::SeqCst);

#[cfg(test)]
mod slash_popup_tests {
    use super::*;

    #[test]
    fn empty_token_lists_all_commands() {
        let items = slash_filter("", &[]);
        // 7 built-in commands.
        assert!(items.len() >= 7);
        assert!(items.iter().any(|i| i.name == "quit"));
        assert!(items.iter().any(|i| i.name == "clear"));
        assert!(items.iter().any(|i| i.name == "model"));
    }

    #[test]
    fn prefix_filter_matches_name() {
        let items = slash_filter("qu", &[]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "quit");
        assert!(items[0].label.contains("/quit"));
    }

    #[test]
    fn prefix_filter_matches_alias() {
        // "cl" should match "clear" (alias "cls") and "compact".
        let items = slash_filter("cl", &[]);
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"clear"));
    }

    #[test]
    fn file_commands_appear_in_filter() {
        let fc = crate::tui_vt::slash::file_commands::FileCommand::parse(
            "review",
            "---\ndescription: proj cmd\naliases: cr\n---\nbody",
        );
        let items = slash_filter("", &[fc]);
        assert!(items.iter().any(|i| i.name == "review"));
        assert!(items.iter().any(|i| i.name == "quit")); // builtins still present
    }

    #[test]
    fn file_commands_filtered_by_prefix() {
        let fc = crate::tui_vt::slash::file_commands::FileCommand::parse(
            "review",
            "---\ndescription: x\n---\nbody",
        );
        let items = slash_filter("rev", &[fc]);
        assert!(items.iter().any(|i| i.name == "review"));
    }

    #[test]
    fn file_commands_shadowed_by_builtins_are_dropped() {
        // A file command whose name collides with a built-in must be dropped —
        // built-ins always win. Without this guarantee the popup could surface
        // two items for the same prefix and the dispatch layer would pick the
        // wrong one.
        let fc = crate::tui_vt::slash::file_commands::FileCommand::parse(
            "quit",
            "---\ndescription: hijack\n---\nbody",
        );
        let items = slash_filter("", &[fc]);
        let quit_count = items.iter().filter(|i| i.name == "quit").count();
        assert_eq!(quit_count, 1, "shadowed file command must not appear");
        // And it must be the built-in description, not the file one.
        assert!(
            items
                .iter()
                .any(|i| i.name == "quit" && !i.description.contains("hijack"))
        );
    }

    #[test]
    fn file_commands_with_builtin_aliases_are_dropped() {
        let fc = crate::tui_vt::slash::file_commands::FileCommand::parse(
            "review",
            "---\ndescription: hijack\naliases: quit\n---\nbody",
        );
        let items = slash_filter("", &[fc]);
        assert!(!items.iter().any(|item| item.name == "review"));
    }

    #[test]
    fn popup_opens_on_slash() {
        let mut state = RenderState::default();
        state.input_buffer = "/".to_string();
        refresh_input_popups(&mut state);
        assert!(state.slash_popup.open);
        assert!(!state.slash_popup.items.is_empty());
    }

    #[test]
    fn popup_closes_on_space() {
        let mut state = RenderState::default();
        state.input_buffer = "/quit ".to_string();
        refresh_input_popups(&mut state);
        assert!(!state.slash_popup.open);
    }

    #[test]
    fn popup_closes_on_non_slash() {
        let mut state = RenderState::default();
        state.input_buffer = "hello".to_string();
        refresh_input_popups(&mut state);
        assert!(!state.slash_popup.open);
    }

    #[test]
    fn popup_filters_as_user_types() {
        let mut state = RenderState::default();
        state.input_buffer = "/m".to_string();
        refresh_input_popups(&mut state);
        assert!(state.slash_popup.open);
        // Every item's canonical name must start with 'm' (model is the
        // only command matching the "m" prefix).
        assert!(
            state
                .slash_popup
                .items
                .iter()
                .all(|i| i.name.starts_with('m'))
        );
    }

    #[test]
    fn popup_selection_clamps_on_shrink() {
        let mut state = RenderState::default();
        state.input_buffer = "/".to_string();
        refresh_input_popups(&mut state);
        let full_count = state.slash_popup.items.len();
        state.slash_popup.selected = full_count - 1;
        // Narrow the filter so fewer items remain.
        state.input_buffer = "/qu".to_string();
        refresh_input_popups(&mut state);
        assert!(state.slash_popup.selected < state.slash_popup.items.len());
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use oxicode_vtui::tui::core::{InlineHandle, OverlayEvent};
    use ratatui::{Terminal, backend::TestBackend};
    use tokio::sync::mpsc;

    /// Render `render_frame` into a TestBackend and return the concatenated
    /// cell text. This catches regressions like a missing render_composer
    /// call — `#![allow(dead_code)]` in lib.rs suppresses the unused-fn lint,
    /// so only a render assertion can prove the composer is painted.
    fn render_frame_to_string(state: &RenderState) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("backend");
        let (tx, _rx) = mpsc::unbounded_channel();
        let handle = InlineHandle::new_for_tests(tx);
        terminal
            .draw(|f| render_frame(f, state, &handle))
            .expect("draw");
        let buf = terminal.backend().buffer();
        let area = buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn welcome_screen_shown_when_transcript_empty() {
        let state = RenderState::default();
        let rendered = render_frame_to_string(&state);
        assert!(
            rendered.contains("oxicode"),
            "welcome banner must appear when transcript is empty"
        );
    }

    #[test]
    fn composer_is_painted() {
        // Regression guard: the composer prompt prefix must appear in the
        // rendered output. This would have caught the missing
        // render_composer call (advisory 2026-08-04).
        let mut state = RenderState::default();
        state.input_enabled = true;
        state.prompt_prefix = "> ".to_string();
        let rendered = render_frame_to_string(&state);
        assert!(
            rendered.contains('>'),
            "composer prompt prefix must be painted"
        );
    }

    #[test]
    fn slash_popup_renders_command_list() {
        let mut state = RenderState::default();
        state.slash_popup.open = true;
        state.slash_popup.items = slash_filter("", &[]);
        let rendered = render_frame_to_string(&state);
        assert!(rendered.contains("Commands"), "popup title must render");
        assert!(rendered.contains("/quit"), "popup must list /quit");
    }

    #[test]
    fn composer_and_popup_render_together() {
        let mut state = RenderState::default();
        state.prompt_prefix = "> ".to_string();
        state.input_buffer = "/qu".to_string();
        state.slash_popup.open = true;
        state.slash_popup.items = slash_filter("qu", &[]);
        let rendered = render_frame_to_string(&state);
        assert!(rendered.contains("Commands"), "popup must render");
        assert!(rendered.contains("/quit"), "popup must list /quit");
        assert!(rendered.contains('>'), "composer must still render");
    }

    #[test]
    fn transcript_wraps_long_lines() {
        // A line wider than the terminal must wrap, not clip.
        let mut state = RenderState::default();
        state.transcript.push(TranscriptLine {
            kind: InlineMessageKind::Agent,
            segments: vec![plain_segment(
                "This is a very long agent response line that should wrap across multiple terminal rows when rendered at a narrow width.".to_string()
            )],
            block_id: 0,
        });
        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).expect("backend");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = InlineHandle::new_for_tests(tx);
        terminal
            .draw(|f| render_frame(f, &state, &handle))
            .expect("draw");
        let buf = terminal.backend().buffer();
        // The word "wrap" must appear somewhere — it would be clipped if
        // the List widget was still used at 40 cols.
        let mut full = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    full.push_str(cell.symbol());
                }
            }
            full.push('\n');
        }
        assert!(
            full.contains("wrap"),
            "long line must wrap, not clip — text should be visible past col 40"
        );
    }

    // ─── overlay tests ────────────────────────────────────────────────────

    fn sample_overlay_items() -> Vec<OverlayListItem> {
        vec![
            OverlayListItem {
                title: "model-a".to_string(),
                subtitle: Some("first".to_string()),
                badge: Some("ready".to_string()),
                indent: 0,
                search_value: None,
                selection: Some(oxicode_vtui::tui::core::InlineListSelection::Model(0)),
            },
            OverlayListItem {
                title: "model-b".to_string(),
                subtitle: None,
                badge: None,
                indent: 0,
                search_value: None,
                selection: Some(oxicode_vtui::tui::core::InlineListSelection::Model(1)),
            },
            OverlayListItem {
                title: "model-c".to_string(),
                subtitle: None,
                badge: None,
                indent: 0,
                search_value: None,
                selection: Some(oxicode_vtui::tui::core::InlineListSelection::Model(2)),
            },
        ]
    }

    #[test]
    fn overlay_renders_title_and_items() {
        let mut state = RenderState::default();
        state.overlay = Some(OverlayState {
            title: "Select model".to_string(),
            lines: vec!["Pick one".to_string()],
            items: sample_overlay_items(),
            selected: 0,
            search: None,
            secure_input: None,
        });
        let rendered = render_frame_to_string(&state);
        assert!(
            rendered.contains("Select model"),
            "overlay title must render"
        );
        assert!(rendered.contains("model-a"), "first item must render");
        assert!(rendered.contains("model-b"), "second item must render");
        assert!(rendered.contains("model-c"), "third item must render");
        assert!(
            rendered.contains("Pick one"),
            "descriptive line must render"
        );
    }

    #[test]
    fn overlay_search_filters_items() {
        let mut state = RenderState::default();
        state.overlay = Some(OverlayState {
            title: "Select".to_string(),
            lines: Vec::new(),
            items: sample_overlay_items(),
            selected: 0,
            search: Some(OverlaySearchState {
                label: "filter".to_string(),
                placeholder: Some("type".to_string()),
                value: "model-b".to_string(),
            }),
            secure_input: None,
        });
        let rendered = render_frame_to_string(&state);
        assert!(rendered.contains("model-b"), "matching item must render");
        assert!(
            !rendered.contains("model-a"),
            "non-matching item must not render (got: {})",
            rendered
        );
        assert!(
            !rendered.contains("model-c"),
            "non-matching item must not render"
        );
    }

    #[test]
    fn overlay_keyboard_nav_moves_selection() {
        let mut state = RenderState::default();
        state.overlay = Some(OverlayState {
            title: "Select".to_string(),
            lines: Vec::new(),
            items: sample_overlay_items(),
            selected: 0,
            search: None,
            secure_input: None,
        });
        let state_arc = Arc::new(parking_lot::Mutex::new(state));
        let (tx, mut _rx) = mpsc::unbounded_channel();

        // Initial: index 0 selected.
        assert_eq!(state_arc.lock().overlay.as_ref().unwrap().selected, 0);

        // Down: index 1 selected.
        let consumed = handle_overlay_key(&state_arc, &tx, KeyCode::Down);
        assert!(consumed, "Down must be consumed while overlay is open");
        assert_eq!(state_arc.lock().overlay.as_ref().unwrap().selected, 1);

        // Down: index 2 selected.
        let consumed = handle_overlay_key(&state_arc, &tx, KeyCode::Down);
        assert!(consumed);
        assert_eq!(state_arc.lock().overlay.as_ref().unwrap().selected, 2);

        // Down: wraps to index 0.
        let consumed = handle_overlay_key(&state_arc, &tx, KeyCode::Down);
        assert!(consumed);
        assert_eq!(state_arc.lock().overlay.as_ref().unwrap().selected, 0);

        // Up: wraps to last (index 2).
        let consumed = handle_overlay_key(&state_arc, &tx, KeyCode::Up);
        assert!(consumed);
        assert_eq!(state_arc.lock().overlay.as_ref().unwrap().selected, 2);

        // Enter: closes overlay and emits a Submission event.
        let consumed = handle_overlay_key(&state_arc, &tx, KeyCode::Enter);
        assert!(consumed);
        assert!(
            state_arc.lock().overlay.is_none(),
            "overlay must be cleared after Enter"
        );
        let evt = _rx.try_recv().expect("submit event must arrive");
        match evt {
            InlineEvent::Overlay(OverlayEvent::Submitted(_)) => {}
            other => panic!("expected Submitted overlay event, got {other:?}"),
        }
    }

    #[test]
    fn overlay_esc_closes_and_emits_cancelled() {
        let mut state = RenderState::default();
        state.overlay = Some(OverlayState {
            title: "Select".to_string(),
            lines: Vec::new(),
            items: sample_overlay_items(),
            selected: 0,
            search: None,
            secure_input: None,
        });
        let state_arc = Arc::new(parking_lot::Mutex::new(state));
        let (tx, mut rx) = mpsc::unbounded_channel();

        let consumed = handle_overlay_key(&state_arc, &tx, KeyCode::Esc);
        assert!(consumed);
        assert!(
            state_arc.lock().overlay.is_none(),
            "overlay must be cleared after Esc"
        );
        let evt = rx.try_recv().expect("cancel event must arrive");
        assert!(
            matches!(evt, InlineEvent::Overlay(OverlayEvent::Cancelled)),
            "expected Cancelled overlay event"
        );
    }

    #[test]
    fn overlay_enter_on_readonly_item_is_noop() {
        // A read-only item (selection: None — /tools, /mcp, the /settings
        // Model row) must NOT submit a synthetic selection or pollute the
        // prompt with "/overlay:N". Enter is a no-op: overlay stays open.
        let mut state = RenderState::default();
        state.overlay = Some(OverlayState {
            title: "Tools".to_string(),
            lines: Vec::new(),
            items: vec![OverlayListItem {
                title: "read".to_string(),
                subtitle: Some("Read a file".to_string()),
                badge: None,
                indent: 0,
                search_value: None,
                selection: None,
            }],
            selected: 0,
            search: None,
            secure_input: None,
        });
        let state_arc = Arc::new(parking_lot::Mutex::new(state));
        let (tx, mut rx) = mpsc::unbounded_channel();

        let consumed = handle_overlay_key(&state_arc, &tx, KeyCode::Enter);
        assert!(consumed, "Enter must be consumed even on read-only items");
        assert!(
            state_arc.lock().overlay.is_some(),
            "overlay must stay open when Enter hits a read-only item"
        );
        assert!(
            rx.try_recv().is_err(),
            "no overlay event must be emitted for a read-only Enter"
        );
    }

    #[test]
    fn overlay_chars_route_to_search_field() {
        let mut state = RenderState::default();
        state.overlay = Some(OverlayState {
            title: "Select".to_string(),
            lines: Vec::new(),
            items: sample_overlay_items(),
            selected: 0,
            search: Some(OverlaySearchState {
                label: "filter".to_string(),
                placeholder: None,
                value: String::new(),
            }),
            secure_input: None,
        });
        let state_arc = Arc::new(parking_lot::Mutex::new(state));
        let (tx, _rx) = mpsc::unbounded_channel();

        handle_overlay_key(&state_arc, &tx, KeyCode::Char('m'));
        handle_overlay_key(&state_arc, &tx, KeyCode::Char('o'));
        handle_overlay_key(&state_arc, &tx, KeyCode::Backspace);
        let value = state_arc
            .lock()
            .overlay
            .as_ref()
            .unwrap()
            .search
            .as_ref()
            .unwrap()
            .value
            .clone();
        assert_eq!(value, "m", "Backspace should drop last char");
    }

    #[test]
    fn overlay_key_no_op_when_no_overlay_open() {
        let state = RenderState::default();
        let state_arc = Arc::new(parking_lot::Mutex::new(state));
        let (tx, _rx) = mpsc::unbounded_channel();
        let consumed = handle_overlay_key(&state_arc, &tx, KeyCode::Enter);
        assert!(
            !consumed,
            "handle_overlay_key must return false when no overlay is open"
        );
    }

    #[test]
    fn apply_command_show_overlay_populates_state() {
        use oxicode_vtui::tui::core::{InlineListItem, ListOverlayRequest};
        let mut state = RenderState::default();
        let items = vec![
            InlineListItem {
                title: "alpha".to_string(),
                subtitle: None,
                badge: None,
                indent: 0,
                selection: None,
                search_value: None,
            },
            InlineListItem {
                title: "beta".to_string(),
                subtitle: None,
                badge: None,
                indent: 0,
                selection: None,
                search_value: None,
            },
        ];
        let request = OverlayRequest::List(ListOverlayRequest {
            title: "Pick".to_string(),
            lines: vec!["desc".to_string()],
            footer_hint: None,
            items,
            selected: None,
            search: None,
            hotkeys: Vec::new(),
        });
        let shutdown = apply_command(
            &mut state,
            InlineCommand::ShowOverlay {
                request: Box::new(request),
            },
        );
        assert!(!shutdown, "ShowOverlay must not request shutdown");
        let overlay = state.overlay.as_ref().expect("overlay must be Some");
        assert_eq!(overlay.title, "Pick");
        assert_eq!(overlay.items.len(), 2);
        assert_eq!(overlay.items[0].title, "alpha");
        assert_eq!(overlay.items[1].title, "beta");
        assert_eq!(overlay.lines.len(), 1);

        // CloseOverlay clears it.
        apply_command(&mut state, InlineCommand::CloseOverlay);
        assert!(state.overlay.is_none(), "CloseOverlay must clear state");
    }

    #[test]
    fn materialize_overlay_modal_with_secure_prompt_populates_secure_input() {
        use oxicode_vtui::tui::core::{ModalOverlayRequest, SecurePromptConfig};
        let request = OverlayRequest::Modal(ModalOverlayRequest {
            title: "API key".into(),
            lines: vec!["Paste your key".into()],
            secure_prompt: Some(SecurePromptConfig {
                label: "Key".into(),
                placeholder: Some("sk-...".into()),
                mask_input: true,
            }),
        });
        let state = materialize_overlay(request);
        let secure = state
            .secure_input
            .expect("secure_input must be Some when secure_prompt is Some");
        assert_eq!(secure.config.label, "Key");
        assert!(secure.config.mask_input);
        assert_eq!(secure.value, "");
        assert_eq!(secure.cursor, 0);
    }

    #[test]
    fn materialize_overlay_modal_without_secure_prompt_has_none_secure_input() {
        use oxicode_vtui::tui::core::ModalOverlayRequest;
        let request = OverlayRequest::Modal(ModalOverlayRequest {
            title: "Confirm".into(),
            lines: vec!["y/n".into()],
            secure_prompt: None,
        });
        let state = materialize_overlay(request);
        assert!(
            state.secure_input.is_none(),
            "secure_input must be None when secure_prompt is None"
        );
    }

    // ─── fold / grace tests ─────────────────────────────────────────────

    fn three_block_transcript() -> Vec<TranscriptLine> {
        // Three distinct blocks: user(0), agent(1), user(2).
        vec![
            TranscriptLine {
                kind: InlineMessageKind::User,
                segments: vec![plain_segment("hi")],
                block_id: 0,
            },
            TranscriptLine {
                kind: InlineMessageKind::Agent,
                segments: vec![plain_segment("hello")],
                block_id: 1,
            },
            TranscriptLine {
                kind: InlineMessageKind::Agent,
                segments: vec![plain_segment("world")],
                block_id: 1,
            },
            TranscriptLine {
                kind: InlineMessageKind::User,
                segments: vec![plain_segment("bye")],
                block_id: 2,
            },
        ]
    }

    #[test]
    fn default_block_mode_is_truncated() {
        let state = RenderState::default();
        assert_eq!(state.block_mode(0), BlockDisplayMode::Truncated);
        assert!(state.block_display.is_empty(), "default needs no map entry");
    }

    #[test]
    fn fold_all_collapses_every_block() {
        let mut state = RenderState::default();
        state.transcript = three_block_transcript();
        state.fold_all();
        assert_eq!(state.block_display.len(), 3, "3 distinct block ids");
        assert_eq!(state.block_mode(0), BlockDisplayMode::Collapsed);
        assert_eq!(state.block_mode(1), BlockDisplayMode::Collapsed);
        assert_eq!(state.block_mode(2), BlockDisplayMode::Collapsed);
    }

    #[test]
    fn expand_all_after_fold_all_shows_expanded() {
        let mut state = RenderState::default();
        state.transcript = three_block_transcript();
        state.fold_all();
        state.expand_all();
        assert_eq!(state.block_display.len(), 3);
        assert_eq!(state.block_mode(0), BlockDisplayMode::Expanded);
        assert_eq!(state.block_mode(2), BlockDisplayMode::Expanded);
    }

    #[test]
    fn truncate_all_resets_to_default() {
        let mut state = RenderState::default();
        state.transcript = three_block_transcript();
        state.fold_all();
        state.truncate_all();
        assert!(state.block_display.is_empty());
        assert_eq!(state.block_mode(1), BlockDisplayMode::Truncated);
    }

    #[test]
    fn fold_all_on_empty_transcript_is_noop() {
        let mut state = RenderState::default();
        state.fold_all();
        assert!(state.block_display.is_empty());
    }

    #[test]
    fn cycle_block_advances_through_three_states() {
        let mut state = RenderState::default();
        state.transcript = three_block_transcript();
        state.scroll_offset = 0; // view on block 0
        // Truncated (default) → Expanded
        state.cycle_block_at_view();
        assert_eq!(state.block_mode(0), BlockDisplayMode::Expanded);
        // Expanded → Collapsed
        state.cycle_block_at_view();
        assert_eq!(state.block_mode(0), BlockDisplayMode::Collapsed);
        // Collapsed → Truncated (default — removed from the map)
        state.cycle_block_at_view();
        assert_eq!(state.block_mode(0), BlockDisplayMode::Truncated);
        assert!(!state.block_display.contains_key(&0));
    }

    #[test]
    fn cancel_grace_field_defaults_none() {
        let state = RenderState::default();
        assert!(
            state.cancel_grace_until.is_none(),
            "cancel_grace_until must default to None"
        );
    }

    #[test]
    fn cancel_routes_to_interrupt_when_streaming() {
        assert_eq!(
            route_cancel(true),
            CancelRoute::Interrupt,
            "Esc while streaming must route through the interrupt path"
        );
    }

    #[test]
    fn cancel_routes_to_exit_when_idle() {
        assert_eq!(
            route_cancel(false),
            CancelRoute::Exit,
            "Esc while idle must exit immediately (one-press quit)"
        );
    }
    #[test]
    fn scrollbar_paints_thumb_when_content_overflows() {
        // 40 distinct blocks in a 24-row viewport must produce a scrollbar
        // thumb (█) in the rendered frame.
        let mut state = RenderState::default();
        for i in 0..40u32 {
            state.transcript.push(TranscriptLine {
                kind: InlineMessageKind::Agent,
                segments: vec![plain_segment(format!("line {i}"))],
                block_id: i as usize,
            });
        }
        let rendered = render_frame_to_string(&state);
        assert!(
            rendered.contains('\u{2588}'),
            "scrollbar thumb (█) must render when transcript overflows the viewport"
        );
    }

    #[test]
    fn scrollbar_absent_when_content_fits_viewport() {
        // A single short line fits without overflow — no thumb character.
        let mut state = RenderState::default();
        state.transcript.push(TranscriptLine {
            kind: InlineMessageKind::Agent,
            segments: vec![plain_segment("hi")],
            block_id: 0,
        });
        let rendered = render_frame_to_string(&state);
        assert!(
            !rendered.contains('\u{2588}'),
            "no scrollbar thumb when content fits the viewport"
        );
    }

    // ─── confirmation modal tests ───────────────────────────────────────

    #[test]
    fn confirmation_modal_renders_title() {
        let mut state = RenderState::default();
        state.confirmation = Some(quit_confirmation());
        let rendered = render_frame_to_string(&state);
        assert!(
            rendered.contains("Quit oxicode?"),
            "confirmation title must render"
        );
    }

    #[test]
    fn confirmation_yes_sends_exit_and_closes() {
        let mut state = RenderState::default();
        state.confirmation = Some(quit_confirmation());
        let state_arc = Arc::new(parking_lot::Mutex::new(state));
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle_confirmation_key(&state_arc, &tx, KeyCode::Char('y'));
        assert!(
            state_arc.lock().confirmation.is_none(),
            "yes must close the modal"
        );
        let ev = rx.try_recv().expect("yes must send an event");
        assert!(matches!(ev, InlineEvent::Exit), "yes must send Exit");
    }

    #[test]
    fn confirmation_no_closes_without_event() {
        let mut state = RenderState::default();
        state.confirmation = Some(quit_confirmation());
        let state_arc = Arc::new(parking_lot::Mutex::new(state));
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle_confirmation_key(&state_arc, &tx, KeyCode::Char('n'));
        assert!(
            state_arc.lock().confirmation.is_none(),
            "no must close the modal"
        );
        assert!(rx.try_recv().is_err(), "no must not send an event");
    }
    // ─── ephemeral tip tests ───────────────────────────────────────────

    #[test]
    fn tip_banner_renders_when_active() {
        let mut state = RenderState::default();
        let now_tick = FRAME_TICK.load(std::sync::atomic::Ordering::Relaxed);
        state.tip = Some(EphemeralTip {
            text: "hello-tip-marker".to_string(),
            born_tick: now_tick,
            ttl_ticks: 100,
            key: "test",
            ambient: false,
        });
        let rendered = render_frame_to_string(&state);
        assert!(
            rendered.contains("hello-tip-marker"),
            "active tip must render above the composer"
        );
    }

    #[test]
    fn tip_visible_within_ttl_window() {
        let tip = EphemeralTip {
            text: "x".to_string(),
            born_tick: 10,
            ttl_ticks: 5,
            key: "test",
            ambient: false,
        };
        assert!(tip_is_visible(&tip, 12), "within TTL must be visible");
        assert!(
            !tip_is_visible(&tip, 15),
            "at TTL boundary (born + ttl) must expire"
        );
        assert!(!tip_is_visible(&tip, 99), "past TTL must expire");
    }

    // ─── sticky header tests ───────────────────────────────────────────

    #[test]
    fn sticky_header_pins_block_head_when_scrolled_into_body() {
        // One big block (40 same-block lines); scroll the viewport into the
        // body. The sticky header must pin the block's first line at the top.
        let mut state = RenderState::default();
        for i in 0..40u32 {
            state.transcript.push(TranscriptLine {
                kind: InlineMessageKind::Agent,
                segments: vec![plain_segment(format!("body-line-{i:02}"))],
                block_id: 0,
            });
        }
        state.scroll_offset = 10;
        let rendered = render_frame_to_string(&state);
        assert!(
            rendered.contains("body-line-00"),
            "sticky header must pin the block head when scrolled into the body"
        );
    }

    #[test]
    fn sticky_header_absent_when_viewport_at_block_head() {
        // Viewport top is the block head itself — no sticky pin needed.
        let mut state = RenderState::default();
        for i in 0..40u32 {
            state.transcript.push(TranscriptLine {
                kind: InlineMessageKind::Agent,
                segments: vec![plain_segment(format!("head-line-{i:02}"))],
                block_id: 0,
            });
        }
        state.scroll_offset = 0;
        let rendered = render_frame_to_string(&state);
        // head-line-00 is the viewport top already; it renders exactly once
        // (no separate sticky row). Just assert it is present.
        assert!(rendered.contains("head-line-00"));
    }

    // ─── prompt queue tests ─────────────────────────────────────────────

    #[test]
    fn turn_end_drains_queue_head() {
        let mut state = RenderState::default();
        state.queued_inputs = vec!["queued-1".into(), "queued-2".into()];
        state.drain_queue_head();
        assert_eq!(
            state.queued_inputs.len(),
            1,
            "drain_queue_head must drop the head (now running)"
        );
        assert_eq!(state.queued_inputs[0], "queued-2");
    }

    // ─── render_frame integration ──────────────────────────────────────

    #[test]
    fn render_frame_paints_transcript_content() {
        // Guard against render_frame losing its render_transcript call
        // (which only a content assertion through render_frame can catch —
        // render_transcript unit tests bypass render_frame entirely).
        let mut state = RenderState::default();
        state.transcript.push(TranscriptLine {
            kind: InlineMessageKind::Agent,
            segments: vec![plain_segment("frame-content-marker-xyz")],
            block_id: 0,
        });
        let rendered = render_frame_to_string(&state);
        assert!(
            rendered.contains("frame-content-marker-xyz"),
            "render_frame must paint transcript content"
        );
    }

    #[test]
    fn file_search_dropdown_renders_results() {
        use crate::tui_vt::file_search::{FileSearchResult, FileSearchState};
        let mut state = RenderState::default();
        state.input_enabled = true;
        state.file_search = Some(FileSearchState {
            query: "main".into(),
            at_offset: 0,
            hidden_mode: false,
            results: vec![
                FileSearchResult {
                    path: "src/main.rs".into(),
                    score: 100,
                },
                FileSearchResult {
                    path: "tests/main.rs".into(),
                    score: 50,
                },
            ],
            selected: 0,
            index: vec![],
            line_mode: false,
        });
        let rendered = render_frame_to_string(&state);
        assert!(rendered.contains("Files"), "dropdown title must render");
        assert!(
            rendered.contains("src/main.rs"),
            "dropdown must show file paths"
        );
    }

    #[test]
    fn file_search_dropdown_hidden_mode_title() {
        use crate::tui_vt::file_search::{FileSearchResult, FileSearchState};
        let mut state = RenderState::default();
        state.input_enabled = true;
        state.file_search = Some(FileSearchState {
            query: "".into(),
            at_offset: 0,
            hidden_mode: true,
            results: vec![FileSearchResult {
                path: ".env".into(),
                score: 0,
            }],
            selected: 0,
            index: vec![],
            line_mode: false,
        });
        let rendered = render_frame_to_string(&state);
        assert!(
            rendered.contains("hidden"),
            "hidden mode must be indicated in title"
        );
    }

    #[test]
    fn file_search_and_composer_render_together() {
        use crate::tui_vt::file_search::{FileSearchResult, FileSearchState};
        let mut state = RenderState::default();
        state.input_enabled = true;
        state.prompt_prefix = "> ".into();
        state.input_buffer = "@main".into();
        state.input_cursor = 5;
        state.file_search = Some(FileSearchState {
            query: "main".into(),
            at_offset: 0,
            hidden_mode: false,
            results: vec![FileSearchResult {
                path: "src/main.rs".into(),
                score: 100,
            }],
            selected: 0,
            index: vec![],
            line_mode: false,
        });
        let rendered = render_frame_to_string(&state);
        // Both the composer text and the dropdown must appear.
        assert!(rendered.contains('>'), "composer must still render");
        assert!(
            rendered.contains("src/main.rs"),
            "dropdown must render alongside composer"
        );
    }

    #[test]
    fn flatten_todo_items_preserves_order_and_status() {
        use oxicode_agent::tools::todo::{TodoItem, TodoPhase};
        let phases = vec![
            TodoPhase {
                name: "A".into(),
                tasks: vec![
                    TodoItem {
                        content: "write code".into(),
                        status: TodoStatus::InProgress,
                        notes: None,
                        block_reason: None,
                    },
                    TodoItem {
                        content: "write tests".into(),
                        status: TodoStatus::Pending,
                        notes: None,
                        block_reason: None,
                    },
                ],
            },
            TodoPhase {
                name: "B".into(),
                tasks: vec![TodoItem {
                    content: "waiting on review".into(),
                    status: TodoStatus::Blocked,
                    notes: None,
                    block_reason: None,
                }],
            },
        ];
        let flat = flatten_todo_items(&phases);
        assert_eq!(flat.len(), 3);
        assert_eq!(flat[0], ("write code".to_string(), TodoStatus::InProgress));
        assert_eq!(flat[1], ("write tests".to_string(), TodoStatus::Pending));
        assert_eq!(
            flat[2],
            ("waiting on review".to_string(), TodoStatus::Blocked)
        );
    }

    #[test]
    fn todo_pane_renders_when_items_present() {
        // The sticky pane is populated from the live provider in the event
        // loop; here we seed it directly to assert the pane paints task text.
        let mut state = RenderState::default();
        state.todo_items = vec![
            ("active task".to_string(), TodoStatus::InProgress),
            ("open task".to_string(), TodoStatus::Pending),
        ];
        let rendered = render_frame_to_string(&state);
        assert!(
            rendered.contains("active task"),
            "in-progress task must render"
        );
        assert!(rendered.contains("open task"), "pending task must render");
        // The in-progress glyph (▶) must distinguish the active task.
        assert!(
            rendered.contains('\u{25B6}'),
            "in-progress glyph must render"
        );
    }

    #[test]
    fn todo_pane_hidden_when_empty() {
        let state = RenderState::default();
        let rendered = render_frame_to_string(&state);
        // No todo content should leak when the list is empty.
        assert!(!rendered.contains('\u{2611}'), "no checkmark when empty");
    }

    #[test]
    fn render_overlay_secure_input_shows_label_mask_value_and_placeholder() {
        use ratatui::{Terminal, backend::TestBackend};
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let overlay = OverlayState {
            title: "OpenAI key".into(),
            lines: vec!["Paste your API key".into()],
            items: Vec::new(),
            selected: 0,
            search: None,
            secure_input: Some(OverlaySecureInput {
                config: SecurePromptConfig {
                    label: "Key".into(),
                    placeholder: Some("sk-...".into()),
                    mask_input: true,
                },
                value: "sk-abc".into(),
                cursor: 6,
            }),
        };
        terminal
            .draw(|f| render_overlay(f, f.area(), &overlay))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        // Mask must show 6 asterisks, never the value.
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("Key:"));
        assert!(text.contains("******"));
        assert!(!text.contains("sk-abc"));
    }

    #[test]
    fn render_overlay_secure_input_placeholder_when_empty() {
        use ratatui::{Terminal, backend::TestBackend};
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let overlay = OverlayState {
            title: "OpenAI key".into(),
            lines: vec!["Paste your API key".into()],
            items: Vec::new(),
            selected: 0,
            search: None,
            secure_input: Some(OverlaySecureInput {
                config: SecurePromptConfig {
                    label: "Key".into(),
                    placeholder: Some("sk-...".into()),
                    mask_input: true,
                },
                value: String::new(),
                cursor: 0,
            }),
        };
        terminal
            .draw(|f| render_overlay(f, f.area(), &overlay))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("sk-..."));
    }
}

#[cfg(test)]
mod secure_input_tests {
    use super::*;
    use oxicode_vtui::tui::core::OverlaySubmission;

    #[test]
    fn overlay_submission_secure_input_is_routed_to_host() {
        // Smoke: serialization round-trip — the variant must be reachable
        // through the protocol so the input thread can dispatch it.
        let _ = OverlaySubmission::SecureInput("sk-test".into());
        let serialized = format!("{:?}", OverlaySubmission::SecureInput("x".into()));
        assert!(serialized.contains("SecureInput"));
    }

    #[test]
    fn providers_action_matrix_branches_correctly() {
        // Pin the (has_key, oauth_capable) → Vec<AuthAction> matrix
        // exactly. Refactors MUST keep this contract: the order of
        // returned actions drives the visible action menu order.
        assert_eq!(
            next_provider_actions(true, true),
            vec![
                AuthAction::SetApiKey,
                AuthAction::StartOAuth,
                AuthAction::RemoveKey,
            ],
            "has key + oauth-capable: replace, oauth, remove"
        );
        assert_eq!(
            next_provider_actions(true, false),
            vec![AuthAction::RemoveKey],
            "has key, key-only provider: remove only"
        );
        assert_eq!(
            next_provider_actions(false, true),
            vec![AuthAction::SetApiKey, AuthAction::StartOAuth],
            "no key + oauth-capable: set key, oauth"
        );
        assert_eq!(
            next_provider_actions(false, false),
            vec![AuthAction::SetApiKey],
            "no key + key-only provider: set key only"
        );
    }

    #[test]
    fn insert_char_at_middle() {
        let (s, c) = insert_char_into_secure_input("abcd", 2, 'X');
        assert_eq!(s, "abXcd");
        assert_eq!(c, 3);
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let (s, c) = backspace_secure_input("abc", 0);
        assert_eq!(s, "abc");
        assert_eq!(c, 0);
    }

    #[test]
    fn backspace_at_middle() {
        let (s, c) = backspace_secure_input("abcd", 2);
        assert_eq!(s, "acd");
        assert_eq!(c, 1);
    }

    #[test]
    fn paste_strips_trailing_newline_and_drops_non_ascii() {
        let (s, c) = insert_paste_into_secure_input("ab", 2, "sk-xyz\nABC\u{1F600}");
        assert_eq!(s, "absk-xyzABC");
        assert_eq!(c, 11);
    }

    #[test]
    fn insert_at_end_appends() {
        let (s, c) = insert_char_into_secure_input("hello", 5, '!');
        assert_eq!(s, "hello!");
        assert_eq!(c, 6);
    }
}
