/// Tool execution logic for agent loop
use crate::events::{ToolCallContext, VisitReason};
use crate::{AgentEvent, AgentToolResult};
use anyhow::Result;
use futures::{FutureExt, StreamExt};
use oxicode_ai::{AssistantMessage, Message, ToolCall, ToolResultMessage, progress_callback};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Notify;

/// Build a cancellation [`Notify`] handle for a single tool call, plus a
/// background tokio task that fires `notify_one()` whenever `cancel_signal`
/// transitions to `true`. Tools that opt into cancellation await the
/// returned `Notify` inside a `tokio::select!` against their main work;
/// when the loop's `cancel_signal` flips, the task wakes the tool within
/// ~250 ms (the poll cadence) without requiring the tool to know about
/// the loop's `AtomicBool`.
///
/// Why `Notify` instead of `oneshot::Sender`: the `AgentTool` trait already
/// exposes a `signal: Option<oneshot::Receiver<()>>` parameter, but every
/// call site was passing `None` (audit finding F-8), defeating the contract.
/// This helper re-establishes the contract with a primitive that survives
/// the closure-move semantics of `tokio::spawn` (`oneshot::Sender` cannot
/// be both moved into the spawned task AND returned to the caller).
///
/// `loop_ref` supplies the loop's `cancel_signal` (`Option<Arc<AtomicBool>>`).
/// When `None` (e.g. tests that don't install a cancel flag) the returned
/// `Notify` is never fired; the tool simply awaits it until the call ends.
/// The detached poll task self-terminates when `notify_one` fires.
fn make_cancellation(loop_ref: &super::AgentLoop) -> Arc<Notify> {
    let notify = Arc::new(Notify::new());
    if let Some(flag) = loop_ref.cancel_signal() {
        let notify_for_task = Arc::clone(&notify);
        tokio::spawn(async move {
            loop {
                if flag.load(std::sync::atomic::Ordering::SeqCst) {
                    notify_for_task.notify_one();
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        });
    }
    notify
}

use super::config::{AfterToolCallHook, ToolExecutionMode};
use super::helpers::{FinalizedToolCall, create_tool_result_message, should_terminate_batch};
use crate::tools::ToolContext as ToolExecContext;

// ── Context inference ─────────────────────────────────────────────────────

/// Infer semantic context from tool name and arguments.
///
/// This is the **single point** where the agent loop assigns meaning
/// to tool calls. Tools themselves remain unaware of semantics — they
/// just do their work and emit facts.
fn infer_context(tool_name: &str, args: &serde_json::Value) -> Option<ToolCallContext> {
    match tool_name {
        "web_search" => args["query"].as_str().map(|q| ToolCallContext::WebSearch {
            query: q.into(),
            engine: args["engines"].as_str().map(String::from),
        }),

        "browse" => args["url"].as_str().map(|u| ToolCallContext::PageVisit {
            url: u.into(),
            reason: Some(VisitReason::DirectNavigation),
            page_title: None,
            page_status: None,
            page_bytes: None,
            page_duration_ms: None,
            navigation_error: None,
            screenshot: None,
        }),

        "browse_extract" => Some(ToolCallContext::DataExtraction {
            target: args["selector"].as_str().unwrap_or("data").to_string(),
            url: args["url"].as_str().map(String::from),
            result_count: None,
            page_status: None,
            page_duration_ms: None,
        }),

        "browse_session" => {
            let action = args["action"].as_str().unwrap_or("unknown");
            // "goto" is semantically a page visit, not a generic action.
            if action == "goto" {
                args["url"].as_str().map(|u| ToolCallContext::PageVisit {
                    url: u.into(),
                    reason: Some(VisitReason::DirectNavigation),
                    page_title: None,
                    page_status: None,
                    page_bytes: None,
                    page_duration_ms: None,
                    navigation_error: None,
                    screenshot: None,
                })
            } else {
                Some(ToolCallContext::SessionAction {
                    action: action.to_string(),
                    url: args["url"].as_str().map(String::from),
                })
            }
        }

        "browse_script" => {
            let total = args["steps"].as_array().map(|a| a.len()).unwrap_or(0);
            if total > 0 {
                Some(ToolCallContext::ScriptStep {
                    current: 0,
                    total,
                    step: "starting".into(),
                })
            } else {
                // No structured steps array — script is YAML text.
                // Parsing requires serde_yaml (native-browser feature).
                #[cfg(feature = "native-browser")]
                {
                    args["script"]
                        .as_str()
                        .and_then(|s| serde_yaml::from_str::<serde_yaml::Value>(s).ok())
                        .as_ref()
                        .and_then(|v| v.get("steps").and_then(|s| s.as_sequence()))
                        .map(|steps| ToolCallContext::ScriptStep {
                            current: 0,
                            total: steps.len(),
                            step: "starting".into(),
                        })
                }
                #[cfg(not(feature = "native-browser"))]
                {
                    None
                }
            }
        }

        _ => None,
    }
}

/// Enrich `context_cell` from `AgentToolResult` metadata after execute.
/// Handles `result_count` for `DataExtraction` contexts.
fn enrich_context_from_metadata(
    context_cell: &Arc<parking_lot::Mutex<Option<ToolCallContext>>>,
    result: &AgentToolResult,
) {
    if let Some(ref meta) = result.metadata
        && let Some(count) = meta.get("result_count").and_then(|v| v.as_u64())
    {
        let mut guard = context_cell.lock();
        if let Some(ToolCallContext::DataExtraction { result_count, .. }) = &mut *guard {
            *result_count = Some(count as usize);
        }
    }
}

/// Build a `BrowseProgressCallback` that enriches `context_cell` with
/// structured data from each `BrowseProgress` event.
///
/// Mapping:
/// - `DocumentReady` + `PageVisit` → fills `page_title`, `page_status`,
///   `page_bytes`, `page_duration_ms`; updates `url` if redirected.
/// - `DocumentReady` + `DataExtraction` → fills `page_status`, `page_duration_ms`.
/// - `NavigationFailed` + `PageVisit` → fills `navigation_error`.
/// - `ScreenshotCaptured` + `PageVisit` → fills `screenshot`.
/// - All other combinations → no-op.
fn make_browse_enrichment_cb(
    context_cell: Arc<parking_lot::Mutex<Option<ToolCallContext>>>,
) -> crate::tools::browse::BrowseProgressCallback {
    Arc::new(move |progress: crate::tools::browse::BrowseProgress| {
        let mut guard = context_cell.lock();
        match (&mut *guard, &progress) {
            (
                Some(ToolCallContext::PageVisit {
                    url,
                    page_title,
                    page_status,
                    page_bytes,
                    page_duration_ms,
                    ..
                }),
                crate::tools::browse::BrowseProgress::DocumentReady {
                    url: ready_url,
                    title,
                    status,
                    bytes,
                    duration_ms,
                },
            ) => {
                // Update URL if redirected.
                if url != ready_url {
                    *url = ready_url.clone();
                }
                *page_title = Some(title.clone());
                *page_status = Some(*status);
                *page_bytes = Some(*bytes);
                *page_duration_ms = Some(*duration_ms);
            }
            (
                Some(ToolCallContext::DataExtraction {
                    page_status,
                    page_duration_ms,
                    ..
                }),
                crate::tools::browse::BrowseProgress::DocumentReady {
                    status,
                    duration_ms,
                    ..
                },
            ) => {
                *page_status = Some(*status);
                *page_duration_ms = Some(*duration_ms);
            }

            // ── NavigationFailed → PageVisit.navigation_error ──
            (
                Some(ToolCallContext::PageVisit {
                    navigation_error, ..
                }),
                crate::tools::browse::BrowseProgress::NavigationFailed { error, .. },
            ) => {
                *navigation_error = Some(error.clone());
            }

            // ── ScreenshotCaptured → PageVisit.screenshot ──
            (
                Some(ToolCallContext::PageVisit { screenshot, .. }),
                crate::tools::browse::BrowseProgress::ScreenshotCaptured {
                    bytes,
                    width,
                    duration_ms,
                },
            ) => {
                *screenshot = Some(crate::events::ScreenshotMeta {
                    bytes: *bytes,
                    width: *width,
                    duration_ms: *duration_ms,
                });
            }

            _ => {}
        }
    })
}

pub(crate) struct ExecutedToolCallBatch {
    pub messages: Vec<ToolResultMessage>,
    pub terminate: bool,
}

enum FinalizedToolCallEntry {
    Immediate(Box<FinalizedToolCall>),
    Future(Pin<Box<dyn futures::Future<Output = FinalizedToolCall> + Send>>),
}

pub(crate) struct ExecutedToolCallOutcome {
    pub result: AgentToolResult,
    pub is_error: bool,
}

enum PreparedToolCallKind {
    Immediate,
    Prepared,
}

struct PreparedToolCallOutcome {
    _kind: PreparedToolCallKind,
    immediate_result: Option<AgentToolResult>,
    is_error: bool,
    tool: Option<Arc<dyn crate::tools::AgentTool>>,
    tool_call: ToolCall,
    args: serde_json::Value,
}

/// Check whether a tool call requires approval before execution.
///
/// Returns `None` if approval is not required or was granted.
/// Returns `Some(AgentToolResult)` (error) if denied or approval-required.
/// In the latter case, also emits [`AgentEvent::ApprovalRequired`].
async fn check_tool_approval(
    loop_ref: &super::AgentLoop,
    tool_call_id: &str,
    tool_name: &str,
    args: &serde_json::Value,
    emit: &super::EmitFn,
) -> Option<AgentToolResult> {
    use crate::agent_loop::config::ApprovalDecision;

    let config = &loop_ref.config.approval_config;

    // No tiers configured = no approval gating.
    if config.require_approval_for.is_empty() {
        return None;
    }

    // No hook registered = permissive.
    let hook = config.hook.as_ref()?;

    // Get the tool's tier. Default Exec (safest) if tool not found.
    let tier = loop_ref
        .tools
        .get(tool_name)
        .map(|t| t.tool_tier())
        .unwrap_or(crate::tools::ToolTier::Exec);

    // This tier doesn't require approval.
    if !config.require_approval_for.contains(&tier) {
        return None;
    }

    match hook(tool_name, args).await {
        Ok(ApprovalDecision::Allow) => None,
        Ok(ApprovalDecision::Deny(reason)) => {
            Some(AgentToolResult::error(format!("Access denied: {}", reason)))
        }
        Ok(ApprovalDecision::RequireApproval(reason)) => {
            emit(AgentEvent::ApprovalRequired {
                tool_call_id: tool_call_id.to_string(),
                tool_name: tool_name.to_string(),
                args: args.clone(),
                reason: reason.clone(),
                session_id: loop_ref.session_id.clone(),
            });
            Some(AgentToolResult::error(format!(
                "Approval required: {}",
                reason
            )))
        }
        Err(e) => {
            tracing::warn!(
                tool = %tool_name,
                error = %e,
                "Approval hook failed, allowing tool call"
            );
            None // Allow on hook error (fail open logged).
        }
    }
}

pub(crate) async fn execute_tool_calls(
    loop_ref: &super::AgentLoop,
    messages: &mut Vec<Message>,
    assistant_message: &AssistantMessage,
    tool_calls: Vec<ToolCall>,
    emit: &super::EmitFn,
    ctx: &ToolExecContext,
) -> Result<ExecutedToolCallBatch> {
    if loop_ref.config.tool_execution == ToolExecutionMode::Sequential {
        execute_tool_calls_sequential(loop_ref, messages, assistant_message, tool_calls, emit, ctx)
            .await
    } else {
        execute_tool_calls_parallel(loop_ref, messages, assistant_message, tool_calls, emit, ctx)
            .await
    }
}

async fn execute_tool_calls_sequential(
    loop_ref: &super::AgentLoop,
    _messages: &mut Vec<Message>,
    _assistant_message: &AssistantMessage,
    tool_calls: Vec<ToolCall>,
    emit: &super::EmitFn,
    ctx: &ToolExecContext,
) -> Result<ExecutedToolCallBatch> {
    let mut finalized_calls = Vec::new();
    let mut tool_result_messages = Vec::new();

    for tool_call in tool_calls {
        // Check cancellation before executing each tool.
        // This allows Ctrl+C to interrupt a batch of tool calls
        // without waiting for all of them to complete.
        if loop_ref.is_cancelled() {
            tracing::info!(
                "[TOOL-EXEC] Cancelled before executing tool {}",
                tool_call.name
            );
            break;
        }
        // Clone tool_call fields once upfront to avoid repeated clones.
        let tc_id = tool_call.id.clone();
        let tc_name = tool_call.name.clone();
        let tc_args = tool_call.arguments.clone();

        let intent = loop_ref
            .tools
            .get(&tc_name)
            .and_then(|t| Some(t.intent()?.to_string()));

        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tc_id.clone(),
            tool_name: tc_name.clone(),
            args: tc_args.clone(),
            intent: intent.clone(),
            context: infer_context(&tc_name, &tc_args),
        });

        // Check approval before executing the tool.
        if let Some(approval_result) =
            check_tool_approval(loop_ref, &tc_id, &tc_name, &tc_args, emit).await
        {
            let finalized = FinalizedToolCall {
                tool_call,
                result: approval_result,
                is_error: true,
            };

            let end_intent = loop_ref
                .tools
                .get(&tc_name)
                .and_then(|t| Some(t.intent()?.to_string()));

            emit(AgentEvent::ToolExecutionEnd {
                tool_call_id: finalized.tool_call.id.clone(),
                tool_name: finalized.tool_call.name.clone(),
                intent: end_intent,
                result: oxicode_ai::ToolResult {
                    tool_call_id: finalized.tool_call.id.clone(),
                    content: finalized.result.output.clone(),
                    status: String::from("error"),
                },
                is_error: true,
            });

            let tool_result_message = create_tool_result_message(&finalized);
            let msg = Message::ToolResult(tool_result_message.clone());
            emit(AgentEvent::MessageStart {
                message: msg.clone(),
            });
            emit(AgentEvent::MessageEnd { message: msg });

            finalized_calls.push(finalized);
            tool_result_messages.push(tool_result_message);
            continue;
        }

        let prepared = prepare_tool_call(loop_ref, &tool_call).await;

        let finalized = if let Some(result) = prepared.immediate_result {
            FinalizedToolCall {
                tool_call,
                result,
                is_error: prepared.is_error,
            }
        } else {
            let executed = execute_prepared_tool_call(loop_ref, &prepared, emit, ctx).await;

            let mut result = executed.result;
            let mut is_error = executed.is_error;

            if let Some(hook) = &loop_ref.after_tool_call {
                match hook(&tc_name, &result).await {
                    Ok(Some(modified)) => {
                        if let Some(ref details) = modified.metadata {
                            tracing::debug!(
                                tool = %tc_name,
                                details = %details,
                                "after_tool_call hook returned details"
                            );
                        }
                        result = modified;
                        is_error = !result.success;
                    }
                    Ok(None) => {}
                    Err(hook_err) => {
                        tracing::warn!(
                            tool = %tc_name,
                            error = %hook_err,
                            "after_tool_call hook failed, creating error result"
                        );
                        result = AgentToolResult::error(format!(
                            "after_tool_call hook failed: {hook_err}"
                        ));
                        is_error = true;
                    }
                }
            }

            FinalizedToolCall {
                tool_call,
                result,
                is_error,
            }
        };

        let end_intent = finalized.result.intent.clone().or(intent);

        emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: finalized.tool_call.id.clone(),
            tool_name: finalized.tool_call.name.clone(),
            intent: end_intent,
            result: oxicode_ai::ToolResult {
                tool_call_id: finalized.tool_call.id.clone(),
                content: finalized.result.output.clone(),
                status: if finalized.is_error {
                    String::from("error")
                } else {
                    String::from("success")
                },
            },
            is_error: finalized.is_error,
        });

        let tool_result_message = create_tool_result_message(&finalized);
        let msg = Message::ToolResult(tool_result_message.clone());
        emit(AgentEvent::MessageStart {
            message: msg.clone(),
        });
        emit(AgentEvent::MessageEnd { message: msg });

        finalized_calls.push(finalized);
        tool_result_messages.push(tool_result_message);
    }

    Ok(ExecutedToolCallBatch {
        messages: tool_result_messages,
        terminate: should_terminate_batch(&finalized_calls),
    })
}

