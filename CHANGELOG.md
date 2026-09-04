# Changelog

All notable changes to the oxicode project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `grep.search.v2` — `grep` now runs on a streaming, ignore-aware,
  cancellable ExactSearchEngine (replaces `grep.search.v1`;
  gitignore/.ignore/global/exclude respected, built-in artifact exclusions
  kept, real glob `include`, cancellation returns partial results with a
  `cancelled` marker). The legacy walker remains reachable via
  `ToolRegistry::with_builtins_cwd` until 0.83.
- `workspace_search` (opt-in, `[workspace_search] enabled = true`): semantic
  workspace discovery over the local oxibrain document plane via a daemonless
  `admin serve --stdio` child. Registers the workspace as an idempotent
  document root, searches the documents plane (hybrid), post-filters by root
  alias, and returns ranked chunk-level passages with a freshness report and
  a verify-with-read/grep reminder. Zero startup cost when disabled.

### Changed

- Behavior change: files matched by repository ignore files no longer appear
  in `grep` results; pass an explicit `path` to search ignored trees.

## [0.81.0] — 2026-09-01

### Added

- **Tool routing for the persistent runtimes.** The `coding-omp-v1` pack
  now routes its exposed `bash`/`eval`/`debug` tools through the host's
  persistent runtimes when wired (`BehaviorSessionServices`): the bash
  tool executes in one long-lived session (cwd/env persist across calls,
  OMP session semantics), the eval tool keeps interpreter state across
  cells with functional `reset`, and the debug tool drives real DAP
  sessions (launch/attach, breakpoints, stepping, inspection, evaluate,
  terminate) returning adapter JSON. Without services, the legacy
  per-invocation implementations are used and the manifest degrades
  honestly, exactly as before. Implementation ids bumped per the
  replacement policy: `bash.session.v1` (replaces `bash.process.v1`),
  `eval.kernel.v2` (replaces `eval.kernel.v1`), `debug.dap.v2` (replaces
  `debug.dap.v1`). New routed-tool fixtures
  (`routed_bash_session_persistence`, `routed_eval_kernel_persistence`,
  `routed_debug_dap_lifecycle`, `routed_tools_fall_back_to_legacy`)
  advance persistent-shell/eval/dap-debugging to **Equivalent**; the CLI
  now wires the runtimes (all lazily spawned). The bundled shell session
  merges stderr into stdout (`exec 2>&1`) so tool output carries
  diagnostics.

### Changed

- `chacha20` 0.10.1 (yanked) → 0.10.2 via `cargo update` (transitive of
  lopdf → printpdf → oxibrowser-render, `native-browser` feature path).
- `RUSTSEC-2026-0269` (wasmtime 41.x sandbox escape, same extism 1.21
  pin) and `RUSTSEC-2026-0253` (lru 0.16.4 panic-soundness via
  azul-layout → printpdf, no 0.16.x fix) tracked in `.cargo/audit.toml`
  and `deny.toml` with justifications — `cargo audit` is clean again.

## [0.80.0] — 2026-09-01

### Added

- **Behavior packs: `oxicode_sdk::behavior` (feature `behavior`).** A
  behavior pack is a declarative, versioned contract — tool descriptors
  (implementation id + model-visible name + advisory capability/side-effect
  classes + port requirements + state scope), runtime extension specs with
  declared lifetimes, prompt layers, and a machine-readable compatibility
  ledger — installed through a host-controlled `BehaviorToolInstaller`
  interception point. `BehaviorPackResolver` resolves packs
  deterministically (explicit `replaces` overlays; duplicate model-visible
  names rejected). Installs produce an `InstalledBehaviorManifest` with
  structured degradation records and an honest compatibility rollup,
  distinct from the lifecycle `ToolManifest`. Design:
  `docs/designs/2026-08-31-omp-compatible-behavior-pack-design.md`.
- **`coding-omp-v1` reference pack.** 16 canonical coding tools
  (read/write/edit/bash/grep/find/ls/ast_grep/ast_edit/web_search/
  get_search_results/todo/subagent/lsp/eval/debug), seven declared
  extensions (HashlineState required; LSP/Shell/Eval/Debug/TTSR/
  Delegation optional), a coding-discipline prompt layer, and a ledger
  pinned to `omp@v18.0.11`: read/write/search + hashline anchors
  Equivalent (fixture-evidenced); LSP/TTSR/delegation Partial; persistent
  shell/eval/DAP Partial — the `ShellSession`/`EvalKernel`/`DebugService`
  contracts now ship reference implementations (see below), while the
  exposed pack tools keep legacy per-call semantics until tool routing
  lands.
- **Persistent coding runtimes (`oxicode-agent/src/runtime/`).**
  `PersistentShellSession`: one long-lived `bash --noprofile --norc` per
  session (own process group, `trap : INT`), marker-terminated commands so
  cwd/env persist across calls, group-SIGINT cancel surfacing exit 130,
  bounded output, explicit reset. `PythonEvalKernel` (`python3 -q -u -i`)
  and `JavaScriptEvalKernel` (`node -i`, bun fallback): persistent
  interpreter/REPL state across cells with inline (filesystem-free) cell
  shipping, bounded stderr-tail error capture, and explicit reset.
  `DapClient`/`DapDebugService`: Content-Length framed DAP over stdio with
  a full launch lifecycle (initialize → launch/attach → `stopped`
  observability → typed-passthrough requests → terminate). All three are
  contract-tested by new behavior fixtures
  (`persistent_shell_session_contract`, `persistent_eval_kernel_contract`,
  `dap_service_protocol_scenario`); hosts wire them through
  `BehaviorSessionServices` — the CLI keeps them unwired for now.
- **CLI consumes `coding-omp-v1`.** The engine's shared tool registry is
  composed before App/Agent construction: legacy builtins register first,
  then the pack installer overwrites the same names with pack-built
  equivalents and applies the patch (session-local hashline snapshot
  store — newly wired for the CLI — plus the prompt layer). `--tools` and
  `disabled_tools` surface as structured `DisabledByHost` degradations.
  Startup logs `behavior pack installed` with tool/degradation counts and
  the compatibility level.
- **Behavior fixtures (CI).** Deterministic scenarios in
  `oxicode-sdk/tests/behavior/`: stale-anchor recovery,
  honest-degradation report, duplicate/overlay replacement end-to-end,
  host-denial bypass-proof, child-agent runner contract, LSP mock
  actions, TTSR patch + rule matching. No network, paid model, TUI, or
  OMP binary required.

## [0.79.0] — 2026-08-30

### Added

- **Unified Oxi home layout (`~/.oxi/oxicode`).** The canonical oxicode
  home is now resolved through one resolver: `oxicode_home()` =
  `$OXICODE_HOME`, else `$OXI_HOME/oxicode`, else `$HOME/.oxi/oxicode`
  (`oxicode-catalog/src/oxi_home.rs`, re-exported as
  `oxicode_ai::oxi_home`). All runtime defaults (auth, settings, sessions,
  skills, extensions, packages, agents, WATCHDOG.md, slash commands,
  runtime config, hook approvals) route through it; readers fall back
  read-only to the legacy `~/.oxicode` when the canonical item is absent,
  and an explicit `$OXICODE_HOME` never silently merges legacy. Writes
  always land in the canonical home.
- **`oxicode migrate home [--dry-run]`.** Journaled, resumable,
  copy-only migration of the legacy home into the unified layout
  (`oxicode-cli/src/home_migrate.rs`). Preflight reports
  `NothingToDo`/`Ready`/`AlreadyMigrated`/`Conflict` (content mismatch ⇒
  abort, both paths reported); the journal
  (`<oxi_home>/oxicode.migration-journal.json`) is written atomically
  before the first mutation; per-file idempotent copy (size + SHA-256
  skip, `.part-<pid>` + fsync + rename); verify step re-walks both trees.
  The legacy source is never modified or deleted. `oxicode doctor` (new)
  prints the active home resolution, legacy presence, and migration
  status; `oxicode reset` now targets the canonical home (legacy only
  when the canonical home is absent).

## [0.78.0] - 2026-08-29

### Fixed

- **Per-process TUI ownership identity.** Every TUI used to share the
  constant liveness id `"tui"` (and a failed flock acquisition was silently
  ignored), so two parallel interactive sessions collapsed into one owner:
  the second TUI passed `require_owner` on issues the first had claimed and
  could mutate them. The identity is now `tui-<pid>-<uuid>` (unique per
  process, same scheme as headless `proc-*`), the shared constant is gone,
  all in-TUI surfaces name the holder through `RenderState.ownership_session_id`,
  and a failed flock acquisition logs a warning instead of being swallowed.

- **Flock-holder provenance.** `liveness::acquire` now records owner
  metadata (session, pid, host, cwd, started-at) inside the
  `.alive/<session_id>` lock file itself; `read_owner_info` reads it back
  and `format_issue_full` shows it (`lock: pid N on host …`). Issue lines
  now show the assignment age (`assigned: xxx since MM-DD HH:MM`) in both
  the agent tool and `oxicode issue` output, and the panel's edit-gate
  error includes the claim age.

- **`oxicode issue close`/`reopen` recover from stale hashes.** The CLI is
  a single-shot CAS caller, so an outdated `--hash` (or a file mutated
  between read and write) failed outright. Both subcommands now run through
  `cas_retry`, re-reading a fresh hash on conflict — matching the agent
  tool's and the TUI panel's behavior.


## [0.77.0] - 2026-08-29

### Fixed

- **Model context windows now follow models.dev end to end.** Four bugs
  made big-context models (e.g. `zai/glm-5.2`, 1M tokens) display wrong
  limits and compact far too early: (1) the TUI CTX denominator was
  hardcoded `128_000` at startup and never re-synced, and the MODEL chip
  kept the boot model after `/model` — both now update on every model
  switch (overlay pickers, `/model`, `/model next`) and at startup via
  `sync_model_chips`; (2) `AgentConfig.context_window` was a `128_000`
  placeholder at every construction site — `Agent::new*`,
  `switch_model`, and `switch_to_model` now sync it from the resolved
  model, so threshold compaction fires against the real window (a 1M
  model previously compacted at ~102k, 8x too early); (3) the
  hand-written `STATIC_MODELS` registry shadowed the catalog with stale
  numbers (gemini-2.5-pro 2M vs 1_048_576, zai/glm-4.7 200k vs 204_800,
  anthropic max_tokens 8192 vs 64k) — numeric metadata is now refreshed
  from the models.dev catalog at registry init, keeping hand-maintained
  transport quirks (`compat`, base URLs) intact; (4) custom-provider and
  LOCAL `/v1/models` discovery registered models with placeholder
  `128_000`/`0` windows — ids matching upstream models now cross-fill
  real metadata from the catalog (`find_entry_by_model_id`), and unknown
  windows display as `? ctx` instead of `0`/`0K`. The embedded models.dev
  snapshot was refreshed to 2026-08 (194 providers / 7290 models; adds
  `zai/glm-5.2`, drops retired legacy ids), and snapshot-count tests now
  pin floors instead of exact counts so future refreshes don't break the
  build.

### Changed

- **TUI: transcript is now a plain surface (omp-style).** The accent
  rail column (`┃`), its running-wave animation, and the `> ` user
  prefix are gone. Speaker identity is weight and color only: user
  input renders bold in the primary color, agent responses in the
  default ink, tools/shell in their kind colors; a blank spacer row
  before each user block remains the turn boundary. System severities
  (`error:` `warning:` `info:` `policy:`) keep their first-line label,
  and the sticky block header keeps its faint background tint. The
  reclaimed rail column belongs to the content.

- **TUI: top status bar removed.** The 1-row status bar (`OXICODE | …`
  chrome) was pure space cost — every session fact already lives on the
  composer's top border. The app-badge prefix was dropped from the border
  (branding stays on the welcome card only), the oxibrain health chip
  (`brain·ok`/`brain·down`, info/error colored) moved to the right side of
  the shortcuts bar ahead of the transcript position (`line N/M`), and the
  reclaimed row belongs to the scrollback. `oxicode-vtui`'s
  `StatusBar`/`StatusBarBuilder` widgets and `AgentViewLayout::status_bar`
  were deleted with it; `ShortcutsBar` gained a `right(...)` status line.

- **oxibrain integration fixed.** The memory backend now connects to the
  daemon's real socket (`~/.oxi/brain/oxibrain.sock`, override
  `OXIBRAIN_SOCKET`) instead of the never-used `XDG_RUNTIME_DIR` resolution,
  and calls the daemon's actual tool set (`remember`/`search`/`retract`
  instead of nonexistent `memory.put`/`memory.delete`). `memory_enabled`
  now defaults to `true`; the backend is gated on daemon socket presence,
  not on a Foundation install.
- **TUI memory visibility.** The TUI gains a `brain·ok`/`brain·down`
  health indicator (background ping every 20s; rendered on the shortcuts
  bar's right side) and a `/memory` slash command
  (aliases `/brain`, `/mem`) showing socket path, enabled state, daemon
  stats, and recovery hints.

### Removed

- **`oxicode-mnemopi` crate deleted** (local SQLite vector memory engine).
  The oxibrain daemon is the only durable-memory authority; there is no
  local fallback. The cli's local memory stack (sqlite/mnemopi backends,
  workers, summary, extraction) and the `embedding_*` / `memory_backend`
  settings were removed with it. `oxicode-sdk`'s `EmbeddingProvider` port
  remains for feature-gated consumers.
- **`oxicode-vtui::vim` deleted** — deprecated since the textarea port;
  the live vim engine is app-owned at `oxicode-cli/src/tui_vt/vim/`.

## [0.76.0] - 2026-08-17

### Added

- **New `oxicode-textarea` crate** — an atomic-mutation text editor widget
  ported from grok's `xai-ratatui-textarea` (MIT, Apache-2.0-compatible).
  Single source of truth for editable text with correct CJK/emoji caret
  positioning, soft-wrap, horizontal scroll, undo/redo, selection, atomic
  `EditBuffer`. 351 tests.
- **Oxi Foundation v1 host** — keychain-backed credentials, profile
  providers, package installation from a foundation lockfile (with
  provenance), and the `oxibrain` daemon as the durable-memory
  authority. The brain client (`oxibrain-client` 0.2) is consumed
  from crates.io so the binary builds on a clean checkout; live
  development override via `[patch.crates-io]` documented in
  `oxicode-cli/Cargo.toml`.
- **New `oxicode migrate brain` command** — one-shot legacy-memory
  import into the brain daemon.
- **TUI: codex-derived presentation primitives** — additive layer
  reused by the host for chrome, agent view, and welcome screens.
- **TUI: restrained slate/teal palette reseed** for the `oxi` theme.
- **TUI `/model` is now a picker, not a transcript line.** Was: a single
  read-only `Current model: <id>` line. Now: a searchable overlay that
  lists every model from providers with a stored API key, pins the
  active model at the top of the list (even when its key has been
  removed mid-session), and falls back to the full catalog when no
  providers are keyed so a fresh TUI is never empty. Selection switches
  the model end-to-end through the existing overlay-submission handler.
  The `next`/`cycle` arm of `/model` still emits the
  "No scoped models configured to cycle" warning (`ScopedModel` is the
  config-curated cycling set and is out of scope here).
- **TUI `/sessions` is now a real resume, not a no-op.** `/sessions <id>`
  (and the `/resume <id>` alias) opens the file via
  `SessionManager::open`, validates the cwd, and atomically swaps the
  live `AgentSessionHandle` so the conversation continues with the prior
  history. Selection from the `/sessions` picker enqueues the same resume
  path (was: re-dispatched to the same picker). `Cannot resume while
  agent is running. Use /cancel first.` gates the same way `/handoff`
  does.

### Changed

- **TUI composer renders through `TextArea`.** The hand-rolled
  `input_buffer: String` + `input_cursor: usize` byte-math is gone; the
  composer now owns an `oxicode_textarea::TextArea` that renders the
  editable body and reports the caret via `cursor_pos_with_state`. This
  fixes the long-standing caret drift on CJK/emoji input (each Hangul is 3
  bytes but 2 columns; the old code counted bytes as columns). Soft-wrap
  and horizontal scroll now keep the caret aligned on long or wide-glyph
  prompts.
- **TUI secure prompt (API-key entry) masks via a `TextElement`,** so the
  real key never reaches a rendered terminal line. The overlay owns an
  `EditBuffer` and paints asterisks through the element's `display`.
- **vim mode relocated into `oxicode-cli`** as an app-owned module;
  `oxicode-vtui::vim` is deprecated (removal in a later release). The vim
  engine mutates the composer's `TextArea` through the existing
  `InputEditor` adapter, so caret/undo stay owned by the editor.
- **TUI host adopts the presentation layer** for chrome layout; the
  `oxicode-vtui` design/layout primitives are now wired as the host's
  rendering source.

### Fixed

- **TUI composer caret no longer double-offsets** on CJK/emoji input.
  `input_cursor` was a byte index combined with a `chars().count()`
  prompt offset — for multi-byte glyphs the result drifted visibly.
  Resolved by the `TextArea` migration above; the helper
  `composer_cursor_position` is now redundant and removed.

### Infrastructure

- `oxibrain-client` resolved from crates.io (drops an absolute
  path dependency that broke CI and `cargo publish`).
- `libdbus-1-dev` installed on ubuntu build jobs (keyring
  `sync-secret-service` backend links libdbus on Linux).
- Smoke/MSRV/Doc Tests job timeouts bumped 15 → 30 min; macos
  nextest 25 → 40 min. Cold-cache builds of the expanded workspace
  exceeded the prior caps.

## [0.75.0] - 2026-08-13

### Fixed

- **TUI `/model` is now a picker, not a transcript line.** Was: a single
  read-only `Current model: <id>` line. Now: a searchable overlay that
  lists every model from providers with a stored API key, pins the
  active model at the top of the list (even when its key has been
  removed mid-session), and falls back to the full catalog when no
  providers are keyed so a fresh TUI is never empty. Selection switches
  the model end-to-end through the existing overlay-submission handler.
  The `next`/`cycle` arm of `/model` still emits the
  "No scoped models configured to cycle" warning (`ScopedModel` is the
  config-curated cycling set and is out of scope here).
- **TUI `/sessions` is now a real resume, not a no-op.** Was: the
  picker reopened on every selection because the picked id was
  dropped. Now: `/sessions <id>` (and the `/resume <id>` alias)
  opens the file via `SessionManager::open`, validates the cwd,
  and atomically swaps the live `AgentSessionHandle` so the
  conversation continues with the prior history. Selection from
  the `/sessions` picker enqueues the same resume path (was:
  fills `/resume <id>` into the input buffer, which then
  re-dispatched to the same picker). `Cannot resume while agent
  is running. Use /cancel first.` gates the same way `/handoff`
  does. The `next`/`cycle` and `set_model` arms of `/model` and

## [0.74.0] - 2026-08-12

### Added

- **`browse_act`** — natural-language goal → grounded `BrowserTab` action.
  Opens a tab, calls `observe()` to capture the page's interactive surface,
  prunes to a top-20 candidate tier via a deterministic Jaccard scorer, then
  asks a construction-injected LLM to pick the element matching the goal.
  Dispatches `click` / `type` / `fill` / `select_option` / `check` /
  `uncheck` / `press` / `hover`. No CSS selectors required from the caller.
  Closes the layer-2 gap that browser-use / Stagehand / Playwright MCP /
  Skyvern / AgentQL / Claude CU / OpenAI CUA already address. Provider +
  Model are injected at factory construction (`browsing_tools(provider,
  model, engine)`).
- **`browse_act` deterministic-only mode.** When the factory is called with
  `provider = None` (tests, offline builds, MCP servers without an LLM wire),
  the tool still works using its deterministic candidate-tier scorer and
  surfaces `mode: "deterministic_only"` in every result. The factory
  signatures are now `(Option<Arc<dyn Provider>>, Option<Model>, engine)`.
- **Factory signature change.** `browsing_tools` / `browsing_tools_with_config`
  / `browsing_tools_with_session` now take `Option<Arc<dyn oxicode_ai::Provider>>`
  + `Option<oxicode_ai::Model>` so `browse_act` can be wired. CLI bootstrap
  updates the wire in lockstep (anthropic default for the LLM, deterministic
  fallback when no model is configured).
- **`/handoff` slash command for session handoff.** New TUI slash command
  (`oxicode-cli/src/tui_vt/slash/`) that captures the current session's
  task state + remaining-work summary as a structured Markdown handoff
  document (`docs/handoff/<timestamp>-<slug>.md`) and writes a
  companion boot token for the next session. The handoff format is
  designed to survive context compaction: state + invariants +
  next-action checklist, not conversation transcripts.
- **`/handoff` progress spinner + auto-continue guard.** While the
  handoff write is in flight the TUI shows an inline spinner. The
  guard prevents the next prompt from being submitted before the
  write completes (avoids orphan-doc races if the user hits Enter
  again mid-handoff).
- **`/providers` TUI flow is complete.** Adds full provider setup
  inside the TUI: list existing providers → row selection chains into
  the secure-prompt API-key entry (replaceable key-only path) or the
  `run-oauth` flow for OAuth-capable providers. Each row shows the
  live OAuth badge (filled = active token, outline = none) so the
  user can see auth state at a glance. Keeps the newly-opened overlay
  alive after row selection (was being dismissed prematurely).

### Fixed

- **CI: `libfontconfig1-dev` missing on smoke-test and msrv jobs.** v0.72.0
  promoted `native-browser` to a default feature of `oxicode-cli`, which
  transitively pulls `oxibrowser-render` (Blitz/Stylo/Taffy/vello) and the
  `yeslogic-fontconfig-sys` build script. The two clippy jobs install
  `libfontconfig1-dev` (added in v0.73.0) but smoke-test and msrv did not,
  so `pkg-config` could not find fontconfig and `cargo test --no-run`
  (smoke-test) or `cargo build --workspace` (msrv) failed at the
  fontconfig-sys build script. Both jobs now install the system dep.
  CI run 31467470837 was the evidence; red since v0.71.0 → v0.72.0.
- **Doc: two broken intra-doc links in `oxibrowser_backend.rs`.** Qualified
  `BrowseWaitCondition` and `Observation` references as
  `super::engine::BrowseWaitCondition` / `super::engine::Observation` so
  `cargo doc --workspace --no-deps` passes under `RUSTDOCFLAGS="-D warnings"`.
- **CI: `libfontconfig1-dev` missing on `cargo doc` and `Doc Tests` jobs.**
  Same native-browser fallout as the smoke-test/msrv fix above — the
  fontconfig-sys build script also runs during `cargo doc --workspace
  --no-deps` and `cargo test --workspace --doc`. v0.73.0 fixed the
  other two jobs but missed these, so both failed at the fontconfig-sys
  build step on every push to main since v0.72.0. Now installs
  `libfontconfig1-dev` on both jobs (ci.yml + test.yml) and restores
  the missing `actions/checkout` + toolchain + rust-cache steps in
  `test-doc`. CI runs 31588320161 / 31588320102 were the evidence.

### Added

- **P2 `browse_session` runtime-verify smoke tests.** Two new
  integration tests in `browse_session_tool.rs`:
  - `test_full_lifecycle_smoke` drives the full action lifecycle
    (open → goto → click → fill → type → press → check → scroll →
    wait_for → hover → content → query_all → evaluate → screenshot
    → pdf → close → double-close) in one sequence, asserting every
    step returns `success=true`. Catches regressions where one
    action's handler accidentally invalidates session state for the
    next.
  - `test_open_after_close_yields_fresh_session` exercises the
    multi-tab lifecycle (round 1: open → goto → content → close →
    round 2: open → goto → content → close). Verifies that
    `current_tab_id()` clears on close and re-arms on the next
    open. The native backend's unique-UUID-per-tab contract is
    covered by oxibrowser's own tests; the agent-layer test
    asserts the lifecycle, not the UUID uniqueness (the MockTab
    used here returns `Uuid::nil()` for every tab).
### Deferred

## [0.73.0] - 2026-08-11

### Added

- **`browse_session` gains a `pdf` action.** Exports the current page as a
  single-page PDF (viewport width configurable via `width`, default 800) and
  persists the bytes to `/tmp/oxicode-<tab_id>-<ms>.pdf`. The handler returns
  `{size_bytes, width, path, note}` — the agent can re-read the file via
  `read <path>`. PDF bytes are not echoed inline (a single page can be many
  MB; would blow the model context). Available only with `native-browser`.
 **`Tab::print_to_pdf`** is now wired through `BrowserEngine` (default body
  returns `BrowserError::Pdf` for non-`oxibrowser` engines; the real impl in
  `OxicodeBrowserEngine` delegates to `oxibrowser_core::Tab::print_to_pdf`).
 New `BrowseProgress::PdfExported` event carries the export metadata.

### Changed

- **oxibrowser upgraded 0.20 → 0.21.** Pulls in `Tab::print_to_pdf` (a
  Rust-API, not just CDP) and WebAssembly support (wasmi ↔ boa_engine bridge,
  pages with WASM modules now run). WASM is enabled automatically by the
  `oxibrowser-core` dep; the only oxicode-visible change is the new
  `print_to_pdf` surface. Pure additive — no breaking changes.
 `oxibrowser` (search-only) + `oxibrowser-core` (optional, `native-browser`)
  pins both bumped. Lockfile picked up `wasmi`, `wasmi_core`, `wasmi_collections`,
  `wasmi_ir`, `wasmparser`, `weezl`, `xmlparser`, `yansi`, `printpdf`.


## [0.72.0] - 2026-08-11

### Changed — ⚠ Breaking

- **Browsing is now a product capability, not an SDK contract.** `oxicode-sdk`
  no longer re-exports browser tooling: the `BrowseTool`/`BrowserEngine`/
  `BrowseSessionTool`/`OxicodeBrowserEngine` re-exports, the
  `browsing_tools()`/`native_browser_tools()`/`browsing_tools_with_session()`/
  `full_tools()` factories, the `.browsing()`/`.browsing_with_config()`/
  `.native_browser()`/`.browsing_with_session()` `AgentBuilder` methods, and the
  `browser`/`native-browser` **features of `oxicode-sdk`** have all been removed.
  `oxibrowser` is the standalone general-purpose headless browser; products that
  want browsing depend on `oxibrowser` directly (or reuse the agent-layer browse
  tools in `oxicode_agent::tools::browse`). The `Capability::WebBrowse` security
  contract is retained. Blast radius is low (the removed surface lived behind
  `unstable` + `default = []`), but consumers must update.

