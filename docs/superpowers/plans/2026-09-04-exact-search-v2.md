# Exact Search v2 (`grep.search.v2`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the legacy recursive `GrepTool` walker with a native streaming, ignore-aware, cancellable `ExactSearchEngine`, shipped through `coding-omp-v1` as `grep.search.v2`.

**Architecture:** New private module `oxicode-agent/src/tools/exact_search.rs` owns traversal (`ignore` crate `WalkParallel`), streaming line search, context coalescing, output budget, and cancellation. `GrepTool` keeps its JSON schema, `PathGuard`, renderer, and internal-URL path, and delegates the filesystem search to the engine inside `spawn_blocking`. The legacy walker survives as `GrepTool::legacy_with_cwd` used only by `ToolRegistry::with_builtins_cwd` (migration window until 0.83.0). The behavior pack descriptor bumps to `grep.search.v2` declaring `.replaces("grep.search.v1")`.

**Tech Stack:** `ignore = "0.4"` (walker; brings `globset`), `regex = "1"` (existing), tokio `spawn_blocking` + `oneshot` (existing).

**Spec:** `docs/designs/2026-09-03-workspace-retrieval-zvec-grep.md` (D2 section; decisions locked with user: v1 artifact-dir list stays as built-in exclusions, cancellation returns partial results + `cancelled` marker).

## Global Constraints

- `cargo fmt` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; tests via `cargo nextest run -p oxicode-agent`.
- Engine is `pub(crate)` — never exported from `oxicode-agent`.
- Output format is FROZEN (v1 parity): match line `"{rel}:{line}: {text}"`, context line `"{rel}-{line}- {text}"`, header `"Found {n} matches:\n"`, empty → `"No matches found"`, 500-char line cap with `"... [truncated]"` suffix.
- Built-in artifact dir exclusions (v1 parity, applied in addition to ignore files): `node_modules`, `target`, `dist`, `build`, `__pycache__`, `.venv`, `venv`.
- Hidden entries skipped; symlinks not followed; explicit `path` roots are always searched (ignore files do not suppress the root itself).
- Cancellation contract: partial results + `metadata.cancelled = true` + trailing `"\nSearch cancelled: results are partial (N shown)."`.
- Never shell out to `rg` or `zg` in product code. `rg` is a manual benchmark yardstick only.

---

### Task 1: Engine skeleton — walk, match, render (no context)

**Files:**
- Create: `oxicode-agent/src/tools/exact_search.rs`
- Modify: `oxicode-agent/src/tools.rs` (add `mod exact_search;` near the other tool module decls)
- Modify: `oxicode-agent/Cargo.toml` (add `ignore = "0.4"` under `[dependencies]`)

**Interfaces:**
- Produces (used by Task 2-4):

```rust
pub(crate) struct SearchRequest {
    pub pattern: String,
    pub case_insensitive: bool,
    pub literal: bool,
    pub context: usize,
    pub include: Option<String>,
    pub max_results: usize,
}

pub(crate) struct SearchOutcome {
    pub output: String,
    pub lines_truncated: bool,
    pub cancelled: bool,
}

pub(crate) fn search(root: &Path, req: &SearchRequest, cancel: Arc<AtomicBool>)
    -> Result<SearchOutcome, String>;
```

- [ ] **Step 1: Add the dependency**

In `oxicode-agent/Cargo.toml` `[dependencies]`, next to `regex = "1"`:

```toml
ignore = "0.4"
```

- [ ] **Step 2: Write the failing tests**

Create `oxicode-agent/src/tools/exact_search.rs` with the module skeleton plus a `#[cfg(test)] mod tests` containing (all helpers build a tempdir workspace; use `tempfile` — already a dev-dependency of the crate):

