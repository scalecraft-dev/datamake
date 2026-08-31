//! The cell context document (ADR 0012, reshaped by ADR 0015): the cell's
//! interface made machine-readable — what `/openapi.json` is to an API, this
//! is to a data product. One artifact, two doors: `GET /context` on the
//! serving plane and `datamk context` on stdout. The document is a
//! *projection* of the cell, never a separate product: write the cell and the
//! context exists; build the cell and it becomes trustworthy.
//!
//! The shape rule (ADR 0015 §3): the document is **flat** — one level, no
//! `declared`/`observed` regions — and every fact says where it came from.
//! A fact is a *claim* iff it carries `from` (a per-field origin map on the
//! record: `cell.yaml`, `warehouse`, a modeling tool); a fact is a
//! *measurement* iff it sits in a block with a timestamp (`build`,
//! `source_check`, `freshness`, an export's `probe`/`check`). Nothing is
//! both, nothing is neither. Absent facts are omitted, never fabricated.

use anyhow::Result;
use indexmap::IndexMap;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::{CellDef, ColumnSpec, Contract, Export, Source, Visibility};
pub use crate::config::{FromMap, Origin};
use crate::engine::run_summary::RunSummary;

/// The document-schema version (`datamk_context`). An integer, distinct from
/// cell semver and `datamk_version`: additive changes don't bump; any removal,
/// rename, or re-meaning bumps (ADR 0012 §2).
///
/// **2**: `declared.docs[].path` renamed to `source_path`.
/// **3**: every emitted request affordance (`include_request`,
/// `exports[].query.sample_request`, `exports[].probe.example_request`) is
/// **relative to the document's own URL** — `orders_daily@2?limit=10`, not
/// `/orders_daily@2?limit=10`. A re-meaning, so it bumps. See
/// `INCLUDE_DOCS_REQUEST` for why.
/// **4**: flat (ADR 0015). The `declared`/`observed` regions are gone;
/// every field that lived under them is top-level or on the record it
/// describes (`exports[].probe`, `exports[].check`, `upstreams[].execution`,
/// `docs[].sha256`), claims carry a `from` origin map, and measurements
/// carry a timestamp. `observed.provenance` is `build`;
/// `observed.source_descriptions` is gone — warehouse prose lands on a
/// bound export's own columns with `from.description: "warehouse"`.
pub const DATAMK_CONTEXT_VERSION: u32 = 4;

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
    /// `draft` | `verified_at_source` | `verified`, weakest to strongest
    /// (issue #6). `verified` means exactly one thing: real provenance (a
    /// published, verify-gated execution) stands behind this document.
    /// Pinless ⇒ draft, by definition (ADR 0012 §4) — a direct-attach cell has
    /// no pin and no run summary, so it is draft even after a local build; the
    /// engine-emitted note says why. `verified_at_source` sits strictly
    /// between the two: no execution, but a `datamk verify` live-checked
    /// every bound export against its declared source (`source_check`) — a
    /// claim about rows as of that check, not about immutable rows that
    /// still exist. Distinct from `verified` on purpose (issue #6 Q3): an
    /// agent that branches on `status` must opt in to trusting a weaker
    /// guarantee, never inherit it silently.
    pub status: Status,
    /// `false` unless a verified build or a live source check stands behind
    /// the document — never `null`, never `true` by assumption (ADR 0012 §2).
    pub grain_verified: bool,
    /// The cell's one-line description (ADR 0012 §3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Origin of each cell-level claim above (ADR 0015 §2).
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub from: FromMap,
    /// ADR 0016 §7: present iff the interface was discovered from a modeling
    /// tool — which tool, which environment, which plan, and how the
    /// "deployed" claim is evidenced. `synced_at` makes it a measurement;
    /// it is outside the digest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovered_from: Option<crate::config::DiscoveredFrom>,
    /// The visibility-filtered exports (a `private` export appears nowhere,
    /// in any form, not even as a name — ADR 0012 §4), each carrying its own
    /// claims (`from`) and its own measurements (`probe`, `check`).
    pub exports: Vec<ExportDoc>,
    /// One-hop upstream edges: the author's `{ref, version}` pin plus, when a
    /// build has been observed, what was actually attached (issue #7). Never
    /// the upstream `table` (the upstream owner's to disclose — ADR 0012 §5).
    pub upstreams: Vec<UpstreamDoc>,
    /// The glossary index (ADR 0017 §2) — always present, never gated
    /// behind `include=`. Narrowed by `?terms=` (`narrow_terms`) and by
    /// `/context/<route>` (`narrow_to`).
    pub definitions: Vec<DefinitionDoc>,
    /// `?terms=` tokens that resolved to no term or alias, echoed verbatim
    /// — engine-emitted, always present (`[]` when nothing was asked or
    /// everything resolved). An unknown term is not an error (ADR 0017 §3).
    pub missing_terms: Vec<String>,
    /// The affordance to fetch a subset of the glossary — a constant,
    /// always present, beside `include_request`.
    pub definitions_request: String,
    /// Docs pages (ADR 0013): identity always; fingerprint once released;
    /// `content` only when `included` names `docs`.
    pub docs: Vec<DocsDoc>,
    /// The affordance to fetch docs content — a constant, always present.
    pub include_request: String,
    /// Build provenance from the published run summary (ADR 0012 §5).
    /// Absent when no published execution stands behind the document — a
    /// direct-attach cell writes no summary; that absence is served as-is,
    /// never as zeros (ADR 0012 §2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<Provenance>,
    /// From `datamk verify`'s live-verify pass (issue #6): present iff a
    /// fresh (digest-matched) `.cell/source_check.json` record exists. Absent
    /// when no live check has run, or its record is stale (a `cell.yaml` edit
    /// since the check — silently omitted, never emitted stale). The
    /// per-export measurements ride each export's `check`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_check: Option<SourceCheck>,
    /// Hosted `/context` only, published mode only: the poll telemetry that
    /// makes bounded staleness visible (ADR 0004 §6). Never in the portable
    /// artifact — poll telemetry is a lie the instant the file is written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<FreshnessBlock>,
    pub data: DataBlock,
    /// Engine-emitted only — no author-supplied string ever lands here.
    pub notes: Vec<String>,
    /// Which optional sections this response inlines (ADR 0013) —
    /// engine-emitted, always present: `[]` on the default variant, `["docs"]`
    /// under `?include=docs` (served) or the default portable emission. Lets
    /// an agent distinguish "server predates this field" (absent — an old
    /// binary) from "this cell has no docs" (present, and `docs` is `[]`).
    pub included: Vec<String>,
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
    /// Issue #6: `#[serde(rename)]` because `rename_all = "lowercase"` alone
    /// would emit `verifiedatsource` (it lowercases, it does not
    /// snake_case).
    #[serde(rename = "verified_at_source")]
    VerifiedAtSource,
    Verified,
}

/// `context?include=docs` — the one and only door to docs content (ADR
/// 0012 §4: one document, one route; there is no `/docs/:name`).
///
/// **Relative, not root-absolute** (ADR 0014, `datamk_context: 3`). This
/// string is part of the interface digest, so a root-absolute affordance has
/// no safe form once a cell can be mounted at a base path: `/context` 404s
/// in a multi-cell server, and prefixing it with the mount makes the same
/// `cell.yaml` yield a different digest depending on where it's served —
/// environment leaking into the contract. A relative reference resolves per
/// RFC 3986 §5 against the document's own URL and is correct unmounted or
/// mounted with one string; the digest never sees the base path.
const INCLUDE_DOCS_REQUEST: &str = "context?include=docs";

/// `context?terms=<term>[,<term>]&include=docs` — the affordance naming how
/// to ask for a subset of the glossary (ADR 0017 §2, §3). A constant, like
/// `INCLUDE_DOCS_REQUEST` — not templated per-cell; the placeholder is for
/// the reader, not resolved server-side.
const DEFINITIONS_REQUEST: &str = "context?terms=<term>[,<term>]&include=docs";

/// One glossary term (ADR 0017 §2) — the index an agent reads before it
/// asks. Always present in full on the default document (never gated
/// behind `include=`): an agent recovering from a `missing_terms` miss
/// needs the vocabulary in the fetch it already made. `description` ships
/// here (the term's short form, `export.description`'s analogue);
/// `docs[]` under `target: "definition:<term>"` carries the long form.
#[derive(Debug, Clone, Serialize)]
pub struct DefinitionDoc {
    pub term: String,
    /// Always present (`[]` when none) — an agent iterating an alias list
    /// must never meet `null`, and absence is reserved for "server
    /// predates the field".
    pub aliases: Vec<String>,
    pub description: String,
    /// `name@major` or `name@major.column`; empty (always present) means
    /// cell-wide.
    pub applies_to: Vec<String>,
    /// Definitions are authored-only today (`{description: "cell.yaml"}`) —
    /// the field exists so a future adapter supplying terms is additive,
    /// not a re-meaning (ADR 0017 §2).
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub from: FromMap,
}

/// One docs page (ADR 0013): identity (`target`, `source_path`,
/// `media_type`) is interface and always present; `sha256`/`bytes` are a
/// release-time fingerprint carried through `published.json`, never
/// computed at load time (which would populate it on every unbuilt cell
/// that merely declares `docs:`); `content` is inlined only when `included`
/// names `docs`. Only identity is in the interface digest — a prose edit
/// must not tell generic tooling the callable surface changed.
#[derive(Debug, Clone, Serialize)]
pub struct DocsDoc {
    /// `"cell"` or the route key (`name@major`) — route keys always carry
    /// `@major`, so an export can never collide with the literal `"cell"`.
    pub target: String,
    /// The author's cell.yaml-relative filesystem path. Named `source_path`,
    /// not `path`, because it is not fetchable — there is no `/docs/:target`
    /// route (ADR 0012 §4); content arrives via `?include=docs`.
    pub source_path: String,
    pub media_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// One docs page's content fingerprint (ADR 0013) — a machine fact computed
/// at release time and carried through `published.json`, never author
/// bytes. Lands on `docs[].{sha256, bytes}`.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct DocsFingerprint {
    pub sha256: String,
    pub bytes: usize,
}

