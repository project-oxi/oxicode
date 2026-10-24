//! Integration tests for the catalog port (`ModelCatalog`).
//!
//! Verifies the trait surface end-to-end:
//! - `NoopModelCatalog` returns empty for everything.
//! - `FileModelCatalog::init` loads the embedded SNAP and reports the
//!   expected provider/model counts.
//! - `OxicodeBuilder::with_catalog` wires the port into `Oxicode::catalog()`.
//! - `CatalogProtocol` enum dispatch works (compile-time, no string match).

use oxicode_sdk::{
    AuthMethod, CatalogProtocol, FileModelCatalog, ModelCatalog, NoopModelCatalog, OxicodeBuilder,
    RefreshOutcome,
};
use std::sync::Arc;

// ═══════════════════════════════════════════════════════════════════════════
// Noop default
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn noop_catalog_returns_empty() {
    let cat = NoopModelCatalog::new();
    assert!(cat.list_providers().await.unwrap().is_empty());
    assert!(cat.get_provider("anthropic").await.unwrap().is_none());
    assert!(cat.list_models("anthropic").await.unwrap().is_empty());
    assert!(
        cat.get_model("anthropic", "claude-3")
            .await
            .unwrap()
            .is_none()
    );
    assert!(cat.search("claude").await.unwrap().is_empty());
    assert_eq!(cat.model_count().await.unwrap(), 0);
}

#[tokio::test]
async fn noop_refresh_is_unchanged() {
    let cat = NoopModelCatalog::new();
    let outcome = cat.refresh().await.unwrap();
    assert!(matches!(outcome, RefreshOutcome::Unchanged));
}

#[tokio::test]
async fn noop_subscribe_yields_receiver() {
    let cat = NoopModelCatalog::new();
    let _rx = cat.subscribe();
    // Drop sender → Receiver.recv() returns Err (channel closed).
    drop(cat);
    // (No assertion — just verify construction doesn't panic.)
}

// ═══════════════════════════════════════════════════════════════════════════
// FileModelCatalog — SNAP-driven reference impl
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn file_catalog_init_loads_snap() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = oxicode_sdk::CatalogConfig {
        cache_path: tmp.path().join("models-dev.json"),
        etag_path: tmp.path().join("models-dev.json.etag"),
        override_path: tmp.path().join("overrides.toml"),
        ..Default::default()
    };
    let cat = FileModelCatalog::init(config).await.unwrap();
    let providers = cat.list_providers().await.unwrap();
    assert!(!providers.is_empty(), "SNAP should have providers");
    assert!(providers.iter().any(|p| p == "anthropic"));
    assert!(providers.iter().any(|p| p == "openai"));
    let count = cat.model_count().await.unwrap();
    assert!(count > 1000, "expected many models, got {count}");
}

#[tokio::test]
async fn stale_cache_is_served_when_fresh_window_expires() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = tmp.path().join("models-dev.json");
    std::fs::write(
        &cache,
        r#"{"staleprov":{"name":"Stale Provider","env":["X"],"models":{"stale-model":{"name":"Stale Model","reasoning":false,"limit":{"context":200000,"output":64000}}}}}"#,
    )
    .unwrap();
    let config = oxicode_sdk::CatalogConfig {
        cache_path: cache,
        etag_path: tmp.path().join("cache.etag"),
        override_path: tmp.path().join("overrides.toml"),
        // Cache instantly ages out of the window — the stale path must
        // still serve it (it beats the older embedded snapshot).
        mtime_window: std::time::Duration::from_millis(1),
        fetch_enabled: false,
        ..Default::default()
    };
    // Let the file age past the window.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let cat = FileModelCatalog::init(config).await.unwrap();
    let providers = cat.list_providers().await.unwrap();
    assert!(
        providers.iter().any(|p| p == "staleprov"),
        "stale cache must be served instead of falling back to SNAP; got {providers:?}"
    );
}

#[tokio::test]
async fn background_refresh_keeps_serving_stale_when_offline() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = tmp.path().join("models-dev.json");
    std::fs::write(
        &cache,
        r#"{"staleprov":{"name":"Stale Provider","env":["X"],"models":{"stale-model":{"name":"Stale Model","reasoning":false,"limit":{"context":200000,"output":64000}}}}}"#,
    )
    .unwrap();
    let config = oxicode_sdk::CatalogConfig {
        cache_path: cache,
        etag_path: tmp.path().join("cache.etag"),
        override_path: tmp.path().join("overrides.toml"),
        mtime_window: std::time::Duration::from_millis(10),
        // Port 9 (discard) — connection refused, guaranteed dead, no
        // real network access.
        models_dev_url: "http://127.0.0.1:9".to_string(),
        ..Default::default()
    };
    // Age the cache past the window.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let cat = FileModelCatalog::init(config).await.unwrap();
    // The synchronous refresh and several background-refresh attempts all
    // fail against the dead endpoint; the stale snapshot must survive.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let providers = cat.list_providers().await.unwrap();
    assert!(
        providers.iter().any(|p| p == "staleprov"),
        "failed refreshes must not wipe the served snapshot; got {providers:?}"
    );
}