```rust
//! ExactSearchEngine — streaming, ignore-aware, cancellable file search.
//!
//! Private engine behind `GrepTool` v2 (`grep.search.v2`). Owns traversal
//! (`ignore` WalkParallel), streaming line matching, context coalescing,
//! output budget, binary detection, and cancellation. `GrepTool` keeps the
//! schema, PathGuard, renderer, and internal-URL path.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// v1-parity artifact exclusions (spec D2: rg policy plus this fixed list).
const ARTIFACT_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "__pycache__",
    ".venv",
    "venv",
];

/// Maximum characters per line (v1 parity).
const MAX_LINE_LENGTH: usize = 500;

/// Byte budget for the binary sniff at file start (rg heuristic).
const SNIFF_LEN: usize = 8192;

pub(crate) struct SearchRequest {
    pub pattern: String,
    pub case_insensitive: bool,
    pub literal: bool,
    pub context: usize,
    pub include: Option<String>,
    pub max_results: usize,
}

#[derive(Debug)]
pub(crate) struct SearchOutcome {
    pub output: String,
    pub lines_truncated: bool,
    pub cancelled: bool,
}

/// Build the regex exactly like v1 (literal → escape; case flag).
fn build_regex(req: &SearchRequest) -> Result<regex::Regex, String> {
    let pattern = if req.literal {
        regex::escape(&req.pattern)
    } else {
        req.pattern.clone()
    };
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(req.case_insensitive)
        .build()
        .map_err(|e| format!("Invalid pattern '{}': {}", req.pattern, e))
}

/// True when the file's first bytes look binary (NUL byte) — mirrors v1,
/// which silently skipped non-UTF-8 files.
fn looks_binary(path: &Path) -> bool {
    let mut buf = [0u8; SNIFF_LEN];
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return true, // unreadable → skip (v1 parity)
    };
    let n = match f.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return true,
    };
    buf[..n].contains(&0)
}

/// Char-boundary-safe 500-char truncation with the v1 suffix.
fn truncate_line(line: &str) -> (String, bool) {
    if line.len() <= MAX_LINE_LENGTH {
        return (line.to_string(), false);
    }
    let mut end = MAX_LINE_LENGTH;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}... [truncated]", &line[..end]), true)
}

pub(crate) fn search(
    root: &Path,
    req: &SearchRequest,
    cancel: Arc<AtomicBool>,
) -> Result<SearchOutcome, String> {
    let re = build_regex(req)?;

    // include → globset glob matched against the root-relative path.
    // Default globset semantics (literal_separator = false) make `*.rs`
    // match nested paths too, which covers v1's basename behavior and adds
    // real path globs like `src/**/*.rs`.
    let include_matcher = match &req.include {
        Some(glob) => Some(
            globset::GlobBuilder::new(glob)
                .build()
                .map_err(|e| format!("Invalid include glob '{glob}': {e}"))?
                .compile_matcher(),
        ),
        None => None,
    };

    // … walk + collect implemented in Steps 3-4 …
    let _ = root;
    Ok(SearchOutcome {
        output: "No matches found".to_string(),
        lines_truncated: false,
        cancelled: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n",
        )
        .unwrap();
        std::fs::write(root.join("notes.txt"), "alpha here\nplain\n").unwrap();
        (dir, root)
    }

    fn req(pattern: &str) -> SearchRequest {
        SearchRequest {
            pattern: pattern.to_string(),
            case_insensitive: false,
            literal: false,
            context: 0,
            include: None,
            max_results: 100,
        }
    }

    fn run(root: &Path, r: &SearchRequest) -> SearchOutcome {
        search(root, r, Arc::new(AtomicBool::new(false))).unwrap()
    }

    #[test]
    fn finds_matches_in_multiple_files() {
        let (_d, root) = ws();
        let out = run(&root, &req("alpha"));
        assert!(out.output.starts_with("Found 2 matches:\n"), "{}", out.output);
        assert!(out.output.contains("src/lib.rs:1: fn alpha() {}"));
        assert!(out.output.contains("notes.txt:1: alpha here"));
        assert!(!out.lines_truncated && !out.cancelled);
    }

    #[test]
    fn no_matches_message() {
        let (_d, root) = ws();
        let out = run(&root, &req("zzz_not_there"));
        assert_eq!(out.output, "No matches found");
    }

    #[test]
    fn literal_mode_escapes_regex_syntax() {
        let (_d, root) = ws();
        std::fs::write(root.join("src/lib.rs"), "fn alpha() {}\na.c\n").unwrap();
        let mut r = req("a.c");
        r.literal = true;
        let out = run(&root, &r);
        assert!(out.output.contains("src/lib.rs:2: a.c"));
        assert!(!out.output.contains("fn alpha"));
    }

    #[test]
    fn case_insensitive_flag() {
        let (_d, root) = ws();
        std::fs::write(root.join("src/lib.rs"), "ALPHA\n").unwrap();
        let mut r = req("alpha");
        r.case_insensitive = true;
        assert!(run(&root, &r).output.contains("ALPHA"));
        r.case_insensitive = false;
        assert_eq!(run(&root, &r).output, "No matches found");
    }

    #[test]
    fn invalid_regex_is_typed_error() {
        let (_d, root) = ws();
        let err = search(&root, &req("(unclosed"), Arc::new(AtomicBool::new(false)))
            .unwrap_err();
        assert!(err.contains("Invalid pattern"), "{err}");
    }

    #[test]
    fn invalid_include_glob_is_typed_error() {
        let (_d, root) = ws();
        let mut r = req("alpha");
        r.include = Some("[bad".to_string());
        let err = search(&root, &r, Arc::new(AtomicBool::new(false))).unwrap_err();
        assert!(err.contains("Invalid include glob"), "{err}");
    }

    #[test]
    fn include_glob_filters_files() {
        let (_d, root) = ws();
        let mut r = req("alpha");
        r.include = Some("*.rs".to_string());
        let out = run(&root, &r);
        assert!(out.output.contains("src/lib.rs:1:"));
        assert!(!out.output.contains("notes.txt"));

        let mut r2 = req("alpha");
        r2.include = Some("*.txt".to_string());
        let out2 = run(&root, &r2);
        assert!(out2.output.contains("notes.txt:1:"));
        assert!(!out2.output.contains("lib.rs"));
    }

    #[test]
    fn path_glob_include_works() {
        let (_d, root) = ws();
        let mut r = req("alpha");
        r.include = Some("src/**/*.rs".to_string());
        assert!(run(&root, &r).output.contains("lib.rs"));
        let mut r2 = req("alpha");
        r2.include = Some("docs/**/*.rs".to_string());
        assert_eq!(run(&root, &r2).output, "No matches found");
    }
}
```

