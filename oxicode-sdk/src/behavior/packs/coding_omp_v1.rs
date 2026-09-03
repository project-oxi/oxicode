//! `coding-omp-v1` — the reference OMP-compatible coding behavior pack.
//!
//! Canonical tool set, runtime extension requirements, prompt discipline
//! layer, and the honest OMP compatibility ledger (design:
//! `docs/designs/2026-08-31-omp-compatible-behavior-pack-design.md`).
//!
//! Compatibility target pinned by release: `omp@v18.0.11
//! (can1357/oh-my-pi@b8ce33a)`.

use std::sync::Arc;

use crate::behavior::installer::BehaviorSessionServices;
use oxicode_agent::AgentTool;
use oxicode_agent::tools::ast_edit::AstEditTool;
use oxicode_agent::tools::ast_grep::AstGrepTool;
use oxicode_agent::tools::bash::BashTool;
use oxicode_agent::tools::bash_session::SessionBashTool;
use oxicode_agent::tools::debug_tool::{DapDebugTool, DebugTool};
use oxicode_agent::tools::edit::EditTool;
use oxicode_agent::tools::eval_tool::{EvalTool, KernelEvalTool};
use oxicode_agent::tools::find::FindTool;
use oxicode_agent::tools::grep::GrepTool;
use oxicode_agent::tools::ls::LsTool;
use oxicode_agent::tools::lsp::LspTool;
use oxicode_agent::tools::read::ReadTool;
use oxicode_agent::tools::search_cache::{GetSearchResultsTool, SearchCache};
use oxicode_agent::tools::subagent::SubagentTool;
use oxicode_agent::tools::todo::TodoTool;
use oxicode_agent::tools::web_search::WebSearchTool;
use oxicode_agent::tools::write::WriteTool;

use super::super::ledger::{CompatibilityContract, FeatureStatus, LedgerEntry};
use super::super::types::{
    BehaviorInstallError, BehaviorPack, BehaviorPackId, BehaviorToolDescriptor, CapabilityClass,
    ExtensionKind, ExtensionScope, PortRequirementKind, PromptLayerSpec, RuntimeExtensionSpec,
    SideEffectClass, ToolFactory, ToolStateScope,
};

/// Prompt-layer id of the coding discipline fragment.
pub const DISCIPLINE_LAYER_ID: &str = "coding-omp-v1/discipline";

/// Coding discipline prompt body installed with the pack.
pub const DISCIPLINE_BODY: &str = "\
You are operating under the coding-omp-v1 behavior pack.

Editing discipline:
- Read a file before editing it. Edits anchor to `[path#TAG]` snapshots; after any
  external change, re-read to refresh anchors.
- Prefer anchored line edits over whole-file rewrites.
- After edits, verify: compile, run the relevant test, or show the diff.

Execution discipline:
- Prefer focused commands with explicit output; check exit codes before proceeding.
- Keep long-running services in managed background processes, not one-shot calls.";

/// The pinned OMP compatibility target.
pub const OMP_TARGET: &str = "omp@v18.0.11 (can1357/oh-my-pi@b8ce33a)";

fn entry(feature: &str, status: FeatureStatus, evidence: &[&str], notes: &str) -> LedgerEntry {
    LedgerEntry {
        feature: feature.to_string(),
        status,
        evidence: evidence.iter().map(|s| s.to_string()).collect(),
        notes: notes.to_string(),
    }
}

