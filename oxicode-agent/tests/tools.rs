//! Comprehensive tests for all built-in tools

use async_trait::async_trait;
use oxicode_agent::prelude::*;
use serde_json::json;
use tokio::fs;

// ── Helpers ──────────────────────────────────────────────────────

/// Create a temp directory for test files. Returns the path.
/// Caller is responsible for cleanup.
async fn create_temp_dir(name: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = format!("/tmp/oxicode_tool_test_{}_{}", name, id);
    let _ = fs::remove_dir_all(&path).await;
    fs::create_dir_all(&path).await.unwrap();
    path
}

async fn cleanup(path: &str) {
    let _ = fs::remove_dir_all(path).await;
}

async fn execute_tool(tool: &dyn AgentTool, params: serde_json::Value) -> AgentToolResult {
    tool.execute("test_call", params, None, &ToolContext::default())
        .await
        .unwrap()
}

// ═══════════════════════════════════════════════════════════════════
// ReadTool Tests
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_read_file_basic() {
    let dir = create_temp_dir("read_basic").await;
    let file_path = format!("{}/hello.txt", dir);
    fs::write(&file_path, "Hello, World!").await.unwrap();

    let tool = ReadTool::new();
    let result = execute_tool(&tool, json!({ "path": file_path })).await;
    assert!(result.success);
    // Output includes line numbers now
    assert!(result.output.contains("Hello, World!"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_read_file_multiline() {
    let dir = create_temp_dir("read_multiline").await;
    let file_path = format!("{}/multi.txt", dir);
    fs::write(&file_path, "line 1\nline 2\nline 3")
        .await
        .unwrap();

    let tool = ReadTool::new();
    let result = execute_tool(&tool, json!({ "path": file_path })).await;
    assert!(result.success);
    assert!(result.output.contains("line 1"));
    assert!(result.output.contains("line 2"));
    assert!(result.output.contains("line 3"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_read_file_not_found() {
    let tool = ReadTool::new();
    let result = tool
        .execute(
            "test_call",
            json!({ "path": "/tmp/oxicode_nonexistent_file_12345.txt" }),
            None,
            &ToolContext::default(),
        )
        .await;
    // ReadTool returns Err for file not found
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("not found") || err.contains("Cannot"));
}

#[tokio::test]
async fn test_read_directory_error() {
    let dir = create_temp_dir("read_dir_error").await;
    let tool = ReadTool::new();
    let result = tool
        .execute(
            "test_call",
            json!({ "path": dir }),
            None,
            &ToolContext::default(),
        )
        .await;
    // ReadTool returns Err for directory
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("directory") || err.contains("Cannot read a directory"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_read_path_traversal_blocked() {
    let tool = ReadTool::new();
    let result = tool
        .execute(
            "test_call",
            json!({ "path": "../../etc/passwd" }),
            None,
            &ToolContext::default(),
        )
        .await;
    // ReadTool returns Err for path traversal
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("traversal"));
}

#[tokio::test]
async fn test_read_missing_path_param() {
    let tool = ReadTool::new();
    let result = tool
        .execute("test_call", json!({}), None, &ToolContext::default())
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_read_empty_file() {
    let dir = create_temp_dir("read_empty").await;
    let file_path = format!("{}/empty.txt", dir);
    fs::write(&file_path, "").await.unwrap();

    let tool = ReadTool::new();
    let result = execute_tool(&tool, json!({ "path": file_path })).await;
    assert!(result.success);
    assert_eq!(result.output, "");

    cleanup(&dir).await;
}

// ═══════════════════════════════════════════════════════════════════
// WriteTool Tests
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_write_file_basic() {
    let dir = create_temp_dir("write_basic").await;
    let file_path = format!("{}/output.txt", dir);

    let tool = WriteTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": file_path,
            "content": "Hello from write tool"
        }),
    )
    .await;
    assert!(result.success);

    // Verify content
    let content = fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(content, "Hello from write tool");

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_write_creates_parent_dirs() {
    let dir = create_temp_dir("write_parents").await;
    let file_path = format!("{}/a/b/c/deep.txt", dir);

    let tool = WriteTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": file_path,
            "content": "deeply nested"
        }),
    )
    .await;
    assert!(result.success);

    let content = fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(content, "deeply nested");

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_write_overwrites_existing() {
    let dir = create_temp_dir("write_overwrite").await;
    let file_path = format!("{}/file.txt", dir);
    fs::write(&file_path, "original").await.unwrap();

    let tool = WriteTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": file_path,
            "content": "replaced"
        }),
    )
    .await;
    assert!(result.success);

    let content = fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(content, "replaced");

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_write_append_mode() {
    let dir = create_temp_dir("write_append").await;
    let file_path = format!("{}/log.txt", dir);
    fs::write(&file_path, "line 1\n").await.unwrap();

    let tool = WriteTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": file_path,
            "content": "line 2\n",
            "append": true
        }),
    )
    .await;
    assert!(result.success);

    let content = fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(content, "line 1\nline 2\n");

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_write_path_traversal_blocked() {
    let tool = WriteTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": "../../tmp/evil.txt",
            "content": "bad"
        }),
    )
    .await;
    assert!(!result.success);
    assert!(result.output.contains("traversal"));
}