Add `use std::path::Path;` is already covered by the imports above; add `globset` is re-exported? — NO: `globset` is a separate crate. Add it explicitly in Step 1:

```toml
ignore = "0.4"
globset = "0.4"
```

(Both are already in the dependency tree via ripgrep-family transitives, so lockfile churn is minimal.)

- [ ] **Step 3: Run the tests to verify they compile and fail/pass appropriately**

Run: `cargo nextest run -p oxicode-agent --lib tools::exact_search`
Expected: the two glob-error and no-match tests pass; `finds_matches_in_multiple_files`, `include_glob_filters_files`, `path_glob_include_works` FAIL (engine is a stub returning "No matches found").

- [ ] **Step 4: Implement the real walk + match + render**

Replace the stub body of `search` with the full implementation:

```rust
pub(crate) fn search(
    root: &Path,
    req: &SearchRequest,
    cancel: Arc<AtomicBool>,
) -> Result<SearchOutcome, String> {
    let re = build_regex(req)?;

    let include_matcher = match &req.include {
        Some(glob) => Some(
            globset::GlobBuilder::new(glob)
                .build()
                .map_err(|e| format!("Invalid include glob '{glob}': {e}"))?
                .compile_matcher(),
        ),
        None => None,
    };

    let blocks: Arc<std::sync::Mutex<Vec<FileBlock>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let budget = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let lines_truncated = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));

    let mut builder = ignore::WalkBuilder::new(root);
    builder.hidden(true); // v1 parity: skip hidden entries
    builder.git_ignore(true);
    builder.git_global(true);
    builder.git_exclude(true);
    builder.ignore(true); // .ignore files (honored outside git repos too)
    builder.parents(true);
    builder.follow_links(false); // spec: symlinks are not followed

    let mut walker = builder.build_parallel();
    walker.visit(|| {
        let re = re.clone();
        let include_matcher = include_matcher.clone();
        let blocks = blocks.clone();
        let budget = budget.clone();
        let done = done.clone();
        let cancelled = cancelled.clone();
        let lines_truncated = lines_truncated.clone();
        let cancel = cancel.clone();
        let root = root.to_path_buf();
        let req_ctx = req.context;
        let req_max = req.max_results;

        Box::new(move |entry| {
            if done.load(Ordering::Relaxed) {
                return ignore::Visit::Exit;
            }
            if cancel.load(Ordering::Relaxed) {
                cancelled.store(true, Ordering::Relaxed);
                done.store(true, Ordering::Relaxed);
                return ignore::Visit::Exit;
            }
            let Ok(entry) = entry else {
                return ignore::Visit::Continue;
            };
            let ft = match entry.file_type() {
                Some(ft) => ft,
                None => return ignore::Visit::Continue,
            };
            if ft.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    if ARTIFACT_DIRS.contains(&name) {
                        return ignore::Visit::Skip;
                    }
                }
                return ignore::Visit::Continue;
            }
            if !ft.is_file() {
                return ignore::Visit::Continue; // symlinks etc. — not followed
            }
            let path = entry.path();
            if looks_binary(path) {
                return ignore::Visit::Continue;
            }
            if let Some(m) = &include_matcher {
                let rel = path.strip_prefix(&root).unwrap_or(path);
                if !m.is_match(rel) {
                    return ignore::Visit::Continue;
                }
            }
            let result = search_file(path, &root, &re, req_ctx, req_max, &budget, &done);
            if result.truncated_any {
                lines_truncated.store(true, Ordering::Relaxed);
            }
            if !result.block.lines.is_empty() {
                blocks.lock().unwrap().push(result.block);
            }
            ignore::Visit::Continue
        })
    });

    walker.run();

    let mut blocks: Vec<FileBlock> = {
        let mut guard = blocks.lock().unwrap();
        std::mem::take(&mut *guard)
    };
    blocks.sort_by(|a, b| a.rel.cmp(&b.rel));

    let mut lines_truncated_flag = lines_truncated.load(Ordering::Relaxed);
    let mut rendered: Vec<String> = Vec::new();
    for block in &blocks {
        for (line_no, is_context, text) in &block.lines {
            if rendered.len() >= req.max_results {
                break;
            }
            let (t, was_trunc) = truncate_line(text);
            if was_trunc {
                lines_truncated_flag = true;
            }
            rendered.push(if *is_context {
                format!("{}-{}- {}", block.rel, line_no, t)
            } else {
                format!("{}:{}: {}", block.rel, line_no, t)
            });
        }
        if rendered.len() >= req.max_results {
            break;
        }
    }

    let was_cancelled = cancelled.load(Ordering::Relaxed);
    let mut output = if rendered.is_empty() {
        "No matches found".to_string()
    } else {
        format!("Found {} matches:\n", rendered.len()) + &rendered.join("\n")
    };
    if was_cancelled && !rendered.is_empty() {
        output.push_str(&format!(
            "\nSearch cancelled: results are partial ({} shown).",
            rendered.len()
        ));
    }

    Ok(SearchOutcome {
        output,
        lines_truncated: lines_truncated_flag,
        cancelled: was_cancelled,
    })
}

/// One file's match rows: (line_no, is_context, rendered-text).
struct FileBlock {
    rel: String,
    lines: Vec<(usize, bool, String)>,
}

struct FileSearchResult {
    block: FileBlock,
    truncated_any: bool,
}

/// Stream one file line-by-line. Pass 1 collects match line numbers; pass 2
/// emits context windows for matches whose output-line budget was claimed
/// atomically, so parallel workers cannot exceed `max_results` in total.
/// Unreadable files and mid-file NUL bytes are skipped silently (v1 parity:
/// `read_to_string` failed on those before).
fn search_file(
    path: &Path,
    root: &Path,
    re: &regex::Regex,
    context: usize,
    max_results: usize,
    budget: &AtomicUsize,
    done: &AtomicBool,
) -> FileSearchResult {
    let empty = |rel: String| FileSearchResult {
        block: FileBlock { rel, lines: Vec::new() },
        truncated_any: false,
    };
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return empty(rel),
    };
    let reader = BufReader::new(file);

    // Pass 1: stream lines, remember matches. A NUL past the sniff marks a
    // binary file — discard everything (v1 parity).
    let mut raw_lines: Vec<String> = Vec::new();
    let mut match_lines: Vec<usize> = Vec::new();
    for (idx, line) in reader.split(b'\n').enumerate() {
        let Ok(bytes) = line else {
            break; // mid-file I/O error → keep what we have
        };
        if bytes.contains(&0) {
            return empty(rel);
        }
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if text.ends_with('\r') {
            text.pop();
        }
        if re.is_match(&text) {
            match_lines.push(idx);
        }
        raw_lines.push(text);
    }

    // Pass 2: claim budget per match window, then emit rows.
    let mut lines: Vec<(usize, bool, String)> = Vec::new();
    for &m in &match_lines {
        if done.load(Ordering::Relaxed) {
            break;
        }
        let start = if context > 0 { m.saturating_sub(context) } else { m };
        let end = std::cmp::min(raw_lines.len(), m + context + 1);
        let claim = budget.fetch_add(end - start, Ordering::Relaxed);
        if claim >= max_results {
            done.store(true, Ordering::Relaxed);
            break;
        }
        for (j, l) in raw_lines.iter().enumerate().take(end).skip(start) {
            let is_context = j != m;
            lines.push((j + 1, is_context, l.clone()));
        }
    }

    FileSearchResult {
        block: FileBlock { rel, lines },
        truncated_any: false,
    }
}
```