async fn execute_tool_calls_parallel(
    loop_ref: &super::AgentLoop,
    _messages: &mut Vec<Message>,
    _assistant_message: &AssistantMessage,
    tool_calls: Vec<ToolCall>,
    emit: &super::EmitFn,
    ctx: &ToolExecContext,
) -> Result<ExecutedToolCallBatch> {
    let mut finalized_calls: Vec<FinalizedToolCallEntry> = Vec::new();

    for tool_call in tool_calls {
        // Check cancellation before preparing each tool.
        if loop_ref.is_cancelled() {
            tracing::info!(
                "[TOOL-EXEC-PARALLEL] Cancelled before preparing tool {}",
                tool_call.name
            );
            break;
        }
        // Clone tool_call fields once upfront to avoid repeated clones.
        let tc_id = tool_call.id.clone();
        let tc_name = tool_call.name.clone();
        let tc_args = tool_call.arguments.clone();

        let intent = loop_ref
            .tools
            .get(&tc_name)
            .and_then(|t| Some(t.intent()?.to_string()));

        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tc_id.clone(),
            tool_name: tc_name.clone(),
            args: tc_args.clone(),
            intent: intent.clone(),
            context: infer_context(&tc_name, &tc_args),
        });

        // Check approval before preparing the tool.
        if let Some(approval_result) =
            check_tool_approval(loop_ref, &tc_id, &tc_name, &tc_args, emit).await
        {
            let finalized = FinalizedToolCall {
                tool_call,
                result: approval_result,
                is_error: true,
            };

            let end_intent = loop_ref
                .tools
                .get(&tc_name)
                .and_then(|t| Some(t.intent()?.to_string()));

            emit(AgentEvent::ToolExecutionEnd {
                tool_call_id: finalized.tool_call.id.clone(),
                tool_name: finalized.tool_call.name.clone(),
                intent: end_intent,
                result: oxicode_ai::ToolResult {
                    tool_call_id: finalized.tool_call.id.clone(),
                    content: finalized.result.output.clone(),
                    status: String::from("error"),
                },
                is_error: true,
            });

            let tool_result_message = create_tool_result_message(&finalized);
            let msg = Message::ToolResult(tool_result_message.clone());
            emit(AgentEvent::MessageStart {
                message: msg.clone(),
            });
            emit(AgentEvent::MessageEnd { message: msg });

            finalized_calls.push(FinalizedToolCallEntry::Immediate(Box::new(finalized)));
            continue;
        }

        let prepared = prepare_tool_call(loop_ref, &tool_call).await;

        if let Some(result) = prepared.immediate_result {
            let finalized = FinalizedToolCall {
                tool_call,
                result,
                is_error: prepared.is_error,
            };

            let end_intent = finalized.result.intent.clone().or(intent);

            emit(AgentEvent::ToolExecutionEnd {
                tool_call_id: finalized.tool_call.id.clone(),
                tool_name: finalized.tool_call.name.clone(),
                intent: end_intent,
                result: oxicode_ai::ToolResult {
                    tool_call_id: finalized.tool_call.id.clone(),
                    content: finalized.result.output.clone(),
                    status: if finalized.is_error {
                        String::from("error")
                    } else {
                        String::from("success")
                    },
                },
                is_error: finalized.is_error,
            });

            finalized_calls.push(FinalizedToolCallEntry::Immediate(Box::new(finalized)));
        } else {
            let tool = prepared.tool.clone();
            let args = prepared.args.clone();
            let after_hook = loop_ref.after_tool_call.clone();
            let emit_clone = emit.clone();
            let ctx_clone = ctx.clone();
            // Pre-build the cancellation notify *outside* the async move
            // closure so `loop_ref` does not need to be `Send + 'static`.
            // The helper only borrows `loop_ref` for the duration of the
            // synchronous body of `make_cancellation` (it returns
            // `Arc<Notify>` plus a detached `tokio::spawn` that owns the
            // flag clone, so by the time this scope ends `loop_ref` is
            // no longer referenced).
            let cancel_notify = make_cancellation(loop_ref);

            finalized_calls.push(FinalizedToolCallEntry::Future(Box::pin(async move {
                let executed = execute_prepared_tool_call_static(
                    tool_call.clone(),
                    tool,
                    args,
                    after_hook.clone(),
                    emit_clone.clone(),
                    &ctx_clone,
                    Some(cancel_notify),
                )
                .await;

                FinalizedToolCall {
                    tool_call,
                    result: executed.result,
                    is_error: executed.is_error,
                }
            })));
        }
    }

    let mut slots: Vec<Option<FinalizedToolCall>> = Vec::with_capacity(finalized_calls.len());
    #[allow(clippy::type_complexity)]
    let mut pending_futures: Vec<(
        usize,
        Pin<Box<dyn futures::Future<Output = FinalizedToolCall> + Send>>,
    )> = Vec::new();

    for (i, entry) in finalized_calls.into_iter().enumerate() {
        match entry {
            FinalizedToolCallEntry::Immediate(f) => slots.push(Some(*f)),
            FinalizedToolCallEntry::Future(f) => {
                slots.push(None);
                pending_futures.push((i, f));
            }
        }
    }

    if !pending_futures.is_empty() {
        // Poll futures with periodic cancel checks.
        // Uses `FuturesUnordered` so we can drain completed results as they
        // arrive and detect cancellation without waiting for all futures.
        let mut active = futures::stream::FuturesUnordered::new();
        for (i, f) in pending_futures {
            active.push(async move { (i, f.await) });
        }

        // Check cancel every 100ms so Ctrl+C is responsive even when
        // tool calls are slow.
        let mut cancel_interval = tokio::time::interval(tokio::time::Duration::from_millis(100));
        cancel_interval.tick().await; // consume immediate first tick

        loop {
            tokio::select! {
                result = active.next() => {
                    match result {
                        Some((idx, finalized)) => {
                            slots[idx] = Some(finalized);
                        }
                        None => break, // all futures completed
                    }
                }
                _ = cancel_interval.tick() => {
                    if loop_ref.is_cancelled() {
                        tracing::info!(
                            "[TOOL-EXEC-PARALLEL] Cancelled during parallel execution, waiting for {} pending futures",
                            active.len()
                        );
                        // Don't abort futures — let them finish (they may have
                        // side effects). But skip waiting and return what we have.
                        break;
                    }
                }
            }
        }

        // Drain any remaining futures that completed before cancellation.
        while let Some(result) = active.next().now_or_never().flatten() {
            slots[result.0] = Some(result.1);
        }
    }

    // Slots for futures that were still running at cancellation time remain None.
    let ordered_finalized_calls: Vec<FinalizedToolCall> = slots.into_iter().flatten().collect();

    let mut tool_result_messages = Vec::new();
    for finalized in &ordered_finalized_calls {
        let tool_result_message = create_tool_result_message(finalized);
        let msg = Message::ToolResult(tool_result_message.clone());
        emit(AgentEvent::MessageStart {
            message: msg.clone(),
        });
        emit(AgentEvent::MessageEnd { message: msg });
        tool_result_messages.push(tool_result_message);
    }

    Ok(ExecutedToolCallBatch {
        messages: tool_result_messages,
        terminate: should_terminate_batch(&ordered_finalized_calls),
    })
}