/// The honest initial compatibility ledger (design table; statuses may only
/// advance when the named fixture scenarios pass).
fn ledger() -> CompatibilityContract {
    CompatibilityContract {
        target: OMP_TARGET.to_string(),
        entries: vec![
            entry(
                "read-write-search",
                FeatureStatus::Equivalent,
                &[
                    "behavior::hashline_read_edit_stale_anchor_recovery",
                    "behavior::grep_search_v2_contract",
                ],
                "File read/write/grep/find/ls exercised through pack-installed registry with \
                 scripted transcripts; no OMP-specific deviation observed. \
                 grep.search.v2 (replaces grep.search.v1) backs the exposed grep tool with \
                 ExactSearchEngine: ignore-file-aware, streaming, cancellable; v1 walker \
                 remains only via with_builtins_cwd until 0.83.",
            ),
            entry(
                "hashline-anchors",
                FeatureStatus::Equivalent,
                &["behavior::hashline_read_edit_stale_anchor_recovery"],
                "Anchored edits via hashline::SnapshotStore; snapshots are session-local \
                 in-memory (bounded). Cross-session persistence intentionally deferred.",
            ),
            entry(
                "lsp",
                FeatureStatus::Partial,
                &["behavior::lsp_mock_actions"],
                "Generic LspProvider port + CLI rust-analyzer discovery only; no broad \
                 default-server matrix or rename-with-file-operations scenario yet.",
            ),
            entry(
                "persistent-shell",
                FeatureStatus::Equivalent,
                &[
                    "behavior::persistent_shell_session_contract",
                    "behavior::routed_bash_session_persistence",
                ],
                "bash.session.v1 (replaces bash.process.v1) routes the exposed bash tool \
                 through PersistentShellSession when the host provides a ShellSession: cwd/env \
                 persist across separate tool calls, abort surfaces 130 while the session \
                 survives, output is bounded. Without the service the tool falls back to the \
                 legacy per-invocation implementation and the manifest degrades honestly.",
            ),
            entry(
                "persistent-eval",
                FeatureStatus::Equivalent,
                &[
                    "behavior::persistent_eval_kernel_contract",
                    "behavior::routed_eval_kernel_persistence",
                ],
                "eval.kernel.v2 (replaces eval.kernel.v1) routes the exposed eval tool through \
                 the host's EvalKernel services (python3 / node+fallback): state persists across \
                 cells, errors are captured as cell output, `reset` drops kernel state. Without \
                 kernels the tool falls back to the legacy fresh-process implementation.",
            ),
            entry(
                "dap-debugging",
                FeatureStatus::Equivalent,
                &[
                    "behavior::dap_service_protocol_scenario",
                    "behavior::routed_debug_dap_lifecycle",
                ],
                "debug.dap.v2 (replaces debug.dap.v1) routes the exposed debug tool through \
                 DapDebugService: launch/attach start real DAP sessions and breakpoint, \
                 stepping, inspection, evaluation, and termination issue live DAP requests. \
                 Proven against a scripted python3 adapter; real-adapter coverage (gdb, \
                 lldb-dap, debugpy, dlv) remains fixture work, and without a DebugService the \
                 tool falls back to the validated guidance scaffold.",
            ),
            entry(
                "ttsr",
                FeatureStatus::Partial,
                &["behavior::ttsr_patch_and_rule_retry"],
                "TtsrEngine + RuleRegistry ports exist and patch wiring is contract-tested; \
                 hosts ship no rules by default.",
            ),
            entry(
                "delegation",
                FeatureStatus::Partial,
                &["behavior::child_agent_runner_contract"],
                "SubagentRunner injection is contract-tested; typed child task context and \
                 inherited-limit enforcement remain host-side.",
            ),
            entry(
                "host-product-tools",
                FeatureStatus::NotApplicable,
                &[],
                "MCP, memory, github, commit and other product tools remain host-composition \
                 concerns, not pack tools.",
            ),
        ],
    }
}

fn descriptor(id: &str, exposed: &str) -> BehaviorToolDescriptor {
    BehaviorToolDescriptor::new(id, exposed)
}

fn simple_tool<F>(f: F) -> ToolFactory
where
    F: Fn(&Path) -> Arc<dyn AgentTool> + Send + Sync + 'static,
{
    Arc::new(move |services| Ok(f(services.workspace_root.as_path())))
}

