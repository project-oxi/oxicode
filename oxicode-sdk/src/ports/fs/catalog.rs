//! File-based implementation of [`ModelCatalog`].
//!
//! Mirrors the legacy `oxicode-ai/src/catalog/models_dev.rs` logic. SNAP is
//! loaded from `oxicode-ai/data/catalog/_snapshot.json.gz` (embedded at compile
//! time), then layered with the runtime cache (mtime + ETag conditional
//! GET) and user overrides from `~/.oxicode/catalog/overrides.toml`.
//!
//! See `docs/designs/2026-06-17-catalog-port-design.md` (v3) §6 for
//! the full architecture rationale.

use std::collections::BTreeMap;
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::error::SdkResult;
use crate::ports::catalog::{
    CatalogEvent, CatalogModelEntry, CatalogProtocol, CatalogProviderEntry, CatalogSource,
    ModelCatalog, RefreshOutcome,
};

// ═══════════════════════════════════════════════════════════════════════════
// Tunables (read from env at init time)
// ═══════════════════════════════════════════════════════════════════════════

const DEFAULT_MTIME_WINDOW: Duration = Duration::from_secs(60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const FETCH_RETRIES: u32 = 2;
const RETRY_BACKOFF: Duration = Duration::from_millis(200);
const DEFAULT_URL: &str = "https://models.dev";
const USER_AGENT: &str = concat!("oxicode-sdk/", env!("CARGO_PKG_VERSION"));
const BROADCAST_CAPACITY: usize = 16;

// ═══════════════════════════════════════════════════════════════════════════
// Configuration
// ═══════════════════════════════════════════════════════════════════════════

/// Configuration for [`FileModelCatalog::init`].
///
/// All paths default to conventional locations (`~/.oxicode/...`). Override
/// for tests via `tempfile::TempDir` paths.
#[derive(Debug, Clone)]
pub struct CatalogConfig {
    /// models.dev cache + JSON store. Default: `~/.oxicode/cache/models-dev.json`.
    pub cache_path: PathBuf,
    /// ETag sidecar file. Default: `~/.oxicode/cache/models-dev.json.etag`.
    pub etag_path: PathBuf,
    /// User overrides TOML. Default: `~/.oxicode/catalog/overrides.toml`.
    pub override_path: PathBuf,
    /// mtime freshness window for the cache. Default: 1 hour.
    pub mtime_window: Duration,
    /// If `false`, never touch the network. Default: true.
    pub fetch_enabled: bool,
    /// models.dev base URL. Default: `https://models.dev`.
    pub models_dev_url: String,
    /// User-Agent header value. Default: `oxicode-sdk/<version>`.
    pub user_agent: String,
    /// Optional local servers (`ollama`, `lmstudio`, etc.) to probe via
    /// `/v1/models` at init time. Empty = skip.
    pub local_discovery_urls: Vec<String>,
    /// Snapshot gzip path. Used at init time to verify the embed exists;
    /// not used directly (`include_bytes!` happens at compile time).
    pub snapshot_path: PathBuf,
}

impl Default for CatalogConfig {
    fn default() -> Self {
        let home = crate::ports::fs::path::home_dir().unwrap_or_else(|_| PathBuf::from(".oxicode"));
        let cache = home.join("cache");
        let catalog_dir = home.join("catalog");
        Self {
            cache_path: cache.join("models-dev.json"),
            etag_path: cache.join("models-dev.json.etag"),
            override_path: catalog_dir.join("overrides.toml"),
            mtime_window: DEFAULT_MTIME_WINDOW,
            fetch_enabled: true,
            models_dev_url: DEFAULT_URL.to_string(),
            user_agent: USER_AGENT.to_string(),
            local_discovery_urls: Vec::new(),
            snapshot_path: home.join("cache").join("models-dev.json"),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// models.dev JSON schema (mirrors upstream `api.json`)
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct MdCatalog(pub BTreeMap<String, MdProvider>);

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MdProvider {
    pub name: String,
    pub env: Vec<String>,
    #[serde(default)]
    pub npm: Option<String>,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub doc: Option<String>,
    pub models: BTreeMap<String, MdModel>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MdModel {
    pub name: String,
    #[serde(default)]
    pub family: Option<String>,
    pub reasoning: bool,
    #[serde(default)]
    pub tool_call: bool,
    #[serde(default)]
    pub attachment: bool,
    #[serde(default)]
    pub temperature: Option<bool>,
    #[serde(default)]
    pub structured_output: Option<bool>,
    #[serde(default)]
    pub knowledge: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub last_updated: Option<String>,
    #[serde(default)]
    pub open_weights: Option<bool>,
    #[serde(default)]
    pub interleaved: Option<serde_json::Value>,
    #[serde(default)]
    pub reasoning_options: Option<Vec<serde_json::Value>>,
    pub limit: MdLimit,
    #[serde(default)]
    pub cost: Option<MdCost>,
    #[serde(default)]
    pub modalities: Option<MdModalities>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub provider: Option<MdModelProvider>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MdModelProvider {
    #[serde(default)]
    pub npm: Option<String>,
    #[serde(default)]
    pub api: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MdLimit {
    pub context: f64,
    #[serde(default)]
    pub input: Option<f64>,
    pub output: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MdCost {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
    #[serde(default)]
    pub tiers: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    pub context_over_200k: Option<serde_json::Value>,
    #[serde(default)]
    pub reasoning: Option<f64>,
    #[serde(default)]
    pub input_audio: Option<f64>,
    #[serde(default)]
    pub output_audio: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MdModalities {
    #[serde(default)]
    pub input: Option<Vec<String>>,
    #[serde(default)]
    pub output: Option<Vec<String>>,
}

// ═══════════════════════════════════════════════════════════════════════════
// User override TOML schema (Layer 2)
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct OverrideFile {
    #[serde(default)]
    pub provider: Vec<OverrideProvider>,
    #[serde(default)]
    pub model: Vec<OverrideModel>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OverrideProvider {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub env_key: Option<String>,
    #[serde(default)]
    pub extra_headers: Vec<(String, String)>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OverrideModel {
    pub provider: String,
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub cost_input: Option<f64>,
    #[serde(default)]
    pub cost_output: Option<f64>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

// ═══════════════════════════════════════════════════════════════════════════
// Snapshot loading
// ═══════════════════════════════════════════════════════════════════════════

/// Decompress and parse the compile-time embedded SNAP.
///
/// The raw bytes come from [`oxicode_ai::catalog::snapshot_gzip_bytes`] — the
/// single source of truth for the snapshot, owned by `oxicode-ai`. `oxicode-sdk`
/// does **not** `include_bytes!` the file directly: the snapshot lives in
/// `oxicode-ai/data/catalog/` (a sibling crate), and a cross-crate
/// `include_bytes!` path escapes the `oxicode-sdk` package root, which made the
/// published crate uncompilable for downstream consumers (fixed in 0.37.1).
/// Reading the bytes through the `oxicode-ai` API keeps the snapshot packaged
/// exactly once and lets `oxicode-sdk` parse it with its own [`MdCatalog`]
/// schema below.
fn load_snapshot() -> Option<MdCatalog> {
    let compressed: &[u8] = oxicode_ai::catalog::snapshot_gzip_bytes();
    let mut decoder = flate2::read::GzDecoder::new(compressed);
    let mut json = String::new();
    decoder.read_to_string(&mut json).ok()?;
    serde_json::from_str::<MdCatalog>(&json).ok()
}

// ═══════════════════════════════════════════════════════════════════════════
// Protocol resolution — npm → CatalogProtocol
// ═══════════════════════════════════════════════════════════════════════════

/// Map a models.dev `npm` string to oxicode's [`CatalogProtocol`].
///
/// This is the **only** protocol knowledge the SDK has. New protocol =
/// add a variant + a match arm here + a bridge dispatch (PR 3). Unknown
/// npm values fall back to [`CatalogProtocol::OpenAiCompatible`].
pub(crate) fn protocol_for(npm: &str) -> CatalogProtocol {
    match npm {
        "@ai-sdk/anthropic" => CatalogProtocol::AnthropicMessages,
        "@ai-sdk/google" => CatalogProtocol::GoogleGenerativeAi,
        "@ai-sdk/google-vertex" | "@ai-sdk/google-vertex/anthropic" => {
            CatalogProtocol::GoogleVertex
        }
        "@ai-sdk/azure" => CatalogProtocol::AzureOpenAiResponses,
        "@ai-sdk/amazon-bedrock" => CatalogProtocol::BedrockConverseStream,
        "@ai-sdk/openai" | "@ai-sdk/openai-compatible" => CatalogProtocol::OpenAiCompletions,
        // unknown npm → OpenAI-compatible fallback (most gateways/aggregators)
        _ => CatalogProtocol::OpenAiCompatible,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Materialize: MdCatalog → (CatalogProviderEntry, CatalogModelEntry)
// ═══════════════════════════════════════════════════════════════════════════

/// Convert a [`MdCatalog`] into the SDK's catalog entries.
///
/// Applies per-model npm overrides, then user overrides from
/// `OverrideFile` (Layer 2 — highest precedence).
pub(crate) fn materialize(
    catalog: &MdCatalog,
    user_overrides: &OverrideFile,
) -> (
    Vec<CatalogProviderEntry>,
    BTreeMap<String, Vec<CatalogModelEntry>>,
) {
    let mut providers = Vec::new();
    let mut models: BTreeMap<String, Vec<CatalogModelEntry>> = BTreeMap::new();

    for (pid, mdprov) in &catalog.0 {
        let provider_protocol = protocol_for(mdprov.npm.as_deref().unwrap_or(""));

        providers.push(CatalogProviderEntry {
            id: pid.clone(),
            display_name: mdprov.name.clone(),
            aliases: Vec::new(),
            protocol: provider_protocol,
            env_key: mdprov.env.first().cloned(),
            extra_env_keys: mdprov.env.get(1..).unwrap_or(&[]).to_vec(),
            base_url: mdprov.api.clone(),
            extra_headers: Vec::new(),
            category: String::new(),
            description: String::new(),
            default_enabled: true,
        });

        for (mid, mdmodel) in &mdprov.models {
            let model_prov = mdmodel.provider.as_ref();
            let model_npm = model_prov
                .and_then(|p| p.npm.as_deref())
                .unwrap_or_else(|| mdprov.npm.as_deref().unwrap_or(""));
            let model_protocol = protocol_for(model_npm);
            let model_base_url = model_prov
                .and_then(|p| p.api.clone())
                .filter(|s| !s.is_empty());

            models
                .entry(pid.clone())
                .or_default()
                .push(CatalogModelEntry {
                    provider: pid.clone(),
                    model_id: mid.clone(),
                    name: mdmodel.name.clone(),
                    protocol: model_protocol,
                    source: CatalogSource::Embedded,
                    base_url: model_base_url,
                    reasoning: mdmodel.reasoning,
                    supports_vision: mdmodel.attachment,
                    cost_input: mdmodel.cost.as_ref().map(|c| c.input).unwrap_or(0.0),
                    cost_output: mdmodel.cost.as_ref().map(|c| c.output).unwrap_or(0.0),
                    cost_cache_read: mdmodel
                        .cost
                        .as_ref()
                        .and_then(|c| c.cache_read)
                        .unwrap_or(0.0),
                    cost_cache_write: mdmodel
                        .cost
                        .as_ref()
                        .and_then(|c| c.cache_write)
                        .unwrap_or(0.0),
                    context_window: mdmodel.limit.context as u32,
                    max_tokens: mdmodel.limit.output as u32,
                    input_modalities: normalize_modalities(&mdmodel.modalities),
                    release_date: mdmodel.release_date.clone(),
                    status: mdmodel.status.clone(),
                });
        }
    }

    apply_user_overrides(&mut providers, &mut models, user_overrides);

    (providers, models)
}

fn normalize_modalities(md: &Option<MdModalities>) -> Vec<String> {
    match md {
        Some(m) => match &m.input {
            Some(input) if !input.is_empty() => input.clone(),
            _ => vec!["text".to_string()],
        },
        None => vec!["text".to_string()],
    }
}

fn apply_user_overrides(
    providers: &mut Vec<CatalogProviderEntry>,
    models: &mut BTreeMap<String, Vec<CatalogModelEntry>>,
    overrides: &OverrideFile,
) {
    // Provider overrides: replace entry with matching id, or push new.
    for ovr in &overrides.provider {
        if let Some(slot) = providers.iter_mut().find(|p| p.id == ovr.id) {
            if let Some(d) = &ovr.display_name {
                slot.display_name = d.clone();
            }
            if let Some(b) = &ovr.base_url {
                slot.base_url = Some(b.clone());
            }
            if let Some(k) = &ovr.env_key {
                slot.env_key = Some(k.clone());
            }
            slot.extra_headers = ovr.extra_headers.clone();
            if let Some(en) = ovr.enabled {
                slot.default_enabled = en;
            }
        } else {
            providers.push(CatalogProviderEntry {
                id: ovr.id.clone(),
                display_name: ovr.display_name.clone().unwrap_or_else(|| ovr.id.clone()),
                aliases: Vec::new(),
                protocol: CatalogProtocol::OpenAiCompatible,
                env_key: ovr.env_key.clone(),
                extra_env_keys: Vec::new(),
                base_url: ovr.base_url.clone(),
                extra_headers: ovr.extra_headers.clone(),
                category: String::new(),
                description: String::new(),
                default_enabled: ovr.enabled.unwrap_or(true),
            });
        }
    }
    // Model overrides: replace by (provider, id), or push new.
    for ovr in &overrides.model {
        let entry = CatalogModelEntry {
            provider: ovr.provider.clone(),
            model_id: ovr.id.clone(),
            name: ovr.name.clone().unwrap_or_else(|| ovr.id.clone()),
            protocol: CatalogProtocol::OpenAiCompatible,
            source: CatalogSource::Override,
            base_url: None,
            reasoning: false,
            supports_vision: false,
            cost_input: ovr.cost_input.unwrap_or(0.0),
            cost_output: ovr.cost_output.unwrap_or(0.0),
            cost_cache_read: 0.0,
            cost_cache_write: 0.0,
            context_window: ovr.context_window.unwrap_or(0),
            max_tokens: ovr.max_tokens.unwrap_or(0),
            input_modalities: vec!["text".to_string()],
            release_date: None,
            status: None,
        };
        let list = models.entry(ovr.provider.clone()).or_default();
        if let Some(slot) = list.iter_mut().find(|m| m.model_id == ovr.id) {
            // Partial update: override only the fields that were set in TOML.
            if let Some(n) = ovr.name.clone() {
                slot.name = n;
            }
            if let Some(c) = ovr.cost_input {
                slot.cost_input = c;
            }
            if let Some(c) = ovr.cost_output {
                slot.cost_output = c;
            }
            if let Some(c) = ovr.context_window {
                slot.context_window = c;
            }
            if let Some(m) = ovr.max_tokens {
                slot.max_tokens = m;
            }
            slot.source = CatalogSource::Override;
        } else {
            list.push(entry);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Snapshot (in-memory) — protected by RwLock
// ═══════════════════════════════════════════════════════════════════════════

struct Snapshot {
    providers: Vec<CatalogProviderEntry>,
    /// provider_id → (model_id → entry). Nested for O(1) `get_model`.
    models: BTreeMap<String, BTreeMap<String, CatalogModelEntry>>,
}

impl Snapshot {
    fn empty() -> Self {
        Self {
            providers: Vec::new(),
            models: BTreeMap::new(),
        }
    }

    fn stats(&self) -> (usize, usize) {
        let model_count = self.models.values().map(|m| m.len()).sum();
        (self.providers.len(), model_count)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// FileModelCatalog — the reference port impl
// ═══════════════════════════════════════════════════════════════════════════

/// File-based catalog backed by models.dev SNAP + runtime cache + user
/// overrides. This is the reference impl of [`ModelCatalog`] used by
/// `oxicode-cli` and similar products.
pub struct FileModelCatalog {
    state: Arc<RwLock<Snapshot>>,
    tx: broadcast::Sender<CatalogEvent>,
    config: CatalogConfig,
}

impl std::fmt::Debug for FileModelCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snap = self.state.read();
        let (providers, models) = snap.stats();
        f.debug_struct("FileModelCatalog")
            .field("providers", &providers)
            .field("models", &models)
            .field("fetch_enabled", &self.config.fetch_enabled)
            .finish_non_exhaustive()
    }
}

impl FileModelCatalog {
    /// Build the catalog by loading SNAP + cache + overrides.
    ///
    /// Cache precedence: fresh cache, then a stale cache (serve-stale — a
    /// stale runtime cache still beats the embedded snapshot), then SNAP.
    /// If the cache is stale and `fetch_enabled`, one refresh is attempted
    /// synchronously (failure logs a warning and keeps the current
    /// snapshot), and a background task keeps re-refreshing every
    /// mtime window so long-running sessions pick up models.dev changes
    /// without a restart.
    pub async fn init(config: CatalogConfig) -> std::io::Result<Arc<Self>> {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let cat = Arc::new(Self {
            state: Arc::new(RwLock::new(Snapshot::empty())),
            tx,
            config,
        });

        // 1. SNAP (embedded at compile time)
        cat.load_snapshot_internal();

        // 2. Runtime cache (fresh first; stale cache served as fallback)
        if cat.try_load_fresh_cache().await.is_none() {
            cat.try_load_stale_cache().await;
        }

        // 3. User overrides (Layer 2 — highest precedence)
        cat.apply_user_overrides_internal();

        // 4. LOCAL discovery (optional)
        cat.discover_local_all().await;

        // 5. Refresh if stale (failure keeps the current snapshot), then
        // keep refreshing in the background for the life of the process.
        cat.refresh_if_stale_internal().await;
        cat.spawn_refresh_task();

        Ok(cat)
    }

    /// Get the embedded catalog event channel for the broadcast sender
    /// (mainly for tests; production code uses `subscribe()`).
    #[allow(dead_code)]
    pub(crate) fn tx(&self) -> &broadcast::Sender<CatalogEvent> {
        &self.tx
    }

    // ─── SNAP load ─────────────────────────────────────────────────────

    fn load_snapshot_internal(&self) {
        let Some(md) = load_snapshot() else {
            tracing::warn!("catalog: embedded SNAP missing or corrupt");
            return;
        };
        let overrides = OverrideFile::default();
        let (providers, models) = materialize(&md, &overrides);
        let mut snap = self.state.write();
        snap.providers = providers;
        snap.models = models
            .into_iter()
            .map(|(pid, list)| {
                let map = list.into_iter().map(|e| (e.model_id.clone(), e)).collect();
                (pid, map)
            })
            .collect();
    }

    // ─── Cache load ─────────────────────────────────────────────────────

    async fn try_load_fresh_cache(&self) -> Option<()> {
        let path = self.config.cache_path.clone();
        let window = self.config.mtime_window;
        let res = tokio::task::spawn_blocking(move || read_cache_if_fresh(&path, window))
            .await
            .ok()
            .flatten();
        res.map(|catalog| self.install_cache_catalog(catalog))
    }

    /// Serve-stale fallback: load the runtime cache regardless of age. A
    /// stale cache is still newer than the build-time snapshot, so it is
    /// the better approximation of models.dev while offline.
    async fn try_load_stale_cache(&self) -> Option<()> {
        let path = self.config.cache_path.clone();
        let res = tokio::task::spawn_blocking(move || read_cache_stale_ok(&path))
            .await
            .ok()
            .flatten();
        res.map(|catalog| {
            tracing::info!("catalog: serving stale runtime cache (background refresh will retry)");
            self.install_cache_catalog(catalog)
        })
    }

    /// Replace the in-memory snapshot with a materialized cache catalog.
    fn install_cache_catalog(&self, catalog: MdCatalog) {
        let overrides = OverrideFile::default();
        let (providers, models) = materialize(&catalog, &overrides);
        let mut snap = self.state.write();
        snap.providers = providers;
        snap.models = models
            .into_iter()
            .map(|(pid, list)| {
                let map = list.into_iter().map(|e| (e.model_id.clone(), e)).collect();
                (pid, map)
            })
            .collect();
    }

    /// Refresh once if fetches are enabled and the runtime cache aged out
    /// of the mtime window. Failure logs a warning and keeps the current
    /// snapshot (serve-stale). Returns whether a refresh succeeded.
    async fn refresh_if_stale_internal(&self) -> bool {
        if !self.config.fetch_enabled {
            return false;
        }
        if self.is_cache_fresh_internal() {
            return false;
        }
        match self.refresh().await {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(error = %error, "catalog refresh failed; keeping current snapshot");
                false
            }
        }
    }

    /// Periodic catalog refresh: wakes every mtime window (doubled, so the
    /// cache has actually aged out by wake time) and re-fetches models.dev
    /// when stale. Success broadcasts `CatalogEvent::Updated` so subscribers
    /// (model pickers) can invalidate. Exits immediately when fetches are
    /// disabled.
    fn spawn_refresh_task(self: &Arc<Self>) {
        if !self.config.fetch_enabled {
            return;
        }
        // Weak: the task must not pin the catalog in memory. When the last
        // owner (builder/consumer) drops it, the task exits on its next wake.
        let cat = Arc::downgrade(self);
        let window = self.config.mtime_window;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(window * 2).await;
                let Some(cat) = cat.upgrade() else {
                    break;
                };
                cat.refresh_if_stale_internal().await;
            }
        });
    }

    fn is_cache_fresh_internal(&self) -> bool {
        let meta = match std::fs::metadata(&self.config.cache_path) {
            Ok(m) => m,
            Err(_) => return false,
        };
        let modified = match meta.modified() {
            Ok(t) => t,
            Err(_) => return false,
        };
        let age = match SystemTime::now().duration_since(modified) {
            Ok(d) => d,
            Err(_) => return false,
        };
        age <= self.config.mtime_window
    }

    // ─── User overrides ────────────────────────────────────────────────

    fn apply_user_overrides_internal(&self) {
        let Ok(body) = std::fs::read_to_string(&self.config.override_path) else {
            return;
        };
        let Ok(overrides) = toml::from_str::<OverrideFile>(&body) else {
            tracing::warn!("catalog: invalid override TOML, ignoring");
            return;
        };
        let mut snap = self.state.write();
        let mut providers = snap.providers.clone();
        let mut models_map = snap.models.clone();
        // Apply via in-memory materialize-like pass
        for ovr in &overrides.provider {
            if let Some(slot) = providers.iter_mut().find(|p| p.id == ovr.id) {
                if let Some(d) = &ovr.display_name {
                    slot.display_name = d.clone();
                }
                if let Some(b) = &ovr.base_url {
                    slot.base_url = Some(b.clone());
                }
                if let Some(k) = &ovr.env_key {
                    slot.env_key = Some(k.clone());
                }
                slot.extra_headers = ovr.extra_headers.clone();
                if let Some(en) = ovr.enabled {
                    slot.default_enabled = en;
                }
            }
        }
        for ovr in &overrides.model {
            let entry = CatalogModelEntry {
                provider: ovr.provider.clone(),
                model_id: ovr.id.clone(),
                name: ovr.name.clone().unwrap_or_else(|| ovr.id.clone()),
                protocol: CatalogProtocol::OpenAiCompatible,
                source: CatalogSource::Override,
                base_url: None,
                reasoning: false,
                supports_vision: false,
                cost_input: ovr.cost_input.unwrap_or(0.0),
                cost_output: ovr.cost_output.unwrap_or(0.0),
                cost_cache_read: 0.0,
                cost_cache_write: 0.0,
                context_window: ovr.context_window.unwrap_or(0),
                max_tokens: ovr.max_tokens.unwrap_or(0),
                input_modalities: vec!["text".to_string()],
                release_date: None,
                status: None,
            };
            let inner = models_map.entry(ovr.provider.clone()).or_default();
            if let Some((_, slot)) = inner.iter_mut().find(|(_, m)| m.model_id == ovr.id) {
                // Partial update (same as apply_user_overrides).
                if let Some(n) = ovr.name.clone() {
                    slot.name = n;
                }
                if let Some(c) = ovr.cost_input {
                    slot.cost_input = c;
                }
                if let Some(c) = ovr.cost_output {
                    slot.cost_output = c;
                }
                if let Some(c) = ovr.context_window {
                    slot.context_window = c;
                }
                if let Some(m) = ovr.max_tokens {
                    slot.max_tokens = m;
                }
                slot.source = CatalogSource::Override;
            } else {
                inner.insert(ovr.id.clone(), entry);
            }
            // (same pattern continues — handled in line 663 etc.)
        }
        snap.providers = providers;
        snap.models = models_map;
        let _ = self.tx.send(CatalogEvent::OverrideApplied {
            path: self.config.override_path.clone(),
            provider_overrides: overrides.provider.len(),
            model_overrides: overrides.model.len(),
        });
    }

    // ─── Local discovery ───────────────────────────────────────────────

    async fn discover_local_all(&self) {
        if self.config.local_discovery_urls.is_empty() {
            return;
        }
        let urls = self.config.local_discovery_urls.clone();
        for base in urls {
            match fetch_local_models(&base).await {
                Ok(entries) if !entries.is_empty() => {
                    let count = entries.len();
                    let mut snap = self.state.write();
                    for entry in entries {
                        let inner = snap.models.entry(entry.provider.clone()).or_default();
                        inner.insert(entry.model_id.clone(), entry);
                    }
                    let _ = self.tx.send(CatalogEvent::LocalDiscovered {
                        base_url: base,
                        model_count: count,
                    });
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(error = %e, base = %base, "local discovery failed");
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Cache I/O helpers (sync, called from spawn_blocking)
// ═══════════════════════════════════════════════════════════════════════════

fn read_cache_if_fresh(path: &Path, window: Duration) -> Option<MdCatalog> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let age = SystemTime::now().duration_since(modified).ok()?;
    if age > window {
        return None;
    }
    let body = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<MdCatalog>(&body) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(error = %e, "cache corrupt, ignoring");
            let _ = std::fs::remove_file(path);
            None
        }
    }
}

/// Read the cache regardless of age — serve-stale fallback. A stale cache
/// is still strictly fresher than the build-time embedded snapshot, so it
/// beats SNAP whenever the fresh read misses. Corrupt files are removed
/// like the fresh path.
fn read_cache_stale_ok(path: &Path) -> Option<MdCatalog> {
    let body = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<MdCatalog>(&body) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(error = %e, "cache corrupt, ignoring");
            let _ = std::fs::remove_file(path);
            None
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// HTTP fetch with ETag conditional GET
// ═══════════════════════════════════════════════════════════════════════════

enum FetchResult {
    Updated(MdCatalog),
    NotModified,
}

async fn fetch_conditional(url: &str, etag: Option<&str>, user_agent: &str) -> Option<FetchResult> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .ok()?;
    let full = format!("{}/api.json", url.trim_end_matches('/'));
    for attempt in 0..FETCH_RETRIES {
        let mut req = client.get(&full).header("User-Agent", user_agent);
        if let Some(e) = etag {
            req = req.header("If-None-Match", e);
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.as_u16() == 304 {
                    return Some(FetchResult::NotModified);
                }
                if status.is_success() {
                    let body = resp.text().await.ok()?;
                    return serde_json::from_str::<MdCatalog>(&body)
                        .ok()
                        .map(FetchResult::Updated);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, attempt, "fetch failed");
            }
        }
        if attempt + 1 < FETCH_RETRIES {
            tokio::time::sleep(RETRY_BACKOFF).await;
        }
    }
    None
}

async fn fetch_local_models(base_url: &str) -> std::io::Result<Vec<CatalogModelEntry>> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(io_err)?;
    let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
    #[derive(Deserialize)]
    struct Resp {
        data: Vec<LocalModel>,
    }
    #[derive(Deserialize)]
    struct LocalModel {
        id: String,
    }
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(io_err)?
        .json::<Resp>()
        .await
        .map_err(io_err)?;
    let provider_id = derive_local_provider(base_url);
    let entries = resp
        .data
        .into_iter()
        .map(|m| {
            // Local `/v1/models` reports bare ids with no limits. Servers
            // usually serve upstream models (qwen3, llama-*, gemini-*, …),
            // so advisory cross-fill from the embedded models.dev catalog
            // beats a permanent "0 ctx" placeholder.
            let known = oxicode_ai::model_db::find_entry_by_model_id(&m.id);
            CatalogModelEntry {
                provider: provider_id.clone(),
                model_id: m.id.clone(),
                name: m.id,
                protocol: CatalogProtocol::OpenAiCompatible,
                source: CatalogSource::Local,
                base_url: Some(base_url.trim_end_matches('/').to_string()),
                reasoning: known.map(|e| e.reasoning).unwrap_or(false),
                supports_vision: known.map(|e| e.supports_vision()).unwrap_or(false),
                cost_input: known.map(|e| e.cost_input.max(0.0)).unwrap_or(0.0),
                cost_output: known.map(|e| e.cost_output.max(0.0)).unwrap_or(0.0),
                cost_cache_read: known.map(|e| e.cost_cache_read.max(0.0)).unwrap_or(0.0),
                cost_cache_write: known.map(|e| e.cost_cache_write.max(0.0)).unwrap_or(0.0),
                context_window: known.map(|e| e.context_window).unwrap_or(0),
                max_tokens: known.map(|e| e.max_tokens).unwrap_or(0),
                input_modalities: vec!["text".to_string()],
                release_date: None,
                status: None,
            }
        })
        .collect();
    Ok(entries)
}

fn derive_local_provider(base_url: &str) -> String {
    // Derive provider id from base URL host (strip port).
    // e.g. "http://localhost:11434" -> "localhost".
    let trimmed = base_url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let host = trimmed.split(':').next().unwrap_or("local");
    if host.is_empty() {
        "local".to_string()
    } else {
        host.to_string()
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

// ═══════════════════════════════════════════════════════════════════════════
// Port trait impl
// ═══════════════════════════════════════════════════════════════════════════

impl ModelCatalog for FileModelCatalog {
    fn list_providers(&self) -> Pin<Box<dyn Future<Output = SdkResult<Vec<String>>> + Send + '_>> {
        let snap = self.state.read();
        let mut ids: Vec<String> = snap.providers.iter().map(|p| p.id.clone()).collect();
        ids.sort();
        Box::pin(async move { Ok(ids) })
    }

    fn get_provider(
        &self,
        provider_id: &str,
    ) -> Pin<Box<dyn Future<Output = SdkResult<Option<CatalogProviderEntry>>> + Send + '_>> {
        let snap = self.state.read();
        let entry = snap.providers.iter().find(|p| p.id == provider_id).cloned();
        Box::pin(async move { Ok(entry) })
    }

    fn list_models(
        &self,
        provider_id: &str,
    ) -> Pin<Box<dyn Future<Output = SdkResult<Vec<CatalogModelEntry>>> + Send + '_>> {
        let snap = self.state.read();
        let list = snap
            .models
            .get(provider_id)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        Box::pin(async move { Ok(list) })
    }

    fn get_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Pin<Box<dyn Future<Output = SdkResult<Option<CatalogModelEntry>>> + Send + '_>> {
        let snap = self.state.read();
        let entry = snap
            .models
            .get(provider_id)
            .and_then(|m| m.get(model_id))
            .cloned();
        Box::pin(async move { Ok(entry) })
    }

    fn search(
        &self,
        pattern: &str,
    ) -> Pin<Box<dyn Future<Output = SdkResult<Vec<CatalogModelEntry>>> + Send + '_>> {
        let snap = self.state.read();
        let lower = pattern.to_lowercase();
        let out: Vec<CatalogModelEntry> = snap
            .models
            .values()
            .flat_map(|m| m.values())
            .filter(|e| {
                e.model_id.to_lowercase().contains(&lower)
                    || e.name.to_lowercase().contains(&lower)
                    || e.provider.to_lowercase().contains(&lower)
            })
            .cloned()
            .collect();
        Box::pin(async move { Ok(out) })
    }

    fn model_count(&self) -> Pin<Box<dyn Future<Output = SdkResult<usize>> + Send + '_>> {
        let snap = self.state.read();
        let count: usize = snap.models.values().map(|m| m.len()).sum();
        Box::pin(async move { Ok(count) })
    }

    fn refresh(&self) -> Pin<Box<dyn Future<Output = SdkResult<RefreshOutcome>> + Send + '_>> {
        let state = Arc::clone(&self.state);
        let tx = self.tx.clone();
        let config = self.config.clone();
        Box::pin(async move {
            if !config.fetch_enabled {
                return Ok(RefreshOutcome::Offline {
                    reason: "fetch_disabled",
                });
            }
            // Fast-path: cache fresh → no HTTP
            if is_cache_fresh_static(&config.cache_path, config.mtime_window) {
                return Ok(RefreshOutcome::Unchanged);
            }
            let etag = std::fs::read_to_string(&config.etag_path)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            match fetch_conditional(&config.models_dev_url, etag.as_deref(), &config.user_agent)
                .await
            {
                Some(FetchResult::Updated(md)) => {
                    let (providers, models) = materialize(&md, &OverrideFile::default());
                    let (pcount, mcount) = {
                        let mut snap = state.write();
                        snap.providers = providers;
                        snap.models = models
                            .into_iter()
                            .map(|(pid, list)| {
                                (
                                    pid,
                                    list.into_iter().map(|e| (e.model_id.clone(), e)).collect(),
                                )
                            })
                            .collect();
                        snap.stats()
                    };
                    // Best-effort persist (fire-and-forget).
                    if let Ok(body) = serde_json::to_string(&md) {
                        let _ = std::fs::create_dir_all(
                            config.cache_path.parent().unwrap_or(Path::new(".")),
                        );
                        let _ = std::fs::write(&config.cache_path, body);
                    }
                    let _ = filetime::set_file_mtime(
                        &config.cache_path,
                        filetime::FileTime::from_system_time(SystemTime::now()),
                    );
                    let _ = tx.send(CatalogEvent::Updated {
                        provider_count: pcount,
                        model_count: mcount,
                    });
                    Ok(RefreshOutcome::Updated {
                        provider_count: pcount,
                        model_count: mcount,
                    })
                }
                Some(FetchResult::NotModified) => {
                    let _ = filetime::set_file_mtime(
                        &config.cache_path,
                        filetime::FileTime::from_system_time(SystemTime::now()),
                    );
                    Ok(RefreshOutcome::Unchanged)
                }
                None => {
                    let (pcount, mcount) = state.read().stats();
                    let _ = tx.send(CatalogEvent::RefreshFailed {
                        reason: "network".into(),
                        provider_count: pcount,
                        model_count: mcount,
                    });
                    Ok(RefreshOutcome::Failed {
                        reason: "network".into(),
                    })
                }
            }
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<CatalogEvent> {
        self.tx.subscribe()
    }

    // ── Sync read-only API ───────────────────────────────────────────────
    // These acquire the `RwLock` read guard, clone the requested data, and
    // return immediately. No I/O. They reflect the currently loaded snapshot
    // (which may lag a successful `refresh()` until the consumer re-queries).

    fn list_providers_sync(&self) -> Vec<String> {
        let snap = self.state.read();
        let mut ids: Vec<String> = snap.providers.iter().map(|p| p.id.clone()).collect();
        ids.sort();
        ids
    }

    fn get_provider_sync(&self, provider_id: &str) -> Option<CatalogProviderEntry> {
        let snap = self.state.read();
        snap.providers.iter().find(|p| p.id == provider_id).cloned()
    }

    fn list_models_sync(&self, provider_id: &str) -> Vec<CatalogModelEntry> {
        let snap = self.state.read();
        snap.models
            .get(provider_id)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    fn get_model_sync(&self, provider_id: &str, model_id: &str) -> Option<CatalogModelEntry> {
        let snap = self.state.read();
        snap.models
            .get(provider_id)
            .and_then(|m| m.get(model_id))
            .cloned()
    }

    fn search_sync(&self, pattern: &str) -> Vec<CatalogModelEntry> {
        let snap = self.state.read();
        let lower = pattern.to_lowercase();
        snap.models
            .values()
            .flat_map(|m| m.values())
            .filter(|e| {
                e.model_id.to_lowercase().contains(&lower)
                    || e.name.to_lowercase().contains(&lower)
                    || e.provider.to_lowercase().contains(&lower)
            })
            .cloned()
            .collect()
    }

    fn model_count_sync(&self) -> usize {
        let snap = self.state.read();
        snap.models.values().map(|m| m.len()).sum()
    }
}

fn is_cache_fresh_static(path: &Path, window: Duration) -> bool {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let modified = match meta.modified() {
        Ok(t) => t,
        Err(_) => return false,
    };
    let age = match SystemTime::now().duration_since(modified) {
        Ok(d) => d,
        Err(_) => return false,
    };
    age <= window
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AuthMethod;

    #[test]
    fn read_cache_stale_ok_reads_regardless_of_age() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache.json");
        std::fs::write(
            &path,
            r#"{"p":{"name":"P","env":[],"models":{"m":{"name":"M","reasoning":false,"limit":{"context":1000,"output":100}}}}}"#,
        )
        .unwrap();
        assert!(read_cache_stale_ok(&path).is_some());
        std::fs::write(&path, "not json").unwrap();
        assert!(read_cache_stale_ok(&path).is_none());
        assert!(!path.exists(), "corrupt cache must be removed");
    }

    #[test]
    fn protocol_for_anthropic() {
        assert_eq!(
            protocol_for("@ai-sdk/anthropic"),
            CatalogProtocol::AnthropicMessages
        );
    }
    #[test]
    fn protocol_for_google() {
        assert_eq!(
            protocol_for("@ai-sdk/google"),
            CatalogProtocol::GoogleGenerativeAi
        );
    }
    #[test]
    fn protocol_for_openai_compat() {
        assert_eq!(
            protocol_for("@ai-sdk/openai-compatible"),
            CatalogProtocol::OpenAiCompletions
        );
    }
    #[test]
    fn protocol_for_unknown_is_openai_compatible() {
        assert_eq!(
            protocol_for("some-new-sdk"),
            CatalogProtocol::OpenAiCompatible
        );
    }
    #[test]
    fn protocol_for_empty_is_openai_compatible() {
        assert_eq!(protocol_for(""), CatalogProtocol::OpenAiCompatible);
    }

    #[test]
    fn default_auth_for_anthropic_is_xapikey() {
        assert_eq!(
            CatalogProtocol::AnthropicMessages.default_auth(),
            AuthMethod::XApiKey
        );
    }
    #[test]
    fn default_auth_for_azure_is_apikey() {
        assert_eq!(
            CatalogProtocol::AzureOpenAiResponses.default_auth(),
            AuthMethod::ApiKey
        );
    }
    #[test]
    fn default_auth_for_google_is_none() {
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
    }
    #[test]
    fn default_auth_for_openai_compat_is_bearer() {
        assert_eq!(
            CatalogProtocol::OpenAiCompletions.default_auth(),
            AuthMethod::Bearer
        );
        assert_eq!(
            CatalogProtocol::OpenAiCompatible.default_auth(),
            AuthMethod::Bearer
        );
        assert_eq!(
            CatalogProtocol::OpenAiResponses.default_auth(),
            AuthMethod::Bearer
        );
    }

    #[test]
    fn as_oxicode_api_round_trip() {
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
            CatalogProtocol::OpenAiCompatible.as_oxicode_api(),
            Api::OpenAiCompletions
        );
        assert_eq!(
            CatalogProtocol::GoogleGenerativeAi.as_oxicode_api(),
            Api::GoogleGenerativeAi
        );
    }

    #[test]
    fn snapshot_loads_and_has_expected_size() {
        let catalog = load_snapshot().expect("SNAP must load");
        assert!(!catalog.0.is_empty(), "SNAP should have providers");
        let model_count: usize = catalog.0.values().map(|p| p.models.len()).sum();
        assert!(
            model_count > 1000,
            "SNAP should have many models, got {model_count}"
        );
    }

    #[test]
    fn materialize_produces_nonzero_entries() {
        let catalog = load_snapshot().expect("SNAP");
        let (providers, models) = materialize(&catalog, &OverrideFile::default());
        assert!(!providers.is_empty());
        let count: usize = models.values().map(|v| v.len()).sum();
        assert!(count > 0);
    }

    #[test]
    fn override_replaces_existing_model() {
        let mut providers = vec![CatalogProviderEntry {
            id: "test".into(),
            display_name: "Original".into(),
            aliases: vec![],
            protocol: CatalogProtocol::OpenAiCompletions,
            env_key: Some("TEST_KEY".into()),
            extra_env_keys: vec![],
            base_url: Some("https://api.test.com".into()),
            extra_headers: vec![],
            category: String::new(),
            description: String::new(),
            default_enabled: true,
        }];
        let mut models: BTreeMap<String, Vec<CatalogModelEntry>> = BTreeMap::new();
        models.insert(
            "test".into(),
            vec![CatalogModelEntry {
                provider: "test".into(),
                model_id: "test-model".into(),
                name: "Original".into(),
                protocol: CatalogProtocol::OpenAiCompletions,
                source: CatalogSource::Embedded,
                base_url: None,
                reasoning: false,
                supports_vision: false,
                cost_input: 0.0,
                cost_output: 0.0,
                cost_cache_read: 0.0,
                cost_cache_write: 0.0,
                context_window: 1000,
                max_tokens: 100,
                input_modalities: vec!["text".into()],
                release_date: None,
                status: None,
            }],
        );
        let overrides = OverrideFile {
            model: vec![OverrideModel {
                provider: "test".into(),
                id: "test-model".into(),
                name: Some("Overridden".into()),
                cost_input: Some(99.0),
                cost_output: None,
                context_window: None,
                max_tokens: None,
            }],
            ..Default::default()
        };
        apply_user_overrides(&mut providers, &mut models, &overrides);
        let entry = models
            .get("test")
            .unwrap()
            .iter()
            .find(|m| m.model_id == "test-model")
            .unwrap();
        assert_eq!(entry.name, "Overridden");
        assert_eq!(entry.source, CatalogSource::Override);
        assert!((entry.cost_input - 99.0).abs() < 1e-9);
        assert_eq!(entry.context_window, 1000, "untouched field kept");
    }
}
