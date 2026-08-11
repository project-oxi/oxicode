//! Handoff prompt construction — builds the LLM prompt that mines the current
//! conversation into a structured handoff document.
//!
//! Also provides git-state gathering and handoff-chain detection.

use std::path::Path;
use std::process::Command;

use oxicode_ai::Message;

// ─────────────────────────────────────────────────────────────────────────
// Git state
// ─────────────────────────────────────────────────────────────────────────

/// Git repository state captured for handoff context.
#[derive(Debug, Clone)]
pub struct GitState {
    pub branch: Option<String>,
    pub recent_commits: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Gather git state by running `git` in `cwd`. Non-fatal — returns
/// `branch: None` when not in a repo or git is unavailable.
pub fn gather_git_state(cwd: &str) -> GitState {
    let branch =
        git_output(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).map(|s| s.trim().to_string());

    let recent_commits = git_output(cwd, &["log", "--oneline", "-5"])
        .map(|s| {
            s.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let modified_files = git_output(cwd, &["diff", "--stat"])
        .map(|s| {
            s.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default();

    GitState {
        branch,
        recent_commits,
        modified_files,
    }
}

fn git_output(cwd: &str, args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
}

// ─────────────────────────────────────────────────────────────────────────
// Handoff chain
// ─────────────────────────────────────────────────────────────────────────

/// Chain-linking metadata for multi-handoff continuity.
#[derive(Debug, Clone)]
pub struct HandoffChain {
    /// Sequence number (1 for the first handoff, 2 for the next, …).
    pub seq: usize,
    /// Path to the previous handoff doc, if any.
    pub prev_path: Option<String>,
}

/// Detect the handoff chain by scanning `handoffs_dir` for existing files.
/// Returns `seq: 1, prev_path: None` if no previous handoffs exist.
pub fn detect_handoff_chain(handoffs_dir: &Path) -> HandoffChain {
    let mut entries: Vec<(i64, std::path::PathBuf)> = Vec::new();

    if let Ok(rd) = std::fs::read_dir(handoffs_dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md")
                && let Ok(meta) = entry.metadata()
            {
                let mtime_ms = meta
                    .modified()
                    .unwrap_or(std::time::UNIX_EPOCH)
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                entries.push((mtime_ms, path));
            }
        }
    }

    if entries.is_empty() {
        return HandoffChain {
            seq: 1,
            prev_path: None,
        };
    }

    // Most recently modified file (descending sort).
    entries.sort_by_key(|&(ms, _)| std::cmp::Reverse(ms));
    let latest = &entries[0].1;

    // Read its chain sequence number.
    let prev_seq = std::fs::read_to_string(latest)
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find_map(|l| l.strip_prefix("> **Chain:** #"))
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|n| n.parse::<usize>().ok())
        })
        .unwrap_or(0);

    HandoffChain {
        seq: prev_seq + 1,
        prev_path: latest.to_str().map(|s| s.to_string()),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Prompt builder
// ─────────────────────────────────────────────────────────────────────────

/// Build the system prompt for the handoff-generating LLM call.
pub fn handoff_system_prompt() -> &'static str {
    "You are a session-handoff document generator for an AI coding agent. \
     Your job is to mine a conversation and produce a structured markdown \
     document that lets a fresh agent continue the work with zero ambiguity."
}

/// Maximum characters of a single message's text to include in the prompt.
/// Tool results can be very long; this keeps the total prompt manageable.
const MAX_MSG_CHARS: usize = 3000;

/// Build the user-message prompt containing the conversation and instructions.
///
/// `messages` — the full conversation. Each message is formatted with its role
/// and text content (truncated to `MAX_MSG_CHARS` to keep the prompt manageable).
pub fn build_handoff_prompt(
    messages: &[Message],
    git_state: &GitState,
    chain: &HandoffChain,
) -> String {
    let mut prompt = String::with_capacity(8 * 1024);

    // ── Instructions ───────────────────────────────────────────────────
    prompt.push_str(INSTRUCTIONS);

    // ── Chain context ──────────────────────────────────────────────────
    if let Some(prev) = &chain.prev_path {
        prompt.push_str(&format!(
            "\n**Chain context:** This is handoff #{} in a chain. \
             The previous handoff is at `{}`. Reference it if the current \
             conversation builds on earlier work.\n",
            chain.seq, prev
        ));
    }

    // ── Git state ──────────────────────────────────────────────────────
    prompt.push_str("\n## Git State\n\n");
    if let Some(branch) = &git_state.branch {
        prompt.push_str(&format!("**Branch:** `{}`\n\n", branch));
    } else {
        prompt.push_str("(not a git repository)\n\n");
    }
    if !git_state.recent_commits.is_empty() {
        prompt.push_str("**Recent commits:**\n```\n");
        for commit in &git_state.recent_commits {
            prompt.push_str(commit);
            prompt.push('\n');
        }
        prompt.push_str("```\n\n");
    }
    if !git_state.modified_files.is_empty() {
        prompt.push_str("**Modified files (uncommitted):**\n```\n");
        for f in &git_state.modified_files {
            prompt.push_str(f);
            prompt.push('\n');
        }
        prompt.push_str("```\n\n");
    }

    // ── Conversation ───────────────────────────────────────────────────
    prompt.push_str("## Conversation\n\n");
    prompt.push_str("Below is the full conversation to mine for the handoff:\n\n");

    for msg in messages {
        let role = match msg {
            Message::User(_) => "User",
            Message::Assistant(_) => "Assistant",
            Message::ToolResult(_) => "Tool",
        };
        let content = msg.text_content().unwrap_or_default();
        let content = truncate_msg(&content, MAX_MSG_CHARS);
        prompt.push_str(&format!("[{}]: {}\n\n", role, content));
    }

    prompt.push_str("---\n\n");
    prompt.push_str(OUTPUT_TEMPLATE);

    prompt
}

fn truncate_msg(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let boundary = s
        .char_indices()
        .take_while(|(i, _)| *i <= max)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(max);
    format!("{}... [truncated]", &s[..boundary])
}

const INSTRUCTIONS: &str = "\
You are writing a **session handoff document** — a structured markdown file that \
captures everything a fresh AI agent needs to continue the current work without \
re-exploring or re-discovering what was already done.

Read the conversation below carefully and produce the handoff document following \
the output template at the end of this message.

**Critical guidelines:**

1. **\"What Was Tried\" is the most valuable section.** Capture every approach \
attempted — especially failures and abandoned approaches. These are the most \
expensive things to rediscover. Include what was tried, what happened, and why \
it was kept or abandoned.

2. **\"Remaining Work\" must be concrete and actionable.** Not vague goals like \
\"improve performance\" — specific tasks like \"Add retry logic to the HTTP client \
in `src/client.rs:45`\". Order them by priority.

3. **\"Key Decisions\" should record what was chosen AND what was rejected, with \
reasoning.** Future sessions need to understand why, not just what.

4. **Include specific file paths and line numbers** wherever relevant.

5. **Be selective.** Exclude conversational filler, repetitions, and digressions. \
Include only what matters for continuing the work.

6. **Write the document as if the reader has zero context** — because they do.";

const OUTPUT_TEMPLATE: &str = "\
## Output Template

Produce the handoff document in this exact structure (fill in all sections; \
omit a section only if truly nothing applies):

```markdown
# Session Handoff — {brief title}

## Goal

{What we're working on and why — 1-2 paragraphs}

## Current State

{Bullet list of concrete accomplishments and current state of work}

## What Was Tried

{Every approach attempted, including failures. This is the most valuable section.}

### {Approach title}
- **What:** what was attempted
- **Result:** what happened
- **Verdict:** kept | abandoned (and why)

## Key Decisions

{Architectural choices with rationale. What was chosen AND what was rejected.}

## Remaining Work

1. {First next step — concrete, with file paths if relevant}
2. {Second next step}
3. ...

## Critical Files

| Path | Role |
|------|------|
| `path/to/file` | what it does and why it matters |

## Gotchas & Risks

- {Known issues, pitfalls, things to watch out for}
```

Write the complete handoff document now. Output ONLY the markdown document — \
no preamble, no explanation, no closing remarks.";

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oxicode_ai::Message;

    fn user_msg(text: &str) -> Message {
        Message::user(text)
    }

    #[test]
    fn build_handoff_prompt_includes_instructions() {
        let messages = vec![user_msg("hello")];
        let git_state = GitState {
            branch: Some("main".into()),
            recent_commits: vec!["abc1234 init".into()],
            modified_files: vec![],
        };
        let chain = HandoffChain {
            seq: 1,
            prev_path: None,
        };

        let prompt = build_handoff_prompt(&messages, &git_state, &chain);
        assert!(prompt.contains("session handoff document"));
        assert!(prompt.contains("What Was Tried"));
        assert!(prompt.contains("[User]: hello"));
        assert!(prompt.contains("Branch"));
        assert!(prompt.contains("main"));
    }

    #[test]
    fn build_handoff_prompt_includes_chain_link() {
        let messages = vec![user_msg("test")];
        let git_state = GitState {
            branch: None,
            recent_commits: vec![],
            modified_files: vec![],
        };
        let chain = HandoffChain {
            seq: 3,
            prev_path: Some(".oxicode/handoffs/prev.md".into()),
        };

        let prompt = build_handoff_prompt(&messages, &git_state, &chain);
        assert!(prompt.contains("handoff #3"));
        assert!(prompt.contains("prev.md"));
    }

    #[test]
    fn truncate_msg_short_unchanged() {
        assert_eq!(truncate_msg("hello", 100), "hello");
    }

    #[test]
    fn truncate_msg_long_truncated() {
        let long = "x".repeat(5000);
        let result = truncate_msg(&long, 100);
        assert!(result.ends_with("[truncated]"));
        assert!(result.len() < long.len());
    }

    #[test]
    fn detect_chain_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let chain = detect_handoff_chain(tmp.path());
        assert_eq!(chain.seq, 1);
        assert!(chain.prev_path.is_none());
    }

    #[test]
    fn detect_chain_with_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("2026-08-12-100000-test.md");
        std::fs::write(&path, "# Handoff\n\n> **Chain:** #2\n").unwrap();

        let chain = detect_handoff_chain(tmp.path());
        assert_eq!(chain.seq, 3);
        assert!(chain.prev_path.is_some());
    }

    #[test]
    fn gather_git_state_in_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let state = gather_git_state(tmp.path().to_str().unwrap());
        // Not a git repo — branch should be None.
        assert!(state.branch.is_none());
    }
}