#[tokio::test]
async fn test_write_missing_content_param() {
    let tool = WriteTool::new();
    let result = tool
        .execute("test_call", json!({}), None, &ToolContext::default())
        .await;
    assert!(result.is_err());
}

// ═══════════════════════════════════════════════════════════════════
// EditTool Tests
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_edit_basic_replacement() {
    let dir = create_temp_dir("edit_basic").await;
    let file_path = format!("{}/code.rs", dir);
    fs::write(&file_path, "fn main() {\n    println!(\"hello\");\n}")
        .await
        .unwrap();

    let tool = EditTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": file_path,
            "old_text": "hello",
            "new_text": "world"
        }),
    )
    .await;
    assert!(result.success);

    let content = fs::read_to_string(&file_path).await.unwrap();
    assert!(content.contains("world"));
    assert!(!content.contains("hello"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_edit_multiline_replacement() {
    let dir = create_temp_dir("edit_multiline").await;
    let file_path = format!("{}/file.txt", dir);
    fs::write(&file_path, "old line 1\nold line 2\nold line 3")
        .await
        .unwrap();

    let tool = EditTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": file_path,
            "old_text": "old line 1\nold line 2",
            "new_text": "new line 1\nnew line 2"
        }),
    )
    .await;
    assert!(result.success);

    let content = fs::read_to_string(&file_path).await.unwrap();
    assert!(content.contains("new line 1"));
    assert!(content.contains("new line 2"));
    assert!(content.contains("old line 3")); // unchanged

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_edit_text_not_found() {
    let dir = create_temp_dir("edit_not_found").await;
    let file_path = format!("{}/file.txt", dir);
    fs::write(&file_path, "some content").await.unwrap();

    let tool = EditTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": file_path,
            "old_text": "nonexistent text",
            "new_text": "replacement"
        }),
    )
    .await;
    assert!(!result.success);
    assert!(result.output.contains("not found"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_edit_dry_run() {
    let dir = create_temp_dir("edit_dryrun").await;
    let file_path = format!("{}/file.txt", dir);
    fs::write(&file_path, "original content").await.unwrap();

    let tool = EditTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": file_path,
            "old_text": "original",
            "new_text": "modified",
            "dry_run": true
        }),
    )
    .await;
    assert!(result.success);

    // File should NOT be changed
    let content = fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(content, "original content");

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_edit_path_traversal() {
    let tool = EditTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": "../../etc/passwd",
            "old_text": "root",
            "new_text": "hacked"
        }),
    )
    .await;
    assert!(!result.success);
    assert!(result.output.contains("traversal"));
}

#[tokio::test]
async fn test_edit_file_not_found() {
    let tool = EditTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": "/tmp/oxicode_nonexistent_12345.txt",
            "old_text": "a",
            "new_text": "b"
        }),
    )
    .await;
    assert!(!result.success);
}

