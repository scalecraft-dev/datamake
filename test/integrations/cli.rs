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

/// An all-bound cell (issue #6/#11): every export `bind:`s a raw-file
/// source, no `transforms:` at all — `config::builds_no_snapshot` is `true`
/// by construction. `deploy/prod.yaml` + `profiles/prod.yaml` are included
/// so both the `run`-refusal (A2) and the deploy-relax (H1) CLI-level
/// regressions can be driven against the exact same fixture shape.
fn all_bound_fixture(tag: &str) -> PathBuf {
    let dir = scratch_dir(tag);
    std::fs::create_dir_all(dir.join("deploy")).unwrap();
    std::fs::write(
        dir.join("cell.yaml"),
        "cell: virtual_only\n\
         interface:\n\
         \x20 - name: customer_pii\n\
         \x20   version: 1.0.0\n\
         \x20   description: PII rows datamk verifies but never stores.\n\
         \x20   grain: [id]\n\
         \x20   bind: raw\n\
         \x20   schema:\n\
         \x20     id: bigint\n\
         \x20     email: string\n\
         sources:\n\
         \x20 raw: ./data.csv\n\
         access:\n\
         \x20 shareable: true\n",
    )
    .unwrap();
    std::fs::write(dir.join("data.csv"), "id,email\n1,a@example.com\n").unwrap();
    std::fs::write(
        dir.join("profiles/local.yaml"),
        "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("profiles/prod.yaml"),
        "storage: s3://datamk-test/cells/virtual_only\ns3:\n  region: us-east-1\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("deploy/prod.yaml"),
        "target: kubernetes\nallow_anonymous: true\nserve: {}\n",
    )
    .unwrap();
    dir
}

