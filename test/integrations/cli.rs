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
            "-P",
            "missing-principals",
            "-p",
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

    // The scaffolded cell builds locally (paths resolve to the cell dir, not cwd).
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

    let _ = std::fs::remove_dir_all(&target);
}

/// A real apply (no `--dry-run`) now goes all the way to `kube::Client::try_default`
/// (ADR 0002 step 3) — this CI environment has no reachable cluster, so it must
/// still fail, but for a *cluster-connection* reason, never the old "not yet
/// implemented" stub. `KUBECONFIG` is pinned to a nonexistent path so the
/// failure mode is deterministic regardless of the runner's ambient kubeconfig
/// (which may itself be a valid-looking, merely-unreachable context).
#[test]
fn deploy_apply_attempts_cluster_without_a_dry_run() {
    let dir = fixture("orders", "deployapply");
    let out = Command::new(bin())
        .current_dir(&dir)
        .args(["deploy", "-f", "cell.yaml", "-p", "prod"])
        .env("KUBECONFIG", "/nonexistent/kubeconfig")
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
        "expected the `try_default` connection context, stderr: {err}"
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
