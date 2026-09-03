# Workspace Retrieval: Native Exact Search and oxibrain-Backed Discovery

**Status:** Adopted for Phase 1 (exact-search v2 shipped; Phase 2 skeleton tracked separately)  
**Date:** 2026-09-03  
**Owners:** Oxicode maintainers  
**Depends on:** `oxicode-agent` tools, `oxicode-sdk` behavior packs, `oxicode-cli`
composition and settings, and the oxibrain document plane (`oxibrain-client`
0.12+: daemonless `admin serve --stdio` child, `register_document_root`,
`search_planes`).
**External dependency:** none.  zvec-grep (npm) is idea provenance only —
its indexed-retrieval shape (BM25 + vector + rank fusion, workspace index,
read-only agent surface, freshness semantics) is reproduced on the oxibrain
document plane we already own.

## Summary

Oxicode will improve workspace retrieval along two deliberately separate paths:

1. **Exact search**: replace the current in-process, recursive `GrepTool`
   walker with a native Rust exact-search engine that respects ignore rules,
   streams files, honours cancellation, and preserves the typed `grep` tool
   contract.
2. **Discovery search**: optionally expose a stable, read-only
   `workspace_search` agent tool backed by the local oxibrain store's
   *document plane* — chunk lexical (TF-IDF/FTS) + dense (embedding vector)
   retrieval with hybrid fusion over a per-workspace index.  It is for
   questions where names, paths, or wording are unknown; it returns ranked
   evidence, after which the agent verifies with `read` or `grep`.

The two paths complement rather than replace one another.  `workspace_search`
does not become a durable-memory backend, does not create indexes implicitly,
and is not required to run Oxicode.  An unavailable oxibrain installation
must leave exact search and the rest of the coding environment fully usable.

```text
Known identifier / text / regex                     Unknown location / concept
──────────────────────────────                     ──────────────────────────
Agent `grep`                                          Agent `workspace_search`
    │                                                         │
    ▼                                                         ▼
ExactSearchEngine                                  oxibrain document plane
(ignore + regex streaming)                         (spawned `admin serve --stdio`
    │                                              child; no resident daemon)
    ▼                                                         │
Bounded path:line output                              TF-IDF + chunk vectors, hybrid
                                                            │
                                                            ▼
                                                  Ranked source passages
                                                            │
                                                            ▼
                                                Oxicode `read` / `grep` verifies
```

## Context

The zvec-grep product supplies the same two capabilities under different
names (`zvec_grep_rg` managed ripgrep; `zvec_grep_search` BM25+vector
indexed retrieval), and its documented grep/search division motivates this
design.  It is not the integration target: it would add a Node.js/npm
runtime dependency, and its MCP route requires a resident loopback server.

The retrieval ideas transfer to infrastructure we already ship.  oxibrain's
**document plane** (two-plane design, 0.12+) is a rebuildable per-store
cache (`documents.db`) that chunks configured document roots into
lexical (chunk FTS/TF-IDF) and dense (chunk embedding vector) indexes with
hybrid fusion, reconciles roots against the filesystem at query time
(freshness evidence: `reconciled_roots`, `skipped_roots`,
`dense_coverage`), and validates each hit against the current file
revision.  Its topology is **daemonless**: the client spawns
`oxibrain admin serve --stdio --dir <dir>` as a caller-owned child
(`BrainClient::spawn_local`, `kill_on_drop`) — no launchd service, no
well-known socket.  Registration is an idempotent upsert
(`register_document_root`: `space`, `alias`, `path`, `include`, `exclude`,
`max_file_bytes`) into a `documents.toml` written only by oxibrain, and
git roots honor repository ignore rules through gix.

Indexed discovery can reduce an agent's broad scans, follow-up reads, and
input tokens for repository-comprehension questions.  It is not a free
latency win: first-query indexing, local embedding inference, and chunk
storage all have costs.  Therefore it stays optional, off by default, and
is measured as a retrieval and agent-efficiency feature, not marketed as a
replacement for `rg`.

## Goals

