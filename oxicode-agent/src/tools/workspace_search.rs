//! `workspace_search` — optional indexed discovery tool.
//!
//! Stable two-parameter schema; retrieval is delegated to a
//! composition-root-supplied [`WorkspaceSearchBackend`] (oxibrain document
//! plane in the CLI). The tool owns policy: parameter caps, snippet bounds,
//! the envelope, and typed unavailable errors naming `grep`/`read`.

use super::{AgentTool, AgentToolResult, ToolContext, ToolError};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::pin::Pin;
use std::sync::Arc;

/// Passages returned when the model omits `limit`.
pub const DEFAULT_LIMIT: usize = 8;
/// Hard ceiling for `limit`; larger values are clamped to this.
pub const MAX_LIMIT: usize = 20;
const MAX_SNIPPET_CHARS: usize = 500;

/// One ranked chunk-level evidence item from the workspace index.
#[derive(Debug, Clone)]
pub struct SearchPassage {
    /// Workspace-root-relative file path.
    pub locator: String,
    /// Chunk index within the file.
    pub ordinal: u32,
    /// Index revision the chunk was built from.
    pub revision: String,
    /// Relevance score assigned by the backend.
    pub score: f64,
    /// Chunk text, pre-truncated by the renderer.
    pub snippet: String,
    /// File modification time in milliseconds since the epoch.
    pub modified_at_ms: i64,
}

/// One page of workspace-search results plus the reconcile report line.
#[derive(Debug, Clone)]
pub struct WorkspaceSearchPage {
    /// Ranked passages (most relevant first).
    pub passages: Vec<SearchPassage>,
    /// Human-readable reconcile report line, when the backend produced one.
    pub freshness: Option<String>,
}

/// Retrieval backend for [`WorkspaceSearchTool`]. The composition root
/// supplies the implementation (oxibrain document plane in the CLI).
pub trait WorkspaceSearchBackend: Send + Sync + std::fmt::Debug + 'static {
    /// Search the workspace index; returns at most `limit` passages.
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Result<WorkspaceSearchPage, ToolError>> + Send + 'a>>;
}

/// Agent tool facade over a [`WorkspaceSearchBackend`]. Owns policy: the
/// two-parameter schema, limit caps, snippet bounds, the result envelope,
/// and typed unavailable errors naming `grep`/`read`.
pub struct WorkspaceSearchTool {
    backend: Arc<dyn WorkspaceSearchBackend>,
}

impl WorkspaceSearchTool {
    /// Build the tool over the given backend.
    pub fn new(backend: Arc<dyn WorkspaceSearchBackend>) -> Self {
        Self { backend }
    }
}