/// One export as the document emits it: interface fields first (in the
/// digest), then the per-export measurements (`probe`, `check` — timestamped,
/// never in the digest).
#[derive(Debug, Clone, Serialize)]
pub struct ExportDoc {
    pub name: String,
    pub version: String,
    /// The route key (`name@major`) — this export's stable identity, and
    /// its docs `target` (ADR 0013 §5), for every export regardless of
    /// servability. It is *also* the HTTP path (`GET /{route}`) only for a
    /// materialized export; a bound export (`Export::bind`, issue #6) has
    /// none — `/openapi.json`'s paths already exclude it, and `query` below
    /// is `null` for exactly the same exports, by construction
    /// (`(!e.is_bound()).then(...)`), so `query` is the machine-checkable
    /// signal for "does `GET /{route}` exist," never `route`'s mere
    /// presence.
    pub route: String,
    pub contract: Contract,
    /// What one row means (ADR 0012 §3) — required once `contract:
    /// supported` (the `verify` lint).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<String>,
    pub grain: Vec<String>,
    /// Origin of `description` and `grain` (ADR 0015 §2), present for each
    /// that is present.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub from: FromMap,
    /// Column -> spec, in declared order. Always the object shape here — the
    /// string-or-mapping union is authoring ergonomics for `cell.yaml`; an
    /// emitted document has no reason to make a consumer handle two shapes.
    pub schema: IndexMap<String, ColumnDoc>,
    /// The served HTTP affordances, exactly (ADR 0012 §2). `null` for a
    /// bound export: the affordance it would describe (a mounted HTTP route)
    /// does not exist for that export, ever, regardless of any serving flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<QueryBlock>,
    /// Where the rows are, for a bound export — present iff `query` is null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding: Option<BindingBlock>,
    /// ADR 0016 §7: lineage inside this cell — route keys of the selected
    /// parents. Not `upstreams[]`, which means a *cell* dependency.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// How many parents selection left out — a count, never their names
    /// (the unselected models are not this cell's to disclose, ADR 0012 §5).
    #[serde(skip_serializing_if = "is_zero")]
    pub depends_on_unselected: usize,
    /// ADR 0016 §7: what the modeling tool says about this model's
    /// deployment — kind, cron, owner, tags, fingerprint, loaded intervals.
    /// A measured block (`at` = the sync time), outside the digest, so an
    /// upstream tag or cron edit never moves the ETag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployed: Option<DeployedBlock>,
    /// Swap-time probe (ADR 0012 §5) — measured against the rows the route
    /// actually serves (pinned snapshot for supported routes), never on the
    /// request path, omitted on failure. Turns the worst agent failure — an
    /// empty result read as a legitimate zero — into a diagnosable miss.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe: Option<ExportProbe>,
    /// What `datamk verify`'s live check measured for this export — the
    /// numbers behind `source_check.outcome`, so a reader sees what passed
    /// and not only that it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<ExportCheck>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// The tool-side facts of a discovered export (ADR 0016 §7), on the wire.
#[derive(Debug, Clone, Serialize)]
pub struct DeployedBlock {
    pub at: String,
    pub model: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intervals: Option<crate::catalog::ir::Interval>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_restatement: Option<bool>,
}

impl From<&crate::config::DiscoveredExport> for DeployedBlock {
    fn from(d: &crate::config::DiscoveredExport) -> Self {
        DeployedBlock {
            at: d.at.clone(),
            model: d.model.clone(),
            kind: d.kind.clone(),
            cron: d.cron.clone(),
            owner: d.owner.clone(),
            tags: d.tags.clone(),
            fingerprint: d.fingerprint.clone(),
            version: d.version.clone(),
            intervals: d.intervals.clone(),
            pending_restatement: d.pending_restatement,
        }
    }
}

/// A bound export's target, exactly as `cell.yaml` writes it — never
/// profile-resolved: `table` is env-expandable and this block is in the
/// interface digest, so a resolved value would churn the digest per
/// environment. A templated table ships as `${DATASET}.fct_x`.
#[derive(Debug, Clone, Serialize)]
pub struct BindingBlock {
    /// The `sources:` key this export binds to.
    pub source: String,
    /// The declared object: a warehouse table path, or a raw file/glob.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    /// The connection alias only — what it resolves to is profile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
}

/// One column as the document emits it. `from` names the origin of every
/// field present (`type` always; `unit`/`description` when set) — ADR 0015
/// §2.
#[derive(Debug, Clone, Serialize)]
pub struct ColumnDoc {
    #[serde(rename = "type")]
    pub ty: String,
    /// Structured unit token (`USD`, `ms`) — never prose (ADR 0012 §3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub from: FromMap,
}

impl From<&ColumnSpec> for ColumnDoc {
    /// Every present field carries its origin: `cell.yaml` unless the spec
    /// says otherwise (a discovered column, ADR 0016).
    fn from(spec: &ColumnSpec) -> Self {
        let origin = |field: &str| spec.from.get(field).copied().unwrap_or(Origin::CellYaml);
        let mut from = FromMap::new();
        from.insert("type".to_string(), origin("type"));
        if spec.unit.is_some() {
            from.insert("unit".to_string(), origin("unit"));
        }
        if spec.description.is_some() {
            from.insert("description".to_string(), origin("description"));
        }
        ColumnDoc {
            ty: spec.ty.clone(),
            unit: spec.unit.clone(),
            description: spec.description.clone(),
            from,
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
    /// The smallest legal call, e.g. `orders_daily@2?limit=10` — one
    /// grounded sentence in the grammar. Relative to the document's own URL,
    /// never root-absolute — see `INCLUDE_DOCS_REQUEST`.
    pub sample_request: String,
}

/// One upstream edge: the author's pin (`ref`, `version` — interface, in the
/// digest) and, once a build has been observed, what the Builder actually
/// attached (`execution`, `data_as_of` — measurements, never in the digest:
/// an execution number there would churn agent caches on every refresh
/// without the interface moving). `execution` is absent for a direct-attach
/// cell source (no execution number exists — ADR 0004 §12) or when nothing
/// has been observed; never fabricated.
#[derive(Debug, Clone, Serialize)]
pub struct UpstreamDoc {
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_as_of: Option<String>,
}

/// What was actually attached for one `cell` source (issue #7), as read off
/// a run summary — merged onto the matching `UpstreamDoc` by `assemble`.
#[derive(Debug, Clone, Serialize)]
pub struct ObservedUpstream {
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_as_of: Option<String>,
}

/// What the swap-time probe measured for one export. `at` is when — the
/// timestamp that makes this a measurement (ADR 0015 §3).
#[derive(Debug, Clone, Serialize)]
pub struct ExportProbe {
    pub at: String,
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
    /// The grain-filtered sibling of `sample_request` — relative to the
    /// document's own URL for the same reason, drawn jointly from ONE real
    /// row, never composed from the per-column values independently (which
    /// can name a combination that co-occurs nowhere, manufacturing the
    /// exact empty-result-as-zero failure the probe exists to kill).
    /// Emitted only when every grain column got a value; never a
    /// placeholder, which an agent pastes literally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example_request: Option<String>,
}

impl ExportProbe {
    /// An empty probe taken at `at` — every measurement still to be filled.
    pub fn at(at: String) -> Self {
        ExportProbe {
            at,
            rows: None,
            coverage: IndexMap::new(),
            values: IndexMap::new(),
            example_request: None,
        }
    }
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

/// The provenance fields admitted to the wire (ADR 0012 §5), as the `build`
/// block. Everything else in `RunSummary` — sources, connections, staged
/// rows, transform filenames — is the private/public seam and never crosses
/// it, which is why this struct is built field-by-field from the summary
/// rather than embedding it.
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

/// A live check of the bound exports against their declared sources (issue
/// #6, live-verify core) — the fact behind `status: verified_at_source`.
/// Deliberately not `Provenance`: there is no execution, no snapshot, no
/// immutable rows behind this — just a machine check that passed as of
/// `checked_at`. Built from `crate::manifest::SourceCheckRecord` (the
/// `.cell/source_check.json` file `datamk verify` writes); the wire shape
/// mirrors the record's admitted fields exactly, field-by-field the same
/// discipline `Provenance` uses for `RunSummary`. The per-export
/// measurements are carried here for `assemble` to place on each export's
/// `check` — they are not serialized as part of this block.
#[derive(Debug, Clone, Serialize)]
pub struct SourceCheck {
    /// `"passed"` by construction — see `SourceCheckRecord`'s doc comment.
    pub outcome: String,
    pub checked_at: String,
    /// Only when a connector can supply it cheaply and truthfully — omitted,
    /// never fabricated, never defaulted to `checked_at` (ADR 0012 §2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_as_of: Option<String>,
    pub datamk_version: String,
    /// Route -> measurement; placed on `exports[].check` by `assemble`.
    #[serde(skip_serializing)]
    pub exports: IndexMap<String, ExportCheck>,
}

/// What one live check measured for one export. `at` is the check's
/// `checked_at` — one pass, one time.
#[derive(Debug, Clone, Serialize)]
pub struct ExportCheck {
    pub at: String,
    pub check: String,
    pub grain: Vec<String>,
    pub rows: i64,
    pub distinct_grain: i64,
}

impl SourceCheck {
    /// Visibility-filtered (ADR 0012 §4): a private export's measurement never
    /// reaches the wire, so this takes the same route list every other
    /// consumer reads rather than copying the record's map wholesale.
    pub fn from_record(
        r: &crate::manifest::SourceCheckRecord,
        routes: &[(String, Export)],
    ) -> Self {
        let exports = routes
            .iter()
            .filter_map(|(route, _)| {
                r.exports.get(route).map(|m| {
                    (
                        route.clone(),
                        ExportCheck {
                            at: r.checked_at.clone(),
                            check: m.check.clone(),
                            grain: m.grain.clone(),
                            rows: m.rows,
                            distinct_grain: m.distinct_grain,
                        },
                    )
                })
            })
            .collect();
        SourceCheck {
            outcome: r.outcome.clone(),
            checked_at: r.checked_at.clone(),
            data_as_of: r.data_as_of.clone(),
            datamk_version: r.datamk_version.clone(),
            exports,
        }
    }
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

/// The interface as a whole — every claim, no measurements: what the digest
/// covers, and what `serve` precomputes once at startup (the interface never
/// changes for the lifetime of the process). `assemble` lays measurements
/// onto a clone of this to produce a document.
#[derive(Debug, Clone)]
pub struct Interface {
    pub description: Option<String>,
    pub from: FromMap,
    pub discovered_from: Option<crate::config::DiscoveredFrom>,
    /// Any `probe`/`check` present here is ignored by the digest.
    pub exports: Vec<ExportDoc>,
    /// Any `execution`/`data_as_of` present here is ignored by the digest.
    pub upstreams: Vec<UpstreamDoc>,
    /// The whole-cell glossary (ADR 0017 §2), never narrowed — the source
    /// `ContextDocument::narrow_terms` resolves `?terms=` against
    /// regardless of any `/context/<route>` narrowing already applied.
    pub definitions: Vec<DefinitionDoc>,
    pub definitions_request: String,
    /// Any `sha256`/`bytes`/`content` present here is ignored by the digest.
    pub docs: Vec<DocsDoc>,
    pub include_request: String,
}

/// The one visibility-filtered route list every consumer reads (ADR 0012 §4):
/// the router's dispatch map, `openapi::generate`, and the `/context` builder
/// all derive from this — never three independent re-applications of the
/// predicate. Sorted by route key so every derived surface is deterministic.
///
/// **Declared, not mounted** (issue #6): every discoverable export, including
/// a bound one — datamk owns its contract even where it doesn't own the
/// rows, so a virtual export is still named,
/// typed, and versioned here. Callers that need only the subset `serve`
/// actually routes over HTTP call `mounted_routes` on this list.
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

/// The snapshot-backed subset of `routes` (issue #6, binding model): drops
/// every bound export (`Export::bind` — no transform, no lake table). This
/// is the list `serve` mounts data routes from, `openapi::generate` builds
/// paths from, and swap-time probes run against — every one of those is a
/// claim (or a query) about rows that exist in the lake, which a bound
/// export's declared object never is (it lives in the warehouse, or wherever
/// `sources:` points).
pub fn mounted_routes(routes: &[(String, Export)]) -> Vec<(String, Export)> {
    routes
        .iter()
        .filter(|(_, e)| !e.is_bound())
        .cloned()
        .collect()
}

/// `data.served_here` (ADR 0012 §4): honest by construction under **both**
/// reasons a data route can be absent — the `--no-data` flag (`data_mounted`)
/// and a cell with nothing in `mounted` to route (every export bound, issue
/// #6). The one place both `serve` call sites (the digest-feeding
/// `DataBlock` and the per-request document) decide this, so the served
/// rows, the digest/ETag, and the claim in the document can never
/// independently disagree with each other.
pub fn served_here(data_mounted: bool, mounted: &[(String, Export)]) -> bool {
    data_mounted && !mounted.is_empty()
}

/// The interface, built from the shared route list. `query` is unconditional
/// interface grammar (ADR 0012 §2, amended) — present for every export that
/// isn't bound. A bound export's `query` block is null: the affordance it
/// would describe (a mounted HTTP route) does not exist for that export,
/// ever, regardless of any serving flag — a genuine interface fact (issue
/// #6), not a serving-mode one, which is why it's the only thing that still
/// gates this field.
///
/// `source_descriptions` (issue #6/#10, `.cell/source_descriptions.json`:
/// source name -> column -> the warehouse's own column comment) lands on a
/// **bound** export's columns that carry no authored description, with
/// `from.description: "warehouse"` (ADR 0015 §4) — a bound export's columns
/// *are* its source's columns. A materialized export's columns are the
/// output of transforms, so no source column is an authority on them and
/// nothing is merged there. Precedence is `cell.yaml` > `warehouse`: an
/// author who writes a description means something different from the
/// upstream's words (`interface import`'s rule, `src/interface.rs`).
pub fn interface(
    def: &CellDef,
    routes: &[(String, Export)],
    source_descriptions: &IndexMap<String, IndexMap<String, String>>,
) -> Interface {
    let exports = routes
        .iter()
        .map(|(route, e)| {
            let mut schema: IndexMap<String, ColumnDoc> = e
                .schema
                .iter()
                .map(|(col, spec)| (col.clone(), ColumnDoc::from(spec)))
                .collect();
            if let Some(bind) = e.bind.as_deref() {
                if let Some(cols) = source_descriptions.get(bind) {
                    for (col, doc) in schema.iter_mut() {
                        if doc.description.is_none() {
                            if let Some(text) = cols.get(col).filter(|t| !t.trim().is_empty()) {
                                doc.description = Some(text.clone());
                                doc.from
                                    .insert("description".to_string(), Origin::Warehouse);
                            }
                        }
                    }
                }
            }
            let origin = |field: &str| e.from.get(field).copied().unwrap_or(Origin::CellYaml);
            let mut from = FromMap::new();
            if e.description.is_some() {
                from.insert("description".to_string(), origin("description"));
            }
            if !e.grain.is_empty() {
                from.insert("grain".to_string(), origin("grain"));
            }
            ExportDoc {
                name: e.name.clone(),
                version: e.version.clone(),
                route: route.clone(),
                contract: e.contract,
                description: e.description.clone(),
                freshness: e.freshness.clone(),
                grain: e.grain.clone(),
                from,
                schema,
                query: (!e.is_bound()).then(|| query_block(route, e)),
                binding: e.bind.as_deref().map(|b| binding_block(def, b)),
                depends_on: e
                    .discovered
                    .as_ref()
                    .map(|d| d.depends_on.clone())
                    .unwrap_or_default(),
                depends_on_unselected: e
                    .discovered
                    .as_ref()
                    .map(|d| d.depends_on_unselected)
                    .unwrap_or(0),
                deployed: e.discovered.as_ref().map(DeployedBlock::from),
                probe: None,
                check: None,
            }
        })
        .collect();

    let mut upstreams: Vec<UpstreamDoc> = def
        .sources
        .values()
        .filter_map(|s| match s {
            Source::Cell { cell, version, .. } => Some(UpstreamDoc {
                reference: cell.clone(),
                version: *version,
                execution: None,
                data_as_of: None,
            }),
            _ => None,
        })
        .collect();
    upstreams.sort_by(|a, b| (&a.reference, a.version).cmp(&(&b.reference, b.version)));
    upstreams.dedup_by(|a, b| a.reference == b.reference && a.version == b.version);

    let mut from = FromMap::new();
    if def.description.is_some() {
        from.insert("description".to_string(), Origin::CellYaml);
    }

    Interface {
        description: def.description.clone(),
        from,
        discovered_from: def.discovered_from.clone(),
        exports,
        upstreams,
        definitions: def.definitions.iter().map(definition_doc).collect(),
        definitions_request: DEFINITIONS_REQUEST.to_string(),
        docs: docs_entries(def, routes),
        include_request: INCLUDE_DOCS_REQUEST.to_string(),
    }
}

/// ADR 0017 §2: definitions are authored-only today — `from.description`
/// is always `cell.yaml`.
fn definition_doc(d: &crate::config::Definition) -> DefinitionDoc {
    let mut from = FromMap::new();
    from.insert("description".to_string(), Origin::CellYaml);
    DefinitionDoc {
        term: d.term.clone(),
        aliases: d.aliases.clone(),
        description: d.description.clone(),
        applies_to: d.applies_to.clone(),
        from,
    }
}

/// A bound export's target, looked up in `sources:`. A `cell:` source is
/// rejected at resolve time (`verify::validate_bound_exports`) and an unknown
/// name can't resolve, so both emit the source name alone rather than a
/// fabricated object.
fn binding_block(def: &CellDef, bind: &str) -> BindingBlock {
    let (object, connection) = match def.sources.get(bind) {
        Some(Source::Raw(path)) => (Some(path.clone()), None),
        Some(Source::Connection {
            connection, table, ..
        }) => (table.clone(), Some(connection.clone())),
        Some(Source::Cell { .. }) | None => (None, None),
    };
    BindingBlock {
        source: bind.to_string(),
        object,
        connection,
    }
}

/// Docs identity only (ADR 0013, ADR 0017 §2): the cell-level page (if
/// declared), every **discoverable** export's page, then every
/// definition's page (`target: "definition:<term>"` — the third target
/// form) — no filesystem access, since identity needs only the declared
/// path and its extension. A private export's docs entry never appears
/// here, matching the same visibility filter `routes` was already built
/// with (ADR 0012 §4). Definitions are cell-wide, not visibility-filtered.
fn docs_entries(def: &CellDef, routes: &[(String, Export)]) -> Vec<DocsDoc> {
    let entry = |target: String, path: &str| DocsDoc {
        target,
        source_path: path.to_string(),
        media_type: crate::config::docs::guess_media_type(path).to_string(),
        sha256: None,
        bytes: None,
        content: None,
    };
    let mut entries = Vec::new();
    if let Some(path) = &def.docs {
        entries.push(entry("cell".to_string(), path));
    }
    for (route, e) in routes {
        if let Some(path) = &e.docs {
            entries.push(entry(route.clone(), path));
        }
    }
    for d in &def.definitions {
        if let Some(path) = &d.docs {
            entries.push(entry(format!("definition:{}", d.term), path));
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
        sample_request: format!("{route}?limit={SAMPLE_LIMIT}"),
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

/// The engine-emitted body a never-backed export's data route serves
/// (issue #6) — same shape as `NOTE_NO_DATA` (a whole cell dark under
/// `--no-data`), but naming the export: this door is dark by declared
/// design, not by an operator flag, and the caller needs to know which.
/// Served post-`authorize()` only (`serve::serve_export_inner`) — a
/// pre-auth 404 here would let an unauthenticated caller enumerate which
/// exports are virtual just by probing routes.
pub fn note_bound_export(route: &str) -> String {
    format!(
        "Rows for '{route}' are not served by this endpoint by design: this export is `bind`ed \
         to an existing object, not materialized — datamk owns its contract, not its rows. \
         Fetch them via the locations listed in the context document's data.channels. See GET \
         /context."
    )
}

/// The engine-emitted note on a document where every discoverable export is
/// bound (issue #6, issue #17): `mounted` is empty regardless of
/// `--no-data`, so no data route exists to route rows over even when the
/// operator never passed the flag. Deliberately distinct from
/// `NOTE_NO_DATA` — this names the cell's own definition as the reason, not
/// an operator flag, since nothing about a deploy argument changes it. The
/// two are mutually exclusive by construction: `served_here` is `false`
/// either because `!data_mounted` (`NOTE_NO_DATA`) or because `mounted` is
/// empty while `data_mounted` is `true` (this note) — never both at once.
///
/// M2: the caller (`serve::context_doc`) additionally gates this on at
/// least one *discoverable* export existing at all — `mounted.is_empty()`
/// implies "every discoverable export is bound" only when there was at
/// least one discoverable export to begin with; a cell with zero (every
/// export `visibility: private`) also reaches `mounted.is_empty()`, and
/// this exact sentence would misdescribe it as bound when none of its
/// exports are.
pub const NOTE_NO_ROUTES_MOUNTED: &str =
    "No data route is mounted for this cell: every export is bound directly to an existing \
     object rather than materialized, so there is nothing here to serve regardless of the \
     --no-data flag. Fetch rows via the locations listed in the context document's \
     data.channels.";

/// The engine-emitted note on a direct-attach (local catalog) document:
/// pinless ⇒ draft by definition (ADR 0012 §4) — data may exist locally, but
/// no published, verify-gated execution stands behind the document.
pub const NOTE_DIRECT_ATTACH: &str =
    "This cell runs in direct-attach (local catalog) mode: no published execution or run \
     summary stands behind this document, so it is served as a draft. Publish with a \
     storage-backed profile for verified provenance.";

/// The engine-emitted note on a storage-backed cell with no materializing
/// transforms (issue #6, issue #16/#3): no execution can ever exist for this
/// cell — `run` refuses to build one (`engine::run`) — so `NOTE_DIRECT_ATTACH`
/// would be actively wrong here (it instructs "publish with a storage-backed
/// profile", which this cell already is, and which produces no execution
/// regardless), and the generic `NOTE_NOTHING_BUILT` undersells it by
/// reading as "not yet" for a cell that never will be. Points at the one
/// command that CAN move this document off `draft`.
///
/// M3: deploy's pre-flight (`check_no_all_never`) does *not* refuse this
/// cell class anymore (issue #6/#11, the deploy relax) — it refuses only a
/// target with no long-lived Server capability at all. A Server IS
/// deployable for exactly this cell shape; only the Builder never applies
/// for one (no snapshot to build, `deploy::targets::kubernetes::render::
/// render_init_job` renders no Job — H1).
pub const NOTE_VIRTUAL_CELL: &str =
    "This cell has no materializing transforms, so there is no snapshot to publish and no \
     execution will ever stand behind this document. Run `datamk verify` to live-check the \
     contract against the bound source(s), which raises this document to \
     `verified_at_source`.";

#[derive(Debug)]
pub struct Facts {
    pub cell: String,
    pub interface: Interface,
    pub provenance: Option<Provenance>,
    pub source_check: Option<SourceCheck>,
    pub freshness: Option<FreshnessBlock>,
    pub upstreams: Vec<ObservedUpstream>,
    pub probes: IndexMap<String, ExportProbe>,
    pub docs_fingerprints: IndexMap<String, DocsFingerprint>,
    pub served_here: bool,
    pub channels: Vec<String>,
    pub direct_attach: bool,
    /// `config::builds_no_snapshot(&transforms)` — the exact predicate
    /// `engine::run` and the deploy pre-flight already refuse a build on.
    /// Distinct from `direct_attach`: a storage-backed cell with no
    /// materializing transforms is neither direct-attach nor ever going to
    /// publish, and needs its own `draft` note (issue #16/#3) rather than
    /// either `NOTE_DIRECT_ATTACH` — which tells the reader to do the one
    /// thing `run`/`release` refuse for this cell class — or the generic
    /// `NOTE_NOTHING_BUILT`, which reads as "not yet" for a cell that never
    /// will be.
    pub is_all_never: bool,
}

/// Assemble a document from prebuilt facts (how the serve handler works —
/// its interface and digest are precomputed at startup). Status is a
/// ladder, weakest to strongest (issue #6 Q3): `provenance` present ⇒
/// `verified` (its wire meaning — ADR 0012 §5) — checked first, so a mixed
/// cell's published execution (which already verified its bound exports live
/// at build time, see `engine::run`) reads as the strongest claim it earns;
/// else `source_check` present ⇒ `verified_at_source`; else `draft`, with
/// the matching engine note.
///
/// Measurements are laid onto the records they measure (ADR 0015 §1):
/// `probes` and `source_check.exports` onto `exports[].probe`/`.check` by
/// route; `upstreams` onto `upstreams[]` by ref; `docs_fingerprints` onto
/// `docs[]` by target. A measurement for a route/ref/target the interface
/// doesn't carry is dropped — the interface is visibility-filtered, and a
/// private export's measurement never reaches the wire (ADR 0012 §4).
pub fn assemble(facts: Facts) -> ContextDocument {
    let Facts {
        cell,
        interface,
        provenance,
        mut source_check,
        freshness,
        upstreams: observed_upstreams,
        mut probes,
        mut docs_fingerprints,
        served_here,
        channels,
        direct_attach,
        is_all_never,
    } = facts;
    let status = if provenance.is_some() {
        Status::Verified
    } else if source_check.is_some() {
        Status::VerifiedAtSource
    } else {
        Status::Draft
    };
    // issue #16/#3: `verify.rs`'s grain-uniqueness check only ever runs
    // `if !export.grain.is_empty()` — a grainless export gets no check at
    // all, so `status != Draft` alone would report `grain_verified: true`
    // for a check that never ran on it. Narrowed to also require every
    // discoverable export to declare a grain; only ever moves `true` to
    // `false` relative to the old formula, and only where the old value was
    // lying. Computed from the interface's exports so it can't drift from
    // what they say — and it never touches `interface_digest`.
    let grain_verified =
        status != Status::Draft && interface.exports.iter().all(|e| !e.grain.is_empty());
    let mut notes = Vec::new();
    if status == Status::Draft {
        notes.push(
            // M1: `is_all_never` checked first, deliberately — a local-dev
            // all-bound cell (every one of this file's own fixtures sets
            // `catalog:`, i.e. `direct_attach: true`) would otherwise get
            // `NOTE_DIRECT_ATTACH`'s "publish with a storage-backed profile
            // for verified provenance," which routes the reader straight
            // into `datamk run`'s own refusal (no materializing transforms
            // means no snapshot, direct-attach or not). `is_all_never` is
            // the stronger, permanent fact — no profile ever fixes it — so
            // it wins regardless of which profile loaded this document.
            if is_all_never {
                NOTE_VIRTUAL_CELL
            } else if direct_attach {
                NOTE_DIRECT_ATTACH
            } else {
                NOTE_NOTHING_BUILT
            }
            .to_string(),
        );
    }

    let mut checks = source_check
        .as_mut()
        .map(|c| std::mem::take(&mut c.exports))
        .unwrap_or_default();
    let exports = interface
        .exports
        .into_iter()
        .map(|mut e| {
            e.probe = probes.shift_remove(&e.route);
            e.check = checks.shift_remove(&e.route);
            e
        })
        .collect();

    let upstreams = interface
        .upstreams
        .into_iter()
        .map(|mut u| {
            if let Some(o) = observed_upstreams
                .iter()
                .find(|o| o.reference == u.reference)
            {
                u.execution = o.execution;
                u.data_as_of = o.data_as_of.clone();
            }
            u
        })
        .collect();

    let docs = interface
        .docs
        .into_iter()
        .map(|mut d| {
            if let Some(f) = docs_fingerprints.shift_remove(&d.target) {
                d.sha256 = Some(f.sha256);
                d.bytes = Some(f.bytes);
            }
            d
        })
        .collect();

    ContextDocument {
        datamk_context: DATAMK_CONTEXT_VERSION,
        cell,
        status,
        grain_verified,
        description: interface.description,
        from: interface.from,
        discovered_from: interface.discovered_from,
        exports,
        upstreams,
        definitions: interface.definitions,
        missing_terms: Vec::new(),
        definitions_request: interface.definitions_request,
        docs,
        include_request: interface.include_request,
        build: provenance,
        source_check,
        freshness,
        data: DataBlock {
            served_here,
            channels,
        },
        notes,
        // Request-specific (`?include=docs`) / flag-specific (`--no-docs`):
        // set by the caller after `assemble`/`build` returns via
        // `inline_docs`, the same post-build mutation pattern `emit` already
        // uses for `emitted_at`.
        included: Vec::new(),
        emitted_at: None,
        cell_yaml_digest: None,
    }
}

impl ContextDocument {
    /// Narrow the document to one export (ADR 0012 §4 amendment,
    /// 2026-08-27): `exports[]` keeps the record whose route is `route`,
    /// `docs[]` keeps the cell page and that export's page, everything
    /// cell-level stays. Same shape as the full document — a consumer
    /// parses one schema — so an agent answering one question fetches one
    /// export's contract and page, not the whole cell. Returns `false`,
    /// leaving the document untouched, when no discoverable export has that
    /// route.
    pub fn narrow_to(&mut self, route: &str) -> bool {
        if !self.exports.iter().any(|e| e.route == route) {
            return false;
        }
        self.exports.retain(|e| e.route == route);
        // ADR 0017 §3: a definition survives route narrowing iff it is
        // cell-wide (empty `applies_to`) or names this route or one of its
        // columns; its page survives alongside it.
        self.definitions.retain(|d| {
            d.applies_to.is_empty() || d.applies_to.iter().any(|e| applies_to_route(e, route))
        });
        let kept_terms: std::collections::HashSet<&str> =
            self.definitions.iter().map(|d| d.term.as_str()).collect();
        self.docs.retain(|d| {
            d.target == "cell"
                || d.target == route
                || d.target
                    .strip_prefix("definition:")
                    .is_some_and(|t| kept_terms.contains(t))
        });
        true
    }

    /// `terms=` (ADR 0017 §3): case-insensitive lookup over `all_definitions`'
    /// terms and aliases, deduplicated to canonical terms. Resolves against
    /// the **whole cell** — `all_definitions`/`all_docs` must be the
    /// unnarrowed lists (the caller captures them before calling
    /// `narrow_to`), never `self.definitions`/`self.docs`, which
    /// `/context/<route>` may already have reduced. Replaces
    /// `self.definitions` with the matched subset (declared order) and
    /// `self.docs` with exactly those terms' `definition:` pages (`sha256`/
    /// `bytes` fingerprints carried over, since `all_docs` is `assemble`'s
    /// output, not the fingerprint-less `Interface.docs`) — the cell page
    /// and any export page are dropped, matching an `include=docs` fetch
    /// under a filter never carrying every page again. Sets and returns
    /// `missing_terms`: every token that resolved to no term or alias,
    /// verbatim, in request order, deduplicated.
    pub fn narrow_terms(
        &mut self,
        tokens: &[String],
        all_definitions: &[DefinitionDoc],
        all_docs: &[DocsDoc],
    ) -> Vec<String> {
        let mut index: IndexMap<String, &str> = IndexMap::new();
        for d in all_definitions {
            index
                .entry(d.term.to_ascii_lowercase())
                .or_insert(d.term.as_str());
            for alias in &d.aliases {
                index
                    .entry(alias.to_ascii_lowercase())
                    .or_insert(d.term.as_str());
            }
        }
        let mut matched: Vec<String> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        for tok in tokens {
            match index.get(&tok.to_ascii_lowercase()) {
                Some(term) => {
                    if !matched.iter().any(|t| t == term) {
                        matched.push(term.to_string());
                    }
                }
                None => {
                    if !missing.contains(tok) {
                        missing.push(tok.clone());
                    }
                }
            }
        }
        self.definitions = all_definitions
            .iter()
            .filter(|d| matched.iter().any(|t| t == &d.term))
            .cloned()
            .collect();
        self.docs = all_docs
            .iter()
            .filter(|d| {
                d.target
                    .strip_prefix("definition:")
                    .is_some_and(|t| matched.iter().any(|m| m == t))
            })
            .cloned()
            .collect();
        self.missing_terms = missing.clone();
        missing
    }

    /// The interface this document carries — the same records, measurements
    /// and all; `interface_digest`'s projection ignores those by
    /// construction, so digesting this equals digesting the interface the
    /// document was assembled from. Lets a consumer holding only a document
    /// recompute its digest.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn to_interface(&self) -> Interface {
        Interface {
            description: self.description.clone(),
            from: self.from.clone(),
            discovered_from: self.discovered_from.clone(),
            exports: self.exports.clone(),
            upstreams: self.upstreams.clone(),
            definitions: self.definitions.clone(),
            definitions_request: self.definitions_request.clone(),
            docs: self.docs.clone(),
            include_request: self.include_request.clone(),
        }
    }

    /// Inline docs content (ADR 0013): marks `included: ["docs"]` and sets
    /// `content` on each `docs[]` entry whose `target` a loaded page names.
    /// `included` is set even when there are no pages — a truthful
    /// (possibly empty) answer, distinct from "server predates this field".
    pub fn inline_docs<'a>(
        &mut self,
        pages: impl IntoIterator<Item = &'a crate::config::docs::DocsPage>,
    ) {
        self.included = vec!["docs".to_string()];
        for p in pages {
            if let Some(d) = self.docs.iter_mut().find(|d| d.target == p.target) {
                d.media_type = p.media_type.clone();
                d.content = Some(p.content.to_string());
            }
        }
    }
}

/// Whether an `applies_to` entry (`name@major` or `name@major.column`)
/// names `route` itself or one of its columns (ADR 0017 §3, §5) — an exact
/// match, or `route` followed by a `.`. `pub(crate)`: `release.rs`'s
/// meaning-digest fan-out uses the identical rule to decide which
/// definitions fold into a route's `description_digest`.
pub(crate) fn applies_to_route(entry: &str, route: &str) -> bool {
    entry == route
        || entry
            .strip_prefix(route)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Build a document straight from a definition. `served_here` is the data
/// block's honest fact about *this* surface — a portable emission keeps the
/// query grammar (ADR 0012 §2, amended: unconditional) but never claims to
/// serve rows itself. `is_all_never` is `config::builds_no_snapshot(&transforms)`
/// — see `Facts`'s doc comment for why it's a separate fact from
/// `direct_attach`.
///
/// Kept positional (not `Facts`-taking) on purpose: this is `assemble`'s one
/// remaining positional caller, isolated to this one thin function so it
/// can't drift into a second copy of the struct-literal pattern below.
#[allow(clippy::too_many_arguments)]
pub fn build(
    def: &CellDef,
    routes: &[(String, Export)],
    provenance: Option<Provenance>,
    source_check: Option<SourceCheck>,
    freshness: Option<FreshnessBlock>,
    upstreams: Vec<ObservedUpstream>,
    docs_fingerprints: IndexMap<String, DocsFingerprint>,
    source_descriptions: IndexMap<String, IndexMap<String, String>>,
    served_here: bool,
    direct_attach: bool,
    is_all_never: bool,
) -> ContextDocument {
    assemble(Facts {
        cell: def.cell.clone(),
        interface: interface(def, routes, &source_descriptions),
        provenance,
        source_check,
        freshness,
        upstreams,
        probes: IndexMap::new(),
        docs_fingerprints,
        served_here,
        channels: Vec::new(),
        direct_attach,
        is_all_never,
    })
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

/// Cell-source name -> upstream ref, in declared order (structural: derived
/// only from `cell.yaml`, so it never changes for the life of a loaded
/// definition). The correlation key `observed_upstreams_from` uses to pair a
/// `SourceRunInfo` entry — keyed by the local source name — with the
/// upstream cell it names, so `observed.upstreams` reports against the same
/// nominal edge `declared.upstreams` already lists.
pub fn cell_source_refs(def: &CellDef) -> Vec<(String, String)> {
    def.sources
        .iter()
        .filter_map(|(name, s)| match s {
            Source::Cell { cell, .. } => Some((name.clone(), cell.clone())),
            _ => None,
        })
        .collect()
}

/// The admitted projection of a run summary's cell-source attachments (issue
/// #7, sibling to `provenance_from`): built field-by-field from the
/// allowlisted `SourceRunInfo.execution`/`data_as_of` — nothing else on
/// `SourceRunInfo` (connection, staged_rows, bytes_scanned…) ever rides
/// along. A source with no matching entry in `summary.sources` (e.g. the
/// summary predates this source, or the two have drifted) reports both
/// fields absent rather than being dropped — the ref still belongs in
/// `declared.upstreams`.
pub fn observed_upstreams_from(
    refs: &[(String, String)],
    summary: &RunSummary,
) -> Vec<ObservedUpstream> {
    refs.iter()
        .map(|(source_name, reference)| {
            let (execution, data_as_of) = summary
                .sources
                .iter()
                .find(|s| &s.name == source_name)
                .map(|s| (s.execution, s.data_as_of.clone()))
                .unwrap_or((None, None));
            ObservedUpstream {
                reference: reference.clone(),
                execution,
                data_as_of,
            }
        })
        .collect()
}

/// The interface digest: the document's `ETag` and `/openapi.json`'s
/// `info.version` (ADR 0012 §2, ADR 0015 §5). Hashes an **explicit
/// projection** of the interface — never the document, never a region:
/// shape, `cell`, `description`, each export's interface fields (name,
/// version, route, contract, description, freshness, grain, schema
/// `{type, unit, description}`, query grammar, binding), each upstream's
/// `{ref, version}`, each docs page's identity, and `data`. Nothing else: not
/// `from` (a description whose text is unchanged but whose origin changed is
/// not an interface change), not any timestamped block (`build`, probes,
/// checks, freshness, upstream executions, docs fingerprints), not notes, not
/// docs content. The digest must change when the *interface* changes, not
/// when data refreshes under it.
pub fn interface_digest(cell: &str, interface: &Interface, data: &DataBlock) -> String {
    #[derive(Serialize)]
    struct Projection<'a> {
        datamk_context: u32,
        cell: &'a str,
        description: &'a Option<String>,
        exports: Vec<ExportProjection<'a>>,
        upstreams: Vec<UpstreamProjection<'a>>,
        definitions: Vec<DefinitionProjection<'a>>,
        docs: Vec<DocsProjection<'a>>,
        include_request: &'a str,
        data: &'a DataBlock,
    }
    #[derive(Serialize)]
    struct ExportProjection<'a> {
        name: &'a str,
        version: &'a str,
        route: &'a str,
        contract: Contract,
        description: &'a Option<String>,
        freshness: &'a Option<String>,
        grain: &'a [String],
        schema: IndexMap<&'a str, ColumnProjection<'a>>,
        query: &'a Option<QueryBlock>,
        binding: &'a Option<BindingBlock>,
        depends_on: &'a [String],
    }
    #[derive(Serialize)]
    struct ColumnProjection<'a> {
        #[serde(rename = "type")]
        ty: &'a str,
        unit: &'a Option<String>,
        description: &'a Option<String>,
    }
    #[derive(Serialize)]
    struct UpstreamProjection<'a> {
        reference: &'a str,
        version: Option<u64>,
    }
    // ADR 0017 §4: term, aliases, applies_to — affordances. `description`
    // stays out (a prose typo must not tell OpenAPI tooling the callable
    // surface changed); a page's identity is already covered by the
    // `docs` projection below (its `target` is `definition:<term>`).
    #[derive(Serialize)]
    struct DefinitionProjection<'a> {
        term: &'a str,
        aliases: &'a [String],
        applies_to: &'a [String],
    }
    #[derive(Serialize)]
    struct DocsProjection<'a> {
        target: &'a str,
        source_path: &'a str,
        media_type: &'a str,
    }
    let projection = Projection {
        datamk_context: DATAMK_CONTEXT_VERSION,
        cell,
        description: &interface.description,
        exports: interface
            .exports
            .iter()
            .map(|e| ExportProjection {
                name: &e.name,
                version: &e.version,
                route: &e.route,
                contract: e.contract,
                description: &e.description,
                freshness: &e.freshness,
                grain: &e.grain,
                schema: e
                    .schema
                    .iter()
                    .map(|(col, c)| {
                        (
                            col.as_str(),
                            ColumnProjection {
                                ty: &c.ty,
                                unit: &c.unit,
                                description: &c.description,
                            },
                        )
                    })
                    .collect(),
                query: &e.query,
                binding: &e.binding,
                depends_on: &e.depends_on,
            })
            .collect(),
        upstreams: interface
            .upstreams
            .iter()
            .map(|u| UpstreamProjection {
                reference: &u.reference,
                version: u.version,
            })
            .collect(),
        definitions: interface
            .definitions
            .iter()
            .map(|d| DefinitionProjection {
                term: &d.term,
                aliases: &d.aliases,
                applies_to: &d.applies_to,
            })
            .collect(),
        docs: interface
            .docs
            .iter()
            .map(|d| DocsProjection {
                target: &d.target,
                source_path: &d.source_path,
                media_type: &d.media_type,
            })
            .collect(),
        include_request: &interface.include_request,
        data,
    };
    let bytes = serde_json::to_vec(&projection).expect("context projection serializes");
    hex(&Sha256::digest(&bytes))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// sha256 of the exact bytes on disk at `file` — the digest both
/// `build_document` (`cell_yaml_digest` on the portable artifact) and
/// `serve`'s startup load (issue #16, gating `.cell/source_check.json`
/// freshness) stamp, so the two doors can never digest different bytes for
/// the same path.
pub fn cell_yaml_digest_of(file: &std::path::Path) -> Result<String> {
    use anyhow::Context as _;
    let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    Ok(sha256_hex(&bytes))
}

/// The portable door's document, built but not yet serialized or written —
/// split out of `emit` so a test (the two-doors regression, `serve::mod`'s
/// `two_doors`) can construct the exact same `ContextDocument` the CLI emits
/// and compare it against the hosted door's, in one process, without
/// shelling out to the binary and parsing stdout. `emit` (below) is the only
/// production caller; everything about *how* the document reaches the
/// caller (stdout vs. `--out`, pretty-printing) stays there.
///
/// Loads the cell without a database (`config::load`); published-mode
/// profiles additionally fetch `LATEST`'s run summary from the store (the
/// same trust and credentials as `datamk status`). Pinless ⇒ draft, by
/// definition — a direct-attach profile has no pin and emits a draft even
/// after a local build.
///
/// `no_docs` (ADR 0013) is the one asymmetry with the served door: a request
/// can be repeated, a file cannot — so a portable artifact **inlines docs by
/// default**; `--no-docs` withholds content, emitting identity + fingerprints
/// only (mirroring `serve --no-data`'s withholding idiom). `included` is
/// truthful in both cases, so a consumer never needs to know which door
/// produced the file.
#[cfg_attr(not(test), allow(dead_code))]
pub fn build_document(
    file: &std::path::Path,
    profile: &str,
    no_docs: bool,
) -> Result<ContextDocument> {
    build_document_for(file, profile, no_docs, None, None)
}

/// `build_document`, optionally narrowed to one export's route (`datamk
/// context --export <route>`, the portable twin of `GET /context/<route>`)
/// and/or to a `--terms` list (ADR 0017 §6, composing with `--export`
/// exactly as `terms=` composes with `/context/<route>`). An unknown route
/// is an error naming the ones that exist; an unknown term is an error
/// naming the known ones — the deliberate CLI asymmetry with the served
/// door (ADR 0013 §7): a file written by `--out` cannot be re-requested.
pub fn build_document_for(
    file: &std::path::Path,
    profile: &str,
    no_docs: bool,
    export: Option<&str>,
    terms: Option<&[String]>,
) -> Result<ContextDocument> {
    let loaded = crate::config::load(file, profile)?;
    let routes = discoverable_routes(&loaded.def)?;
    let direct_attach = crate::config::direct_attach(&loaded.bindings);
    let is_all_never = crate::config::builds_no_snapshot(&loaded.transforms);
    let refs = cell_source_refs(&loaded.def);

    // Computed once, up front: it stamps every emitted document
    // (`cell_yaml_digest`) and is the staleness key for the live-verify
    // source-check record below (issue #6) — both need the exact same bytes,
    // read once. `cell_yaml_digest_of` is the same call `serve` (issue #16)
    // makes at startup, so the two doors can never digest different bytes.
    let cell_yaml_digest = cell_yaml_digest_of(file)?;

    // A cell with no materializing transforms never publishes — its storage
    // is never touched, so no store client (and no store credentials) here.
    let (provenance, upstreams) = if direct_attach || is_all_never {
        (None, Vec::new())
    } else {
        let store = crate::store::Store::for_storage(
            &loaded.bindings.storage,
            loaded.bindings.s3.as_ref(),
            loaded.bindings.gcs.as_ref(),
        )?;
        let summary = match store.latest()? {
            None => None,
            Some(n) => store
                .get(&crate::store::run_summary_key(n))?
                .and_then(|bytes| serde_json::from_slice::<RunSummary>(&bytes).ok()),
        };
        match summary {
            Some(summary) => (
                Some(provenance_from(&summary, None)),
                observed_upstreams_from(&refs, &summary),
            ),
            None => (None, Vec::new()),
        }
    };

    // Issue #6: `datamk verify` and `datamk context` run as separate
    // processes (CI: verify against the live warehouse, then emit the
    // document) — the record left under `.cell/source_check.json` is the
    // only thing that carries a passed live check across that boundary.
    // Embedded only when its digest still matches this `cell.yaml` AND it
    // was written under this same `profile` (issue #16) — either mismatch
    // means the record does not attest what's being read right now, and is
    // silently omitted, never emitted as if it still applied. `fresh_for`
    // is the one place this match happens (`serve`'s startup load, issue
    // #16, calls the same function).
    let source_check =
        crate::manifest::SourceCheckRecord::fresh_for(&loaded.dir, &cell_yaml_digest, profile)
            .as_ref()
            .map(|r| SourceCheck::from_record(r, &routes));

    // Docs fingerprints (ADR 0013 §5): a release-time fact, read from
    // `published.json` when one exists — never recomputed here (computing at
    // load/emit time would populate `observed.docs` on every never-run cell
    // that merely declares `docs:`, the exact invariant break §5 forbids).
    let docs_fingerprints: IndexMap<String, DocsFingerprint> =
        crate::manifest::Published::load(&loaded.dir)
            .map(|p| p.docs.into_iter().collect())
            .unwrap_or_default();

    // Issue #6/#10: same fresh_for gate as source_check above, same reason —
    // `datamk verify`'s live bind pass is the only thing that ever observes
    // upstream descriptions, and it runs as a separate process.
    let source_descriptions: IndexMap<String, IndexMap<String, String>> =
        crate::manifest::SourceDescriptionsRecord::fresh_for(
            &loaded.dir,
            &cell_yaml_digest,
            profile,
        )
        .map(|r| r.sources.into_iter().collect())
        .unwrap_or_default();

    let mut doc = build(
        &loaded.def,
        &routes,
        provenance,
        source_check,
        /* freshness */ None, // poll telemetry is a lie the instant the file is written
        upstreams,
        docs_fingerprints,
        source_descriptions,
        /* served_here */ false, // a file serves no rows
        direct_attach,
        is_all_never,
    );
    doc.data.channels = loaded.bindings.channels.clone();
    // ADR 0016 §5: a discovered cell with no fresh record has no interface
    // to describe — say so, rather than emit an empty export list that reads
    // as "this cell has no exports".
    if let Some(crate::config::Discovery::Stale(why)) = &loaded.discovery {
        // The served door refuses to start on a stale record
        // (`refuse_stale_discovery`); this portable door emits, so it must
        // be loud in both channels — the document's own notes, and stderr
        // for the operator who never reads `notes[]` (ADR 0017 amendment).
        tracing::warn!(
            "discovered interface unavailable ({why}) — emitting a document with no exports;              re-run `datamk sync`"
        );
        doc.notes.push(format!(
            "This cell discovers its interface, but {why}. No exports are listed until it is."
        ));
        // ADR 0017 amendment: `applies_to` validation is deferred until the
        // discovered interface exists, so on a stale record it never ran.
        // Definitions stay listed — they are authored prose, and dropping
        // them would lie by omission in the other direction — but the
        // document says what state they are in.
        if doc.definitions.iter().any(|d| !d.applies_to.is_empty()) {
            doc.notes.push(
                "definitions[] is authored prose and still listed, but its applies_to                  references have not been validated against any interface — re-run `datamk                  sync` before trusting them."
                    .to_string(),
            );
        }
    }

    // ADR 0017 §3/§6: captured before any narrowing — `--export` and
    // `--terms` compose (route narrowing first, then terms, exactly as the
    // served door does), and `terms=` resolves against the whole cell, not
    // the route's scope, so `narrow_terms` must never see the route-reduced
    // lists.
    let all_definitions = doc.definitions.clone();
    let all_docs = doc.docs.clone();

    if let Some(route) = export {
        if !doc.narrow_to(route) {
            anyhow::bail!(
                "no export '{route}' — discoverable exports: {}",
                if doc.exports.is_empty() {
                    "none".to_string()
                } else {
                    doc.exports
                        .iter()
                        .map(|e| e.route.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            );
        }
    }

    if let Some(tokens) = terms {
        let missing = doc.narrow_terms(tokens, &all_definitions, &all_docs);
        if !missing.is_empty() {
            let mut known: Vec<&str> = Vec::new();
            for d in &all_definitions {
                known.push(d.term.as_str());
                known.extend(d.aliases.iter().map(String::as_str));
            }
            anyhow::bail!(
                "unknown term(s): {} — known terms: {}",
                missing.join(", "),
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            );
        }
    }

    // ADR 0013 §7 / ADR 0017 §6: inline by default — a portable artifact
    // with null content pointing at a path the reader doesn't have is a
    // dangling pointer. `--no-docs` withholds page content only; a term's
    // `description` ships regardless (it's the short form, never withheld
    // for exports either). Runs after narrowing, so under `--terms` only
    // the selected pages inline.
    if !no_docs {
        let pages = crate::config::docs::load_declared(&loaded.dir, &loaded.def, &routes)?;
        doc.inline_docs(&pages);
    }

    doc.emitted_at = Some(crate::timeutil::rfc3339_utc(crate::timeutil::unix_now()));
    doc.cell_yaml_digest = Some(cell_yaml_digest);

    Ok(doc)
}

/// `datamk context` (ADR 0012 §4): emit the document to stdout (`--out` to
/// write a file). No server, no port, no token — commit it, host it
/// statically, paste it into an agent's context. A thin wrapper over
/// `build_document`: everything about the document's *content* lives there;
/// this function only decides where the serialized bytes go.
pub fn emit(
    file: &std::path::Path,
    profile: &str,
    out: Option<&std::path::Path>,
    no_docs: bool,
    export: Option<&str>,
    terms: Option<&[String]>,
) -> Result<()> {
    use anyhow::Context as _;

    let doc = build_document_for(file, profile, no_docs, export, terms)?;
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
    use std::path::PathBuf;

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
            /* source_check */ None,
            Some(FreshnessBlock {
                serving_execution: 47,
                latest_seen: 47,
                last_successful_poll_age_seconds: Some(3),
            }),
            /* upstreams */ Vec::new(),
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            /* served_here */ true,
            /* direct_attach */ false,
            false,
        )
    }

    /// The golden test (`RunSummary` precedent): the exact wire shape. A
    /// diff here is a contract change — additive is fine; a rename or
    /// re-meaning needs a `DATAMK_CONTEXT_VERSION` bump and a deprecation
    /// window (ADR 0012 §2).
    #[test]
    fn context_document_serializes_to_the_documented_shape() {
        let json = serde_json::to_string_pretty(&sample_verified()).unwrap();
        let expected = include_str!("../test/fixtures/context_v4_golden.json").trim_end();
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
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            true,
            false,
            false,
        );
        let v: serde_json::Value = serde_json::to_value(&doc).unwrap();
        assert_eq!(v["status"], "draft");
        assert_eq!(v["grain_verified"], false);
        assert!(v.get("build").is_none(), "build must be absent, got {v}");
        assert!(
            v.get("source_check").is_none(),
            "source_check must be absent, got {v}"
        );
        assert!(
            v["exports"][0].get("probe").is_none(),
            "no probe on an unbuilt cell: {v}"
        );
        assert_eq!(v["notes"][0], NOTE_NOTHING_BUILT);
        // ADR 0013: `included` is always present, `[]` when nothing was
        // requested/inlined — never absent, the old-binary/no-docs signal.
        assert_eq!(v["included"], serde_json::json!([]));
        assert_eq!(
            v["docs"],
            serde_json::json!([]),
            "docs identity is always present"
        );
    }

    #[test]
    fn direct_attach_document_is_draft_with_the_direct_attach_note() {
        let def = sample_def();
        let routes = discoverable_routes(&def).unwrap();
        let doc = build(
            &def,
            &routes,
            None,
            None,
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            true,
            true,
            false,
        );
        assert_eq!(doc.status, Status::Draft);
        assert_eq!(doc.notes, vec![NOTE_DIRECT_ATTACH.to_string()]);
    }

    /// issue #16/#3: a storage-backed cell with no materializing transforms
    /// is neither direct-attach nor ever going to publish — it must get its own
    /// note, not `NOTE_DIRECT_ATTACH` (which would tell the reader to
    /// "publish with a storage-backed profile", the one thing `run`/
    /// `release` refuse for this cell class).
    #[test]
    fn virtual_cell_document_is_draft_with_its_own_note_not_direct_attach() {
        let def = sample_def();
        let routes = discoverable_routes(&def).unwrap();
        let doc = build(
            &def,
            &routes,
            None,
            None,
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            true,
            /* direct_attach */ false,
            /* is_all_never */ true,
        );
        assert_eq!(doc.status, Status::Draft);
        assert_eq!(doc.notes, vec![NOTE_VIRTUAL_CELL.to_string()]);
    }

    /// M1: the untested combination — `direct_attach: true` AND
    /// `is_all_never: true` (a local-dev all-bound cell: every one of this
    /// file's own fixtures reaches this exact combo, since `catalog:` in
    /// `profiles/local.yaml` is precisely what makes a profile
    /// direct-attach). `NOTE_DIRECT_ATTACH` would route the reader to
    /// "publish with a storage-backed profile," which `run` refuses for
    /// this cell class regardless of profile — `NOTE_VIRTUAL_CELL` must win
    /// even though `direct_attach` is also true.
    #[test]
    fn virtual_cell_note_wins_over_direct_attach_when_both_are_true() {
        let def = sample_def();
        let routes = discoverable_routes(&def).unwrap();
        let doc = build(
            &def,
            &routes,
            None,
            None,
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            true,
            /* direct_attach */ true,
            /* is_all_never */ true,
        );
        assert_eq!(doc.status, Status::Draft);
        assert_eq!(
            doc.notes,
            vec![NOTE_VIRTUAL_CELL.to_string()],
            "an all-bound cell must never be told to publish with a storage-backed \
             profile, even when the profile that loaded it happens to be direct-attach"
        );
    }

    /// issue #16/#3: `verify.rs`'s grain-uniqueness check only ever runs for
    /// a column-declared grain (`!export.grain.is_empty()`) — a grainless
    /// export gets no check at all, so `grain_verified` must not read `true`
    /// for it just because *some* provenance exists elsewhere in the
    /// document.
    #[test]
    fn grain_verified_is_false_when_a_discoverable_export_declares_no_grain() {
        let mut def = sample_def();
        def.interface[0].grain = Vec::new();
        let routes = discoverable_routes(&def).unwrap();
        let doc = build(
            &def,
            &routes,
            Some(Provenance {
                execution: 47,
                snapshot_id: Some(12),
                verify_outcome: "passed".to_string(),
                started_at: "2026-07-13T10:00:00Z".to_string(),
                finished_at: "2026-07-13T10:00:05Z".to_string(),
                datamk_version: "0.0.12".to_string(),
                data_as_of: None,
            }),
            None,
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            true,
            false,
            false,
        );
        assert_eq!(doc.status, Status::Verified);
        assert!(
            !doc.grain_verified,
            "a grainless discoverable export must not report grain_verified: true"
        );
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

    /// The digest covers the interface projection and never a measurement
    /// (ADR 0015 §5): a data refresh must not churn agent caches; an
    /// interface change must.
    fn digest_of(doc: &ContextDocument) -> String {
        interface_digest(&doc.cell, &doc.to_interface(), &doc.data)
    }

    fn page(target: &str, content: &str) -> crate::config::docs::DocsPage {
        crate::config::docs::DocsPage {
            target: target.to_string(),
            path: "docs/x.md".to_string(),
            media_type: "text/markdown; charset=utf-8".to_string(),
            content: std::sync::Arc::from(content),
            sha256: "abc".to_string(),
            bytes: content.len(),
        }
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
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            true,
            false,
            false,
        );
        assert_eq!(
            digest_of(&verified),
            digest_of(&draft),
            "observed/status/notes must not move the digest"
        );

        // Issue #6: a populated `observed.source_check` (verified_at_source)
        // must not move the digest either — `observed` never does, this
        // field included.
        let verified_at_source = build(
            &def,
            &routes,
            None,
            Some(SourceCheck {
                outcome: "passed".to_string(),
                checked_at: "2026-08-07T10:00:00Z".to_string(),
                data_as_of: None,
                datamk_version: "0.0.13".to_string(),
                exports: IndexMap::new(),
            }),
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            true,
            false,
            false,
        );
        assert_eq!(
            digest_of(&draft),
            digest_of(&verified_at_source),
            "a populated observed.source_check must not move the digest"
        );

        let mut def2 = sample_def();
        def2.interface[0].grain.push("channel".to_string());
        let routes2 = discoverable_routes(&def2).unwrap();
        let changed = build(
            &def2,
            &routes2,
            None,
            None,
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            true,
            false,
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
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            true,
            false,
            false,
        );

        // Injecting top-level content (as `?include=docs` would) must not
        // move the digest — content lives outside `declared`.
        let mut with_content = base.clone();
        with_content.inline_docs(&[page("cell", "Some prose an agent will read.")]);
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
        let with_fp = build(
            &def,
            &routes,
            None,
            None,
            None,
            Vec::new(),
            fp,
            IndexMap::new(), // source_descriptions
            true,
            false,
            false,
        );
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
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            true,
            false,
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
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            true,
            false,
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
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            false,
            true,
            false,
        );
        doc.emitted_at = Some("2026-08-06T00:00:00Z".to_string());
        doc.cell_yaml_digest = Some("abc123".to_string());
        let v: serde_json::Value = serde_json::to_value(&doc).unwrap();
        assert_eq!(v["data"]["served_here"], false);
        assert!(v["exports"][0]["query"].is_object());
        assert_eq!(v["emitted_at"], "2026-08-06T00:00:00Z");
        assert_eq!(v["cell_yaml_digest"], "abc123");
        // Poll telemetry never rides the portable artifact.
        assert!(v.get("freshness").is_none());
    }