/// The `coding-omp-v1` pack: 16 canonical coding tools, seven declared
/// extensions (HashlineState required), one prompt layer, and the pinned
/// compatibility ledger.
///
/// # Errors
///
/// Errors when a tool implementation id is registered twice (a programming
/// error in this module).
pub fn pack() -> Result<BehaviorPack, BehaviorInstallError> {
    let cache = Arc::new(SearchCache::new());
    let web_factory: ToolFactory = {
        let cache = cache.clone();
        Arc::new(move |_| Ok(Arc::new(WebSearchTool::new(cache.clone())) as Arc<dyn AgentTool>))
    };
    let results_factory: ToolFactory = {
        let cache = cache.clone();
        Arc::new(move |_| {
            Ok(Arc::new(GetSearchResultsTool::new(cache.clone())) as Arc<dyn AgentTool>)
        })
    };

    BehaviorPack::new(BehaviorPackId::coding_omp_v1(), OMP_TARGET.to_string())
        .with_prompt_layer(PromptLayerSpec {
            id: DISCIPLINE_LAYER_ID.to_string(),
            body: DISCIPLINE_BODY.to_string(),
        })
        .with_compatibility(ledger())
        // Extensions — HashlineState is REQUIRED: a host that cannot supply a
        // snapshot store fails pack resolution loudly (design: "reject a
        // required tool, causing pack resolution to fail before an agent
        // turn begins").
        .with_extension(RuntimeExtensionSpec {
            kind: ExtensionKind::HashlineState,
            scope: ExtensionScope::SessionWorkspace,
            required: true,
        })
        .with_extension(RuntimeExtensionSpec {
            kind: ExtensionKind::LspHost,
            scope: ExtensionScope::Workspace,
            required: false,
        })
        .with_extension(RuntimeExtensionSpec {
            kind: ExtensionKind::ShellSession,
            scope: ExtensionScope::SessionWorkspace,
            required: false,
        })
        .with_extension(RuntimeExtensionSpec {
            kind: ExtensionKind::EvalKernel,
            scope: ExtensionScope::SessionLanguage,
            required: false,
        })
        .with_extension(RuntimeExtensionSpec {
            kind: ExtensionKind::DebugService,
            scope: ExtensionScope::WorkspaceDebugTarget,
            required: false,
        })
        .with_extension(RuntimeExtensionSpec {
            kind: ExtensionKind::TtsrEngine,
            scope: ExtensionScope::Turn,
            required: false,
        })
        .with_extension(RuntimeExtensionSpec {
            kind: ExtensionKind::Delegation,
            scope: ExtensionScope::ChildAgentLifecycle,
            required: false,
        })
        // Tools — declaration order = install order.
        .with_tool(
            descriptor("read.file.v1", "read")
                .capability(CapabilityClass::FsRead)
                .side_effect(SideEffectClass::ReadOnly)
                .state_scope(ToolStateScope::HashlineSession)
                .essential(),
            simple_tool(|p| Arc::new(ReadTool::with_cwd(p.to_path_buf()))),
        )?
        .with_tool(
            descriptor("write.file.v1", "write")
                .capability(CapabilityClass::FsWrite)
                .side_effect(SideEffectClass::Mutating)
                .state_scope(ToolStateScope::HashlineSession)
                .essential(),
            simple_tool(|p| Arc::new(WriteTool::with_cwd(p.to_path_buf()))),
        )?
        .with_tool(
            descriptor("edit.hashline.v1", "edit")
                .capability(CapabilityClass::FsWrite)
                .side_effect(SideEffectClass::Mutating)
                .state_scope(ToolStateScope::HashlineSession)
                .port(PortRequirementKind::HashlineSnapshotStore, true)
                .essential(),
            simple_tool(|p| Arc::new(EditTool::with_cwd(p.to_path_buf()))),
        )?
        // bash: routed through the host's persistent ShellSession when one
        // is provided (OMP-style session semantics — cwd/env persist);
        // legacy per-invocation fallback otherwise. Semantics changed with
        // the routing, so the implementation id is bumped and declares the
        // lineage per the replacement policy.
        .with_tool(
            descriptor("bash.session.v1", "bash")
                .capability(CapabilityClass::Process)
                .side_effect(SideEffectClass::ProcessSpawning)
                .state_scope(ToolStateScope::ShellSession)
                .port(PortRequirementKind::ShellSession, false)
                .replaces("bash.process.v1")
                .essential(),
            {
                let legacy = simple_tool(|p| Arc::new(BashTool::with_cwd(p.to_path_buf())));
                Arc::new(move |services: &BehaviorSessionServices| {
                    match services.shell_session.clone() {
                        Some(session) => {
                            Ok(Arc::new(SessionBashTool::new(session)) as Arc<dyn AgentTool>)
                        }
                        None => legacy(services),
                    }
                }) as ToolFactory
            },
        )?
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
        .with_tool(
            descriptor("find.search.v1", "find")
                .capability(CapabilityClass::Search)
                .side_effect(SideEffectClass::ReadOnly)
                .essential(),
            simple_tool(|p| Arc::new(FindTool::with_cwd(p.to_path_buf()))),
        )?
        .with_tool(
            descriptor("ls.fs.v1", "ls")
                .capability(CapabilityClass::FsRead)
                .side_effect(SideEffectClass::ReadOnly)
                .essential(),
            simple_tool(|p| Arc::new(LsTool::with_cwd(p.to_path_buf()))),
        )?
        .with_tool(
            descriptor("ast-grep.search.v1", "ast_grep")
                .capability(CapabilityClass::Search)
                .side_effect(SideEffectClass::ReadOnly),
            simple_tool(|p| Arc::new(AstGrepTool::with_cwd(p.to_path_buf()))),
        )?
        .with_tool(
            descriptor("ast-edit.write.v1", "ast_edit")
                .capability(CapabilityClass::FsWrite)
                .side_effect(SideEffectClass::Mutating)
                .state_scope(ToolStateScope::Workspace),
            Arc::new(|_| Ok(Arc::new(AstEditTool::new()) as Arc<dyn AgentTool>)),
        )?
        .with_tool(
            descriptor("web-search.network.v1", "web_search")
                .capability(CapabilityClass::Network)
                .side_effect(SideEffectClass::Networked),
            web_factory,
        )?
        .with_tool(
            descriptor("search-results.cache.v1", "get_search_results")
                .capability(CapabilityClass::Search)
                .side_effect(SideEffectClass::ReadOnly),
            results_factory,
        )?
        .with_tool(
            descriptor("todo.session.v1", "todo")
                .capability(CapabilityClass::Ui)
                .side_effect(SideEffectClass::Mutating)
                .state_scope(ToolStateScope::Workspace)
                .port(PortRequirementKind::TodoStateProvider, false),
            Arc::new(|_| Ok(Arc::new(TodoTool) as Arc<dyn AgentTool>)),
        )?
        .with_tool(
            descriptor("subagent.delegation.v1", "subagent")
                .capability(CapabilityClass::Delegation)
                .side_effect(SideEffectClass::ProcessSpawning)
                .port(PortRequirementKind::SubagentRunner, false),
            simple_tool(|p| Arc::new(SubagentTool::with_cwd(p.to_path_buf()))),
        )?
        .with_tool(
            descriptor("lsp.host.v1", "lsp")
                .capability(CapabilityClass::Lsp)
                .side_effect(SideEffectClass::ReadOnly)
                .port(PortRequirementKind::LspProvider, false),
            Arc::new(|_| Ok(Arc::new(LspTool) as Arc<dyn AgentTool>)),
        )?
        // eval: persistent kernels when the host provides them (state
        // persists across cells, `reset` drops kernel state); legacy
        // fresh-process fallback otherwise. Id bumped per the replacement
        // policy — routed cells change the exposed semantics.
        .with_tool(
            descriptor("eval.kernel.v2", "eval")
                .capability(CapabilityClass::Process)
                .side_effect(SideEffectClass::ProcessSpawning)
                .state_scope(ToolStateScope::EvalKernel)
                .port(PortRequirementKind::EvalKernel, false)
                .replaces("eval.kernel.v1"),
            Arc::new(|services: &BehaviorSessionServices| {
                if services.eval_kernels.is_empty() {
                    Ok(Arc::new(EvalTool) as Arc<dyn AgentTool>)
                } else {
                    Ok(Arc::new(KernelEvalTool::new(services.eval_kernels.clone()))
                        as Arc<dyn AgentTool>)
                }
            }),
        )?
        // debug: real DAP sessions when the host provides a DebugService;
        // the validated guidance scaffold otherwise. Id bumped per the
        // replacement policy.
        .with_tool(
            descriptor("debug.dap.v2", "debug")
                .capability(CapabilityClass::Process)
                .side_effect(SideEffectClass::ProcessSpawning)
                .state_scope(ToolStateScope::DebugTarget)
                .port(PortRequirementKind::DebugService, false)
                .replaces("debug.dap.v1"),
            Arc::new(
                |services: &BehaviorSessionServices| match services.debug_service.clone() {
                    Some(service) => Ok(Arc::new(DapDebugTool::new(service)) as Arc<dyn AgentTool>),
                    None => Ok(Arc::new(DebugTool) as Arc<dyn AgentTool>),
                },
            ),
        )
}