pub(crate) async fn execute_prepared_tool_call_static(
    tool_call: ToolCall,
    tool: Option<Arc<dyn crate::tools::AgentTool>>,
    args: serde_json::Value,
    after_hook: Option<AfterToolCallHook>,
    emit: Arc<dyn Fn(AgentEvent) + Send + Sync>,
    ctx: &ToolExecContext,
    cancel_notify: Option<Arc<Notify>>,
) -> ExecutedToolCallOutcome {
    let tool_call_id = tool_call.id.clone();
    let tool_name = tool_call.name.clone();
    let static_intent = tool.as_ref().and_then(|t| Some(t.intent()?.to_string()));

    let mut result = AgentToolResult::success("");
    let mut is_error = false;

    if let Some(ref tool) = tool {
        // Infer semantic context — same as sequential path.
        let context = infer_context(&tool_name, &args);

        // Shared context cell for progressive enrichment.
        let context_cell: Arc<parking_lot::Mutex<Option<ToolCallContext>>> =
            Arc::new(parking_lot::Mutex::new(context));

        // Tab ID slot.
        let tab_id_slot: Arc<parking_lot::Mutex<Option<uuid::Uuid>>> =
            Arc::new(parking_lot::Mutex::new(None));
        tool.set_tab_id_slot(Arc::clone(&tab_id_slot));

        // String progress callback.
        let emit_for_cb = emit.clone();
        let tcid = tool_call_id.clone();
        let tn = tool_name.clone();
        let cc = Arc::clone(&context_cell);
        let progress_cb: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |msg: String| {
            let tab_id = *tab_id_slot.lock();
            let ctx = cc.lock().clone();
            emit_for_cb(AgentEvent::ToolExecutionUpdate {
                tool_call_id: tcid.clone(),
                tool_name: tn.clone(),
                partial_result: msg,
                tab_id,
                context: ctx,
            });
        });
        tool.on_progress(progress_callback(move |msg: String| {
            progress_cb(msg);
        }));

        // Browse progress callback — enriches context cell.
        tool.on_browse_progress(make_browse_enrichment_cb(Arc::clone(&context_cell)));
        // F-8 (audit 2026-06-21): see the matching note in
        // `execute_prepared_tool_call`. Same `select!`-based cancellation
        // wrap, same trade-off — additive, no trait change.
        let exec_fut = tool.execute(&tool_call_id, args, None, ctx);
        tokio::pin!(exec_fut);
        let exec_result: Result<AgentToolResult, String> = match cancel_notify {
            Some(notify) => {
                let notify_for_select = Arc::clone(&notify);
                tokio::select! {
                    r = &mut exec_fut => r,
                    _ = notify_for_select.notified() => Err(format!(
                        "tool '{}' cancelled by agent loop",
                        tool_call_id
                    )),
                }
            }
            None => exec_fut.await,
        };
        match exec_result {
            Ok(r) => result = r,
            Err(e) => {
                result = AgentToolResult::error(e);
                is_error = true;
            }
        }

        enrich_context_from_metadata(&context_cell, &result);

        // Drop the progress callback registered above. `on_progress`
        // stores it on the shared, long-lived tool instance (tools are
        // reused across every turn); leaving the closure attached keeps
        // its captured `emit` (and the mpsc `Sender<AgentEvent>` inside
        // it) alive forever, which never lets the event channel close.
        // A run whose forwarder thread blocks on `event_rx.recv()`
        // forever wedges the single-consumer prompt queue — every later
        // queued prompt sits undelivered. Re-registering a no-op drops
        // the previous closure immediately.
        tool.on_progress(progress_callback(|_| {}));
    }

    if let Some(hook) = &after_hook {
        match hook(&tool_call.name, &result).await {
            Ok(Some(modified)) => {
                if let Some(details) = &modified.metadata {
                    tracing::debug!(
                        tool = %tool_call.name,
                        details = %details,
                        "after_tool_call hook returned details"
                    );
                }
                result = modified;
                is_error = !result.success;
            }
            Ok(None) => {}
            Err(hook_err) => {
                tracing::warn!(
                    tool = %tool_call.name,
                    error = %hook_err,
                    "after_tool_call hook failed, creating error result"
                );
                result = AgentToolResult::error(format!("after_tool_call hook failed: {hook_err}"));
                is_error = true;
            }
        }
    }

    let end_intent = result.intent.clone().or(static_intent);

    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: tool_call_id.clone(),
        tool_name: tool_name.clone(),
        intent: end_intent,
        result: oxicode_ai::ToolResult {
            tool_call_id,
            content: result.output.clone(),
            status: if is_error {
                String::from("error")
            } else {
                String::from("success")
            },
        },
        is_error,
    });

    ExecutedToolCallOutcome { result, is_error }
}