- [ ] **Step 5: Run the engine tests**

Run: `cargo nextest run -p oxicode-agent --lib tools::exact_search`
Expected: ALL PASS (8 tests).

- [ ] **Step 6: Commit**

```bash
git add oxicode-agent/src/tools/exact_search.rs oxicode-agent/src/tools.rs oxicode-agent/Cargo.toml Cargo.lock
git commit -m "feat(agent): ExactSearchEngine skeleton with walk/match/render"
```

(`tools.rs` change in this task is just `mod exact_search;` — place it next to `mod grep;`.)

### Task 2: Engine — ignore policy, binary, symlink fixtures

**Files:**
- Modify: `oxicode-agent/src/tools/exact_search.rs` (tests only, unless a defect surfaces)

**Interfaces:**
- Consumes: `search()` from Task 1.
- Produces: confidence fixtures for the spec's ignore policy (`.ignore` files always honored; `.gitignore` honored inside git repos; built-in artifact dirs skipped; hidden skipped; symlinks not followed; explicit file root searched).

- [ ] **Step 1: Write the failing tests** (append to `mod tests`):

```rust
    #[test]
    fn dot_ignore_files_are_honored() {
        let (_d, root) = ws();
        std::fs::write(root.join(".ignore"), "notes.txt\n").unwrap();
        let out = run(&root, &req("alpha"));
        assert!(out.output.contains("src/lib.rs:1:"));
        assert!(!out.output.contains("notes.txt"));
    }

    #[test]
    fn gitignore_honored_inside_git_repo() {
        let (_d, root) = ws();
        std::fs::write(root.join(".gitignore"), "notes.txt\n").unwrap();
        // run `git init` so the repo is a git worktree (rg/ignore parity:
        // .gitignore is authoritative only inside one).
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .expect("git available");
        assert!(status.success());
        let out = run(&root, &req("alpha"));
        assert!(out.output.contains("src/lib.rs:1:"));
        assert!(!out.output.contains("notes.txt"));
    }

    #[test]
    fn artifact_dirs_are_skipped() {
        let (_d, root) = ws();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("target/debug/junk.rs"), "alpha\n").unwrap();
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::write(root.join("node_modules/pkg/x.rs"), "alpha\n").unwrap();
        let out = run(&root, &req("alpha"));
        assert!(out.output.contains("src/lib.rs:1:"));
        assert!(!out.output.contains("target"));
        assert!(!out.output.contains("node_modules"));
    }

    #[test]
    fn hidden_entries_are_skipped() {
        let (_d, root) = ws();
        std::fs::create_dir_all(root.join(".secret")).unwrap();
        std::fs::write(root.join(".secret/a.rs"), "alpha\n").unwrap();
        std::fs::write(root.join(".hidden_file"), "alpha\n").unwrap();
        let out = run(&root, &req("alpha"));
        assert!(!out.output.contains(".secret"));
        assert!(!out.output.contains(".hidden_file"));
    }

    #[test]
    fn binary_files_are_skipped() {
        let (_d, root) = ws();
        let mut bytes = b"alpha then NUL".to_vec();
        bytes.push(0);
        std::fs::write(root.join("blob.bin"), &bytes).unwrap();
        let out = run(&root, &req("alpha"));
        assert!(out.output.contains("src/lib.rs:1:"));
        assert!(!out.output.contains("blob.bin"));
    }

    #[test]
    fn symlinks_are_not_followed() {
        let (_d, root) = ws();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("outer.rs"), "alpha\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), root.join("linked")).unwrap();
        let out = run(&root, &req("alpha"));
        assert!(!out.output.contains("outer.rs"), "{}", out.output);
    }

    #[test]
    fn explicit_file_root_is_searched() {
        let (_d, root) = ws();
        let out = run(&root.join("notes.txt"), &req("alpha"));
        assert!(out.output.contains("notes.txt:1: alpha here"));
    }

    #[test]
    fn crlf_lines_match_and_render_clean() {
        let (_d, root) = ws();
        std::fs::write(root.join("src/lib.rs"), "fn alpha() {}\r\n").unwrap();
        let out = run(&root, &req("alpha"));
        assert!(out.output.contains("fn alpha() {}"));
        assert!(!out.output.contains('\r'));
    }
```