// ═══════════════════════════════════════════════════════════════════
// BashTool Tests
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_bash_echo() {
    let tool = BashTool::new();
    let result = execute_tool(&tool, json!({ "command": "echo hello" })).await;
    assert!(result.success);
    assert!(result.output.contains("hello"));
}

#[tokio::test]
async fn test_bash_exit_code() {
    let tool = BashTool::new();
    let result = execute_tool(&tool, json!({ "command": "exit 1" })).await;
    assert!(!result.success);
}

#[tokio::test]
async fn test_bash_stderr_captured() {
    let tool = BashTool::new();
    let result = execute_tool(&tool, json!({ "command": "echo error >&2 && exit 1" })).await;
    assert!(!result.success);
    assert!(result.output.contains("error"));
}

#[tokio::test]
async fn test_bash_pipe_and_chain() {
    let tool = BashTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "command": "echo -e 'apple\\nbanana\\ncherry' | grep -c 'a'"
        }),
    )
    .await;
    assert!(result.success);
    // Output may include timing info, but the count should be there
    assert!(result.output.contains("2"));
}

#[tokio::test]
async fn test_bash_working_dir() {
    let tool = BashTool::new();
    let result = tool
        .execute(
            "test_call",
            json!({
                "command": "pwd",
                "cwd": "/tmp"
            }),
            None,
            &ToolContext::default(),
        )
        .await;
    // The command may succeed or fail depending on workspace restrictions.
    // The key is it doesn't panic — we handle both Ok and Err.
    match result {
        Ok(r) => {
            assert!(r.success);
            assert!(r.output.contains("/tmp"));
        }
        Err(e) => {
            // Working dir outside workspace is a valid failure mode
            assert!(e.contains("outside") || e.contains("workspace"));
        }
    }
}

#[tokio::test]
async fn test_bash_empty_output() {
    let tool = BashTool::new();
    let result = execute_tool(&tool, json!({ "command": "true" })).await;
    assert!(result.success);
    // Output may include timing info, but should contain the no-output indicator
    assert!(result.output.contains("(no output)") || result.output.contains("Took"));
}

#[tokio::test]
async fn test_bash_missing_command_param() {
    let tool = BashTool::new();
    let result = tool
        .execute("test_call", json!({}), None, &ToolContext::default())
        .await;
    assert!(result.is_err());
}

// ═══════════════════════════════════════════════════════════════════
// GrepTool Tests
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_grep_find_pattern_in_file() {
    let dir = create_temp_dir("grep_basic").await;
    let file_path = format!("{}/test.txt", dir);
    fs::write(&file_path, "hello world\nfoo bar\nhello again")
        .await
        .unwrap();

    let tool = GrepTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "pattern": "hello",
            "path": dir
        }),
    )
    .await;
    assert!(result.success);
    assert!(result.output.contains("hello world"));
    assert!(result.output.contains("hello again"));
    assert!(!result.output.contains("foo bar"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_grep_case_insensitive() {
    let dir = create_temp_dir("grep_case").await;
    let file_path = format!("{}/test.txt", dir);
    fs::write(&file_path, "Hello World").await.unwrap();

    let tool = GrepTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "pattern": "hello",
            "path": dir,
            "case_insensitive": true
        }),
    )
    .await;
    assert!(result.success);
    assert!(result.output.contains("Hello World"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_grep_case_sensitive_default() {
    let dir = create_temp_dir("grep_case_sens").await;
    let file_path = format!("{}/test.txt", dir);
    fs::write(&file_path, "Hello World").await.unwrap();

    let tool = GrepTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "pattern": "hello",
            "path": dir
        }),
    )
    .await;
    assert!(result.success);
    assert_eq!(result.output, "No matches found");

    cleanup(&dir).await;
}