1. Make exact `grep` substantially more efficient and ripgrep-compatible in
   the common repository case without adding Node.js, zvec-grep, or a daemon as
   an Oxicode runtime dependency.
2. Give agents a narrow, stable tool for indexed workspace discovery when the
   user has explicitly enabled oxibrain-backed workspace search.
3. Preserve Oxicode's tool names, policy boundary, workspace isolation,
   behavior-pack compatibility process, and usable offline baseline.
4. Keep persistent index administration user-controlled and observable.
5. Measure user-visible effects: answer quality, tool calls, input tokens,
   tool latency, agent wall time, index build time, index size, and memory.

## Non-goals

- Reimplement chunking, embedding, or hybrid ranking in Oxicode — the
  oxibrain document plane owns them.
- Make the workspace index a `MemoryStore`, `EmbeddingProvider`, or durable
  memory.  Durable memory remains exclusively oxibrain; the document plane
  is a disposable cache and never enters the episode ledger.
- Expose oxibrain index administration (root registration/removal, extraction
  drain, redaction) to an agent.
- Spawn the oxibrain child, download models, or write a document root without
  explicit user opt-in.
- Replace `grep` with a shell command or accept an arbitrary `rg` command
  string in Oxicode's model-facing API.
- Add a new SDK port or workspace crate.  Workspace retrieval is an agent
  capability wired by the composition root, not a product infrastructure port.

## Decisions

### D1. Keep `grep` and indexed retrieval as two tools

`grep` remains the exact, bounded, typed search tool.  It is the default for:

- exact names, symbols, paths, file names, configuration keys, quotations, and
  regular expressions;
- exhaustive occurrence requests;
- small, one-off searches where an index would cost more than it saves.

`workspace_search` is a separate optional tool.  It is selected for a
workspace-grounded question whose relevant wording or location is unknown, or
which requires relationship, chronology, causality, comparison, data flow, or
cross-file synthesis.  The agent performs at most one focused discovery probe
before falling back to exact search; a no-result from an index is evidence, not
a reason to broaden the query repeatedly.

This is intentionally the same division zvec-grep documents between native
grep/rg and its indexed search.  zvec-grep's `rg` facility is useful to an
operator, but it is not the first integration target: Oxicode already owns a
safer typed `grep` contract and should make that primitive fast itself.

### D2. Make exact search native Rust, not a zvec-grep dependency

Introduce an internal `ExactSearchEngine` in `oxicode-agent`, used by the
existing `GrepTool`.  It will use established Rust search primitives (the
`ignore` walker plus streaming regex search) rather than read complete files
into memory.

The engine owns traversal, ignore filtering, binary detection, matching,
context coalescing, output limits, and cancellation.  `GrepTool` continues to
own its JSON schema, `PathGuard`, output rendering, and internal-URL search
path.

Expected implementation boundary:

```text
oxicode-agent/src/tools/
  grep.rs                 AgentTool schema + renderer + internal URLs
  exact_search.rs         private ExactSearchEngine and result records
```

The first version accepts the current schema, including `literal`,
`case_insensitive`, `context`, `include`, and `max_results`.  It upgrades
`include` to a real glob matcher but keeps the current rendered
`path:line: text` and `path-line- text` formats and the 500-character line
cap.  The cancellation receiver is checked while walking and matching.

The new default ignore policy will match ripgrep's repository-oriented policy:
`.gitignore`, `.git/info/exclude`, global ignore rules, hidden-file exclusion,
and built-artifact exclusions are applied.  Explicit paths and an eventual
opt-in `hidden` parameter retain an escape hatch.  Symlinks are not followed by
default.  This is a semantic change from the legacy walker, so the behavior
pack implementation identity must change from `grep.search.v1` to
`grep.search.v2`; the new fixture must prove the advertised behavior before
the compatibility ledger is advanced.

The legacy `GrepTool` implementation remains available only to legacy registry
consumers during the migration window.  `coding-omp-v1` receives the v2
implementation through its normal installer, never through an ad-hoc CLI tool
list.

### D3. Wrap the oxibrain document plane behind a stable Oxicode tool