/// A cell declaring a raw source and no `interface:` at all (issue #18):
/// `datamk interface import`'s starting point — nothing declared yet, one
/// unambiguous source to bind.
fn source_only_fixture(tag: &str) -> PathBuf {
    let dir = scratch_dir(tag);
    std::fs::write(
        dir.join("cell.yaml"),
        "cell: importsmoke\nsources:\n  gold_customer: ./data.csv\naccess:\n  shareable: true\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("data.csv"),
        "customer_id,credits_balance,is_active\nc1,10.5,true\nc2,20.25,false\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("profiles/local.yaml"),
        "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
    )
    .unwrap();
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

/// Issue #6/#11 (H1): `datamk deploy --dry-run` against a real all-bound
/// cell, through the actual CLI/preflight/render path — not just the pure
/// `manifests()` unit test — renders no init Job at all. Both reviewers
/// caught H1 by execution (a `<cell>-init-<hash>` Job in `--dry-run`
/// output, and `datamk run` exiting 1 on the same fixture); this pins the
/// fixed behavior at the same layer the bug was found at.
#[test]
fn deploy_dry_run_renders_no_init_job_for_an_all_bound_cell() {
    let dir = all_bound_fixture("nodryinit");
    let out = run(
        &dir,
        &["deploy", "-f", "cell.yaml", "-p", "prod", "--dry-run"],
    );
    assert!(
        out.status.success(),
        "dry-run deploy of an all-bound cell should succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("kind: ConfigMap"), "stdout: {stdout}");
    assert!(stdout.contains("kind: Deployment"), "stdout: {stdout}");
    assert!(stdout.contains("kind: Service"), "stdout: {stdout}");
    assert!(
        !stdout.contains("kind: Job"),
        "an all-bound cell has nothing to build — no init Job may be \
         rendered, or a real apply would crash-loop it: stdout: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #8: an overlay with `schedule:` and no `serve:` deploys the Builder
/// only — no Service, no Deployment — through the real CLI/preflight/render
/// path. The fixture is made `shareable: false` too, so this also pins that
/// the agnostic Server-only pre-flight (servable/auth) is skipped when no
/// Server is rendered: a compose-only cell has no HTTP surface to protect.
#[test]
fn deploy_dry_run_without_serve_renders_no_server_and_skips_server_preflight() {
    let dir = fixture("orders", "deploynoserve");
    std::fs::write(
        dir.join("deploy/prod.yaml"),
        "target: kubernetes\nschedule: \"0 * * * *\"\n",
    )
    .unwrap();
    let cell = std::fs::read_to_string(dir.join("cell.yaml")).unwrap();
    assert!(cell.contains("shareable: true"), "fixture drift: {cell}");
    std::fs::write(
        dir.join("cell.yaml"),
        cell.replace("shareable: true", "shareable: false"),
    )
    .unwrap();

    let out = run(
        &dir,
        &["deploy", "-f", "cell.yaml", "-p", "prod", "--dry-run"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(stderr.contains("preflight  ok"), "stderr: {stderr}");
    assert!(stderr.contains("builder  "), "stderr: {stderr}");
    assert!(!stderr.contains("server   "), "stderr: {stderr}");
    assert!(
        stderr.contains("this cell is built, not served"),
        "stderr: {stderr}"
    );
    assert!(stdout.contains("kind: ConfigMap"), "stdout: {stdout}");
    assert!(stdout.contains("kind: Job"), "stdout: {stdout}");
    assert!(stdout.contains("kind: CronJob"), "stdout: {stdout}");
    assert!(!stdout.contains("kind: Deployment"), "stdout: {stdout}");
    assert!(!stdout.contains("kind: Service"), "stdout: {stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #14: `serviceAccounts:` renders one ServiceAccount per identity,
/// ahead of every pod-bearing object, and the Server pod mounts the reduced
/// `<cell>-<profile>-server` Secret while Builder pods mount `<cell>-<profile>`.
#[test]
fn deploy_dry_run_renders_service_accounts_and_the_split_profile_secrets() {
    let dir = fixture("orders", "deploysa");
    std::fs::write(
        dir.join("deploy/prod.yaml"),
        "target: kubernetes\nallow_anonymous: true\nschedule: \"0 * * * *\"\nserve: {}\n\
         serviceAccounts:\n  builder: orders-builder\n  server: orders-server\n",
    )
    .unwrap();
    let out = run(
        &dir,
        &["deploy", "-f", "cell.yaml", "-p", "prod", "--dry-run"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(
        stderr.contains("rendered   ServiceAccount orders-builder"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("rendered   ServiceAccount orders-server"),
        "stderr: {stderr}"
    );
    let first_kind = stdout.find("kind: ServiceAccount").unwrap();
    let first_pod_bearer = stdout.find("kind: Job").unwrap();
    assert!(
        first_kind < first_pod_bearer,
        "accounts must precede pods: {stdout}"
    );
    assert!(
        stdout.contains("serviceAccountName: orders-builder"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("serviceAccountName: orders-server"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("secretName: orders-prod-server"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("secretName: orders-prod\n"),
        "stdout: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #8: neither `serve:` nor `schedule:` is refused with the fix named.
#[test]
fn deploy_refuses_an_overlay_with_neither_serve_nor_schedule() {
    let dir = fixture("orders", "deployneither");
    std::fs::write(
        dir.join("deploy/prod.yaml"),
        "target: kubernetes\nallow_anonymous: true\n",
    )
    .unwrap();
    let out = run(
        &dir,
        &["deploy", "-f", "cell.yaml", "-p", "prod", "--dry-run"],
    );
    assert!(!out.status.success(), "must be refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("neither `serve:` nor `schedule:`"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("serve: {}"), "stderr: {stderr}");
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

// `full_refresh_is_a_warned_noop_with_no_transforms_and_no_incremental_
// sources` (pre-binding-model) is gone, not converted: it needed a cell
// with zero transforms and zero incremental sources to reach the
// `--full-refresh` "no effect" warning — but `run` now refuses a cell with
// no materializing transforms before that warning is ever evaluated
// (binding model, issue #6: `config::builds_no_snapshot`,
// `engine::FullRefreshEffect`'s doc comment). The state this test built to
// reach is no longer reachable through `datamk run` at all, by any
// fixture. The refusal itself — the thing that replaced it — is pinned at
// the CLI layer immediately below (A2): previously covered only by a unit
// test calling `engine::run()` directly, which can't catch a future
// argument-parsing or profile-loading regression the way a real subprocess
// invocation can.

/// Issue #6/#11 (A2): the zero-transform/all-bound refusal
/// (`config::builds_no_snapshot`), driven through the real CLI binary —
/// not just `engine::run()` called directly — so a future change to `clap`
/// arg parsing or profile loading can't silently break it.
#[test]
fn run_refuses_an_all_bound_cell_with_no_materializing_transforms() {
    let dir = all_bound_fixture("norun");
    let out = run(&dir, &["run", "-f", "cell.yaml", "-p", "local"]);
    assert!(
        !out.status.success(),
        "run must refuse a cell with no materializing transforms"
    );
    let err = combined(&out);
    assert!(err.contains("no materializing transforms"), "got: {err}");
    assert!(err.contains("no snapshot to commit"), "got: {err}");
    assert!(err.contains("datamk verify"), "got: {err}");
    assert!(err.contains("datamk context"), "got: {err}");
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
/// has no matching `connections.<name>` entry, `run` must fail with the
/// existing missing-connection error — `incremental:` must not mask or
/// change that error, and no BigQuery/network access is required to prove it.
/// Since issue #14 the error is raised by `connectors::prepare` (first thing
/// `bind_sources` does, still before `BEGIN`) rather than `config::resolve`,
/// so the fixture carries a materializing transform — otherwise `run`'s
/// all-bound refusal fires first and never reaches binding.
#[test]
fn incremental_source_with_missing_connection_fails_with_the_existing_error() {
    let dir = scratch_dir("incremental_missing_conn");
    std::fs::create_dir_all(dir.join("sql")).unwrap();
    std::fs::write(dir.join("sql/stg_events.sql"), "SELECT 1 AS id\n").unwrap();
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
         transforms:\n\
        \x20 - sql/stg_events.sql\n",
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

/// `datamk context` (ADR 0012 §4): the portable emission — no server, no
/// port, no token. A direct-attach (local) profile is pinless, therefore
/// draft, with the engine-emitted note and no fabricated provenance; the
/// portable artifact carries `emitted_at` + the cell.yaml digest and never
/// claims to serve rows itself.
#[test]
fn context_emits_a_draft_document_for_a_local_cell() {
    let target = std::env::temp_dir().join(format!("datamk_it_context_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);
    let out = Command::new(bin())
        .args(["init", "ctxcell", "-p"])
        .arg(&target)
        .output()
        .expect("spawning datamk init");
    assert!(out.status.success());

    let out = run_ok(&target, &["context", "-f", "cell.yaml"]);
    let doc: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("context emits valid JSON on stdout");

    assert_eq!(doc["datamk_context"], 4);
    assert_eq!(doc["cell"], "ctxcell");
    assert_eq!(doc["status"], "draft", "pinless => draft, by definition");
    assert_eq!(doc["grain_verified"], false);
    assert!(
        doc.get("build").is_none(),
        "no fabricated provenance: {doc}"
    );
    assert_eq!(doc["data"]["served_here"], false, "a file serves no rows");
    let export = &doc["exports"][0];
    assert_eq!(export["route"], "orders_daily@2");
    assert_eq!(export["grain"], serde_json::json!(["order_date", "region"]));
    assert_eq!(export["query"]["sample_request"], "orders_daily@2?limit=10");
    assert!(doc["emitted_at"].is_string(), "{doc}");
    assert_eq!(
        doc["cell_yaml_digest"].as_str().map(str::len),
        Some(64),
        "sha256 of the emitted-from cell.yaml: {doc}"
    );

    // --out writes the same document to a file instead of stdout.
    let out_path = target.join("context.json");
    run_ok(
        &target,
        &["context", "-f", "cell.yaml", "--out", "context.json"],
    );
    let from_file: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out_path).unwrap()).unwrap();
    assert_eq!(from_file["cell"], "ctxcell");
    let _ = std::fs::remove_dir_all(&target);
}

/// `datamk mesh emit` against a live cell (ADR 0012 §6): every field beyond
/// {name, url, auth_hint} is copied from the cell's own context document —
/// description, exports summary, and the interface digest (the /context
/// ETag) — and a fetch miss leaves a bare {name, url} entry, fabricating
/// nothing.
#[test]
fn mesh_emit_copies_the_context_summary_from_a_live_cell() {
    let target = std::env::temp_dir().join(format!("datamk_it_mesh_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);
    let out = Command::new(bin())
        .args(["init", "meshcell", "-p"])
        .arg(&target)
        .output()
        .expect("spawning datamk init");
    assert!(out.status.success());
    run_ok(&target, &["run", "-f", "cell.yaml"]);

    let port = 18632u16;
    let mut serve = Command::new(bin())
        .current_dir(&target)
        .args(["serve", "-f", "cell.yaml", "--port", &port.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawning datamk serve");
    let base = format!("http://127.0.0.1:{port}");
    let mut ready = false;
    for _ in 0..50 {
        if ureq::get(&base).call().is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(ready, "serve did not become ready");

    std::fs::write(
        target.join("mesh_cells.yaml"),
        format!(
            "cells:\n  - name: meshcell\n    url: {base}\n    auth_hint: meshcell-token\n  - name: unreachable\n    url: http://127.0.0.1:1\n"
        ),
    )
    .unwrap();
    let out = run(&target, &["mesh", "emit", "--cells", "mesh_cells.yaml"]);
    let _ = serve.kill();
    let _ = serve.wait();
    assert!(
        out.status.success(),
        "mesh emit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // stdout is the manifest JSON, uncorrupted by log lines (they go to
    // stderr) — piping `datamk mesh emit | jq` must work.
    let manifest: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is pure manifest JSON");
    assert_eq!(manifest["datamk_mesh"], 1);
    let cells = manifest["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 2);

    let live = &cells[0];
    assert_eq!(live["name"], "meshcell");
    assert_eq!(
        live["description"],
        "Daily order revenue by region (demo scaffold data)"
    );
    assert_eq!(live["exports"][0]["name"], "orders_daily");
    assert_eq!(live["exports"][0]["version"], "2.1.0");
    assert_eq!(live["exports"][0]["contract"], "experimental");
    assert_eq!(
        live["context_digest"].as_str().map(str::len),
        Some(64),
        "the /context ETag digest is stamped on the entry: {live}"
    );
    assert_eq!(live["auth_hint"], "meshcell-token");

    // The unreachable cell keeps its bare {name, url} — nothing fabricated.
    let miss = &cells[1];
    assert_eq!(miss["name"], "unreachable");
    assert!(miss.get("description").is_none(), "{miss}");
    assert!(miss.get("exports").is_none(), "{miss}");
    assert!(miss.get("context_digest").is_none(), "{miss}");

    let _ = std::fs::remove_dir_all(&target);
}

// --- issue #18: `datamk interface import` ----------------------------------

/// stdout carries ONLY the YAML block (pipeable straight into `cell.yaml`);
/// every narration byte goes to stderr. Types come from the live-bound raw
/// source (no BigQuery credentials needed for this shape — DuckDB is the
/// sole authority for a raw file, correctly, not a fallback).
#[test]
fn interface_import_prints_only_yaml_to_stdout() {
    let dir = source_only_fixture("importstdout");
    let out = run_ok(
        &dir,
        &[
            "interface",
            "import",
            "-p",
            "local",
            "--as",
            "qfai_customer",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim_start().starts_with("- name: qfai_customer"),
        "{stdout}"
    );
    assert!(stdout.contains("bind: gold_customer"), "{stdout}");
    assert!(stdout.contains("customer_id: string"), "{stdout}");
    assert!(stdout.contains("# description:"), "{stdout}");
    assert!(stdout.contains("contract: experimental"), "{stdout}");
    // Never emits an actual description or the removed `unit:` field.
    assert!(!stdout.contains("unit:"), "{stdout}");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Paste this into"),
        "narration must land on stderr: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--bind` is optional when the cell declares exactly one source — the
/// fixture's sole `gold_customer` source is picked automatically.
#[test]
fn interface_import_defaults_bind_to_the_sole_source() {
    let dir = source_only_fixture("importsolesource");
    let out = run_ok(&dir, &["interface", "import", "-p", "local"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // --as omitted too: the export name defaults to the source name.
    assert!(stdout.contains("name: gold_customer"), "{stdout}");
    assert!(stdout.contains("bind: gold_customer"), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--write` splices the block directly into `cell.yaml`, and the result
/// passes `datamk verify` end to end — the whole point of emitting types
/// datamk itself just read live.
#[test]
fn interface_import_write_splices_a_verify_passing_export() {
    let dir = source_only_fixture("importwrite");
    run_ok(
        &dir,
        &[
            "interface",
            "import",
            "-p",
            "local",
            "--as",
            "qfai_customer",
            "--write",
        ],
    );
    let cell_yaml = std::fs::read_to_string(dir.join("cell.yaml")).unwrap();
    assert!(
        cell_yaml.contains("interface:\n  - name: qfai_customer"),
        "{cell_yaml}"
    );
    // The pre-existing sources:/access: content survives untouched — a
    // splice, not a serde_yaml round-trip.
    assert!(
        cell_yaml.contains("sources:\n  gold_customer: ./data.csv"),
        "{cell_yaml}"
    );

    run_ok(&dir, &["verify", "-p", "local"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Re-importing the same `--as` name without `--force` is refused; with
/// `--force` it replaces the existing entry in place (no duplicate).
#[test]
fn interface_import_refuses_a_collision_without_force_then_replaces_with_it() {
    let dir = source_only_fixture("importforce");
    run_ok(
        &dir,
        &[
            "interface",
            "import",
            "-p",
            "local",
            "--as",
            "qfai_customer",
            "--write",
        ],
    );

    let out = run(
        &dir,
        &[
            "interface",
            "import",
            "-p",
            "local",
            "--as",
            "qfai_customer",
            "--write",
        ],
    );
    assert!(!out.status.success(), "must refuse without --force");
    let err = combined(&out);
    assert!(err.contains("already exists"), "got: {err}");
    assert!(err.contains("--force"), "got: {err}");

    run_ok(
        &dir,
        &[
            "interface",
            "import",
            "-p",
            "local",
            "--as",
            "qfai_customer",
            "--write",
            "--force",
        ],
    );
    let cell_yaml = std::fs::read_to_string(dir.join("cell.yaml")).unwrap();
    assert_eq!(
        cell_yaml.matches("- name: qfai_customer").count(),
        1,
        "force must replace, never duplicate: {cell_yaml}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// An ambiguous cell (2+ sources, no `--bind`) is refused with the source
/// list named, not a confusing pick-one-for-you guess.
#[test]
fn interface_import_requires_bind_when_the_cell_has_multiple_sources() {
    let dir = scratch_dir("importambiguous");
    std::fs::write(
        dir.join("cell.yaml"),
        "cell: t\nsources:\n  a: ./a.csv\n  b: ./b.csv\naccess:\n  shareable: true\n",
    )
    .unwrap();
    std::fs::write(dir.join("a.csv"), "id\n1\n").unwrap();
    std::fs::write(dir.join("b.csv"), "id\n1\n").unwrap();
    std::fs::write(
        dir.join("profiles/local.yaml"),
        "catalog: ./.cell/catalog.ducklake\nstorage: ./.cell/data\n",
    )
    .unwrap();

    let out = run(&dir, &["interface", "import", "-p", "local"]);
    assert!(!out.status.success());
    let err = combined(&out);
    assert!(err.contains("--bind"), "got: {err}");
    assert!(err.contains('a') && err.contains('b'), "got: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

// --- ADR 0016: discovered cells over a SQLMesh state store ----------------

/// A SQLMesh state store + the deployed virtual-layer objects, as a real
/// DuckDB file, built from the checked-in fixtures (`test/fixtures/sqlmesh/
/// state/*.jsonl` — exported from a `sqlmesh init` project with prod + a
/// dev environment). Mirrors `catalog::sqlmesh::state::fixture::build_file`
/// (the binary's own tests); duplicated here because an integration test
/// only sees the binary.
fn sqlmesh_state_file(path: &Path) {
    let conn = duckdb::Connection::open(path).unwrap();
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("test/fixtures/sqlmesh/state");
    conn.execute_batch("CREATE SCHEMA sqlmesh; CREATE SCHEMA sqlmesh_example;")
        .unwrap();
    let typed = [
        ("_versions", "{schema_version: 'BIGINT', sqlglot_version: 'VARCHAR', sqlmesh_version: 'VARCHAR'}"),
        ("_environments", "{name: 'VARCHAR', snapshots: 'VARCHAR', start_at: 'VARCHAR', end_at: 'VARCHAR', plan_id: 'VARCHAR', previous_plan_id: 'VARCHAR', expiration_ts: 'BIGINT', finalized_ts: 'BIGINT', promoted_snapshot_ids: 'VARCHAR', suffix_target: 'VARCHAR', catalog_name_override: 'VARCHAR', previous_finalized_snapshots: 'VARCHAR', normalize_name: 'BOOLEAN', requirements: 'VARCHAR', gateway_managed: 'BOOLEAN'}"),
        ("_snapshots", "{name: 'VARCHAR', identifier: 'VARCHAR', version: 'VARCHAR', snapshot: 'VARCHAR', kind_name: 'VARCHAR', updated_ts: 'BIGINT', unpaused_ts: 'BIGINT', ttl_ms: 'BIGINT', unrestorable: 'BOOLEAN', forward_only: 'BOOLEAN', dev_version: 'VARCHAR', fingerprint: 'VARCHAR'}"),
        ("_intervals", "{id: 'VARCHAR', created_ts: 'BIGINT', name: 'VARCHAR', identifier: 'VARCHAR', version: 'VARCHAR', dev_version: 'VARCHAR', start_ts: 'BIGINT', end_ts: 'BIGINT', is_dev: 'BOOLEAN', is_removed: 'BOOLEAN', is_compacted: 'BOOLEAN', is_pending_restatement: 'BOOLEAN', last_altered_ts: 'BIGINT'}"),
    ];
    for (table, columns) in typed {
        conn.execute_batch(&format!(
            "CREATE TABLE sqlmesh.{table} AS SELECT * FROM read_json('{}', format = 'newline_delimited', columns = {columns});",
            base.join(format!("{table}.jsonl")).display()
        ))
        .unwrap();
    }
    let cols: Vec<serde_json::Value> =
        std::fs::read_to_string(base.join("warehouse_columns.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
    let objects: Vec<serde_json::Value> =
        std::fs::read_to_string(base.join("warehouse_objects.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    for o in &objects {
        let table = o["table"].as_str().unwrap();
        let defs: Vec<String> = cols
            .iter()
            .filter(|c| c["table"] == table)
            .map(|c| {
                format!(
                    "{} {}",
                    q(c["column"].as_str().unwrap()),
                    c["type"].as_str().unwrap()
                )
            })
            .collect();
        conn.execute_batch(&format!(
            "CREATE TABLE sqlmesh_example.{} ({});",
            q(table),
            defs.join(", ")
        ))
        .unwrap();
        for c in cols.iter().filter(|c| c["table"] == table) {
            if let Some(comment) = c["comment"].as_str() {
                conn.execute_batch(&format!(
                    "COMMENT ON COLUMN sqlmesh_example.{}.{} IS '{}';",
                    q(table),
                    q(c["column"].as_str().unwrap()),
                    comment.replace('\'', "''")
                ))
                .unwrap();
            }
        }
    }
}

/// `init --from sqlmesh` → `sync` → `status` → `context` → `serve` refusal
/// once the record is gone: the discovered-cell surface, end to end, through
/// the binary. Reads the state store and warehouse from one duckdb file;
/// nothing else is configured, which is the credential-light claim.
#[test]
fn discovered_cell_syncs_describes_and_refuses_to_serve_without_a_record() {
    let parent = scratch_dir("discover");
    let dir = parent.join("gold");
    run_ok(
        &parent,
        &[
            "init",
            "gold",
            "--from",
            "sqlmesh",
            "-p",
            dir.to_str().unwrap(),
        ],
    );
    assert!(dir.join("cell.yaml").is_file());
    assert!(
        !dir.join("sql").exists(),
        "a discovered cell scaffolds no transforms"
    );
    sqlmesh_state_file(&dir.join("state.db"));
    std::fs::write(
        dir.join("cell.yaml"),
        "cell: gold\n\
         description: The fixture project's models.\n\
         discover:\n\
         \x20 from: sqlmesh\n\
         \x20 state: sqlmesh_state\n\
         \x20 warehouse: warehouse\n\
         \x20 select:\n\
         \x20   tags: [gold]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("profiles/local.yaml"),
        "catalog: ./.cell/catalog.ducklake\n\
         storage: ./.cell/data\n\
         connections:\n\
         \x20 sqlmesh_state: { type: duckdb, path: ./state.db }\n\
         \x20 warehouse: { type: duckdb, path: ./state.db }\n",
    )
    .unwrap();

    // Dry run writes nothing and still reports.
    let out = run_ok(&dir, &["sync", "--dry-run"]);
    let text = combined(&out);
    assert!(
        text.contains("[dry run]") && text.contains("2 selected"),
        "{text}"
    );
    assert!(!dir.join(".cell/deployed_catalog.json").exists());

    let out = run_ok(&dir, &["sync"]);
    let text = combined(&out);
    assert!(
        text.contains("Discovered 6 models from sqlmesh environment 'prod'"),
        "{text}"
    );
    assert!(
        text.contains("Wrote") && text.contains("deployed_catalog.json"),
        "{text}"
    );
    assert!(dir.join(".cell/deployed_catalog.json").is_file());

    let out = run_ok(&dir, &["status"]);
    let text = combined(&out);
    assert!(
        text.contains("source: sqlmesh (environment prod)"),
        "{text}"
    );
    assert!(text.contains("exports: 2 discovered"), "{text}");

    let out = run_ok(&dir, &["context"]);
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["discovered_from"]["tool"], "sqlmesh");
    let names: Vec<&str> = doc["exports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "sqlmesh_example_documented_model",
            "sqlmesh_example_inline_comment_model"
        ]
    );
    assert_eq!(
        doc["exports"][1]["schema"]["num_orders"]["from"]["description"],
        "sqlmesh"
    );
    assert!(
        doc["exports"][0]["query"].is_null(),
        "bound exports have no data route"
    );

    // `run` refuses with the sync hint.
    let out = run(&dir, &["run"]);
    assert!(!out.status.success());
    assert!(combined(&out).contains("datamk sync"), "{}", combined(&out));

    // Without the record, `serve` refuses to start rather than serving an
    // empty interface; `context` still emits, as a draft with a note.
    std::fs::remove_file(dir.join(".cell/deployed_catalog.json")).unwrap();
    let out = run(&dir, &["serve", "--port", "0"]);
    assert!(!out.status.success(), "serve must refuse without a record");
    let text = combined(&out);
    assert!(
        text.contains("refuses to start") && text.contains("datamk sync"),
        "{text}"
    );
    let out = run_ok(&dir, &["context"]);
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["exports"], serde_json::json!([]));
    assert!(
        doc["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("datamk sync")),
        "{doc}"
    );

    let _ = std::fs::remove_dir_all(&parent);
}

/// `serve` handles SIGTERM (0.0.25): stops accepting, drains, exits 0 —
/// so a container with datamk as PID 1 stops in milliseconds instead of
/// sitting through the termination grace period and being SIGKILLed.
#[cfg(unix)]
#[test]
fn serve_exits_zero_promptly_on_sigterm() {
    use std::io::{Read, Write};
    let dir = all_bound_fixture("sigterm");
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let child = Command::new(bin())
        .current_dir(&dir)
        .args([
            "serve",
            "-f",
            "cell.yaml",
            "--port",
            &port.to_string(),
            "--drain-timeout",
            "5",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawning datamk serve");

    // Wait for the health route.
    let started = std::time::Instant::now();
    let mut healthy = false;
    while started.elapsed() < std::time::Duration::from_secs(20) {
        if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            let _ = s.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n");
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            if buf.starts_with("HTTP/1.") && buf.contains("200") {
                healthy = true;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(healthy, "serve never became healthy on port {port}");

    let sent = std::time::Instant::now();
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let out = child.wait_with_output().expect("waiting for datamk serve");
    let took = sent.elapsed();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "serve must exit 0 on SIGTERM: {out:?}\n{text}"
    );
    assert!(
        took < std::time::Duration::from_secs(5),
        "took {took:?} to stop: {text}"
    );
    assert!(text.contains("shutdown requested"), "{text}");
    assert!(
        text.contains("stopped: in-flight requests drained"),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `datamk mcp` (issue #32): JSON-RPC over stdio against a real built cell.
/// The property the protocol depends on — stdout carries nothing but one
/// JSON message per line — is asserted on every byte the process writes.
#[test]
fn mcp_speaks_json_rpc_over_stdio_and_keeps_stdout_clean() {
    use std::io::{BufRead, Write};

    let target = std::env::temp_dir().join(format!("datamk_it_mcp_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&target);
    let out = Command::new(bin())
        .args(["init", "mcpcell", "-p"])
        .arg(&target)
        .output()
        .expect("spawning datamk init");
    assert!(out.status.success());
    run_ok(&target, &["run", "-f", "cell.yaml"]);

    let mut child = Command::new(bin())
        .current_dir(&target)
        .args(["mcp", "-f", "cell.yaml"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawning datamk mcp");

    let requests = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"it","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"query_export","arguments":{"route":"orders_daily@2","filters":{"revenue":1}}}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"query_export","arguments":{"route":"orders_daily@2","limit":1}}}"#,
    ];
    {
        let mut stdin = child.stdin.take().unwrap();
        for r in requests {
            writeln!(stdin, "{r}").unwrap();
        }
        // Dropping stdin is the client's goodbye: the server drains and exits.
    }
    let status = child.wait().expect("waiting for datamk mcp");
    assert!(status.success(), "mcp exited {status}");

    let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut by_id: std::collections::HashMap<u64, serde_json::Value> = Default::default();
    for line in stdout.lines() {
        let line = line.unwrap();
        let v: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("non-JSON on stdout ({e}): {line:?}"));
        assert_eq!(v["jsonrpc"], "2.0", "{line}");
        by_id.insert(v["id"].as_u64().expect("every reply carries its id"), v);
    }
    assert_eq!(
        by_id.len(),
        4,
        "one reply per request, none for the notification"
    );

    assert_eq!(by_id[&1]["result"]["serverInfo"]["name"], "datamk");
    assert_eq!(by_id[&2]["result"]["tools"].as_array().unwrap().len(), 3);

    let bad = &by_id[&3]["result"];
    assert_eq!(bad["isError"], true);
    assert!(
        bad["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("unknown query parameter 'revenue'"),
        "{bad}"
    );

    let page = &by_id[&4]["result"]["structuredContent"];
    assert_eq!(page["row_count"], 1);
    assert_eq!(page["truncated"], true);
    assert_eq!(page["next"]["offset"], 1);
    let _ = std::fs::remove_dir_all(&target);
}