### Added

- **`native-browser` is now a default feature of `oxicode-cli`.** `cargo install
  oxicode` now ships the built-in headless browser (browse tools) out of the box
  — "browsing-equipped AI agent" identity in every build. Pass
  `--no-default-features --features self-update` for the lightweight build.
- **`read` now fetches `http(s)` URLs as reader-mode markdown.** A lightweight
  static path (no JavaScript, no browser engine) that works in every build — use
  `browse` for dynamic/JS pages, screenshots, or interaction. Role split:
  `read <url>` = fast static body; `browse <url>` = full browser. The system
  prompt now teaches the `web_search → read <url> → browse <url>` research route.
- **Browse tool factories moved to the agent layer.**
  `oxicode_agent::tools::browse::{browsing_tools, browsing_tools_with_session}`
  are the new home for browser-tool assembly.

## [0.71.0] - 2026-08-10

### Added

- **Vision routing is now live in the routing hot path.** The
  `VisionSignal` → `ensure_vision_model` chain (fully built but previously
  orphaned — `stream()` called only plain `route()` and hardcoded
  `is_vision_triggered: false`) is now wired into `RouterProvider::stream()`:
  when recent messages carry image content (e.g. a `browse` screenshot), the
  resolved tier model is swapped to a vision-capable fallback or upgraded tier,
  and the decision records the real `is_vision_triggered` / `vision_images`. New
  `router::vision_routing_tests` cover the swap.

### Changed

- **`oxibrowser` / `oxibrowser-core` 0.16 / 0.17 → 0.20** (`oxicode-agent`,
  `oxicode-sdk`). Resolves the lingering version skew. 0.20 adds a new
  `oxibrowser-render` crate (Blitz / Stylo CSS + Taffy + vello_cpu), so
  screenshots are now real CSS-laid-out PNGs (full-page by default) and pages
  execute `<script>` on navigation (SPA fidelity). API-compatible — zero oxicode
  source changes were needed for the bump. The default build is lighter
  (`oxibrowser` 0.20 `default = []` = search-only, no `boa_engine`); the
  `native-browser` feature now compiles the Blitz stack.

### Fixed

- **Supply-chain config cleaned up for the 0.20 dep tree.** `rav1e` / `ravif` /
  `libfuzzer-sys` are gone — the dead `libfuzzer-sys` NCSA license exception was
  removed from `deny.toml`. The `paste` (RUSTSEC-2024-0436) ignore comment was
  corrected (path is now `boa_engine → boa_string`, not `rav1e`) and
  `rustybuzz` (RUSTSEC-2026-0206, new via the Blitz text-shaping stack) was
  documented in `audit.toml` / `deny.toml`.
- **`oxicode-cli` publish failure fixed.** The OAuth provider-spec loader used
  `include_str!` to reach into a sibling crate's (`oxicode-catalog`) `data/`
  directory, which is absent from `oxicode-cli`'s standalone tarball and broke
  `cargo publish` (cli was stuck at 0.69.0 while the rest shipped 0.70.0). The
  raw `product-meta.toml` is now exposed via
  `oxicode_catalog::product_meta_toml()` and consumed through a direct crate
  dependency instead of the filesystem path.

## [0.70.0] - 2026-08-10

### Added

- **In-TUI OAuth provider setup.** Full OAuth 2.0 PKCE flow: provider
  spec loader from `product-meta.toml`, auth-URL builder + code exchange
  + browser launch, single-shot loopback callback listener, end-to-end
  flow runner with refresh-token coalescing, and a headless manual-URL
  fallback with a 5-minute wait window.
- **TUI secure prompt input overlay.** The overlay now renders a masked
  input box and routes submissions through a dedicated
  `OverlaySubmission::SecureInput` path; `secure_prompt` is threaded
  into `OverlayState`.
- **TUI `/providers` action matrix.** Inline provider management — add
  API key, start OAuth, and remove providers — directly from the
  `/providers` slash command.

### Fixed

- **TUI: restored overlay cleanup in the `Submitted` arm** that was
  dropped when the secure-input path was added.
- **OAuth: missing-state branch returns `BadRequest`, not `MissingCode`.**
  The loopback listener now rejects requests missing the OAuth state
  parameter with the correct HTTP status.

## [0.69.0] - 2026-08-10

### Added

- **Autonomy mode (Auto/Goal) with Shift+Tab toggle.** The TUI now exposes a
  new mode dimension that runs the agent end-to-end without confirming
  intermediate decisions. The default mode is interactive; Shift+Tab toggles
  between interactive and Auto. The mode is first-class on the
  `OxicodeBuilder` (SDK) so products can pin or programmatically switch it
  independent of the host TUI.
- **TUI user-defined slash commands.** Load any `*.md` file under
  `.oxicode/commands/` at startup and the TUI registers its frontmatter
  name as a slash command that prompts the agent with the body. Enables
  teams to ship per-repo workflows without compiling.
- **TUI slash-command expansion.** `/settings`, `/shortcuts`, `/help`,
  `/model`, `/providers`, `/tools`, `/mcp`, `/info`, `/export`,
  `/commands`, `/themes`, `/search`, `/vim`, `/sessions`, `/abort`,
  `/quit` are registered; in-TUI provider/model edits are now wired
  end-to-end.
- **Todo agent tool: `block` / `unblock` ops + auto-promotion.** The
  todo tool now lets the agent mark work blocked (waiting on external
  input) and unblock it; the UI auto-promotes the earliest unblocked
  pending item to `in_progress` when the active task closes. A live TUI
  panel renders the current todo set.
- **Todo agent tool: stop-time incomplete-todo reminder.** When the
  agent loop returns to the user with an idle todo set still carrying
  pending or in-progress items, the runtime injects a single-turn
  reminder so the agent surfaces the unfinished work before actually
  stopping. Bounded by signature dedup and a max-counter.

### Changed

- **Todo agent tool: removed the dead duplicate `TodoStateProvider`
  trait.** The instrumentation-around-state path was never wired; the
  single, shipped provider trait is the source of truth.

### Documentation

- **TUI: fixed 17 rustdoc `broken-intra-doc-links` errors** that the
  `cargo doc` CI gate was rejecting. Affected: `oxicode-agent` (ttsr,
  mcp transport, grep truncate), `oxicode-ai` (bedrock AWS literals),
  `oxicode-mnemopi` (MemoryRow path), `oxicode-sdk` (SupervisorBuilder
  path), `oxicode-cli` (AgentAdvisor, ToolCallEmitResult, TUI module
  links, slash-command private-link). Brings the `cargo doc` job back
  to green.

## [0.68.0] - 2026-08-07

### Added

- **TUI: markdown rendering — lists, tables, theme-aware code blocks.** The
  production renderer (`oxicode-vtui::tui::ui::markdown`) now emits ordered/
  unordered list markers with nesting indentation, renders GFM tables as
  box-bordered grids (natural column widths, CJK-aligned), and highlights fenced
  code blocks with the active syntect theme (`LazyLock`-cached sets,
  `base16-ocean.dark` fallback for unmapped names). Fixes a pre-existing bug
  where `Effects::insert` (takes `self` by value) silently dropped bold/italic/
  strike/underline/dim.
- **TUI: `/settings` and `/shortcuts` slash commands** are now registered (they
  were fully implemented but never wired into the command registry). `/settings`
  toggles thinking/auto-compaction/auto-retry/advisor; `/shortcuts` shows the
  cheatsheet.
- **Theme: `"oxi"` is now the default** — the oxi-design-system brand theme
  (pure-black canvas, warm ink, color-as-data), superseding `oxide-dark`.

### Changed

- **TUI documentation rewritten for the production stack.** AGENTS.md's TUI
  section, the theme pitfall, and the dependency flow now describe
  `oxicode-vtui` + `tui_vt/main_loop.rs` (ratatui main-screen render), not the
  retired tape engine.

### Removed

- **Deleted the dead `oxicode-tui` crate** (~27.6K LOC). It was never in the
  workspace build, absent from `Cargo.lock`, and had zero dependents. All living
  references (CODEOWNERS, README, CONTRIBUTING, labels, issue template) updated.

### Fixed

- **TUI: inline code in markdown table cells** is no longer dropped (the table
  event router now captures `Event::Code`).
- **TUI: CJK-aware column padding** in table rendering (display-width, not char
  count, so wide characters keep borders aligned).

## [0.67.0] - 2026-08-05

### Added

- **TUI: grok-build parity (Phase 1 + Phase 2, 20 features).** Slash-command
  autocomplete popup on `/` and a full main-screen restyle; file search with
  fuzzy matching; `/settings` overlay; OSC 8 hyperlinks; line wrapping with
  reclaimed scrollbar space; structured tool-call rendering with status
  glyphs; distinct visual treatment for thinking blocks; reasoning/tool-stage
  indicator above the composer; queue pane, todo checklist, and follow-up
  suggestion chips; pending-quit hint and compact shortcuts on short
  terminals; overlay picker system with `/model` and `/help`; vim mode for
  prompt editing; theme fold/palette/diff improvements.
- **TUI: `oxide-dark` default theme (GrokNight-inspired)** — calm monochrome
  palette, with startup contrast validation so defined-but-unreadable color
  slots fail loudly instead of shipping.
- **TUI: vtui `WelcomeLayout`** for the wide-terminal empty state.
- **SDK: `AgentBuilder::with_compactor`** — custom context compaction,
  alongside the shipped `SnapcompactCompactor` (PNG-frame, LLM-free).

### Fixed

- **TUI: diff rendering** — `try_render_diff` now gated on `@@` markers, and
  dead `transcript_line` code removed.
- **Docs: broken intra-doc links** — compactor references and private-const
  links that failed the `cargo doc -D warnings` CI gate.

## [0.66.0] - 2026-08-04

### Added

- **Hooks system (Claude Code-compatible).** A new `HookRunner` port (#16)
  lets products run shell commands on agent-lifecycle and tool events
  (PreToolUse, PostToolUse, SessionStart, SessionEnd, SubagentStop,
  Notification). Ships with three implementations: `CommandHookRunner`
  (shell-command reference impl), `InMemoryHookRunner` (tests/headless),
  and `NoopHookRunner` (default). `HookMiddleware` bridges the port into
  the existing `MiddlewarePipeline` so hooks fire alongside audit/authorizer.
  `AgentBuilder::with_port_hooks` / `with_session_hooks` compose hooks into
  a single `set_hooks` call. oxicode-cli adds a `[[hooks]]` schema in
  `settings.toml`, a first-run approval gate for project hooks, and a
  pre-build `SessionState` threaded through teardown/recreate cycles.
- **Agent: shake compaction** — mechanical (LLM-free) context compression.
  Elides large tool-result payloads and fenced code blocks from older
  messages while preserving a recent token tail verbatim, falling through
  to LLM compaction only when savings are below threshold.
- **Agent: TTSR AST condition matching** — rules can now carry an
  `ast_condition` (ast-grep Smart pattern) matched against file content
  after tool writes, in addition to the existing regex `condition`.
- **Agent: typed `StreamDelta`** — `MessageUpdate` streaming events now
  surface a typed delta instead of a raw string.
- **LSP: reload/capabilities/request actions** — the LSP bridge gains
  `reload`, `capabilities`, and `request` actions with capabilities
  caching.

### Fixed

- **Agent: `afterToolCall` error handling** — errors from the after-tool
  hook are now propagated instead of swallowed, and spawned tool tasks
  are guarded against panics.
- **CLI: hooks middleware replace-semantics bug** — installing runtime
  hooks no longer wipes the existing middleware pipeline; the
  replace-vs-append semantics are now correct.

## [0.65.0] - 2026-08-03

### Breaking

- **Project renamed: oxi → oxicode.** All crate names (`oxi-*` → `oxicode-*`),
  Rust identifiers (`oxi_*` → `oxicode_*`), environment variables (`OXI_*` →
  `OXICODE_*`), config paths (`~/.oxi/` → `~/.oxicode/`, `.oxi/` → `.oxicode/`),
  binary name (`oxi` → `oxicode`), type names (`Oxi` → `Oxicode`, `OxiBuilder` →
  `OxicodeBuilder`), and documentation have been migrated. Sister projects
  (oxios, oxipage, oxiline, oxinot) and external dependencies (oxibrowser) are
  unchanged. Users must update their `use` statements, env vars, config paths,
  and binary invocations.

## [0.64.0] - 2026-08-02

### Breaking

- **oxicode-sdk: unstable re-export surface moved behind opt-in cargo features.**
  Every `#[oxicode_unstable]` re-export now carries a matching
  `#[cfg(feature = "...")]` gate (completes R3 follow-up D). The
  previously always-compiled unstable surface — `router`,
  `role-routing`, `role-switching`, `advisor`, `memory`, `subagent`,
  `agent-hub`, `lsp`, `browser`, `delegation`, `url-resolver`,
  `workflow-dsl` — is now opt-in. Consumers that name these via the
  `oxicode_sdk::` path must enable the matching feature (or the `unstable`
  umbrella); enable all twelve at once with
  `oxicode-sdk = { features = ["unstable"] }`. Internal callers reach the
  underlying types via their definition paths
  (`crate::url_resolver::*`, `crate::delegation::*`), so builder
  methods stay available in default builds. oxicode-cli enables the four
  features it consumes (`router`, `role-routing`, `role-switching`,
  `url-resolver`). See `docs/release-process.md` §Stability Tier.
- **oxicode-ai: `OAuthError` is now `#[non_exhaustive]`.** Completes the R7
  error-stability policy for the OAuth subsystem. Consumers MUST add a
  catch-all `_ =>` arm when matching `OAuthError`; a new variant
  `InvalidAuthorizationEndpoint` was added in the same change.
- **Retrospective — symbols removed in 0.61.0 / 0.63.0 without adequate
  warning.** Documented retroactively per the Breaking Change Policy
  (see `docs/release-process.md`). All are now classified as
  `#[unstable]`/consumer-owned, and the SDK provides behavior traits
  (`CircuitBreaker`, `SpawnValidator`) plus reference impls that
  consumers can wire without depending on the removed types:
  - `oxicode_ai::ProviderPool`, `oxicode_ai::RateLimitPolicy` — removed in 0.61.0
    (`provider_pool` module, 203 LOC). No direct replacement; superseded by
    the `RouterPipeline` in `oxicode_ai::router`.
  - `oxicode_ai::CircuitBreakerConfig`, `oxicode_ai::ProviderCircuitBreaker` —
    removed in 0.61.0 (`circuit_breaker` module, 944 LOC). A minimal
    `CircuitBreaker` behavior trait + `DefaultCircuitBreaker` reference
    impl are reintroduced in this release (R6).
  - `oxicode_ai::MultiProviderBuilder`, `oxicode_ai::RoutingConfig`,
    `oxicode_ai::MultiProviderConfig` — removed in 0.61.0 (`multi_provider`
    module, 1283 + 359 LOC). Superseded by `RouterPipeline` + the
    `router://local` provider.

### Deprecated

- **oxicode-ai: `oauth::build_authorization_url` is deprecated** (since
  0.64.0; removal slated for 0.66.0). It panics on a malformed
  authorization endpoint. Use the non-panicking
  `build_authorization_url_result` instead, which returns
  `Result<PkceState, OAuthError>`. The legacy function delegates to the
  new one and preserves its historical panic behavior.

### Added

- **SDK: behavior↔policy ownership contract** (`docs/oxicode-sdk-ownership.md`)
  — R0/R5 of the SDK Stability & Ownership Program. Pinned
  behavior/policy split between SDK and consumers (oxios, oxicode-cli, etc.).
- **SDK: `oxicode-api-stability` proc-macro crate** — `#[stable]` /
  `#[unstable]` / `#[internal]` / `#[deprecated]` attribute macros
  for stability tier documentation in `cargo doc` (R3, scope-narrowed
  from original spec — see `docs/oxicode-sdk-ownership.md` §6.1).
- **oxicode-ai: `CircuitBreaker` trait + `DefaultCircuitBreaker`** (R6) —
  composable behavior trait for circuit-breaking resilience with
  threshold + half-open state machine. `BreakerError::Open` is
  non-retryable by design (the breaker's whole purpose is to stop
  hammering a failing upstream).
- **oxicode-agent: `SpawnValidator` trait + `NoopSpawnValidator`** (R6) —
  composable policy hook for MCP server spawn validation. Consumers
  register their own `validate_command` / `sanitize_env` policy;
  oxicode-cli's default wires a permissive policy, oxios can wire its
  strict impl. The SDK keeps a hardcoded `BLOCKED_ENV_VARS` floor
  (loader-injection vectors) that always applies.
- **oxicode-sdk / oxicode-ai: `#[non_exhaustive]` on `SdkError` and
  `ProviderError`** (R7) — error type stability. Existing named
  variants are frozen; new variants may be added freely. Consumers
  MUST add a catch-all `_ =>` arm in their matches.
- **oxicode-ai: `oauth::build_authorization_url_result`** — non-panicking
  PKCE URL builder returning `Result<PkceState, OAuthError>`, plus a new
  `OAuthError::InvalidAuthorizationEndpoint` variant for malformed
  endpoint URLs (R4 follow-up C).
- **oxicode-agent: CircuitBreaker wired into retry path** — `AgentLoopConfig`
  now accepts an optional `SharedBreaker`; the retry loop short-circuits
  when the breaker is open (R6).
- **oxicode-sdk: stability tier annotations on all 17 re-export blocks**
  (R3) — every public re-export now carries `#[stable]` / `#[unstable]` /
  `#[internal]`.
- **oxicode-cli (TUI): `/agents` Agent Hub overlay** — `/agents` (alias
  `/hub`) opens a centered panel listing every registered agent
  (kind/name/status) from the live `AgentSession` hub registry; `q`
  closes it. Completes the VT-TUI cutover (item A).
- **oxicode-cli (TUI): synchronized-output tape rendering** — each frame
  flush is wrapped in Begin/End Synchronized Update
  (`\x1b[?2026h` / `\x1b[?2026l`), eliminating mid-frame tearing on the
  main-screen tape.
- **ci: cargo-public-api diff gate** — `api-diff.yml` workflow captures
  public API snapshots and enforces no undocumented API removals on PRs
  (R1).

### Changed

- **oxicode-ai: feature-gated protobuf providers** — Devin + Cursor
  providers moved behind a new `protobuf` cargo feature (R8). Default
  builds no longer pull `prost` / `prost-build` / `protoc-bin-vendored`
  (~120 transitive crates). Consumers using those providers enable
  with `--features protobuf`.
- **oxicode-cli (TUI): ordinary chat renders on the main screen** — the TUI
  no longer enters alternate screen (`\x1b[?1049h`) for ordinary chat;
  alt screen is reserved for transient overlays, matching the main-screen
  tape design. Ctrl+C now uses a two-press quit (first press arms, second
  exits) so a single accidental press never kills the session.

### Fixed

- **oxicode-sdk: structured error promotion + embedding port bridge + smoke
  tests** — promoted string errors to typed `SdkError` variants, added
  real example code, and wired `MnemopiEmbeddingBridge` to bridge
  oxicode-mnemopi to the SDK async `EmbeddingProvider` port.
- **oxicode-ai / oxicode-agent / oxicode-sdk: R4 zero-panic enforcement** — audited
  and replaced `unwrap()` / `expect()` calls that could panic on
  external input with proper error propagation.
- **docs: broken intra-doc links** — resolved all `cargo doc -D warnings`
  failures across oxicode-ai, oxicode-agent, oxicode-sdk, and oxicode-cli.

## [0.63.0] - 2026-08-01

### Added

- **VT-based TUI framework** — New `oxicode-vtui` and `oxicode-vtui-compat` crates
  providing a terminal UI framework based on vtcode-ui, with theme pipeline,
  design system, and full TUI widget support.
- **grok-build layout system** — Ported the agent view layout system from
  grok-build as the structural foundation for the new TUI.
- **Slash command registry** — VT TUI slash command system with layout bridge
  for CLI integration.
- **UI protocol compatibility layer** — `oxicode-vtui-compat` bridges the VT UI
  protocol with oxicode's agent/UI types.

### Changed

- **CLI: VT TUI cutover** — Replaced the legacy ratatui-based TUI with the
  new VT-based TUI framework across all CLI modules.

### Fixed

- **TUI: black screen on launch (oxicode-vtui)** — The event loop drew its first
  frame only *after* the first event arrived, and the keyboard input thread
  exited within ~50 ms of startup: it used `while let Ok(true) =
  event::poll(...)`, which treats the `Ok(false)` poll timeout as loop
  termination, dropping its event sender. With no input thread and no
  initial draw, the TUI rendered nothing until Ctrl+C, then flashed a single
  frame and re-blocked. Fixed by (1) keeping the input thread alive across
  poll timeouts (exit only on a read error), (2) drawing an initial frame
  before the loop blocks, and (3) adding a 50 ms render tick so typed input
  is echoed without each keystroke needing to send an event.
- **TUI: invisible text / wrong colors (oxicode-vtui)** — Text rendered
  near-black on a dark background and was invisible until the user
  drag-selected it (which inverts colors). Root cause: the
  `oxicode-vtui-compat` color-constant block was a garbled copy of the
  authoritative values — the RGB blend clamp was `[0,1]` instead of
  `[0,255]` (collapsing almost every blend to `(1,1,1)`), "white" was set
  to the luminance coefficients `0.299/0.587/0.114` instead of `1.0`, and
  the relative-luminance constants used NTSC luma rather than sRGB/WCAG.
  The theme pipeline ran every palette through this broken math, so every
  computed color degenerated to near-black. Fixed by correcting the
  constants to the WCAG/sRGB values already used by `oxicode-vtui`'s own
  `ui.rs`. Also paint the theme background across the whole frame so
  fg-only text always sits on the intended surface.

## [0.62.0] - 2026-07-30

### Added

- **CLI: persona provider integration** — `PersonaProvider` port wired into
  `App` and session prompt construction, enabling persona-based system prompt
  fragments.
- **SDK: `PortMemoryBackend` bridge** — New `port_memory_backend` module
  bridges the SDK's `MemoryStore` + `EmbeddingProvider` ports into the
  `memory_*` agent tools. SDK consumers can now make memory functional
  end-to-end via `AgentBuilder::with_port_memory()`.
- **Packages: `RuntimeConfig`, `ProjectPluginOverrides`, `Doctor`** — Built-in
  package manager with runtime configuration, project-scoped plugin overrides,
  and a `Doctor` diagnostic command for validating the extension setup.
- **Providers: explicit Codex Responses + Gemini CLI dispatch** — Added
  dedicated transport dispatching for `openai-codex` and `google-gemini-cli`
  dialects, enabling proper provider routing for these model families.

### Fixed

- **Memory: tokio `blocking_lock` safety** — Wrapped `blocking_lock` calls in
  `tokio::task::block_in_place` to prevent async-runtime hangs when the
  memory store uses a synchronous lock.

### Changed

- **TUI: stale docs cleanup** — Removed outdated `tape/engine` documentation
  and dead language-policy UI code from the TUI crate.

## [0.61.0] - 2026-07-29

### Changed

- **TUI: production tape cutover** — Ordinary chat rendering now uses the
  main-screen `TapeEngine` with memoized transcript components and pinned
  sticky rows. Alternate screen is entered only for transient overlays.
  The PTY acceptance test asserts no ordinary `1049h` entry and cursor
  restoration on exit.

- **TUI: v2 crate retirement + rename (P2.1)** — Retired the grok-inspired
  `oxicode-tui` v2 crate. Ported cursor dedup (`CursorState`) from v2 to legacy
  `DiffBackend` (which already had CSI 2026 sync, DECCARA, row-level diffing).
  Deleted the v2 crate (~9.8K LOC). Renamed `oxicode-tui-legacy` → `oxicode-tui` as

### Removed — oxicode-ai (Breaking Changes — retrospective, 0.61.0)

- **ai: P0.2 multi-provider/circuit-breaker modules** (retrospective entry
  per the Breaking Change Policy — these were removed in 0.61.0 without
  adequate advance warning; documenting here so SDK consumers can audit
  impact):
  - `oxicode_ai::ProviderPool`, `oxicode_ai::RateLimitPolicy` — removed
    (`provider_pool` module, 203 LOC). No direct replacement; the router
    pipeline (`RouterPipeline`) supersedes multi-provider routing.
  - `oxicode_ai::CircuitBreakerConfig`, `oxicode_ai::ProviderCircuitBreaker` —
    removed (`circuit_breaker` module, 944 LOC). A minimal
    `CircuitBreaker` trait will be re-introduced (see R6).
  - `oxicode_ai::MultiProviderBuilder`, `oxicode_ai::RoutingConfig`,
    `oxicode_ai::MultiProviderConfig` — removed (`multi_provider` module,
    1283+359 LOC). Superseded by `RouterPipeline` + `router://local`
    provider.

### Added

- **TUI: omp tape engine (P2.2)** — Standalone append-only native-scrollback
  rendering engine in `oxicode-tui/src/tape/`. Component trait with content_hash
  memoization, TapeEngine with committed prefix / live region / differential
  rendering (scroll-append fast path, chunk commit, in-window diff), ED3
  scrollback clear for resize/session-replace, CSI 2026 synchronized output.
  21 tests. Wired into the default render path in the production cutover.
- **TUI: tape component implementations (P2.3)** — TextMessage (finalized
  message), StreamingMessage (active streaming with finalized/live boundary),
  ToolCallBlock (running/completed states). 22 tests.
- **TUI: input system (P2.4)** — Kitty keyboard protocol parser
  (`parse_kitty_key`), bracketed paste handler (`PasteBuffer`/`PasteEvent`),
  kill ring (`KillRing` with yank/yank-pop). 37 tests.
- **TUI: LaTeX-to-Unicode converter (P2.5)** — `latex_to_unicode()` with 145
  symbol mappings (Greek letters, math operators, arrows, sets, subscripts/
  superscripts, accents, `\frac`, `\sqrt`). 18 tests.
- **TUI: theme/glyph verification (P2.6)** — Confirmed single-source theme
  (`oxicode-tui/src/theme.rs`) and glyph (`oxicode-tui/src/symbols.rs`) systems.
  Updated all remaining `oxicode-tui-legacy` comment references to `oxicode-tui`.

### Added (P2 integration)

- **TUI: PTY render verification test** — Guards the production tape path:
  spawns the actual binary in a PTY, verifies synchronized main-screen tape
  output without alternate-screen entry, sends Ctrl+C, confirms cursor
  restoration and clean exit. Permanent guardrail.
- **TUI: KillRing in input editor (OXICODE_KILL_RING=1)** — Emacs-style kill ring.
  Ctrl+Shift+k: kill to line end → push to ring. Ctrl+Shift+u: kill to
  line start. Ctrl+y: yank. Alt+y: yank-pop. Gated by env var; default
  behavior (Ctrl+Shift+k = plain delete) preserved when unset.
- **TUI: LaTeX-to-Unicode in markdown (OXICODE_LATEX_INLINE=1)** — Preprocesses
  markdown segments through `latex_to_unicode()` (145 symbol mappings)
  before parsing. Code blocks unaffected. Gated by env var.
- **TUI: Kitty keyboard protocol (OXICODE_KITTY_KEYBOARD=1)** — When set, terminal
  setup pushes full `KeyboardEnhancementFlags` (`DISAMBIGUATE_ESCAPE_CODES |
  REPORT_EVENT_TYPES | REPORT_ALTERNATE_KEYS`). crossterm 0.29+ parses
  CSI-u sequences natively; event loop works unchanged.
### Added (omp-adoption-2)

- **TUI: Agent Hub overlay (Ctrl+h / /agents)** — Fullscreen alt-screen
  overlay listing the main agent, advisor reviewer, and persisted subagents.
  Table view with j/k navigation, Enter for live transcript viewer (mtime-
  polled at 250ms), and severity-colored advisor advice cards persistent
  in the scrollback. HubRegistry wired into AgentSession. 4 new overlay
  modules.
- **tools: todo tool + sticky panel** — Phased todo system with 7 ops
  (init/start/done/drop/rm/append/view), Markdown round-trip, sub-agent
  matching, and TodoPanelState synced into AppState every frame. 950 LOC.
- **tools: commit tool** — Hybrid LLM+deterministic commit generator.
  Scope extraction (lockfile exclusion, churn ranking, wide-change
  detection), Kahn topological ordering, conventional-commit formatting,
  LLM analysis with deterministic fallback. 1,798 LOC, 44 tests.
- **tools: hindsight memory (retain/recall/reflect/edit)** — 4 AgentTool
  implementations backed by MemoryBackend trait. Boot-time recall
  injection (`build_memory_recall` + `read_path_block`) and background
  memory pipeline (per-session extraction + cross-session consolidation
  every 60s). 26 tests.
- **ai: Snapcompact compaction mode** — `SnapcompactCompactor` implementing
  the Compactor trait, producing PNG frame renderings via the `oxicode-snapcompact`
  crate (bundled fonts, text rasterizer). `CompactionStrategy::Snapcompact`
  variant in the compaction engine. 5+15 compactor/renderer tests.
- **mnemopi: SQLite memory engine** — `oxicode-mnemopi` crate with FTS5 full-text
  search, vector recall (cosine sim + MMR), polyphonic recall, temporal
  decay, episodic graph, veracity consolidation, and MCP server. 40+ source
  files.
- **lsp: oxicode-lsp crate + LSP tool** — `oxicode-lsp` thin LSP adapter
  (async_lsp + lsp_types), `LspTool` agent tool with 11 operations
  (diagnostics/definition/references/hover/rename/symbols/status/
  code_actions/type_definition/implementation/file_rename), `CliLspProvider`
  multi-server lifecycle manager. Conditionally wired when rust-analyzer
  is on PATH.
- **tui: Mermaid diagram renderer** — Pure-Rust ASCII renderer for 4 diagram
  types (flowchart/sequence/state/class), wired into markdown fenced code
  blocks, process-level cache. 2,608 LOC, 25+ tests.

## [0.60.0] - 2026-07-27 — TUI bug fixes

### Fixed

- **`/skill off` completion** — `complete_arg` computed `looking_for_skill`
  with `!rest.contains(' ')`, but `strip_prefix("off")` leaves a leading
  space (`" off open"` → `" off"`), so the check flipped false and dropped
  every skill for `/skill off <partial>`. Extracted `skill_complete_prefix`
  as a pure helper and fixed the space handling. 4 tests added.
- **Extension commands in `/help` and completion** — `state.slash_registry`
  was built as bare `builtins()` at `AppState::new` and never synced with
  `wasm_ext`. Dispatch uses a per-call local registry that syncs extensions,
  so extension commands executed but stayed invisible in `/help` and
  command-name completion. Now synced at both mutation sites: initial wiring
  (`app.rs`) and runtime reload via `/settings` and `/reload` (`settings.rs`).
- **MCP manager on session agent registry** — `register_arc` copies only
  `Arc<dyn AgentTool>`, not the `mcp_manager` field. The TUI session builder
  created a fresh `ToolRegistry`, copied tools, but never called
  `set_mcp_manager` — so `session.agent_ref().tools().mcp_manager()` returned
  `None` and `/mcp dashboard|status|reauth` warned "MCP runtime manager
  unavailable" despite `McpTool` being registered. Now mirrors
  `bootstrap.rs:425`. Contract test added.
- **`/provider` overlay dropped all providers** — `build_visible_items`
  iterated 6 hardcoded categories and silently dropped every provider whose
  category didn't match. The catalog (sourced from models.dev) sets
  `category=""` for all providers — models.dev JSON carries no category field
  — so all providers were dropped while the count hint still rendered. Added
  a fallback bucket that renders empty/unrecognized-category providers
  alphabetically; the "Other" header is suppressed when it is the only bucket.
  3 regression tests added.

## [0.59.0] - 2026-07-27 — RPC auto-retry, provider robustness, MCP spawn consent

### Added — RPC auto-retry + agent loop enhancements

- **RPC auto-retry** — two new JSON-RPC commands, `set_auto_retry` and
  `abort_retry` (`rpc_mode/handlers.rs`), let IDE clients drive automatic
  retry of a failing agent turn. Session handoff + export improvements
  thread retry state through `agent_session.rs`.
- **Agent loop / retry** — `agent_loop/mod.rs` (+103) and `retry.rs` (+25)
  harden the retry loop; `agent.rs` (+72) threads the new state through the
  runtime.
- **`send_raw()` test helper** (`rpc_mode/utils.rs`) — emits arbitrary
  commands for RPC testing; `rpc_mode.rs` test for "unsupported command"
  updated to a genuinely-bogus command now that `set_auto_retry` is real.
- RPC parse-error responses now carry the caller's request ID (extracted
  before `serde_json::from_value` consumes the JSON), so `RpcClient`'s
  response-matcher finds them instead of timing out at 60s.

