# Session Handoff Feature — Design Spec

> **Status:** Approved (autonomous — user asleep, review on wake)
> **Branch:** `feat/session-handoff`
> **Date:** 2026-08-12

## 1. Problem

Long TUI sessions suffer context rot — the model degrades as the conversation
grows, losing track of earlier decisions, repeating abandoned approaches, and
ignoring instructions buried mid-context. The existing `/compact` mitigates this
by summarizing in-place, but:

- The summary is lossy — it can drop critical nuances, failed approaches, and
  the reasoning behind decisions.
- The summary stays in the same bloated context; it doesn't give the model a
  clean window.
- There is no durable, reviewable artifact — the user cannot inspect what was
  preserved before the old context is discarded.

## 2. Solution

A `/handoff` slash command that:

1. **Generates** a structured handoff markdown document by making a one-shot LLM
   call that mines the current conversation for goals, decisions, failed
   approaches, current state, and remaining work.
2. **Writes** the document to `.oxicode/handoffs/` (project-local, chain-linked).
3. **Starts a new session** — fresh `SessionManager` (new file, new session ID),
   cleared agent state, cleared transcript.
4. **Auto-continues** — submits a prompt to the new session that instructs the
   agent to read the handoff document and immediately pick up the next task.

This is a clean cutover: the model gets a fresh context window seeded with a
purpose-built brief, and the user gets a durable, reviewable artifact.

## 3. Research Synthesis

Session handoff is a well-established pattern in the AI agent ecosystem (53+
repos on GitHub under `session-handoff`). Key implementations studied:

| Implementation | Stars | Key insight applied here |
|---|---|---|
| **catchup** (Go) | 62 | Cross-agent session transfer via markdown transcript |
| **claude-handoff** (Shell) | 39 | "What We Tried" section is most valuable — failures are expensive to rediscover; chain-linking across sessions; multi-pass map-reduce at 500K+ tokens |
| **agent-work-mem** | 15 | Tiered storage (hot/warm/cold log); `AIMemory/` protocol folder |
| **softaworks session-handoff** | — | Validation (no TODOs, no secrets, quality score); staleness detection; smart scaffolding |
| **MindStudio** (article) | — | Four patterns: rolling summary, phase-based, separate summary agent, persistent state object |

**Design decisions from research:**

- The "What Was Tried" section (including abandoned approaches) is the single
  most valuable part of a handoff — the research is unanimous on this.
- Chain-linking handoffs (each references the previous) provides long-running
  project continuity.
- A separate one-shot LLM call for summarization (MindStudio Pattern 3) keeps the
  handoff quality high without polluting the main agent loop.
- The handoff doc should be intentionally selective — not a full transcript dump.

## 4. Architecture

Three components, all in `oxicode-cli`:

```
/handoff [args]
  │
  ▼
HandoffCommand (slash/registry.rs)
  │  reads: ctx.session.messages(), ctx.session.state()
  │  spawns async task ──────────────────────┐
  │  replies "Generating handoff…"           │
  │  returns SlashOutcome::Handled           │
  │                                          ▼
  │                              ┌─ handoff.rs (generate_handoff_doc) ─┐
  │                              │ 1. Gather git state (branch, commits)│
  │                              │ 2. Build handoff prompt from messages│
  │                              │ 3. LLM call: complete(model, ctx)    │
  │                              │ 4. Write .oxicode/handoffs/*.md      │
  │                              │ 5. session.start_new_session()       │
  │                              │ 6. emit HandoffComplete { doc_path } │
  │                              └──────────────────────────────────────┘
  │                                          │
  ▼                                          ▼
Event loop (session_rx arm)      SessionEvent::HandoffComplete
  │  clears transcript                         │
  │  sends continuation prompt via prompt_tx   │
  ▼                                            ▼
New session reads handoff doc → continues work
```

### Component responsibilities

| Component | File | Responsibility |
|---|---|---|
| `HandoffCommand` | `tui_vt/slash/handoff.rs` | Slash command: parse args, spawn task, reply status |
| `generate_handoff_doc` | `app/handoff.rs` | Read messages, build prompt, LLM call, write doc |
| `start_new_session` | `app/agent_session.rs` | Create new SessionManager, swap, reset agent state |
| `SessionEvent::HandoffComplete` | `app/agent_session.rs` | Signal event loop to clear transcript + auto-submit |
| Event loop handler | `tui_vt/main_loop.rs` | Catch event, clear transcript, send prompt |

## 5. Handoff Document Structure

