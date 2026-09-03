//! ExactSearchEngine — streaming, ignore-aware, cancellable file search.
//!
//! Private engine behind `GrepTool` v2 (`grep.search.v2`). Owns traversal
//! (`ignore` WalkParallel), streaming line matching, context coalescing,
//! output budget, binary detection, and cancellation. `GrepTool` keeps the
//! schema, PathGuard, renderer, and internal-URL path.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

/// Root-relative path used for include matching and rendering. When `path`
/// IS the root (explicit file root), `strip_prefix` yields an empty path and
/// the file name is the sensible relative path.
fn rel_to_root<'a>(path: &'a Path, root: &Path) -> &'a Path {
    let rel = path.strip_prefix(root).unwrap_or(path);
    if rel.as_os_str().is_empty() {
        path.file_name().map(Path::new).unwrap_or(rel)
    } else {
        rel
    }
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

    let blocks: Arc<parking_lot::Mutex<Vec<FileBlock>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
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

    let walker = builder.build_parallel();
    walker.run(|| {
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
                return ignore::WalkState::Quit;
            }
            if cancel.load(Ordering::Relaxed) {
                cancelled.store(true, Ordering::Relaxed);
                done.store(true, Ordering::Relaxed);
                return ignore::WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return ignore::WalkState::Continue;
            };
            let ft = match entry.file_type() {
                Some(ft) => ft,
                None => return ignore::WalkState::Continue,
            };
            if ft.is_dir() {
                if let Some(name) = entry.file_name().to_str()
                    && ARTIFACT_DIRS.contains(&name)
                {
                    return ignore::WalkState::Skip;
                }
                return ignore::WalkState::Continue;
            }
            if !ft.is_file() {
                return ignore::WalkState::Continue; // symlinks etc. — not followed
            }
            let path = entry.path();
            if looks_binary(path) {
                return ignore::WalkState::Continue;
            }
            if let Some(m) = &include_matcher {
                let rel = rel_to_root(path, &root);
                if !m.is_match(rel) {
                    return ignore::WalkState::Continue;
                }
            }
            let result = search_file(path, &root, &re, req_ctx, req_max, &budget, &done);
            if result.truncated_any {
                lines_truncated.store(true, Ordering::Relaxed);
            }
            if !result.block.lines.is_empty() {
                blocks.lock().push(result.block);
            }
            ignore::WalkState::Continue
        })
    });

    let mut blocks: Vec<FileBlock> = {
        let mut guard = blocks.lock();
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
        block: FileBlock {
            rel,
            lines: Vec::new(),
        },
        truncated_any: false,
    };
    let rel = rel_to_root(path, root).to_string_lossy().to_string();
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
        let start = if context > 0 {
            m.saturating_sub(context)
        } else {
            m
        };
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
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
        assert!(
            out.output.starts_with("Found 2 matches:\n"),
            "{}",
            out.output
        );
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
        let case_insensitive_out = run(&root, &r);
        assert!(case_insensitive_out.output.contains("ALPHA"));
        assert!(
            case_insensitive_out
                .output
                .contains("notes.txt:1: alpha here")
        );
        r.case_insensitive = false;
        // Case-sensitive: the ALPHA file must drop out (notes.txt still has
        // a lowercase "alpha", which legitimately keeps matching).
        let case_sensitive_out = run(&root, &r);
        assert!(!case_sensitive_out.output.contains("src/lib.rs"));
        assert!(
            case_sensitive_out
                .output
                .contains("notes.txt:1: alpha here")
        );
    }

    #[test]
    fn invalid_regex_is_typed_error() {
        let (_d, root) = ws();
        let err = search(&root, &req("(unclosed"), Arc::new(AtomicBool::new(false))).unwrap_err();
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
}