- [ ] **Step 2: Run** — `cargo nextest run -p oxicode-agent --lib tools::exact_search`
Expected: ALL PASS. If `symlinks_are_not_followed` fails because `WalkBuilder` yields the symlink itself: verify the entry's `file_type().is_file()` is false for symlinks (it is, without `follow_links`) and the file-content read never happens.

- [ ] **Step 3: Commit**

```bash
git add oxicode-agent/src/tools/exact_search.rs
git commit -m "test(agent): exact-search ignore/binary/symlink policy fixtures"
```

### Task 3: Engine — context, budget, truncation, cancellation

**Files:**
- Modify: `oxicode-agent/src/tools/exact_search.rs` (budget wiring + tests)

**Interfaces:**
- Produces: context rendering (v1 format), `max_results` output-line budget, `lines_truncated` flag, cancellation with partial+marker.

- [ ] **Step 1: Write the failing tests** (append):

```rust
    #[test]
    fn context_lines_render_with_v1_format() {
        let (_d, root) = ws();
        std::fs::write(root.join("src/lib.rs"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let mut r = req("three");
        r.context = 1;
        let out = run(&root, &r);
        assert!(out.output.contains("src/lib.rs-2- two"), "{}", out.output);
        assert!(out.output.contains("src/lib.rs:3: three"));
        assert!(out.output.contains("src/lib.rs-4- four"));
    }

    #[test]
    fn max_results_budget_limits_output_lines() {
        let (_d, root) = ws();
        std::fs::write(
            root.join("src/lib.rs"),
            "alpha 1\nalpha 2\nalpha 3\nalpha 4\nalpha 5\n",
        )
        .unwrap();
        let mut r = req("alpha");
        r.max_results = 2;
        let out = run(&root, &r);
        let body = out.output.lines().count() - 1; // minus header
        assert!(body <= 2, "budget exceeded: {}", out.output);
        assert!(out.output.contains("Found"));
    }

    #[test]
    fn long_lines_are_truncated_with_flag() {
        let (_d, root) = ws();
        let long = format!("alpha {}\n", "x".repeat(700));
        std::fs::write(root.join("src/lib.rs"), long).unwrap();
        let out = run(&root, &req("alpha"));
        assert!(out.lines_truncated);
        assert!(out.output.contains("... [truncated]"));
        assert!(out.output.lines().nth(1).unwrap().chars().count() < 600);
    }

    #[test]
    fn unicode_long_line_truncates_on_char_boundary() {
        let (_d, root) = ws();
        let long = format!("alpha {}\n", "한".repeat(600)); // 3 bytes each
        std::fs::write(root.join("src/lib.rs"), long).unwrap();
        let out = run(&root, &req("alpha")); // must not panic
        assert!(out.lines_truncated);
    }

    #[test]
    fn cancellation_returns_partial_with_marker() {
        let (_d, root) = ws();
        for i in 0..40 {
            std::fs::write(root.join(format!("f{i:03}.txt")), "alpha here\n").unwrap();
        }
        let cancel = Arc::new(AtomicBool::new(true)); // cancelled before start
        let out = search(&root, &req("alpha"), cancel).unwrap();
        assert!(out.cancelled);
    }
```

- [ ] **Step 2: Wire budget + cancellation into `search_file`** — per the Task 1 NOTE: claim budget before pushing each match block; on crossing, stop and set `done`; drop `raw_lines` retention by streaming match collection in two passes (pass 1: line numbers; pass 2: render only claimed windows) if memory review requires — optional.