Do not make the model depend on raw engine tool names, evolving vendor
schemas, or a generic MCP proxy.  Add a first-class optional
`WorkspaceSearchTool` with a stable Oxicode schema:

```json
{
  "query": "where is session ownership validated?",
  "limit": 8
}
```

`query` is required; `limit` defaults to 8 and is capped at 20.  There is no
`root`, `include`, `exact_terms`, `freshness`, or `vector` parameter —
scoping is a registration concern (D4), exact terms belong to `grep` (D1),
and freshness is inherent to query-time reconciliation, not a caller
choice.  Those omissions are deliberate policy boundaries, not missing
features.

The tool is backend-abstracted exactly like the memory tools
(`MemoryBackend` precedent in `oxicode-agent/src/tools.rs`):

```text
oxicode-agent/src/tools/workspace_search.rs
    WorkspaceSearchTool + schema + envelope rendering
    trait WorkspaceSearchBackend { search(query, limit) -> Vec<SearchPassage> }
oxicode-cli/src/foundation/brain_workspace.rs
    BrainWorkspaceSearch — oxibrain-client implementation
oxicode-cli/src/bootstrap.rs                     conditional registration
oxicode-cli/src/store/settings.rs                opt-in settings (D4)
```

`BrainWorkspaceSearch` rides the daemonless transport:
`BrainClient::spawn_local(LocalProcessEndpoint { executable, dir })` spawns
`<executable> admin serve --stdio --dir <brain-dir>` as a caller-owned
child (`kill_on_drop`; nothing survives the session).  Each search calls
`search_planes(query, space, mode = "hybrid", limit, planes =
["documents"])` so memory-plane hits never mix into code retrieval, and
maps `DocumentHitDto` (`locator`, `ordinal`, `revision`, `score`,
chunk `text`, freshness) into `SearchPassage`.  Hits are chunk-level:
`locator` names the file and `ordinal` the chunk — line numbers are
deliberately absent, and the envelope directs the agent to `read`/`grep`
for exact positions.

`search` is space-scoped and has no root filter, and the client cannot
create spaces — so a shared space would leak other projects' chunks into
the results.  `BrainWorkspaceSearch` therefore over-fetches
(`min(limit * 3, 48)`), post-filters hits to the workspace's own root
alias, and truncates to `limit`; cross-project contamination becomes a
recall caveat, not a correctness one.  On first use it also verifies via
`list_spaces()` that the configured space exists — a missing space
disables the root with a doctor error, which must surface as the typed
unavailable result, not an empty result set.

The envelope is a small Oxicode-owned text block: ranked
`locator (score)` passages with bounded snippets, a freshness line derived
from the reconcile report (`reconciled` / `stale` / `dense coverage`), and
a reminder to verify evidence with `read` or `grep`.  Raw DTO JSON is never
shown to the model.

Registration timing: the CLI resolves the backend lazily on first
`workspace_search` call, not at startup — a missing `oxibrain` executable
or store cannot slow or break sessions that never use the tool, and the
tool is registered whenever the setting is enabled (the executable is
resolved per call; a failure surfaces as the typed unavailable result).

If the child cannot spawn, the space/root is missing, or reconciliation
fails, `workspace_search` returns a typed unavailable/error result that
names `grep` and `read` as alternatives and points at the enabling
setting.  It must never silently convert the failure into a broad
filesystem scan or claim that no relevant source exists.

### D4. Index lifecycle stays user-controlled; the agent stays read-only

The data plane is read-only agent retrieval.  The control plane belongs to
the user, expressed as Oxicode settings plus oxibrain's own boundaries:

```toml
# <oxicode home>/settings.toml — all keys default off/absent
[workspace_search]
enabled    = true                          # explicit opt-in; false by default
executable = "/path/to/oxibrain"           # default: probe PATH, then ~/.oxi/bin
space      = "dev"                         # brain space the workspace registers into
include    = ["**/*.rs", "**/*.ts", ...]   # code-oriented default glob set
exclude    = ["**/target/**", "**/node_modules/**", ...]
max_file_bytes = 10485760
```