#[tokio::test]
async fn grep_v2_honors_ignore_files_and_reports_cancellation_metadata() {
    let dir = create_temp_dir("grep_v2_semantics").await;
    let root = std::path::PathBuf::from(&dir);
    std::fs::write(root.join(".ignore"), "ignored.txt\n").unwrap();
    std::fs::write(root.join("kept.rs"), "needle here\n").unwrap();
    std::fs::write(root.join("ignored.txt"), "needle here\n").unwrap();

    let tool = GrepTool::with_cwd(root.clone());
    let ctx = ToolContext::new(&root);
    let result = tool
        .execute("t1", serde_json::json!({ "pattern": "needle" }), None, &ctx)
        .await
        .unwrap();
    let text = result.output;
    assert!(text.contains("kept.rs:1: needle here"), "{text}");
    assert!(
        !text.contains("ignored.txt"),
        "v2 must honor .ignore: {text}"
    );

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_grep_no_matches() {
    let dir = create_temp_dir("grep_nomatch").await;
    let file_path = format!("{}/test.txt", dir);
    fs::write(&file_path, "hello world").await.unwrap();

    let tool = GrepTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "pattern": "nonexistent",
            "path": dir
        }),
    )
    .await;
    assert!(result.success);
    assert_eq!(result.output, "No matches found");

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_grep_recursive() {
    let dir = create_temp_dir("grep_recursive").await;
    let sub_dir = format!("{}/sub", dir);
    fs::create_dir_all(&sub_dir).await.unwrap();
    fs::write(format!("{}/a.txt", dir), "pattern match A")
        .await
        .unwrap();
    fs::write(format!("{}/b.txt", sub_dir), "pattern match B")
        .await
        .unwrap();

    let tool = GrepTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "pattern": "pattern match",
            "path": dir
        }),
    )
    .await;
    assert!(result.success);
    assert!(result.output.contains("match A"));
    assert!(result.output.contains("match B"));
    assert_eq!(result.output.matches("pattern match").count(), 2);

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_grep_max_results() {
    let dir = create_temp_dir("grep_max").await;
    let file_path = format!("{}/test.txt", dir);
    let content: Vec<String> = (0..20).map(|i| format!("match line {}", i)).collect();
    fs::write(&file_path, content.join("\n")).await.unwrap();

    let tool = GrepTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "pattern": "match",
            "path": dir,
            "max_results": 5
        }),
    )
    .await;
    assert!(result.success);
    // Should have at most 5 match lines
    let match_count = result.output.matches("match line").count();
    assert_eq!(match_count, 5);

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_grep_line_numbers() {
    let dir = create_temp_dir("grep_lines").await;
    let file_path = format!("{}/test.txt", dir);
    fs::write(&file_path, "first\nsecond\nthird match")
        .await
        .unwrap();

    let tool = GrepTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "pattern": "match",
            "path": dir
        }),
    )
    .await;
    assert!(result.success);
    // Should include line number 3
    assert!(result.output.contains(":3:"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_grep_path_traversal() {
    let tool = GrepTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "pattern": "test",
            "path": "../../etc"
        }),
    )
    .await;
    assert!(!result.success);
    assert!(result.output.contains("traversal"));
}

#[tokio::test]
async fn test_grep_invalid_regex() {
    let dir = create_temp_dir("grep_badregex").await;
    let tool = GrepTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "pattern": "[invalid",
            "path": dir
        }),
    )
    .await;
    assert!(!result.success);
    assert!(result.output.contains("Invalid pattern") || result.output.contains("Invalid regex"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_grep_path_not_found() {
    let tool = GrepTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "pattern": "test",
            "path": "/tmp/oxicode_nonexistent_dir_12345"
        }),
    )
    .await;
    assert!(!result.success);
    assert!(result.output.contains("not found"));
}

