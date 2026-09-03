use super::path_security::PathGuard;
/// Grep tool - search files for patterns
use super::{AgentTool, AgentToolResult, ToolContext, ToolError};
use async_trait::async_trait;
use regex::RegexBuilder;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::oneshot;

/// Maximum characters per line in grep output
const GREP_MAX_LINE_LENGTH: usize = 500;

/// Truncate a single line to max characters, adding "... <kbd>truncated</kbd>" suffix.
fn truncate_line(line: &str) -> (String, bool) {
    if line.len() <= GREP_MAX_LINE_LENGTH {
        (line.to_string(), false)
    } else {
        (
            format!("{}... [truncated]", &line[..GREP_MAX_LINE_LENGTH]),
            true,
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Engine {
    Native,
    Legacy,
}

/// GrepTool.
pub struct GrepTool {
    root_dir: Option<PathBuf>,
    engine: Engine,
}

impl GrepTool {
    /// Create with no explicit root (uses ToolContext.workspace_dir at runtime).
    pub fn new() -> Self {
        Self {
            root_dir: None,
            engine: Engine::Native,
        }
    }

    /// Create with a specific working directory (overrides ToolContext).
    pub fn with_cwd(cwd: PathBuf) -> Self {
        Self {
            root_dir: Some(cwd),
            engine: Engine::Native,
        }
    }

    /// Legacy walker (pre-v2 semantics). Migration-window only: used by
    /// `ToolRegistry::with_builtins_cwd`; removal targeted 0.83.0.
    pub fn legacy_with_cwd(cwd: PathBuf) -> Self {
        Self {
            root_dir: Some(cwd),
            engine: Engine::Legacy,
        }
    }
}

/// Pre-v2 recursive walker. Behavior is frozen (byte-identical to v1);
/// only reachable via [`GrepTool::legacy_with_cwd`] during the migration
/// window (removal targeted 0.83.0).
pub(crate) mod legacy {
    use super::{PathGuard, RegexBuilder, ToolError, truncate_line};
    use std::path::Path;
    use tokio::fs;

    impl super::GrepTool {
        /// Check if a filename matches a simple glob pattern like "*.rs", "*.ts"
        fn matches_glob(file_name: &str, pattern: &str) -> bool {
            if let Some(ext) = pattern.strip_prefix("*.") {
                file_name.ends_with(ext)
            } else if pattern.contains('*') {
                // Simple wildcard matching
                let parts: Vec<&str> = pattern.split('*').collect();
                if parts.len() == 2 {
                    file_name.starts_with(parts[0]) && file_name.ends_with(parts[1])
                } else {
                    file_name == pattern
                }
            } else {
                file_name == pattern
            }
        }

        #[allow(clippy::type_complexity)]
        #[allow(clippy::too_many_arguments)]
        pub(super) async fn grep_impl(
            root_dir: &Path,
            pattern: &str,
            path: &str,
            case_insensitive: bool,
            literal: bool,
            context_before: usize,
            context_after: usize,
            include: Option<&str>,
            max_results: usize,
        ) -> Result<(String, bool), ToolError> {
            // Security: validate path with PathGuard
            let guard = PathGuard::new(root_dir);
            let root = guard
                .validate_traversal(Path::new(path))
                .map_err(|e| e.to_string())?;

            if !root.exists() {
                return Err(format!("Path not found: {}", path));
            }

            // Escape the pattern for literal matching if needed
            let pattern = if literal {
                regex::escape(pattern)
            } else {
                pattern.to_string()
            };

            let re = RegexBuilder::new(&pattern)
                .case_insensitive(case_insensitive)
                .build()
                .map_err(|e| format!("Invalid pattern '{}': {}", pattern, e))?;

            let mut matches: Vec<String> = Vec::new();
            let mut lines_truncated = false;
            Self::grep_walk(
                &root,
                &root,
                &re,
                include,
                context_before,
                context_after,
                max_results,
                &mut matches,
                &mut lines_truncated,
            )
            .await?;

            if matches.is_empty() {
                Ok(("No matches found".to_string(), false))
            } else {
                let header = format!("Found {} matches:\n", matches.len());
                Ok((header + &matches.join("\n"), lines_truncated))
            }
        }

        /// Read a file and return lines as a vector
        async fn read_file_lines(path: &Path) -> Result<Vec<String>, ToolError> {
            match fs::read_to_string(path).await {
                Ok(content) => {
                    // Normalize line endings: replace CRLF and standalone CR with LF, then split
                    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
                    Ok(normalized.lines().map(|s| s.to_string()).collect())
                }
                Err(e) => Err(format!("Cannot read file: {}", e)),
            }
        }

        #[allow(clippy::too_many_arguments)]
        async fn grep_walk(
            root: &Path,
            current: &Path,
            re: &regex::Regex,
            include: Option<&str>,
            context_before: usize,
            context_after: usize,
            max_results: usize,
            matches: &mut Vec<String>,
            lines_truncated: &mut bool,
        ) -> Result<(), ToolError> {
            if matches.len() >= max_results {
                return Ok(());
            }

            // Detect and skip broken symlinks - they cause read_dir to fail
            // and should not cause the entire search to fail
            if current
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
                && !current.exists()
            {
                return Ok(());
            }

            if current.is_file() {
                // Check include filter
                if let Some(glob) = include {
                    let file_name = current
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if !Self::matches_glob(&file_name, glob) {
                        return Ok(());
                    }
                }

                // Try to read and search the file
                match Self::read_file_lines(current).await {
                    Ok(lines) => {
                        let relative = current.strip_prefix(root).unwrap_or(current).display();

                        for (i, line) in lines.iter().enumerate() {
                            if re.is_match(line) {
                                // Check if adding this match would exceed max_results
                                // We may need to add context lines too
                                let context_lines_count = if context_before > 0 || context_after > 0
                                {
                                    let start = if context_before > 0 {
                                        i.saturating_sub(context_before)
                                    } else {
                                        i
                                    };
                                    let end = std::cmp::min(lines.len(), i + context_after + 1);
                                    end - start
                                } else {
                                    1
                                };

                                if matches.len() + context_lines_count > max_results {
                                    // Can't add this match with its context, stop
                                    return Ok(());
                                }

                                // Add context lines before match
                                if context_before > 0 && i > 0 {
                                    let start = i.saturating_sub(context_before);
                                    for (j, context_line) in
                                        lines.iter().enumerate().take(i).skip(start)
                                    {
                                        let (truncated_text, was_truncated) =
                                            truncate_line(context_line);
                                        if was_truncated {
                                            *lines_truncated = true;
                                        }
                                        matches.push(format!(
                                            "{}-{}- {}",
                                            relative,
                                            j + 1,
                                            truncated_text
                                        ));
                                    }
                                }

                                // Add the match line
                                let (truncated_text, was_truncated) = truncate_line(line);
                                if was_truncated {
                                    *lines_truncated = true;
                                }
                                matches.push(format!("{}:{}: {}", relative, i + 1, truncated_text));

                                // Add context lines after match
                                if context_after > 0 {
                                    let end = std::cmp::min(lines.len(), i + context_after + 1);
                                    for (j, context_line) in
                                        lines.iter().enumerate().take(end).skip(i + 1)
                                    {
                                        let (truncated_text, was_truncated) =
                                            truncate_line(context_line);
                                        if was_truncated {
                                            *lines_truncated = true;
                                        }
                                        matches.push(format!(
                                            "{}-{}- {}",
                                            relative,
                                            j + 1,
                                            truncated_text
                                        ));
                                    }
                                }

                                if matches.len() >= max_results {
                                    return Ok(());
                                }
                            }
                        }
                    }
                    Err(_) => {
                        // Skip files we can't read (binary, permissions, etc.)
                    }
                }
                return Ok(());
            }

            // Directory: walk entries
            let mut entries = fs::read_dir(current)
                .await
                .map_err(|e| format!("Cannot read directory {}: {}", current.display(), e))?;

            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|e| format!("Error reading entry: {}", e))?
            {
                let entry_path = entry.path();

                // Skip hidden files/dirs
                if entry_path
                    .file_name()
                    .map(|n| n.to_string_lossy().starts_with('.'))
                    .unwrap_or(false)
                {
                    continue;
                }

                // Skip common non-searchable dirs
                if entry_path.is_dir() {
                    let dir_name = entry_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if matches!(
                        dir_name.as_str(),
                        "node_modules"
                            | "target"
                            | ".git"
                            | "dist"
                            | "build"
                            | "__pycache__"
                            | ".venv"
                            | "venv"
                    ) {
                        continue;
                    }
                }

                Box::pin(Self::grep_walk(
                    root,
                    &entry_path,
                    re,
                    include,
                    context_before,
                    context_after,
                    max_results,
                    matches,
                    lines_truncated,
                ))
                .await?;
            }

            Ok(())
        }
    }
}

impl Default for GrepTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn label(&self) -> &str {
        "Grep"
    }

    fn essential(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        "Search files for a pattern. Returns matching lines with file paths and line numbers. Use literal=true to treat pattern as a literal string. Use context=n to show n lines before and after matches. Long lines are truncated to 500 chars."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The pattern to search for (regex by default, or literal string if literal=true)"
                },
                "path": {
                    "type": "string",
                    "description": "The directory or file to search in",
                    "default": "."
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "If true, perform case-insensitive search",
                    "default": false
                },
                "literal": {
                    "type": "boolean",
                    "description": "If true, treat pattern as a literal string instead of regex",
                    "default": false
                },
                "context": {
                    "type": "integer",
                    "description": "Number of lines to show before and after each match",
                    "default": 0
                },
                "include": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g., '*.rs', '*.ts')"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of results to return",
                    "default": 100
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: Option<oneshot::Receiver<()>>,
        ctx: &ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let pattern = params
            .get("pattern")
            .and_then(|v: &Value| v.as_str())
            .ok_or_else(|| "Missing required parameter: pattern".to_string())?;

        let path = params
            .get("path")
            .and_then(|v: &Value| v.as_str())
            .unwrap_or(".");

        let case_insensitive = params
            .get("case_insensitive")
            .and_then(|v: &Value| v.as_bool())
            .unwrap_or(false);

        let literal = params
            .get("literal")
            .and_then(|v: &Value| v.as_bool())
            .unwrap_or(false);

        let context = params
            .get("context")
            .and_then(|v: &Value| v.as_u64())
            .unwrap_or(0) as usize;

        let include = params.get("include").and_then(|v: &Value| v.as_str());

        let max_results = params
            .get("max_results")
            .and_then(|v: &Value| v.as_u64())
            .unwrap_or(100) as usize;

        // ── Internal URL dispatch ──
        // If the input looks like an internal URL (scheme://…), resolve
        // content and search in-memory instead of touching the filesystem.
        if let Some(ref resolver) = ctx.url_resolver
            && resolver.can_resolve(path)
        {
            let resolved = resolver.resolve(path).await?;

            // Build the regex (same as grep_impl)
            let pattern_str = if literal {
                regex::escape(pattern)
            } else {
                pattern.to_string()
            };
            let re = RegexBuilder::new(&pattern_str)
                .case_insensitive(case_insensitive)
                .build()
                .map_err(|e| format!("Invalid pattern '{}': {}", pattern, e))?;

            let lines: Vec<&str> = resolved.content.lines().collect();
            let mut matches: Vec<String> = Vec::new();
            let mut lines_truncated = false;

            for (i, line) in lines.iter().enumerate() {
                if matches.len() >= max_results {
                    break;
                }
                if re.is_match(line) {
                    // Context before
                    if context > 0 && i > 0 {
                        let start = i.saturating_sub(context);
                        for (j, ctx_line) in lines.iter().enumerate().take(i).skip(start) {
                            let (truncated, was_trunc) = truncate_line(ctx_line);
                            if was_trunc {
                                lines_truncated = true;
                            }
                            matches.push(format!("{}-{}- {}", path, j + 1, truncated));
                        }
                    }

                    // The match line
                    let (truncated, was_trunc) = truncate_line(line);
                    if was_trunc {
                        lines_truncated = true;
                    }
                    matches.push(format!("{}:{}: {}", path, i + 1, truncated));

                    // Context after
                    if context > 0 {
                        let end = std::cmp::min(lines.len(), i + context + 1);
                        for (j, ctx_line) in lines.iter().enumerate().take(end).skip(i + 1) {
                            let (truncated, was_trunc) = truncate_line(ctx_line);
                            if was_trunc {
                                lines_truncated = true;
                            }
                            matches.push(format!("{}-{}- {}", path, j + 1, truncated));
                        }
                    }

                    if matches.len() >= max_results {
                        break;
                    }
                }
            }

            let output = if matches.is_empty() {
                "No matches found".to_string()
            } else {
                format!("Found {} matches:\n{}", matches.len(), matches.join("\n"))
            };

            let mut result = AgentToolResult::success(output);
            if lines_truncated {
                result.metadata = Some(json!({
                    "lines_truncated": true,
                    "message": "Some lines truncated to 500 chars. Use read tool to see full lines."
                }));
            }
            return Ok(result);
        }

        // Use root_dir if set, else ctx.root()
        let root = self.root_dir.as_deref().unwrap_or(ctx.root());

        if self.engine == Engine::Legacy {
            // unchanged legacy call
            return match Self::grep_impl(
                root,
                pattern,
                path,
                case_insensitive,
                literal,
                context,
                context,
                include,
                max_results,
            )
            .await
            {
                Ok((output, lines_truncated)) => {
                    let mut result = AgentToolResult::success(output);
                    if lines_truncated {
                        result.metadata = Some(json!({
                            "lines_truncated": true,
                            "message": "Some lines truncated to 500 chars. Use read tool to see full lines."
                        }));
                    }
                    Ok(result)
                }
                Err(e) => Ok(AgentToolResult::error(e)),
            };
        }

        // v2: the model-supplied path resolves against the search root
        // (with_cwd / workspace), not the process cwd. Absolute `path`
        // values pass through unchanged (Path::join semantics).
        let target = root.join(path);
        let guard = PathGuard::new(root);
        let validated = match guard.validate_traversal(&target) {
            Ok(v) => v,
            // Traversal attempts surface as an error result (v1 parity),
            // never as an Err from `execute`.
            Err(e) => return Ok(AgentToolResult::error(e.to_string())),
        };
        if !validated.exists() {
            return Ok(AgentToolResult::error(format!("Path not found: {}", path)));
        }
        let request = super::exact_search::SearchRequest {
            pattern: pattern.to_string(),
            case_insensitive,
            literal,
            context,
            include: include.map(str::to_string),
            max_results,
        };
        let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if let Some(rx) = _signal {
            let flag = cancel_flag.clone();
            tokio::spawn(async move {
                let _ = rx.await; // send OR sender-drop = abort request
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            });
        }
        let vroot = validated.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            super::exact_search::search(&vroot, &request, cancel_flag)
        })
        .await
        .map_err(|e| format!("search task failed: {e}"))?;
        match outcome {
            Ok(o) => {
                let mut result = AgentToolResult::success(o.output);
                if o.lines_truncated || o.cancelled {
                    result.metadata = Some(json!({
                        "lines_truncated": o.lines_truncated,
                        "cancelled": o.cancelled,
                    }));
                }
                Ok(result)
            }
            Err(e) => Ok(AgentToolResult::error(e)),
        }
    }
}