#[tokio::test]
async fn file_catalog_get_model_anthropic() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = oxicode_sdk::CatalogConfig {
        cache_path: tmp.path().join("cache.json"),
        etag_path: tmp.path().join("cache.etag"),
        override_path: tmp.path().join("overrides.toml"),
        // Deterministic: test the *embedded* SNAP, not live models.dev
        // (which can retire dated model ids and make this flaky).
        fetch_enabled: false,
        ..Default::default()
    };
    let cat = FileModelCatalog::init(config).await.unwrap();
    let entry = cat
        .get_model("anthropic", "claude-sonnet-4-6")
        .await
        .unwrap()
        .expect("anthropic claude-sonnet-4-6 should be present in SNAP");
    assert_eq!(entry.provider, "anthropic");
    assert_eq!(entry.model_id, "claude-sonnet-4-6");
    assert_eq!(entry.protocol, CatalogProtocol::AnthropicMessages);
    assert!(entry.cost_input > 0.0, "cost should be populated");
    assert!(entry.context_window > 0);
}

#[tokio::test]
async fn file_catalog_provider_lookup() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = oxicode_sdk::CatalogConfig {
        cache_path: tmp.path().join("cache.json"),
        etag_path: tmp.path().join("cache.etag"),
        override_path: tmp.path().join("overrides.toml"),
        ..Default::default()
    };
    let cat = FileModelCatalog::init(config).await.unwrap();
    let p = cat
        .get_provider("anthropic")
        .await
        .unwrap()
        .expect("anthropic provider should exist");
    assert_eq!(p.id, "anthropic");
    assert_eq!(p.protocol, CatalogProtocol::AnthropicMessages);
    assert!(p.default_enabled);
}

#[tokio::test]
async fn file_catalog_search_finds_models() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = oxicode_sdk::CatalogConfig {
        cache_path: tmp.path().join("cache.json"),
        etag_path: tmp.path().join("cache.etag"),
        override_path: tmp.path().join("overrides.toml"),
        ..Default::default()
    };
    let cat = FileModelCatalog::init(config).await.unwrap();
    let results = cat.search("claude").await.unwrap();
    assert!(!results.is_empty(), "should find claude models");
    for entry in &results {
        let lc = format!("{}{}", entry.provider, entry.model_id).to_lowercase();
        assert!(
            lc.contains("claude") || entry.name.to_lowercase().contains("claude"),
            "search for 'claude' should match: {}",
            entry.model_id
        );
    }
}

#[tokio::test]
async fn file_catalog_refresh_with_no_fetch_returns_offline_or_unchanged() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = oxicode_sdk::CatalogConfig {
        cache_path: tmp.path().join("cache.json"),
        etag_path: tmp.path().join("cache.etag"),
        override_path: tmp.path().join("overrides.toml"),
        fetch_enabled: false,
        ..Default::default()
    };
    let cat = FileModelCatalog::init(config).await.unwrap();
    let outcome = cat.refresh().await.unwrap();
    // With fetch disabled, init() did not call refresh. Now an explicit
    // refresh returns Offline (no network attempted).
    assert!(matches!(outcome, RefreshOutcome::Offline { .. }));
}

// ═══════════════════════════════════════════════════════════════════════════
// OxicodeBuilder integration
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn oxicode_builder_with_catalog_wires_port() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = oxicode_sdk::CatalogConfig {
        cache_path: tmp.path().join("cache.json"),
        etag_path: tmp.path().join("cache.etag"),
        override_path: tmp.path().join("overrides.toml"),
        ..Default::default()
    };
    let catalog: Arc<dyn ModelCatalog> = FileModelCatalog::init(config).await.unwrap();
    let oxicode = OxicodeBuilder::new().with_catalog(catalog).build();
    // Oxicode holds one strong reference (catalog was consumed by `with_catalog`).
    assert_eq!(Arc::strong_count(oxicode.catalog()), 1);
    let providers = oxicode.catalog().list_providers().await.unwrap();
    assert!(!providers.is_empty());
}