    #[test]
    fn discoverable_routes_filters_private_and_sorts() {
        let def = sample_def();
        let routes = discoverable_routes(&def).unwrap();
        let keys: Vec<&str> = routes.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["orders_daily@2"]);
    }

    // --- issue #7: observed.upstreams --------------------------------

    use crate::engine::run_summary::SourceRunInfo;

    fn sample_run_summary(sources: Vec<SourceRunInfo>) -> RunSummary {
        RunSummary {
            execution: 47,
            snapshot_id: Some(12),
            started_at: "2026-07-13T10:00:00Z".to_string(),
            finished_at: "2026-07-13T10:00:05Z".to_string(),
            datamk_version: "0.0.12".to_string(),
            verify_outcome: "passed".to_string(),
            sources,
            transforms: Vec::new(),
        }
    }

    #[test]
    fn cell_source_refs_maps_source_name_to_upstream_ref() {
        let def = sample_def();
        assert_eq!(
            cell_source_refs(&def),
            vec![("upstream_flights".to_string(), "flights".to_string())]
        );
    }

    /// The published-mode shape (issue #7): the actually-attached execution
    /// and its snapshot time thread from `SourceRunInfo` (keyed by the
    /// local source name) into `ObservedUpstream` (keyed by the upstream
    /// ref), never fabricated, never carrying the upstream `table`.
    #[test]
    fn observed_upstreams_from_projects_execution_and_data_as_of() {
        let refs = vec![("upstream_flights".to_string(), "flights".to_string())];
        let summary = sample_run_summary(vec![SourceRunInfo {
            name: "upstream_flights".to_string(),
            connection: None,
            kind: None,
            staged_rows: None,
            bytes_scanned: None,
            execution: Some(41),
            data_as_of: Some("2026-08-04 06:00:11+00".to_string()),
        }]);
        let upstreams = observed_upstreams_from(&refs, &summary);
        assert_eq!(upstreams.len(), 1);
        assert_eq!(upstreams[0].reference, "flights");
        assert_eq!(upstreams[0].execution, Some(41));
        assert_eq!(
            upstreams[0].data_as_of.as_deref(),
            Some("2026-08-04 06:00:11+00")
        );
    }