- [ ] **Step 3: Run** — `cargo nextest run -p oxicode-agent --lib tools::exact_search`
Expected: ALL PASS.

- [ ] **Step 4: Commit**

```bash
git add oxicode-agent/src/tools/exact_search.rs
git commit -m "feat(agent): exact-search context, budget, truncation, cancellation"
```

### Task 4: GrepTool v2 wiring + legacy split

**Files:**
- Modify: `oxicode-agent/src/tools/grep.rs` (delegate to engine; `legacy_with_cwd`; move walker into `mod legacy`)
- Modify: `oxicode-agent/src/tools.rs` (`with_builtins_cwd` → `GrepTool::legacy_with_cwd`)

**Interfaces:**
- Consumes: `exact_search::{search, SearchRequest}` from Tasks 1-3.
- Produces: `GrepTool::with_cwd` (v2, used by the pack) and `GrepTool::legacy_with_cwd` (v1 walker, used by `with_builtins_cwd`). Public API surface otherwise unchanged.

- [ ] **Step 1: Write the failing test** (add to `oxicode-agent/src/tools.rs` test module, near the existing GrepTool tests):

```rust
    #[tokio::test]
    async fn grep_v2_honors_ignore_files_and_reports_cancellation_metadata() {
        let dir = create_temp_dir("grep_v2_semantics").await;
        std::fs::write(dir.path().join(".ignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("kept.rs"), "needle here\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "needle here\n").unwrap();

        let tool = GrepTool::with_cwd(dir.path().to_path_buf());
        let ctx = ToolContext::new(dir.path());
        let result = tool
            .execute(
                "t1",
                serde_json::json!({ "pattern": "needle" }),
                None,
                &ctx,
            )
            .await
            .unwrap();
        let text = result.text();
        assert!(text.contains("kept.rs:1: needle here"), "{text}");
        assert!(!text.contains("ignored.txt"), "v2 must honor .ignore: {text}");
    }
```

(Adapt `result.text()` to the actual accessor used by neighboring tests in the same file — they already render output, mirror them.)

- [ ] **Step 2: Run** — `cargo nextest run -p oxicode-agent --lib tools::tests::grep_v2_honors_ignore_files`
Expected: FAIL (with_cwd still legacy).

- [ ] **Step 3: Implement**

In `grep.rs`:

1. Wrap the entire existing walker (`matches_glob`, `grep_impl`, `read_file_lines`, `grep_walk`) in `pub(crate) mod legacy { … }` inside `grep.rs` (single file, no move), keeping behavior byte-identical.
2. `GrepTool` gains:

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
enum Engine { Native, Legacy }

pub struct GrepTool {
    root_dir: Option<PathBuf>,
    engine: Engine,
}