#[tokio::test]
async fn oxicode_builder_without_catalog_uses_noop() {
    let oxicode = OxicodeBuilder::new().build();
    // Noop returns empty for everything.
    assert!(oxicode.catalog().list_providers().await.unwrap().is_empty());
    assert_eq!(oxicode.catalog().model_count().await.unwrap(), 0);
}

// ═══════════════════════════════════════════════════════════════════════════
// CatalogProtocol enum — compile-time dispatch
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn catalog_protocol_default_auth_matches_documented_mapping() {
    // Anthropic → x-api-key (per dynamic-catalog §2.3).
    assert_eq!(
        CatalogProtocol::AnthropicMessages.default_auth(),
        AuthMethod::XApiKey
    );
    // Azure → api-key.
    assert_eq!(
        CatalogProtocol::AzureOpenAiResponses.default_auth(),
        AuthMethod::ApiKey
    );
    // Google / Bedrock → none.
    assert_eq!(
        CatalogProtocol::GoogleVertex.default_auth(),
        AuthMethod::None
    );
    assert_eq!(
        CatalogProtocol::GoogleGenerativeAi.default_auth(),
        AuthMethod::None
    );
    assert_eq!(
        CatalogProtocol::BedrockConverseStream.default_auth(),
        AuthMethod::None
    );
    // OpenAI-family → bearer.
    assert_eq!(
        CatalogProtocol::OpenAiCompletions.default_auth(),
        AuthMethod::Bearer
    );
    assert_eq!(
        CatalogProtocol::OpenAiResponses.default_auth(),
        AuthMethod::Bearer
    );
    assert_eq!(
        CatalogProtocol::OpenAiCompatible.default_auth(),
        AuthMethod::Bearer
    );
}

#[test]
fn catalog_protocol_as_oxicode_api_covers_all_variants() {
    use oxicode_ai::Api;
    assert_eq!(
        CatalogProtocol::AnthropicMessages.as_oxicode_api(),
        Api::AnthropicMessages
    );
    assert_eq!(
        CatalogProtocol::OpenAiCompletions.as_oxicode_api(),
        Api::OpenAiCompletions
    );
    assert_eq!(
        CatalogProtocol::OpenAiResponses.as_oxicode_api(),
        Api::OpenAiResponses
    );
    assert_eq!(
        CatalogProtocol::AzureOpenAiResponses.as_oxicode_api(),
        Api::AzureOpenAiResponses
    );
    assert_eq!(
        CatalogProtocol::GoogleGenerativeAi.as_oxicode_api(),
        Api::GoogleGenerativeAi
    );
    assert_eq!(
        CatalogProtocol::GoogleVertex.as_oxicode_api(),
        Api::GoogleVertex
    );
    assert_eq!(
        CatalogProtocol::BedrockConverseStream.as_oxicode_api(),
        Api::BedrockConverseStream
    );
    // OpenAI-compatible maps to OpenAiCompletions (chat completions protocol).
    assert_eq!(
        CatalogProtocol::OpenAiCompatible.as_oxicode_api(),
        Api::OpenAiCompletions
    );
}

#[test]
fn catalog_protocol_as_str_is_stable() {
    assert_eq!(
        CatalogProtocol::AnthropicMessages.as_str(),
        "anthropic-messages"
    );
    assert_eq!(
        CatalogProtocol::OpenAiCompletions.as_str(),
        "openai-completions"
    );
    assert_eq!(
        CatalogProtocol::OpenAiCompatible.as_str(),
        "openai-compatible"
    );
    assert_eq!(
        CatalogProtocol::BedrockConverseStream.as_str(),
        "bedrock-converse-stream"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Debug impl smoke (verify fields are accessible)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn noop_catalog_debug_is_clean() {
    let cat = NoopModelCatalog::new();
    let s = format!("{:?}", cat);
    assert!(s.contains("NoopModelCatalog"));
}

#[tokio::test]
async fn file_catalog_debug_includes_stats() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = oxicode_sdk::CatalogConfig {
        cache_path: tmp.path().join("cache.json"),
        etag_path: tmp.path().join("cache.etag"),
        override_path: tmp.path().join("overrides.toml"),
        ..Default::default()
    };
    let cat = FileModelCatalog::init(config).await.unwrap();
    let s = format!("{:?}", cat);
    assert!(s.contains("FileModelCatalog"));
    assert!(s.contains("providers"));
    assert!(s.contains("models"));
}

// ═══════════════════════════════════════════════════════════════════════════
// Synchronous read-only API (sync read of in-memory snapshot)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn noop_catalog_sync_returns_empty() {
    let cat = NoopModelCatalog::new();
    use oxicode_sdk::ports::catalog::ModelCatalog;
    assert!(cat.list_providers_sync().is_empty());
    assert_eq!(cat.model_count_sync(), 0);
    assert!(cat.search_sync("").is_empty());
    assert!(cat.list_models_sync("anything").is_empty());
    assert!(cat.get_provider_sync("anything").is_none());
}

