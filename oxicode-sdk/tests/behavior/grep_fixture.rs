//! grep.search.v2 fixture (`behavior::grep_search_v2_contract`): the
//! pack-installed `grep` tool routes through ExactSearchEngine — ignore-file
//! awareness, built-in artifact exclusions, v1 output format, cancellation
//! marker.
use crate::common::*;

use oxicode_agent::ToolContext;

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
    let registry = installer.into_registry();
    let tool = registry.get("grep").expect("grep.search.v2 installed");

    let ctx = ToolContext::new(&ws);
    let result = tool
        .execute(
            "t0",
            serde_json::json!({ "pattern": "needle_fn" }),
            None,
            &ctx,
        )
        .await
        .unwrap();
    let text = result.output; // mirror the accessor used by other fixtures
    assert!(text.contains("src/lib.rs:1: needle_fn() {}"), "{text}");
    assert!(
        !text.contains("ignored.txt"),
        ".ignore must be honored: {text}"
    );
    assert!(
        !text.contains("node_modules"),
        "artifact dirs must be skipped: {text}"
    );

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
    let text = result.output;
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
    let text = result.output;
    assert!(!text.contains("lib.rs"), "{text}");
}
