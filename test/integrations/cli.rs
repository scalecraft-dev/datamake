//! Integration tests driving the built `datamk` binary against the fixtures in
//! this directory. Each test copies its fixture to a fresh temp dir (skipping
//! generated `.cell/`) so runs are isolated and never mutate the committed cells.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_datamk")
}

/// Copy a fixture cell to an isolated temp dir; `tag` keeps parallel tests apart.
fn fixture(name: &str, tag: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test/integrations")
        .join(name);
    let dst = std::env::temp_dir().join(format!("datamk_it_{name}_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dst);
    copy_dir(&src, &dst);
    dst
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == ".cell" {
            continue; // generated state, never copy
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawning datamk")
}

fn run_ok(dir: &Path, args: &[&str]) -> Output {
    let out = run(dir, args);
    assert!(
        out.status.success(),
        "`datamk {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// stdout+stderr concatenated. `tracing_subscriber::fmt()` writes to stdout by
/// default (see `src/main.rs`), while `anyhow`'s `Debug` chain prints to
/// stderr on a returned `Err` — tests that check for either a log line or an
/// error chain should not have to know which stream carries it.
fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A fresh, empty temp dir for tests that write a doctored `cell.yaml` /
/// `profiles/<name>.yaml` directly rather than copying a committed fixture
/// (ADR 0005 work item 5's CLI-surface tests: the malformed/typo'd shapes
/// below are deliberately never checked in as fixtures).
fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("datamk_it_scratch_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("profiles")).unwrap();
    dir
}

/// `run` writes a persistent file log under `.cell/logs/` and prints its
/// path on stderr — the two discoverability halves of the feature: the file
/// itself, and the one line that says where it went.
#[test]
fn run_writes_a_log_file_and_prints_its_path() {
    let dir = fixture("orders", "logfile");
    let out = run_ok(&dir, &["run", "-f", "cell.yaml", "-p", "local"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    let log_line = stderr
        .lines()
        .find(|l| l.starts_with("log: "))
        .unwrap_or_else(|| panic!("expected a `log:` line on stderr, got: {stderr}"));
    let printed_path = log_line.trim_start_matches("log: ").trim();
    let log_path = dir.join(printed_path.trim_start_matches("./"));
    assert!(
        log_path.exists(),
        "log file {log_path:?} (printed as {printed_path:?}) does not exist"
    );

    let name = log_path.file_name().unwrap().to_string_lossy();
    assert!(
        name.starts_with("datamk_run_") && name.ends_with(".log"),
        "unexpected log filename: {name}"
    );
    assert!(
        log_path.starts_with(dir.join(".cell/logs")),
        "log file must default under .cell/logs: {log_path:?}"
    );

    let content = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        content.contains("running pipeline") && content.contains("pipeline complete"),
        "log file missing expected narration lines: {content}"
    );
    assert!(
        !content.contains('\u{1b}'),
        "file log must carry no ANSI escapes (with_ansi(false)): {content:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `verify`/`status` never write a file log — only the four producer
/// commands do (`run`/`release`/`rollback`/`deploy`).
#[test]
fn verify_and_status_never_write_a_log_file() {
    let dir = fixture("orders", "logfile-excluded");
    run_ok(&dir, &["run", "-f", "cell.yaml", "-p", "local"]);
    let after_run = std::fs::read_dir(dir.join(".cell/logs")).unwrap().count();
    assert_eq!(
        after_run, 1,
        "run itself should have written exactly one log"
    );

    run_ok(&dir, &["verify", "-f", "cell.yaml", "-p", "local"]);
    let after_verify = std::fs::read_dir(dir.join(".cell/logs")).unwrap().count();
    assert_eq!(after_verify, 1, "verify must not add a log file");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `DATAMK_LOG=off` disables file logging outright; no `.cell/logs` at all.
#[test]
fn datamk_log_off_disables_file_logging_entirely() {
    let dir = fixture("orders", "logfile-off");
    let out = Command::new(bin())
        .current_dir(&dir)
        .args(["run", "-f", "cell.yaml", "-p", "local"])
        .env("DATAMK_LOG", "off")
        .output()
        .expect("spawning datamk");
    assert!(out.status.success(), "run should still succeed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.lines().any(|l| l.starts_with("log: ")),
        "no `log:` line expected under DATAMK_LOG=off, got: {stderr}"
    );
    assert!(
        !dir.join(".cell/logs").exists(),
        ".cell/logs must not be created under DATAMK_LOG=off"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `release` pins the supported snapshot into `.cell/published.json`.
#[test]
fn release_pins_supported_snapshot() {
    let dir = fixture("orders", "release");
    run_ok(&dir, &["run", "-f", "cell.yaml", "-p", "local"]);
    run_ok(&dir, &["release", "-f", "cell.yaml", "-p", "local"]);

    let pin = std::fs::read_to_string(dir.join(".cell/published.json")).unwrap();
    assert!(
        pin.contains("\"orders_daily@2\""),
        "pin missing route: {pin}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The deprecated `publish` alias warns on stderr but still pins.
#[test]
fn publish_alias_warns_and_still_pins() {
    let dir = fixture("orders", "publish");
    run_ok(&dir, &["run", "-f", "cell.yaml", "-p", "local"]);

    let out = run(&dir, &["publish", "-f", "cell.yaml", "-p", "local"]);
    assert!(out.status.success(), "publish alias should still succeed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("renamed to `release`"),
        "expected deprecation notice, got: {stderr}"
    );
    assert!(
        dir.join(".cell/published.json").exists(),
        "publish alias should still write the pin"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// §8 companion hardening: `serve` fails loud (non-zero, named error) when
/// `principals:` is set but the file is missing — not a silent all-deny server.
/// Only the failure path is exercised, since it exits before binding.
#[test]
fn serve_fails_loud_on_missing_principals() {
    let dir = fixture("orders-secured", "missingprinc");
    run_ok(&dir, &["run", "-f", "cell.yaml", "-p", "local"]);

    let out = run(
        &dir,
        &[
            "serve",
            "-f",
            "cell.yaml",
            "-p",
            "missing-principals",
            "--port",
            "18091",
        ],
    );
    assert!(
        !out.status.success(),
        "serve must refuse to start with a missing principals file"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("principals file"),
        "expected a principals error, got: {combined}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `deploy --dry-run -p prod` runs the full agnostic pre-flight and renders real
/// Kubernetes manifests with NO database and NO cluster. The prod profile points
/// at a Postgres catalog and S3 bucket that don't exist, so success is itself
/// proof no DB was opened.
#[test]
fn deploy_dry_run_passes_preflight_without_a_db() {
    let dir = fixture("orders", "deploydry");
    let out = run(
        &dir,
        &["deploy", "-f", "cell.yaml", "-p", "prod", "--dry-run"],
    );
    assert!(
        out.status.success(),
        "dry-run deploy should succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stderr.contains("preflight  ok"), "stderr: {stderr}");
    assert!(stderr.contains("dry run"), "stderr: {stderr}");
    // Rendered manifests go to stdout (pipeable into `kubectl apply -f -`).
    assert!(stdout.contains("kind: ConfigMap"), "stdout: {stdout}");
    assert!(stdout.contains("kind: Deployment"), "stdout: {stdout}");
    assert!(stdout.contains("kind: Service"), "stdout: {stdout}");
    // The profile/DSN is secret-grade and must never reach a rendered manifest.
    assert!(!stdout.contains("postgres://"), "stdout: {stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `deploy -p local` is refused early: local is the run/serve profile, not deployable.
#[test]
fn deploy_refuses_local_profile() {
    let dir = fixture("orders", "deploylocal");
    let out = run(&dir, &["deploy", "-f", "cell.yaml", "-p", "local"]);
    assert!(!out.status.success(), "deploy -p local must be refused");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not deployable"), "stderr: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `datamk init` scaffolds a tracked deploy overlay + a deployable prod profile,
/// references `release`/`deploy`, and the scaffolded cell runs locally.
#[test]
fn init_scaffolds_deploy_overlay_and_runnable_cell() {
    let target = std::env::temp_dir().join(format!("datamk_it_init_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);

    let out = Command::new(bin())
        .args(["init", "mycell", "-p"])
        .arg(&target)
        .output()
        .expect("spawning datamk init");
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(target.join("deploy/prod.yaml").exists(), "deploy/prod.yaml");
    assert!(
        target.join("profiles/prod.yaml").exists(),
        "profiles/prod.yaml"
    );
    let deploy = std::fs::read_to_string(target.join("deploy/prod.yaml")).unwrap();
    assert!(deploy.contains("target: kubernetes"), "{deploy}");
    let readme = std::fs::read_to_string(target.join("README.md")).unwrap();
    assert!(readme.contains("datamk release"), "README: {readme}");
    assert!(readme.contains("datamk deploy"), "README: {readme}");
    let gitignore = std::fs::read_to_string(target.join(".gitignore")).unwrap();
    assert!(gitignore.contains("deploy/ is tracked"), "{gitignore}");

    // The scaffold is 100% declarative (ADR 0008: one language — every sql/
    // file is a bare SELECT; every table-building decision lives in
    // cell.yaml, never hand-written DDL). stg_orders.sql and orders_daily.sql
    // are bare-path entries (`materialize: replace` implied — full rebuild
    // each run, legal here: no incremental source); order_totals.sql is an
    // explicit `materialize: upsert` mapping (an accumulator, demonstrating
    // the other shape) alongside them.
    let cell_yaml = std::fs::read_to_string(target.join("cell.yaml")).unwrap();
    assert!(cell_yaml.contains("materialize: upsert"), "{cell_yaml}");
    assert!(cell_yaml.contains("sql/stg_orders.sql"), "{cell_yaml}");
    assert!(cell_yaml.contains("sql/orders_daily.sql"), "{cell_yaml}");
    assert!(cell_yaml.contains("sql/order_totals.sql"), "{cell_yaml}");
    // Only the live `transforms:` block matters here — the commented-out
    // `sources:` prose above it mentions `materialize: replace` by name as
    // documentation, which is fine; it's not a `transforms:` entry.
    let transforms_block = cell_yaml
        .split("transforms:")
        .nth(1)
        .and_then(|s| s.split("interface:").next())
        .unwrap_or_default();
    assert!(
        !transforms_block.contains("materialize: replace"),
        "stg_orders.sql/orders_daily.sql are bare paths now — replace is implied, not spelled \
         out in the transforms: block: {transforms_block}"
    );
    for f in ["stg_orders.sql", "order_totals.sql", "orders_daily.sql"] {
        let path = target.join("sql").join(f);
        assert!(path.exists(), "sql/{f}");
        let sql = std::fs::read_to_string(&path).unwrap();
        // Strip `--` comment lines before checking — the comments legitimately
        // explain what cell.yaml's `materialize:` wraps this SELECT in, which
        // mentions CREATE OR REPLACE by name; the code itself must not.
        let code: String = sql
            .lines()
            .filter(|line| !line.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.to_uppercase().contains("CREATE"),
            "sql/{f} must be SELECT-only, zero hand-written DDL — got code: {code}"
        );
    }

    // The scaffolded cell builds locally (paths resolve to the cell dir, not
    // cwd) — and it exercises every declarative entry for real: `run`
    // composes and executes the staging/bootstrap/strategy DML (or the
    // single `replace` statement) over each of the three sql/ files.
    let run = Command::new(bin())
        .arg("run")
        .arg("-f")
        .arg(target.join("cell.yaml"))
        .args(["-p", "local"])
        .output()
        .expect("spawning datamk run");
    assert!(
        run.status.success(),
        "scaffolded cell failed to run: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    // A second run — replay-safety by construction (ADR 0008 §3): the
    // declarative accumulator must not error, and must not duplicate rows,
    // on an identical re-delivery of the same synthesized demo data.
    let run2 = Command::new(bin())
        .arg("run")
        .arg("-f")
        .arg(target.join("cell.yaml"))
        .args(["-p", "local"])
        .output()
        .expect("spawning datamk run (second run)");
    assert!(
        run2.status.success(),
        "scaffolded cell's declarative entry failed on a second run (idempotent re-delivery): {}",
        String::from_utf8_lossy(&run2.stderr)
    );

    // `datamk verify` must pass against the built snapshot too — the whole
    // interface, including the `orders_daily` export (sourced from a bare-path
    // replace rollup), unaffected by the upsert entry interleaved ahead of it.
    let verify = Command::new(bin())
        .arg("verify")
        .arg("-f")
        .arg(target.join("cell.yaml"))
        .args(["-p", "local"])
        .output()
        .expect("spawning datamk verify");
    assert!(
        verify.status.success(),
        "scaffolded cell failed to verify: {}",
        String::from_utf8_lossy(&verify.stderr)
    );

    let _ = std::fs::remove_dir_all(&target);
}

/// A real apply (no `--dry-run`) goes all the way to `kube::Client::try_default`
/// (ADR 0002 step 3) — this CI environment has no reachable cluster, so it must
/// still fail, but for a *cluster-connection* reason, never the old "not yet
/// implemented" stub. Along the way, the ADR 0004 §3 host-side conditional-PUT
/// probe must NOT hard-fail the deploy just because the fixture's bucket is
/// unreachable from this host — unreachability defers to the in-pod probe
/// (`engine::run` runs it; the init Job surfaces failures with build logs).
/// `KUBECONFIG` is pinned to a nonexistent path so the failure mode is
/// deterministic regardless of the runner's ambient kubeconfig.
#[test]
fn deploy_apply_attempts_cluster_and_defers_unreachable_probe() {
    let dir = fixture("orders", "deployapply");
    let out = Command::new(bin())
        .current_dir(&dir)
        .args(["deploy", "-f", "cell.yaml", "-p", "prod"])
        .env("KUBECONFIG", "/nonexistent/kubeconfig")
        // Pin the AWS env so the probe fails deterministically on credentials/
        // reachability regardless of the runner's ambient identity.
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_PROFILE")
        .output()
        .expect("spawning datamk deploy");
    assert!(
        !out.status.success(),
        "real apply should fail with no reachable cluster"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("not yet implemented"),
        "the old ADR 0002 stub message must be gone: {err}"
    );
    assert!(
        err.contains("Kubernetes cluster"),
        "expected the `try_default` connection context (probe unreachability must \
         defer, not fail): {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--dry-run` never constructs a `kube::Client` (ADR 0002 §2): with an
/// unreachable/nonexistent `KUBECONFIG`, a dry-run deploy must still succeed
/// and print manifests, proving it never tried to connect.
#[test]
fn deploy_dry_run_never_contacts_a_cluster() {
    let dir = fixture("orders", "deploydryoffline");
    let out = Command::new(bin())
        .current_dir(&dir)
        .args(["deploy", "-f", "cell.yaml", "-p", "prod", "--dry-run"])
        .env("KUBECONFIG", "/nonexistent/kubeconfig")
        .output()
        .expect("spawning datamk deploy --dry-run");
    assert!(
        out.status.success(),
        "dry-run must succeed with no reachable cluster: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("kind: ConfigMap"), "stdout: {stdout}");
    assert!(stdout.contains("kind: Deployment"), "stdout: {stdout}");
    assert!(stdout.contains("kind: Service"), "stdout: {stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

// --- ADR 0005 (incremental source loading): CLI-surface tests -------------
//
// Incremental applies only to `connection` sources, and the only connector is
// BigQuery, so a genuine two-execution incremental run (bootstrap -> delta)
// cannot be driven through the CLI locally; that lives behind the
// credential-gated warehouse test and the kind/MinIO e2e harness (see
// test/integrations/kind_e2e/README.md). What IS locally testable — and
// exercised here — is the flag surface: the no-op warnings, `--help` text,
// and that the two Stage-1 config errors (missing connection, malformed
// `incremental:` block) actually reach a user running `datamk run`, not just
// the `src/config` unit tests.

/// The true `--full-refresh` no-op needs a cell with **zero transforms and
/// zero incremental sources** (ADR 0008: every transform is declarative by
/// construction now, so a cell with any `transforms:` entry always has
/// something for `--full-refresh` to rebuild — see
/// `full_refresh_rebuilds_declarative_tables_with_no_incremental_source_present`
/// below). `orders` used to be this fixture back when a bare-path entry was
/// a raw transform excluded from the declarative count; it no longer is,
/// so this test builds its own empty cell instead of reusing `orders`.
#[test]
fn full_refresh_is_a_warned_noop_with_no_transforms_and_no_incremental_sources() {
    let dir = scratch_dir("fullrefreshnoop");
    std::fs::write(dir.join("cell.yaml"), "cell: empty\ninterface: []\n").unwrap();
    std::fs::write(
        dir.join("profiles/local.yaml"),
        "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
    )
    .unwrap();
    let out = run_ok(
        &dir,
        &["run", "-f", "cell.yaml", "-p", "local", "--full-refresh"],
    );
    let log = combined(&out);
    assert!(
        log.contains("--full-refresh has no effect: this cell declares no incremental sources"),
        "expected the no-op warning, got: {log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR 0008 §6: the `init` scaffold has a declarative `materialize:` entry
/// but no incremental *source* at all (synthesized local demo data) — the
/// third `--full-refresh` state. The stale "no effect" warning would lie
/// here (a real rebuild happens); the engine must say what actually runs
/// instead.
#[test]
fn full_refresh_rebuilds_declarative_tables_with_no_incremental_source_present() {
    let target = std::env::temp_dir().join(format!(
        "datamk_it_fullrefresh_declarative_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&target);
    let init = Command::new(bin())
        .args(["init", "mycell", "-p"])
        .arg(&target)
        .output()
        .expect("spawning datamk init");
    assert!(init.status.success(), "{}", combined(&init));

    let out = run_ok(
        &target,
        &["run", "-f", "cell.yaml", "-p", "local", "--full-refresh"],
    );
    let log = combined(&out);
    // Three transform entries now (ADR 0008: one language — stg_orders/
    // orders_daily are bare paths, `replace` implied; order_totals is an
    // explicit `materialize: upsert` mapping) — all three count towards the
    // notice, regardless of strategy or syntax.
    assert!(
        log.contains(
            "full refresh: rebuilding 3 declarative table(s) from scratch; no incremental \
             watermarks to rewind"
        ),
        "expected the declarative-rebuild notice, got: {log}"
    );
    assert!(
        !log.contains("--full-refresh has no effect"),
        "must not claim no effect when a declarative table exists to rebuild: {log}"
    );

    let _ = std::fs::remove_dir_all(&target);
}

/// Same shape as above for `--verify-replay`: no incremental sources means
/// nothing to replay, and the engine says so rather than silently skipping.
#[test]
fn verify_replay_is_a_warned_noop_without_incremental_sources() {
    let dir = fixture("orders", "verifyreplaynoop");
    let out = run_ok(
        &dir,
        &["run", "-f", "cell.yaml", "-p", "local", "--verify-replay"],
    );
    let log = combined(&out);
    assert!(
        log.contains("--verify-replay has no effect: this cell declares no incremental sources"),
        "expected the no-op warning, got: {log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `datamk run --help` documents both ADR 0005 flags with the key phrases an
/// operator needs (what each does, that both no-op cleanly without
/// incremental sources).
#[test]
fn run_help_documents_full_refresh_and_verify_replay() {
    let out = Command::new(bin())
        .args(["run", "--help"])
        .output()
        .expect("spawning datamk run --help");
    assert!(out.status.success(), "run --help must succeed");
    let help = combined(&out);
    assert!(help.contains("--full-refresh"), "help: {help}");
    assert!(
        help.contains("rewrite its watermark"),
        "expected --full-refresh's watermark-rewrite phrase, got: {help}"
    );
    assert!(help.contains("--verify-replay"), "help: {help}");
    assert!(
        help.contains("replay them once against the same staged delta"),
        "expected --verify-replay's replay phrase, got: {help}"
    );
    assert!(
        help.contains("No-op on a cell with no incremental sources"),
        "both flags document the no-op case, got: {help}"
    );
}

/// A `connection` source with an `incremental:` block still goes through the
/// same profile-resolution path as a plain connection source: if the profile
/// has no matching `connections.<name>` entry, resolution must fail with the
/// existing missing-connection error — `incremental:` must not mask or
/// change that error, and no BigQuery/network access is required to prove it
/// (resolution fails in `config::resolve`, before any DB is opened).
#[test]
fn incremental_source_with_missing_connection_fails_with_the_existing_error() {
    let dir = scratch_dir("incremental_missing_conn");
    std::fs::write(
        dir.join("cell.yaml"),
        "cell: incremental_missing_conn\n\
         \n\
         sources:\n\
        \x20 events:\n\
        \x20   connection: crm\n\
        \x20   table: analytics.events\n\
        \x20   incremental:\n\
        \x20     cursor: updated_at\n\
         \n\
         transforms: []\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("profiles/local.yaml"),
        "storage: ./.cell/data\ncatalog: ./.cell/catalog.ducklake\n",
    )
    .unwrap();

    let out = run(&dir, &["run", "-f", "cell.yaml", "-p", "local"]);
    assert!(
        !out.status.success(),
        "run must fail when the profile has no matching connection"
    );
    let err = combined(&out);
    assert!(
        err.contains(
            "source 'events' uses connection 'crm', but the profile has no \
             `connections.crm` entry"
        ),
        "expected the existing missing-connection error, got: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A typo'd `incremenetal:` key (ADR 0005 §1's motivating hazard) must not
/// silently deserialize as a plain connection source running full scans
/// forever — it must fail `datamk run` with the Stage-1 schema error, and
/// that failure must actually reach the CLI's stderr/exit code, not just the
/// `src/config/schema.rs` unit tests.
#[test]
fn malformed_incremental_block_typo_fails_datamk_run_with_the_stage1_error() {
    let dir = scratch_dir("incremental_typo");
    std::fs::write(
        dir.join("cell.yaml"),
        "cell: incremental_typo\n\
         \n\
         sources:\n\
        \x20 events:\n\
        \x20   connection: crm\n\
        \x20   table: analytics.events\n\
        \x20   incremenetal:\n\
        \x20     cursor: updated_at\n\
         \n\
         transforms: []\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("profiles/local.yaml"),
        "storage: ./.cell/data\ncatalog: ./.cell/catalog.ducklake\n",
    )
    .unwrap();

    let out = run(&dir, &["run", "-f", "cell.yaml", "-p", "local"]);
    assert!(
        !out.status.success(),
        "run must fail on a typo'd `incremenetal:` key"
    );
    let err = combined(&out);
    assert!(
        err.contains("parsing cell definition"),
        "expected the CellDef::load context, got: {err}"
    );
    assert!(
        err.contains(
            "unknown field `incremenetal` — a connection source has `connection`, one of \
             `table`/`query`, and optional `incremental`"
        ),
        "expected the Stage-1 unknown-field error text, got: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `attach --download` is a native-GCS-extension-only escape hatch; every
/// other profile shape attaches its catalog directly and must reject the flag.
#[test]
fn attach_download_is_rejected_outside_native_gcs_profiles() {
    let dir = fixture("orders", "attachdl");
    let out = run(
        &dir,
        &["attach", "-f", "cell.yaml", "-p", "local", "--download"],
    );
    assert!(
        !out.status.success(),
        "--download on a direct-attach profile must be refused"
    );
    let err = combined(&out);
    assert!(
        err.contains("--download only applies to native-GCS-extension profiles"),
        "stderr: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `attach --help` documents the native-GCS `--download` contract: local,
/// machine-specific, pinned copy.
#[test]
fn attach_help_documents_download() {
    let out = Command::new(bin())
        .args(["attach", "--help"])
        .output()
        .expect("spawning datamk attach --help");
    assert!(out.status.success(), "attach --help must succeed");
    let help = combined(&out);
    assert!(help.contains("--download"), "help: {help}");
    assert!(
        help.contains("cannot ATTACH a remote catalog file"),
        "expected the native-extension rationale, got: {help}"
    );
    assert!(
        help.contains("machine-specific"),
        "expected the locality caveat, got: {help}"
    );
}