    /// Direct-attach cell sources have no execution number (ADR 0004 §12) —
    /// the projection must report `None`, never fabricate one, even though
    /// the source did produce a `SourceRunInfo` entry.
    #[test]
    fn observed_upstreams_from_reports_none_execution_for_direct_attach() {
        let refs = vec![("upstream_flights".to_string(), "flights".to_string())];
        let summary = sample_run_summary(vec![SourceRunInfo {
            name: "upstream_flights".to_string(),
            connection: None,
            kind: None,
            staged_rows: None,
            bytes_scanned: None,
            execution: None,
            data_as_of: Some("2026-08-04 06:00:11+00".to_string()),
        }]);
        let upstreams = observed_upstreams_from(&refs, &summary);
        assert_eq!(upstreams[0].execution, None);
        assert!(upstreams[0].data_as_of.is_some());
    }

    /// A declared cell source with nothing in the summary (drift, or a
    /// summary predating this source) still gets an entry — both fields
    /// absent, never dropped and never fabricated.
    #[test]
    fn observed_upstreams_from_handles_a_source_missing_from_the_summary() {
        let refs = vec![("upstream_flights".to_string(), "flights".to_string())];
        let summary = sample_run_summary(Vec::new());
        let upstreams = observed_upstreams_from(&refs, &summary);
        assert_eq!(upstreams.len(), 1);
        assert_eq!(upstreams[0].reference, "flights");
        assert_eq!(upstreams[0].execution, None);
        assert_eq!(upstreams[0].data_as_of, None);
    }

