use crate::config::Target;
use clap::builder::styling::{Effects, RgbColor, Styles};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

// Monokai palette, minus the hot pink: purple carries the headers,
// green the names, italic cyan the placeholders.
const GREEN: RgbColor = RgbColor(0xA6, 0xE2, 0x2E);
const CYAN: RgbColor = RgbColor(0x66, 0xD9, 0xEF);
const ORANGE: RgbColor = RgbColor(0xFD, 0x97, 0x1F);
const PURPLE: RgbColor = RgbColor(0xAE, 0x81, 0xFF);
const YELLOW: RgbColor = RgbColor(0xE6, 0xDB, 0x74);

// Help/error color theme. clap only emits these on a tty and honors
// NO_COLOR, so piped output stays clean.
const STYLES: Styles = Styles::styled()
    .header(PURPLE.on_default().effects(Effects::BOLD))
    .usage(PURPLE.on_default().effects(Effects::BOLD))
    .literal(GREEN.on_default().effects(Effects::BOLD))
    .placeholder(CYAN.on_default().effects(Effects::ITALIC))
    .error(PURPLE.on_default().effects(Effects::BOLD))
    .valid(GREEN.on_default().effects(Effects::BOLD))
    .invalid(ORANGE.on_default().effects(Effects::BOLD))
    .context(YELLOW.on_default())
    .context_value(CYAN.on_default());

#[derive(Parser)]
#[command(
    name = "datamk",
    version,
    about = "Manage your data products — build, verify, release, and serve",
    styles = STYLES
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Directory for run logs. Every invocation writes one plain-text log
    /// (datamk_<command>_<UTC-timestamp>.log) — the durable record of source
    /// routing, bytes scanned, staged row counts, and watermark moves that
    /// otherwise scroll past in the terminal. Defaults to .cell/logs under
    /// the cell directory.
    #[arg(long, global = true, env = "DATAMK_LOG_DIR")]
    pub log_dir: Option<PathBuf>,

    /// Keep only the newest N log files; older ones are pruned at startup.
    #[arg(long, global = true, default_value_t = 20, env = "DATAMK_LOG_KEEP")]
    pub log_keep: u32,
}

#[derive(Subcommand)]
pub enum Command {
    /// Scaffold a new cell (an implementation project)
    Init(InitArgs),
    /// Execute the transform pipeline, commit a snapshot, auto-verify (the Builder workload)
    Run(RunArgs),
    /// Discover the interface from a modeling tool's deployed state and
    /// record it (`discover:` cells, ADR 0016). Reads the tool's state store
    /// and the warehouse — read-only — and writes .cell/deployed_catalog.json,
    /// which `verify`, `context` and `serve` then read with no credentials.
    Sync(SyncArgs),
    /// Machine-verify actual output against the declared interface
    Verify(FileArgs),
    /// Pin the current snapshot as the supported contract
    Release(FileArgs),
    /// Deploy the cell as a managed workload on an orchestrator
    Deploy(DeployArgs),
    /// Serve the declared interface as REST + OpenAPI (the Server workload)
    Serve(ServeArgs),
    /// Show the published executions and the LATEST pointer (published-artifact profiles)
    Status(FileArgs),
    /// Print ready-to-run SQL that attaches the cell's catalog in DuckDB
    /// (read-only). Pipe it: duckdb -c "$(datamk attach -p prod) SELECT ..."
    Attach(AttachArgs),
    /// Emit the cell's context document (ADR 0012) — the interface made
    /// machine-readable for agents: exports, grain, schema, query grammar,
    /// and (published profiles) verified provenance. Same JSON `serve`
    /// hosts at GET /context.
    Context(ContextArgs),
    /// Mesh-level tooling (ADR 0012): emit the static manifest that tells an
    /// agent which cells exist. A document an operator hosts anywhere —
    /// never a registry service, never served by `serve`.
    Mesh(MeshArgs),
    /// Interface-authoring tooling (issue #18): import a warehouse object's
    /// types into a ready-to-edit bound export block.
    Interface(InterfaceArgs),
    /// Roll back the served DATA to an earlier execution by repointing LATEST.
    /// (To roll back a version/code change, use your orchestrator's rollout undo.)
    Rollback(RollbackArgs),

    /// Deprecated alias for `release`; kept for one release.
    #[command(hide = true)]
    Publish(FileArgs),