On the first search of a session, with `enabled = true`, the CLI performs
the idempotent `register_document_root` upsert for the canonical workspace
(deterministic alias: directory name + short content hash of the path;
re-registration is a no-op).  Index creation, refresh, and storage are
oxibrain's: roots reconcile at query time, chunks live in the store's
rebuildable `documents.db`, and `documents.toml` is written only by
oxibrain (atomic temp+rename).  Oxicode never mutates a repository file,
never downloads a model, and never spawns the child when `enabled` is
false.  Removal of a root remains an operator action through oxibrain tooling
(`oxibrain` CLI / Brain UI); a later `oxicode search` subcommand may wrap
status/diagnostics (Phase 3) but grants the agent nothing.

### D5. Local-only and workspace-safe by default

The default (and only supported) topology is fully local: a caller-owned
`oxibrain admin serve --stdio` child, a brain store under the user's home,
and local embedding models.  No loopback endpoint, bearer token, or
remote-embedding authorization surface exists on this path.  Remote
embedding providers remain an oxibrain-side authorization decision; if the
user enables one there, it is outside Oxicode's boundary and outside this
tool's data path.

`WorkspaceSearchTool` validates the canonical workspace root using the
strict `PathGuard::validate` boundary before the backend issues any
request, and the registered root path is that canonical path only —
model input can never redirect the index at another tree.  Result limits
are capped, snippet sizes are bounded, per-call timeouts are bounded, and
all failures are audited through the normal tool event path.

## Configuration and startup

Configuration lives in Oxicode settings (D4 shows the full table); the
brain store location reuses the Foundation discovery order
(`$OXIBRAIN_SOCKET`-independent: the store dir, not a socket — canonical
`~/.oxi/brain`).  There is no MCP server entry: the child is spawned by
the CLI backend itself, inherits no ambient state, and dies with the
session.  `enabled` is false by default; every other key has a safe
default.  A user who wants raw engine access can still mount oxibrain's
MCP surface through the existing generic `mcp` tool — that route is
independent of this wrapper and stays cache-driven as today.

Startup cost of the disabled state is zero: no spawn, no probe, no
registration, no prompt fragment.  The enabled state resolves the
executable lazily on first use (see D3).

## Prompt and behavior-pack composition

`workspace_search` is a host-product optional tool, not part of the canonical
OMP compatibility tool set.  The CLI adds this short prompt fragment only if
the wrapper registered successfully:

```text
For a workspace-grounded question with unknown wording or location, use one
focused workspace_search query.  Verify its source passages with read or grep.
Use grep for exact identifiers, paths, literals, regular expressions, and
exhaustive occurrence searches.  Do not use workspace_search to create, update,
or delete an index.
```

The fragment must not mention engine internals (oxibrain or otherwise) or
tell the model to use a tool that is unavailable.  No change is made to the
OMP compatibility ledger for this optional host capability.  The `grep`
engine change, however, follows the descriptor replacement and
ledger-fixture rules described in D2.


### Phase 0 — Baseline and decision record

- Capture baseline exact-search and agent-retrieval measurements on small,
  medium, and large repositories; include this repository as one Rust
  monorepo fixture.
- Record current `grep` result semantics, ignored-path behavior, context
  rendering, binary-file handling, cancellation, and peak memory.
- Pin the oxibrain build used for all POC runs (release binary or crates.io
  version).  Do not benchmark against a moving checkout.

### Phase 1 — Native exact-search v2

- Implement `ExactSearchEngine` and wire `GrepTool` v2 through
  `coding-omp-v1` as `grep.search.v2`, declaring a replacement for v1.
- Add ignore/glob, context, binary, symlink, cancellation, limit, unicode, and
  output-compatibility tests.
- Run a performance comparison against the legacy walker and `rg` for exact
  queries.  Keep the Rust engine; do not shell out to either `rg` or `zg`.

### Phase 2 — Indexed discovery skeleton (oxibrain document plane)

- Bump `oxibrain-client` to 0.10.1+ in `oxicode-cli` as a renamed dependency
  (`oxibrain-client-brain`), keeping the legacy 0.2 memory wiring untouched
  during the migration window; unify in a dedicated memory-transport PR.
  (0.10.1 already ships `register_document_root`, `search_planes`, and the
  `admin serve --stdio --dir` spawn argv; pin client and binary together.)
