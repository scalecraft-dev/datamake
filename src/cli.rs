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
    /// Roll back the served DATA to an earlier execution by repointing LATEST.
    /// (To roll back a version/code change, use your orchestrator's rollout undo.)
    Rollback(RollbackArgs),

    /// Deprecated alias for `release`; kept for one release.
    #[command(hide = true)]
    Publish(FileArgs),
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
}

#[derive(Args)]
pub struct ServeArgs {
    /// Path to the cell definition
    #[arg(short, long, default_value = "cell.yaml")]
    pub file: PathBuf,
    /// Binding profile to use (reads profiles/<name>.yaml)
    #[arg(short, long, default_value = "local")]
    pub profile: String,
    /// Port to bind
    #[arg(long, default_value_t = 8080)]
    pub port: u16,
    /// Seconds between LATEST-pointer checks in published-artifact mode — the
    /// staleness bound for experimental "latest" routes (ADR 0004). Ignored in
    /// direct-attach (local catalog) mode.
    #[arg(long, default_value_t = 15)]
    pub poll_interval: u64,
}