async fn prepare_tool_call(
    loop_ref: &super::AgentLoop,
    tool_call: &ToolCall,
) -> PreparedToolCallOutcome {
    let tool = match loop_ref.tools.get(&tool_call.name) {
        Some(t) => t,
        None => {
            return PreparedToolCallOutcome {
                _kind: PreparedToolCallKind::Immediate,
                immediate_result: Some(AgentToolResult::error(format!(
                    "Tool '{}' not found",
                    tool_call.name
                ))),
                is_error: true,
                tool: None,
                tool_call: tool_call.clone(),
                args: tool_call.arguments.clone(),
            };
        }
    };

    // Validate arguments against the tool's JSON Schema before any hook or
    // execution (upstream pi: validateToolArguments). A rejection here never
    // reaches the tool's `execute`; the model sees the validation error and
    // can retry with corrected arguments.
    if loop_ref.config.validate_tool_arguments {
        let schema_tool = oxicode_ai::Tool {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            parameters: tool.parameters_schema(),
        };
        if let Err(error) = oxicode_ai::validate_args(&schema_tool, &tool_call.arguments) {
            return PreparedToolCallOutcome {
                _kind: PreparedToolCallKind::Immediate,
                immediate_result: Some(AgentToolResult::error(format!(
                    "Invalid arguments for tool '{}': {error}",
                    tool_call.name
                ))),
                is_error: true,
                tool: None,
                tool_call: tool_call.clone(),
                args: tool_call.arguments.clone(),
            };
        }
    }

    let validated_args = tool_call.arguments.clone();

    if let Some(hook) = &loop_ref.before_tool_call
        && let Some(blocked) = hook(&tool_call.name, &validated_args).await.ok().flatten()
    {
        return PreparedToolCallOutcome {
            _kind: PreparedToolCallKind::Immediate,
            immediate_result: Some(blocked),
            is_error: true,
            tool: None,
            tool_call: tool_call.clone(),
            args: validated_args,
        };
    }

    PreparedToolCallOutcome {
        _kind: PreparedToolCallKind::Prepared,
        immediate_result: None,
        is_error: false,
        tool: Some(Arc::clone(&tool)),
        tool_call: tool_call.clone(),
        args: validated_args,
    }
}