```markdown
# Session Handoff — {YYYY-MM-DD HH:MM}

> **From session:** {session_id_short}
> **Branch:** {git_branch}
> **Model:** {model_id}
> **Generated:** {ISO timestamp}
> **Chain:** #{seq} (continues from {prev_handoff_path or "—"})

## Goal

{What we're working on and why — 1-2 paragraphs. The original user request
distilled to its essence.}

## Current State

{What is done and what is in progress — bullet list of concrete accomplishments
and the current state of work. Be specific: "auth middleware implemented and
tested" not "worked on auth".}

## What Was Tried

{Every approach attempted, including failures. This is the most valuable section
— abandoned approaches are the most expensive thing to rediscover.}

### {Approach 1: short title}
- **What:** what was attempted
- **Result:** what happened
- **Verdict:** kept | abandoned (and why)

## Key Decisions

{Architectural choices with rationale. Record what was chosen AND what was
rejected, and why.}

## Remaining Work

1. {First next step — concrete, actionable, with file paths if relevant}
2. {Second next step}
3. ...

## Critical Files

| Path | Role |
|------|------|
| `path/to/file` | what it does and why it matters |

## Gotchas & Risks

- {Known issues, pitfalls, things to watch out for, pre-existing flaky tests,
  race conditions, etc.}
```

### Prompt engineering

The LLM prompt (in `prompt/handoff_prompt.rs`) will:

1. Include the full conversation as context (all messages with roles).
2. Include git state (branch, last 5 commits, modified files).
3. Include chain context (previous handoff path if exists).
4. Instruct the model to produce the structured markdown above.
5. Emphasize: capture **failures and abandoned approaches**, not just successes.
6. Emphasize: "Remaining Work" must be concrete and actionable — not vague goals.

## 6. `/handoff` Command Interface

```
/handoff                  Generate handoff, start new session, auto-continue
/handoff --review         Generate handoff, start new session, wait for user
/handoff --dry-run        Generate handoff doc only, don't start new session
/handoff <slug>           Generate with a custom filename slug
```

**Default flow (`/handoff`):**

1. Validate: at least 2 messages in conversation (otherwise refuse).
2. Reply: "Generating handoff document…"
3. Spawn async task (non-blocking — UI stays responsive).
4. Async task completes → new session started → auto-continue prompt submitted.

**`--review` flow:**

Same as default but does NOT auto-submit. Instead replies:
```
Handoff written to .oxicode/handoffs/{filename}.
New session started. Press Enter or type a prompt to continue.
```

**`--dry-run` flow:**

Generates the doc but does NOT start a new session. Useful for reviewing the
handoff before committing to the cutover.

## 7. Technical Design

### 7.1 `HandoffCommand` (slash command)

```rust
// tui_vt/slash/handoff.rs
struct HandoffCommand;

impl SlashCommand for HandoffCommand {
    fn name(&self) -> &'static str { "handoff" }
    fn aliases(&self) -> &'static [&'static str] { &["hd"] }
    fn description(&self) -> &'static str {
        "Generate a handoff document and start a fresh session (alias: /hd)"
    }
    fn execute(&self, args: &str, ctx: &mut SlashCtx<'_>) -> SlashOutcome {
        // Parse flags: --review, --dry-run, <slug>
        // Validate message count
        // Clone session handle
        // Reply "Generating handoff…"
        // tokio::spawn(async move { generate_and_handoff(session, opts).await })
        // SlashOutcome::Handled
    }
}
```

Registered in `register_all()` alongside other commands.

### 7.2 `generate_handoff_doc` (core logic)

```rust
// app/handoff.rs
pub async fn generate_handoff_doc(
    session: &AgentSessionHandle,
    slug: Option<&str>,
) -> Result<HandoffResult> {
    // 1. Read conversation
    let messages = session.messages();
    let model_id = session.model_id();

    // 2. Gather git state
    let git_state = gather_git_state(session.cwd());

    // 3. Detect chain
    let chain = detect_chain(session.cwd());

    // 4. Build prompt
    let prompt = build_handoff_prompt(&messages, &git_state, &chain);

    // 5. Resolve model
    let model = resolve_model(&model_id)?;

    // 6. LLM call (one-shot complete)
    let doc = complete_handoff_llm_call(&model, &prompt).await?;

    // 7. Write doc
    let path = write_handoff_doc(session.cwd(), &doc, slug, &chain)?;

    Ok(HandoffResult { doc_path: path, chain_seq: chain.seq })
}
```