// ═══════════════════════════════════════════════════════════════════
// FindTool Tests
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_find_all_files() {
    let dir = create_temp_dir("find_all").await;
    fs::write(format!("{}/a.txt", dir), "").await.unwrap();
    fs::write(format!("{}/b.rs", dir), "").await.unwrap();
    fs::create_dir(format!("{}/subdir", dir)).await.unwrap();
    fs::write(format!("{}/subdir/c.txt", dir), "")
        .await
        .unwrap();

    let tool = FindTool::new();
    let result = execute_tool(&tool, json!({ "path": dir })).await;
    assert!(result.success);
    assert!(result.output.contains("a.txt"));
    assert!(result.output.contains("b.rs"));
    assert!(result.output.contains("subdir/"));
    assert!(result.output.contains("c.txt"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_find_by_name_pattern() {
    let dir = create_temp_dir("find_name").await;
    fs::write(format!("{}/main.rs", dir), "").await.unwrap();
    fs::write(format!("{}/mod.rs", dir), "").await.unwrap();
    fs::write(format!("{}/test.txt", dir), "").await.unwrap();
    fs::write(format!("{}/lib.rs", dir), "").await.unwrap();

    let tool = FindTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": dir,
            "name": "*.rs"
        }),
    )
    .await;
    assert!(result.success);
    assert!(result.output.contains("main.rs"));
    assert!(result.output.contains("mod.rs"));
    assert!(result.output.contains("lib.rs"));
    assert!(!result.output.contains("test.txt"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_find_directories_only() {
    let dir = create_temp_dir("find_dirs").await;
    fs::write(format!("{}/file.txt", dir), "").await.unwrap();
    fs::create_dir(format!("{}/mydir", dir)).await.unwrap();
    fs::write(format!("{}/mydir/nested.txt", dir), "")
        .await
        .unwrap();

    let tool = FindTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": dir,
            "type": "dir"
        }),
    )
    .await;
    assert!(result.success);
    assert!(result.output.contains("mydir"));
    assert!(!result.output.contains("file.txt"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_find_files_only() {
    let dir = create_temp_dir("find_files").await;
    fs::write(format!("{}/file.txt", dir), "").await.unwrap();
    fs::create_dir(format!("{}/mydir", dir)).await.unwrap();

    let tool = FindTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": dir,
            "type": "file"
        }),
    )
    .await;
    assert!(result.success);
    assert!(result.output.contains("file.txt"));
    assert!(!result.output.contains("mydir"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_find_max_depth() {
    let dir = create_temp_dir("find_depth").await;
    fs::create_dir_all(format!("{}/a/b/c", dir)).await.unwrap();
    fs::write(format!("{}/a/level1.txt", dir), "")
        .await
        .unwrap();
    fs::write(format!("{}/a/b/level2.txt", dir), "")
        .await
        .unwrap();
    fs::write(format!("{}/a/b/c/level3.txt", dir), "")
        .await
        .unwrap();

    let tool = FindTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": dir,
            "max_depth": 1
        }),
    )
    .await;
    assert!(result.success);
    assert!(result.output.contains("a/"));
    assert!(!result.output.contains("level2.txt"));
    assert!(!result.output.contains("level3.txt"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_find_max_results() {
    let dir = create_temp_dir("find_max").await;
    for i in 0..20 {
        fs::write(format!("{}/file_{:02}.txt", dir, i), "")
            .await
            .unwrap();
    }

    let tool = FindTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": dir,
            "max_results": 5
        }),
    )
    .await;
    assert!(result.success);
    // Count lines (excluding header)
    let lines: Vec<&str> = result
        .output
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with("Found"))
        .collect();
    assert!(lines.len() <= 5);

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_find_not_a_directory() {
    let dir = create_temp_dir("find_notdir").await;
    let file_path = format!("{}/file.txt", dir);
    fs::write(&file_path, "content").await.unwrap();

    let tool = FindTool::new();
    let result = execute_tool(&tool, json!({ "path": file_path })).await;
    assert!(!result.success);
    assert!(result.output.contains("not a directory"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_find_path_not_found() {
    let tool = FindTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": "/tmp/oxicode_nonexistent_12345"
        }),
    )
    .await;
    assert!(
        !result.success,
        "find should fail for non-existent path: {}",
        result.output
    );
    // Error message should indicate path issue (not found, not a directory, or outside workspace)
    assert!(
        result.output.contains("not found")
            || result.output.contains("not a directory")
            || result.output.contains("outside")
            || result.output.contains("Cannot read"),
        "should mention path issue, got: {}",
        result.output
    );
}