    /// Developer tooling — not part of the surface.
    #[command(hide = true)]
    Debug(DebugArgs),
}

#[derive(Args)]
pub struct DebugArgs {
    #[command(subcommand)]
    pub command: DebugCommand,
}

#[derive(Subcommand)]
pub enum DebugCommand {
    /// The differential check for the inline-comment extractor (ADR 0016
    /// §4): reads a JSON file `{ "<model>": { "sql": "...", "dialect":
    /// "bigquery", "expected": { "<column>": "<description>" } } }` (SQLMesh's
    /// own `column_descriptions` per model, produced next to the project),
    /// runs the extractor on each `sql`, and prints every mismatch. Exits
    /// non-zero if any. Nothing leaves the machine.
    SqlmeshComments {
        /// The JSON file described above.
        file: PathBuf,
    },
    /// Pre-install every DuckDB extension the engine may `INSTALL` at
    /// first use (ducklake, httpfs, json, postgres, sqlite, and the
    /// community `bigquery`) into this user's DuckDB extension directory —
    /// what an image build runs so a pod needs no registry egress and no
    /// exec-capable scratch filesystem at start-up.
    InstallExtensions,
}

#[derive(Args)]
pub struct SyncArgs {
    /// Path to the cell definition
    #[arg(short, long, default_value = "cell.yaml")]
    pub file: PathBuf,
    /// Binding profile to use (reads profiles/<name>.yaml) — names the
    /// state-store and warehouse connections.
    #[arg(short, long, default_value = "local")]
    pub profile: String,
    /// Read and report, but write nothing.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct AttachArgs {
    /// Path to the cell definition
    #[arg(short, long, default_value = "cell.yaml")]
    pub file: PathBuf,
    /// Binding profile to use (reads profiles/<name>.yaml)
    #[arg(short, long, default_value = "local")]
    pub profile: String,
    /// Attach a specific published execution instead of what LATEST names.
    /// Caveat: superseded artifacts survive rollbacks as immutable dead
    /// branches — pinning one can show data a rollback retired.
    #[arg(long)]
    pub execution: Option<u64>,
    /// Native-GCS-extension profiles only: fetch the resolved execution to
    /// <cell>/.cell/attach/ and print an ATTACH of that LOCAL copy. Required
    /// because a native GCS extension cannot ATTACH a remote catalog file.
    /// The copy is machine-specific and pinned — it will not track new
    /// executions; re-run to refresh. Delete .cell/attach/ to reclaim space.
    #[arg(long)]
    pub download: bool,
}

#[derive(Args)]
pub struct MeshArgs {
    #[command(subcommand)]
    pub command: MeshCommand,
}

#[derive(Subcommand)]
pub enum MeshCommand {
    /// Build the mesh manifest: name the cells (--cells file, or --store
    /// census + --url-template), fetch each cell's /context, and copy its
    /// routing summary — every field beyond {name, url} comes from the
    /// cell's own document, never typed by hand.
    Emit(MeshEmitArgs),
}

#[derive(Args)]
pub struct MeshEmitArgs {
    /// Hand-authored cells file: `cells: [{name, url, auth_hint?,
    /// bearer_env?}]`. `bearer_env` names an env var holding a token the
    /// emitter uses to fetch that cell's context — a variable NAME; no
    /// token ever appears in a file.
    #[arg(long)]
    pub cells: Option<PathBuf>,
    /// Name census over a shared parent prefix (S3/GCS only), e.g.
    /// s3://bucket/cells — lists immediate child prefixes as cell names.
    /// Cannot produce serving URLs; pair with --url-template.
    #[arg(long)]
    pub store: Option<String>,
    /// URL per cell for the census, e.g. "https://{name}.data.internal".
    #[arg(long)]
    pub url_template: Option<String>,
    /// Write the manifest to a file instead of stdout
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(Args)]
pub struct InterfaceArgs {
    #[command(subcommand)]
    pub command: InterfaceCommand,
}

#[derive(Subcommand)]
pub enum InterfaceCommand {
    /// Emit a ready-to-edit bound export block from a warehouse object's
    /// own types — never its prose. Types are safe to copy because `verify`
    /// checks them against the warehouse every run, so a stale copy is
    /// caught; descriptions have no such check, so a copy would silently
    /// rot. The warehouse's own prose already rides the bound export's
    /// columns (`from.description: "warehouse"`), live — write `description:` only when you mean
    /// something different from it.
    Import(InterfaceImportArgs),
}

