//! Session handoff — generate a structured handoff document from the current
//! conversation, write it to disk, start a fresh session, and signal the event
//! loop to auto-continue.

use std::path::PathBuf;

use anyhow::{Context as _, Result};

use oxicode_ai::{Context, Message, StreamOptions, UserMessage, complete};

use crate::app::agent_session::AgentSessionHandle;
use crate::prompt::handoff_prompt::{
    build_handoff_prompt, detect_handoff_chain, gather_git_state, handoff_system_prompt,
};

/// Handoffs directory name (relative to the project cwd).
const HANDOFFS_DIR: &str = ".oxicode/handoffs";

/// Options for the handoff operation.
#[derive(Debug, Clone)]
pub struct HandoffOptions {
    /// Custom filename slug. If `None`, derived from the first user message.
    pub slug: Option<String>,
    /// If `true`, auto-submit a continuation prompt to the new session.
    /// If `false`, write the doc and start the new session but wait for user.
    pub auto_continue: bool,
    /// If `true`, write the doc but do NOT start a new session.
    pub dry_run: bool,
}

impl Default for HandoffOptions {
    fn default() -> Self {
        Self {
            slug: None,
            auto_continue: true,
            dry_run: false,
        }
    }
}

/// Generate a handoff document, write it, and (unless `dry_run`) start a new
/// session. Returns the path to the written document.
///
/// This is the core entry point called by the `/handoff` slash command's
/// spawned async task.
pub async fn generate_and_apply_handoff(
    session: &AgentSessionHandle,
    opts: &HandoffOptions,
) -> Result<String> {
    // 1. Read conversation
    let messages = session.messages();
    let model_id = session.model_id();
    let cwd = session.cwd().to_string();

    // 2. Gather git state
    let git_state = gather_git_state(&cwd);

    // 3. Detect chain
    let handoffs_dir = PathBuf::from(&cwd).join(HANDOFFS_DIR);
    let chain = detect_handoff_chain(&handoffs_dir);

    // 4. Build prompt
    let prompt = build_handoff_prompt(&messages, &git_state, &chain);

    // 5. Resolve model
    let model = oxicode_agent::model_id::resolve_model_from_id(&model_id).context(format!(
        "Failed to resolve model '{model_id}' for handoff generation"
    ))?;

    // 6. LLM call (one-shot complete)
    let mut llm_context = Context::new();
    llm_context.set_system_prompt(handoff_system_prompt());
    llm_context.add_message(Message::User(UserMessage::new(prompt)));

    let llm_options = StreamOptions {
        temperature: Some(0.3),
        max_tokens: Some(4096),
        ..Default::default()
    };

    let result = complete(&model, &llm_context, Some(llm_options))
        .await
        .context("Handoff LLM call failed")?;

    let doc_content = result.text_content();

    // 7. Write doc
    let slug = opts.slug.clone().unwrap_or_else(|| derive_slug(&messages));
    let doc_path = write_handoff_doc(&cwd, &doc_content, &slug, &chain)?;

    // 8. Start new session + emit event (unless dry-run)
    if !opts.dry_run {
        session.start_new_session();
        session.emit_handoff_complete(doc_path.clone(), opts.auto_continue);
    }

    Ok(doc_path)
}

/// Write the handoff document to `.oxicode/handoffs/YYYY-MM-DD-HHMMSS-{slug}.md`.
fn write_handoff_doc(
    cwd: &str,
    content: &str,
    slug: &str,
    chain: &crate::prompt::handoff_prompt::HandoffChain,
) -> Result<String> {
    let handoffs_dir = PathBuf::from(cwd).join(HANDOFFS_DIR);
    std::fs::create_dir_all(&handoffs_dir).context("Failed to create handoffs directory")?;

    let timestamp = chrono::Utc::now().format("%Y-%m-%d-%H%M%S");
    let filename = format!("{}-{}.md", timestamp, slug);
    let path = handoffs_dir.join(&filename);

    // Always prepend chain metadata so detect_handoff_chain can find it
    // in subsequent handoffs, regardless of LLM output formatting.
    let prev_link = chain
        .prev_path
        .as_ref()
        .map(|p| format!(" (continues from {})", p))
        .unwrap_or_default();
    let full_content = format!("> **Chain:** #{}{}\n\n{}", chain.seq, prev_link, content);

    std::fs::write(&path, &full_content).context("Failed to write handoff document")?;

    Ok(path.to_string_lossy().into_owned())
}

/// Derive a kebab-case slug from the first user message.
fn derive_slug(messages: &[Message]) -> String {
    let first_user = messages
        .iter()
        .find(|m| matches!(m, Message::User(_)))
        .map(|m| m.text_content().unwrap_or_default())
        .unwrap_or_else(|| "session".to_string());

    // Take first line, lowercase, replace non-alphanumeric with hyphens,
    // collapse consecutive hyphens, trim, truncate to 40 chars.
    let slug: String = first_user
        .lines()
        .next()
        .unwrap_or("session")
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let slug: String = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "session".to_string()
    } else {
        slug.chars().take(40).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_slug_basic() {
        let msgs = vec![Message::user("Fix the authentication bug in login")];
        let slug = derive_slug(&msgs);
        assert_eq!(slug, "fix-the-authentication-bug-in-login");
    }

    #[test]
    fn derive_slug_truncates() {
        let long = "x".repeat(100);
        let msgs = vec![Message::user(long)];
        let slug = derive_slug(&msgs);
        assert!(slug.len() <= 40);
    }

    #[test]
    fn derive_slug_no_messages() {
        let slug = derive_slug(&[]);
        assert_eq!(slug, "session");
    }

    #[test]
    fn derive_slug_special_chars() {
        let msgs = vec![Message::user("Hello, World!!! @#$%")];
        let slug = derive_slug(&msgs);
        assert_eq!(slug, "hello-world");
    }

    #[test]
    fn write_handoff_doc_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let chain = crate::prompt::handoff_prompt::HandoffChain {
            seq: 1,
            prev_path: None,
        };
        let path =
            write_handoff_doc(cwd, "# Session Handoff — test\n\nbody", "test", &chain).unwrap();
        assert!(std::path::Path::new(&path).exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Session Handoff"));
    }
}