    /// The digest must not move when `observed.upstreams` is populated —
    /// it's observed telemetry, not interface, and churning it on every
    /// refresh is exactly the failure this feature must not reintroduce
    /// (ADR 0012 §2, same guarantee `digest_ignores_observed_but_tracks_declared`
    /// pins for `provenance`/`freshness`).
    #[test]
    fn digest_ignores_observed_upstreams() {
        let def = sample_def();
        let routes = discoverable_routes(&def).unwrap();
        let draft = build(
            &def,
            &routes,
            None,
            None,
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            true,
            false,
            false,
        );
        let with_upstreams = build(
            &def,
            &routes,
            None,
            None,
            None,
            vec![ObservedUpstream {
                reference: "flights".to_string(),
                execution: Some(41),
                data_as_of: Some("2026-08-04 06:00:11+00".to_string()),
            }],
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            true,
            false,
            false,
        );
        assert_eq!(
            digest_of(&draft),
            digest_of(&with_upstreams),
            "observed.upstreams must not move the digest"
        );
    }

    /// All three new-facts sources at once (issue #7 + ADR 0013 + issue #6):
    /// a document with populated `observed.upstreams`, populated docs
    /// (identity, inlined content, and a fingerprint), AND a populated
    /// `observed.source_check` must still digest identically to the bare
    /// draft — none of the three is part of the interface, and the features
    /// landing across releases must not compound into a churned digest.
    #[test]
    fn digest_ignores_observed_upstreams_and_docs_content_together() {
        let mut def = sample_def();
        def.docs = Some("docs/overview.md".to_string());
        let routes = discoverable_routes(&def).unwrap();
        let draft = build(
            &def,
            &routes,
            None,
            None,
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            true,
            false,
            false,
        );

        let mut fp = IndexMap::new();
        fp.insert(
            "cell".to_string(),
            DocsFingerprint {
                sha256: "abc123".to_string(),
                bytes: 3,
            },
        );
        let mut loaded = build(
            &def,
            &routes,
            None,
            Some(SourceCheck {
                outcome: "passed".to_string(),
                checked_at: "2026-08-07T10:00:00Z".to_string(),
                data_as_of: None,
                datamk_version: "0.0.13".to_string(),
                exports: IndexMap::new(),
            }),
            None,
            vec![ObservedUpstream {
                reference: "flights".to_string(),
                execution: Some(41),
                data_as_of: Some("2026-08-04 06:00:11+00".to_string()),
            }],
            fp,
            IndexMap::new(), // source_descriptions
            true,
            false,
            false,
        );
        // Inline docs content too, as `?include=docs` would.
        loaded.inline_docs(&[page("cell", "Some prose an agent will read.")]);

        assert_eq!(
            digest_of(&draft),
            digest_of(&loaded),
            "observed.upstreams, observed.source_check, and docs content/fingerprint together \
             must not move the digest"
        );
    }

