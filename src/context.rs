//! The cell context document (ADR 0012): the cell's interface made
//! machine-readable — what `/openapi.json` is to an API, this is to a data
//! product. One artifact, two doors: `GET /context` on the serving plane and
//! `datamk context` on stdout. The document is a *projection* of the cell,
//! never a separate product: write the cell and the context exists; build the
//! cell and it becomes trustworthy.
//!
//! The single most important shape constraint (ADR 0012 §2): `declared` holds
//! author claims, `observed` holds machine facts, and the two are never
//! flattened — an agent must never be able to mistake a claim for a
//! measurement. Absent facts are omitted or `null`, never fabricated.

use anyhow::Result;
use indexmap::IndexMap;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::{CellDef, ColumnSpec, Contract, Export, Source, Visibility};
use crate::engine::run_summary::RunSummary;

/// The document-schema version (`datamk_context`). An integer, distinct from
/// cell semver and `datamk_version`: additive changes don't bump; any removal,
/// rename, or re-meaning bumps, with the prior version served through a
/// deprecation window (ADR 0012 §2).
pub const DATAMK_CONTEXT_VERSION: u32 = 1;

/// The `limit` in every emitted `sample_request` — the smallest useful legal
/// call, a pure function of the route key and the limit grammar.
const SAMPLE_LIMIT: usize = 10;

/// One cell's context document. Field names are a stable contract the moment
/// an agent scripts against them (ADR 0012 Consequences) — do not rename or
/// re-mean without bumping `DATAMK_CONTEXT_VERSION`; additive fields are fine.
/// Guarded by the golden serialization test below, per the `RunSummary`
/// precedent.
///
/// Always constructed through the builders in this module and serialized from
/// the typed struct — never inline `json!`.
#[derive(Debug, Clone, Serialize)]
pub struct ContextDocument {
    pub datamk_context: u32,
    pub cell: String,
    /// `draft` | `verified`. Verified means exactly one thing: real provenance
    /// (a published, verify-gated execution) stands behind this document.
    /// Pinless ⇒ draft, by definition (ADR 0012 §4) — a direct-attach cell has
    /// no pin and no run summary, so it is draft even after a local build; the
    /// engine-emitted note says why.
    pub status: Status,
    /// `false` unless a verified build stands behind the document — never
    /// `null`, never `true` by assumption (ADR 0012 §2).
    pub grain_verified: bool,
    /// Author claims: the interface exactly as `cell.yaml` declares it.
    pub declared: Declared,
    /// Machine facts. `null` (never `{}`) when nothing has been built or
    /// verified behind this document.
    pub observed: Option<Observed>,
    pub data: DataBlock,
    /// Engine-emitted only — no author-supplied string ever lands here
    /// (author prose lives in `declared`, labeled as a claim).
    pub notes: Vec<String>,
    /// Which optional sections this response inlines (ADR 0013) —
    /// engine-emitted, always present: `[]` on the default variant, `["docs"]`
    /// under `?include=docs` (served) or the default portable emission. Lets
    /// an agent distinguish "server predates this field" (absent — an old
    /// binary) from "this cell has no docs" (present, and `docs` below is
    /// `Some({})`), the same assert-absence discipline as `observed`.
    pub included: Vec<String>,
    /// Docs page content (ADR 0013), present ONLY when `included` contains
    /// `"docs"`. Top-level (not nested under `declared`) so a prose-only edit
    /// never moves `interface_digest` — that digest is also `/openapi.json`'s
    /// `info.version` and the mesh manifest's `context_digest`, and a prose
    /// typo must not tell generic tooling the callable surface changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs: Option<IndexMap<String, DocsContentEntry>>,
    /// Portable artifact only (`datamk context`): when the document was
    /// emitted. A hosted `/context` omits it — the response is always now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emitted_at: Option<String>,
    /// Portable artifact only: sha256 of the `cell.yaml` this document was
    /// emitted from, so a reader can tie the file back to a definition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell_yaml_digest: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Draft,
    Verified,
}

/// The declared interface: visibility-filtered exports (a `private` export
/// appears nowhere, in any form, not even as a name — ADR 0012 §4) plus
/// nominal one-hop upstream edges.
#[derive(Debug, Clone, Serialize)]
pub struct Declared {
    /// The cell's one-line description (ADR 0012 §3) — an author claim,
    /// which is why it lives here and not at the top level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub exports: Vec<DeclaredExport>,
    /// `{ref, version}` from `cell` sources — never the upstream `table`
    /// (the upstream owner's to disclose, on its own document, under its own
    /// auth — ADR 0012 §5).
    pub upstreams: Vec<UpstreamRef>,
    /// Docs page **identity only** (ADR 0013) — `{target, path, media_type}`
    /// per declared page, always present (`[]` when none). No
    /// content-derived value here (no sha256, no bytes): this struct is what
    /// `interface_digest` serializes whole, and a prose typo must not tell
    /// generic OpenAPI tooling the callable surface changed. Identity
    /// (adding/removing/renaming a page) legitimately does move the digest —
    /// see `digest_tracks_docs_identity_but_ignores_content_and_fingerprint`.
    pub docs: Vec<DeclaredDocsEntry>,
    /// The affordance to fetch the pages above — a constant, always present,
    /// the same precedent as `DeclaredExport::query` (an affordance field
    /// inside `declared`).
    pub include_request: String,
}

