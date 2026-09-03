//! Live `workspace_search` integration against a real oxibrain binary.
//!
//! Gated: needs `OXICODE_BRAIN_WS_BIN=<path-to-oxibrain>` and runs only
//! under `cargo nextest run -- --ignored` (excluded from normal offline
//! CI). The test initializes an ISOLATED brain store in a temp dir — it
//! never touches `~/.oxi/brain`.
//!
//! Observed store prerequisite (oxibrain 0.12.x CLI): the init verb lives
//! under the admin namespace (`oxibrain admin init --dir <dir>`), and a
//! freshly initialized store contains exactly one space, `personal` —
//! there is no `dev` space. The test therefore registers into `personal`.

#![cfg(unix)]

use oxicode_agent::tools::{AgentTool, ToolContext, WorkspaceSearchTool};

fn bin() -> Option<std::path::PathBuf> {
    std::env::var_os("OXICODE_BRAIN_WS_BIN").map(std::path::PathBuf::from)
}

#[tokio::test]
#[ignore = "requires OXICODE_BRAIN_WS_BIN and a local oxibrain store"]
async fn registers_root_and_returns_ranked_hits() {
    let Some(bin) = bin() else {
        panic!("set OXICODE_BRAIN_WS_BIN to run this test");
    };
    // Isolated store + workspace so the user's real brain is untouched.
    let brain = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(
        ws.path().join("auth.rs"),
        "// session ownership is validated here via flock\nfn require_owner() {}\n",
    )
    .unwrap();

    // The spawned child requires an initialized store. Initialize it first
    // (`admin init` — the init verb lives under the admin namespace on this
    // binary) and assert success before spawning.
    let init = std::process::Command::new(&bin)
        .args(["admin", "init", "--dir", brain.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(
        init.success(),
        "oxibrain admin init on the temp store failed"
    );

    let mut settings = oxicode::store::settings::Settings::default();
    settings.workspace_search.enabled = true;
    settings.workspace_search.executable = Some(bin.display().to_string());
    // A fresh store has exactly one space, `personal` (verified against the
    // pinned binary). Registering into it exercises the real happy path.
    settings.workspace_search.space = "personal".into();

    let backend =
        oxicode::foundation::brain_workspace::create_workspace_search_backend(&settings, ws.path())
            .expect("backend constructed");

    let tool = WorkspaceSearchTool::new(backend);
    let ctx = ToolContext::new(ws.path());
    // The first query reconciles the freshly registered root; the workspace
    // is one file, but allow a generous budget. The engine's lexical channel
    // is an FTS5 implicit-AND over query tokens (no stopword removal) and
    // the child has no embedder (no dense channel), so the query must use
    // tokens that appear in the indexed file.
    let out = tool
        .execute(
            "t",
            serde_json::json!({"query": "session ownership validated flock"}),
            None,
            &ctx,
        )
        .await
        .expect("workspace_search call should succeed against the live store");
    let text = out.output;
    assert!(
        text.contains("auth.rs"),
        "expected a hit in auth.rs: {text}"
    );
    assert!(text.contains("Verify each passage"), "{text}");
}