    /// An observed attachment lands on the declared upstream edge it
    /// belongs to (ADR 0015 §1) — one record, pin and measurement together.
    #[test]
    fn observed_execution_lands_on_the_declared_upstream_edge() {
        let def = sample_def();
        let routes = discoverable_routes(&def).unwrap();
        let doc = build(
            &def,
            &routes,
            None,
            None,
            None,
            vec![ObservedUpstream {
                reference: "flights".to_string(),
                execution: Some(41),
                data_as_of: None,
            }],
            IndexMap::new(),
            IndexMap::new(), // source_descriptions
            true,
            false,
            false,
        );
        let v: serde_json::Value = serde_json::to_value(&doc).unwrap();
        assert_eq!(v["status"], "draft", "no provenance ⇒ still draft");
        assert_eq!(v["upstreams"][0]["ref"], "flights");
        assert_eq!(v["upstreams"][0]["version"], 7);
        assert_eq!(v["upstreams"][0]["execution"], 41);
        assert!(
            v["upstreams"][0].get("data_as_of").is_none(),
            "never fabricated: {v}"
        );
    }

    /// `upstreams` is always present — `[]` when the cell declares no `cell`
    /// sources — and an unobserved edge carries the pin alone.
    #[test]
    fn upstreams_is_present_and_carries_only_the_pin_when_unobserved() {
        let v = serde_json::to_value(sample_verified()).unwrap();
        assert_eq!(
            v["upstreams"],
            serde_json::json!([{"ref": "flights", "version": 7}])
        );

        let mut def = sample_def();
        def.sources.shift_remove("upstream_flights");
        let routes = discoverable_routes(&def).unwrap();
        let doc = build(
            &def,
            &routes,
            None,
            None,
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(),
            true,
            false,
            false,
        );
        let v = serde_json::to_value(doc).unwrap();
        assert_eq!(v["upstreams"], serde_json::json!([]));
    }