impl GrepTool {
    pub fn new() -> Self { Self { root_dir: None, engine: Engine::Native } }
    pub fn with_cwd(cwd: PathBuf) -> Self { Self { root_dir: Some(cwd), engine: Engine::Native } }
    /// Legacy walker (pre-v2 semantics). Migration-window only: used by
    /// `ToolRegistry::with_builtins_cwd`; removal targeted 0.83.0.
    pub fn legacy_with_cwd(cwd: PathBuf) -> Self { Self { root_dir: Some(cwd), engine: Engine::Legacy } }
}
```

3. In `execute`, after the internal-URL branch (which stays first and unchanged), branch:

```rust
let root = self.root_dir.as_deref().unwrap_or(ctx.root());
if self.engine == Engine::Legacy {
    // unchanged legacy call: Self::legacy::grep_impl(...)
} else {
    let guard = PathGuard::new(root);
    let validated = guard
        .validate_traversal(Path::new(path))
        .map_err(|e| e.to_string())?;
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
```

4. In `tools.rs` `with_builtins_cwd`: change `Box::new(GrepTool::with_cwd(cwd.clone()))` → `Box::new(GrepTool::legacy_with_cwd(cwd.clone()))` with a one-line comment: `// legacy walker until 0.83 (migration window); the pack installs grep.search.v2`.

- [ ] **Step 4: Run the full grep-related test set**

Run: `cargo nextest run -p oxicode-agent --lib tools`
Expected: ALL PASS — including every pre-existing `test_grep_*` test. They run the LEGACY engine via `with_builtins_cwd`-style `GrepTool::new()`… wait: existing tests construct `GrepTool::new()` directly, which is now NATIVE. Audit each existing test in `tools.rs` (`test_grep_*`, ~10 tests) and `edge_cases.rs`: they use temp dirs with no ignore files, so native and legacy agree on visible semantics (hidden skip, artifact-dir skip, include globs, context format). Expected PASS without edits; if a test asserted non-obvious fs-ordering across files, make it order-tolerant (assert `contains`), never weaken semantics.

- [ ] **Step 5: Full-crate gate**

Run: `cargo fmt --all && cargo clippy -p oxicode-agent --all-targets -- -D warnings && cargo nextest run -p oxicode-agent`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add oxicode-agent/src/tools/grep.rs oxicode-agent/src/tools.rs
git commit -m "feat(agent): GrepTool v2 delegates to ExactSearchEngine; legacy walker behind legacy_with_cwd"
```

### Task 5: Behavior pack `grep.search.v2` + fixture + ledger

**Files:**
- Modify: `oxicode-sdk/src/behavior/packs/coding_omp_v1.rs` (descriptor + ledger entry)
- Create: `oxicode-sdk/tests/behavior/grep_fixture.rs`
- Modify: `oxicode-sdk/tests/behavior/main.rs` (add `mod grep_fixture;`)

**Interfaces:**
- Consumes: `GrepTool::with_cwd` (native v2) from Task 4; `install_pack_with_services`, `RecordingInstaller` from `tests/behavior/common/mod.rs`.
- Produces: fixture evidence id `behavior::grep_search_v2_contract` referenced by the ledger.

- [ ] **Step 1: Bump the descriptor**

In `coding_omp_v1.rs`, replace the grep tool entry (mirroring the `bash.session.v1` replacement pattern):

```rust
        // grep: v2 routes through ExactSearchEngine (ignore-aware, streaming,
        // cancellable). Semantics changed with the engine, so the
        // implementation id is bumped and declares the lineage per the
        // replacement policy.
        .with_tool(
            descriptor("grep.search.v2", "grep")
                .capability(CapabilityClass::Search)
                .side_effect(SideEffectClass::ReadOnly)
                .replaces("grep.search.v1")
                .essential(),
            simple_tool(|p| Arc::new(GrepTool::with_cwd(p.to_path_buf()))),
        )?
```

- [ ] **Step 2: Update the ledger**

In `ledger()`'s `read-write-search` entry: add `"behavior::grep_search_v2_contract"` to `evidence`, extend `notes`: `"grep.search.v2 (replaces grep.search.v1) backs the exposed grep tool with ExactSearchEngine: ignore-file-aware, streaming, cancellable; v1 walker remains only via with_builtins_cwd until 0.83."`.

- [ ] **Step 3: Write the fixture** — `grep_fixture.rs`:

```rust
//! grep.search.v2 fixture (`behavior::grep_search_v2_contract`): the
//! pack-installed `grep` tool routes through ExactSearchEngine — ignore-file
//! awareness, built-in artifact exclusions, v1 output format, cancellation
//! marker.
use crate::common::*;

use oxicode_agent::{AgentTool, ToolContext};

fn v2_workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().to_path_buf();
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join(".ignore"), "ignored.txt\n").unwrap();
    std::fs::write(ws.join("src/lib.rs"), "needle_fn() {}\n").unwrap();
    std::fs::write(ws.join("ignored.txt"), "needle_fn() in ignored\n").unwrap();
    std::fs::create_dir_all(ws.join("node_modules/pkg")).unwrap();
    std::fs::write(ws.join("node_modules/pkg/i.js"), "needle_fn() in nm\n").unwrap();
    (dir, ws)
}

#[tokio::test]
async fn grep_search_v2_contract() {
    let (_dir, ws) = v2_workspace();
    let services = minimal_services(&ws);
    let mut installer = RecordingInstaller::new(WrapMode::Trace);
    let (manifest, _patch) = install_pack_with_services(&services, &mut installer);

    // No degradation either way for this fixture; the point is the tool.
    assert!(manifest.degraded.is_empty() || manifest.degraded.iter().all(|d| d.feature != "grep"));

    // Fetch the pack-installed `grep` from the installer's registry.
    let registry = installer.registry();
    let tool = registry.get("grep").expect("grep.search.v2 installed");

    let ctx = ToolContext::new(&ws);
    let result = tool
        .execute("t0", serde_json::json!({ "pattern": "needle_fn" }), None, &ctx)
        .await
        .unwrap();
    let text = result.text(); // mirror the accessor used by other fixtures
    assert!(text.contains("src/lib.rs:1: needle_fn() {}"), "{text}");
    assert!(!text.contains("ignored.txt"), ".ignore must be honored: {text}");
    assert!(!text.contains("node_modules"), "artifact dirs must be skipped: {text}");

    // Context rendering keeps the v1 format (`path-N- text`).
    let result = tool
        .execute(
            "t1",
            serde_json::json!({ "pattern": "needle_fn", "context": 1 }),
            None,
            &ctx,
        )
        .await
        .unwrap();
    let text = result.text();
    assert!(text.contains("src/lib.rs:1: needle_fn() {}"), "{text}");

    // include glob (real globset semantics).
    let result = tool
        .execute(
            "t2",
            serde_json::json!({ "pattern": "needle_fn", "include": "*.txt" }),
            None,
            &ctx,
        )
        .await
        .unwrap();
    let text = result.text();
    assert!(!text.contains("lib.rs"), "{text}");
}
```

Implementer notes: (a) `RecordingInstaller` registry accessor — read `common/mod.rs:119-176`; the struct "hands back a registry"; use its actual method (`registry()`, `take_registry()`, or similar). If only per-tool capture exists, extend `RecordingInstaller` with `pub fn registry(&self) -> Arc<ToolRegistry>` in the same commit. (b) `result.text()` — mirror whatever neighboring fixtures use to read tool output. (c) The degraded-assertion is a sanity guard, not the contract.

- [ ] **Step 4: Register the module** — add `mod grep_fixture;` to `tests/behavior/main.rs` (alphabetical: after `eval_fixture`, before `hashline_fixture`).

- [ ] **Step 5: Run**

Run: `cargo nextest run -p oxicode-sdk --test behavior grep_fixture`
Expected: PASS.

- [ ] **Step 6: Full behavior suite + gates**

Run: `cargo fmt --all && cargo clippy -p oxicode-sdk --all-targets -- -D warnings && cargo nextest run -p oxicode-sdk`
Expected: clean — the suite includes `duplicate_fixture` (rejects duplicate implementation ids) and degradation fixtures which must still hold.

- [ ] **Step 7: Commit**

```bash
git add oxicode-sdk/src/behavior/packs/coding_omp_v1.rs oxicode-sdk/tests/behavior/grep_fixture.rs oxicode-sdk/tests/behavior/main.rs
git commit -m "feat(sdk): grep.search.v2 replaces grep.search.v1 with fixture-backed ledger evidence"
```

### Task 6: Benchmark + record numbers

**Files:**
- Create: `oxicode-agent/examples/grep_bench.rs`

**Interfaces:**
- Consumes: `GrepTool::with_cwd` (native) and `GrepTool::legacy_with_cwd` from Task 4.

- [ ] **Step 1: Write the harness**:

```rust
//! Manual benchmark: legacy walker vs ExactSearchEngine on a real tree.
//! Run: cargo run --release -p oxicode-agent --example grep_bench -- <path> [pattern]
//! rg yardstick (manual): rg -c <pattern> <path>