use std::path::Path;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::behavior::installer::BehaviorSessionServices;
    use crate::behavior::types::BehaviorInstallError;
    use oxicode_hashline::InMemorySnapshotStore;
    use parking_lot::Mutex;
    use std::sync::atomic::AtomicUsize;

    struct CountingInstaller {
        installed: Mutex<Vec<String>>,
        calls: AtomicUsize,
    }

    impl CountingInstaller {
        fn new() -> Self {
            Self {
                installed: Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl crate::behavior::installer::BehaviorToolInstaller for CountingInstaller {
        fn install(
            &mut self,
            descriptor: &BehaviorToolDescriptor,
            _tool: Arc<dyn AgentTool>,
        ) -> Result<(), BehaviorInstallError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.installed.lock().push(descriptor.exposed_name.clone());
            Ok(())
        }
    }

    fn expected_names() -> Vec<&'static str> {
        vec![
            "read",
            "write",
            "edit",
            "bash",
            "grep",
            "find",
            "ls",
            "ast_grep",
            "ast_edit",
            "web_search",
            "get_search_results",
            "todo",
            "subagent",
            "lsp",
            "eval",
            "debug",
        ]
    }

    #[test]
    fn descriptor_set_matches_design_table() {
        let p = pack().unwrap();
        let names: Vec<&str> = p.tools.iter().map(|t| t.exposed_name.as_str()).collect();
        assert_eq!(names, expected_names());
        let by_name = |n: &str| {
            p.tools
                .iter()
                .find(|t| t.exposed_name == n)
                .unwrap_or_else(|| panic!("missing {n}"))
        };
        // ids
        assert_eq!(by_name("edit").id.0, "edit.hashline.v1");
        assert_eq!(by_name("bash").id.0, "bash.session.v1");
        assert_eq!(by_name("eval").id.0, "eval.kernel.v2");
        assert_eq!(by_name("debug").id.0, "debug.dap.v2");
        // essentials
        for n in ["read", "write", "edit", "bash", "grep", "find", "ls"] {
            assert!(by_name(n).essential, "{n} must be essential");
        }
        for n in [
            "ast_grep",
            "ast_edit",
            "web_search",
            "todo",
            "subagent",
            "lsp",
            "eval",
            "debug",
        ] {
            assert!(!by_name(n).essential, "{n} must be optional");
        }
        // ports
        let edit = by_name("edit");
        assert!(
            edit.required_ports
                .iter()
                .any(|p| p.kind == PortRequirementKind::HashlineSnapshotStore && p.required)
        );
        assert!(
            by_name("bash")
                .required_ports
                .iter()
                .any(|p| p.kind == PortRequirementKind::ShellSession && !p.required)
        );
        assert!(
            by_name("eval")
                .required_ports
                .iter()
                .any(|p| p.kind == PortRequirementKind::EvalKernel && !p.required)
        );
        assert!(
            by_name("debug")
                .required_ports
                .iter()
                .any(|p| p.kind == PortRequirementKind::DebugService && !p.required)
        );
        assert!(
            by_name("lsp")
                .required_ports
                .iter()
                .any(|p| p.kind == PortRequirementKind::LspProvider && !p.required)
        );
        assert!(
            by_name("subagent")
                .required_ports
                .iter()
                .any(|p| p.kind == PortRequirementKind::SubagentRunner && !p.required)
        );
    }

    #[test]
    fn ledger_matches_design_targets() {
        let l = ledger();
        assert!(l.target.starts_with("omp@v18.0.11"));
        assert_eq!(l.entries.len(), 9);
        let status = |f: &str| {
            l.entries
                .iter()
                .find(|e| e.feature == f)
                .unwrap_or_else(|| panic!("missing {f}"))
                .status
        };
        assert_eq!(status("read-write-search"), FeatureStatus::Equivalent);
        assert_eq!(status("hashline-anchors"), FeatureStatus::Equivalent);
        assert_eq!(status("lsp"), FeatureStatus::Partial);
        assert_eq!(status("ttsr"), FeatureStatus::Partial);
        assert_eq!(status("delegation"), FeatureStatus::Partial);
        assert_eq!(status("persistent-shell"), FeatureStatus::Equivalent);
        assert_eq!(status("persistent-eval"), FeatureStatus::Equivalent);
        assert_eq!(status("dap-debugging"), FeatureStatus::Equivalent);
        assert_eq!(status("host-product-tools"), FeatureStatus::NotApplicable);
        assert_eq!(l.rollup(), FeatureStatus::Partial);
        for e in l
            .entries
            .iter()
            .filter(|e| e.status == FeatureStatus::Unavailable)
        {
            assert!(
                e.evidence.is_empty(),
                "{} must carry no evidence",
                e.feature
            );
        }
    }

    #[test]
    fn install_with_minimal_services_degrades_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let p = pack().unwrap();
        let services =
            BehaviorSessionServices::new(dir.path().to_path_buf())
                .with_snapshot_store(Arc::new(InMemorySnapshotStore::new())
                    as Arc<dyn oxicode_hashline::SnapshotStore>);
        let mut installer = CountingInstaller::new();
        let manifest = p.install(&services, &mut installer).unwrap();
        assert_eq!(manifest.tools.len(), 16);
        assert_eq!(
            installer.calls.load(std::sync::atomic::Ordering::SeqCst),
            16
        );
        let mut degraded: Vec<&str> = manifest
            .degraded
            .iter()
            .map(|d| d.feature.as_str())
            .collect();
        degraded.sort_unstable();
        let mut expected = vec![
            "debug-service",
            "delegation",
            "eval-kernel",
            "lsp-host",
            "shell-session",
            "ttsr-engine",
        ];
        expected.sort_unstable();
        assert_eq!(degraded, expected);
        assert_eq!(
            manifest.prompt_layers,
            vec![DISCIPLINE_LAYER_ID.to_string()]
        );
        assert_eq!(manifest.compatibility_level(), FeatureStatus::Partial);
    }

    #[test]
    fn required_hashline_extension_fails_without_store() {
        let dir = tempfile::tempdir().unwrap();
        let p = pack().unwrap();
        let services = BehaviorSessionServices::new(dir.path().to_path_buf());
        let mut installer = CountingInstaller::new();
        let err = p.install(&services, &mut installer).unwrap_err();
        assert!(matches!(
            err,
            BehaviorInstallError::RequiredExtensionMissing {
                kind: ExtensionKind::HashlineState,
                ..
            }
        ));
    }
}