- Add `WorkspaceSearchBackend` + `WorkspaceSearchTool` (agent crate) and
  `BrainWorkspaceSearch` (CLI) per D3/D4, behind
  `[workspace_search] enabled` (default false).
- Unit-test the tool against a fake backend; integration-test the CLI
  backend in-process against `run_session` (duplex pipe, temp `--dir`),
  plus one ignored live test against a real pinned binary.
- Wire conditional registration + prompt fragment in the composition root.
- Run controlled paired baseline/treatment evaluations.  The only treatment
  changes are the prepared index, the optional tool, and its conditional
  routing guidance.
- No setup wizard, automatic spawning, or index-management agent tool.

### Phase 3 — Operator UX and hardening

- Add explicit `oxicode search` status/diagnostics operations and TUI
  visibility (root registration state, freshness report, executable probe).
- Add stale-index and dense-coverage reporting; document root removal as an
  oxibrain-side operator action.
- Promote the setting (docs, defaults) only if Phase 2 meets the acceptance
  criteria below.

## Acceptance criteria and measurement

### Exact search

- All v1 contract fixtures that remain intentional pass under v2; changed
  ignore semantics have explicit fixtures and release notes.
- No whole-file `Vec<String>` allocation is required to search a normal text
  file.
- Cancellation ends a broad search promptly and produces no partial success
  that looks exhaustive.
- On the benchmark corpus, v2 improves p95 exact-search latency and peak memory
  against the legacy walker; it must not regress correctness relative to an
  agreed ripgrep command profile.

Measured on the exact-search v2 stack (2026-09-04, Apple M4, release build,
warm page cache, best of 5 runs of `cargo run --release -p oxicode-agent
--example grep_bench -- . <pattern>` against a repository worktree; corpus:
911 tracked files / 19.9 MB, target dirs excluded by ignore rules):

| pattern      | legacy walker | ExactSearchEngine v2 | rg yardstick |
|--------------|--------------:|---------------------:|-------------:|
| `needle_fn`  | 58 ms         | 21 ms                | ~19 ms       |
| `fn execute` | 59 ms         | 18 ms                | ~19 ms       |

On this corpus v2 is consistently ~3x faster than the legacy walker and in
the same band as the manual `rg -c <pattern> .` yardstick (best of 5, warm);
`rg` is a manual lower-bound reference only — product code never shells out
to it. Legacy walker time does not reach the seconds range here because the
walker's per-file async overhead dominates only on colder caches and larger
trees; the ordering (v2 < legacy) held in every measured run.

### Indexed discovery

- On a repository-comprehension suite, treatment answer quality is non-inferior
  to baseline and has a pre-registered improvement in at least one efficiency
  metric (median input tokens, tool calls, or agent wall time).  Index build
  time and storage are reported separately.
- Results are evaluated over repeated independent runs with identical model,
  model version, reasoning setting, task prompt, workspace revision, timeout,
  and base tool set.  A single favourable trajectory is not evidence.
- Exact-identifier and exhaustive-search tasks do not regress: routing uses
  `grep`, not `workspace_search`.
- Disabling the setting, removing the oxibrain executable, or deleting the
  document-plane index cannot prevent an agent from using the standard coding
  tool set.

## Test strategy