use oxicode_agent::tools::{AgentTool, GrepTool, ToolContext};
use std::time::Instant;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: grep_bench <path> [pattern]");
    let pattern = args.next().unwrap_or_else(|| "fn execute".to_string());

    for (name, tool) in [
        ("legacy", GrepTool::legacy_with_cwd(path.clone().into())),
        ("v2     ", GrepTool::with_cwd(path.clone().into())),
    ] {
        let ctx = ToolContext::new(&path);
        let params = serde_json::json!({ "pattern": pattern, "max_results": 500 });
        // warmup
        let _ = tool.execute("b", params.clone(), None, &ctx).await;
        let mut best = u128::MAX;
        for _ in 0..5 {
            let t = Instant::now();
            let r = tool.execute("b", params.clone(), None, &ctx).await.unwrap();
            let ms = t.elapsed().as_millis();
            if ms < best {
                best = ms;
            }
            let _ = r;
        }
        println!("{name}: best {best} ms");
    }
}
```

(Adjust constructor/serde imports to whatever `oxicode_agent` re-exports; `serde_json` is a dependency of the crate so examples can use it.)

- [ ] **Step 2: Run on this repo + a small fixture, record numbers**

Run: `cargo run --release -p oxicode-agent --example grep_bench -- . needle_fn` and `-- . "fn execute"`.
Manual rg yardstick: `time rg -c "fn execute" .`.
Expected shape: legacy in the hundreds-of-ms to seconds; v2 in the tens-to-hundreds of ms; rg as the yardstick. Record the table (path, pattern, legacy ms, v2 ms, rg ms, repo file count 906 / 19.8 MB) into the design doc under "Acceptance criteria and measurement → Exact search".

- [ ] **Step 3: Commit**

```bash
git add oxicode-agent/examples/grep_bench.rs docs/designs/2026-09-03-workspace-retrieval-zvec-grep.md
git commit -m "chore(agent): grep v2 vs legacy benchmark harness and recorded numbers"
```

### Task 7: Docs + status flip

**Files:**
- Modify: `docs/designs/2026-09-03-workspace-retrieval-zvec-grep.md` (Status → Phase 1 Adopted)
- Modify: `CHANGELOG.md` (unreleased section)
- Modify: `AGENTS.md` (pitfall note)

- [ ] **Step 1: Design doc** — `**Status:** Proposed` → `**Status:** Adopted for Phase 1 (exact-search v2 shipped; Phase 2 skeleton tracked separately)`.
- [ ] **Step 2: CHANGELOG** — under `[Unreleased]` / `### Added`: "`grep.search.v2` — `grep` now runs on a streaming, ignore-aware, cancellable ExactSearchEngine (replaces `grep.search.v1`; gitignore/.ignore/global/exclude respected, built-in artifact exclusions kept, real glob `include`, cancellation returns partial results with a `cancelled` marker). The legacy walker remains reachable via `ToolRegistry::with_builtins_cwd` until 0.83." Under `### Changed`: "Behavior change: files matched by repository ignore files no longer appear in `grep` results; pass an explicit `path` to search ignored trees."
- [ ] **Step 3: AGENTS.md** — one paragraph under Pitfalls: grep v2 semantics (ignore-aware + artifact list + explicit-path escape hatch; legacy walker in `with_builtins_cwd` until 0.83; `grep.search.v2` replaces v1, ledger fixture `behavior::grep_search_v2_contract`).
- [ ] **Step 4: Commit**

```bash
git add docs/designs/2026-09-03-workspace-retrieval-zvec-grep.md CHANGELOG.md AGENTS.md
git commit -m "docs: exact-search v2 adoption notes, changelog, agent pitfalls"
```