The LLM call uses `oxicode_ai::high_level::complete()` — the same function
`LlmCompactor::summarize_with_llm` uses. It constructs a `Context` with a
handoff-specific system prompt and the conversation as the user message.

### 7.3 `start_new_session` (session swap)

New method on `AgentSession`:

```rust
// app/agent_session.rs
pub fn start_new_session(&self) {
    let cwd = self.cwd.clone();
    let session_dir = get_default_session_dir();
    let new_manager = SessionManager::create(&cwd, Some(&session_dir));

    *self.session_manager.write() = new_manager;
    *self.session_id.write() = self.session_manager.read().get_session_id();

    self.agent.reset();
    *self.overflow_recovery_attempted.write() = false;
    self.clear_queue();
}
```

This creates a new session file, updates the session ID, and clears the agent
state — a true fresh start.

### 7.4 `SessionEvent::HandoffComplete`

New event variant:

```rust
// app/agent_session.rs — SessionEvent enum
HandoffComplete {
    doc_path: String,
    auto_continue: bool,
}
```

Emitted by the async task after the doc is written and the new session is
started. The event loop catches it and:

1. Clears the transcript (`state.transcript.clear()`, `state.message_buffer.clear()`).
2. If `auto_continue`: sends the continuation prompt via `prompt_tx.send()`.

### 7.5 Event loop integration

In `run_event_loop`'s `session_rx` arm, add handling for `HandoffComplete`:

```rust
Some(event) = session_rx.recv() => {
    if let SessionEvent::HandoffComplete { doc_path, auto_continue } = &event {
        let mut s = state.lock();
        s.transcript.clear();
        s.message_buffer.clear();
        s.scroll_offset = usize::MAX;
        s.append_line(InlineMessageKind::Info,
            format!("Handoff written to {}. New session started.", doc_path));
        if *auto_continue {
            let prompt = format!(
                "Continue our work. Read the handoff document at {} \
                 and start with the first item in \"Remaining Work\".",
                doc_path
            );
            let _ = prompt_tx.send(prompt);
        }
    } else {
        handle_session_event(&mut state.lock(), handle, &event);
    }
}
```

### 7.6 Storage

- Directory: `.oxicode/handoffs/` (project-local, gitignored by default).
- Naming: `YYYY-MM-DD-HHMMSS-{slug}.md`
- Slug: derived from the first user message or the custom arg; kebab-case,
  max 40 chars.
- Chain detection: scan `.oxicode/handoffs/*.md` for the most recent file,
  read its `Chain: #N` header, increment.

### 7.7 Git state gathering

Run `git rev-parse --abbrev-ref HEAD`, `git log --oneline -5`, and
`git diff --stat` via `std::process::Command` (same as the agent's bash tool
would, but synchronously — this is fast). Fall back gracefully if not a git
repo.

## 8. Error Handling

| Condition | Behavior |
|---|---|
| < 2 messages | Refuse: "Not enough conversation to hand off." |
| LLM call fails | Reply error, do NOT start new session, preserve current session |
| File write fails | Reply error, do NOT start new session |
| Not a git repo | Omit git state sections (non-fatal) |
| Agent is streaming | Refuse: "Cannot hand off while agent is running. /cancel first." |

## 9. `.gitignore` entry

Add `.oxicode/handoffs/` to `.gitignore` (handoff docs are local working state,
not committed artifacts — same as sessions).

## 10. Out of Scope (v1)

Deferred to future iterations:

- **Proactive auto-trigger** (token threshold) — not yet; v1 is manual only.
- **Validation scripts** (secret detection, quality score) — the LLM generates
  the doc; validation can be added later.
- **Staleness detection** — checking if the handoff is still current.
- **Cross-agent handoff** — this is same-agent (oxicode → oxicode) only.
- **Handoff doc editing UI** — the doc is a plain markdown file; users edit it
  with their editor.
- **Multi-pass map-reduce** for very large conversations — v1 sends the full
  conversation to the LLM in one shot. For 500K+ token conversations, this may
  need chunking (can be added later, following claude-handoff's approach).

## 11. Testing

- **Unit test:** `generate_handoff_doc` with a mock provider — verify the
  prompt is constructed correctly and the output is written.
- **Unit test:** `start_new_session` — verify session ID changes, agent state
  is cleared, new file is created.
- **Unit test:** Chain detection — verify sequence numbering and linking.
- **Integration test:** Full `/handoff` flow with a mock provider — verify
  doc generation, session swap, and event emission.
- **Manual smoke test:** Run the TUI, have a conversation, `/handoff`, verify
  new session starts and reads the doc.