| Layer | Coverage |
|---|---|
| `exact_search` unit tests | ignore rules, globs, regex/literal modes, binary detection, line/context accounting, long-line truncation, cancellation, symlink policy |
| `GrepTool` integration tests | schema compatibility, PathGuard boundary, output formatting, internal URLs, v1/v2 migration fixtures |
| `WorkspaceSearchTool` unit tests | schema caps, envelope rendering, unavailable/missing-backend errors, backend-trait boundary (fake backend) |
| backend integration tests | in-process `run_session` duplex (temp `--dir`): success, empty index, reconcile-freshness reporting, child-spawn failure |
| optional live test | pinned oxibrain release binary + local model, `#[ignore]`-gated, excluded from normal offline CI; validates real registration + hybrid retrieval |
| behavior fixtures | `grep.search.v2` output and routing tests; prompt fragment appears only with registered workspace search |
| benchmark harness | paired task records including tool trace, token counts, latency, index preparation duration/size, and environment manifest |

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| First-query index build (chunking + embedding a large tree) is slower than retrieval saves | Opt-in by default-off; report reconcile/index cost in the envelope; require measured net value before promotion. |
| Agent overuses semantic search for trivial exact lookups | Conditional routing prompt, capped single probe, and keep `grep` as the exact default. |
| Chunk-level hits lack line spans | Envelope surfaces `locator` + snippet and directs the agent to `read`/`grep` for exact positions; oxibrain may add spans later. |
| oxibrain client version skew (memory pins 0.2, workspace path needs 0.10.1+) | Renamed coexisting dependency; unify in a dedicated memory-transport PR; contract-test the tool against the pinned client. |
| Space-scoped search has no root filter (cross-project hits) | Over-fetch + post-filter by root alias; dedicated per-workspace spaces deferred until oxibrain exposes space creation to clients. |
| Chunker is paragraph/line-oriented, not symbol-aware | Phase 2 paired evaluation measures retrieval quality; code-aware chunking is an oxibrain-side follow-up, not an Oxicode hack. |
| An agent mutates index or memory state | Documents-plane search only; root registration/removal and extraction stay operator-side oxibrain actions. |
| Query text or code reaches a remote embedding provider | The document plane uses the store's local model configuration; remote embeddings are an oxibrain-side authorization outside this tool's path. |
| The child searches or registers a root outside the active workspace | Root path is the `PathGuard::validate`-canonicalized workspace only; model input cannot set `root`. |
| Exact-search migration changes visible behavior | Use `grep.search.v2`, fixture-backed ledger review, release notes, and a bounded legacy compatibility window. |

## Alternatives considered

### Wrap zvec-grep directly (MCP or CLI)

Rejected.  It adds a Node.js/npm runtime dependency to a Rust product, and
the indexed-search route is MCP-only, which forces a resident loopback
server (`zg server on`) with its own lifecycle, token, and daemon logs —
exactly the operational weight this design set out to avoid.  The same
retrieval shape (lexical + vector + fusion, per-workspace index, freshness,
read-only agent surface) already exists in the oxibrain document plane we
own, daemonless.

### Make exact `grep` shell out to `rg` or `zg query --rg`

Rejected as the primary design.  Oxicode would gain an external binary
dependency and an opaque command-string interface for a capability the
agent runtime should own natively.  `rg` remains the correctness yardstick
for Phase 1, not a dependency.

### Expose the engine's raw tool surface to the agent

Rejected.  oxibrain's MCP surface includes ingest, extraction, and
redaction operations; presenting it raw would give the model write paths
over durable memory and index administration, contrary to the authority
boundary.  The wrapper exposes one read-only operation.

### Add a `WorkspaceRetrieval` SDK port

Rejected.  The external index is an optional agent tool, not a product-owned
infrastructure contract like state, auth, or memory.  A port would duplicate
composition and conflict with the established rule that optional capability
backends are wired by the composition root behind agent-side traits
(`MemoryBackend` precedent).

## Open questions

1. ~~Which local embedding model should the POC standardize on?~~ Resolved:
   whatever the brain store's document plane is configured with (currently a
   local gguf embedding model under `~/.oxi/brain/models`); Oxicode does not
   pick or ship models.
2. ~~Freshness mode exposure?~~ Resolved: freshness is inherent to oxibrain's
   query-time reconciliation; no caller-facing `freshness` parameter.  The
   envelope reports reconcile status honestly.
3. What benchmark task set best represents Oxicode's users beyond generic
   repository QA: bug localization, architecture questions, implementation
   planning, or multi-crate dependency tracing?
4. ~~How long should the legacy v1 grep implementation remain reachable?~~
   Proposed: `with_builtins_cwd` keeps the legacy walker through 0.82.x and
   it is removed in 0.83.0; the pack installs v2 only.

