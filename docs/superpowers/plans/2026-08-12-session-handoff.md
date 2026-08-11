# Session Handoff Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `/handoff` slash command that generates a structured handoff markdown doc from the current conversation, starts a fresh session, and auto-continues work.

**Architecture:** `/handoff` → spawn async task → LLM generates handoff doc via `complete()` → write `.oxicode/handoffs/*.md` → `start_new_session()` → emit `SessionEvent::HandoffComplete` → event loop clears transcript + auto-submits continuation prompt.

**Tech Stack:** Rust, oxicode-cli (slash command + event loop), oxicode-ai (`high_level::complete`), oxicode-agent (model resolution).

## Global Constraints

- `cargo fmt` before every commit.
- `cargo clippy --workspace --all-targets -- -D warnings` must pass.
- Follow existing patterns: `CompactCommand` for async-spawn, `SlashCtx` for command access.
- Handoff docs go to `.oxicode/handoffs/` (project-local, gitignored).
- All code is in `oxicode-cli` except model resolution (`oxicode-agent`).

---

### Task 1: Handoff prompt template + git state gathering

**Files:**
- Create: `oxicode-cli/src/prompt/handoff_prompt.rs`
- Modify: `oxicode-cli/src/prompt/mod.rs` (add module declaration)

**Interfaces:**
- Produces: `pub fn build_handoff_prompt(messages, git_state, chain) -> String`
- Produces: `pub struct GitState { branch, recent_commits, modified_files }`
- Produces: `pub fn gather_git_state(cwd) -> GitState`
- Produces: `pub struct HandoffChain { seq, prev_path }`
- Produces: `pub fn detect_handoff_chain(handoffs_dir) -> HandoffChain`

- [ ] Create `handoff_prompt.rs` with prompt builder, git state, chain detection
- [ ] Add `pub mod handoff_prompt;` to `prompt/mod.rs`
- [ ] Commit

### Task 2: SessionEvent variant + start_new_session

**Files:**
- Modify: `oxicode-cli/src/app/agent_session.rs`

**Interfaces:**
- Produces: `SessionEvent::HandoffComplete { doc_path: String, auto_continue: bool }`
- Produces: `pub fn start_new_session(&self)` on AgentSession

- [ ] Add `HandoffComplete` variant to `SessionEvent`
- [ ] Add `start_new_session` method to `AgentSession`
- [ ] Commit

### Task 3: Handoff doc generation + file writing

**Files:**
- Create: `oxicode-cli/src/app/handoff.rs`
- Modify: `oxicode-cli/src/app/mod.rs` (add module declaration)

**Interfaces:**
- Produces: `pub async fn generate_and_apply_handoff(session, opts) -> Result<()>`
- Consumes: `build_handoff_prompt`, `gather_git_state`, `detect_handoff_chain` (Task 1)
- Consumes: `start_new_session`, `SessionEvent::HandoffComplete` (Task 2)

- [ ] Create `handoff.rs` with `generate_and_apply_handoff`
- [ ] Add `pub mod handoff;` to `app/mod.rs`
- [ ] Commit

### Task 4: HandoffCommand slash command + registration + event loop

**Files:**
- Modify: `oxicode-cli/src/tui_vt/slash/registry.rs` (register + add command)
- Modify: `oxicode-cli/src/tui_vt/main_loop.rs` (event loop handling)

**Interfaces:**
- Consumes: `generate_and_apply_handoff` (Task 3)
- Consumes: `SessionEvent::HandoffComplete` (Task 2)

- [ ] Add `HandoffCommand` struct + impl in `registry.rs`
- [ ] Register in `register_all()`
- [ ] Add `HandoffComplete` handling in event loop's `session_rx` arm
- [ ] Add `.oxicode/handoffs/` to `.gitignore`
- [ ] Commit

### Task 5: Build + test + verify

- [ ] `cargo fmt --all`
- [ ] `cargo clippy -p oxicode-cli -- -D warnings`
- [ ] `cargo build -p oxicode-cli`
- [ ] Write unit tests for prompt building, chain detection, git state
- [ ] `cargo nextest run -p oxicode-cli`
- [ ] Commit