#[derive(Args)]
pub struct InterfaceImportArgs {
    /// Path to the cell definition
    #[arg(short, long, default_value = "cell.yaml")]
    pub file: PathBuf,
    /// Binding profile to use (reads profiles/<name>.yaml) — the live
    /// warehouse connection the column types are read from.
    #[arg(short, long, default_value = "local")]
    pub profile: String,
    /// Which `sources:` entry to bind. Optional when the cell declares
    /// exactly one source; required (and named) otherwise.
    #[arg(long)]
    pub bind: Option<String>,
    /// The new export's name. Defaults to the bound source's own name.
    #[arg(long = "as")]
    pub as_name: Option<String>,
    /// Splice the block directly into cell.yaml's `interface:` list instead
    /// of printing it. A byte-range textual edit, never a full
    /// `serde_yaml` round-trip — that would destroy every comment already
    /// in the file, teaching ones included.
    #[arg(long)]
    pub write: bool,
    /// With --write, overwrite an existing export of the same name instead
    /// of refusing.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct ContextArgs {
    /// Path to the cell definition
    #[arg(short, long, default_value = "cell.yaml")]
    pub file: PathBuf,
    /// Binding profile to use (reads profiles/<name>.yaml)
    #[arg(short, long, default_value = "local")]
    pub profile: String,
    /// Write the document to a file instead of stdout
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Emit identity + fingerprints only — withhold docs page content (ADR
    /// 0013). Mirrors `serve --no-data`'s withholding idiom: a portable
    /// artifact inlines docs by default (a request can be repeated, a file
    /// cannot — a null-content pointer to a path the reader doesn't have is
    /// a dangling pointer), so this is a negative flag.
    #[arg(long)]
    pub no_docs: bool,
    /// Narrow the document to one export's route key (`orders_daily@2`):
    /// the same shape with `exports[]` and `docs[]` reduced to that export
    /// — the portable twin of `GET /context/<route>`.
    #[arg(long, value_name = "ROUTE")]
    pub export: Option<String>,
    /// Narrow `definitions[]`/`docs[]` to a comma-separated list of terms
    /// or aliases (ADR 0017), resolved against the whole cell and composing
    /// with `--export` exactly as `?terms=` composes with
    /// `/context/<route>`. Unlike the served door, an unknown term exits
    /// non-zero, naming the known ones — a file written by `--out` cannot
    /// be re-requested.
    #[arg(long, value_name = "TERMS", value_delimiter = ',')]
    pub terms: Option<Vec<String>>,
}

#[derive(Args)]
pub struct RollbackArgs {
    /// Path to the cell definition
    #[arg(short, long, default_value = "cell.yaml")]
    pub file: PathBuf,
    /// Binding profile to use (reads profiles/<name>.yaml)
    #[arg(short, long)]
    pub profile: String,
    /// Execution number to roll back to (default: the one before LATEST)
    #[arg(long)]
    pub execution: Option<u64>,
}

#[derive(Args)]
pub struct RunArgs {
    /// Path to the cell definition
    #[arg(short, long, default_value = "cell.yaml")]
    pub file: PathBuf,
    /// Binding profile to use (reads profiles/<name>.yaml)
    #[arg(short, long, default_value = "local")]
    pub profile: String,
    /// Published-mode compaction window in days (ADR 0004 §10): expire
    /// snapshots older than this (never pinned ones), delete data files
    /// unreferenced for at least this long, and GC superseded catalog
    /// artifacts. 0 disables compaction. Ignored in direct-attach mode.
    #[arg(long, default_value_t = 30)]
    pub retention_days: u64,
    /// Re-read every incremental source from zero and rewrite its watermark to the
    /// fresh max(cursor) at commit. The recovery path for a changed cursor, an
    /// upstream backfill behind the cursor, or a direct-attach verify failure. On a
    /// large table this is a full scan and a full bill — the schedule never does it;
    /// run it as a one-off Job. No-op on a cell with no incremental sources.
    #[arg(long)]
    pub full_refresh: bool,
    /// After transforms succeed, replay them once against the same staged delta and
    /// fail if any output table's row count or content changed. Catches transforms
    /// that duplicate (plain INSERT) an incremental source before publish. One extra
    /// local pass; the warehouse is not re-read, so it is cheap enough for CI.
    /// No-op on a cell with no incremental sources.
    #[arg(long)]
    pub verify_replay: bool,
}

#[derive(Args)]
pub struct FileArgs {
    /// Path to the cell definition
    #[arg(short, long, default_value = "cell.yaml")]
    pub file: PathBuf,
    /// Binding profile to use (reads profiles/<name>.yaml)
    #[arg(short, long, default_value = "local")]
    pub profile: String,
}

#[derive(Args)]
pub struct DeployArgs {
    /// Path to the cell definition
    #[arg(short, long, default_value = "cell.yaml")]
    pub file: PathBuf,
    /// Binding profile: reads profiles/<name>.yaml + deploy/<name>.yaml. Required —
    /// you don't deploy `local`.
    #[arg(short, long)]
    pub profile: String,
    /// Orchestrator to deploy to (overrides `target:` in deploy/<profile>.yaml)
    #[arg(long, value_enum)]
    pub target: Option<Target>,
    /// Render the target's manifests to stdout without applying
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the deploy-time init build. By default `deploy` runs `datamk run`
    /// once and waits for it, so the Server never starts against an
    /// uninitialized catalog; pass this if you drive the Builder yourself.
    #[arg(long)]
    pub skip_init: bool,
    /// Seconds to wait for the init build to complete before failing the deploy.
    #[arg(long, default_value_t = 300)]
    pub init_timeout: u64,
}

#[derive(Args)]
pub struct InitArgs {
    /// Cell name
    pub name: String,
    /// Directory to create (defaults to ./<name>)
    #[arg(short, long)]
    pub path: Option<PathBuf>,
    /// Scaffold a DISCOVERED cell instead (ADR 0016): `sqlmesh` — the
    /// interface is read from the tool's deployed state by `datamk sync`,
    /// never authored. Writes a `discover:` cell.yaml and profiles naming
    /// the state-store and warehouse connections.
    #[arg(long, value_name = "TOOL")]
    pub from: Option<String>,
}

#[derive(Args)]
pub struct ServeArgs {
    /// Path to the cell definition, or to a project file (top-level `datamk:`)
    /// that mounts several cells behind one port. With no flag: datamk.yaml in
    /// the current directory, else cell.yaml.
    // No `default_value`: "the user asked for cell.yaml" and "clap filled it
    // in" must stay distinguishable, or discovery can never prefer a project
    // file. `DeployArgs.profile` set the same precedent.
    #[arg(short, long)]
    pub file: Option<PathBuf>,
    /// Binding profile to use (reads profiles/<name>.yaml) [default: local].
    /// Serving a project, this overrides the project's `profile:` and every
    /// per-cell `profile:` — every cell is served from this one profile.
    #[arg(short, long)]
    pub profile: Option<String>,
    /// Port to bind
    #[arg(long, default_value_t = 8080)]
    pub port: u16,
    /// Seconds between LATEST-pointer checks in published-artifact mode — the
    /// staleness bound for experimental "latest" routes (ADR 0004). Ignored in
    /// direct-attach (local catalog) mode.
    #[arg(long, default_value_t = 15)]
    pub poll_interval: u64,
    /// Maximum concurrent in-flight requests; requests over the cap are shed
    /// immediately with 503 instead of queueing without bound. A global cap,
    /// not per-client fairness — put a reverse proxy in front for real rate
    /// limiting (docs/guides/serving.md).
    #[arg(long, default_value_t = crate::serve::DEFAULT_MAX_CONCURRENCY)]
    pub max_concurrency: usize,
    /// Serve the context document without mounting the data routes (ADR
    /// 0012): agents learn what exports mean, but rows never leave — for
    /// estates where consumers fetch data through existing warehouse grants.
    /// Unmounted routes return 404; the profile's `channels:` list tells
    /// callers where rows actually live. Serving a project, this applies to
    /// every mounted cell; a per-cell `no_data: true` applies to that cell
    /// alone. There is no per-cell way to turn it off.
    #[arg(long)]
    pub no_data: bool,
    /// On SIGTERM/SIGINT: stop accepting connections, finish in-flight
    /// requests for up to this many seconds, then exit 0 — the shape a
    /// rollout, scale-down, or node drain expects behind a readiness probe.
    /// Requests still open when the drain expires are dropped, with a
    /// warning. Keep it under the orchestrator's termination grace period.
    #[arg(long, default_value_t = 10, value_name = "SECONDS")]
    pub drain_timeout: u64,
}