async fn execute_prepared_tool_call(
    loop_ref: &super::AgentLoop,
    prepared: &PreparedToolCallOutcome,
    emit: &super::EmitFn,
    ctx: &ToolExecContext,
) -> ExecutedToolCallOutcome {
    let tool_call_id = prepared.tool_call.id.clone();
    let tool_name = prepared.tool_call.name.clone();

    let mut result = AgentToolResult::success("");
    let mut is_error = false;

    if let Some(ref tool) = prepared.tool {
        let tool_call_id_clone = tool_call_id.clone();
        let tool_name_clone = tool_name.clone();
        let emit_clone = emit.clone();

        // Infer semantic context from tool name + args.
        let context = infer_context(&tool_name, &prepared.args);

        // Shared mutable context cell. The String progress callback reads
        // from here; the browse progress callback writes enriched fields.
        let context_cell: Arc<parking_lot::Mutex<Option<ToolCallContext>>> =
            Arc::new(parking_lot::Mutex::new(context));

        // Shared slot for the active tab ID. Tools that manage browser tabs
        // (BrowseTool) populate this when they open a tab; the progress
        // callback reads it to include `tab_id` in `ToolExecutionUpdate`.
        let tab_id_slot: Arc<parking_lot::Mutex<Option<uuid::Uuid>>> =
            Arc::new(parking_lot::Mutex::new(None));

        // Pass the slot to the tool so it can write the tab_id.
        tool.set_tab_id_slot(Arc::clone(&tab_id_slot));

        // String progress callback — reads context from shared cell.
        let tab_id_slot_cb = Arc::clone(&tab_id_slot);
        let cc = Arc::clone(&context_cell);
        let progress_cb: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |msg: String| {
            let tab_id = *tab_id_slot_cb.lock();
            let ctx = cc.lock().clone();
            emit_clone(AgentEvent::ToolExecutionUpdate {
                tool_call_id: tool_call_id_clone.clone(),
                tool_name: tool_name_clone.clone(),
                partial_result: msg,
                tab_id,
                context: ctx,
            });
        });

        // Wire up progress callback BEFORE execute — pi-mono: tool's onUpdate
        tool.on_progress(progress_callback(move |msg: String| {
            progress_cb(msg);
        }));

        // Browse progress callback — enriches context cell with structured data.
        // Wire up browse progress callback.
        tool.on_browse_progress(make_browse_enrichment_cb(Arc::clone(&context_cell)));

        // F-8 (audit 2026-06-21): wrap tool execution in a `tokio::select!`
        // against the loop's cancellation notify. Before this, every call
        // site passed `None` for the `signal: Option<oneshot::Receiver<()>>`
        // parameter, defeating the cancellation contract — a long-running
        // tool (e.g. `bash` with a 30s sleep) would only observe a cancel
        // flag on the next 500 ms poll cycle, *after* it had already done
        // most of its work. With `select!`, the tool's `await` is dropped
        // the instant `cancel_signal` flips, returning control to the loop
        // within ~250 ms (the poll cadence in `make_cancellation`).
        //
        // The tool's own `signal` argument is still `None` here — that
        // parameter would require a trait signature change. `select!`
        // cancellation at the call site is the minimal, additive fix
        // that preserves the existing trait contract while honoring
        // cancellation at the *outer* await point. Tools that respect
        // `ctx.cancelled` (most do) still observe the loop's stop flag
        // through `ctx`; tools that ignore it now at least get their
        // outer future cancelled promptly.
        let cancel_notify = make_cancellation(loop_ref);
        let cancel_for_select = Arc::clone(&cancel_notify);
        let tool_call_id_for_exec = tool_call_id.clone();
        let exec_fut = tool.execute(&tool_call_id_for_exec, prepared.args.clone(), None, ctx);
        tokio::pin!(exec_fut);
        let cancelled_msg = format!("tool '{}' cancelled by agent loop", tool_call_id_for_exec);
        let cancelled_msg_for_select = cancelled_msg.clone();
        let exec_result: Result<AgentToolResult, String> = tokio::select! {
            r = &mut exec_fut => r,
            _ = cancel_for_select.notified() => Err(cancelled_msg_for_select),
        };
        match exec_result {
            Ok(r) => result = r,
            Err(e) => {
                result = AgentToolResult::error(e);
                is_error = true;
            }
        }

        enrich_context_from_metadata(&context_cell, &result);

        // See the matching comment in `execute_prepared_tool_call_static`:
        // drop the progress closure (and its captured `emit`/`Sender`)
        // now instead of leaving it wired to the shared tool instance.
        tool.on_progress(progress_callback(|_| {}));
    }

    ExecutedToolCallOutcome { result, is_error }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn infer_context_web_search() {
        let ctx = infer_context("web_search", &json!({ "query": "rust headless browser" }));
        assert!(matches!(
            ctx,
            Some(ToolCallContext::WebSearch { query, .. }) if query == "rust headless browser"
        ));
    }

    #[test]
    fn infer_context_web_search_with_engine() {
        let ctx = infer_context("web_search", &json!({ "query": "rust", "engines": "bing" }));
        assert!(matches!(
            ctx,
            Some(ToolCallContext::WebSearch { engine: Some(e), .. }) if e == "bing"
        ));
    }

    #[test]
    fn infer_context_browse() {
        let ctx = infer_context(
            "browse",
            &json!({ "url": "https://github.com/example/repo" }),
        );
        match ctx {
            Some(ToolCallContext::PageVisit { url, reason, .. }) => {
                assert_eq!(url, "https://github.com/example/repo");
                assert!(matches!(reason, Some(VisitReason::DirectNavigation)));
            }
            other => panic!("expected PageVisit, got {:?}", other),
        }
    }

    #[test]
    fn infer_context_browse_extract() {
        let ctx = infer_context(
            "browse_extract",
            &json!({ "url": "https://example.com", "selector": ".title" }),
        );
        match ctx {
            Some(ToolCallContext::DataExtraction { target, url, .. }) => {
                assert_eq!(target, ".title");
                assert_eq!(url.as_deref(), Some("https://example.com"));
            }
            other => panic!("expected DataExtraction, got {:?}", other),
        }
    }

    #[test]
    fn infer_context_browse_session_goto() {
        let ctx = infer_context(
            "browse_session",
            &json!({ "action": "goto", "url": "https://example.com" }),
        );
        match ctx {
            Some(ToolCallContext::PageVisit { url, reason, .. }) => {
                assert_eq!(url, "https://example.com");
                assert!(matches!(reason, Some(VisitReason::DirectNavigation)));
            }
            other => panic!("expected PageVisit, got {:?}", other),
        }
    }

    #[test]
    fn infer_context_browse_session_click() {
        let ctx = infer_context(
            "browse_session",
            &json!({ "action": "click", "selector": "#btn" }),
        );
        match ctx {
            Some(ToolCallContext::SessionAction { action, url }) => {
                assert_eq!(action, "click");
                assert!(url.is_none());
            }
            other => panic!("expected SessionAction, got {:?}", other),
        }
    }

    #[test]
    fn infer_context_browse_script_with_steps_array() {
        let ctx = infer_context(
            "browse_script",
            &json!({ "steps": [{"goto": "https://example.com"}, {"click": "#btn"}] }),
        );
        match ctx {
            Some(ToolCallContext::ScriptStep {
                current,
                total,
                step,
            }) => {
                assert_eq!(current, 0);
                assert_eq!(total, 2);
                assert_eq!(step, "starting");
            }
            other => panic!("expected ScriptStep, got {:?}", other),
        }
    }

    #[test]
    fn infer_context_browse_script_empty() {
        let ctx = infer_context("browse_script", &json!({ "script": "" }));
        #[cfg(feature = "native-browser")]
        assert!(ctx.is_none());
        #[cfg(not(feature = "native-browser"))]
        assert!(ctx.is_none());
    }

    #[test]
    fn infer_context_unknown_tool() {
        let ctx = infer_context("bash", &json!({ "command": "ls" }));
        assert!(ctx.is_none());
    }

    #[test]
    fn infer_context_missing_args() {
        // browse without url → None
        let ctx = infer_context("browse", &json!({}));
        assert!(ctx.is_none());

        // web_search without query → None
        let ctx = infer_context("web_search", &json!({}));
        assert!(ctx.is_none());
    }

    #[test]
    fn tool_context_serde_roundtrip() {
        let contexts = vec![
            ToolCallContext::WebSearch {
                query: "test".into(),
                engine: Some("ddg".into()),
            },
            ToolCallContext::PageVisit {
                url: "https://example.com".into(),
                reason: Some(VisitReason::DirectNavigation),
                page_title: None,
                page_status: None,
                page_bytes: None,
                page_duration_ms: None,
                navigation_error: None,
                screenshot: None,
            },
            ToolCallContext::PageVisit {
                url: "https://example.com".into(),
                reason: Some(VisitReason::SearchResult { position: 3 }),
                page_title: None,
                page_status: None,
                page_bytes: None,
                page_duration_ms: None,
                navigation_error: None,
                screenshot: None,
            },
            ToolCallContext::PageVisit {
                url: "https://example.com".into(),
                reason: None,
                page_title: Some("Example Page".into()),
                page_status: Some(200),
                page_bytes: Some(12400),
                page_duration_ms: Some(245),
                navigation_error: None,
                screenshot: None,
            },
            ToolCallContext::DataExtraction {
                target: ".title".into(),
                url: Some("https://example.com".into()),
                result_count: None,
                page_status: None,
                page_duration_ms: None,
            },
            ToolCallContext::DataExtraction {
                target: ".items".into(),
                url: Some("https://shop.example.com/products".into()),
                result_count: Some(42),
                page_status: Some(200),
                page_duration_ms: Some(180),
            },
            ToolCallContext::SessionAction {
                action: "goto".into(),
                url: Some("https://example.com".into()),
            },
            ToolCallContext::ScriptStep {
                current: 3,
                total: 10,
                step: "clicking".into(),
            },
        ];

        for ctx in &contexts {
            let json = serde_json::to_string(ctx).unwrap();
            let restored: ToolCallContext = serde_json::from_str(&json).unwrap();
            let json2 = serde_json::to_string(&restored).unwrap();
            assert_eq!(json, json2, "roundtrip failed for {:?}", ctx);
        }
    }

    #[test]
    fn tool_execution_update_backward_compat() {
        // Old JSON without context field → deserializes with context: None
        let old_json = json!({
            "type": "toolExecutionUpdate",
            "tool_call_id": "call_123",
            "tool_name": "browse",
            "partial_result": "Loading...",
            "tab_id": null
        });
        let event: crate::events::AgentEvent = serde_json::from_value(old_json).unwrap();
        match event {
            crate::events::AgentEvent::ToolExecutionUpdate { context, .. } => {
                assert!(context.is_none());
            }
            other => panic!("expected ToolExecutionUpdate, got {:?}", other),
        }
    }

    #[test]
    fn tool_execution_start_backward_compat() {
        // Old JSON without context field
        let old_json = json!({
            "type": "toolExecutionStart",
            "tool_call_id": "call_123",
            "tool_name": "browse",
            "args": { "url": "https://example.com" }
        });
        let event: crate::events::AgentEvent = serde_json::from_value(old_json).unwrap();
        match event {
            crate::events::AgentEvent::ToolExecutionStart { context, .. } => {
                assert!(context.is_none());
            }
            other => panic!("expected ToolExecutionStart, got {:?}", other),
        }
    }

    #[test]
    fn browse_enrichment_callback_fills_page_visit() {
        use crate::tools::browse::BrowseProgress;
        use std::sync::Arc;

        let cell: Arc<parking_lot::Mutex<Option<ToolCallContext>>> =
            Arc::new(parking_lot::Mutex::new(Some(ToolCallContext::PageVisit {
                url: "https://example.com".into(),
                reason: Some(VisitReason::DirectNavigation),
                page_title: None,
                page_status: None,
                page_bytes: None,
                page_duration_ms: None,
                navigation_error: None,
                screenshot: None,
            })));
        let cb = make_browse_enrichment_cb(Arc::clone(&cell));
        cb(BrowseProgress::DocumentReady {
            url: "https://example.com/final".into(),
            title: "Example".into(),
            status: 200,
            bytes: 4096,
            duration_ms: 245,
        });
        let snapshot = cell.lock().clone();
        match snapshot {
            Some(ToolCallContext::PageVisit {
                url,
                page_title,
                page_status,
                page_bytes,
                page_duration_ms,
                ..
            }) => {
                assert_eq!(url, "https://example.com/final");
                assert_eq!(page_title.as_deref(), Some("Example"));
                assert_eq!(page_status, Some(200));
                assert_eq!(page_bytes, Some(4096));
                assert_eq!(page_duration_ms, Some(245));
            }
            other => panic!("expected PageVisit, got {:?}", other),
        }
    }

    #[test]
    fn browse_enrichment_callback_fills_data_extraction() {
        use crate::tools::browse::BrowseProgress;
        use std::sync::Arc;

        let cell: Arc<parking_lot::Mutex<Option<ToolCallContext>>> = Arc::new(
            parking_lot::Mutex::new(Some(ToolCallContext::DataExtraction {
                target: ".item".into(),
                url: Some("https://shop.example.com".into()),
                result_count: None,
                page_status: None,
                page_duration_ms: None,
            })),
        );
        let cb = make_browse_enrichment_cb(Arc::clone(&cell));
        cb(BrowseProgress::DocumentReady {
            url: "https://shop.example.com".into(),
            title: "Shop".into(),
            status: 200,
            bytes: 8192,
            duration_ms: 180,
        });
        let snapshot = cell.lock().clone();
        match snapshot {
            Some(ToolCallContext::DataExtraction {
                page_status,
                page_duration_ms,
                ..
            }) => {
                assert_eq!(page_status, Some(200));
                assert_eq!(page_duration_ms, Some(180));
            }
            other => panic!("expected DataExtraction, got {:?}", other),
        }
    }

    #[test]
    fn browse_enrichment_callback_no_op_for_mismatched() {
        use crate::tools::browse::BrowseProgress;
        use std::sync::Arc;

        // DocumentReady + ScriptStep → no-op
        let cell: Arc<parking_lot::Mutex<Option<ToolCallContext>>> =
            Arc::new(parking_lot::Mutex::new(Some(ToolCallContext::ScriptStep {
                current: 1,
                total: 5,
                step: "click".into(),
            })));
        let cb = make_browse_enrichment_cb(Arc::clone(&cell));
        cb(BrowseProgress::DocumentReady {
            url: "x".into(),
            title: "t".into(),
            status: 200,
            bytes: 0,
            duration_ms: 0,
        });
        // ScriptStep should be untouched
        assert!(matches!(
            cell.lock().as_ref(),
            Some(ToolCallContext::ScriptStep { .. })
        ));
    }

    #[test]
    fn browse_enrichment_callback_fills_navigation_error() {
        use crate::tools::browse::BrowseProgress;
        use std::sync::Arc;

        let cell: Arc<parking_lot::Mutex<Option<ToolCallContext>>> =
            Arc::new(parking_lot::Mutex::new(Some(ToolCallContext::PageVisit {
                url: "https://example.com".into(),
                reason: Some(VisitReason::DirectNavigation),
                page_title: None,
                page_status: None,
                page_bytes: None,
                page_duration_ms: None,
                navigation_error: None,
                screenshot: None,
            })));
        let cb = make_browse_enrichment_cb(Arc::clone(&cell));
        cb(BrowseProgress::NavigationFailed {
            url: "https://example.com".into(),
            error: "connection refused".into(),
        });
        let snapshot = cell.lock().clone();
        match snapshot {
            Some(ToolCallContext::PageVisit {
                navigation_error, ..
            }) => {
                assert_eq!(navigation_error.as_deref(), Some("connection refused"));
            }
            other => panic!("expected PageVisit, got {:?}", other),
        }
    }

    #[test]
    fn browse_enrichment_callback_fills_screenshot() {
        use crate::tools::browse::BrowseProgress;
        use std::sync::Arc;

        let cell: Arc<parking_lot::Mutex<Option<ToolCallContext>>> =
            Arc::new(parking_lot::Mutex::new(Some(ToolCallContext::PageVisit {
                url: "https://example.com".into(),
                reason: Some(VisitReason::DirectNavigation),
                page_title: None,
                page_status: None,
                page_bytes: None,
                page_duration_ms: None,
                navigation_error: None,
                screenshot: None,
            })));
        let cb = make_browse_enrichment_cb(Arc::clone(&cell));
        cb(BrowseProgress::ScreenshotCaptured {
            bytes: 2048,
            width: 800,
            duration_ms: 120,
        });
        let snapshot = cell.lock().clone();
        match snapshot {
            Some(ToolCallContext::PageVisit { screenshot, .. }) => {
                let meta = screenshot.expect("screenshot should be set");
                assert_eq!(meta.bytes, 2048);
                assert_eq!(meta.width, 800);
                assert_eq!(meta.duration_ms, 120);
            }
            other => panic!("expected PageVisit, got {:?}", other),
        }
    }

    #[test]
    fn browse_enrichment_callback_navigation_failed_ignores_non_page_visit() {
        use crate::tools::browse::BrowseProgress;
        use std::sync::Arc;

        // NavigationFailed + DataExtraction → no-op
        let cell: Arc<parking_lot::Mutex<Option<ToolCallContext>>> = Arc::new(
            parking_lot::Mutex::new(Some(ToolCallContext::DataExtraction {
                target: ".title".into(),
                url: None,
                result_count: None,
                page_status: None,
                page_duration_ms: None,
            })),
        );
        let cb = make_browse_enrichment_cb(Arc::clone(&cell));
        cb(BrowseProgress::NavigationFailed {
            url: "https://example.com".into(),
            error: "timeout".into(),
        });
        assert!(matches!(
            cell.lock().as_ref(),
            Some(ToolCallContext::DataExtraction { .. })
        ));
    }
}