fn truncate_snippet(text: &str) -> String {
    if text.len() <= MAX_SNIPPET_CHARS {
        return text.to_string();
    }
    let mut end = MAX_SNIPPET_CHARS;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

fn render(page: &WorkspaceSearchPage) -> String {
    if page.passages.is_empty() {
        return "No relevant passages found. Use grep for exact search or read \
                to inspect files directly."
            .to_string();
    }
    let mut out = format!("Found {} ranked passages:\n", page.passages.len());
    for (i, p) in page.passages.iter().enumerate() {
        out.push_str(&format!(
            "\n{}. {}#{} (score {:.3}, rev {})\n   {}\n",
            i + 1,
            p.locator,
            p.ordinal,
            p.score,
            p.revision,
            truncate_snippet(&p.snippet)
        ));
    }
    if let Some(f) = &page.freshness {
        out.push_str(&format!("\nfreshness: {f}\n"));
    }
    out.push_str("\nVerify each passage with read or grep before relying on it.");
    out
}

#[async_trait]
impl AgentTool for WorkspaceSearchTool {
    fn name(&self) -> &str {
        "workspace_search"
    }

    fn label(&self) -> &str {
        "Workspace Search"
    }

    fn essential(&self) -> bool {
        false
    }

    fn description(&self) -> &str {
        "Semantic discovery over the workspace index: find relevant source \
         passages when the wording or location is unknown. Results are ranked \
         chunk-level evidence (file + excerpt), not exact matches. Use grep \
         for exact identifiers, paths, or regular expressions."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language query describing what to find"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum passages to return (default 8, max 20)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: Option<tokio::sync::oneshot::Receiver<()>>,
        _ctx: &ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter: query".to_string())?;
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v.min(MAX_LIMIT as u64) as usize)
            .unwrap_or(DEFAULT_LIMIT);

        let page = self.backend.search(query, limit).await?;
        Ok(AgentToolResult::success(render(&page)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    #[derive(Debug)]
    struct FakeBackend {
        page: Mutex<Option<WorkspaceSearchPage>>,
        seen_limit: Mutex<Vec<usize>>,
        fail: bool,
    }

    impl WorkspaceSearchBackend for FakeBackend {
        fn search<'a>(
            &'a self,
            query: &'a str,
            limit: usize,
        ) -> Pin<Box<dyn Future<Output = Result<WorkspaceSearchPage, ToolError>> + Send + 'a>>
        {
            Box::pin(async move {
                if self.fail {
                    return Err("workspace_search is unavailable: backend offline. \
                                Use grep for exact search and read to inspect files."
                        .to_string());
                }
                self.seen_limit.lock().push(limit);
                let _ = query;
                let page = self.page.lock().clone().unwrap_or(WorkspaceSearchPage {
                    passages: vec![],
                    freshness: None,
                });
                Ok(page)
            })
        }
    }

    fn page() -> WorkspaceSearchPage {
        WorkspaceSearchPage {
            passages: vec![SearchPassage {
                locator: "src/auth.rs".into(),
                ordinal: 3,
                revision: "abc".into(),
                score: 0.912,
                snippet: "ownership_session_id".repeat(80), // long
                modified_at_ms: 1,
            }],
            freshness: Some("reconciled: ws-oxi (12 files)".into()),
        }
    }

    #[tokio::test]
    async fn limit_is_capped_at_max_and_defaults_to_eight() {
        let backend = Arc::new(FakeBackend {
            page: Mutex::new(None),
            seen_limit: Mutex::new(vec![]),
            fail: false,
        });
        let tool = WorkspaceSearchTool::new(backend.clone());
        let ctx = ToolContext::new("/tmp");
        tool.execute("t", json!({"query": "x", "limit": 500}), None, &ctx)
            .await
            .unwrap();
        tool.execute("t", json!({"query": "x"}), None, &ctx)
            .await
            .unwrap();
        let seen = backend.seen_limit.lock().clone();
        assert_eq!(seen, vec![20, 8], "cap at 20, default 8");
    }

    #[tokio::test]
    async fn empty_page_names_alternatives() {
        let tool = WorkspaceSearchTool::new(Arc::new(FakeBackend {
            page: Mutex::new(Some(WorkspaceSearchPage {
                passages: vec![],
                freshness: None,
            })),
            seen_limit: Mutex::new(vec![]),
            fail: false,
        }));
        let ctx = ToolContext::new("/tmp");
        let out = tool
            .execute("t", json!({"query": "x"}), None, &ctx)
            .await
            .unwrap();
        let text = out.output;
        assert!(text.contains("Use grep for exact search"), "{text}");
    }

    #[tokio::test]
    async fn backend_failure_surfaces_typed_error() {
        let tool = WorkspaceSearchTool::new(Arc::new(FakeBackend {
            page: Mutex::new(None),
            seen_limit: Mutex::new(vec![]),
            fail: true,
        }));
        let ctx = ToolContext::new("/tmp");
        let err = tool
            .execute("t", json!({"query": "x"}), None, &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("unavailable") && err.contains("grep"), "{err}");
    }

    #[tokio::test]
    async fn rendered_page_truncates_snippet_and_reports_freshness() {
        let tool = WorkspaceSearchTool::new(Arc::new(FakeBackend {
            page: Mutex::new(Some(page())),
            seen_limit: Mutex::new(vec![]),
            fail: false,
        }));
        let ctx = ToolContext::new("/tmp");
        let out = tool
            .execute("t", json!({"query": "x"}), None, &ctx)
            .await
            .unwrap();
        let text = out.output;
        assert!(text.contains("Found 1 ranked passages"), "{text}");
        assert!(text.contains("src/auth.rs#3"), "{text}");
        assert!(text.contains("(score 0.912, rev abc)"), "{text}");
        // The oversized snippet is truncated to the 500-char bound + marker.
        assert!(!text.contains(&"ownership_session_id".repeat(80)), "{text}");
        assert_eq!(
            text.matches("...").count(),
            1,
            "exactly one truncation marker"
        );
        assert!(
            text.contains("freshness: reconciled: ws-oxi (12 files)"),
            "{text}"
        );
        assert!(
            text.contains("Verify each passage with read or grep"),
            "{text}"
        );
    }
}