    // --- ADR 0013: long-form docs pages -------------------------------

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
        let d = interface(&def, &routes, &IndexMap::new());

        assert_eq!(d.include_request, "context?include=docs");
        assert_eq!(d.docs.len(), 2, "{:?}", d.docs);
        assert_eq!(d.docs[0].target, "cell");
        assert_eq!(d.docs[0].source_path, "docs/overview.md");
        assert_eq!(d.docs[0].media_type, "text/markdown; charset=utf-8");
        assert_eq!(d.docs[1].target, "orders_daily@2");
        assert_eq!(d.docs[1].source_path, "docs/orders_daily.md");

        // Private export's docs page never appears, in any form.
        assert!(d.docs.iter().all(|p| p.source_path != "docs/internal.md"));
    }

    #[test]
    fn declared_docs_is_empty_but_present_when_nothing_is_declared() {
        let def = sample_def();
        let routes = discoverable_routes(&def).unwrap();
        let d = interface(&def, &routes, &IndexMap::new());
        assert!(d.docs.is_empty());
        assert_eq!(d.include_request, "context?include=docs");
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
        emit(
            &dir.join("cell.yaml"),
            "local",
            Some(&default_out),
            false,
            None,
            None,
        )
        .unwrap();
        let default_doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&default_out).unwrap()).unwrap();
        assert_eq!(default_doc["included"], serde_json::json!(["docs"]));
        assert_eq!(default_doc["docs"][0]["target"], "cell");
        assert!(
            default_doc["docs"][0]["content"]
                .as_str()
                .unwrap()
                .contains("What this cell is for"),
            "{default_doc}"
        );

        let no_docs_out = dir.join("context_no_docs.json");
        emit(
            &dir.join("cell.yaml"),
            "local",
            Some(&no_docs_out),
            true,
            None,
            None,
        )
        .unwrap();
        let no_docs_doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&no_docs_out).unwrap()).unwrap();
        assert_eq!(no_docs_doc["included"], serde_json::json!([]));
        assert!(
            no_docs_doc["docs"][0].get("content").is_none(),
            "{no_docs_doc}"
        );
        // Identity still ships either way — `--no-docs` withholds content,
        // not the fact that a page exists.
        assert_eq!(no_docs_doc["docs"][0]["source_path"], "docs/overview.md");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- issue #6, live-verify core: the `.cell/source_check.json` --------
    // --- round trip ----------------------------------------------------

    /// A fresh, on-disk all-`never` cell — `verify::run` and `context::emit`
    /// driven for real (`config::load` + a real `.cell/` directory), same
    /// pattern `release.rs`'s and `verify.rs`'s live-verify tests use, and
    /// for the same reason: this round trip is specifically about the two
    /// commands agreeing through a file on disk, which nothing in-memory
    /// can stand in for.
    fn all_bound_cell_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datamk-context-source-check-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sql")).unwrap();
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::write(
            dir.join("cell.yaml"),
            "cell: virtual_only\n\
             interface:\n\
             \x20 - name: virtual_pii\n\
             \x20   version: 1.0.0\n\
             \x20   grain: [id]\n\
             \x20   bind: raw\n\
             sources:\n\
             \x20 raw: ./data.csv\n",
        )
        .unwrap();
        std::fs::write(dir.join("data.csv"), "id,val\n1,a\n2,b\n").unwrap();
        std::fs::write(
            dir.join("profiles/local.yaml"),
            "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn context_embeds_source_check_and_reports_verified_at_source_after_a_passing_live_verify() {
        let dir = all_bound_cell_dir("fresh");
        let file = dir.join("cell.yaml");
        crate::verify::run(&file, "local").expect("live-verify the all-never cell");
        assert!(
            dir.join(".cell/source_check.json").is_file(),
            "verify must have written the source-check record"
        );

        let out = dir.join("context.json");
        emit(&file, "local", Some(&out), false, None, None).expect("emit the context document");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(v["status"], "verified_at_source");
        assert_eq!(v["grain_verified"], true);
        assert!(
            v.get("build").is_none(),
            "no execution ⇒ no build block: {v}"
        );
        assert_eq!(v["source_check"]["outcome"], "passed");
        assert!(
            v["source_check"]["checked_at"].is_string(),
            "checked_at must be a real timestamp: {v}"
        );
        assert!(
            v["source_check"]["datamk_version"].is_string(),
            "datamk_version must be present: {v}"
        );
        // ADR 0012 §2: never fabricated, never defaulted to checked_at — no
        // connector in this slice supplies one, so it must be absent.
        assert!(
            v["source_check"].get("data_as_of").is_none(),
            "data_as_of must be omitted, not fabricated: {v}"
        );
        // The per-export measurement rides the export (ADR 0015 §1), not
        // the cell-level block.
        assert!(v["source_check"].get("exports").is_none(), "{v}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The binding is the only actionable fact on a cell that serves no rows.
    /// Before it, the warehouse object existed only in prose and `channels`
    /// gave the dataset alone, so writing a query meant reading English.
    #[test]
    fn a_bound_export_emits_its_binding_and_a_materialized_one_does_not() {
        let def: CellDef = serde_yaml::from_str(
            r#"
cell: qfai
sources:
  gold_customer:
    connection: dw-main
    table: ${DATASET}.fct_qfai_customer
  local_file: ./data/*.parquet
interface:
  - name: qfai_customer
    version: 1.0.0
    grain: [customer_id]
    bind: gold_customer
  - name: from_file
    version: 1.0.0
    grain: [id]
    bind: local_file
  - name: materialized
    version: 1.0.0
    grain: [id]
"#,
        )
        .unwrap();
        let routes = discoverable_routes(&def).unwrap();
        let d = interface(&def, &routes, &IndexMap::new());
        let by_name = |n: &str| d.exports.iter().find(|e| e.name == n).unwrap();

        let bound = by_name("qfai_customer");
        let b = bound
            .binding
            .as_ref()
            .expect("a bound export names its target");
        assert_eq!(b.source, "gold_customer");
        // Verbatim cell.yaml: expanding this would put the environment inside
        // `interface_digest`.
        assert_eq!(b.object.as_deref(), Some("${DATASET}.fct_qfai_customer"));
        assert_eq!(b.connection.as_deref(), Some("dw-main"));
        assert!(bound.query.is_none(), "binding and query are complements");

        let raw = by_name("from_file").binding.as_ref().unwrap();
        assert_eq!(raw.object.as_deref(), Some("./data/*.parquet"));
        assert!(raw.connection.is_none(), "a raw source has no connection");

        let m = by_name("materialized");
        assert!(m.binding.is_none());
        assert!(m.query.is_some());
    }

    /// `grain_verified: true` used to be the whole story: `verify` computed
    /// the counts, compared them, and threw them away.
    #[test]
    fn source_check_carries_the_grain_measurement_through_to_the_document() {
        let dir = all_bound_cell_dir("measurements");
        let file = dir.join("cell.yaml");
        crate::verify::run(&file, "local").expect("live-verify the all-bound cell");

        let out = dir.join("context.json");
        emit(&file, "local", Some(&out), false, None, None).expect("emit the context document");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();

        assert_eq!(v["exports"][0]["route"], "virtual_pii@1");
        let m = &v["exports"][0]["check"];
        assert_eq!(
            m["at"], v["source_check"]["checked_at"],
            "one pass, one time"
        );
        assert_eq!(m["check"], "grain_unique");
        assert_eq!(m["grain"], serde_json::json!(["id"]));
        // data.csv holds two rows with distinct ids.
        assert_eq!(m["rows"], 2);
        assert_eq!(m["distinct_grain"], 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn context_omits_a_stale_source_check_record_and_falls_back_to_draft() {
        // Issue #6: `verify` and `context` are separate processes; a
        // `cell.yaml` edit between the two must silently invalidate the
        // record rather than let a check of the *previous* contract ride
        // along as if it still applied.
        let dir = all_bound_cell_dir("stale");
        let file = dir.join("cell.yaml");
        crate::verify::run(&file, "local").expect("live-verify the all-never cell");
        assert!(dir.join(".cell/source_check.json").is_file());

        // Edit cell.yaml after the check ran — the record's digest no
        // longer matches.
        let mut yaml = std::fs::read_to_string(&file).unwrap();
        yaml.push_str("description: added after the live check ran\n");
        std::fs::write(&file, yaml).unwrap();

        let out = dir.join("context.json");
        emit(&file, "local", Some(&out), false, None, None)
            .expect("emit must still succeed, just without the stale record");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(
            v["status"], "draft",
            "a stale source-check record must not promote status: {v}"
        );
        assert!(
            v.get("source_check").is_none() && v["exports"][0].get("check").is_none(),
            "a stale source-check record must be omitted entirely, not emitted stale: {v}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- ADR 0015: per-field provenance --------------------------------

    fn bound_def() -> CellDef {
        serde_yaml::from_str(
            r#"
cell: qfai
sources:
  gold_customer:
    connection: dw-main
    table: gold.fct_qfai_customer
interface:
  - name: qfai_customer
    version: 1.0.0
    description: One row per customer.
    grain: [customer_id]
    bind: gold_customer
    schema:
      customer_id: bigint
      revenue:
        type: decimal
        unit: USD
        description: Authored, and therefore wins.
      churned: boolean
  - name: derived
    version: 1.0.0
    grain: [customer_id]
    schema:
      customer_id: bigint
      churned: boolean
"#,
        )
        .unwrap()
    }

    fn warehouse_descriptions() -> IndexMap<String, IndexMap<String, String>> {
        let mut cols = IndexMap::new();
        cols.insert("customer_id".to_string(), "The customer key.".to_string());
        cols.insert("revenue".to_string(), "Warehouse says gross.".to_string());
        cols.insert("churned".to_string(), "TRUE once cancelled.".to_string());
        let mut m = IndexMap::new();
        m.insert("gold_customer".to_string(), cols);
        m
    }

    /// Every authored claim names `cell.yaml` as its origin, on the record
    /// that carries it (ADR 0015 §2) — and only for fields that are present.
    #[test]
    fn authored_claims_carry_cell_yaml_provenance_per_field() {
        let v = serde_json::to_value(sample_verified()).unwrap();
        assert_eq!(v["from"], serde_json::json!({"description": "cell.yaml"}));
        let e = &v["exports"][0];
        assert_eq!(
            e["from"],
            serde_json::json!({"description": "cell.yaml", "grain": "cell.yaml"})
        );
        assert_eq!(
            e["schema"]["order_date"]["from"],
            serde_json::json!({"type": "cell.yaml"})
        );
        assert_eq!(
            e["schema"]["revenue"]["from"],
            serde_json::json!({"type": "cell.yaml", "unit": "cell.yaml", "description": "cell.yaml"})
        );
    }

    /// Warehouse column comments land on a bound export's columns with
    /// `from.description: "warehouse"`, lose to an authored description, and
    /// never touch a materialized export (ADR 0015 §4).
    #[test]
    fn warehouse_descriptions_land_on_bound_columns_only_and_lose_to_authored_ones() {
        let def = bound_def();
        let routes = discoverable_routes(&def).unwrap();
        let i = interface(&def, &routes, &warehouse_descriptions());
        let bound = i
            .exports
            .iter()
            .find(|e| e.name == "qfai_customer")
            .unwrap();
        let id = &bound.schema["customer_id"];
        assert_eq!(id.description.as_deref(), Some("The customer key."));
        assert_eq!(id.from["description"], Origin::Warehouse);
        assert_eq!(id.from["type"], Origin::CellYaml);
        let revenue = &bound.schema["revenue"];
        assert_eq!(
            revenue.description.as_deref(),
            Some("Authored, and therefore wins.")
        );
        assert_eq!(revenue.from["description"], Origin::CellYaml);

        let derived = i.exports.iter().find(|e| e.name == "derived").unwrap();
        assert!(derived.schema["churned"].description.is_none());
        assert!(!derived.schema["churned"].from.contains_key("description"));
    }

    /// A warehouse comment edit on a bound column IS an interface change to
    /// the agent reading it, so it moves the digest (ADR 0015 §5) — while a
    /// change of origin with identical text does not (`from` is excluded).
    #[test]
    fn digest_tracks_description_text_but_not_its_origin() {
        let def = bound_def();
        let routes = discoverable_routes(&def).unwrap();
        let data = DataBlock {
            served_here: false,
            channels: Vec::new(),
        };
        let bare = interface(&def, &routes, &IndexMap::new());
        let with_wh = interface(&def, &routes, &warehouse_descriptions());
        assert_ne!(
            interface_digest("qfai", &bare, &data),
            interface_digest("qfai", &with_wh, &data),
            "a warehouse description an agent reads is part of the interface"
        );

        let mut edited = warehouse_descriptions();
        edited["gold_customer"].insert("customer_id".to_string(), "Renamed meaning.".to_string());
        let with_edit = interface(&def, &routes, &edited);
        assert_ne!(
            interface_digest("qfai", &with_wh, &data),
            interface_digest("qfai", &with_edit, &data)
        );

        // Same text, different origin: write the warehouse's words into
        // cell.yaml — the digest must not move.
        let mut def2 = def.clone();
        let bound_schema = &mut def2.interface[0].schema;
        bound_schema.get_mut("customer_id").unwrap().description =
            Some("The customer key.".to_string());
        bound_schema.get_mut("churned").unwrap().description =
            Some("TRUE once cancelled.".to_string());
        let routes2 = discoverable_routes(&def2).unwrap();
        let authored = interface(&def2, &routes2, &IndexMap::new());
        let authored_bound = authored
            .exports
            .iter()
            .find(|e| e.name == "qfai_customer")
            .unwrap();
        assert_eq!(
            authored_bound.schema["customer_id"].from["description"],
            Origin::CellYaml
        );
        assert_eq!(
            interface_digest("qfai", &with_wh, &data),
            interface_digest("qfai", &authored, &data),
            "origin alone must not move the digest"
        );
    }

    /// The v4 shape rule (ADR 0015 §3): every top-level measured block
    /// carries a timestamp; every export-level one too.
    #[test]
    fn measurements_carry_timestamps_and_land_on_their_records() {
        let def = sample_def();
        let routes = discoverable_routes(&def).unwrap();
        let mut probes = IndexMap::new();
        let mut probe = ExportProbe::at("2026-08-07T10:00:00Z".to_string());
        probe.rows = Some(4);
        probes.insert("orders_daily@2".to_string(), probe);
        let mut checks = IndexMap::new();
        checks.insert(
            "orders_daily@2".to_string(),
            ExportCheck {
                at: "2026-08-07T09:00:00Z".to_string(),
                check: "grain_unique".to_string(),
                grain: vec!["order_date".to_string(), "region".to_string()],
                rows: 4,
                distinct_grain: 4,
            },
        );
        let doc = assemble(Facts {
            cell: def.cell.clone(),
            interface: interface(&def, &routes, &IndexMap::new()),
            provenance: None,
            source_check: Some(SourceCheck {
                outcome: "passed".to_string(),
                checked_at: "2026-08-07T09:00:00Z".to_string(),
                data_as_of: None,
                datamk_version: "0.0.13".to_string(),
                exports: checks,
            }),
            freshness: None,
            upstreams: Vec::new(),
            probes,
            docs_fingerprints: IndexMap::new(),
            served_here: true,
            channels: Vec::new(),
            direct_attach: false,
            is_all_never: false,
        });
        let v = serde_json::to_value(&doc).unwrap();
        assert_eq!(v["status"], "verified_at_source");
        assert_eq!(v["exports"][0]["probe"]["at"], "2026-08-07T10:00:00Z");
        assert_eq!(v["exports"][0]["probe"]["rows"], 4);
        assert_eq!(v["exports"][0]["check"]["at"], "2026-08-07T09:00:00Z");
        assert_eq!(v["exports"][0]["check"]["distinct_grain"], 4);
        assert_eq!(v["source_check"]["checked_at"], "2026-08-07T09:00:00Z");
        // Neither measurement moves the digest.
        let bare = build(
            &def,
            &routes,
            None,
            None,
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(),
            true,
            false,
            false,
        );
        assert_eq!(digest_of(&bare), digest_of(&doc));
    }

    // --- ADR 0017: definitions in the context document ---------------------

    fn definition(
        term: &str,
        aliases: &[&str],
        description: &str,
        applies_to: &[&str],
    ) -> crate::config::Definition {
        serde_yaml::from_str(&format!(
            "term: {term}\naliases: [{}]\ndescription: {description}\napplies_to: [{}]\n",
            aliases.join(","),
            applies_to.join(","),
        ))
        .unwrap()
    }

    fn def_with_definitions() -> CellDef {
        let mut def = sample_def();
        def.definitions = vec![
            // Cell-wide: no export names it.
            definition(
                "active_customer",
                &[],
                "A customer with an order in 90 days.",
                &[],
            ),
            // Scoped to orders_daily@2's revenue column.
            definition(
                "net_revenue",
                &["nr", "revenue_net"],
                "Invoiced revenue less credit memos.",
                &["orders_daily@2.revenue"],
            ),
            // Scoped to a route this cell doesn't have — never resolves.
            definition("unrelated", &[], "Scoped elsewhere.", &["margins@2"]),
        ];
        def
    }

    #[test]
    fn docs_entries_emits_a_definition_target_for_each_definitions_page() {
        let mut def = def_with_definitions();
        def.definitions[1].docs = Some("docs/net_revenue.md".to_string());
        let routes = discoverable_routes(&def).unwrap();
        let iface = interface(&def, &routes, &IndexMap::new());
        let entry = iface
            .docs
            .iter()
            .find(|d| d.target == "definition:net_revenue")
            .expect("definition page must be a docs[] entry");
        assert_eq!(entry.source_path, "docs/net_revenue.md");
    }

    #[test]
    fn interface_gains_the_definitions_index_and_the_definitions_request_affordance() {
        let def = def_with_definitions();
        let routes = discoverable_routes(&def).unwrap();
        let iface = interface(&def, &routes, &IndexMap::new());
        let terms: Vec<&str> = iface.definitions.iter().map(|d| d.term.as_str()).collect();
        assert_eq!(terms, vec!["active_customer", "net_revenue", "unrelated"]);
        assert_eq!(
            iface.definitions_request,
            "context?terms=<term>[,<term>]&include=docs"
        );
        let nr = iface
            .definitions
            .iter()
            .find(|d| d.term == "net_revenue")
            .unwrap();
        assert_eq!(nr.aliases, vec!["nr", "revenue_net"]);
        assert_eq!(nr.applies_to, vec!["orders_daily@2.revenue"]);
        assert_eq!(nr.from.get("description"), Some(&Origin::CellYaml));
    }

    /// ADR 0017 §4: a description-text edit must not move the digest (a
    /// docs-class field); adding/renaming a term must (an interface change).
    #[test]
    fn interface_digest_ignores_definition_description_but_moves_on_term_rename() {
        let mut def = def_with_definitions();
        let routes = discoverable_routes(&def).unwrap();
        let data = DataBlock {
            served_here: true,
            channels: vec![],
        };
        let base = interface_digest("orders", &interface(&def, &routes, &IndexMap::new()), &data);

        def.definitions[1].description = "A completely different sentence.".to_string();
        let same = interface_digest("orders", &interface(&def, &routes, &IndexMap::new()), &data);
        assert_eq!(base, same, "a description edit must not move the digest");

        def.definitions[1].term = "net_revenue_v2".to_string();
        let moved = interface_digest("orders", &interface(&def, &routes, &IndexMap::new()), &data);
        assert_ne!(base, moved, "renaming a term must move the digest");
    }

    /// ADR 0017 §3: `/context/<route>` keeps a definition iff `applies_to`
    /// is empty (cell-wide) or names the route/one of its columns — and
    /// keeps its page alongside the cell page and the export's.
    #[test]
    fn narrow_to_filters_definitions_by_applies_to() {
        let mut def = def_with_definitions();
        def.docs = Some("docs/cell.md".to_string());
        def.definitions[1].docs = Some("docs/net_revenue.md".to_string());
        let routes = discoverable_routes(&def).unwrap();
        let mut doc = build(
            &def,
            &routes,
            None,
            None,
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(),
            true,
            false,
            false,
        );
        assert!(doc.narrow_to("orders_daily@2"));
        let terms: Vec<&str> = doc.definitions.iter().map(|d| d.term.as_str()).collect();
        assert_eq!(
            terms,
            vec!["active_customer", "net_revenue"],
            "cell-wide and route-matching definitions survive; the unrelated one doesn't"
        );
        // `orders_daily@2` itself declares no `docs:` in this fixture, so
        // only the cell page and the surviving definition's page appear.
        let doc_targets: Vec<&str> = doc.docs.iter().map(|d| d.target.as_str()).collect();
        assert!(doc_targets.contains(&"cell"));
        assert!(doc_targets.contains(&"definition:net_revenue"));
        assert!(!doc_targets.contains(&"definition:unrelated"));
    }

    fn full_doc_with_definitions() -> (ContextDocument, Vec<DefinitionDoc>, Vec<DocsDoc>) {
        let mut def = def_with_definitions();
        def.definitions[1].docs = Some("docs/net_revenue.md".to_string());
        let routes = discoverable_routes(&def).unwrap();
        let doc = build(
            &def,
            &routes,
            None,
            None,
            None,
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(),
            true,
            false,
            false,
        );
        let all_definitions = doc.definitions.clone();
        let all_docs = doc.docs.clone();
        (doc, all_definitions, all_docs)
    }

    #[test]
    fn narrow_terms_resolves_case_insensitively_by_term_or_alias_and_reports_missing() {
        let (mut doc, all_definitions, all_docs) = full_doc_with_definitions();
        let missing = doc.narrow_terms(
            &["NET_REVENUE".to_string(), "nope".to_string()],
            &all_definitions,
            &all_docs,
        );
        assert_eq!(missing, vec!["nope".to_string()]);
        assert_eq!(doc.missing_terms, vec!["nope".to_string()]);
        assert_eq!(doc.definitions.len(), 1);
        assert_eq!(doc.definitions[0].term, "net_revenue");
        // docs[] holds only the selected term's page — cell/export pages dropped.
        assert_eq!(doc.docs.len(), 1);
        assert_eq!(doc.docs[0].target, "definition:net_revenue");

        // Two tokens resolving to the same term (an alias and the term
        // itself) yield one entry, not two.
        let (mut doc2, all_definitions2, all_docs2) = full_doc_with_definitions();
        doc2.narrow_terms(
            &["nr".to_string(), "net_revenue".to_string()],
            &all_definitions2,
            &all_docs2,
        );
        assert_eq!(doc2.definitions.len(), 1);
    }

    /// ADR 0017 §3: composed with `/context/<route>`, `terms=` resolves
    /// against the **whole cell**, not the route's scope — a term whose
    /// `applies_to` names a different route must still resolve when asked
    /// for explicitly, and its page must still be inlinable.
    #[test]
    fn narrow_terms_after_narrow_to_resolves_against_the_whole_cell_not_the_route() {
        let (mut doc, all_definitions, all_docs) = full_doc_with_definitions();
        assert!(doc.narrow_to("orders_daily@2"));
        // `unrelated` applies_to `margins@2` — narrow_to alone would have
        // dropped it, but an explicit ask for it must still resolve.
        let missing = doc.narrow_terms(&["unrelated".to_string()], &all_definitions, &all_docs);
        assert!(missing.is_empty(), "{missing:?}");
        assert_eq!(doc.definitions.len(), 1);
        assert_eq!(doc.definitions[0].term, "unrelated");
    }
}
