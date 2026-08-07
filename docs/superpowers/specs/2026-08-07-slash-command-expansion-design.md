# TUI Slash Command & Provider Configuration Expansion

**Date:** 2026-08-07
**Status:** Approved (autonomous — user delegated design + implementation)
**Scope:** `oxicode-cli/src/tui_vt/` (slash commands, overlays), small touch in `lib.rs`

## Problem

The TUI ships 13 slash commands — functional but thin next to what a modern
coding TUI offers. The two sharpest gaps:

1. **Provider/model config is only reachable via `oxicode setup`**, which *exits*
   the TUI. Inside a session, `/model` only lists the handful of *scoped*
   models — the full 5000+ model catalog is unbrowsable, and there is no way to
   see which providers have credentials, let alone manage them.
2. No introspection commands: tools list, MCP status, diagnostics, export.

## Design

### Architectural enabler: catalog in the slash context

`SlashCtx` currently holds `{ session, handle, state }`. The `ModelCatalog` port
lives on `App.oxicode.catalog()`, not on `AgentSessionHandle`. To let commands
browse the catalog, thread it in:

- `App::catalog()` → `Arc<dyn ModelCatalog>` (new accessor; delegates to
  `self.oxicode.catalog().clone()`).
- `run_tui` captures the `Arc`, threads it through `run_event_loop` →
  `handle_inline_event` → `SlashCtx.catalog: Option<&Arc<dyn ModelCatalog>>`.
  `run_event_loop` already carries `#[allow(clippy::too_many_arguments)]`.

Fallback when no catalog is wired: `oxicode_sdk` static model-db accessors
(`get_all_models`, `get_builtin_providers`) — same fallback the setup wizard uses.

### Overlay primitives available

The list modal (`show_list_modal`) is a single-selection searchable list
returning one `InlineListSelection`. It does **not** do free-text entry — so
in-TUI *API key entry* stays with `oxicode setup` (which has its own input
handling). What the overlay *can* do: browse + select + confirm. That covers
model switching, provider status viewing, and key *removal* (confirm only).

New `InlineListSelection` variants: `CatalogModel(usize)`, `ProviderRow(usize)`.
New `RenderState` fields: `overlay_catalog_models: Vec<(String,String)>`
(`(provider, model_id)`), `overlay_providers: Vec<String>`.

### Commands

| Command | Type | Behavior |
|---|---|---|
| `/models [query]` | NEW | Searchable list of **all** catalog models (`provider/model · ctx · $/M`). Select → `set_model`. Pre-filters on `query`. |
| `/providers` | NEW | List of providers, badge `key`/`—`, subtitle base_url. Select → detail reply (masked key, env hint, model count). Provider *with* a key → confirm modal to remove via `shared_auth_storage().remove_api_key()`. |
| `/tools` | NEW | List modal of registered tools (`agent_ref().tools()`): name, description, `essential` badge. Read-only. |
| `/mcp` | NEW | MCP dashboard from `tools().mcp_manager().dashboard_data()` (sync): servers, connection state, tool counts. Text modal. |
| `/info` | NEW | Diagnostics: version, cwd, session id/file, config paths, log path, model + provider + key status, catalog layer. Text modal. |
| `/export` | NEW | `session.export_html()` → write to `./oxicode-export-<sessionid>.html`, reply with path. |
| `/status` | ENHANCE | Add provider, key status, context window, compaction/advisor state. |
| `/model` | ENHANCE | Picker subtitle shows context window. |
| `/shortcuts` | UPDATE | Cheatsheet gains the new commands. (`/help` auto-enumerates the registry.) |

### Out of scope

- API key *entry* in-TUI (needs text-input overlay → stays with `oxicode setup`).
- `/usage` monetary cost (`CostTracker` not reachable from the session; `/status`
  enhancement covers the useful runtime info).
- `/rewind` (the `RewindCheckpoint` compat variants have no CLI handler).

## Verification

- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`.
- `cargo clippy -p oxicode-sdk --features native-browser -- -D warnings`.
- `cargo nextest run -p oxicode-cli` (existing slash registry tests +
  `builtin_commands_exposes_aliases_for_rpc` must still pass; add unit tests for
  new pure helpers).
- Smoke: `cargo build --release -p oxicode-cli` builds the binary.
