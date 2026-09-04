# oxicode

Rust port of [pi](https://github.com/earendil-works/pi) — terminal-based AI coding assistant. Multi-crate workspace providing multi-provider LLM access, an agent tool-calling loop, a terminal UI, a port-based adapter system, and an SDK for building multi-agent systems.

## Quick Facts

| Item | Value |
|------|-------|
| Language | Rust 2024 edition |
|Workspace crates|12 crates — see "Workspace Layout" below (do NOT hardcode the count; the set evolves)|
| Version | see `Cargo.toml` / `git tag` — single source of truth (do NOT hardcode the number here; it drifts) |
| License | MIT |
| CI | `cargo fmt`, `cargo clippy -D warnings`, `cargo nextest run`, `cargo audit`, `cargo deny check` |
| Workflows | `ci.yml` (8 jobs: fmt/clippy/clippy-native-browser/smoke-test/audit/deny/msrv/doc), `test.yml` (macOS-only matrix + doc), `pr-gate.yml`, `publish.yml` (crates.io, unified), `build-binaries.yml`, `sbom.yml`, `labels.yml` |

> The legacy `oxicode-store` crate (settings, sessions, auth) was absorbed
> into `oxicode-cli/src/store/` as a self-contained sub-module. The legacy
> `oxicode-fs` crate (file-based port adapters) was absorbed into
> `oxicode-sdk/src/ports/fs/`. See the refactor history in CHANGELOG.md.

## Workspace Layout

```
oxicode/
├── oxicode-ai/            Unified LLM API — streaming, multi-provider abstraction (foundation)
├── oxicode-agent/         Agent runtime — tool-calling loop, MCP client, built-in tools
├── oxicode-sdk/           Multi-agent SDK + port contract: 15 port traits + reference impls
├── oxicode-cli/           CLI binary — composition root (TUI + RPC + print modes)
├── oxicode-catalog/       Shared model-catalog types (models.dev snapshot consumers)
├── oxicode-hashline/      Line-anchored patch format for AI-assisted code editing
├── oxicode-lsp/           LSP bridge
├── oxicode-api-stability/ Semver-stability lint helpers for the public API surface
├── oxicode-snapcompact/   Context compaction via PNG rasterization (fontdue)
├── oxicode-textarea/      Atomic-mutation text editor widget (ported from grok's xai-ratatui-textarea)
├── oxicode-vtui/          Terminal UI framework — theme registry, design/layout, markdown
├── oxicode-vtui-compat/   Compat stubs for vendored vtcode-ui (protocol types + substrate)
```

> The standalone `oxicode-tui` widget library (tape engine, DiffBackend, glyph
> system, 28-slot ColorScheme) was DELETED as dead code — it was never in the
> workspace build and had zero dependents. The production TUI is `oxicode-vtui`
> (an adapted re-vendoring of vtcode-ui) consumed by `oxicode-cli/src/tui_vt/`.
> Rendering is ratatui on the main screen (no tape engine, no alt screen); the
> single render driver is `tui_vt/main_loop.rs::render_frame`.

### Dependency Flow

Leaf crates (zero internal `oxicode-*` deps): `oxicode-hashline`, `oxicode-lsp`,
`oxicode-catalog`, `oxicode-api-stability`, `oxicode-snapcompact`, `oxicode-vtui`
(`+ oxicode-vtui-compat`).

```
oxicode-ai  (foundation)              oxicode-hashline (independent)
  ↓                                 ↓
oxicode-agent  ←  oxicode-ai, oxicode-hashline
  ↓
oxicode-sdk  ←  oxicode-ai, oxicode-agent, oxicode-snapcompact
  ↓
oxicode-cli  ←  oxicode-ai, oxicode-agent, oxicode-sdk, oxicode-lsp, oxicode-vtui, oxicode-textarea
```

`oxicode-ai` is the foundation layer with zero internal dependencies.
`oxicode-cli` is the integration layer that depends on all other crates.
Never create circular dependencies between crates.

## Port System (oxicode-sdk)

`oxicode-sdk` defines **15 port traits** as the contract between the SDK
and product-specific infrastructure. Each port has a noop default;
products register their own implementations via `OxicodeBuilder::with_port_*`
or `with_ports(PortRegistry)`.

| Port | Purpose | oxicode-cli uses | oxios (sister repo) uses |
|---|---|:-:|:-:|
| `StateStore` | Durable key-value / append-only | ✅ `FileStateStore` | 🔜 TBD |
| `ConfigStore` | Layered configuration | ✅ `FileConfigStore` | 🔜 TBD |
| `AuthProvider` | API keys + OAuth | ✅ `FileAuthProvider` | 🔜 TBD |
| `EventBus` | pub/sub kernel events | ✅ `InProcessEventBus` | 🔜 TBD |
| `SkillLoader` | SKILL.md discovery | ✅ `FileSkillLoader` | 🔜 TBD |
| `PersonaProvider` | System-prompt fragments | ✅ `FilePersonaProvider` | 🔜 TBD |
| `AccessGate` | Pre-execution policy | ✅ `SimpleAccessGate` | 🔜 TBD |
| `CapabilityResolver` | Subject → tool visibility | ✅ `TomlCapabilityResolver` | 🔜 TBD |
| `MemoryStore` | Episodic / semantic | ✅ `InMemoryMemoryStore` | 🔜 TBD |
| `CronScheduler` | Time-based triggers | ✅ `InMemoryCronScheduler` | 🔜 TBD |
| `ResourceMonitor` | Usage limits | ✅ `CountingResourceMonitor` | 🔜 TBD |
| `InternalUrlRouter` | Resolve internal URIs (`skill://`, `issue://`, …) | ✅ wired | 🔜 TBD |
| `ProtocolHandler` | Handle internal-protocol requests | ✅ wired (7 impls: issue, pr, memory, skill, rule, agent, local in `services.rs`) | 🔜 TBD |
| `RuleRegistry` | Project steering rules (TTSR) | ✅ wired | 🔜 TBD |
| `EmbeddingProvider` | Vector embeddings for memory | — (noop; durable memory = oxibrain daemon) | 🔜 TBD |

(Plus `ModelCatalog` in `ports/catalog.rs` for catalog/model-data access.) See `oxicode-sdk/src/ports/mod.rs` for the canonical trait list.

Reference implementations live in `oxicode-sdk/src/ports/fs/` (file-based)
and `oxicode-sdk/src/ports/inmem/` (in-memory). See `docs/PORT_GUIDE.md`
for the full contract, the noop-fallback semantics, and patterns for
writing new impls.

> See also [`docs/oxicode-sdk-ownership.md`](docs/oxicode-sdk-ownership.md) for the
> behavior↔policy ownership contract that prevents parallel evolution between
> the SDK and consumers (oxios).

## Architecture Overview

### oxicode-ai — Unified LLM API

Provider-agnostic streaming interface. Core trait in `providers/trait_def.rs`:

```rust
pub trait Provider: Send + Sync + 'static {
    fn stream<'a>(
        &'a self,
        model: &'a Model,
        context: &'a Context,
        options: Option<StreamOptions>,
    ) -> Pin<Box<dyn Future<Output = StreamResult> + Send + 'a>>;
}
```

> **Identity ≠ transport.** The trait has **no `name()`** — provider identity
> (the canonical catalog id) lives in the registry key and `Model.provider`,
> never on the transport (Step 2 / P0.3 three-way split).

**8 built-in providers** in `src/providers/`: openai, openai-responses, anthropic, google, vertex, azure, bedrock, ollama.
`model_db.rs` + `catalog/` index pricing/context/feature data for 5000+ models
across 70+ providers, with models.dev as the source of truth (see
`data/catalog/README.md`):
- **SNAP (Layer 1)** — embedded models.dev snapshot `_snapshot.json.gz`
  (`include_bytes!`-ed; works fully offline on first run).
- **LIVE (Layer 2.5)** — runtime cache `<home>/cache/models-dev.json` (canonical oxicode home; ETag-aware
  conditional GET, ~1h mtime window). `catalog/models_dev.rs`.
- **Layer 2** — user overrides (`<home>/catalog/overrides.toml`.
- **LOCAL (Layer 3)** — runtime `/v1/models` discovery for local servers
  (ollama/lmstudio/vllm/sglang).
  Gates: `OXICODE_MODELS_DEV`, `OXICODE_MODELS_DEV_URL`, `OXICODE_MODELS_DEV_DISABLE_FETCH`,
  `OXICODE_MODELS_DEV_MTIME_WINDOW`, `OXICODE_MODELS_DEV_FORCE_REFRESH`,
  `OXICODE_MODELS_DEV_CACHE_PATH`, `OXICODE_CATALOG_SNAPSHOT`.
`compaction.rs` summarizes old messages when context grows too large.
`ProviderRegistry` in `mod.rs` supports both custom providers (via `register()`) and built-in fallback (via `register_builtins.rs`).

Key types: `Model`, `Context`, `Message`, `ContentBlock`, `Tool`, `ProviderEvent`, `ProviderError`, `ProviderRegistry`.

### oxicode-agent — Agent Runtime

Manages the LLM tool-calling loop. Core trait in `src/tools.rs`:

```rust
#[async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn label(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;
    fn essential(&self) -> bool { false }
    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        signal: Option<oneshot::Receiver<()>>,
        ctx: &ToolContext,
    ) -> Result<AgentToolResult, ToolError>;
}
```

**~21 tools** across `src/tools/` (registered by `ToolRegistry::with_builtins_cwd`, plus `ask` wired by the `oxicode-cli` composition root): read, write, edit, bash, grep, find, ls, todo, ask, web_search, get_search_results, github, subagent, memory_recall, memory_reflect, memory_retain, memory_edit, mcp, context7 (2 sub-tools), generate_image, commit.
**7 essential tools** (cannot be disabled): read, write, edit, bash, grep, find, ls.
`agent_loop/` contains streaming, tool execution, retry logic, and queue management.
`mcp/` implements Model Context Protocol client.
`agent.rs` has `ProviderResolver` trait for resolving provider/model by name.

Key types: `Agent`, `AgentEvent`, `AgentState`, `AgentConfig`, `ToolRegistry`.

### oxicode-vtui — Terminal UI Framework

`oxicode-vtui` is an adapted re-vendoring of the third-party vtcode-ui
framework (itself ported from grok-build). It provides the theme registry,
design/layout primitives, and the markdown renderer consumed by the CLI host.
`oxicode-vtui-compat` is a thin stub substrate (protocol data types + no-op
shims) that lets the vendored framework compile without its original
vtcode-config/vtcode-commons deps.

**No `oxicode-*` dependencies** — pure UI framework. The CLI host
(`oxicode-cli/src/tui_vt/`) owns the event loop and the render driver
(`main_loop.rs::render_frame`); it renders chat on the terminal **main screen**
via ratatui (NOT an alternate screen, NOT a tape engine). `oxicode-vtui`
supplies the parts: `theme::` (62-theme registry + contrast pipeline),
`design::layout` (chrome/agent-view/welcome geometry), `tui::ui::markdown`
(the production `render_markdown`), and `tui::core_tui` (the `InlineCommand`/
`InlineEvent`/`InlineHandle` protocol).

> **Composer text + vim now live outside vtui.** The editable composer buffer
> is `oxicode_textarea::TextArea` (grok's `xai-ratatui-textarea`, ported into
> the `oxicode-textarea` crate) — correct CJK/emoji caret, soft-wrap,
> horizontal scroll, undo/redo. Vim mode is an app-owned module at
> `oxicode-cli/src/tui_vt/vim/`. The old `oxicode-vtui::vim` is **deprecated**
> (removal in a later release).

- **Theme system** (`theme/registry.rs`): 62 themes (58 static + 4 Catppuccin),
  a `ThemePalette`→`ThemeStyles` derivation with WCAG contrast guarantees, and
  a runtime with active/preview state. Programmatic activation
  (`set_active_theme`); no file hot-reload.
- **Markdown renderer** (`tui/ui/markdown/mod.rs`): pulldown-cmark →
  `Vec<Vec<InlineSegment>>`. Renders headings/emphasis/strong/strike/links/
  blockquote/rule/inline-code, fenced code blocks with syntect highlighting
  (theme-aware via `theme::syntax::get_active_syntax_theme`), ordered/unordered
  lists with nesting, and GFM tables (box-bordered). `InlineSegment`/
  `InlineTextStyle` live in `oxicode-vtui-compat::ui_protocol`.
  **Syntect theme caveat:** `ThemeSet::load_defaults()` ships only 7 themes
  (base16-*, Solarized, InspiredGitHub). UI themes whose mapped name isn't in
  that set (Dracula, GitHub, Gruvbox, Catppuccin, OneDark, Material, Night Owl,
  Monokai, Zenburn, Tomorrow, ayu, …) fall back to `base16-ocean.dark` (colored,
  not plain). The default `"oxi"` theme maps to `base16-ocean.dark` and works.
  To get true per-theme colors, vendor extra `.tmTheme` files into the ThemeSet.
- **Vim engine** (`vim/`): **deprecated** — relocated to
  `oxicode-cli/src/tui_vt/vim/` (app-owned). The CLI's `InputEditor` adapter
  bridges vim mutations onto the composer's `oxicode_textarea::TextArea`, so
  caret/undo stay owned by the editor.
- **Glyphs**: the composer's context row honors `glyph_set = "nerd"` —
  text labels (MODEL/THINK/RUN/DIR/GIT/CTX, brain chip) swap for Nerd
  Font private-use glyphs (`oxicode-cli/src/symbols::nerd`, never
  emoji). Other chrome glyphs (⚙ ✓ ☑ ☐ ▸) remain hardcoded in
  `main_loop.rs`.

Key types: `ThemeStyles`, `ThemeDefinition`, `InlineSegment`, `InlineTextStyle`,
`InlineCommand`, `HostAdapter`.

### oxicode-sdk — Multi-Agent SDK + Port Contract

`OxicodeBuilder` is the entry point:

```rust
let oxicode = OxicodeBuilder::new()
    .with_builtins()
    .with_state(Arc::new(my_state_store))
    .with_auth(Arc::new(my_auth))
    .build();
let agent = oxicode.agent(AgentConfig { /* ... */ }).build()?;
```

`AgentGroup` supports parallel, sequential, and fan-out strategies.
`MessageBus` provides pub/sub inter-agent communication.
`KernelToolProvider` bridges SDK agents to the host tool registry.

The SDK is **re-exported through the oxicode-sdk crate** — products do
not depend on `oxicode-ai` or `oxicode-agent` directly. See `docs/PORT_GUIDE.md`
for the full port contract and "single dependency" pattern
(`oxios → oxicode-sdk`, no `oxicode-ai` direct dep).

Key types: `Oxicode`, `OxicodeBuilder`, `AgentBuilder`, `AgentGroup`, `MessageBus`, `PortRegistry`.

### oxicode-cli — CLI Binary

Single binary with three run modes: **TUI** (interactive), **print**
(plain or JSON), **RPC** (JSON-over-stdin/stdout for IDE integration).

Composition root: `oxicode-cli/src/main.rs` is the binary entry point. The
top-level `main()` is a thin dispatcher (12 lines) that routes
to either `handle_subcommand(command)` for non-interactive
subcommands or `oxicode::bootstrap::run_with_args(args)` for the
default TUI/print/RPC modes. **However**, `handle_subcommand`
itself is a large match arm (~90 lines) that delegates each
subcommand to an inline `handle_*` function declared in the same
file (F-5 audit 2026-06-21). Those `handle_*` functions together
add ~1,400 LOC to `main.rs` — contradicting the older "12-line
dispatcher" claim that this paragraph originally made. The
follow-up refactor (move each `handle_*` to `oxicode-cli/src/cli/commands/*.rs`,
see audit F-5 in `oxicode-code-audit-report.html`) is non-trivial because
clap's `Subcommand`-derived enums can hit generic-bound surprises when
referenced from a sibling module — a structural split should be done in
a dedicated PR with explicit testing of every subcommand.

All wiring (settings merge, custom-provider registration, router
registration, built-in tool registration, WASM extension loading) lives
in `oxicode-cli/src/bootstrap.rs`. The run-mode dispatcher
(`dispatch_run_mode`) routes to TUI / print / RPC based on flags.

The `App` struct is built via `App::from_oxicode(oxicode, settings)` from
the wired `Oxicode` engine — no manual `Agent::new(provider, config, ...)`
calls anymore. Subcommand handlers (config, session, export, share,
setup, models) stay in `main.rs` because they are dispatch targets,
not bootstrap.

Self-contained submodules in `oxicode-cli/src/`:

- `store/` — domain types and file-based adapters (was `oxicode-store`):
  `session.rs` (JSONL session persistence), `settings.rs` (layered
  config), `auth_storage.rs` (API keys + OAuth), `router_config.rs`
  (auto-routing rules), `session_cwd.rs` (cwd binding).
- `bootstrap.rs` — composition root.
- `tui_vt/` — interactive TUI host (the production render driver + event loop):
  `main_loop.rs` (`render_frame`, `run_event_loop`, `spawn_input_thread`,
  ~4.9K LOC), `frame_layout.rs` (chrome), `file_search.rs` (@-file picker),
  `notifications.rs`, `slash/registry.rs` (slash commands).
- `app/agent_session*.rs` — single-shot session wrapper around
  `Agent`.
- `setup_wizard.rs` — interactive `oxicode setup`.
- `rpc_mode/` — JSON-RPC mode.
- `extensions/` — WASM / native extension loading.
- `storage/packages.rs` — built-in package manager (ClawHub-style).

Extension system (`src/extensions/types.rs`):

- `ExtensionManifest`: metadata with permissions (FileRead, FileWrite, Bash, Network)
- `ExtensionState`: Pending, Active, Disabled, Failed, Unloaded
- `InputEventResult`: Continue, Transform { text }, Handled

## Conventions

### Code Style

- `cargo fmt` before every commit — no exceptions.
- `cargo clippy --workspace --all-targets -- -D warnings` must pass clean
  (the ci.yml `clippy` job and the pre-commit hook both enforce this).
  Test/bench/example code relaxes exactly two test-idiom lints —
  `clippy::unwrap_used` and `clippy::field_reassign_with_default` — via
  `#![cfg_attr(test, allow(...))]` at each library crate root; shipped
  (non-test) code still `warn`s on `unwrap_used`. Every other lint
  (correctness, suspicious, style, complexity) is enforced even in tests.
- **`native-browser` must always compile.** `native-browser` is a default
  feature of `oxicode-cli` (browsing is a product capability, not an SDK
  contract). The `ci.yml` `clippy-native-browser` job runs
  `cargo clippy -p oxicode-cli -- -D warnings` (compiles
  `oxibrowser_backend.rs`, the pure-Rust headless browser backend) plus
  `cargo build -p oxicode-agent --features native-browser`. This path was
  previously never CI-verified, which let edition-2024 lifetime bugs ship
  (0.32.0–0.34.0). To verify locally:
  ```bash
  cargo clippy -p oxicode-cli -- -D warnings
  cargo build -p oxicode-agent --features native-browser
  ```
- **Pre-commit hooks** — `.pre-commit-config.yaml` mirrors the ci.yml
  gate. Install once: `pre-commit install`. On every `git commit`,
  trailing whitespace, EOF, YAML/TOML lint, merge-conflict, large
  files, private-key scan, and `cargo fmt --check` /
  `cargo clippy --all-targets` run automatically. PRs that fail
  locally fail remotely too.
- Module structure: `mod.rs` re-exports public API, implementation in sibling files.
- Prefer `anyhow::Result` for application code, custom error enums (`thiserror`) for library crates.
  - **Library crates** (oxicode-ai, oxicode-agent, oxicode-sdk): define typed error enums with `thiserror::Error` for public API functions. Internal helpers may use `anyhow`.
  - **Application crate** (oxicode-cli): use `anyhow::Result` everywhere.
  - **Leaf crate** (oxicode-vtui): `anyhow` is acceptable — no public error types needed.
  - Never create a shared workspace error crate. Each library owns its own error type.
- Use `parking_lot::RwLock` instead of `std::sync::RwLock`. (But
  `parking_lot::MutexGuard` is `!Send` — drop the guard before any
  `.await` or use `tokio::sync::Mutex` instead.)
- Atomic file writes: use the `temp + rename` pattern
  (e.g. `oxicode-sdk/src/ports/fs/session.rs::FileStateStore`).
- Async: `tokio` runtime with `#[tokio::main]`. Use `async_trait` for trait objects.

### Testing

- Unit tests in `#[cfg(test)] mod tests` within each module.
- Integration tests in `<crate>/tests/*.rs` (e.g., `oxicode-agent/tests/agent_loop_full.rs`).
- Mock providers (`MockProvider`) for agent/loop testing.
- **Test runner: `cargo-nextest`** — parallel execution, per-test timeouts.
- Config: `.config/nextest.toml` (profiles: `default`, `ci`, `release`).
- `cargo nextest run --workspace` must pass before merge.

### Adding a New LLM Provider

1. Create `oxicode-ai/src/providers/<name>.rs`.
2. Implement the `Provider` trait.
3. Add `BuiltinProvider` entry in `oxicode-ai/src/providers/register_builtins.rs`.
4. The model catalog is powered by models.dev (see `oxicode-ai/data/catalog/README.md`).
   If the provider exists on models.dev, its models appear automatically once the
   embedded snapshot is refreshed (regenerate `data/catalog/_snapshot.json.gz`).
   Add oxicode-specific provider metadata (extra HTTP headers, etc.) to
   `data/catalog/product-meta.toml`.

### Adding a New Tool

1. Create `oxicode-agent/src/tools/<name>.rs`.
2. Implement the `AgentTool` trait.
3. Add module declaration in `oxicode-agent/src/tools.rs`.
4. Register in `ToolRegistry::with_builtins_cwd()`.
5. Mark `essential()` as `true` if it cannot be disabled.


### Behavior Packs (`coding-omp-v1`)

The canonical coding composition is the `coding-omp-v1` behavior pack
(`oxicode-sdk/src/behavior/packs/coding_omp_v1.rs`, feature `behavior`).
The CLI installs it through the SDK's host installer interception point
(`oxicode-cli/src/behavior.rs`) before App/Agent construction.

- `ToolRegistry::with_builtins_cwd` remains a low-level convenience; it is
  NOT a `coding-omp-v1` behavior guarantee and must not be described as one.
- Do not hardcode the pack's tool list outside `behavior/packs/` — the
  descriptor table there is the single source of truth.
- Tool changes that alter coding semantics require a descriptor id bump
  (e.g. `edit.hashline.v1` → `edit.hashline.v2`) plus a ledger review; a
  replacement must declare `replaces` and schema compatibility.
- The compatibility ledger (`omp@v18.0.11` target) may only advance from
  `Partial`/`Unavailable` to `Equivalent` when the named fixture scenario
  in `oxicode-sdk/tests/behavior/` passes. Hosts can render ledger statuses
  but never upgrade them.
### Adding a New Port Implementation

1. Implement the port trait from `oxicode_sdk::ports::*`.
2. The SDK does not include concrete adapters (beyond `fs/` and
   `inmem/`) — products write their own.
3. Register via `OxicodeBuilder::with_port_*(Arc::new(my_impl))` in your
   composition root.

### Adding a New Extension Type

1. Define types in `oxicode-cli/src/extensions/types.rs`.
2. Implement loading in `oxicode-cli/src/extensions/loading.rs` (native) or `wasm.rs` (WASM).
3. Register hooks in `oxicode-cli/src/extensions/registry.rs`.

## Common Commands

```bash
# Build & Test
cargo build                          # Debug build
cargo build --release                # Release binary
cargo nextest run --workspace        # Run all tests (parallel)
cargo nextest run -p oxicode-agent       # Test single crate
cargo nextest run --profile ci       # CI profile (retries, no fail-fast)
cargo nextest run -j 1               # Sequential (debug race conditions)

# Lint & Format
cargo clippy --workspace --all-targets -- -D warnings   # Lint
cargo fmt --all -- --check           # Format check

# Docs
cargo doc --workspace --no-deps      # Build docs
cargo test --workspace --doc         # Doc tests
```

## File Locations

| Purpose | Path |
|---------|------|
| Global config | `<home>/settings.{toml,json}` — canonical `$OXICODE_HOME` / `$OXI_HOME`/`oxicode` / `~/.oxi/oxicode`; legacy `~/.oxicode` read-only fallback |
| Project config | `.oxicode/settings.toml` (project-local, separate namespace) |
| Sessions | `<home>/sessions/` (canonical; legacy read-only fallback) |
| Extensions | `<home>/extensions/` (canonical; legacy read-only fallback) |
| Skills | `<home>/skills/<name>/SKILL.md` (canonical; legacy read-only fallback) |
| **models.dev cache** | `<home>/cache/models-dev.json` (~1h mtime window, atomic writes) |
| **oxibrain socket** | `~/.oxi/brain/oxibrain.sock` (override: `OXIBRAIN_SOCKET`) |
| MCP config | `~/.config/oxicode/mcp.json` or `.oxicode/mcp.json` |
| Logs | `<home>/logs/` |
| Nextest config | `.config/nextest.toml` |
| Pre-commit config | `.pre-commit-config.yaml` |
| Issue labels | `.github/labels.yml` (synced by `labels.yml` workflow) |
| Funding | `.github/FUNDING.yml` |

## CI/CD & Release Pipeline

oxicode ships a multi-stage pipeline. The full source is under
`.github/workflows/`:

| Workflow | Trigger | Purpose |
|----------|---------|---------|
| `ci.yml` | PR + push to main/develop | Fast feedback: `fmt`, `clippy`, `clippy-native-browser`, `smoke-test`, `audit`, `deny`, `msrv`, `doc`. ~2-3 min for the fast jobs. |
| `publish.yml` | `v*` tag push + manual | Single workflow: tag-on-main verification, packaging dry-run, then `cargo publish` in topological order. All on `ubuntu-latest` (GitHub-hosted). No binaries, no GitHub Release, no self-hosted runners. |
| `pr-gate.yml` | PR opened/synchronized/reopened | Conventional-Commit title, PR size ≤ 4000 lines, no merge commits, issue linkage. |
| `build-binaries.yml` | weekly cron + manual | Continuous binary build (no release artifact) for sanity. |
| `sbom.yml` | push to main + release | Generates CycloneDX 1.5 SBOM and submits it to GitHub's dependency-graph API. |
| `labels.yml` | weekly + labels.yml change | Syncs `.github/labels.yml` to the repo's label set via `EndBug/label-sync`. |

### Required GitHub Secrets

| Secret | Used by | Required? | How to create |
|--------|---------|:---:|---------------|
| `CARGO_TOKEN` | `publish.yml` | ✅ **Yes** (to publish) | <https://crates.io/settings/tokens>, scope: publish |

**Scope decisions (2026-06-20):**

- **Distribution channel:** crates.io only. No Homebrew tap, no Scoop bucket.
  No binary builds, no GitHub Release artifacts.
- **Runner:** `ubuntu-latest` (GitHub-hosted) only. Self-hosted runners are
  no longer required — `release.yml` was replaced by a unified `publish.yml`
  that runs entirely on GitHub infrastructure.
- **Supply chain:** crates.io package signing. No SHA256SUMS, no GPG, no SBOM
  in the publish pipeline (the `sbom.yml` workflow still generates CycloneDX
  SBOMs on push to main and release).

With **only** `CARGO_TOKEN` configured, the full pipeline runs:
CI gates (`ci.yml`) + tests (`test.yml`) + PR gate + crates.io publish
(`publish.yml`) + SBOM + label sync.

1. **Streaming-first** — every provider streams tokens. No blocking request/response.
2. **Provider-agnostic** — all LLM providers share the same `Provider` trait and message types.
3. **Append-only sessions** — conversation history is never mutated, only appended. Branching creates new entries.
4. **Progressive enhancement** — core works with zero config. Settings, extensions, skills, MCP are all opt-in layers.
5. **Sandboxed extensions** — WASM extensions get zero host access by default. Permissions must be explicitly requested.
6. **Atomic I/O** — file writes go through temp+rename to prevent corruption on crash.
7. **Port-based adapters** — infrastructure (state, auth, events, memory, skills, …) is **opt-in**. Products register only the ports they care about; the SDK provides noop fallbacks for the rest.
8. **SDK is the contract, not the implementation** — `oxicode-sdk` defines port traits and ships small reference impls. Products (oxicode-cli, oxios) write their own domain-specific impls.
9. **Composition root in one place** — each binary has a single module that wires its `Oxicode` engine. Wiring code does not leak into business logic.

## Pitfalls

- **grep v2 semantics (`grep.search.v2`).** The pack-installed `grep` runs on
  ExactSearchEngine: repository ignore files are honored (`.gitignore`,
  `.ignore`, global/exclude), hidden entries are skipped, built-in artifact
  dirs (`node_modules`, `target`, `dist`, `build`, `__pycache__`, `.venv`,
  `venv`) are excluded, symlinks are not followed, and cancellation returns
  partial results with a `cancelled` marker. Files matched by ignore files
  no longer appear in results — pass an explicit `path` to search ignored
  trees. The legacy walker survives only via
  `ToolRegistry::with_builtins_cwd` until 0.83 (migration window);
  `grep.search.v2` replaces `grep.search.v1` with ledger fixture
  `behavior::grep_search_v2_contract`.
- **Durable memory = oxibrain daemon, nothing else.** The only memory
  authority is the local oxibrain daemon over its unix socket
  (`~/.oxi/brain/oxibrain.sock`, override `OXIBRAIN_SOCKET`; resolver:
  `oxicode-cli/src/foundation/brain.rs::default_socket_path`). Do NOT add
  local fallback stores (SQLite/JSONL) — `memory_enabled && socket present`
  is the gate (`services.rs::brain_socket_present`); without the daemon the
  memory tools degrade to typed unavailable errors. The legacy
  `oxicode-mnemopi` crate and the cli's local memory stack were deleted
  (2026-08-18); the `oxicode-sdk` `EmbeddingProvider` port remains for
  feature-gated consumers (oxios). Legacy `~/.oxicode/memory/items.jsonl`
  migrates via `oxicode migrate brain`. TUI surfaces health via the
  `brain·ok`/`brain·down` chip right-aligned on the composer's top
  border and the `/brain` (alias `/memory`) slash command — which also
  REVIVES a down daemon (launchd bootstrap/kickstart, detached spawn
  fallback), and the TUI prober auto-revives once per session. The
  static shortcuts bar was removed 2026-08-22 (contextual abort/quit
  hints only).

- `oxicode-ai` has no dependency on other oxicode crates. Do not import `oxicode_agent` from it.
- Session entries form a tree via `parent_id`, not a flat list. Always traverse with this in mind.
- Provider message formats differ significantly (Anthropic vs OpenAI). Use `transform.rs` for conversion.
- The tool-calling loop in `agent_loop/` has retry logic — tool implementations must be idempotent.
- **Issue-system ownership identity (Phase 0 / defect #13).** The local `issue`
  tool's `start`/`close` ownership checks use `ToolContext.session_id` as the
  caller identity, and `is_session_alive` checks a per-session `flock` under
  `.oxicode/issues/.alive/<session_id>`. For this to actually protect anything,
  the identity must (a) be **non-empty** and (b) **match a flock the process
  holds**. `oxicode-cli` enforces both by construction:
  - `bootstrap.rs::build_app` picks the identity — `tui-<pid>-<uuid>` in TUI
    mode, `proc-<pid>-<uuid>` otherwise. Both are unique per process: a
    shared identity across parallel sessions would collapse ownership
    exclusivity (the historical constant `"tui"` did exactly that — the
    second TUI's flock failed silently and both sessions passed
    `require_owner` as the same caller).
  - `App::from_oxicode(..., ownership_session_id)` acquires the flock for `App`'s
    lifetime AND sets `AgentConfig.session_id = Some(ownership_session_id)`,
    which `agent.rs` threads into `AgentLoopConfig.session_id` →
    `ToolContext.session_id`.
  - `App::ownership_session_id` is the single source of truth. It is injected
    into `RenderState.ownership_session_id` (TUI) — the panel and the `/issue`
    slash command name the flock holder through that field, never through a
    shared constant. Flock acquisition failure is logged loudly
    (`acquire_ownership_guard`), never silently ignored.
  - Liveness provenance: `liveness::acquire` records `OwnerInfo`
    (session/pid/host/cwd/started) INSIDE the `.alive/<session_id>` lock
    file; `read_owner_info` reads it back and `format_issue_full` surfaces it
    (`lock: pid N on host (cwd: …)`). Absent/legacy-empty payload means
    "unknown holder", not "dead".
  - **Do NOT** re-introduce a hardcoded `session_id: None` in `agent.rs`'s
    `AgentLoopConfig` construction (that was the #13 bug — it made every agent
    `start` write an empty-string owner that was instantly reclaimable).
    Regression coverage: `session_id_wiring_tests` (oxicode-agent) +
    `start_with_distinct_live_owners_collides` (oxicode-cli).
- **Issue-system CAS: strict store, recovery in the tool (Phase 2 / #2).** The
  store's `update` is deliberately strict — it returns raw `IssueError::Conflict`
  and **never retries on its own**. Only the `issue` agent tool wraps mutations in
  `cas_retry` (4 attempts: first try uses the agent's `content_hash` as a fast
  path; on conflict it re-reads a fresh hash and retries). Consequence: **a
  stale `content_hash` from an earlier `read` is advisory, not fatal** — the tool
  auto-reconciles. If you add a new mutating store op the agent can call, route
  it through `cas_retry` (see `oxicode-agent/src/tools/issue.rs`; since 2026-08-29 the
  issue store + tool live in `oxicode-agent/src/issues/` and are re-exported through
  `oxicode-sdk` — the TUI panel and CLI stay in `oxicode-cli`). Direct `oxicode issue` CLI calls do
  **not** retry (CLI is a single-shot caller; re-read manually on conflict).
  `IssuePatch` (absent=keep, `Some([])`=clear labels, `Some`=replace) is the
  precise mutation surface — prefer `apply_patch`/`reopen`/`close` over
  hand-rolled `update` closures, and note `apply_patch` **enforces ownership**
  (different non-empty assignee → `NotAssigned`), matching the legacy policy.
  `update` also does **no-op detection** (skips write/timestamp/invalidate when
  nothing meaningful changed), so don't rely on a no-op `update` bumping
  `updated_at` or the dir mtime.
- The catalog is models.dev-sourced, not hand-written. The embedded snapshot
  `data/catalog/_snapshot.json.gz` is the source of truth (regenerate per
  `oxicode-ai/data/catalog/README.md`); per-provider TOML no longer exists.
  oxicode-specific provider metadata lives in `data/catalog/product-meta.toml`.
  Set `OXICODE_MODELS_DEV=off` to test the embedded snapshot alone.
- SSE parsing handles partial UTF-8 lines. Do not assume line boundaries are clean.
- `Agent::is_running` field prevents concurrent agent runs — check this before spawning parallel tasks.
- Port trait methods are **async**. `MutexGuard`s held across `.await` will not compile (`!Send`). Use `tokio::sync::Mutex` or scope the lock.
- The legacy `oxicode-store` crate no longer exists. If a new file needs session/settings/auth, put it in `oxicode-cli/src/store/` (or in a sibling product's store).
- The `oxicode-cli` crate is a **monorepo monolith** by design (~66K lines as of 2026-07-25; was ~17K when this rationale was written — the crate has grown 4× since). Do not split it into more crates unless the 4 separation conditions (independent reuse, independent versioning, build isolation, team boundary) genuinely hold — re-evaluate at ~80K LOC.
- **Language policy settings were removed in P4.3.** Do not reintroduce
  `language_policy_enabled`, `output_languages`, `KNOWN_CHANNELS`, or prompt
  directives for them without a new product decision. `/settings` exposes only
  live settings that still exist; print, RPC, and TUI share no hidden language
  policy behavior.
- **Theme system lives in `oxicode-vtui`.** The standalone `oxicode-tui`
  `ColorScheme` (28 slots, TOML hot-reload, glyph `Symbols`) documented in
  older revisions described a now-DELETED dead crate — ignore it. The
  production TUI (`tui_vt/`) renders via `oxicode_vtui::theme`:
  - To change colors, edit the `"oxi"` `ThemeDefinition` seed fields in
    `oxicode-vtui/src/theme/registry.rs`. The `color_math` WCAG pipeline
    derives the rest — never hand-patch derived colors.
  - `ThemeStyles` (23 ratatui `Style` fields) comes from
    `ThemePalette::build_styles_with_accessibility`; read it via
    `theme::active_styles()`. Contrast is enforced by `validate_theme_contrast`.
  - Default theme `"oxi"` (DEFAULT_THEME in `oxicode-vtui-compat`): pure-black
    canvas `#000000`, warm ink `#fbfaf7`, calm-gray chrome `#c8c8c8`, blue info
    `#53a3f2`, red alert `#ff6467`, purple logo `#cc97f3` (oxi-design-system
    `DESIGN.md` v1.0). See `docs/oxi-design-system-tui.md` for the
    OKLCH→palette mapping.

## TUI architecture (current)

The production TUI stack is **`oxicode-vtui` + `oxicode-cli/src/tui_vt/`**.
The host (`tui_vt/main_loop.rs`, ~4.9K lines) owns the event loop and renders
chat via ratatui on the **main screen** (no alternate screen, no tape engine).
The single render driver is `render_frame`; `oxicode-vtui` supplies theme,
layout, markdown, vim, and the `InlineCommand`/`InlineEvent` protocol. The
older grok-inspired `oxicode-tui` v2 (RetainedTree/DiffBackend) and the legacy
widget library (`oxicode-tui`, tape engine, glyph system) are both DELETED as
dead code — neither was in the production path.

## Adding a TUI Slash Command

Slash commands run in the VT TUI host (`oxicode-cli/src/tui_vt/slash/`):

- `slash/registry.rs` — `SlashCommand` trait, `SlashCtx`, `SlashRegistry::builtins()`, `register_all()` (core commands: quit/clear/model/theme/find/…). `/help` auto-enumerates the registry; autocomplete uses `builtin_commands()`.
- `slash/commands.rs` — extended commands, registered via `register_extra()` (catalog/introspection-related ones go here).

1. Implement `SlashCommand`: `name`, `description`, optional `aliases()`, `execute(&self, args: &str, ctx: &mut SlashCtx) -> SlashOutcome`.
2. Register in `register_all()` (registry.rs) or `register_extra()` (commands.rs).
3. Picker UI: build `Vec<InlineListItem>` + an `InlineListSelection` variant, call `ctx.handle.show_list_modal(title, header_lines, items, selected, search)`. New selection variants live in `oxicode-vtui-compat/src/ui_protocol/selection.rs`; backing data in a `RenderState` field (`main_loop.rs`).
4. Handle the selection in `main_loop.rs`'s `OverlayEvent::Submitted` arm (mirror the `Model`/`Theme`/`CatalogModel` handlers).

Reachable from `SlashCtx`: `ctx.session` (Deref→`AgentSession`: `model_id`/`set_model`/`cycle_model`/`thinking_level`/`session_stats`/`export_html`/`agent_ref().tools()`/`mcp_manager().dashboard_data()`), `ctx.state.catalog` (`Option<Arc<dyn ModelCatalog>>`, sync `search_sync`/`list_providers_sync`/… methods), `crate::store::auth_storage::shared_auth_storage()` (`get_api_key`/`set_api_key`/`configured_providers`). Reply via `ctx.reply(InlineMessageKind::{Info,Warning,Error}, text)`.

Limits: the list overlay has NO free-text input — secret *entry* routes to `oxicode setup`; *removal* can stay in-TUI via `ModalConfirmation` + `ConfirmationAction` (mirror the `/clear --yes` re-dispatch in `handle_confirmation_key`). `execute` is synchronous — `tokio::spawn` async work (see `CompactCommand`).

Verify: `cargo fmt` · `cargo clippy -p oxicode-cli --all-targets -- -D warnings` · `cargo nextest run -p oxicode-cli`. Reference impls in `commands.rs`: `/models`, `/providers`, `/tools`, `/mcp`, `/info`, `/export`.

## Verifying platform-gated clippy locally

Host `cargo clippy` skips `#[cfg(target_os = "...")]` modules for other platforms entirely. Verify locally instead of burning CI round-trips (~5–7 min each):

1. `rustup target add x86_64-unknown-linux-gnu` (one-time)
2. `cargo clippy --target x86_64-unknown-linux-gnu -p <crate> --all-targets -- -D warnings`

Works only when the crate's dep tree is pure-Rust (no C libs/linkers: serde, thiserror, tracing, parking_lot, …). If cross-compile fails for unrelated reasons, fall back to CI. ALWAYS rerun `cargo fmt --all -- --check` after editing platform-gated source — rustfmt expands `vec![]` macros one element per line; a grouped `["a","b","c",]` style passes review but fails fmt on CI.