/// `/context?include=docs` — the one and only door to docs content (ADR
/// 0012 §4: one document, one route; there is no `/docs/:name`).
const INCLUDE_DOCS_REQUEST: &str = "/context?include=docs";

/// One declared docs page's identity — never its content or a
/// content-derived fingerprint (those live at the top-level `docs` field and
/// under `observed.docs` respectively).
#[derive(Debug, Clone, Serialize)]
pub struct DeclaredDocsEntry {
    /// `"cell"` or the route key (`name@major`) — route keys always carry
    /// `@major`, so an export can never collide with the literal `"cell"`.
    pub target: String,
    pub path: String,
    pub media_type: String,
}

/// One docs page's content, as served under `?include=docs` (ADR 0013).
/// Never under `observed` — author bytes are a claim, not a measurement.
#[derive(Debug, Clone, Serialize)]
pub struct DocsContentEntry {
    pub media_type: String,
    pub content: String,
}

/// One docs page's content fingerprint (ADR 0013) — a machine fact computed
/// at release time and carried through `published.json`, never author bytes.
/// Present in both the default and docs variant (it is cheap identity data,
/// not gated behind `include=docs`) and never in `interface_digest`.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct DocsFingerprint {
    pub sha256: String,
    pub bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeclaredExport {
    pub name: String,
    pub version: String,
    /// The serving route key (`name@major`).
    pub route: String,
    pub contract: Contract,
    /// What one row means (ADR 0012 §3) — required once `contract:
    /// supported` (the `verify` lint).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<String>,
    pub grain: Vec<String>,
    /// Declared column -> spec, in declared order. Always the object shape
    /// here — the string-or-mapping union is authoring ergonomics for
    /// `cell.yaml`; an emitted document has no reason to make a consumer
    /// handle two shapes.
    pub schema: IndexMap<String, ColumnDoc>,
    /// The served HTTP affordances, exactly (ADR 0012 §2). Omitted where the
    /// data routes are not mounted (`--no-data`) — it describes affordances
    /// that do not exist there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<QueryBlock>,
}

/// One declared column as the document emits it.
#[derive(Debug, Clone, Serialize)]
pub struct ColumnDoc {
    #[serde(rename = "type")]
    pub ty: String,
    /// Structured unit token (`USD`, `ms`) — never prose (ADR 0012 §3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl From<&ColumnSpec> for ColumnDoc {
    fn from(spec: &ColumnSpec) -> Self {
        ColumnDoc {
            ty: spec.ty.clone(),
            unit: spec.unit.clone(),
            description: spec.description.clone(),
        }
    }
}

/// The closed query grammar, restated as data. Every value here is derived
/// from the same constants and validation `serve` enforces (`build_query` /
/// `validate_params`) — a fixture test in `serve` binds the two so a change
/// to either fails loudly (ADR 0012 §7).
#[derive(Debug, Clone, Serialize)]
pub struct QueryBlock {
    /// The grain columns — the only filterable columns.
    pub filters: Vec<String>,
    pub filter_semantics: String,
    pub limit_default: usize,
    pub limit_max: usize,
    pub offset_max: usize,
    /// The smallest legal call, e.g. `/orders_daily@2?limit=10` — one
    /// grounded sentence in the grammar, honest wherever the data routes are
    /// mounted.
    pub sample_request: String,
}

/// Machine facts about what actually stands behind the document.
#[derive(Debug, Clone, Serialize)]
pub struct Observed {
    /// From the published run summary. `null` when none exists — a
    /// direct-attach cell writes no summary; that absence is served as-is,
    /// never as zeros (ADR 0012 §2).
    pub provenance: Option<Provenance>,
    /// Hosted `/context` only, published mode only: the poll telemetry that
    /// makes bounded staleness visible (ADR 0004 §6). Never in the portable
    /// artifact — poll telemetry is a lie the instant the file is written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<FreshnessBlock>,
    /// Swap-time probe results, per route (ADR 0012 §5) — measured against
    /// the rows the route actually serves (pinned snapshot for supported
    /// routes), never computed on the request path, omitted on failure.
    /// These turn the worst agent failure — an empty result read as a
    /// legitimate zero — into a diagnosable miss.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub exports: IndexMap<String, ExportProbe>,
    /// Docs page fingerprints (ADR 0013), `{target: {sha256, bytes}}` —
    /// computed at release time and carried through `published.json`, never
    /// at config-load time (which would force this non-empty on every
    /// unbuilt cell that merely declares `docs:`, breaking the "`observed`
    /// stays null on an unbuilt cell" invariant below). Author bytes never
    /// sit here — only their fingerprint.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub docs: IndexMap<String, DocsFingerprint>,
}

