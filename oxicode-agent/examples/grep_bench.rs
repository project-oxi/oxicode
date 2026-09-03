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