### Security — MCP spawn consent + credential masking

- **F-2: MCP spawn consent gate** — MCP server spawn is now gated behind
  explicit consent (`ConsentState::Ask` sits in `connect()` before the
  transport match, covering both stdio and HTTP). Unknown servers default
  to `Ask` at the spawn gate, closing the clone-to-RCE surface; the
  per-tool gate keeps its `Allow` default so `direct_tool.rs` is
  unaffected. A one-time migration auto-trusts **only** global config
  servers (`~/.config/oxicode/mcp.json`) — project-local servers from a
  cloned repo stay gated. Typed `McpError::ConsentDenied` replaces
  anyhow at the new boundary. 5 new consent tests.
- **F-1: `AuthCredential` Debug masking** — replaced
  `#[derive(Debug)]` with a manual impl that masks
  `key`/`access_token`/`refresh_token`/`token` as `<redacted>`, fixing
  the info-disclosure vector where `dbg!`/`tracing::debug!`/panic
  backtraces echoed raw API keys. 3 new masking tests.

### Fixed — provider robustness

- **F-3: Header `.expect()` → `?`** — replaced 14
  `.expect("valid header value")` panics with
  `.map_err(ProviderError::InvalidResponse)?` across all 6 providers
  (anthropic, azure, bedrock, mistral, openai, openai_responses). A
  malformed API key (newline, non-ASCII, corrupt config) no longer kills
  the process via `panic=abort`. Azure's `build_headers()` now returns
  `Result<HeaderMap, ProviderError>`.
- **F-10: Azure/Mistral SSE chunk-boundary fix** — Azure and Mistral SSE
  streams now use `split_complete_lines` + `pending_bytes` (the same
  pattern as openai/anthropic/google/vertex). SSE `data:` lines split
  across HTTP chunk boundaries are no longer silently dropped.
- **F-14: Doc drift** — corrected AGENTS.md oxicode-cli LOC (17K → 66K),
  `ProtocolHandler` port status (🔜 TBD → ✅ wired), and README provider
  count (10 → 8).

## [0.58.0] - 2026-07-23 — oxicode-tui v2 Terminal-First Pipeline

### Added — oxicode-agent streaming lifecycle events

- **`AgentEvent::ToolCallDelta`** (P0) — the provider's streamed tool-arg
  fragments are now forwarded as `AgentEvent::ToolCallDelta { tool_call_id,
  args_delta }`, so downstream consumers can render tool-argument
  construction token-by-token (LobeHub chat-UX parity). The `tool_call_id` is
  resolved from a `content_index → id` map populated at `ToolCallStart` and
  reconciled at `ToolCallEnd`, because providers (Anthropic, OpenAI, Azure,
  Mistral) keep the id in a private pending map and never embed a
  `ContentBlock::ToolCall` in the streaming partial until the call finalizes
  — the doc's proposed `extract_tool_call_id` partial-content lookup was dead
  for all providers. Serialized camelCase: `{type:"toolCallDelta",
  toolCallId, argsDelta}`.
- **`AgentEvent::ThinkingEnd`** (P1) — signal-only event marking the end of a
  reasoning span, forwarded from `ProviderEvent::ThinkingEnd` (previously
  dropped by the catch-all). Fires per thinking span, giving the exact
  reasoning↔text boundary for interleaved-reasoning models (Claude 4,
  o-series).
- **Scope/caveat** — `oxicode-agent` only (`events.rs`, `agent_loop/streaming.rs`
  + unit tests). Oxios consumes `AgentEvent` directly via `oxicode-sdk`
  (re-exported, `lib.rs:192`), so the LobeHub port's already-built
  `AgentEvent → KernelEvent` handler now receives both events with no
  further oxicode change. The JSON-RPC IDE bridge (`agent_event_to_rpc`) now
  also forwards both variants — `RpcEvent::ThinkingEnd` and
  `RpcEvent::ToolCallDelta { tool_call_id, args_delta }` — across the SEND
  path (agent → JSON for IDE clients) and the RECEIVE path (both `RpcClient`
  JSON → RpcEvent parsing blocks). Wire shapes (RpcEvent is snake_case):
  `{"type":"thinking_end"}` and
  `{"type":"tool_call_delta","tool_call_id":…,"args_delta":…}`.

### Architecture — oxicode-tui Greenfield Rewrite

- **Terminal-first rendering pipeline** — `draw_frame()` decomposes ratatui's
  `Terminal::draw()` into `autoresize → hash-skip → render → flush →
  conditional cursor → swap_buffers`. Cursor blink preserved via dedup
  (same position → 0 bytes). No fork, no writer thread, no SafeBuf.
  Ratatui 0.30 lifecycle methods are all `pub` — no Terminal fork needed.

- **Retained widget tree + content_hash memoization** — `Renderable` trait
  with `content_hash()` / `height_for()` / `render()`. `RetainedChild<T>`
  wrapper auto-skips unchanged subtrees. During streaming, only the active
  message re-renders; siblings (Footer, panels) skip entirely.

- **CursorSlot tri-state** — `{NotSet, Show(Position), Hide}` distinguishes
  "hash-skipped widget" from "explicit hide". Prevents cursor flicker
  regression that simple `.or(last_cursor)` fallback would cause.

- **Streaming markdown checkpoint renderer** — stable content (behind
  paragraph break or closed code block) is parsed once and frozen; only the
  unstable tail is re-parsed per token. Reduces CPU from O(N) to O(tail).

- **Capability-aware theme** — `theme/capability.rs` detects terminal caps
  AND adapts theme colors in the same module. Structural fix for legacy's
  394-LOC dead `color_level.rs` (detection without consumption).