/// What the swap-time probe measured for one export.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ExportProbe {
    /// Total row count behind the route.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<i64>,
    /// min/max per date/timestamp-typed grain column — the aggregate that
    /// turns an empty answer into a diagnosable miss, and it names no entity.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub coverage: IndexMap<String, ColumnCoverage>,
    /// Distinct values per low-cardinality string grain column (`LIMIT 51`):
    /// ≤50 back ⇒ listed with `complete: true`; 51 back ⇒ values omitted,
    /// `complete: false`. Row-derived — omitted entirely under `--no-data`
    /// (shipping them would exfiltrate a projection of the withheld rows).
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub values: IndexMap<String, ColumnValues>,
    /// The grain-filtered sibling of `sample_request`, drawn jointly from
    /// ONE real row — never composed from the per-column values
    /// independently, which can name a combination that co-occurs nowhere
    /// (manufacturing the exact empty-result-as-zero failure the probe
    /// exists to kill). Emitted only when every grain column got a value;
    /// never a placeholder, which an agent pastes literally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example_request: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnCoverage {
    pub min: String,
    pub max: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnValues {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<String>>,
    pub complete: bool,
}

/// The provenance fields admitted to the wire (ADR 0012 §5). Everything else
/// in `RunSummary` — sources, connections, staged rows, transform filenames —
/// is the private/public seam and never crosses it, which is why this struct
/// is built field-by-field from the summary rather than embedding it.
#[derive(Debug, Clone, Serialize)]
pub struct Provenance {
    pub execution: u64,
    pub snapshot_id: Option<i64>,
    /// `"passed"` by construction when present — `run` publishes only after
    /// verify succeeds, so the honest wire meaning is "a verified build
    /// stands behind this document", never a tri-state (ADR 0012 §5).
    pub verify_outcome: String,
    pub started_at: String,
    pub finished_at: String,
    pub datamk_version: String,
    /// Newest lake snapshot time (`ducklake_snapshots('lake')`), cached at
    /// swap — when the data actually last moved. `None` when it couldn't be
    /// read cheaply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_as_of: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FreshnessBlock {
    pub serving_execution: u64,
    pub latest_seen: u64,
    pub last_successful_poll_age_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DataBlock {
    /// Derived from whether the data routes are mounted — honest by
    /// construction (ADR 0012 §2).
    pub served_here: bool,
    /// Where rows actually live when not served here. Environment: binds in
    /// the profile, never in `cell.yaml`; stays empty when the profile
    /// declares none — never fabricated.
    pub channels: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpstreamRef {
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
}

/// The one visibility-filtered route list every consumer reads (ADR 0012 §4):
/// the router's dispatch map, `openapi::generate`, and the `/context` builder
/// all derive from this — never three independent re-applications of the
/// predicate. Sorted by route key so every derived surface is deterministic.
pub fn discoverable_routes(def: &CellDef) -> Result<Vec<(String, Export)>> {
    let mut routes = Vec::new();
    for export in &def.interface {
        if export.visibility == Visibility::Discoverable {
            routes.push((export.route()?, export.clone()));
        }
    }
    routes.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(routes)
}

/// The declared region, built from the shared route list.
pub fn declared(def: &CellDef, routes: &[(String, Export)], with_query: bool) -> Declared {
    let exports = routes
        .iter()
        .map(|(route, e)| DeclaredExport {
            name: e.name.clone(),
            version: e.version.clone(),
            route: route.clone(),
            contract: e.contract,
            description: e.description.clone(),
            freshness: e.freshness.clone(),
            grain: e.grain.clone(),
            schema: e
                .schema
                .iter()
                .map(|(col, spec)| (col.clone(), ColumnDoc::from(spec)))
                .collect(),
            query: with_query.then(|| query_block(route, e)),
        })
        .collect();

    let mut upstreams: Vec<UpstreamRef> = def
        .sources
        .values()
        .filter_map(|s| match s {
            Source::Cell { cell, version, .. } => Some(UpstreamRef {
                reference: cell.clone(),
                version: *version,
            }),
            _ => None,
        })
        .collect();
    upstreams.sort_by(|a, b| (&a.reference, a.version).cmp(&(&b.reference, b.version)));
    upstreams.dedup_by(|a, b| a.reference == b.reference && a.version == b.version);

    Declared {
        description: def.description.clone(),
        exports,
        upstreams,
        docs: docs_entries(def, routes),
        include_request: INCLUDE_DOCS_REQUEST.to_string(),
    }
}

/// Docs identity only (ADR 0013): the cell-level page (if declared) plus
/// every **discoverable** export's page, in that order — no filesystem
/// access, since identity needs only the declared path and its extension. A
/// private export's docs entry never appears here, matching the same
/// visibility filter `routes` was already built with (ADR 0012 §4).
fn docs_entries(def: &CellDef, routes: &[(String, Export)]) -> Vec<DeclaredDocsEntry> {
    let mut entries = Vec::new();
    if let Some(path) = &def.docs {
        entries.push(DeclaredDocsEntry {
            target: "cell".to_string(),
            path: path.clone(),
            media_type: crate::config::docs::guess_media_type(path).to_string(),
        });
    }
    for (route, e) in routes {
        if let Some(path) = &e.docs {
            entries.push(DeclaredDocsEntry {
                target: route.clone(),
                path: path.clone(),
                media_type: crate::config::docs::guess_media_type(path).to_string(),
            });
        }
    }
    entries
}

/// The served affordances for one export — derived from the exact constants
/// `serve` enforces, so the claims cannot drift from the behavior.
pub fn query_block(route: &str, export: &Export) -> QueryBlock {
    QueryBlock {
        filters: export.grain.clone(),
        filter_semantics: "exact equality only — no ranges, no operators, no non-grain columns"
            .to_string(),
        limit_default: crate::serve::DEFAULT_LIMIT,
        limit_max: crate::serve::MAX_LIMIT,
        offset_max: crate::serve::MAX_OFFSET,
        sample_request: format!("/{route}?limit={SAMPLE_LIMIT}"),
    }
}

/// The engine-emitted note on a document with nothing built behind it
/// (ADR 0012 §2): absence alone reads to an agent as "couldn't compute",
/// not "never existed" — so the status is asserted positively.
pub const NOTE_NOTHING_BUILT: &str = "Nothing behind this document has been built or verified.";

/// The engine-emitted sentence a `--no-data` endpoint serves — in the
/// document's notes and in every unmounted data route's 404 body, the same
/// text (ADR 0012 §4).
pub const NOTE_NO_DATA: &str =
    "Rows are not served by this endpoint by design. Fetch them via the locations listed \
     in the context document's data.channels.";

/// The engine-emitted note on a direct-attach (local catalog) document:
/// pinless ⇒ draft by definition (ADR 0012 §4) — data may exist locally, but
/// no published, verify-gated execution stands behind the document.
pub const NOTE_DIRECT_ATTACH: &str =
    "This cell runs in direct-attach (local catalog) mode: no published execution or run \
     summary stands behind this document, so it is served as a draft. Publish with a \
     storage-backed profile for verified provenance.";

/// Assemble a document from a prebuilt declared region (how the serve
/// handler works — its declared region and digest are precomputed at
/// startup). `provenance` present ⇒ verified (its wire meaning — ADR 0012
/// §5); absent ⇒ draft, with the matching engine note.
#[allow(clippy::too_many_arguments)]
pub fn assemble(
    cell: String,
    declared: Declared,
    provenance: Option<Provenance>,
    freshness: Option<FreshnessBlock>,
    probes: IndexMap<String, ExportProbe>,
    docs_fingerprints: IndexMap<String, DocsFingerprint>,
    served_here: bool,
    channels: Vec<String>,
    direct_attach: bool,
) -> ContextDocument {
    let verified = provenance.is_some();
    let mut notes = Vec::new();
    if !verified {
        notes.push(
            if direct_attach {
                NOTE_DIRECT_ATTACH
            } else {
                NOTE_NOTHING_BUILT
            }
            .to_string(),
        );
    }
    ContextDocument {
        datamk_context: DATAMK_CONTEXT_VERSION,
        cell,
        status: if verified {
            Status::Verified
        } else {
            Status::Draft
        },
        grain_verified: verified,
        declared,
        observed: if provenance.is_some()
            || freshness.is_some()
            || !probes.is_empty()
            || !docs_fingerprints.is_empty()
        {
            Some(Observed {
                provenance,
                freshness,
                exports: probes,
                docs: docs_fingerprints,
            })
        } else {
            None
        },
        data: DataBlock {
            served_here,
            channels,
        },
        notes,
        // Request-specific (`?include=docs`) / flag-specific (`--no-docs`):
        // set by the caller after `assemble`/`build` returns, the same
        // post-build mutation pattern `emit` already uses for `emitted_at`.
        included: Vec::new(),
        docs: None,
        emitted_at: None,
        cell_yaml_digest: None,
    }
}

/// Build a document straight from a definition. `with_query` controls the
/// per-export `query` blocks (the served affordances); `served_here` is the
/// data block's honest fact about *this* surface — a portable emission keeps
/// the grammar (`with_query`) but never claims to serve rows itself.
#[allow(clippy::too_many_arguments)]
pub fn build(
    def: &CellDef,
    routes: &[(String, Export)],
    provenance: Option<Provenance>,
    freshness: Option<FreshnessBlock>,
    docs_fingerprints: IndexMap<String, DocsFingerprint>,
    with_query: bool,
    served_here: bool,
    direct_attach: bool,
) -> ContextDocument {
    assemble(
        def.cell.clone(),
        declared(def, routes, with_query),
        provenance,
        freshness,
        IndexMap::new(),
        docs_fingerprints,
        served_here,
        Vec::new(),
        direct_attach,
    )
}

/// The admitted projection of a run summary (ADR 0012 §5) — built
/// field-by-field so nothing beyond the allowlist can ride along.
pub fn provenance_from(summary: &RunSummary, data_as_of: Option<String>) -> Provenance {
    Provenance {
        execution: summary.execution,
        snapshot_id: summary.snapshot_id,
        verify_outcome: summary.verify_outcome.clone(),
        started_at: summary.started_at.clone(),
        finished_at: summary.finished_at.clone(),
        datamk_version: summary.datamk_version.clone(),
        data_as_of,
    }
}

/// The interface digest: the document's `ETag` and `/openapi.json`'s
/// `info.version` (ADR 0012 §2). Covers the interface regions — shape,
/// `declared` (including the query grammar), `data` — and never `observed`
/// telemetry or notes: the digest must change when the *interface* changes,
/// not when data refreshes under it (an execution number would churn agent
/// caches on every refresh without the interface moving).
pub fn interface_digest(cell: &str, declared: &Declared, data: &DataBlock) -> String {
    #[derive(Serialize)]
    struct InterfaceRegions<'a> {
        datamk_context: u32,
        cell: &'a str,
        declared: &'a Declared,
        data: &'a DataBlock,
    }
    let bytes = serde_json::to_vec(&InterfaceRegions {
        datamk_context: DATAMK_CONTEXT_VERSION,
        cell,
        declared,
        data,
    })
    .expect("context document serializes");
    hex(&Sha256::digest(&bytes))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `datamk context` (ADR 0012 §4): emit the document to stdout (`--out` to
/// write a file). No server, no port, no token — commit it, host it
/// statically, paste it into an agent's context. Loads the cell without a
/// database (`config::load`); published-mode profiles additionally fetch
/// `LATEST`'s run summary from the store (the same trust and credentials as
/// `datamk status`). Pinless ⇒ draft, by definition — a direct-attach
/// profile has no pin and emits a draft even after a local build.
///
/// `no_docs` (ADR 0013) is the one asymmetry with the served door: a request
/// can be repeated, a file cannot — so a portable artifact **inlines docs by
/// default**; `--no-docs` withholds content, emitting identity + fingerprints
/// only (mirroring `serve --no-data`'s withholding idiom). `included` is
/// truthful in both cases, so a consumer never needs to know which door
/// produced the file.
pub fn emit(
    file: &std::path::Path,
    profile: &str,
    out: Option<&std::path::Path>,
    no_docs: bool,
) -> Result<()> {
    use anyhow::Context as _;

    let loaded = crate::config::load(file, profile)?;
    let routes = discoverable_routes(&loaded.def)?;
    let direct_attach = loaded.bindings.catalog.is_some();

    let provenance = if direct_attach {
        None
    } else {
        let store = crate::store::Store::for_storage(
            &loaded.bindings.storage,
            loaded.bindings.s3.as_ref(),
            loaded.bindings.gcs.as_ref(),
        )?;
        match store.latest()? {
            None => None,
            Some(n) => store
                .get(&crate::store::run_summary_key(n))?
                .and_then(|bytes| serde_json::from_slice::<RunSummary>(&bytes).ok())
                .map(|summary| provenance_from(&summary, None)),
        }
    };

    // Docs fingerprints (ADR 0013 §5): a release-time fact, read from
    // `published.json` when one exists — never recomputed here (computing at
    // load/emit time would populate `observed.docs` on every never-run cell
    // that merely declares `docs:`, the exact invariant break §5 forbids).
    let docs_fingerprints: IndexMap<String, DocsFingerprint> =
        crate::manifest::Published::load(&loaded.dir)
            .map(|p| p.docs.into_iter().collect())
            .unwrap_or_default();

    let mut doc = build(
        &loaded.def,
        &routes,
        provenance,
        /* freshness */ None, // poll telemetry is a lie the instant the file is written
        docs_fingerprints,
        /* with_query */ true,
        /* served_here */ false, // a file serves no rows
        direct_attach,
    );
    doc.data.channels = loaded.bindings.channels.clone();

    // ADR 0013 §7: inline by default — a portable artifact with null content
    // pointing at a path the reader doesn't have is a dangling pointer.
    if !no_docs {
        let pages = crate::config::docs::load_declared(&loaded.dir, &loaded.def, &routes)?;
        doc.included = vec!["docs".to_string()];
        doc.docs = Some(
            pages
                .into_iter()
                .map(|p| {
                    (
                        p.target,
                        DocsContentEntry {
                            media_type: p.media_type,
                            content: p.content.to_string(),
                        },
                    )
                })
                .collect(),
        );
    }

    doc.emitted_at = Some(crate::timeutil::rfc3339_utc(crate::timeutil::unix_now()));
    doc.cell_yaml_digest = Some(sha256_hex(
        &std::fs::read(file).with_context(|| format!("reading {}", file.display()))?,
    ));

    let json = serde_json::to_string_pretty(&doc)?;
    match out {
        Some(path) => {
            std::fs::write(path, json.as_bytes())
                .with_context(|| format!("writing {}", path.display()))?;
            eprintln!("wrote {}", path.display());
        }
        None => println!("{json}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_def() -> CellDef {
        serde_yaml::from_str(
            r#"
cell: orders
description: Daily order revenue by region.
sources:
  raw: ./data/*.parquet
  upstream_flights:
    cell: flights
    table: fct_flights
    version: 7
interface:
  - name: orders_daily
    version: 2.1.0
    description: One row per (order_date, region) with the summed order revenue.
    grain: [order_date, region]
    schema:
      order_date: date
      region: string
      revenue:
        type: decimal
        unit: USD
        description: Gross order revenue, before refunds.
    freshness: daily
    contract: supported
  - name: internal
    version: 1.0.0
    visibility: private
"#,
        )
        .unwrap()
    }

    fn sample_verified() -> ContextDocument {
        let def = sample_def();
        let routes = discoverable_routes(&def).unwrap();
        build(
            &def,
            &routes,
            Some(Provenance {
                execution: 47,
                snapshot_id: Some(12),
                verify_outcome: "passed".to_string(),
                started_at: "2026-07-13T10:00:00Z".to_string(),
                finished_at: "2026-07-13T10:00:05Z".to_string(),
                datamk_version: "0.0.12".to_string(),
                data_as_of: Some("2026-07-13 10:00:04+00".to_string()),
            }),
            Some(FreshnessBlock {
                serving_execution: 47,
                latest_seen: 47,
                last_successful_poll_age_seconds: Some(3),
            }),
            IndexMap::new(),
            /* with_query */ true,
            /* served_here */ true,
            /* direct_attach */ false,
        )
    }

    /// The golden test (`RunSummary` precedent): the exact wire shape. A
    /// diff here is a contract change — additive is fine; a rename or
    /// re-meaning needs a `DATAMK_CONTEXT_VERSION` bump and a deprecation
    /// window (ADR 0012 §2).
    #[test]
    fn context_document_serializes_to_the_documented_shape() {
        let json = serde_json::to_string_pretty(&sample_verified()).unwrap();
        let expected = r#"{
  "datamk_context": 1,
  "cell": "orders",
  "status": "verified",
  "grain_verified": true,
  "declared": {
    "description": "Daily order revenue by region.",
    "exports": [
      {
        "name": "orders_daily",
        "version": "2.1.0",
        "route": "orders_daily@2",
        "contract": "supported",
        "description": "One row per (order_date, region) with the summed order revenue.",
        "freshness": "daily",
        "grain": [
          "order_date",
          "region"
        ],
        "schema": {
          "order_date": {
            "type": "date"
          },
          "region": {
            "type": "string"
          },
          "revenue": {
            "type": "decimal",
            "unit": "USD",
            "description": "Gross order revenue, before refunds."
          }
        },
        "query": {
          "filters": [
            "order_date",
            "region"
          ],
          "filter_semantics": "exact equality only — no ranges, no operators, no non-grain columns",
          "limit_default": 100,
          "limit_max": 1000,
          "offset_max": 1000000,
          "sample_request": "/orders_daily@2?limit=10"
        }
      }
    ],
    "upstreams": [
      {
        "ref": "flights",
        "version": 7
      }
    ],
    "docs": [],
    "include_request": "/context?include=docs"
  },
  "observed": {
    "provenance": {
      "execution": 47,
      "snapshot_id": 12,
      "verify_outcome": "passed",
      "started_at": "2026-07-13T10:00:00Z",
      "finished_at": "2026-07-13T10:00:05Z",
      "datamk_version": "0.0.12",
      "data_as_of": "2026-07-13 10:00:04+00"
    },
    "freshness": {
      "serving_execution": 47,
      "latest_seen": 47,
      "last_successful_poll_age_seconds": 3
    }
  },
  "data": {
    "served_here": true,
    "channels": []
  },
  "notes": [],
  "included": []
}"#;
        assert_eq!(json, expected);
    }

    /// ADR 0012 §2: an unbuilt cell asserts its status positively — draft,
    /// `observed: null` (never `{}`), `grain_verified: false`, and the
    /// engine-emitted note.
    #[test]
    fn draft_document_asserts_unbuilt_status_positively() {
        let def = sample_def();
        let routes = discoverable_routes(&def).unwrap();
        let doc = build(
            &def,
            &routes,
            None,
            None,
            IndexMap::new(),
            true,
            true,
            false,
        );
        let v: serde_json::Value = serde_json::to_value(&doc).unwrap();
        assert_eq!(v["status"], "draft");
        assert_eq!(v["grain_verified"], false);
        assert!(v["observed"].is_null(), "observed must be null, got {v}");
        assert_eq!(v["notes"][0], NOTE_NOTHING_BUILT);
        // ADR 0013: `included` is always present, `[]` when nothing was
        // requested/inlined — never absent, the old-binary/no-docs signal.
        assert_eq!(v["included"], serde_json::json!([]));
        assert!(
            v["docs"].is_null(),
            "docs must be absent when not requested"
        );
    }

    #[test]
    fn direct_attach_document_is_draft_with_the_direct_attach_note() {
        let def = sample_def();
        let routes = discoverable_routes(&def).unwrap();
        let doc = build(&def, &routes, None, None, IndexMap::new(), true, true, true);
        assert_eq!(doc.status, Status::Draft);
        assert_eq!(doc.notes, vec![NOTE_DIRECT_ATTACH.to_string()]);
    }

    /// ADR 0012 §4: a `private` export appears nowhere, in any form, not
    /// even as a name.
    #[test]
    fn private_exports_appear_nowhere_in_the_document() {
        let json = serde_json::to_string(&sample_verified()).unwrap();
        assert!(!json.contains("internal"), "leaked private export: {json}");
    }

    /// ADR 0012 §8: the serialization guard — a fully-populated document
    /// from a cell with raw/cell sources must carry nothing
    /// credentials- or URI-shaped. `Resolved*` types never derive
    /// `Serialize`; this guards the field names on top of that.
    #[test]
    fn context_document_never_carries_environment_shaped_content() {
        let json = serde_json::to_string(&sample_verified())
            .unwrap()
            .to_lowercase();
        for banned in [
            "s3://",
            "gs://",
            "postgres:",
            "credential",
            "secret",
            "password",
            "key_id",
            "account",
            "billing_project",
        ] {
            assert!(
                !json.contains(banned),
                "context document leaked `{banned}`-shaped content: {json}"
            );
        }
    }

    /// ADR 0012 §5: the upstream edge is nominal — `{ref, version}` only,
    /// never the upstream `table`.
    #[test]
    fn upstream_edges_never_carry_the_table() {
        let json = serde_json::to_string(&sample_verified()).unwrap();
        assert!(json.contains(r#""ref":"flights""#), "{json}");
        assert!(
            !json.contains("fct_flights"),
            "leaked upstream table: {json}"
        );
    }

    /// The digest covers the interface regions and never observed telemetry
    /// (ADR 0012 §2): a data refresh must not churn agent caches; an
    /// interface change must.
    fn digest_of(doc: &ContextDocument) -> String {
        interface_digest(&doc.cell, &doc.declared, &doc.data)
    }

    #[test]
    fn digest_ignores_observed_but_tracks_declared() {
        let def = sample_def();
        let routes = discoverable_routes(&def).unwrap();
        let verified = sample_verified();
        let draft = build(
            &def,
            &routes,
            None,
            None,
            IndexMap::new(),
            true,
            true,
            false,
        );
        assert_eq!(
            digest_of(&verified),
            digest_of(&draft),
            "observed/status/notes must not move the digest"
        );

        let mut def2 = sample_def();
        def2.interface[0].grain.push("channel".to_string());
        let routes2 = discoverable_routes(&def2).unwrap();
        let changed = build(
            &def2,
            &routes2,
            None,
            None,
            IndexMap::new(),
            true,
            true,
            false,
        );
        assert_ne!(
            digest_of(&draft),
            digest_of(&changed),
            "a declared-interface change must move the digest"
        );
    }

    /// ADR 0013: the digest tracks docs **identity** (adding, removing, or
    /// renaming a page moves it, since that's part of `Declared`) but never
    /// docs **content** or its fingerprint (neither lives inside `Declared`)
    /// — in the mold of `digest_ignores_observed_but_tracks_declared`.
    #[test]
    fn digest_tracks_docs_identity_but_ignores_content_and_fingerprint() {
        let mut def = sample_def();
        def.docs = Some("docs/overview.md".to_string());
        let routes = discoverable_routes(&def).unwrap();
        let base = build(
            &def,
            &routes,
            None,
            None,
            IndexMap::new(),
            true,
            true,
            false,
        );

        // Injecting top-level content (as `?include=docs` would) must not
        // move the digest — content lives outside `declared`.
        let mut with_content = base.clone();
        with_content.included = vec!["docs".to_string()];
        with_content.docs = Some(IndexMap::from([(
            "cell".to_string(),
            DocsContentEntry {
                media_type: "text/markdown; charset=utf-8".to_string(),
                content: "Some prose an agent will read.".to_string(),
            },
        )]));
        assert_eq!(
            digest_of(&base),
            digest_of(&with_content),
            "inlined docs content must not move the digest"
        );

        // Neither must a fingerprint (`observed.docs`) — a machine fact, not
        // part of the interface.
        let mut fp = IndexMap::new();
        fp.insert(
            "cell".to_string(),
            DocsFingerprint {
                sha256: "abc123".to_string(),
                bytes: 3,
            },
        );
        let with_fp = build(&def, &routes, None, None, fp, true, true, false);
        assert_eq!(
            digest_of(&base),
            digest_of(&with_fp),
            "a docs fingerprint must not move the digest"
        );

        // Renaming the declared page DOES move the digest.
        let mut def_renamed = def.clone();
        def_renamed.docs = Some("docs/other.md".to_string());
        let routes_renamed = discoverable_routes(&def_renamed).unwrap();
        let renamed = build(
            &def_renamed,
            &routes_renamed,
            None,
            None,
            IndexMap::new(),
            true,
            true,
            false,
        );
        assert_ne!(
            digest_of(&base),
            digest_of(&renamed),
            "renaming a declared docs page must move the digest"
        );

        // Removing it DOES move the digest.
        let mut def_removed = def.clone();
        def_removed.docs = None;
        let routes_removed = discoverable_routes(&def_removed).unwrap();
        let removed = build(
            &def_removed,
            &routes_removed,
            None,
            None,
            IndexMap::new(),
            true,
            true,
            false,
        );
        assert_ne!(
            digest_of(&base),
            digest_of(&removed),
            "removing a declared docs page must move the digest"
        );
    }

    /// The portable emission never claims to serve rows and keeps the
    /// grammar: `served_here: false`, `query` present.
    #[test]
    fn portable_shape_keeps_the_grammar_without_claiming_to_serve() {
        let def = sample_def();
        let routes = discoverable_routes(&def).unwrap();
        let mut doc = build(
            &def,
            &routes,
            None,
            None,
            IndexMap::new(),
            true,
            false,
            true,
        );
        doc.emitted_at = Some("2026-08-06T00:00:00Z".to_string());
        doc.cell_yaml_digest = Some("abc123".to_string());
        let v: serde_json::Value = serde_json::to_value(&doc).unwrap();
        assert_eq!(v["data"]["served_here"], false);
        assert!(v["declared"]["exports"][0]["query"].is_object());
        assert_eq!(v["emitted_at"], "2026-08-06T00:00:00Z");
        assert_eq!(v["cell_yaml_digest"], "abc123");
        // Poll telemetry never rides the portable artifact.
        assert!(v["observed"].is_null());
    }

    #[test]
    fn discoverable_routes_filters_private_and_sorts() {
        let def = sample_def();
        let routes = discoverable_routes(&def).unwrap();
        let keys: Vec<&str> = routes.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["orders_daily@2"]);
    }

    /// ADR 0013: `declared.docs` is identity only (target/path/media_type),
    /// always present (`[]` when none), and excludes a private export's page
    /// exactly like every other private-export fact (ADR 0012 §4).
    #[test]
    fn declared_docs_carries_identity_only_and_excludes_private_exports() {
        let mut def = sample_def();
        def.docs = Some("docs/overview.md".to_string());
        def.interface[0].docs = Some("docs/orders_daily.md".to_string());
        def.interface[1].docs = Some("docs/internal.md".to_string()); // private export
        let routes = discoverable_routes(&def).unwrap();
        let d = declared(&def, &routes, true);

        assert_eq!(d.include_request, "/context?include=docs");
        assert_eq!(d.docs.len(), 2, "{:?}", d.docs);
        assert_eq!(d.docs[0].target, "cell");
        assert_eq!(d.docs[0].path, "docs/overview.md");
        assert_eq!(d.docs[0].media_type, "text/markdown; charset=utf-8");
        assert_eq!(d.docs[1].target, "orders_daily@2");
        assert_eq!(d.docs[1].path, "docs/orders_daily.md");

        // Private export's docs page never appears, in any form.
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("internal.md"), "{json}");
    }

    #[test]
    fn declared_docs_is_empty_but_present_when_nothing_is_declared() {
        let def = sample_def();
        let routes = discoverable_routes(&def).unwrap();
        let d = declared(&def, &routes, true);
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["docs"], serde_json::json!([]));
        assert_eq!(v["include_request"], "/context?include=docs");
    }

    /// ADR 0013 §7: `datamk context` inlines docs by default — a request can
    /// be repeated, a file cannot — and `--no-docs` emits identity +
    /// fingerprints only. Exercises the real `emit` entry point end to end
    /// on a direct-attach profile (no DB, no network — `emit` never touches
    /// either in that mode), so this is the seam that actually proves the
    /// CLI flag reaches the file, not just the internal `build`/`assemble`
    /// plumbing.
    #[test]
    fn emit_inlines_docs_by_default_and_no_docs_withholds_content() {
        let dir = std::env::temp_dir().join(format!(
            "datamk-context-emit-docs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(
            dir.join("cell.yaml"),
            "cell: orders\n\
             description: Daily order revenue by region.\n\
             docs: docs/overview.md\n\
             interface:\n\
             \x20 - name: orders_daily\n\
             \x20   version: 1.0.0\n\
             \x20   description: One row per order.\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("docs/overview.md"),
            "# Orders\n\nWhat this cell is for, at length.",
        )
        .unwrap();
        std::fs::write(
            dir.join("profiles/local.yaml"),
            "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
        )
        .unwrap();

        let default_out = dir.join("context.json");
        emit(&dir.join("cell.yaml"), "local", Some(&default_out), false).unwrap();
        let default_doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&default_out).unwrap()).unwrap();
        assert_eq!(default_doc["included"], serde_json::json!(["docs"]));
        assert!(
            default_doc["docs"]["cell"]["content"]
                .as_str()
                .unwrap()
                .contains("What this cell is for"),
            "{default_doc}"
        );

        let no_docs_out = dir.join("context_no_docs.json");
        emit(&dir.join("cell.yaml"), "local", Some(&no_docs_out), true).unwrap();
        let no_docs_doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&no_docs_out).unwrap()).unwrap();
        assert_eq!(no_docs_doc["included"], serde_json::json!([]));
        assert!(no_docs_doc["docs"].is_null(), "{no_docs_doc}");
        // Identity still ships either way — `--no-docs` withholds content,
        // not the fact that a page exists.
        assert_eq!(
            no_docs_doc["declared"]["docs"][0]["path"],
            "docs/overview.md"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