#[tokio::test]
async fn file_catalog_sync_api_reads_snapshot() {
    // The sync read API must mirror the async API results — both read the
    // same in-memory snapshot. This test loads a FileModelCatalog and
    // cross-checks sync vs async for a few representative queries.
    use oxicode_sdk::ports::catalog::ModelCatalog;
    let tmp = tempfile::TempDir::new().unwrap();
    let config = oxicode_sdk::CatalogConfig {
        cache_path: tmp.path().join("cache.json"),
        etag_path: tmp.path().join("cache.etag"),
        override_path: tmp.path().join("overrides.toml"),
        fetch_enabled: false,
        ..Default::default()
    };
    let cat = FileModelCatalog::init(config).await.unwrap();

    // provider list
    let sync_providers = cat.list_providers_sync();
    let async_providers = cat.list_providers().await.unwrap();
    assert_eq!(sync_providers, async_providers);
    assert!(sync_providers.contains(&"anthropic".to_string()));

    // model count
    let sync_count = cat.model_count_sync();
    let async_count = cat.model_count().await.unwrap();
    assert_eq!(sync_count, async_count);
    assert!(sync_count > 100, "expected many models, got {sync_count}");

    // anthropic provider entry exists via sync
    let entry = cat
        .get_provider_sync("anthropic")
        .expect("anthropic provider");
    assert_eq!(entry.id, "anthropic");
    assert!(!entry.display_name.is_empty());

    // anthropic models via sync vs async
    let sync_models = cat.list_models_sync("anthropic");
    let async_models = cat.list_models("anthropic").await.unwrap();
    assert_eq!(sync_models.len(), async_models.len());
    assert!(sync_models.iter().any(|m| m.model_id.contains("claude")));

    // search sync
    let sync_search = cat.search_sync("claude");
    let async_search = cat.search("claude").await.unwrap();
    assert_eq!(sync_search.len(), async_search.len());
    assert!(!sync_search.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
// Oxicode::resolve_model uses the catalog port (bridge layer integration)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn oxicode_resolve_model_uses_catalog() {
    // When a catalog port is wired, Oxicode::resolve_model should resolve
    // models through the catalog (sync read) rather than the static model
    // registry. We verify by resolving a known catalog model.
    let tmp = tempfile::TempDir::new().unwrap();
    let config = oxicode_sdk::CatalogConfig {
        cache_path: tmp.path().join("cache.json"),
        etag_path: tmp.path().join("cache.etag"),
        override_path: tmp.path().join("overrides.toml"),
        fetch_enabled: false,
        ..Default::default()
    };
    let cat = FileModelCatalog::init(config).await.unwrap();
    let oxicode = oxicode_sdk::OxicodeBuilder::new()
        .with_builtins()
        .with_catalog(cat)
        .build();

    // claude-sonnet-4-20250514 exists in the embedded SNAP.
    let model = oxicode
        .resolve_model("anthropic/claude-sonnet-4-20250514")
        .expect("model should resolve via catalog");
    assert_eq!(model.provider, "anthropic");
    assert_eq!(model.api, oxicode_ai::Api::AnthropicMessages);
    assert!(model.context_window > 0);
}

#[tokio::test]
async fn bridge_catalog_entry_to_model() {
    // Direct bridge layer test: catalog entry → oxicode_ai::Model.
    let tmp = tempfile::TempDir::new().unwrap();
    let config = oxicode_sdk::CatalogConfig {
        cache_path: tmp.path().join("cache.json"),
        etag_path: tmp.path().join("cache.etag"),
        override_path: tmp.path().join("overrides.toml"),
        fetch_enabled: false,
        ..Default::default()
    };
    let cat = FileModelCatalog::init(config).await.unwrap();
    let entry = cat
        .get_model_sync("anthropic", "claude-sonnet-4-6")
        .expect("entry exists");
    let model = oxicode_sdk::bridge::catalog_entry_to_model("anthropic", &entry);
    assert_eq!(model.id, "claude-sonnet-4-6");
    assert_eq!(model.provider, "anthropic");
    assert!(!model.name.is_empty());
}