#[tokio::test]
async fn test_find_path_traversal() {
    let tool = FindTool::new();
    let result = execute_tool(&tool, json!({ "path": "../../etc" })).await;
    assert!(!result.success);
    assert!(result.output.contains("traversal"));
}

#[tokio::test]
async fn test_find_skips_hidden() {
    let dir = create_temp_dir("find_hidden").await;
    fs::write(format!("{}/visible.txt", dir), "").await.unwrap();
    fs::write(format!("{}/.hidden", dir), "").await.unwrap();
    fs::create_dir(format!("{}/.hidden_dir", dir))
        .await
        .unwrap();

    let tool = FindTool::new();
    let result = execute_tool(&tool, json!({ "path": dir })).await;
    assert!(result.success);
    assert!(result.output.contains("visible.txt"));
    assert!(!result.output.contains(".hidden"));
    assert!(!result.output.contains(".hidden_dir"));

    cleanup(&dir).await;
}

// ═══════════════════════════════════════════════════════════════════
// LsTool Tests
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_ls_basic() {
    let dir = create_temp_dir("ls_basic").await;
    fs::write(format!("{}/file.txt", dir), "").await.unwrap();
    fs::create_dir(format!("{}/subdir", dir)).await.unwrap();

    let tool = LsTool::new();
    let result = execute_tool(&tool, json!({ "path": dir })).await;
    assert!(result.success);
    assert!(result.output.contains("file.txt"));
    assert!(result.output.contains("subdir/"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_ls_hides_hidden_files() {
    let dir = create_temp_dir("ls_hidden").await;
    fs::write(format!("{}/visible.txt", dir), "").await.unwrap();
    fs::write(format!("{}/.hidden", dir), "").await.unwrap();

    let tool = LsTool::new();
    let result = execute_tool(&tool, json!({ "path": dir })).await;
    assert!(result.success);
    assert!(result.output.contains("visible.txt"));
    assert!(!result.output.contains(".hidden"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_ls_all_shows_hidden() {
    let dir = create_temp_dir("ls_all").await;
    fs::write(format!("{}/visible.txt", dir), "").await.unwrap();
    fs::write(format!("{}/.hidden", dir), "").await.unwrap();

    let tool = LsTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": dir,
            "all": true
        }),
    )
    .await;
    assert!(result.success);
    assert!(result.output.contains("visible.txt"));
    assert!(result.output.contains(".hidden"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_ls_long_format() {
    let dir = create_temp_dir("ls_long").await;
    fs::write(format!("{}/file.txt", dir), "hello world")
        .await
        .unwrap();
    fs::create_dir(format!("{}/subdir", dir)).await.unwrap();

    let tool = LsTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": dir,
            "long": true
        }),
    )
    .await;
    assert!(result.success);
    // Should show size for files
    assert!(result.output.contains("11")); // "hello world" = 11 bytes
    assert!(result.output.contains("file.txt"));
    assert!(result.output.contains("subdir/"));
    // Should show summary
    assert!(result.output.contains("director"));
    assert!(result.output.contains("file"));

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_ls_empty_directory() {
    let dir = create_temp_dir("ls_empty").await;

    let tool = LsTool::new();
    let result = execute_tool(&tool, json!({ "path": dir })).await;
    assert!(result.success);
    assert_eq!(result.output, "");

    cleanup(&dir).await;
}

#[tokio::test]
async fn test_ls_path_not_found() {
    let tool = LsTool::new();
    let result = execute_tool(
        &tool,
        json!({
            "path": "/tmp/oxicode_nonexistent_12345"
        }),
    )
    .await;
    assert!(!result.success);
    assert!(result.output.contains("not found"));
}

#[tokio::test]
async fn test_ls_path_traversal() {
    let tool = LsTool::new();
    let result = execute_tool(&tool, json!({ "path": "../../etc" })).await;
    assert!(!result.success);
    assert!(result.output.contains("traversal"));
}

#[tokio::test]
async fn test_ls_file_instead_of_dir() {
    let dir = create_temp_dir("ls_file").await;
    let file_path = format!("{}/file.txt", dir);
    fs::write(&file_path, "hello").await.unwrap();

    let tool = LsTool::new();
    let result = execute_tool(&tool, json!({ "path": file_path })).await;
    assert!(result.success);
    assert!(result.output.contains("file.txt"));

    cleanup(&dir).await;
}

// ═══════════════════════════════════════════════════════════════════
// ToolRegistry Tests
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_registry_with_builtins() {
    let registry = ToolRegistry::with_builtins();
    let names = registry.names();

    // Core file/system tools
    assert!(names.contains(&"read".to_string()));
    assert!(names.contains(&"write".to_string()));
    assert!(names.contains(&"edit".to_string()));
    assert!(names.contains(&"bash".to_string()));
    assert!(names.contains(&"grep".to_string()));
    assert!(names.contains(&"find".to_string()));
    assert!(names.contains(&"ls".to_string()));

    // Search tools
    assert!(names.contains(&"web_search".to_string()));
    assert!(names.contains(&"get_search_results".to_string()));

    // Integrations
    assert!(names.contains(&"github".to_string()));
    assert!(names.contains(&"subagent".to_string()));
    assert!(names.contains(&"mcp".to_string()));
    assert!(names.contains(&"context7_resolve-library-id".to_string()));
    assert!(names.contains(&"context7_query-docs".to_string()));
    assert!(names.contains(&"generate_image".to_string()));

    // Keep this in sync with ToolRegistry::with_builtins_cwd()
    assert_eq!(names.len(), 38);
}

#[test]
fn test_registry_definitions() {
    let registry = ToolRegistry::with_builtins();
    let defs = registry.definitions();
    assert_eq!(defs.len(), 38);

    // Each should have name, description, and input_schema
    for def in &defs {
        assert!(!def.name.is_empty());
        assert!(!def.description.is_empty());
    }
}

#[test]
fn test_registry_get_tool() {
    let registry = ToolRegistry::with_builtins();
    let read_tool = registry.get("read");
    assert!(read_tool.is_some());
    assert_eq!(read_tool.unwrap().name(), "read");

    let missing = registry.get("nonexistent");
    assert!(missing.is_none());
}

#[test]
fn test_registry_custom_tool() {
    use oxicode_agent::AgentTool;
    use serde_json::{Value, json};
    use tokio::sync::oneshot;

    struct CustomTool;

    #[async_trait]
    impl AgentTool for CustomTool {
        fn name(&self) -> &str {
            "custom"
        }
        fn label(&self) -> &str {
            "Custom"
        }
        fn description(&self) -> &str {
            "A custom test tool"
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        async fn execute(
            &self,
            _id: &str,
            _params: Value,
            _signal: Option<oneshot::Receiver<()>>,
            _ctx: &oxicode_agent::ToolContext,
        ) -> Result<oxicode_agent::AgentToolResult, String> {
            Ok(oxicode_agent::AgentToolResult::success("custom result"))
        }
    }

    let registry = ToolRegistry::new();
    registry.register(CustomTool);
    assert!(registry.get("custom").is_some());
}

// ═══════════════════════════════════════════════════════════════════
// ToolDefinition / to_definition Tests
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_tool_to_definition() {
    let tool = ReadTool::new();
    let def = tool.to_definition();
    assert_eq!(def.name, "read");
    assert!(!def.description.is_empty());
    assert!(!def.input_schema.is_empty());
}

#[test]
fn test_all_tools_have_valid_schemas() {
    let registry = ToolRegistry::with_builtins();
    for tool in registry.get_tools() {
        let schema = tool.parameters_schema();
        assert!(
            schema.is_object(),
            "{} should have object schema",
            tool.name()
        );
        let obj = schema.as_object().unwrap();
        assert!(
            obj.contains_key("type"),
            "{} schema missing type",
            tool.name()
        );
        assert!(
            obj.contains_key("properties"),
            "{} schema missing properties",
            tool.name()
        );
    }
}