- **OSC8 hyperlink emission** — `DiffBackend::set_links()` stores link spans;
  row writes emit `\x1b]8;;<url>\x1b\\` inline, inside the CSI 2026 sync
  window. Caps-gated (zero bytes if terminal doesn't support hyperlinks).

- **CJK-aware word wrap** — `unicode-width` based, handles double-width
  characters and soft/hard break tracking.

### oxicode-cli Integration

- **Pipeline LIVE** — main loop uses `draw_frame_closure()` instead of
  `terminal.draw(closure)`. Cursor dedup + CSI 2026 sync + DECCARA bg-fill
  active for the real TUI.

- **Dual-write ChatLog** — agent events (MessageStart/Update/End) feed both
  legacy `ChatViewState` AND new `ChatLog`. State preparation for rendering
  migration.

- **v2 render path** — `OXICODE_V2_RENDER=1` env var gates new ChatView widget
  rendering. When enabled, chat area renders via `ChatView::render()` (new
  Renderable API). Footer/input/overlays still legacy (incremental
  migration).

- **Bridge layer** — `ClosureRoot` adapter wraps legacy render closures as
  `Renderable`. `LegacyOverlayAdapter` wraps old overlays. `RenderCtx::
  with_frame()` provides temporary Frame access for legacy bridging.

### Module Structure (oxicode-tui v2, 42 files, ~9.7K LOC)

```
pipeline/   draw_frame, draw_frame_closure, CursorState, CursorSlot,
            DiffBackend (4-file: mod/row/deccara/caps), OSC8 emission
widget/     Renderable, RetainedTree, RetainedChild, RenderCtx, Text
  chat/     ChatView, MessageItem, ToolCall, Spinner
  panel/    Footer, Sticky, Overlay
  primitive/ Border, List (virtualized), Scrollbar
content/    ChatLog (O(1) hash), ChatViewState, ChatMessage, StreamingState
text/       StreamingMarkdown (checkpoint), CJK wrap, syntax (feature-gated)
theme/      palette (28 slots, 6 constructors), capability (detect+adapt),
            serializer (TOML load/save with atomic write)
input/      textarea wrapper (stock ratatui-textarea 0.9)
```

### Legacy Preservation

- `oxicode-tui` renamed to `oxicode-tui-legacy` — all 27 oxicode-cli callsites migrated
  (`oxicode_tui` → `oxicode_tui_legacy`). Legacy crate continues to work unchanged.
  Removal planned after full rendering migration (Plan D).
## [0.56.0] - 2026-07-19

### Added

- **Snapcompact renderer** — `oxicode-snapcompact` crate with full fontdue-based
  text→PNG rasterizer ported from pi-natives. Five bundled fonts (BDF + TTF),
  real PNG output via the `png` crate. `SnapcompactCompactor` in `oxicode-sdk`
  implements `oxicode_ai::Compactor` for vision-capable models.
- **`CompactionStrategy::Snapcompact`** variant — always compacts using the
  snapcompact PNG renderer. Wire via `CompactionManager::set_compactor()`.
- **RPC mode rewrite** — `RpcActor` replaces the legacy stub with a full
  AgentSession-backed actor: 15 real commands (prompt, steer, follow_up,
  abort, get_state, set_model, compact, bash, etc.), JSONL framing,
  TurnOutcome lifecycle, and a programmatic `RpcClient`.
- **Internal URL bridge** — 7 scheme handlers (issue, pr, memory, skill,
  rule, agent, local) wired through `SdkUrlResolver` → `AgentConfig` →
  `ToolContext`. `read`/`grep`/`find` tools resolve internal URLs
  transparently.
- **oxicode-lsp crate** — thin LSP protocol adapter (`async-lsp` + `lsp-types`).
  `CliLspProvider` in `oxicode-cli` bridges to multi-server `LspManager` with
  11 action handlers (definition, references, hover, rename, code_actions,
  file_rename, etc.). Edit/write tools auto-notify LSP after mutations.
- **WorkflowEngine** — `oxicode-sdk` engine that executes `WorkflowDefinition`
  with 6 step types (Run, Parallel, Chain, ForEach, Vote, SetState).
  `{previous}` substitution and `SharedMemory` integration.
- **SubagentCoordinator** — lifecycle tracking for subagents
  (Pending→Claimed→InProgress→Completed), `CancellationToken` propagation,
  depth guarding, `resume_from` preamble inheritance.
- **Observability decorator** — `ObservabilityDecorator` bundles audit,
  authorizer, tracer, and cost tracker. Replaces the old `SupervisorBuilder`
  no-op setters. `AgentBuilder::tracer()` now fully functional: `SpanGuard`
  owns `Arc<Tracer>`, spans recorded for Run/Turn/Tool lifecycle events.
- **oxicode-tui UX improvements** — `FollowMode` state machine, virtual
  coordinate scroll layer, reflow-safe clamp, sticky turn-prompt header,
  mouse scroll normalization with wheel/trackpad detection, color level
  detection + `adapt_color`, terminal support pattern for EPT overrides,
  A4 layout cache.
- **oxicode-tui slash dropdown widget** — nucleo-based fuzzy command matching
  with MRU decay (7-day half-life) and inline ghost completion.
- **Memory pipeline scaffolding** — real Stage 1/Stage 2 worker loops
  with LLM-backed extraction and consolidation via `Oxicode` resolver.
  `/memory status`, `sleep`, `harmonize` commands operational.

### Changed

- **`GroupStrategy::Orchestrated` removed** — replaced by
  `WorkflowEngine` + `SubagentCoordinator` for multi-agent coordination.
- **`stream_responses` setting removed** — always streaming, matching OMP
  behavior. Existing `settings.toml` entries are silently ignored.
- **`SupervisorBuilder` no-op setters removed** — `with_audit`,
  `with_authorizer`, `with_tracer`, `with_cost_tracker` replaced by
  `with_agent_decorator(Arc<dyn AgentDecorator>)`.
- **`tool_call_loop_guard` now live** — detects repetitive tool loops and
  injects a steering message with the `TERMINAL_TOOL_RESULT_ABORT_REASON`
  pattern (inner loop abort only, not full run termination).
- **`RoutingControl` live in `Oxicode::resolve_model`** — exclusion checks
  and fallback models are read from the shared `Arc<RoutingControl>`.

### Fixed

- **`syntect` feature fixed** — `default-fallbacks` → `default-fancy`
  for syntect 5.3 compatibility.
- **`oxicode-tui` module compile fix** — restored `tool_renderer` module
  declaration.
- **Workspace version uniformity** — all 9 crates at `0.56.0`.

## [0.55.0] - 2026-07-18

### Changed

- **BREAKING — Single credential authority for the agent loop (#40)**.
  The vestigial `AgentConfig::api_key` field and the `api_key` parameters on
  `Agent::switch_model` / `Agent::switch_to_model` / `Agent::refresh_api_key`
  have been removed. The provider instance is now the sole credential holder,
  populated exclusively through `OxicodeBuilder::api_key(...)` (explicit override)
  or the wired `AuthProvider` port via its new sync fast-path
  (`get_api_key_sync`). This eliminates the leak where a stray per-stream
  `api_key` could override the resolver-baked credential (`Provider stream
  error: Failed to resolve model` / cross-provider 401s).
  `Agent::refresh_credentials()` is the new entry point for picking up
  auth-store updates — it re-resolves the current provider via the resolver.
  CLI migration: `services.rs::build_oxicode` now registers the
  `shared_auth_storage()` singleton directly via a new
  `impl oxicode_sdk::ports::AuthProvider for AuthStorage` adapter, eliminating
  the dual-cache (FileAuthProvider vs AuthStorage) and schema-mismatch
  issues that previously hid behind the explicit per-stream `api_key`.

- **`oxicode-sdk` — `Oxicode as ProviderResolver` (#39)**. `AgentBuilder::build()`
  now uses `self.oxicode.clone()` directly as the agent loop's resolver
  (replacing the hand-rolled `OxicodeResolver` closure). `Oxicode::resolve_model`
  consults the catalog port first and falls back to the static registry,
  so catalog-only models (newer Z.AI / OpenCode / models.dev entries) work
  end-to-end instead of returning `Failed to resolve model` at stream time.

### Added

- **`oxicode-sdk` — `AuthProvider::get_api_key_sync` optional fast-path**.
  Default returns `Ok(None)`; impls with synchronous backing stores override
  it. `FileAuthProvider` re-reads `path` from disk on every call (so external
  writers are picked up without restart); `AuthStorage` (oxicode-cli) delegates
  to its existing sync `get_api_key`. Consumed by `Oxicode::create_provider`
  at build / `switch_model` / `refresh_credentials` time.

### Fixed

- **Workspace version skew.** All six crates now share `0.55.0`. The
  pre-existing broken `oxibrowser` 0.17 bump in `oxicode-agent/Cargo.toml`
  (added a `browser` feature that 0.17 doesn't expose) was reverted to
  `0.16` so the workspace resolves.

## [0.54.0] - 2026-07-05

### Added

- **`oxicode-agent` — `AgentConfig` passthrough for `AgentLoopConfig` fields (#32, #33)**.
  `max_tool_result_bytes`, `subagent_runner`, and `subagent_depth` are now
  exposed on `AgentConfig` as `#[serde(skip, default)]` passthroughs, mirroring
  the existing `memory`/`todo`/`agent_pool` pattern. This unblocks library
  consumers (e.g. Oxios) that build agents via `AgentBuilder`/`Agent::new`
  from configuring issue #28 gaps 1 & 3 — previously the fields existed only
  on `AgentLoopConfig`, which is constructed internally with no injection point.

### Fixed

- **CI: PR gate shell injection**. PR title and body were interpolated via
  `${{ github.event.pull_request.* }}` directly into `run:` bash heredocs.
  PR bodies containing markdown backticks (e.g. `` `AgentConfig` ``) triggered
  command substitution, and unbalanced parens inside backticks produced
  syntax errors that falsely failed the gate on otherwise-correct PRs.
  Fixed via `env:` indirection (`PR_TITLE`/`PR_BODY`).

### Changed

- **Dependency updates — 14 cargo major version bumps** (#25): toml 0.9,
  jsonschema 0.46, rand 0.9, dirs 6, rusqlite 0.40, notify 8, zip 8,
  lru 0.18, similar 3, fastembed 5, strum 0.28, criterion 0.8, libloading 0.9.
  5 RustCrypto bumps (sha2/digest/hmac/signature/pkcs8) reverted — incompatible
  with stable rsa 0.9.x (depends on digest 0.10; no stable rsa release supports
  digest 0.11 yet). `self_update` pinned at 0.41 (0.44 has internal compile
  break; crate is declared but unused in `oxicode-cli`).
- **CI: actions/checkout v5→v7, actions/upload-artifact v4→v7** (#21).

## [0.53.0] - 2026-07-03

### Fixed

- **`oxicode-agent` — issue #28 gap 2: provider-reported token accounting for
  compaction** (addresses #28 gap 2 — the smallest, highest-leverage fix;
  gaps 1 and 3 follow in separate PRs). #28 remains open.

  The agent loop now drives `CompactionStrategy::Threshold` from the
  provider-reported `usage.input_tokens` (ground truth) instead of the
  legacy `serialized_json.len() / 4` heuristic. The heuristic can
  undercount by 3-4× on token-dense content (base64, JSON, CJK) and was
  the reason `Threshold` was effectively a no-op in the failure mode
  reported in #28 (35k estimated vs 122k actual, 139 KB request body
  growing monotonically across 11 tool rounds). With this fix, compaction
  fires on turn 2+ using the count the provider actually saw.

  - **`oxicode-agent/src/state.rs`**: `AgentState` now carries a
    non-cumulative `last_input_tokens: Option<usize>`, plus
    `last_estimate_at_report` and `last_estimate_divergence` for
    observability. New `current_token_source()` returns
    `TokenSource::{None, Heuristic(n), Real(n)}` — `Real` once a
    `ProviderEvent::Done` has been observed, `Heuristic` only on cold
    start (turn 1). `clear()` resets the new fields alongside the
    existing cumulative counters. The existing `AgentState::input_tokens`
    remains cumulative for lifetime accounting.

  - **`oxicode-agent/src/agent_loop/streaming.rs`**: the `Done` handler now
    records the provider-reported `usage.input` via
    `AgentState::record_provider_turn` alongside the existing
    `record_usage`. The drift estimate is taken over the **prompt-only
    prefix** `&messages[..messages.len() - 1]` (the trailing element at
    `Done` is the just-completed assistant message, which the provider
    did not tokenize as input).

  - **`oxicode-agent/src/agent_loop/mod.rs`** (`maybe_compact`): now reads
    `current_token_source()` and uses the `Real` value when available;
    falls back to the heuristic only on cold start. Emits a `tracing::warn!`
    when the last `last_estimate_divergence` exceeds 2.0 so the operator
    can see how badly the heuristic is undercounting.

  - **`oxicode-agent/src/compaction.rs`**: `CompactionEvent::Triggered` gained
    a `source: String` field
    (`"provider-reported"` / `"bytes/4 heuristic (cold start)"` /
    `"empty"`) so consumers can tell whether a trigger is real-token-based
    or heuristic. The new test
    `test_provider_reported_usage_drives_compaction_threshold` asserts
    this end-to-end through the loop with a 900-token reported usage
    against a conversation whose `bytes/4` estimate is far below the
    800-token `Threshold(0.8) * 1000` cutoff.

  - **Test infrastructure**: `MockResponse` gained an optional
    `usage: oxicode_ai::Usage` field (default: zero) and a `with_usage(input)`
    builder; `MockStream` carries it onto the synthetic `Done` event.
    Every existing `MockResponse { content: ... }` literal was migrated
    to `..Default::default()` — no test logic changed.

### Added — issue #28 gaps 1 + 3

- **Gap 1: tool-result eviction** (`oxicode-agent/src/agent_loop/config.rs`,
  `mod.rs`). `AgentLoopConfig` gains `max_tool_result_bytes: Option<usize>`.
  When set, tool results whose text content exceeds the limit are
  truncated before being pushed into the message history, with a marker
  appended: `"... [truncated: N bytes omitted]"`. Default `None`
  (unlimited) — opt-in. This prevents a single large tool output (huge
  file read, verbose bash output) from consuming the context window.

- **Gap 3: library-native delegation** (`oxicode-agent/src/tools.rs`,
  `oxicode-agent/src/tools/subagent.rs`, `oxicode-sdk/src/delegation.rs`).
  New `SubagentRunner` trait + `ForkResult` type. When wired into
  `ToolContext` via `with_subagent_runner`, the `subagent` tool prefers
  an **in-process** isolated sub-agent run over shelling out to the CLI
  binary. `oxicode-sdk` provides `SdkSubagentRunner` — wraps an `Oxicode`
  instance, builds a fresh `Agent` with an empty context per call, runs
  it, and returns only the final text + usage. Library consumers (Oxios)
  that embed `oxicode-agent` without an `oxicode` subprocess can now use
  delegation.

  - **Depth safety**: the in-process path uses `ToolContext.subagent_depth`
    (a `u8` field) instead of env vars (`OXICODE_SUBAGENT_DEPTH`). Concurrent
    `std::env::set_var` is UB and state leaks between sequential forks;
    the config field avoids both. The CLI backend retains its env-var
    mechanism (safe: each subprocess has its own env).
  - `AgentLoopConfig` gains `subagent_runner` + `subagent_depth` fields.
  - `ToolContext` gains matching fields, wired via `build_tool_context`.
  - The `subagent` tool checks `ctx.subagent_runner` first; if `Some`,
    calls `execute_in_process` (handles single/parallel/chain modes).
    If `None`, falls back to the existing CLI spawn path — no behavior
    change for `oxicode-cli`.

### Notes

- All three gaps from #28 are now addressed: gap 2 (compaction accuracy,
  PR #29), gap 1 (tool-result eviction), and gap 3 (library-native
  delegation). #28 can be closed.
- **CI / packaging hygiene** (commit ba7886d9): repaired 4 broken
  intra-doc links in `oxicode-sdk` introduced by PR #31's refactor
  (`cargo doc -D warnings` had been failing on `crate::OxicodeBuilder::agent`
  and `crate::builder::AgentBuilder`); allowlisted two `quick-xml` 0.23.1
  DoS advisories (RUSTSEC-2026-0194/0195) that are unfixable upstream
  (no `self_update` release pulls `quick-xml` ≥0.41) and sit behind the
  optional non-default `self-update` feature; bumped `anyhow`
  1.0.102 → 1.0.103 to clear RUSTSEC-2026-0190.

## [0.52.1] - 2026-06-30

### Fixed

- **`oxicode-agent/src/agent_loop/streaming.rs`**: `ProviderEvent::ThinkingStart`
  and `ProviderEvent::ThinkingDelta` now emit the discrete
  `AgentEvent::Thinking` and `AgentEvent::ThinkingDelta { text }` events
  in addition to the existing `MessageUpdate`. Previously these variants
  were defined in `events.rs` but had no emission site, so kernel code
  subscribed to live reasoning updates saw nothing. Now reasoning text
  streams live for every provider that emits `ProviderEvent::Thinking*`
  (Anthropic, Bedrock, Google, DeepSeek, ZAI). The `MessageUpdate` emit
  on `ThinkingDelta` is preserved with `delta.clone()` so the move into
  `MessageUpdate { delta: Some(delta) }` still compiles.


## [0.52.0] - 2026-06-30

### Added — Mnemopi memory engine wiring + observability middleware

- **`oxicode-mnemopi` crate wired into `oxicode-cli` composition root**: the
  SQLite vector memory engine (ported from omp Mnemopi) is now
  constructed at boot via `services::start_memory_pipeline`, providing
  auto-recall on session start, auto-retain on each turn, and
  consolidate-on-dispose lifecycle hooks.
- **Observability middleware pipeline**: `AgentBuilder` now wires
  `AuditLogMiddleware`, `CostTrackerMiddleware`, and
  `AuthorizerMiddleware` through `build_hooks`, with event dispatch
  for real-time agent event monitoring.
- **`publish.yml` now includes `oxicode-mnemopi`**: added to the publish
  matrix, package-check leaf list, and wait-step (CI waits for both
  `oxicode-sdk` and `oxicode-mnemopi` before publishing `oxicode-cli`).

### Fixed

- **`oxicode-sdk/tests/integration.rs`**: fixed unclosed delimiter in
  `oxicode_instance_isolation()` — orphaned test functions now properly
  top-level.
- **`oxicode-mnemopi-mcp.rs`**: `RemoteEmbeddingProvider::new()` returns
  `Self` directly (not `Result`) — removed spurious `match`.
- **`oxicode-mnemopi/src/mcp.rs`**: fixed broken intra-doc link
  (`MnemopiDb::with_conn` → `crate::MnemopiDb::with_conn`).
- **2x `unused mut` lints**: removed stale `mut` from `let mut agent`
  in integration tests where `add_tool` uses interior mutability.

## [0.51.0] - 2026-06-29

### Fixed — crates.io publish pipeline (publish blocker since 0.50.0)

- **TLS backend unification**: forced the entire workspace off `native-tls` /
  OpenSSL onto `rustls`, resolving the OpenSSL/btls-sys linker conflict that
  failed the `cargo nextest` compile step on `ubuntu-latest` (undefined
  symbols `SSL_read_ex` / `SSL_write_ex` / `SSL_CTX_ctrl`). `v0.50.0` was
  tagged but **never published** because of this — it is skipped on the
  registry and `0.51.0` ships its content too. Changes: `oxicode-agent` and
  `oxicode-ai` `reqwest` now use `default-features = false` + `rustls-tls`;
  `oxicode-ai` `jsonschema` drops `resolve-http` (only in-memory
  `Validator::new` is used — no remote `$ref`); `oxicode-cli` `self_update`
  switches to its `rustls` feature. After the fix `openssl-sys` /
  `native-tls` / `openssl` are absent from the Linux target dependency tree
  entirely; `btls-sys` (via `oxibrowser`) is retained. (`oxicode-sdk` and
  `oxicode-cli`'s own `reqwest` already used rustls.)
- **`oxicode-ai` Anthropic adapter**: strips a trailing `/v1` from the configured
  base URL to prevent a double-`/v1` (`/v1/v1/...`) 404 when the base URL
  already includes the version segment.

### Fixed — `cargo doc -D warnings` CI job

- Resolved 30 broken / private / feature-gated intra-doc links across
  `oxicode-mnemopi`, `oxicode-hashline`, `oxicode-agent`, `oxicode-cli`, and `oxicode-tui`
  (file-path refs, private consts, cfg-gated types, cross-crate port types,
  and `Self::`-scoped method links). The job had been aborting at `oxicode-tui`
  and hiding a cascade of pre-existing warnings.

## [0.50.0] - 2026-06-28

### Fixed — product namespace isolation (`OXICODE_HOME` unification)

- **`oxicode-ai` no longer hardcodes `~/.oxicode/`** for its catalog-override probe,
  models.dev cache, and auth store. These three reads now resolve through the
  `OXICODE_HOME` environment variable (falling back to `~/.oxicode`), matching the
  convention `oxicode-sdk` already used. Previously, every embedder of `oxicode-ai`
  (including `oxios`) involuntarily inherited the `oxicode-cli` catalog-override
  namespace on the provider hot path — a library-layer coupling smell.
- New module `oxicode_ai::product_env` (`home_dir`/`catalog_override_dir`/
  `cache_dir`/`auth_path`) is the single source of truth; `oxicode_sdk::fs::home_dir`
  now delegates to it, removing a duplicated resolution path.
- **Backward compatible**: with `OXICODE_HOME` unset, all paths are identical to
  prior behavior. Embedders isolate with `OXICODE_HOME=~/.oxios`.
- `find_override_files` refactored to a testable `find_override_files_at`
  core (parameterized global dir) — race-free regression tests, no env
  mutation.
- Fixed pre-existing broken intra-doc links in `oxicode-sdk/src/agent_builder.rs`
  (`TodoStateProvider`) that failed the `cargo doc -D warnings` CI job.
- Design doc: `docs/designs/2026-06-28-product-namespace-isolation.md`.

## [0.49.0] - 2026-06-28

### Added — omp-parity browser capabilities (pure-Rust, no Chrome)

- **`oxicode-cli` opt-in `native-browser` feature**: building with
  `--features native-browser` now constructs the pure-Rust
  `oxibrowser-core` headless engine in `build_app` and registers the
  browse tools (`browse`, `browse_extract`, `browse_script`,
  `browse_session`). The default build is unchanged — the browser stays
  dormant unless the feature is enabled. This closes the long-standing
  "tools exist but ship unreachable" gap.
- **`BrowserTab::wait_for_condition`** + `BrowseWaitCondition` enum
  (`Visible`/`NetworkIdle`/`DomContentLoaded`/`Load`): structured waits
  matching omp's `waitFor*`/network-idle. The native backend maps 1:1 to
  `oxibrowser-core`'s `WaitCondition` (real in-flight-traffic NetworkIdle
  with a quiet window); the default impl degrades `Visible` to `wait_for`.
- **`browse_session` `wait` action**: `wait_condition` ∈
  `{network_idle, dom_content_loaded, load}` + `timeout_ms`.
- **`BrowserTab::observe`** + `Observation`/`ObservedElement` types
  (omp `observe()` parity): the native backend synthesizes the page's
  visible/interactive surface via a JS walk and returns stable
  `data-oxicode-ref="eN"` selectors the agent can `click`/`fill`/`type` by.
  **Experimental / best-effort: the synthesis JS is not yet
  runtime-validated against live boa pages** — the chief risk is
  over-inclusion if `getComputedStyle` returns empty strings for some
  properties. No coordinates are returned (the boa layout engine only
  approximates geometry). See the `Observation` doc-comment.
- **`browse_session` `observe` action**: returns the `Observation` as JSON.

### Added — Advisor engine (omp parity)

- **Advisor runtime** (`oxicode-agent/src/advisor/`): second, read-only LLM agent
  that watches the primary agent's transcript turn-by-turn and emits advisory
  notes (nit/concern/blocker severity) through configurable delivery channels
  (aside, intercept, post-interrupt).
- **Advisor delivery channels**: `AdvisorEmissionGuard` deduplicates notes
  (normalized NFKC, FIFO at 4096 entries); `resolve_delivery_channel` maps
  severity + immune turn state to `intercept`, `aside`, or `post-interrupt`.
- **Advisor context injection**: `assemble_advisor_system_prompt` discovers
  `AGENTS.md`/`CLAUDE.md` for `<project-context>` and `WATCHDOG.md` for
  `<attention>` blocks from `cwd` → home + `~/.oxicode/WATCHDOG.md`.
- **Wired into oxicode-cli**: the advisor is constructed at session start with
  its own provider/model, receives turn-end events via `AgentEvent::TurnEnd`,
  and delivers notes through the event bus.

### Added — Mnemopi memory engine (Rust port from omp)

- **New `oxicode-mnemopi` crate**: local SQLite vector memory engine with
  LRU-cached embeddings, semantic search, episodic/semantic memory store,
  and consolidate/reorganize operations.

### Fixed

- **CI**: added `libssl-dev` to MSRV and smoke-test CI jobs (openssl-sys
  linker error on ubuntu-latest).
- **Security**: bumped `lru` 0.12 → 0.16.4 to resolve RUSTSEC-2026-0002
  (IterMut violates Stacked Borrows).
- **Format**: auto-applied `cargo fmt` fixes across advisor and session code.

### Changed

- **`oxibrowser`/`oxibrowser-core` 0.15 → 0.16.0** (`oxicode-agent`, `oxicode-sdk`).
  Picks up upstream `ChallengeDetector` (Cloudflare/Turnstile/reCAPTCHA/
  hCaptcha — auto-retry on `goto`, transparent to callers), the always-on
  V8-parity stealth bootstrap, the native `extract.rs` engine, and the
  `wreq` HTTP backend. oxicode touches oxibrowser only via `Browser`/`Tab`/
  `BrowserConfig`/`BrowserEvent`/`BrowseResult`, so the upstream
  `HttpClient::request() → FetchOutcome` breaking change has zero impact.

## [0.48.0] - 2026-06-26

### Added

- **Model roles system** (ported from omp `model-roles`): named role →
  model-pattern assignments (`model_roles` settings field) with `pi/<role>`
  alias expansion and cycle detection. Defaults are empty, so the existing
  single-model path is unchanged unless a user opts in.
  - New `oxicode-ai` modules: `roles` (registry + resolution), `role_switcher`
    (signal-based decision engine — explicit override → current tool → long
    context → thinking → trivial → default), and `role_routing` (a
    transparent `Provider` wrapper that routes each request to the selected
    model without changes to `oxicode-agent`'s loop).
  - **`oxicode-sdk` re-exports the full surface** (`ModelRole`, `RoleRegistry`,
    `RoleRoutingProvider`, `decide_role`, `resolve_role_to_model`, …), so
    products follow the single-dependency pattern (`oxios → oxicode-sdk`, no
    `oxicode-ai` direct dep).
  - **`oxicode-cli` wiring**: the TUI session build path wraps every agent with
    the role router (edits apply live on the next turn); a configured
    `commit` role upgrades the deterministic `CommitTool` to an LLM-backed
    one; settings migrated v8 → v9 (`model_roles`, defaults empty); new
    `/roles` overlay + slash command for live per-role model assignment.

### Security

- Bumped `quinn-proto` 0.11.14 → 0.11.15 ([RUSTSEC-2026-0185](https://rustsec.org/advisories/RUSTSEC-2026-0185),
  remote memory exhaustion from unbounded out-of-order stream reassembly,
  transitive via `reqwest`).

## [0.47.0] - 2026-06-24

### Added — 테마 시스템 전면 재설계 (Phase 1 + 2)

- **7 new background color slots** added to `ColorScheme` (total: 28):
  `response_bg`, `thinking_bg`, `surface_bg`, `panel_bg`, `diff_add_bg`,
  `diff_remove_bg`, `diff_hunk_bg`. Each is themeable via TOML/JSON and
  populated with derived values across all 6 built-in themes
  (`oxicode-tui/src/theme.rs`).

### Fixed — dead theme slots wired to render code

- **`user_bg`**: user message rows now fill the entire row width with
  `user_bg` (previously only a 1-cell border stripe; the slot was defined
  but never consumed) (`chat/render.rs`).
- **`code_bg`**: fenced code blocks now paint `code_bg` via Line-level
  style in `highlight_code()`; inline `` `code` `` uses theme-aware
  `OxicodeStyleSheet` (previously hardcoded to dark-theme amber `#231e14`
  regardless of active theme) (`highlight.rs`, `markdown_styles.rs`).
- **`selection_bg`**: dashboard selection now uses explicit
  `.bg(selection_bg)` instead of `Modifier::REVERSED` (terminal-default
  swap) (`dashboard.rs`).
- **Chat viewport, footer, completion popup, routing overlay, settings
  overlay, thinking blocks**: all now paint their respective background
  colors (`background`, `surface_bg`, `panel_bg`, `thinking_bg`) instead
  of relying on terminal default transparency.
- **Diff rendering**: added/removed/hunk lines now carry semantic
  background tints (`diff_add_bg`, `diff_remove_bg`, `diff_hunk_bg`)
  alongside their foreground colors via `Style::patch()` composition
  (`tool_renderer.rs`).
- **`DashboardWidget`**: no longer hardcodes `Theme::dark()` — accepts
  `&Theme` as a parameter, so the MCP dashboard overlay respects the
  active theme (`dashboard.rs`, `mcp_dashboard.rs`).
- **`OxicodeStyleSheet`**: now theme-aware — heading, code, link, and
  blockquote styles derive from the active `ThemeStyles` instead of
  hardcoded RGB values. Constructed via `OxicodeStyleSheet::from_styles()`
  (`markdown_styles.rs`).

### Docs

- `docs/THEME_GUIDE.md`: new comprehensive theme authoring guide with
  full 28-slot TOML schema, color format reference, and brightness
  hierarchy documentation.
- `docs/designs/2026-06-24-theme-system-redesign.md`: finalized design
  document (audit → review → implementation).
- `examples/theme_demo.rs`: updated to print all 28 slots across 6
  built-in themes.

### Fixed — MCP dashboard completion (`/mcp dashboard`)

- **`Disconnect` (`D`)**: was a "not yet implemented" stub. Implemented
  `McpManager::disconnect` (public, idempotent — returns whether a live
  connection was actually closed) and wired the `D` key to it
  (`handlers.rs`, `mcp/mod.rs`).
- **Consent keying bug**: the dashboard *displayed* consent via the
  original (un-prefixed) tool name but *wrote* it via the prefixed name
  (never read back at enforcement time), so `a`/`x` were silent no-ops.
  All three paths — badge, `decide`, and `check` — now use the original
  name. The overlay tracks per-item `ItemMeta { server, is_tool,
  original_name }` so `a`/`x` only fire on tools (`mcp_dashboard.rs`).
- **Server targeting from a tool selection**: `r`/`D` previously used the
  selected item's id verbatim, so reconnect/disconnect on a *tool*
  targeted the prefixed tool name and failed. They now target the owning
  server (`meta.server`).
- **`mark_refresh` never worked**: it was an inherent method, but every
  call site dispatches through `Box<dyn OverlayComponent>` → the trait
  vtable → the no-op default, so the dashboard never refreshed after
  actions. It is now a proper trait override (`mcp_dashboard.rs`).
- **`SetConsent`** now calls `mark_refresh()` so the ALLOW/DENY badge
  updates immediately; **`ReconnectAll`** now reports
  "reconnected X/Y servers" (`handlers.rs`).

### Added

- **`e: Manage`** key in the MCP dashboard: jumps from the read-only
  runtime view to the interactive management overlay (add/edit/remove
  servers, persisted to disk). New `McpAction::ManageServers` variant
  (`mcp_dashboard.rs`, `handlers.rs`).

### Fixed — `/model` selector shows duplicate models for aliased upstreams (#24)

- **Provider alias deduplication**: when multiple providers share one upstream
  API host (e.g. `zai` and `zai-coding-plan` both hit `api.z.ai`), the `/model`
  selector and `/router` setup no longer list the same model under every alias.
  A catalog provider is now shadowed when the user has *explicitly* registered
  (stored credential or `settings.custom_providers` entry) another provider on
  the same upstream host. Hosts with no explicit registration are left
  untouched, so env-keyed builtins keep working without the setup wizard
  (`oxicode-cli/src/tui/slash/builtin/model.rs`, `router.rs`).

### Fixed — session resume loses tool-call history and context (#23)

- **LLM context restored on resume**: resuming a session now seeds the agent's
  conversation state from the session branch, so the model sees prior user,
  assistant, tool-call, and tool-result turns instead of a blank slate. The
  new `resume_messages_from_branch` is the inverse of the persist mapping and
  preserves `tool_call_id` pairing; compaction summaries are honoured
  (`oxicode-cli/src/app/agent_session.rs`).
- **TUI chat restored on resume**: the resume and goto-entry reconstruction
  paths now render tool-call and tool-result blocks (and correct Assistant
  role) instead of collapsing history to user/assistant text. Shared via the
  new `render_session_branch_to_chat` helper (`oxicode-cli/src/tui/app.rs`).
- **No double-persist**: the safety-net `persist_session` reconciles against
  the on-disk entry count, so seeded history is never rewritten to the JSONL.

## [0.46.0] - 2026-06-22

### Added

- **4 new built-in color schemes**: Nord, Catppuccin Mocha, GitHub Dark,
  and Monokai (`oxicode-tui/src/theme.rs`). Each is a complete 22-field
  `ColorScheme` with contrast-checked palette values.
- **`Theme::by_name()` resolver**: maps `settings.theme` names
  (`oxicode_dark`, `oxicode_light`, `nord`, `catppuccin`, `github_dark`, `monokai`)
  to built-in `Theme` constructors. `THEME_NAMES` constant lists all
  available names for the `/settings` overlay cycle.
- **`Agent::set_compaction_strategy()`**: updates the compaction strategy
  in `inner.config` so the next agent run picks it up — the strategy is
  read fresh at the start of each run (`oxicode-agent/src/agent.rs`).

### Fixed

- **Theme system was dead code — `settings.theme` is now actually applied.**
  The TUI render loop always hardcoded `Theme::dark()`, ignoring the
  `settings.theme` field entirely. `get_theme_name()` had zero callers,
  `ThemeManager` was never instantiated in `oxicode-cli`, and the `/settings`
  overlay's `theme` Choice had no cycling options (`get_choice_options`
  returned `vec![]`). All three are now wired: the render loop
  (`app.rs`) resolves the theme via `Theme::by_name(&settings.get_theme_name())`
  at startup and on reload, the overlay cycles through `THEME_NAMES`, and
  `/reload` sets `appearance_needs_reload` so hand-edited theme changes
  apply without a restart.
- **`auto_compact` toggle now applies live.** Previously, toggling
  `auto_compaction` in the `/settings` overlay persisted to disk but never
  reached the runtime — the `compaction_config` (`Arc<RwLock<CompactionConfig>>`)
  and the Agent's `CompactionStrategy` were both set at session construction
  and never updated. `rebuild_system_prompt()` now syncs both:
  `compaction_config.enabled` (for `auto_compaction_enabled()` display) and
  `Agent::set_compaction_strategy()` (for the next agent-loop run).
- **`extensions` toggle now applies live.** The `/settings` overlay Esc
  handler did not reload/unload WASM extensions — only the `/reload` slash
  command did. The reload logic is extracted into `sync_wasm_extensions()`,
  shared by both paths.
- **`routing` toggle now switches the session model.** `enable_routing`
  was a dead setting (never consumed). The overlay Esc handler now switches
  to `router/auto` when enabled and back to the default model when disabled,
  mirroring `/router enable|disable`.

### Changed

- **`stream_responses` is now read-only in the `/settings` overlay.** The
  setting is persisted but has no consumer (the agent always streams).
  The overlay now displays it as `(not wired)` read-only instead of a
  functional toggle, preventing users from expecting a live effect.

### Fixed (settings consistency across interfaces)

- **`/settings` overlay `max_response_tokens` now shows `effective_max_tokens()`.**
  `oxicode config set max_tokens 8192` sets the `max_tokens` (u32) field, but the
  overlay previously displayed only `max_response_tokens` (usize) — a different
  field. Now merged via `effective_max_tokens()` so CLI and overlay agree.
- **Setup wizard theme list unified with `THEME_NAMES`.** `load_themes()` in
  `setup_wizard.rs` was a hardcoded copy of the 6 theme names, bound to drift
  out of sync with `oxicode_tui::THEME_NAMES`. Now reads from the canonical
  constant.
- **`oxicode config show` displays resolved theme name.** Previously showed the
  raw `settings.theme` (e.g. `"default"`) instead of the resolved name
  (`"oxicode_dark"`), inconsistent with the `/settings` overlay which shows
  `get_theme_name()`.
- **CLI `config set`/`get` gained keys for overlay-editable settings.**
  `glyph`/`glyph_set`, `enable_routing`/`routing`, and
  `language_policy_enabled`/`language_policy` were editable in the `/settings`
  overlay but absent from `oxicode config set` — attempting `oxicode config set glyph
  ascii` failed with "Unknown setting". All three are now CLI-settable.
  `extensions` is also accepted as an alias for `extensions_enabled` in
  `config set`.
- **Overlay `auto_compact` label renamed to `auto_compaction`.** The overlay
  label didn't match the CLI key (`oxicode config set auto_compaction`), making
  cross-reference confusing. Now consistent.
- **`oxicode config show` now displays glyph set and routing status.** Previously
  absent — the only way to check these was the `/settings` overlay.

## [0.45.1] - 2026-06-22

### Fixed

- Strip orphan ToolCall blocks from Assistant messages and align compaction
  split boundary to prevent bisecting tool_call/tool_result pairs

## [0.45.0] - 2026-06-22

### Added

- **Fallback model construction from catalog for providers without built-in trait impls**:
  When `get_model` returns no match, `resolve_model_from_id` now falls back to
  constructing a `Model` from the `ModelEntry` catalog (materialized from models.dev
  snapshot). This handles catalog-only providers (e.g. `zai-coding-plan/glm-5-turbo`)
  that have model data but no native `Provider` trait implementation
  (`oxicode-agent/src/model_id.rs`).

## [0.44.0] - 2026-06-22

### Changed — 설정 마법사 프로바이더 단계 재설계

- **프로바이더 단계 fzf 방식 상시 필터 도입**: 기존 `/` 모달 검색 모드를
  폐지하고 모델 단계와 동일한 상시 라이브 필터 방식으로 통일했다. 입력 즉시
  프로바이더를 필터링하며 필터 입력창은 항상 표시된다. 별도의 검색 모드
  진입 키가 없으므로 모든 인쇄 가능한 문자가 필터에 사용된다
  (`oxicode-cli/src/setup_wizard.rs`).
- **센티넬 행을 스크롤 영역 밖으로 분리**: "+ Add custom provider…" 행을
  프로바이더 리스트(스크롤 영역) 바깥의 영구 1행으로 옮겨 70+ 프로바이더를
  스크롤하더라도 항상 화면에 보이도록 했다. `on_sentinel: bool` 필드가
  선택 위치를 추적하고 `snap_provider_selection`이 필터 변화에 따라
  선택을 보정한다. `↓`/`↑`로 리스트와 센티넬 사이를 자유롭게 이동 가능.
- **API key 삭제를 다이얼로그 안으로 이동**: 1단계 최상위에서 `d`/`Del`로
  하던 키 삭제를 다이얼로그 내부 `Ctrl+R`로 옮겼다. 실수로 누르기 어려운
  비-인쇄 키에 묶어 의도치 않은 삭제 위험을 줄이고, 키 다이얼로그에
  "Ctrl+R: remove" 힌트를 동적으로 표시한다.

- **푸터 단축키 힌트 잘림 수정**: 화면 폭에 따라 푸터가 동적으로 높이를
  계산하고(wrapped_line_count) `Wrap`으로 감싸도록 변경했다. 좁은
  단말기(50열)에서도 힌트가 잘리지 않고 줄바꿈된다. 푸터 힌트 자체도
  "Type to filter · ↑/↓ · Enter: act · → next · Esc back" 등으로 축약해
  가독성과 공간 효율을 높였다. 센티넬 행도 하나의 Span으로 정리.

## [0.43.0] - 2026-06-22

### Changed — 설정 마법사 모델 단계 재설계

- **모델 목록을 구성된 프로바이더로 제한**: 2단계(기본 모델 선택)가 더 이상
  5000+ 전체 카탈로그를 보여주지 않고, 1단계에서 API key를 등록한
  프로바이더의 모델만 표시한다. `load_models`가 `allowed` 프로바이더 집합을
  받아 필터링하고, `keyed_provider_names`가 key가 등록된 프로바이더를
  수집한다. key 추가/삭제 시 `models_dirty` 플래그를 세팅하면 메인 루프가
  2단계 진입 전에 목록을 재구축한다. 구성된 프로바이더가 없으면 빈 상태
  안내("No providers with an API key configured yet. Press Left to go back…")
  로 1단계로 돌려보낸다 (`oxicode-cli/src/setup_wizard.rs`).
- **단계 인디케이터가 사라지던 버그 수정**: 인디케이터가 각 단계 `List`의
  `Borders::NONE` 블록 `.title()`로 들어가 첫 번째 리스트 항목과 같은
  0행을 덮어써 보이지 않았다. 이제 타이틀바 아래 전용 1행(`Length(1)`)
  레이아웃 영역으로 옮겨 네 단계 모두 항상 표시된다. 검증을 위해 렌더링
  클로저를 `render_wizard(f, state)`로 분리하고 `TestBackend` 버퍼 테스트를
  추가했다.
- **`Esc` 단일 뒤로/종료 키 도입**: 기존 `q` 종료 단축키를 폐지하고 `Esc`를
  범용 "한 단계 뒤로" 키로 통일했다. 1단계(프로바이더) 최상위에서 `Esc`는
  종료, 모델 단계에서는 필터가 있으면 지우고 없으면 이전 단계로, 테마·완료
  단계에서도 각각 뒤로/종료로 동작한다. 서브모드(검색·다이얼로그)의
  `Esc` 취소 동작은 기존 그대로 유지된다. `←` 키도 명시적 이전 단계로
  유지된다.

## [0.42.0] - 2026-06-22

### Changed — 설정 마법사 첫 실행 경험 개선

- **첫 실행 시 설정 마법사 자동 실행**: 모델이 구성되지 않은 상태에서
  `oxicode`를 대화형(TUI) 모드로 실행하면 더 이상 `Error: No model
  configured. Run \`oxicode setup\` to configure.`로 종료되지 않고, 즉시
  설정 마법사가 실행된다. `print` / `json` / RPC / 단일 프롬프트 등
  비대화형 모드에서는 기존대로 에러로 종료한다 (`oxicode-cli/src/bootstrap.rs`).
  마법사 종료 후 settings를 다시 로드하고 CLI override를 재적용한다.
- **프로바이더 단계 텍스트 검색 추가**: `/` 키로 검색 모드 진입 후 타이핑으로
  프로바이더를 필터링한다. 필터된 목록을 ↑/↓로 탐색하고 Enter로 선택하여
  API key 편집 다이얼로그로 진입한다. 필터 입력창에 실선 블록 커서 표시
  (`oxicode-cli/src/setup_wizard.rs`).
- **모델 단계 전면 재작성**: 기존 모델 단계는 `Paragraph`로 렌더링되어
  선택 항목 하이라이트가 없었고, 검색 모드에서 ↑/↓ 탐색이 불가해 첫 번째
  매치만 선택 가능했으며, fake `_` 커서가 거의 보이지 않는 문제가 있었다.
  이제 fzf 스타일의 always-on 라이브 필터로 전환: 타이핑 즉시 필터링,
  stateful `List` 위젯으로 `▶` 하이라이트 + 스크롤 지원(거대한 카탈로그
  5000+ 모델 처리), 필터된 결과를 ↑/↓로 자유 탐색, 실선 블록 커서로
  타이핑 위치 명확화.
- **`is_tui_mode` 버그 수정**: `args.prompt.is_empty()`(Vec)를 검사하던 것을
  `dispatch_run_mode`와 일치하게 join된 문자열로 검사하도록 수정. clap의
  `default_value = ""` 때문에 bare `oxicode`가 `prompt == vec![""]`(비어있지 않은
  Vec, 빈 join)이 되어, 대화형 실행을 단일 프롬프트로 오분류하던 잠재
  결함. 기존에는 liveness identity 선택에만 영향했으나 자동 실행 로직에서
  의미를 갖는다.
- **모델 필터링 성능**: `ModelEntry`에 `id_lower`/`provider_lower`를 로드
  시점에 캐시하여 매 키 입력마다 5000+ 모델에 대해 `to_lowercase()`가
  할당하지 않도록 개선. 필터는 캐시된 소문자 필드로 비교.
- **빈 매치 시 Enter 차단**: 모델 단계에서 필터가 하나도 매치하지 않으면
  Enter로 다음 단계로 넘어가지 않는다 (이전 선택이 조용히 이월되던 문제 방지).
- **`merge_cli` 중복 제거**: `build_app`에서 CLI override 적용을 클로저로
  추출하여 초기 적용과 마법사 재실행 후 재적용이 동일 로직을 공유. 새 플래그
  추가 시 한쪽만 갱신하는 silent miss 방지.
- **필터 커서 위치 개선**: 블록 커서가 입력된 텍스트 바로 뒤(빈 경우
  "Filter:" 바로 뒤)에 위치하도록 수정 — 이전에는 placeholder 끝에 있었다.

### Added

- 설정 마법사 필터/탐색 로직 단위 테스트 6개 추가
  (`setup_wizard::tests`): 프로바이더/모델 필터 매칭, 선택 snap 동작.

## [0.41.2] - 2026-06-22

### Security — 코드 감사 보고서 결함 일괄 수정 (audit 2026-06-21)

9개 도메인 분석에서 도출된 15개 결함 중 13건을 수정. 상세 근거와
file:line 인용은 `oxicode-code-audit-report.html` 참조.

#### Critical

- **F-1**: Lockfile 무결성 해시가 write-only — `oxicode-cli/src/storage/packages.rs`
  의 `LockEntry.integrity`가 설치 시에만 계산되고 이후 `load_installed`
  에서 절대 재검증되지 않던 결함. `verify_lockfile_integrity()` 추가 +
  `load_installed()`에서 lockfile-mismatch 발견 시 `installed` 맵에서
  패키지 제외 + `lockfile.packages.remove(name)` 호출. 변조된 패키지는
  silent load 대신 다음 부팅 시 안전한 un-install 상태로 강등.
- **F-2**: 네이티브 확장 무결성 미검증 — `load_extension()`이
  `libloading::Library::new()` 호출 전 SHA-256을 검증하지 않음.
  `expected_checksum: Option<&str>` 파라미터 추가 + `validate_extension`
  wiring. lockfile 무결성 해시와 비교하여 변조된 .so/.dylib/.dll 거부.
  `ValidatedExtension`에 `Debug` derive 추가 (테스트 호환).

#### High

- **F-3**: 프로바이더 공유 HTTP 클라이언트에 timeout 없음 —
  `oxicode-ai/src/providers/mod.rs::shared_client()`가 `reqwest::Client::new()`
  로 빌드되어 LLM 스트림이 영원히 hang 가능. `connect_timeout(10s)` +
  `timeout(600s)` 설정 추가. CLI의 동일 함수
  (`oxicode-cli/src/util/http_client.rs:14-22`)와 동일한 패턴.
- **F-4**: `keyring` feature가 코드에는 있는데 `Cargo.toml`에는 없는
  dead module. `Enable the 'keyring' feature` 경고가 사용자에게 거짓
  안내. 실제 위치(`~/.oxicode/auth.json`)와 옵션(`oxicode-auth-keyring` crate)
  을 가리키는 정확한 안내로 교체 + `keyring_support` 모듈에
  `#[deprecated(note = ...)]` 추가.
- **F-6 (partial)**: 8개 프로바이더 SSE 파서의 2-pass scan (`text.split('\n').filter(count)` + `events.reserve()` + parse) 제거.
  `events.reserve(estimated_events)`를 `Vec::with_capacity(text.len() / 80)`
  로 교체 (openai_responses는 `/40`). `Arc::new(partial.clone())` O(N²) 패턴은
  **이번 PR scope 밖** — 별도 추적 (cross-cutting 시그니처 변경 필요).
- **F-7 (partial)**: `session.rs::_persist`의 매번 `fs::OpenOptions::open()`
  + `writeln!` 패턴에 `BufWriter` wrapping 추가 — per-line `write()` syscall
  → 8 KiB buffer. File handle 캐싱과 단일 Mutex 통합은 **별도 추적**.
- **F-8**: `AgentTool::execute()`가 항상 `signal: None`으로 호출되어
  oneshot 취소 계약이 사실상 무력화되던 결함. `AgentLoop::cancel_signal()`
  getter 추가 + `make_cancellation()` 헬퍼로 cancel flag → `Arc<Notify>`
  브릿지 + 호출 측 `tokio::select!` wrapping. sequential/parallel 양쪽
  경로 적용. 도구의 `signal: Option<oneshot::Receiver<()>>` 파라미터
  시그니처는 변경하지 않음 (additive).
- **F-9**: `stream_retry.rs::is_retryable`이 `ProviderError::MissingApiKey`를
  transient로 오분류하여 14초 backoff 낭비. early-return 추가:
  `"missing API key — set the corresponding *_API_KEY env var or run \`oxicode setup\`"`.
  **리뷰 중 추가 수정**: `on_failure()` callback 호출이 MissingApiKey 분기
  *후*에 실행되도록 순서 조정 — 그렇지 않으면 MissingApiKey가
  circuit breaker의 `consecutive_failures`를 증가시켜 5회 반복 시
  circuit open, 이후 정상 API key 설정 후에도 recovery timeout 동안
  모든 요청 거부. Configuration error는 transient가 아니므로
  circuit breaker 카운트에서 제외.
- **F-10**: `BashTool::is_dangerous_command`가 warn만, 차단 없음.
  `OXICODE_STRICT_BASH=1` 환경 변수가 켜져 있으면 실행 전 차단.
  기존 동작은 보존(opt-in).
- **F-15**: `publish.yml`의 `cargo publish`에 `--locked` 누락.
  `cargo publish --locked --token "$CARGO_REGISTRY_TOKEN"` 로 변경.
  ci.yml:147, sbom.yml:41과 일치.

#### Medium

- **F-13**: `store/settings.rs`가 `oxicode_tui::GlyphSet`을 import (데이터
  레이어 → UI 레이어 의존). 실제 분리는 on-disk TOML 호환성 + 5개
  call site 변경이 필요하여 **follow-up PR**로 미루고, 같은 파일에
  layering violation 의도를 명시한 doc-comment 추가.

#### Verified regressions

- `verify_lockfile_integrity_accepts_matching_dir` (F-1)
- `verify_lockfile_integrity_rejects_tampered_dir` (F-1)
- `verify_lockfile_integrity_rejects_bad_prefix` (F-1)
- `load_installed_skips_tampered_package` (F-1)
- `validate_extension_is_deterministic` (F-2)
- `validate_extension_distinguishes_content` (F-2)
- `validate_extension_rejects_wrong_platform_ext_on_macos` (F-2, macOS only)
- `validate_extension_handles_missing_path` (F-2)
- `test_strict_bash_blocks_pipe_to_shell` (F-10, requires `--test-threads=1`)
- `test_strict_bash_off_preserves_warning_behavior` (F-10)

#### Deferred (큰 구조 변경)

- **F-5**: `main.rs` (1,622 LOC) 핸들러 분할 — clap의 `Subcommand`-derived
  enum을 sibling module로 옮길 때 generic-bound surprise 발생. 별도 PR에서
  각 subcommand별로 명시적 테스트와 함께 진행. AGENTS.md 갱신으로
  부정확한 "12-line dispatcher" claim을 정직한 설명으로 교체.
- **F-11**: 컴포지션 루트 단일화 (lib.rs ↔ bootstrap.rs ↔ services.rs).
  1,080 LOC의 wiring이 3개 파일에 분산. F-5와 함께 진행.
- **F-12**: `App` 구조체 10-field 분할 (AppCore / AppExtensions /
  IssueOwnership). 13+ caller 영향. F-5와 함께 진행.

### 검증

- `cargo fmt --all -- --check` 통과.
- `cargo clippy --workspace --all-targets -- -D warnings` 통과.
- `cargo nextest run --workspace --profile ci`: **2751 passed, 0 failed**.
- `cargo test --workspace --doc`: **40 passed, 0 failed**.


## [0.41.0] - 2026-06-21
## [0.41.1] - 2026-06-21

### Fixed — `cargo install oxicode-cli` failed on v0.41.0 release build

The v0.41.0 release's TUI startup called `AskBridge::attach()`
directly, which is gated behind
`#[cfg(any(test, debug_assertions))]` and only exists in debug
builds. Release builds (`cargo build --release`,
`cargo install oxicode-cli`) failed to compile with
`no method named 'attach' found for reference '&Arc<AskBridge>'`.

- **Switched the call site** (`oxicode-cli/src/tui/app.rs`) from
  `bridge.attach()` to
  `bridge.attach_with_session(app.ownership_session_id())`.
  `attach_with_session` is the production-bound API (matches the
  ownership-identity invariant from AGENTS.md pitfall
  "Issue-system ownership identity"); `attach()` is the test-only
  convenience and was never meant to ship.
- **Verified** by `cargo build --release -p oxicode-cli` and a
  full `cargo install oxicode-cli --version 0.41.1` smoke test.

No source-level behavior change beyond the wire: the session
identity was already bound by the issue-system invariant, this
release just removes the dependency on the test-only stub.


### Added — TUI widget layer reads from glyph-set table

The widget layer (`footer`, `chat render`, `highlight`, `completion`,
`routing`, `tool renderer`) now reads every UI glyph from the `Symbols`
table that ships with each `GlyphSet` preset. The ASCII and Nerd Font
presets can now fully re-skin the TUI from one setting, completing the
glyph-set feature whose table shipped in v0.40.0.

- **New symbol fields**: arrows (`arrow_up`, `arrow_down`), thinking
  levels (off / minimal / low / medium / high), tool icons
  (`tool_bash`, `tool_edit`, `tool_write`, `tool_read`, `tool_search`,
  `tool_task`, `tool_web`, `tool_lsp`, `tool_debug`, `tool_mcp`,
  `tool_ask`, `tool_generic`), and language identifiers
  (`icon_lang_rust`, `icon_lang_python`, `icon_lang_javascript`,
  `icon_lang_typescript`, `icon_lang_go`, `icon_lang_ruby`).
- **Migrated call sites**: footer token arrows, chat header thinking
  icon, completion/overlay separators, routing health indicators, and
  the tool renderer status marks all draw from `styles.symbols.*`.
- **Hard rule reaffirmed** (AGENTS.md): widgets never hardcode glyphs
  — read from `Symbols` so the `glyph_set` setting re-skins the whole
  UI. Migrated call sites are the canonical reference for the rule.

### Changed — TUI 언어 정책 default OFF + 자동 적용

기존 TUI 언어 정책(`Settings::output_languages`)은 **사용자 설정이 있어도
기본적으로 활성화되지 않도록** 변경되었으며, 오버레이 변경이 라이브
세션에 즉시 반영되도록 개선되었다. `oxicode --print` 및 RPC 모드의 비대칭은
**의도된 설계**이므로 변경되지 않았다 (AGENTS.md pitfalls 참조).

- **Master toggle 신설** (`Settings::language_policy_enabled: bool`,
  `oxicode-cli/src/store/settings.rs`): default `false`. `output_languages`
  맵에 값이 있어도 이 플래그가 `false`이면 정책이 주입되지 않는다.
  신규/기존(v5) 사용자 모두 OFF로 시작한다. `/settings` 오버레이에서
  명시적으로 ON 해야 동작.
- **자동 적용** (`oxicode-cli/src/tui/overlay/settings.rs`,
  `oxicode-cli/src/app/agent_session.rs`): `/settings` 오버레이 Esc 시
  `changed=true`이면 `persist_changes()` + `AgentSession::rebuild_system_prompt()`
  를 자동 호출한다. 디스크 저장이 `set_system_prompt()`까지 단일 흐름으로
  연결된다. `/reload` 슬래시 명령은 백업 경로로 유지.
- **in-memory 캐시 동기화** (`rebuild_system_prompt`): 호출 직전에
  디스크에서 fresh load하여 `AgentSession::settings` `Arc<RwLock<Settings>>`
  를 교체한다. overlay가 `AgentSession` mutable API를 알 필요 없이
  결정적으로 동기화됨.
- **OFF 시 채널 설정 보존**: `language_policy_enabled`를 false로 두어도
  `output_languages` 맵은 디스크에 보존된다. 다시 ON 하면 이전 채널
  매핑이 그대로 적용됨.
- **Disabled UI** (`SettingsItem::Choice::disabled: bool`): OFF일 때
  채널 항목 4개는 회색으로 표시되고 `Enter`/`Space`로 순환되지 않는다.
  시도 시 "Enable language_policy first." notification 표시.
- **시그니처 변경** (`oxicode-cli/src/prompt/system_prompt.rs`,
  `oxicode-cli/src/app/agent_session_runtime.rs`):
  `language_directive(enabled: bool, channels: &HashMap<...>)` —
  마스터 게이트 신규. `build_system_prompt(thinking, enabled, languages)`,
  `build_compaction_instruction(enabled, languages)` — `enabled` 인자 추가.
  기존 8개 테스트 시그니처 반영 + 신규 8개 테스트 추가.
- **마이그레이션** (`Settings::SETTINGS_VERSION: 5 → 6`): 누락 시
  `#[serde(default = "default_false")]`로 안전하게 false로 떨어진다.
  별도 데이터 변환 없음 (필드 추가 only).
- **문서**: `AGENTS.md` pitfalls에 "TUI-only — by design, not oversight"
  및 채널이 분류기가 아님을 명시. `/settings` 슬래시 description 정정.
  `docs/designs/2026-06-17-tui-language-policy.md` 신규.

### Changed — Edition upgrade (2024 edition)

- **Rust edition**: upgraded from 2021 to **2024** across all workspace
  crates (`oxicode-ai`, `oxicode-agent`, `oxicode-tui`, `oxicode-sdk`, `oxicode-cli`,
  `scripts`).
- **MSRV**: bumped from **1.82** to **1.96** (2024 edition requires
  Rust ≥ 1.85; 1.96 is the MSRV floor going forward).
- `rust-toolchain.toml` now pins to channel `1.96` (was `stable`).
- All workspace crates inherit `edition` and `rust-version` from
  `[workspace.package]` in the root `Cargo.toml`.
- **Match ergonomics (2024)**: removed redundant `ref`/`ref mut`
  bindings in patterns matching on references — the compiler now
  implicitly borrows in these positions.
- **`set_var`/`remove_var` → unsafe**: wrapped all calls to
  `std::env::set_var` and `std::env::remove_var` in `unsafe {}`
  blocks (these functions became `unsafe fn` in the 2024 edition).
  Affected files: `oxicode-cli/src/store/settings.rs`,
  `oxicode-ai/src/providers/vertex.rs`,
  `oxicode-ai/src/providers/register_builtins.rs`,
  `oxicode-ai/src/env_api_keys.rs`,
  `oxicode-ai/src/provider_registry.rs`.
- **Clippy 1.96**: auto-fixed `collapsible_if` and `let_and_return`
  lints (new in Rust 1.96 clippy) across the workspace.
- **CI**: `RUST_VERSION_MSRV` in `.github/workflows/ci.yml` updated
  to `1.96`.
- **README**: Rust badge and install instructions updated to reflect
  the new MSRV (≥ 1.96).

### Documentation

- **Added** `docs/release-process.md` — internal release playbook
  capturing the workspace release workflow (version bump across all
  six crates, CHANGELOG update, push, `publish.yml` topological-order
  publish, rollback) for future maintainers.

### Scope decisions (2026-06-07)

- **Distribution channel:** crates.io only. No Homebrew tap, no Scoop
  bucket, no apt/yum repos.
- **Build target:** `aarch64-apple-darwin` (macOS Apple Silicon) only.
  The maintainer does not have access to Linux or Windows build
  environments, so cross-OS verification is not part of this pipeline.
- **Supply chain:** SHA256SUMS generated on every release (unsigned).
  No GPG signing, no Codecov coverage reporting.

### Added — CI/CD & Supply Chain

- **`release.yml` enhancements**:
  - New `tag-check` job rejects tags not reachable from `origin/main`
    (defense against force-pushed stale tags).
  - Release job now generates `SHA256SUMS` next to binaries.
  - CycloneDX 1.5 SBOM (`oxicode.cdx.json`) attached to the GitHub release.
  - Matrix simplified to a single target (`aarch64-apple-darwin`).
- **`publish.yml`** (new) — publishes all 6 workspace crates to
  `crates.io` in topological order on `release: published`, with a
  dry-run `cargo package --no-verify` pre-flight. Requires `CARGO_TOKEN`
  secret. Run `workflow_dispatch` for a manual dry run.
- **`sbom.yml`** (new) — generates a CycloneDX SBOM on every push to
  `main`, submits it to GitHub's dependency-graph API (so Dependabot
  sees transitive crates), and uploads the JSON as a workflow artifact.
- **`labels.yml`** (new) — single source of truth for issue labels
  (priority, area, status, type, provider). 30+ labels, including
  `good first issue` and `help wanted`. Synced to the repo by the
  `labels.yml` workflow (weekly + on labels.yml change).
- **`FUNDING.yml`** (new) — surfaces a "Sponsor" button on the repo
  page (GitHub Sponsors).
- **`.pre-commit-config.yaml`** (new) — local pre-commit hooks that
  mirror the ci.yml gate: trailing whitespace, EOF, YAML/TOML lint,
  merge-conflict, large files, private keys, no-commit-to-main,
  `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`.
- **`ci.yml` enhancements**:
  - `smoke-test` now has a 15-min timeout.
  - New `msrv` job verifies the workspace builds on Rust 1.82.
  - New `doc` job builds `cargo doc --no-deps` with
    `RUSTDOCFLAGS="-D warnings"`.
- **`test.yml` enhancements**:
  - Triggered on `pull_request` (was: main-only). Every PR now runs
    the full nextest matrix.
  - Matrix simplified to `macos-latest` only.
- **`build-binaries.yml`** matrix simplified to `aarch64-apple-darwin`
  only.

### Changed — Repository Hygiene

- **Dependabot groups** — `dependabot.yml` now groups all cargo
  patches into a single weekly PR (with separate major-bump group),
  and groups all GitHub Actions updates similarly. Reduces PR
  noise from 3-5/week to 1-2/week.
- **Removed `[patch.crates-io]` from `Cargo.toml`** — workspace
  members are auto-resolved via `members`, and the explicit patches
  blocked `cargo publish`. This is a prerequisite for `publish.yml`.

### Added — Issue/PR Workflow

- **30+ standardized issue labels** — `priority: critical/high/medium/low`,
  `area: ai/agent/tui/sdk/cli/ci/docs/extensions/security`,
  `status: needs-triage/in-progress/review/blocked`,
  `type: regression/performance/refactor/breaking-change`,
  `provider: anthropic/openai/google/other`, plus `good first issue`,
  `help wanted`, `dependencies`, `release`.

### Fixed — release-build regression (immediate hotfix in 0.41.1)

The v0.41.0 release was **broken in release builds**. The TUI's
AskBridge was wired via `bridge.attach()` — a method annotated
`#[cfg(any(test, debug_assertions))]` (test-only convenience) — which
exists in `cargo check` / `cargo clippy` / `cargo test` (debug builds)
but does not compile in `cargo build --release` or `cargo install`.
`cargo install oxicode-cli --version 0.41.0` fails with
`no method named 'attach' found for reference '&Arc<AskBridge>'`.
The fix landed the same day as 0.41.1 and switches the call site to
the production `AskBridge::attach_with_session(session_id)` API with
`app.ownership_session_id()` (which equals `TUI_OWNERSHIP_ID` in
TUI mode per AGENTS.md). Upgrade to 0.41.1.

## [0.40.0] - 2026-06-21
### Fixed — production-readiness audit remediation

A full production-readiness audit found the workspace failing its own
`cargo clippy --workspace --all-targets -- -D warnings` gate (115 errors),
publishing not gated on CI, and several lying stubs. All resolved:

- **Clippy gate restored (115 → 0 errors).** Documented all undocumented
  public items across `oxicode-agent`/`oxicode-tui`/`oxicode-cli`; fixed correctness
  lints: `await`-holding-a-`MutexGuard` (`mcp/mod.rs`), a dead
  `&& false` branch in the `todo` tool, `unwrap()`/`expect()` panic sites
  in `commit`/`todo`/`discovery` (replaced with `?`/poison-recovery),
  `collapsible_if`/`let_and_return`/`needless_borrow`/`len_zero`, and an
  `items-after-test-module` structural issue.
- **Publishing now gated on CI.** `publish.yml` `publish` job depends on a
  new `verify` job (fmt + clippy + nextest on MSRV 1.96) and `package-check`
  (now `cargo package`, no longer `--no-verify --list`). Broken code can no
  longer reach crates.io via a `v*` tag.
- **Extension security hardened (default-deny).** `oxicode_exec` (WASM) and
  native-extension loading (unsandboxed `libloading`) are now **off by
  default**, opt-in via `OXICODE_EXTENSION_EXEC=1` / `OXICODE_NATIVE_EXTENSIONS=1`.
  Extension `permissions` are now parsed and logged (no longer dead code).
  The misleading "zero host access by default" docstring was corrected.
- **`lru` 0.12.5 → 0.18.0** (`oxicode-hashline`), dropping the
  RUSTSEC-2026-0002 unsound advisory and unifying a duplicated dependency.
- **Stubs made honest.** `AgentGroup::run_orchestrated` now returns an
  explicit "not implemented" error instead of silently running only the
  leader; `memory_reflect` now requires a `summary` instead of returning a
  success placeholder for a no-op.
- **Docs brought current.** AGENTS.md stale counts fixed (11→15 port
  traits, 17→~21 tools incl. the `questionnaire`→`ask` rename, 1099→5000+
  models / 30+→70+ providers); `oxicode-store`/`oxicode-fs` references removed from
  CODEOWNERS, CONTRIBUTING, the issue template, PORT_GUIDE, and the
  production-audit README (marked historical); `oxicode-hashline` gained a
  README and LICENSE file.

### Added — selectable Unicode / ASCII / Nerd Font glyph set (default: Unicode)

Every UI symbol (status markers, list cursors, box drawing, spinners, icons)
now comes from a pluggable glyph-set table, so the whole UI can switch
rendering styles from one setting. Based on the omp (oh-my-pi) symbol-preset
design.

- **New `oxicode_tui::symbols` module**: `GlyphSet` enum (`Unicode` / `Ascii` /
  `Nerd`) + `Symbols` table (`Copy`, all `&'static str`). Three preset
  constructors; `GlyphSet::default()` is `Unicode`. Symbol codepoints are
  standards-defined (Unicode box-drawing range + Nerd Fonts PUA).
- **`Theme` / `ThemeStyles` carry the active `Symbols`**: `with_glyph_set` /
  `set_glyph_set`; `to_styles()` propagates them, so every render fn that
  already takes `&ThemeStyles` gets the glyph set with no signature change.
- **Hardcoded glyphs migrated**: `tool_renderer` (✓/✗/⚠/→), the chat
  tool-call status icons (○/●/✓/✗ on every call + result box), list
  highlight cursors (`stateful_list`, `table_list`, `completion`, and 9
  overlays), health/status dots (`routing`, `dashboard`, todo panel, MCP,
  extensions, provider-select), notification toast icons, the chat spinner,
  todo status markers, and horizontal rules now read from the symbol table.
  The `/skill` slash-command listing marker and `📋` todo header also resolve
  through the active glyph set.
- **New `glyph_set` setting** (`settings.toml`, snake_case): `unicode` |
  `ascii` | `nerd`. Settings version bumped 7 → 8 with migration (defaults to
  Unicode). Selectable live in `/settings` → `glyph` (cycles the three
  presets, each shown with a live sample) and **applied immediately** — the
  main loop rebuilds the live theme from freshly-loaded settings on the next
  draw, no restart needed. Also a new step in `oxicode setup` (step 4: Glyph Set)
  with per-preset sample preview.

### Changed — `questionnaire` → `ask` (omp-style sequential selector)

The interactive user-questioning tool was redesigned to match omp's `ask`
design. Full design in `docs/designs/2026-06-21-omp-ask-redesign.md`.

- **Renamed**: tool `questionnaire` → `ask`; types `QuestionnaireTool` →
  `AskTool`, `QuestionnaireBridge` → `AskBridge`, `PendingQuestionnaire` →
  `PendingAsk`, `QuestionnaireResponse` → `AskResponse`,
  `QuestionnaireOverlay` → `AskOverlay`. Settings field
  `questionnaire_timeout_secs` → `ask_timeout_secs` (serde alias kept for
  migration).
- **Sequential one-question-at-a-time flow** (was tabbed modal): one question
  per screen, ←/→ to navigate between questions with `(k/N)` progress.
- **omp-style markers**: radio (`◉`/`○`) for single-choice, checkbox (`☑`/`☐`)
  for multi-select, drawn inline on each option row — not just the transcript
  preview.
- **"Other (type your own)" auto-appended** to every question with
  `allow_other`; selecting it opens an inline text editor.
- **"Done selecting"** row for multi-select; **"(Recommended)"** suffix on the
  recommended option label; **countdown timer** `(Ns)` in the title.
- **Compact mode + fuzzy search** when options exceed 12 (label-only rows +
  highlighted-option description + type-to-search filter).
- **Filled-menu transcript result**: `format_ask_result` reconstructs every
  offered option with its selection marker filled in (success vs dim), plus
  custom free-text and an "auto-selected after timeout" footer — by combining
  the call arguments (full option list) with the result text. The
  `format_tool_result` / cache `format_result` API gained an `arguments`
  parameter for tools that need the original call JSON.
- **Prompt discipline** (omp `ask.md`): "default to action", "do NOT include
  Other — UI adds it", "2-5 concise options", `recommended` marks the default.
- **Ownership identity**: `AskBridge::attach_with_session(session_id)` binds
  the bridge to a non-empty session identity (mirrors the issue-system
  invariant from AGENTS.md pitfall "Issue-system ownership identity (Phase 0 /
  defect #13)"). `AskTool::execute` refuses to run when the identity is empty,
  preventing concurrent agents from impersonating each other's ask overlays.
- **Dead-code cleanup**: removed 5 unused overlay modules alongside the
  rename — `overlay/questionnaire.rs` (985 lines), `overlay/model_select.rs`,
  `overlay/resume_select.rs`, `overlay/logout_select.rs` (all three were
  superseded by the `factories.rs` impls and had zero callers), and the
  legacy `oxicode-agent/src/tools/questionnaire.rs` (681 lines).
- **Layer 2 wrapper deferred**: the design doc
  (`2026-06-21-selector-consolidation-refactor.md`) sketches a generic
  `SelectorOverlay` wrapping `ListSelectorState`. That wrapper was drafted but
  proved to be unused dead code (`factories.rs` and `AskOverlay` both drive
  `ListSelectorState` directly), so it is **not** shipped in this release.
  A future PR may resurrect it if enough call sites benefit.

### Changed — TUI rendering efficiency (omp parity, Phase 0)

Three low-risk improvements to the `DiffBackend` cell writer and tool-block
rendering, closing the bulk of the byte/CPU gap with omp's TUI. Full design in
`docs/designs/2026-06-20-oxicode-tui-improvement-plan.md`.

- **SGR attribute delta (`render::mod`)**: the cell writer no longer emits a
  full `SGR 0` reset plus fg/bg re-apply on every modifier change. Instead it
  emits only the minimal SGR off/on delta (`modifier_delta_codes`), which
  leaves fg/bg untouched — so an intra-row style transition (common in tool
  boxes and markdown) costs a few bytes instead of a reset + two color
  re-emits. Bold/dim share SGR 22 and are re-enabled correctly after a clear.
  First cell of a changed row force-clears attributes by diffing from the
  all-attributes mask (terminal state is unknown after skipped rows).
- **Tool-block formatting cache (`ToolFormatCache`)**: `format_tool_call` /
  `format_tool_result` are now memoized in `ChatViewState`, keyed by an input
  hash and invalidated wholesale on any theme/glyph-set change (via
  `ThemeStyles: PartialEq`). Completed tool blocks skip the JSON parse, diff
  auto-detection, and per-tool dispatch on cache hit (soft cap 512 entries).
- **Boxed section separator**: the rule between a tool call and its result now
  uses sharp-box tees (`├─┤` via new `Symbols::sharp_tee_right` /
  `sharp_tee_left`, with `+` ASCII fallback) so the result reads as a framed
  sub-section instead of a free-floating divider.

`ThemeStyles` now derives `PartialEq` to support cache invalidation.

### Added — terminal capability detection table (omp parity, Phase 1)

`TerminalCapabilities` now identifies the host terminal family from its
environment-variable signature and derives a capability set from a built-in
knowledge table, replacing the shallow ad-hoc env sniff. This is the safe,
fully unit-tested baseline; a live device-attributes probe (DA1/XTGETTCAP) is
a follow-up. Design: `docs/designs/2026-06-20-oxicode-tui-improvement-plan.md`.

- **New capability flags**: `synchronized_output`, `deccara`, `sixel`, and
  `terminal_name`, each with a *safe default* for unknown terminals — sync on
  (harmless if ignored), deccara/sixel off (harmful if unsupported).
- **Recognized terminals**: kitty, ghostty, wezterm, iTerm2, foot, contour,
  alacritty, konsole, blackbox, tabby, Apple Terminal, xterm, tmux, screen.
  Kitty/Ghostty are the only terminals flagged `deccara` (the Kitty rectangular
  SGR extension) — the gate Phase 2's DECCARA bg-fill optimizer will require.
- **`DiffBackend` gates CSI 2026** on `synchronized_output` (constructed via
  `TerminalCapabilities::detect()`; inject with `DiffBackend::with_capabilities`).
- **`OXICODE_NO_SYNC_OUTPUT=1`** manually disables synchronized output for
  terminals that misbehave under it.

### Added — DECCARA background-fill optimizer (omp parity, Phase 2)

For terminals that implement Kitty's DECCARA rectangular-SGR extension (Kitty,
Ghostty), `DiffBackend` now replaces a solid-background row's trailing
space-padding with a single rectangle escape, instead of writing every
background-styled space each frame. Adapted from omp's `deccara.ts`, but
operating on structured ratatui `Cell`s (simpler and more reliable than omp's
ANSI-string parser).

- **New `render::deccara` planner** (pure, unit-tested): `analyze_row` proves a
  row is a full-width trailing background fill; `plan_fills` coalesces adjacent
  rows into rectangles and emits them only when the dropped space bytes beat the
  rectangle + per-row EL-clear + DECSACE-wrapper cost — so the plan never
  exceeds the original byte count.
- **`DiffBackend` integration**: changed rows write only up to the fill cutoff,
  EL-clear the previous frame's glyphs from the fill region (DECCARA repaints
  attributes, not glyphs), and emit the coalesced rectangles inside the
  synchronized-update window.
- **Gated on `caps.deccara`** (Kitty/Ghostty only) with an `OXICODE_NO_DECCARA=1`
  kill switch. No effect on other terminals.

### Added — LaTeX math → Unicode in markdown (omp parity, Phase 3)

Inline `$...$` and display `$$...$$` math in markdown is now rewritten to
Unicode before rendering, so the model's `$\alpha + \beta = x^2$` renders as
`α + β = x²` instead of verbatim LaTeX. Adapted from omp's
`latex-to-unicode.ts`, scoped to the common subset.

- **New `render::latex` module** (pure, unit-tested): Greek letters, common
  operators/relations, and `^`/`_` scripts over digits and the letters that
  have Unicode super/subscript forms. Unmapped commands/scripts fall back to
  the literal source, so output is always readable.
- **Currency-safe**: a bare `$5`/`$10` (no `\`, `^`, or `_`) is left verbatim —
  only bodies that look like math are converted. Escaped `\$` is respected.
- Wired into `md_lines` so both the table and regular markdown paths benefit.

### Added — render-loop watchdog + bracketed-paste marker (omp parity, Phase 3)

Two small safety / UX features ported from omp.

- **Render-loop watchdog**: after every `tui.draw()` the frame interval and draw
  duration are measured. Intervals < 1 ms trigger a `[watchdog] render storm`
  warning; frames taking > 100 ms log a `slow frame` warning, both via
  `tracing::warn`. Helps spot runaway re-renders or blocking event handlers.
- **Bracketed-paste marker**: pastes exceeding 10 lines are compacted into a
  `[paste +N lines]` marker shown in the input box. The full text is stored in
  `AppState::pending_paste` and flushed into the message on submit — the user
  can type around the marker and the paste content is prepended when they send.

### Changed — pure-Rust Mermaid renderer (single-binary, no `mmdc`)

The Mermaid → ASCII renderer in `oxicode_tui::render::mermaid` is now a
self-contained pure-Rust implementation. The previous implementation shelled
out to the `mmdc` (mermaid-cli) Node binary, which violated oxicode's single-
binary / no-runtime-deps design — most users saw diagrams silently fall back
to a plain code block.

- **Supports** the four most common diagram types — `graph` / `flowchart`
  (TD/LR/RL/BT, node shapes, labelled edges), `sequenceDiagram`
  (participants, messages, notes, dashed arrows, self-loops),
  `stateDiagram`[-v2] (start/end markers, transition labels), and
  `classDiagram` (class boxes with attrs/methods, UML relations).
- **Unsupported syntax** (pie, gantt, ER, journey, gitGraph, requirement,
  etc.) returns `None` so callers fall back to displaying the source as a
  fenced code block, unchanged from before.
- **Process-level render cache** preserved (keyed by source + options).
- **`which` crate removed** from `oxicode-tui`'s dependencies (it was only used
  to locate `mmdc`).
- **Public API unchanged**: `render_mermaid_ascii`, `render_ascii_diagram`,
  `clear_mermaid_cache`, `MermaidRenderOptions`, `MermaidColorMode` keep
  their signatures; `markdown.rs`'s ` ```mermaid ` block hook is untouched.

Known v1 limitations: only forward edges between adjacent BFS ranks are
rendered as connectors in flowcharts; CJK / wide-character labels misalign
the char-cell canvas; stylistic directives (`classDef`, `style`,
`linkStyle`, `click`) are ignored; state composite states are flattened.

## [0.39.0] - 2026-06-20
### Added — SDK consumers can now use the todo tool with observable state

`oxicode-sdk` exposes the todo tool (`TodoProvider::default()`) for SDK consumers,
backed by observable state via `RegistrySnapshot`.

- **New `TodoProvider` trait** in `oxicode_sdk::tool_providers` (with default impl
  that wraps the built-in `TodoState` + `InMemoryTodoStore`).
- **`RegistrySnapshot` observable state**: SDK consumers can subscribe to
  `AgentGroupState` registries via `.state()`, getting a diff-driven snapshot
  (`RegistrySnapshot`) that includes `todo_state: Vec<String>` for live TUI
  rendering without polling.
- **Todo tool wiring in `OxicodeBuilder`**: `with_todo_provider()` to register a
  custom impl; default is `TodoProvider::default()` noop-compatible.
- **Dependency**: the todo tool (always available, no `essential` flag), crate
  metadata, and `constants` module are now exported from `oxicode-sdk`.


## [0.38.0] - 2026-06-20
### Changed — HTML export tool 렌더링 재설계 (C1)

`oxicode-cli/src/storage/export.rs` 의 도구 호출 렌더링을 구조화된
세션 데이터 기반으로 전면 재작성했다.

* **데드 코드 제거**: 이모지 prefix (`🔧`/`📤`/`📄`/`📝`/`✏️`/`🔍`) 를
  파싱하는 `render_tool_blocks`, `extract_path_from_line`, `ToolOp` enum,
  5개 fused 렌더러 (`render_bash_tool` 등), `render_markdown_with_options`
  내 이모지 분기, 그리고 이들이 사용하던 ~80줄의 dead CSS 를 삭제했다.
  이 코드들은 실제 프로듀서가 없는 self-fulfilling 코드였다.
* **구조적 렌더링 추가**: `AssistantContentBlock::ToolCall` 을 도구 이름별로
  디스패치하는 `render_tool_call_block`, `AgentMessage::ToolResult` 를
  bare `.tool-result` div 로 렌더하는 `render_tool_result_block` 를 추가.
  `render_entry` 가 어시스턴트 블록을 순서대로 순회하도록 재구조화했다.
* **`include_tool_calls` 의미 재정의**: 이모지 라인 필터링에서 구조적
  엔트리 스킵으로 변경. `false` 시 `ToolCall` 블록과 `ToolResult` 엔트리를
  완전히 제외한다.
* **`find` 도구 라벨 수정**: 검색어(`name`)가 아닌 디렉토리(`path`) 가
  표시되던 문제를 수정했다.
* **`extract_text` 헬퍼**: `ContentValue` → text 추출 로직을 공용 함수로
  통합하여 User/System/ToolResult arm 의 중복을 제거했다.
* `BashExecution` 변형은 생산자가 없으므로 렌더링을 유보했다
  (설계 문서 §3.7 참조).


### Fixed — release/publish 워크플로우 (v0.37.1 릴리스 중 발견)

v0.37.1 릴리스 파이프라인에서 두 가지 자동화 결함이 드러나 수정했다.

* **release.yml `trigger-publish` 잡에 checkout 누락**:
  `gh workflow run publish.yml` 이 로컬 git 체크아웃에서 대상 워크플로우
  파일을 식별하는데, checkout 단계가 없어 `fatal: not a git repository`
  로 실패하던 것을 `actions/checkout@v5` 추가로 수정. 결과적으로
  v0.37.1 은 GitHub Release 생성까지는 성공했지만 publish.yml 이
  자동 dispatch 되지 않아 수동 dispatch 로 이어졌다.
* **publish.yml 멱등성 부재**: 일부 크레이트만 게시된 뒤 일시적 네트워크
  에러(curl HTTP2 framing)로 매트릭스가 중단된 경우, 재실행하면 이미
  게시된 크레이트에서 `already exists` 에러로 실패해 남은 크레이트에
  도달하지 못하던 것을 수정했다. 각 게시 단계가
  `already exists/already uploaded/already been published` 메시지를
  감지하면 성공으로 처리하도록 했다. (이번 0.37.1 의 `oxicode-cli` 가 이
  케이스로 실패했고, 로컬에서 직접 게시해 마무리했다.)

## [0.37.1] - 2026-06-19

### Fixed — CI 워크플로우 (v0.37.0 릴리스 직후 발견)

v0.37.0 릴리스 시도에서 세 가지 CI 인프라 문제가 드러났다.

* **SBOM** (`sbom.yml`): `cargo cyclonedx` 가 각 워크스페이스 멤버마다
  `oxicode.json` 을 생성하는데 (not `target/`), 워크플로우는 단일
  `target/oxicode.cdx.json` 을 기대해 `find: 'target': No such file` 로
  실패하던 것을 수정. `oxicode-cli` (전체 의존성 트리 포함) 을 단일 SBOM 으로
  취합.
* **Sync Labels** (`labels.yml`): 존재하지 않는 `github/issue-labeler@v2`
  태그 + 잘못된 도구 (이슈 분류용) → `.github/labels.yml` 포맷과 정확히
  호환되는 `EndBug/label-sync@v2` 로 교체.
* **crates.io publish 자동화** (`release.yml` + `publish.yml`):
  GITHUB_TOKEN 으로 만든 GitHub Release 는 `release: published` 이벤트를
  다른 워크플로우로 전파하지 않아 `publish.yml` 이 자동 실행되지 않던
  것을, `release.yml` 에 `trigger-publish` 잡을 추가해 Release 생성 직후
  `gh workflow run` 으로 명시적 dispatch 하도록 수정.

### Fixed — published `oxicode-sdk` 0.37.0 가 downstream 에서 컴파일되지 않던 결함 (crates.io 패키징 버그)

crates.io 에 게시된 `oxicode-sdk` 0.37.0 를 의존하는 consumer(oxios 등)가
컴파일할 수 없었던 치명적 패키징 결함을 수정했다.

```
error: couldn't read '.../oxicode-sdk-0.37.0/src/ports/fs/../../../../oxicode-ai/data/catalog/_snapshot.json.gz':
       No such file or directory
  --> oxicode-sdk-0.37.0/src/ports/fs/catalog.rs:247
    include_bytes!("../../../../oxicode-ai/data/catalog/_snapshot.json.gz")
```

- **근본 원인**: `oxicode-sdk/src/ports/fs/catalog.rs::load_snapshot()` 가
  크레이트 바깥(형제 크레이트 `oxicode-ai/data/`)을 가리키는 경로로
  `include_bytes!` 를 썼다. oxicode 워크스페이스 안(in-tree) 에서는 상대경로가
  해석되어 빌드/테스트가 통과하지만, crates.io 배포판은 `oxicode-sdk` 자체
  파일만 포함하므로 게시된 tarball 안에서 해당 파일이 존재하지 않는다
  → consumer 컴파일 실패.
- **왜 게시까지 통과했나**: `publish.yml` 의 게시 단계와 사전 검사 단계가
  **모두** 컴파일 검증을 건너뛰었다 — 사전 검사는
  `cargo package --no-verify --list` (메타데이터/파일 조립만), 게시는
  `cargo publish --no-verify`. `include_bytes!` 경로 문제는 in-tree
  빌드로는 절대 잡히지 않고, 오직 **게시된 tarball 을 registry 의존성에
  대해 컴파일**(`cargo publish` 의 verify 단계)할 때만 드러난다.

**수정**:

- **`oxicode-ai`**: `catalog::snapshot_gzip_bytes() -> &'static [u8]` 신규
  공개 접근자. `oxicode-ai` 자기 트리 안의 자체 포함 `include_bytes!` 로
  snapshot 의 단일 진실 소스가 된다 (`catalog/materialize.rs` +
  `catalog/mod.rs` 재내보내기). 기존 `load_snapshot_catalog()` 도 이 접근자를
  사용하도록 통일.
- **`oxicode-sdk`**: `load_snapshot()` 가 직접 `include_bytes!` 하던 것을
  `oxicode_ai::catalog::snapshot_gzip_bytes()` 호출로 교체. 크레이트 바깥으로
  빠져나가는 경로를 완전히 제거했고, oxicode-sdk 의 자체 `MdCatalog` 스키마로
  파싱하는 기존 동작은 그대로 유지.
- **`publish.yml`**: 게시 단계의 `cargo publish --no-verify` 에서
  `--no-verify` 제거. 이제 각 크레이트가 자기 registry 의존성에 대해
  컴파일 검증된다 (위상 순서 게시 + 의존성 가시성 폴링이 이미
  선행의존성의 신규 버전을 보장). 사전 검사(package-check) 단계는
  메타데이터 검사로 유지하되, 실제 컴파일 게이트는 게시 단계임을
  주석로 명시했다.

**검증**: in-workspace 빌드 + 단위테스트 회귀 없음 (oxicode-ai 553 / oxicode-sdk 314 /
catalog_port 19 전부 통과). `cargo package`(verify) 로 게시 시나리오 재현 —
`oxicode-sdk` 패키지 검증이 `oxicode-ai` 0.37.1 의 registry 가시성을 요구함을
확인(위상 순서 게시로 해결됨).

## [0.37.0] - 2026-06-18

### Added — Catalog Port (12번째 port, models.dev 동적 카탈로그)

SDK 에 **catalog port** (`ModelCatalog` trait) 를 추가하여, 모델/프로바이더
메타데이터를 동적으로 수신할 수 있게 했다. 이는 정적 TOML 카탈로그에서
동적 models.dev 기반 시스템으로의 전환을 완성하며, SDK consumer 가 모델
갱신을 런타임에 수신할 수 있도록 한다.

> **설계 문서**: `docs/designs/2026-06-17-catalog-port-design.md` (v4, §7.9
> sync API 회피 정정 포함). 데이터 흐름은
> `docs/designs/2026-06-17-dynamic-catalog-design.md`.

- **신규 port** `oxicode-sdk/src/ports/catalog.rs`: `ModelCatalog` trait (async
  read + refresh + subscribe) + **sync read API** (`*_sync`, §7.9). noop 기본값.
  `CatalogProtocol` enum (SDK 소유, oxicode-ai `Api` 와 역방향 의존 없음).
  `CatalogModelEntry`, `CatalogProviderEntry`, `CatalogEvent`, `RefreshOutcome`.
- **신규 bridge layer** `oxicode-sdk/src/bridge.rs`: `catalog_entry_to_model()`,
  `provider_base_url()`, modality 변환. SDK 소유 (oxicode-ai 역방향 의존 방지).
- **참조 구현** `oxicode-sdk/src/ports/fs/catalog.rs`: `FileModelCatalog` —
  embedded SNAP + runtime cache + ETag 조건부 GET + user overrides +
  LOCAL `/v1/models` discovery. lazy on-call refresh (백그라운드 작업 없음).
- **`OxicodeBuilder::with_catalog()`** / **`Oxicode::catalog()`** 접근자.
- **`Oxicode::resolve_model()`** catalog fallback 통합 (sync 유지, §7.9).
- **`SdkError` catalog 변종** 3개: `CatalogUnavailable`, `CatalogOverrideParse`,
  `CatalogRefresh`.

**sync read API (§7.9, 구현 중 단순화)**: catalog 데이터가 이미 메모리에
존재하므로, read-only 조회는 I/O 가 아닌 단순 락 획득 + clone 이다. 이로 인해
v3 설계가 명시했던 `ProviderResolver` trait async화 ripple (agent_loop /
multi_provider / fallback_chain 전체) 을 **전면 회피**했다. PR 3 이 "대공사"에서
bridge layer + resolve_model 통합으로 축소되었다.

**oxicode-cli 이관**: composition root (`services::build_oxicode`) 가
`FileModelCatalog::init()` + `with_catalog()` 로 catalog 를 등록한다. TUI
(`AppState.catalog` 필드 주입), setup wizard, `oxicode models` / `oxicode refresh`
명령이 모두 catalog port 기반으로 동작한다. legacy `init_models_dev()` 제거.

**하위 호환성**: legacy free fn (`get_all_models`, `get_provider`, 등) 은
**fallback path 로 유지** (catalog 가 `None` 일 때만). custom provider 동적 등록
(`fetch_models_blocking`/`register_model`) 과 `oxicode-ai/src/catalog/` 모듈은
SNAP 데이터 위치 때문에 제거하지 않고 다음 메이저 버전으로 연기.

**테스트**: catalog port 19개 (sync API 2개, bridge 7개, resolve_model 통합 2개
포함) + 전체 회귀 **2297/2297 통과**.

### Fixed — issue 시스템 소유권 복원 (Phase 0 / 결함 #13)

에이전트가 `issue` 도구의 `start`/`close`를 호출할 때 소유권/liveness
검사가 **조용히 우회**되던 결함을 수정했다. 근본 원인: `agent.rs`가
`AgentLoopConfig.session_id`를 항상 `None`으로 하드코딩하여,
`ToolContext.session_id`가 `None` → 도구 caller id가 빈 문자열(`""`)이
되었고, 빈 문자열은 어떤 `flock` 홀더와도 매칭되지 않아 모든 할당이
즉시 reclaim 가능했다 (두 에이전트가 같은 이슈를 `start` 하면 마지막이
조용히 승리).

- **`oxicode-agent`**: `AgentConfig.session_id: Option<String>` 신규 필드
  (additive, `#[serde(default)]`, `with_session_id` 빌더). `agent.rs`의
  두 `AgentLoopConfig` 생성지점이 이제 config에서 `session_id`를 주입.
- **`oxicode-cli`**: `bootstrap.rs::build_app`가 run-mode별 ownership identity
  생성 — TUI 모드는 `liveness::TUI_OWNERSHIP_ID`("tui"), 그 외는
  `proc-<pid>-<uuid>`. `App::from_oxicode(..., ownership_session_id)`가 프로세스
  수명 동안 `flock`을 잡고 `AgentConfig.session_id`에 동일 id 주입.
  `liveness::TUI_OWNERSHIP_ID` 상수가 단일 진실 소스 — 에이전트 도구·
  TUI 패널·`/issue` 슬래시 명령이 모두 같은 flock 홀더를 본다.
- **`tui/app.rs`**: `run_tui_interactive_impl`의 중복 flock 획득 제거
  (`App`가 이제 보유). `debug_assert!`로 identity 일치 검증.
- **회귀 테스트**: `session_id_wiring_tests`(oxicode-agent,
  `build_tool_context` 레벨), `start_with_distinct_live_owners_collides` +
  `empty_session_assignment_is_immediately_reclaimable_documentation`(oxicode-cli).

> 설계 문서: `docs/designs/2026-06-17-issue-system-hardening.md` (P0–P4 전부).

### Fixed — `atomic_write` temp 이름 UUID 접미 (Phase 1 / 결함 #1)

`store::issues`·`store::session`의 `atomic_write`가 temp 파일을
`tmp.<pid>`로 명명했다. PID-namespace 재활용(컨테이너)이나 fork+exec에서
두 프로세스가 같은 PID로 같은 temp 를 덮어 한 쪽 write 가 손실될 수 있었다.

- **신규** `oxicode-cli/src/store/fs_util.rs`: `atomic_write`/`atomic_write_bytes`.
  temp 이름을 `<base>.tmp.<pid>.<uuid-simple>`로 변경 (PID는 디버깅용,
  UUID 가 유일성 보장). rename 실패 시 best-effort orphan 제거.
- issues/session 양쪽 로컬 복사본 제거 → 공유 헬퍼로 마이그레이션.
- 테스트: 16스레드 동시 동일경로 쓰기, temp 이름 형식, rename 실패 시 orphan 미누출.

### Changed — CAS 재시도 + `IssuePatch` + `reopen` + no-op 감지 (Phase 2 / #2 #3 #4 #9 #12)

저장소는 **엄격**을 유지(원시 `Conflict` 반환, 재시도 없음)하고, **도구만**
회복을 담당한다. 소유권 정책(assignee 한정)은 **유지**.

- **`store::update`**: no-op 감지(#12) — 직렬화된 before/after 를 `updated_at` 를
  정규화해 비교; 의미 변화가 없으면 쓰기/타임스탬프/cache invalidate 스킵.
- **`IssuePatch`**(#3): 모든 필드 `Option` (None=유지, Some=교체). `labels`만
  의미적 공란 — `Some([])`=전체 삭제, `None`=유지. 도구 스키마로 표현 못 하던
  absent vs `[]` 구분 해소.
- **`apply_patch`**: 정밀 patch 를 엄격 CAS 로 적용, 소유권 **강제**(다른
  assignee → `NotAssigned`). `status=Open` 시 `closed_at` 도 클리어(#4 잠재 버그 수정).
- **`reopen`** 액션(#4): `status=Open` + `closed_at=None`.
- **도구 `cas_retry`**(#2): bounded CAS 회복(4회). 첫 시도는 에이전트 hash(빠른
  경로), conflict 시 fresh hash 재독 후 재시도 — stale hash 가 advisory 로 작동.
  모든 변형 액션이 이를 경유. `update` 는 `apply_patch`+`cas_retry` 조합.
- 테스트: stale hash 회복·바운드 후 포기; reopen/apply_patch 의 closed_at 클리어;
  no-op 미갱신; labels keep/clear/replace; 소유권 강제.

### Changed — 스키마 정밀화 + 크기 상한 + github readOnly (Phase 3 / #5 #6 #7)

- **`validate_size`**(#5): `create`/`update` 의 과대 페이로드 조기 거부 —
  title≤512자, body≤256KiB, labels≤32, 라벨당≤64자. 디스크 채우기 방지.
- **스키마 description**(#6, #7): `status` 이중의미 정리(list 필터 vs update 값,
  close/reopen 권장), `labels` REPLACES 시맨틱, `content_hash` ADVISORY 표기,
  `github` `readOnly` 속성 추가(Phase 6 동기화 전용). 도구 최상위 설명에
  update 필드 관례·reopen 워크플로우·자동재조정 정책 명시.
- 테스트: 소형 통과·비텍스트 액션 스킵·body/title/라벨수/긴라벨 거부.

### Changed — orphan 수거 + `top_free_priority` + flock 헬퍼 (Phase 4 / #8 #10 #11)

- **`liveness::reap_orphans`**(#8): `.alive/` dead 파일 수거(best-effort,
  멱등, TOCTOU 안전). (1) 홀더 체크(is_session_alive)로 live 락 미건드림,
  (2) `ORPHAN_AGE_SECS`(1h) age gate 로 최근 파일 보존. `FileIssueStore::open` 에서
  lazy 호출(실패는 warn 만, 시작 차단 없음).
- **flock 헬퍼**(#11): 산재하던 2곳의 `unsafe libc::flock` 호출을
  `try_flock_exclusive`/`probe_flock_shared` 명명 함수로 중앙화(SAFETY 주석).
- **`top_free_priority`**(#10): open + 미할당 이슈 중 최대 우선순위 —
  "지금 당장 손댈 것" 신호. Cache 필드로 계산, 접근자 노출.
- 테스트: reap 멱등/최근 dead 보존(age gate)/오래된 dead 제거+live 보존;
  top_free_priority 가 할당·닫힌 이슈 무시.

### Added — `oxicode issue` CLI `reopen`·`reap` 서브커맨드

설계 §11 권장 항목. store와 에이전트 도구에 이미 노출된 `reopen`/
`reap_orphans`의 CLI 래퍼.

- `oxicode issue reopen <id> [--hash]`: 닫힌 이슈 재개(status=Open, closed_at 클리어).
  close 와 달리 소유권 락 불필요(닫힌 후엔 owner 없음).
- `oxicode issue reap`: `.oxicode/issues/.alive/` dead 파일 수거(age-gated, 멱등) 후
  제거 개수 출력. 주의: store 생성자가 자체 lazy reap 을 돌리므로, 이 명령은
  store 를 열지 않고 디렉토리를 직접 reap 해 **정확한 카운트**를 보고한다.
  (없으면 double-reap 으로 0이 된다.)

### Fixed — `oxicode issue close` CAS 경쟁 (기존 버그)

`close` 핸들러가 `start`→`close` 호출에 **같은 content_hash** 를 재사용했다.
`start` 가 assignment 를 쓰면서 파일(와 해시)을 바꾸기 때문에, 이어지는
`close` 가 거의 항상 `Conflict`("was modified since last read")로 실패했다
(미할당 이슈를 close 할 때 특히). `start` 직후 파일을 다시 읽어 fresh 해시를
`close` 에 넘기도록 수정.

### Fixed — Production readiness (crates.io publish)

main 의 두 CI 잡이 실패 상태였고, 그대로는 crates.io publish 가 막히는
상황이었다.

* **doc** (ci.yml, `RUSTDOCFLAGS=-D warnings`): oxicode-ai 와 oxicode-sdk 의
  깨진 intra-doc 링크 6곳, oxicode-cli 의 모호한 링크 1곳 수정.
* **test-doc** (test.yml, `cargo test --doc`): 컴파일조차 안 되는 doctest
  2건 수정 — `build_oxicode_engine` 이 `async` 가 된 뒤 `.await` 가 누락된 것,
  `OxicodeBuilder::with_catalog` 예시의 빈 `/* ... */;` RHS.

### Changed — clippy `--all-targets` 게이트 강화

`cargo clippy --workspace --all-targets -- -D warnings` 가 **완전히 clean**
하도록 정리했다 (기존 ~1448 warning, 전부 test/bench/example 코드).
shipped 라이브러리는 엄격함을 유지하고, test 코드는 정확히 두 가지
test-idiom lint (`clippy::unwrap_used`, `clippy::field_reassign_with_default`)
만 `#![cfg_attr(test, allow(...))]` 로 완화했다. 나머지 모든 lint
(correctness/suspicious/style/complexity) 는 test 코드에서도 그 자리에서
수정했다.

* ci.yml 의 `clippy` 잡과 `.pre-commit-config.yaml` 의 hook 을 `--workspace`
  에서 `--workspace --all-targets` 로 강화 (AGENTS.md 의 "Pre-existing TODO"
  후속 작업 완료).
* oxicode-ai 의 `benches/` exclude 를 제거해 패키지된 크레이트가 벤치마크 소스를
  포함하도록 수정 (`cargo package` 경고 제거).

## [0.36.0]

### Added — models.dev 라이브 보강 (catalog Layer 2.5)

opencode가 사용하는 동일 진실 소스인 **models.dev** (MIT)에서
`https://models.dev/api.json` 을 런타임에 페치하여 카탈로그를 보강한다.
이는 oxicode-original TOML의 광범위한 `0.0` 가격 결손을 해소한다
(anthropic/openai/azure 등 유료 모델의 cost_input/cost_output이
대부분 `0.0`으로, 비용 리포트가 부정확했다).

- **신규 모듈** `oxicode-ai/src/catalog/models_dev.rs`: models.dev 스키마
  파서, provider ID 매핑(oxicode 지역 변형 collapse), reasoning 보존
  allowlist(TEE/tput/compound/FP8 변형), enrich 로직, fetch/캐시
  (5분 TTL, atomic temp→rename, 교차프로세스 Flock, 2회 재시도).
- **단일 진입점**: `model_db::all_provider_models()`의 OnceLock 클로저에
  enrich 3줄 삽입 — 모든 소비자(`get_model_entry`/
  `model_from_entry`/`fallback_chain`/TUI 슬래시)가 자동 보강.
  부트스트랩(`bootstrap.rs::build_app`)에서 `init_models_dev().await` 호출.
- **우선순위**: Layer 2 override > models.dev > Layer 1. 양수 가격/
  양수 limit만 덮어쓰며, verified-free/unknown은 보존. openclaw
  `-1.0` 센티넬은 models.dev 양수 도착 시 자동 정상화.
- **오프라인 안전**: init 미실행/페치 실패 시 `get()`=None →
  Layer 1로 graceful fallback. 기능은 항상 동작, 비용 정확도만 저하.
- **게이트**: `OXICODE_MODELS_DEV`(`on`/`auto`/`off`, 기본 `auto`),
  `OXICODE_MODELS_DEV_URL`, `OXICODE_MODELS_DEV_DISABLE_FETCH`(에어갑),
  `OXICODE_MODELS_DEV_TTL`, `OXICODE_MODELS_DEV_CACHE_PATH`.
- **테스트**: 단위 10개(스키마/enrich/매핑/allowlist) + end-to-end
  통합 1개(캐시 fixture → init → model_db 조회 검증).
- **문서**: `docs/MODELS_DEV_SYNC.md`(설계 청사진),
  `data/catalog/README.md`(Upstream sync / Price data quality 표 정정),
  `AGENTS.md`(catalog 4-tier 설명, 환경변수, Pitfalls 갱신).

### Added — TUI 출력 언어 정책 (TUI-only, per-channel)

`Settings::output_languages`를 신설하여 **TUI 세션**에서 출력
채널별 언어 정책을 구성할 수 있게 했다. `oxicode --print` 및 RPC
모드는 정책이 있어도 **조용히 무시**된다 (의도적 격리 — TUI
하네스 전용).

- **데이터 모델** (`oxicode-cli/src/store/settings.rs`):
  `output_languages: HashMap<String, String>` — 채널 키
  (`response`, `code_comment`, `documentation`,
  `commit_message`)에 ISO 639-1 코드(`en`, `ko`, `ja`, …) 또는
  `"auto"`를 매핑. 기본값 = 전 채널 `auto` (현재 동작 100% 보존).
  `settings.toml` v4→5 마이그레이션은 값 변환 없이 버전만 올림.
- **확장형 맵**: 핵심 4채널 외에 사용자가 임의 키를 추가할 수
  있다 (예: `pr_description = "en"`). `KNOWN_CHANNELS`는 이제
  prompt label 매핑 테이블로만 사용되며, 알 수 없는 채널은 raw
  키를 label fallback으로 directive에 포함된다.
- **3레이어 전파** (`oxicode-cli/src/app/agent_session_runtime.rs`,
  `oxicode-cli/src/prompt/system_prompt.rs`):
  1. 시스템 프롬프트의 **마지막 섹션**에 "Output Language
     Policy (enforced)"로 부착 — 모델이 가장 강하게 attend하는
     위치.
  2. `compaction_instruction`에도 같은 directive를 흘려 요약
     누출 차단. 단, summarizer는 이를 `"Focus areas: …"`로
     wrap하므로 강도가 약해짐 (문서화됨, 별도 cross-crate 변경
     필요).
  3. 서브에이전트는 부모 `system_prompt`를 `--append-system-prompt`
     플래그로 자식에게 전달하고 자식은 `set_system_prompt()`로
     통째 replace하므로, **부모 directive가 자식에게 자연
     전파**된다 (추가 코드 불필요).
- **TUI UX** (`oxicode-cli/src/tui/overlay/settings.rs`):
  `/settings` 오버레이에 "Language (TUI)" 섹션 추가. 채널당
  Choice로 `auto → en → ko → ja → zh → es → fr → de → auto` 사이클.
  Esc로 디스크 persist + `OXI`-mode 알림.
- **Hot-apply** (`oxicode-cli/src/app/agent_session.rs`,
  `oxicode-cli/src/tui/slash.rs`): `AgentSession::rebuild_system_prompt()`
  신규, `/reload` 슬래시 명령에서 `set_thinking_level`과 함께
  호출. 변경 후 `/reload` 한 번이면 다음 턴부터 적용.
- **검증**: 알 수 없는 언어 코드는 `tracing::warn!` 후 유지
  (사용자가 새 언어 추가 가능). 알 수 없는 채널 키는 그대로
  통과 (확장형). 화이트리스트 검증 없음.
- **테스트 8개 신규** (settings 4, system_prompt 4,
  agent_session_runtime 4) — 핵심 invariant를 단위 테스트로
  잠금.
- **Strong default, NOT a hard guarantee.** 코드 docstring 4곳
  + AGENTS.md Pitfall에 한계 명시. 100% 보장이 필요하면 도구
  출력 wrapping 또는 응답 후처리가 필요 (현재 MVP 범위 외).

### 사용 예시 (`~/.oxicode/settings.toml`)

```toml
[output_languages]
response = "ko"
code_comment = "en"
documentation = "en"
commit_message = "en"
```

## [0.35.0] - 2026-06-15

### Fixed — native-browser 부활: edition 2024 lifetime 버그 전면 수정

`--features native-browser`로 컴파일하면 27~28개의 컴파일 에러가
발생하던 치명적 버그 수정. 근본 원인은 `BrowserTab`/
`BrowserEngine` trait가 수동 `Pin<Box<dyn Future + 'a>>` 패턴을
사용했는데, edition 2024의 정밀한 lifetime 캡처 규칙에 위배되었기
때문. 이 버그는 oxicode CI가 `native-browser` feature를 단 한 번도
컴파일하지 않아 0.32.0~0.34.0까지 배포된 채 방치되었음.

- **`BrowserTab`/`BrowserEngine` trait를 `#[async_trait]`로 전환**
  (oxicode-agent): 30개 메서드 시그니처를 `async fn`으로 단순화.
  `async-trait = "0.1"`은 이미 의존성이었고 sibling `AgentTool`
  trait 4개가 같은 패턴을 사용 중이었으므로 일관성 확보.
  `Pin<Box<...>>` 보일러플레이트 약 480줄 제거.
  `dyn BrowserTab`/`dyn BrowserEngine` object-safety 유지.
- **oxibrowser_backend.rs impl을 async_trait 기반으로 재작성**
  (oxicode-agent): 27개 메서드 전부 `async fn`으로 변환.
  `tab_id(&'a self)`, `evaluate_await`의 선언되지 않은 lifetime
  버그(E0261), `new_tab`의 Box coercion 실패(E0271) 동시 해결.
- **Mock 구현체 3종 동기화** (oxicode-agent): `tab_guard.rs::MockTab`,
  `browse_tool.rs::MockEngine`, `browse_session_tool.rs::MockTab`/
  `MockEngine` 전부 async_trait 기반으로 변환.

### Changed

- **`oxibrowser-core` 0.14.1 → 0.15 정렬** (oxicode-sdk): oxicode-agent은
  이미 0.15를 사용 중이었으나 oxicode-sdk의 re-export만 0.14.1에
  고정되어 있던 버전 불일치 해결.

### CI — 재부팅 영구 차단

- **`clippy-native-browser` job 추가** (ci.yml): 매 PR마다
  `cargo clippy -p oxicode-sdk --features native-browser -- -D warnings`
  + `cargo build -p oxicode-agent --features native-browser` 실행.
  native-browser 코드 경로가 다시 부서지는 것을 영구 차단.
- AGENTS.md에 native-browser 컴파일 의무화 명시.

## [0.34.0] - 2026-06-15

### Added — MCP 서버 관리 TUI + 표준 config 호환성

`/mcp` 명령이 읽기전용 대시보드에서 **인터랙티브 관리 오버레이**로
승격. 서버 추가/편집/삭제를 TUI에서 직접 하고 디스크에 저장하면 런타임
`McpManager`에 핫 리로드. pi-mcp-adapter의 `/mcp` 패널 UX를 참고.

- **`McpConfigOverlay`** (oxicode-cli): `/mcp`로 열리는 관리 오버레이.
  - **List 모드**: 라이브 연결 상태 표시(●/○/✗), scope 표시,
    unsaved 배지.
  - **Edit 모드**: 폼 편집기 — name, transport(stdio/http 토글),
    command+args 또는 url, lifecycle, idle timeout, direct-tools,
    env/headers.
  - **Confirm-remove 모드**: 삭제 가드.
  - **Discard-guard 패턴**: dirty 상태에서 첫 Esc/Tab은 경고만,
    두 번째가 실제 닫기/전환 — 조용한 데이터 손실 방지.
  - **명시적 Transport 토글**: 자동감지 대신 선택 필드로 URL 필드에
    접근 가능. 토글 시 관련 첫 입력 필드로 포커스 이동.
- **`/mcp` 자동완성**: `BUILTIN_SLASH_COMMANDS`에 추가되어 슬래시
  자동완성 목록에 표시.
- **Config 저장 헬퍼** (oxicode-agent): `save_mcp_config()`(temp+rename
  atomic write), `load_or_default()`, `default_write_path_global/project()`.

### Changed

- **`/mcp` 라우팅 분리**: `/mcp` → 관리 오버레이, `/mcp dashboard` →
  기존 읽기전용 상태 대시보드, `/mcp status` → 상태 알림(변경 없음).
- **표준 MCP config 포맷 호환** (oxicode-agent): serde가 canonical camelCase
  (`mcpServers`, `idleTimeout`, `directTools`, `toolPrefix`, …)로 직렬화.
  역직렬화는 camelCase와 legacy snake_case 모두 허용(`serde alias`)하여
  기존 파일이 그대로 동작.
- **`McpManager::replace_config()`** (oxicode-agent): 런타임 config 핫 교체.
  새로 추가된 서버가 재시작 없이 proxy tool에 도달 가능.
  (direct-tool 등록은 여전히 부팅 시 1회만 수행하므로 재시작 필요.)
- **`OverlayAction::McpConfigApplied`** (oxicode-cli): 오버레이가 디스크에
  쓴 merged config를 라이브 매니저에 반영하고 성공 알림.

### Fixed

- **"Save & Apply" 버튼 미작동**: Edit 모드 Save가 메모리만 고치고
  디스크 저장/live 적용을 안 하던 버그 수정 — `commit_and_save()`로
  stage + write + apply를 한 번에 수행.
- **scope 전환(Tab) 시 unsaved 변경사항 조용히 손실**: discard-guard로
  두 번 누르기 패턴 적용.
- **Esc로 닫을 때 unsaved 손실**: 동일한 discard-guard 패턴 적용.

## [0.33.0] - 2026-06-13

### Added — MCP 고도화 (Phase 1-3 + SDK + TUI)

pi-mcp-adapter 아키텍처 기반으로 MCP 기능 대폭 확장.

- **Disk-backed metadata cache** (`~/.oxicode/mcp-cache.json`): 서버 연결 없이도
  `search`/`list`/`describe` 동작. 원본 툴 이름만 저장하여 `tool_prefix`
  설정 변경에도 무효화 불필요.
- **Channel-based lifecycle manager**: `mpsc` 채널로 idle disconnect 타이머와
  keep-alive health check를 `McpManagerInner` 뮤텍스 밖에서 실행 → 데드락 방지.
- **`McpTransport` trait**: stdio 전송을 추상화. 향후 HTTP/SSE 추가 용이.
- **`McpManager::spawn()`**: `Arc::new_cyclic`으로 lifecycle 태스크에
  `Weak<McpManager>` 전달. `Eager`/`KeepAlive` 서버는 백그라운드 자동 연결.
- **`McpDirectTool`**: 개별 MCP 툴을 `AgentTool`로 직접 등록.
  `directTools`/`excludeTools` 설정으로 제어. Consent system과 연동.
- **`ConsentManager`**: 툴 실행 전 Allow/Deny 사전 승인.
  `~/.oxicode/mcp-consent.json`에 저장.
- **Generic `DashboardWidget`** (oxicode-tui): MCP 독립적인 제네릭 대시보드.
  섹션/아이템/필터/뱃지 지원.
- **`McpDashboardOverlay`** (oxicode-cli): `/mcp` 슬래시 명령으로 열리는
  인터랙티브 MCP 관리 대시보드. 서버 연결/해제, consent 관리, 필터 지원.
- **SDK 레이어**: `OxicodeBuilder::with_mcp_config()`, `Oxicode::mcp()`,
  `mcp_tools()` factory. oxicode-sdk re-export로 SDK 컨슈머(oxios 등)가
  MCP를 직접 사용 가능.
- **MCP 디스크 경로 커스터마이징** (SDK 컨슈머용): `McpManager::spawn_with_paths(config, cache, consent)`와
  `OxicodeBuilder::with_mcp_paths(cache, consent)` 추가. SDK 컨슈머(oxios 등)가
  자체 디렉토리(`~/.oxios/`) 아래에 MCP 캐시/consent 상태를 self-host할 수 있도록
  additive API. `oxicode_sdk::MetadataCache` 재내보내기 포함. 기존 `spawn()`/
  `spawn_with_config()`는 `spawn_with_paths`의 thin wrapper가 됨 (관측 동작 불변).
  (참고: `docs/proposals/mcp-disk-path-customization.md`)

### Changed

- `McpManager::new()` → `Arc<Self>` 반환 (내부적으로 `spawn()` 호출).
  `ToolRegistry::with_builtins_cwd()`에서 `Arc` 한 겹 제거.
- `McpClient`가 `Box<dyn McpTransport>` 기반으로 리팩터링.
- `ToolRegistry`에 `mcp_manager` 필드 및 `set_mcp_manager()`/`mcp_manager()` getter 추가.
- `ServerEntry`, `McpSettings`에 `#[serde(default)]` 및 `Default` 추가.
- `ServerEntry`에 `direct_tools`, `exclude_tools` 필드 추가.
- `McpSettings`에 `direct_tools`, `disable_proxy_tool` 필드 추가.

### Fixed

- `McpManager::spawn()` / `spawn_with_paths()`가 Tokio runtime 밖에서
  호출되면 panic하던 회귀 수정 — `OxicodeBuilder::build()`를 runtime 없이 부르는
  단위 테스트(oxicode-sdk 6개)가 `tokio::spawn` panic으로 실패했다. runtime
  가드(`Handle::try_current()`)를 추가해 runtime이 없으면 lifecycle/eager
  task를 생략 (`new_no_spawn()` 패턴 차용).
- `OxicodeBuilder::build()`에서 MCP paths-only 분기가 빈 `McpConfig`를 사용하던
  풋건 수정 — 이제 `with_mcp_config` 없이 `with_mcp_paths`만 호출해도
  표준 경로에서 config를 자동 발견한다.
- `McpClient`/`McpPrompt`/`McpLogLevel`/`McpSamplingRequest` 등 공개 API의
  missing-doc 누락 보충 및 clippy(clapsed-if, derive, map) 경고 해소.

## [0.32.0] - 2026-06-12

### Changed — RFC-008: Remove `max_iterations` loop guard

The agent loop no longer enforces a turn limit. This matches pi-agent's
behavior where the loop runs until the LLM naturally stops making tool calls.

- **`should_stop_after_turn()`** now only checks `external_stop` (Ctrl+C).
  The `max_iterations`, `turn_number`, `messages`, and `assistant_message`
  parameters were removed — the function signature is now
  `fn should_stop_after_turn(external_stop: &Arc<AtomicBool>) -> bool`.
- **`AgentConfig::max_iterations`** field removed. Existing code that sets
  this field will get a compile error — remove the field from struct literals.
- **`AgentLoopConfig::max_iterations`** field removed.
- **`AgentConfig::with_max_iterations()`** builder method removed.
- **`AgentEvent::ForcedSummary`** variant removed (was added during RFC-008
  development but is no longer needed without the max-iterations guard).
- **`LoopStopReason`** enum removed.

### Removed

- `max_iterations` field from `AgentConfig` and `AgentLoopConfig`.
- `with_max_iterations()` builder from `AgentConfig`.
- `LoopStopReason` enum from `agent_loop::helpers`.
- `ForcedSummary` variant from `AgentEvent`.

### Migration

Remove all `max_iterations` fields from `AgentConfig` and `AgentLoopConfig`
struct literals. The loop now runs indefinitely until the LLM produces a
text-only response (no tool calls) or the user cancels (Ctrl+C).

## [0.31.6] - 2026-06-12

### Fixed — Session persistence bug

- **`AgentMessage::User` and `AgentMessage::System` failed to serialize** due to
  `#[serde(flatten)]` on a `ContentValue` field. `ContentValue::String` serializes
  as a bare JSON string, but `flatten` can only merge structs/maps — causing
  `serde_json::to_string` to fail silently. User messages were never written to
  disk, making sessions invisible to `/resume`. Removed `#[serde(flatten)]` from
  both variants (`oxicode-cli/src/store/session.rs`).
- **Silent serialization failures in `_persist()`** now emit `tracing::warn!`
  instead of being silently swallowed.
- Added regression tests: `test_session_roundtrip_preserves_user_content`,
  `test_session_list_finds_sessions_with_user_messages`.

## [0.31.0] - 2026-06-07

### Changed — Rust 2024 edition modernization

- **`async-trait` crate removed**: All 104 `#[async_trait]` annotations across
  59 files replaced with native `async fn` in trait (stable since Rust 1.75).
  Trait methods now return `Pin<Box<dyn Future + Send>>` explicitly, eliminating
  macro expansion overhead and improving debuggability.
- **`once_cell::sync::Lazy` → `std::sync::LazyLock`**: All 4 uses in `oxicode-ai`
  replaced with the standard library equivalent (stable since Rust 1.80).
- **Rust 2024 let chains**: 16 nested `if let` patterns flattened to
  `if let A && let B` syntax across the workspace.
- **oxibrowser upgraded** from 0.14.1 to **0.15.0** (edition 2024 update).

### Removed dependencies

- `async-trait` — from all 4 crates (oxicode-ai, oxicode-agent, oxicode-sdk, oxicode-cli)
- `once_cell` — from oxicode-ai (replaced by `std::sync::LazyLock`)
- `lazy_static` — from oxicode-cli (unused)
- `tokio-test` — from oxicode-ai, oxicode-agent (unused)

## [0.30.0] - 2026-06-06

### Changed — oxicode-agent

- **Replace `a3s-search` with `oxibrowser` search module**: Web search (`web_search` tool) now uses `oxibrowser::search::dispatch()` instead of the `a3s-search` crate. This consolidates search functionality into the oxibrowser ecosystem and removes the `a3s-search` dependency.
- **Remove Brave engine**: The `brave` engine option is no longer available. Supported engines: `ddg`, `wiki`, `bing`.
- **`SearchResult` type migration**: `search_cache::SearchResult` replaced by `oxibrowser::SearchResult` (fields `engines`/`score` → `source`/`extra`).

### Removed

- `a3s-search` dependency from `oxicode-agent`.
- `RUSTSEC-2025-0057` (fxhash) advisory exception — no longer a transitive dependency.

### Changed — oxicode-sdk

- `oxibrowser-core` dependency updated to `0.14.1`.

## [0.29.1] - 2026-06-06

### Added — oxicode-agent

- **`ScreenshotMeta` struct**: Screenshot metadata (bytes, width, duration_ms) attached to `ToolCallContext::PageVisit`.
- **`PageVisit.navigation_error`**: Navigation error message from `BrowseProgress::NavigationFailed`.
- **`PageVisit.screenshot`**: Screenshot metadata from `BrowseProgress::ScreenshotCaptured`.
- **Enrichment match arms**: `make_browse_enrichment_cb` now handles `NavigationFailed` and `ScreenshotCaptured` events (previously only `DocumentReady` was processed).
- **Unit tests**: `browse_enrichment_callback_fills_navigation_error`, `browse_enrichment_callback_fills_screenshot`, `browse_enrichment_callback_navigation_failed_ignores_non_page_visit`.

### Fixed — oxicode-cli

- **Clippy `large_enum_variant`**: `SessionEvent::Agent` variant boxed to reduce enum size from 264 bytes.

## [0.29.0] - 2026-06-06

### Added — oxicode-agent

- **`ToolCallContext` enum**: Semantic context for tool calls (`WebSearch`, `PageVisit`, `DataExtraction`, `SessionAction`, `ScriptStep`). The agent loop infers context from tool name + args via `infer_context()`; tools remain unaware of semantics.
- **`BrowseProgress` enum**: Structured progress events from browser tab lifecycle (`NavigationStarted`, `WaitingForSelector`, `DocumentReady`, `ScreenshotCaptured`, `NavigationFailed`). Converted from `oxibrowser_core::BrowserEvent` in the backend drain task.
- **`VisitReason` enum**: `DirectNavigation`, `SearchResult { position }`, `LinkFollow` — distinguishes *why* a page was visited.
- **`BrowseCallbacks` mixin** (`callback_mixin.rs`): Eliminates duplicated pending-callback boilerplate across 4 browse tools. Provides `store_progress()`, `store_browse()`, `register_on_registry()`, `register_on_tab()`.
- **`TabCallbacks` composite** in `TabCallbackRegistry`: Single `HashMap<Uuid, TabCallbacks>` replaces the dual-map pattern. One `clear()` removes both string and browse callbacks atomically — no key-set divergence possible.
- **`make_browse_enrichment_cb()`**: Shared closure factory that enriches `ToolCallContext::PageVisit` and `DataExtraction` with `DocumentReady` data (title, status, bytes, duration).
- **`enrich_context_from_metadata()`**: Post-execute enrichment that fills `DataExtraction.result_count` from `AgentToolResult.metadata`.
- **Parallel tool execution parity**: `execute_prepared_tool_call_static` (parallel path) now has full context_cell, tab_id_slot, progress callback, and browse callback wiring — identical observability to the sequential path.
- **`browse_session "goto" → PageVisit`**: Semantic upgrade — `goto` action now produces `PageVisit { reason: DirectNavigation }` instead of generic `SessionAction`.
- **`browse_script → ScriptStep`**: `infer_context` parses step count from YAML or JSON args, producing `ScriptStep { current: 0, total: N, step: "starting" }`.
- **`browse_extract result_count`**: Extraction results include `result_count` in metadata; context enrichment populates `DataExtraction.result_count` after execute.
- **Integration tests**: `engine_forwards_browse_progress_to_callback`, `engine_routes_browse_progress_by_tab_id` — end-to-end browse progress verification with real browser.
- **Unit tests**: `browse_progress_serde_roundtrip`, `browse_enrichment_callback_*`, `infer_context_browse_script_*` — 18 new tests total.
- **`AgentTool::on_browse_progress`**: Default trait method for structured browse progress callbacks.
- **`BrowserTab::set_browse_progress_callback`**: Default trait method; only backends with browse callback support override.

### Changed — oxicode-agent

- **`TabCallbackRegistry` restructured**: Dual `callbacks` + `browse_callbacks` maps → single `entries: HashMap<Uuid, TabCallbacks>` with composite `TabCallbacks { progress, browse }`. `clear()` is now atomic for both callback types.
- **`BrowserTab::clear_browse_progress_callback` removed**: `TabCallbacks` clearing handles both; no separate method needed.
- **4 browse tools refactored**: `pending_callback` + `pending_browse_callback` fields replaced with single `callbacks: BrowseCallbacks` field. ~80 lines of duplicated boilerplate eliminated.
- **`BrowseScriptTool` YAML parser rewritten**: `parse_steps` now handles the `{ steps: [...] }` map format correctly, with per-step variant dispatch and shorthand support (`- goto: "url"` for single-field struct variants, `screenshot: {}` for unit variants). Fixes 10 previously-failing tests.
- **`browse_progress_from_event`**: `NavigationFailed` match arm gated behind `oxibrowser-core ≥ 0.14` (crates.io 0.13 compatibility).

### Removed — oxicode-agent (Breaking Changes)

- **`ToolProgress` enum**: Unused structured progress type (replaced by `BrowseProgress`).
- **`FileOp` enum**: Unused file operation types (part of `ToolProgress`).
- **`StructuredProgressCallback` type**: Unused callback type (replaced by `BrowseProgressCallback`).
- **`AgentTool::on_structured_progress`**: Unused trait method (replaced by `on_browse_progress`).

### Changed — oxicode-sdk

- Re-exports `BrowseProgress`, `BrowseProgressCallback`, `ToolCallContext`, `VisitReason`.

### Changed — oxicode-cli

- `ToolExecutionStart` and `ToolExecutionUpdate` pattern matches updated with `..` for backward compatibility.

### Changed — workspace

- Bumped all crate versions to 0.29.0.
- Inter-crate dependency versions aligned to 0.29.0.

- Per-`tab_id` `TabCallbackRegistry` replaces the single-slot `ProgressForwarder`.
  Concurrent `BrowseTool` calls (each with their own tab) are now routed correctly.
  Each `BrowseTool::execute` registers its callback on the specific tab; the
  engine's background event-drain task routes events by `tab_id`.
- `AgentTool::set_tab_id_slot` and `AgentTool::current_tab_id` default methods
  on the tool trait, enabling the agent loop to read the active tab ID.
- `BrowserTab::tab_id`, `BrowserTab::as_any`, `BrowserTab::clear_progress_callback`
  default methods on the browser tab trait.
- `BrowseTool::pending_callback` pattern: `on_progress` stores the callback;
  `execute` registers it on the actual tab (tab_id not known until tab creation).
- Integration test `engine_routes_events_by_tab_id_concurrent`: opens two tabs,
  registers per-tab callbacks, and verifies event isolation.

### Changed — oxicode-agent

- `oxibrowser-core` dependency bumped from 0.12 to **0.13**.
- `BrowseTool::execution_mode` remains `SequentialOnly` (per-tab routing makes
  parallel safe, but no concrete multi-tab use case yet).

### Fixed — oxicode-agent

- `AgentEvent::ToolExecutionUpdate.tab_id` is now populated (no longer always `None`).
  The agent loop passes a shared `tab_id_slot` to the tool; `BrowseTool` writes
  the tab ID when it opens a tab, and the progress callback reads it.
- `TabGuard::close` now calls `clear_progress_callback()` to unregister the
  per-tab callback, preventing stale callbacks from accumulating in the registry.

### Fixed — workspace

- Resolved CI gate violations (12 errors total under `cargo clippy --workspace -- -D warnings` and `RUSTFLAGS="-D warnings" cargo build --workspace`):
  - **oxicode-sdk** (3): removed unused `std::sync::Arc` import in `ports/fs/access.rs`; replaced `let _ = tokio::spawn(...)` with `drop(tokio::spawn(...))` in `ports/mod.rs`; collapsed nested `if` in `ports/fs/capability.rs` wildcard prefix resolution.
  - **oxicode-cli** (9): removed unused `clap::Parser` / `std::sync::Arc` imports in `bootstrap.rs` and `setup_wizard.rs`; removed unused `oxicode::extensions::ExtensionRegistry` / `std::path::PathBuf` imports in `main.rs`; silenced `unexpected_cfgs` on the `keyring` placeholder cfg in `store/auth_storage.rs::persist`; deleted dead `run_single_prompt` helper from `bootstrap.rs` (replaced by `crate::main_dispatch::run_single_prompt`); dropped needless `&` on `args` borrow in `register_builtin_tools` call; suppressed unused `Result` from `App::switch_model` call in `lib.rs`; added missing `///` doc comment on `init_logging`; split doc-comment/regular-comment collision before `build_system_prompt` in `lib.rs`.
  - **oxicode-agent** (1): `cargo fmt` trailing blank line in `tools/browse/engine.rs` (auto-fixed by `cargo fmt --all`).

### Changed — workspace

- Bumped all crate versions to 0.27.1 (oxicode-ai, oxicode-cli, oxicode-sdk, oxicode-tui). oxicode-agent was already at 0.27.1. Inter-crate dependency versions aligned to 0.27.1.

### Fixed — oxicode-agent

- `BrowseTool::execution_mode` now returns `SequentialOnly` to prevent the OxicodeBrowserEngine progress forwarder race. (Future work: per-tool_call_id forwarder.)

### Changed — infrastructure

- **CI**: Added `smoke-test` job to `.github/workflows/ci.yml` so PRs run a lightweight test subset
- **CI**: Replaced `cargo install` with `taiki-e/install-action` for `cargo-audit` and `cargo-deny` (saves ~3 min/job)
- **CI**: Added macOS to `test.yml` matrix for cross-platform test coverage
- **CI**: Added `RUSTDOCFLAGS=-D warnings` to `test.yml` so doc-tests fail on warnings
- **Release**: Switched x86_64 macOS runner from `macos-13` (deprecated) to `macos-14` (cross-compiled)
- **Release**: Added tag-on-main verification step to prevent releases from stale branches
- **PR Gate**: Conventional commit title is now enforced (error, not warning); PR size hard cap at 4000 lines
- **PR Gate**: Added merge-commit detection and issue-linkage encouragement
- **Dependabot**: Added `github-actions` ecosystem alongside cargo
- **Cargo**: Removed conflicting `[profile.release]` from `.cargo/config.toml` (workspace `Cargo.toml` is now the single source of truth)
- **Cargo audit/deny**: Synced ignore lists across `.cargo/audit.toml` and `deny.toml`; added upgrade tracker comment for extism ≥ 1.22 (wasmtime ≥ 43)
- **Docs**: Added `CODEOWNERS` for per-area review assignment

[0.39.0]: https://github.com/a7garden/oxicode/compare/v0.38.0...v0.39.0
[0.62.0]: https://github.com/a7garden/oxicode/compare/v0.61.0...v0.62.0
[0.61.0]: https://github.com/a7garden/oxicode/compare/v0.60.0...v0.61.0
[Unreleased]: https://github.com/a7garden/oxicode/compare/v0.62.0...HEAD

## [0.24.0] - 2026-05-30

### Changed — workspace

- Bumped all crate versions to 0.24.0
- Fixed 18 doc warnings across all crates (unresolved links, bare URLs, HTML tags)
- Added `.cargo/audit.toml` with documented vulnerability ignore rationale (wasmtime 41.x via extism)
- Updated README version badge to 0.24.0
- Updated AGENTS.md version to 0.24.0

## [0.25.7] - 2026-05-31

### Changed — oxicode-cli

- **Provider select overlay improvements**: Updated handler logic, factory enhancements, and slash command integration
- Bumped all crate versions to 0.25.7

## [0.25.4] - 2026-05-31

### Added — oxicode-sdk

- `oxicode-sdk/examples/builder_demo.rs` — end-to-end SDK usage example

### Changed — workspace

- Added proper attribution to original [pi](https://github.com/earendil-works/pi) project (MIT License, Copyright © 2025 Mario Zechner)
- Updated LICENSE.md with dual copyright notice (pi + oxicode contributors)
- Added NOTICE.md with detailed attribution of derived architecture
- Updated README.md, AGENTS.md, CONTRIBUTING.md to reflect port provenance
- Root repository cleaned up: removed 75+ analysis/report markdown files and orphaned source files
- All Korean comments and doc strings translated to English across 15 source files
- `.gitignore` expanded with editor, OS, and profiling exclusions
- `rust-toolchain.toml` added to pin toolchain version
- `deny.toml` added for `cargo deny` dependency auditing
- `.editorconfig` added for cross-editor consistency
- `.cargo/config.toml` added for build configuration
- CI pipeline enhanced with `cargo doc`, `cargo test --doc`, and `cargo deny` jobs
- `docs.rs` metadata added to all library crate Cargo.toml files
- Bumped all crate versions to 0.25.4

### Fixed — oxicode-agent

- `truncate.rs` test updated to use emoji-based multi-byte characters

### Fixed — oxicode-tui

- `fuzzy.rs` Unicode match test updated for ASCII pattern
- `chat.rs` CJK wrapping tests updated with English text
- `input.rs` CJK input tests updated with ASCII equivalents
- `text.rs` CJK truncation tests updated with ASCII equivalents

## [0.24.0] - 2026-05-19

### Added — oxicode-sdk

- Re-export `SearchCache`, `CompactionEvent`, `UserMessage` and all built-in tools (`EditTool`, `ReadTool`, `WriteTool`, `GrepTool`, `FindTool`, `LsTool`, `WebSearchTool`, `GetSearchResultsTool`) for single-dependency access via `oxicode-sdk`

## [0.15.1] - 2026-05-16

### Fixed — oxicode-agent

- **tool_exec.rs**: Add `+ Send` bound to `FinalizedToolCallEntry::Future` and `pending_futures` type alias, making `AgentLoop::run()` / `run_messages()` / `continue_loop()` futures `Send`-compatible for `tokio::spawn`

### Changed — oxicode-sdk, oxicode-cli

- Bump `oxicode-agent` dependency to 0.15.1

## [0.15.0] - 2026-05-16

(No changelog entry recorded)

## [0.14.0] - 2026-05-16

### Added — oxicode-sdk (oxios Agent OS Engine)

- **KernelToolProvider trait** (`oxicode-sdk/src/kernel_bridge.rs`): Bridge interface for oxios kernel tools (exec, memory, browser, persona) to be plugged into the SDK agent builder
- **AgentGroup** (`oxicode-sdk/src/agent_group.rs`): Multi-agent orchestration with Pipeline/Parallel/Orchestrated strategies
- **MessageBus** (`oxicode-sdk/src/message_bus.rs`): Broadcast-based inter-agent communication for oxios environments
- **AgentMetrics** (`oxicode-sdk/src/metrics.rs`): Atomic counters for tracking runs, tokens, durations with snapshot export

### Added — oxicode-agent

- **Agent::export_state() / import_state()**: Session persistence via JSON serialization of AgentState
- **Agent::continue_with()**: Session continuation within same agent instance
- **Agent::run_tokio_stream()**: Tokio-native event streaming with tokio::sync::mpsc channels (WebSocket/SSE gateway friendly)
- **StructuredOutput** (`oxicode-agent/src/structured_output.rs`): JSON extraction and schema validation from agent responses
- **AgentState Serialize/Deserialize**: Full state serialization including messages, tokens, iteration progress
- **AgentConfig::output_mode**: Optional structured output mode configuration

### Added — oxicode-ai

- **ProviderPool** (`oxicode-ai/src/provider_pool.rs`): Rate limiting and concurrency control with semaphore + sliding window RPM for multi-agent shared API key scenarios

### Added — oxicode-sdk / oxicode-agent

- **AgentBuilder::kernel_tools()**: Register kernel tools via KernelToolProvider during agent construction

### Fixed — oxicode-agent

- **edit_diff.rs**: Detect and reject ambiguous matches (old_text appearing >1 time) with clear error message
- **edit.rs**: Add serde aliases for `old_text`/`new_text` to fix multi-edit JSON parsing
- **grep.rs**: Detect and skip broken symlinks before `read_dir` to prevent crashes

### Fixed — tests

- **edge_cases.rs**: Fix `test_read_large_file` offset (101 for 1-indexed), `test_grep_with_broken_symlink` error handling
- **tools.rs**: Fix `test_bash_working_dir` (handle workspace restriction errors), `test_find_path_not_found` (accept 'Cannot read' error)
- **provider_mock.rs**: Fix `test_empty_stream` expectation (1 Start event, not 0)

### Changed — oxicode-agent

- **SharedState now Clone + Arc-based**: `SharedState` wraps `Arc<RwLock<AgentState>>` enabling state sharing across async boundaries
- **AgentInner now Clone**: Inner config/provider cloneable for tokio streaming paths

## [0.13.0] - 2026-05-15

### Added — oxicode-cli / oxicode-agent

- **Thinking level display in footer**: Model shown with thinking level indicator (e.g., `(minimax) MiniMax-M2.7 • high`)
- **Shift+Tab to cycle thinking level**: Press Shift+Tab to cycle through thinking levels: off → minimal → low → medium → high → xhigh → off
- **Thinking level in TUI footer**: Footer now shows thinking level as secondary info (muted color) next to model name

### Changed — oxicode-store

- **ThinkingLevel enum aligned with pi-agent**: Changed from `none, minimal, standard, thorough` to `off, minimal, low, medium, high, xhigh` to match pi-agent naming conventions
- **Default thinking level is now `medium`**: Consistent with pi-agent behavior

### Changed — oxicode-cli / oxicode-ai

- **Thinking level system prompts updated**: All thinking levels (off, minimal, low, medium, high, xhigh) now have appropriate system prompts with distinct characteristics

### Fixed — oxicode-store

- **Fixed failing tests**: Updated environment variable tests to reflect that `apply_env()` and `from_env()` are now no-op (env overrides disabled)
- **Fixed PoisonError in parallel tests**: Removed unnecessary ENV_LOCK usage from tests that don't modify env vars

## [0.8.0] - 2026-05-06

### Added — oxicode-agent

- **2-level agentic loop** matching pi-mono architecture: outer loop (follow-up messages), inner loop (tool calls + steering)
- **turn_start / turn_end events** emitted each iteration for lifecycle tracking
- **Steering messages**: inject user messages mid-run via `session.steer()`, polled after each turn
- **Follow-up messages**: queue messages during agent execution, processed when agent would stop via `session.follow_up()`
- **beforeToolCall / afterToolCall hooks** for tool execution pipeline customization
- **shouldStopAfterTurn hook** for graceful early termination
- **ToolExecutionMode** (Sequential / Parallel) config on AgentHooks
- **Terminate flag propagation**: batch terminates only when every tool result sets `terminate: true`
- **Streaming message lifecycle events**: `MessageStart` → `MessageUpdate` (per delta) → `MessageEnd`
- **ThinkingDelta forwarding** to TUI for real-time reasoning display
- **AgentHooks** struct with all hook types (get_steering_messages, get_follow_up_messages, etc.)
- **ToolBatchResult** for batch tool execution results
- **Compaction per iteration**: context window check at each iteration, not just once

### Added — oxicode-cli

- **Tool snippets in system prompt**: Available tools now show descriptions instead of "(none)"
- **AgentSession queue → Agent hooks connection**: steering/follow-up queues wired to agent loop
- **Input unlock during agent busy**: typing, paste, and Enter allowed while agent is streaming
- **Enter while busy → queue as steering message** instead of being ignored

### Fixed

- **TurnEnd event**: real assistant message instead of placeholder UserMessage
- **Fallback model logic restored** on stream error
- **turn_number**: incremented before use (was starting at 0)
- **web_search.rs** compilation error simplified
- **Removed dead code**: old `execute_tool()` method, unused imports, Korean comments → English
- **ToolExecutionMode default**: Sequential (parallel was fallback to sequential anyway)

### Changed

- System prompt tool descriptions now populated from `tool_snippets` HashMap
- Agent loop restructured from single loop to pi-mono 2-level loop architecture

## [0.5.0] - 2026-05-05

### Fixed — oxicode-ai

- **TextDelta double-push bug** in `high_level.rs` `complete()` function. Text was being pushed to `text_buffer` twice at block boundaries, causing double-counting. Fixed by reordering logic to execute `text_buffer.push_str(&delta)` exactly once.
- **ToolCallStart synthetic ID generation** now uses the actual `tool_call_id` from provider events instead of always generating synthetic IDs.

- **SSE parsing edge cases** comprehensively tested for both OpenAI and Anthropic providers. Added 39 unit tests covering single/multiple events, finish reasons, tool call deltas, usage accumulation, thinking blocks, carriage return line endings, and malformed input handling.
- **Serialization roundtrip tests** added to `types.rs`, `messages.rs`, and `error.rs`. All core types now have comprehensive test coverage for JSON/MessagePack roundtrips.
- Fixed pre-existing `concat!` macro syntax errors in `providers/anthropic.rs` and `providers/openai.rs`.


### Changed — oxicode-ai

- `ProviderEvent::ToolCallStart` now carries `tool_call_id: Option<String>` for real tool call IDs from providers.

- `ContentBlockStart` (Anthropic) now includes `id` field.
- `ContentBlockRef` (Bedrock) now includes `id` field.

### Added — oxicode-agent

- **Parallel tool execution**: `execute_tool_calls_parallel` now uses `futures::future::join_all` for concurrent execution while preserving result order.
- **Circuit breaker integration**: `CircuitBreaker` from `recovery.rs` is now wired into `AgentLoop`. Configurable threshold and open duration with automatic recovery.
- **18 integration tests** covering multi-turn tool use loop, compaction flow, cross-provider model switching, error recovery scenarios, steering messages, and follow-up queue processing.

### Added — oxicode-cli

- **48 AgentSession tests** covering model cycling, thinking level changes, steering/follow-up queues, compaction trigger logic, session persistence, and event subscriptions.

## [0.1.0-alpha] - 2025-05-03

Initial alpha release of the oxicode workspace.

### Added — oxicode-ai

- Unified LLM API with provider-agnostic `Context` and `Message` types
- Streaming response handling via async `ProviderEvent` streams
- Multi-provider support (OpenAI, Anthropic, Google, Ollama, OpenRouter)
- Tool/function calling with typed definitions and responses
- Token estimation with hybrid algorithm (character + token heuristic)
- Conversation context management and message compaction
- Cross-provider message transformation
- JSON Schema validation for structured outputs

### Added — oxicode-agent

- Agent runtime with streaming event loop
- `AgentTool` trait for defining LLM-callable tools
- `ToolRegistry` for tool management and dispatch
- Built-in tools: read, write, edit, bash, web search, questionnaire, review loop
- Context compaction for long conversations
- Tool streaming and progress updates
- Agent event types (thinking, text, tool calls, completion)

### Added — oxicode-tui

- Component-based terminal UI framework
- Differential rendering (line-level dirty tracking)
- Theme system with TOML/JSON hot-reload
- Built-in components: Text, Input, Editor, Markdown, Completion
- Overlay system for modals and popovers
- Image rendering with Kitty and iTerm2 protocol support
- Chat view with streaming display
- Unified keyboard, mouse, and resize event handling

### Added — oxicode (CLI)

- Interactive REPL for chatting with LLMs
- Session system with persistence and branching
- CLI argument parsing via clap
- Skill/template system for reusable prompt patterns
- Extension loading system for dynamic plugins
- Error handling and recovery
- TUI integration for interactive mode

### Added — Skills

- Brainstorming skill for collaborative ideation
- Deep-research skill for investigation and design
- Scout skill for fast codebase reconnaissance
- Super-review skill for deep system analysis
- Design-farmer skill for design system construction
- Playwright CLI skill for browser automation
- Worktree skill for git worktree management
- Obsidian skill for vault operations

### Infrastructure

- Workspace with 4 crates: oxicode, oxicode-ai, oxicode-agent, oxicode-tui
- Comprehensive test suites for all built-in tools
- Project README files for each crate
- MIT license
