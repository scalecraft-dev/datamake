//! Persistent per-invocation run logs: a second `tracing_subscriber` fmt
//! layer, alongside the console, for the commands that produce a durable
//! change and are worth a durable record of what happened.
//!
//! **Which commands log to file** (`command_name`): `run`, `release`,
//! `rollback`, `deploy` — producers only. Deliberately excluded:
//! `status`/`verify`/`init`/`attach` (queries or scaffolding — `status`
//! specifically often runs in watch loops, where "keep the newest N" is not
//! a license to generate spray). `serve` is deferred: a long-lived process
//! needs a rolling-file design (size/time-based rotation), not the
//! one-file-per-invocation shape this module implements.
//!
//! **Filter policy**: both sinks start from the same base directives
//! (`RUST_LOG` if set, else the project default) but the file sink appends
//! the credential-narrating pins (`CREDENTIAL_NARRATING_TARGETS`) *last* —
//! `EnvFilter` is last-directive-wins per target, so those pins are
//! unlowerable for the file even under a hostile
//! `RUST_LOG=aws_config=debug`. The console keeps today's unmodified
//! semantics; a human debugging a credential problem locally can still ask
//! for `aws_config` at `debug` on their ephemeral terminal, but the durable
//! file on disk never carries an access key id.
//!
//! **Writer**: a plain, blocking `std::fs::File` — `tracing_subscriber`
//! implements `MakeWriter` for `File` directly. Deliberately *not*
//! `tracing_appender::non_blocking`: these are one-shot commands, and a
//! dropped `WorkerGuard` before the final flush would silently lose the
//! last lines (e.g. "published execution N") — the exact failure mode a
//! run log exists to prevent.

use anyhow::{Context, Result};
use std::fs::File;
use std::path::{Path, PathBuf};
use tracing_subscriber::{prelude::*, EnvFilter, Registry};

use crate::cli::Command;
use crate::timeutil::{filename_utc, unix_now};

/// Credential-narrating targets, pinned to `warn` for the file sink only.
/// `aws_config` narrates every credential-chain resolution — including
/// access key ids — at `info`; today's console default already silences it
/// (`main.rs`'s prior `"info,aws_config=warn"`), and the file sink must
/// never be able to lose that pin, not even to an operator's own `RUST_LOG`.
pub const CREDENTIAL_NARRATING_TARGETS: &str = "aws_config=warn";

/// The base directive string both sinks start from.
pub fn base_directives() -> String {
    std::env::var("RUST_LOG").unwrap_or_else(|_| "info,aws_config=warn".to_string())
}

/// The file sink's directive string: `base`, then the credential pins
/// appended last so they win regardless of what `base` itself says about
/// those same targets.
pub fn file_filter_directives(base: &str) -> String {
    format!("{base},{CREDENTIAL_NARRATING_TARGETS}")
}

/// `DATAMK_LOG=off` disables file logging outright — the escape hatch for a
/// read-only or ephemeral filesystem. Env-only, deliberately not a CLI
/// flag: the deployed image bakes this in (see `Dockerfile`) rather than
/// every producer command's Args growing a `--no-log-file`.
pub fn disabled_by_env() -> bool {
    std::env::var("DATAMK_LOG").is_ok_and(|v| v.eq_ignore_ascii_case("off"))
}

/// The file-log command name for `command`, `None` for everything that
/// doesn't write one. See the module doc for which and why.
pub fn command_name(command: &Command) -> Option<&'static str> {
    match command {
        Command::Run(_) => Some("run"),
        Command::Release(_) => Some("release"),
        Command::Rollback(_) => Some("rollback"),
        Command::Deploy(_) => Some("deploy"),
        _ => None,
    }
}

/// The `-f`/`--file` cell path for a logging-eligible command. Kept
/// separate from `command_name` (rather than one `Option<(name, &Path)>`)
/// so "is this a logging command" reads at call sites that don't need the
/// path.
fn command_file(command: &Command) -> Option<&Path> {
    match command {
        Command::Run(a) => Some(&a.file),
        Command::Release(a) => Some(&a.file),
        Command::Rollback(a) => Some(&a.file),
        Command::Deploy(a) => Some(&a.file),
        _ => None,
    }
}

/// The effective log directory: `--log-dir`/`DATAMK_LOG_DIR` if given, else
/// `.cell/logs` under the cell directory — the existing gitignored
/// workspace dir, zero new ignore rules.
pub fn log_dir(explicit: Option<&Path>, cell_file: &Path) -> PathBuf {
    match explicit {
        Some(d) => d.to_path_buf(),
        None => crate::config::cell_dir(cell_file)
            .join(".cell")
            .join("logs"),
    }
}

/// `datamk_<command>_<UTC-timestamp>.log`, probing for a same-second
/// collision by suffixing `-2`, `-3`, … — two invocations landing in the
/// same wall-clock second (a script looping `datamk run`) must never
/// clobber each other's log.
pub fn log_file_path(dir: &Path, command: &str, now_unix: i64) -> PathBuf {
    let stamp = filename_utc(now_unix);
    let base = format!("datamk_{command}_{stamp}");
    let plain = dir.join(format!("{base}.log"));
    if !plain.exists() {
        return plain;
    }
    for n in 2u32.. {
        let candidate = dir.join(format!("{base}-{n}.log"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("u32 exhausted probing for a free log filename")
}

/// Prune `dir` to the newest `keep` `datamk_*.log` files — glob-scoped, so
/// a `--log-dir` repointed at a shared or pre-existing directory never
/// touches a file this module didn't write. Best-effort: called before
/// this run's own log file is created (so it's never among the
/// candidates), and a failure here must not block the command it's merely
/// tidying up after.
pub fn prune(dir: &Path, keep: u32) -> Result<()> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading log dir {}", dir.display())),
    };

    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in read_dir {
        let entry = entry.with_context(|| format!("reading log dir {}", dir.display()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("datamk_") && name.ends_with(".log")) {
            continue; // never delete-all-but-N of arbitrary files
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        candidates.push((modified, entry.path()));
    }
    candidates.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified)); // newest first
    for (_, path) in candidates.into_iter().skip(keep as usize) {
        let _ = std::fs::remove_file(&path); // best-effort; a stale file next run is harmless
    }
    Ok(())
}

/// Resolve, create, and prune the log directory, then open this run's log
/// file. `Ok(None)` for a non-logging command or `DATAMK_LOG=off` (not an
/// error — the caller just proceeds console-only); `Err` only wraps a
/// genuine I/O failure, and every caller in this module treats even that as
/// a warning, never a command failure — a log is narration, not truth.
fn open_file_log(
    command: &Command,
    log_dir_arg: Option<&Path>,
    keep: u32,
) -> Option<(PathBuf, File)> {
    let name = command_name(command)?;
    if disabled_by_env() {
        return None;
    }
    let cell_file = command_file(command)?;
    let dir = log_dir(log_dir_arg, cell_file);

    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "warning: could not create log dir {} ({e}) — continuing without a file log",
            dir.display()
        );
        return None;
    }
    if let Err(e) = prune(&dir, keep) {
        eprintln!("warning: pruning log dir {} failed: {e}", dir.display());
    }

    let path = log_file_path(&dir, name, unix_now());
    match File::create(&path) {
        Ok(file) => Some((path, file)),
        Err(e) => {
            eprintln!(
                "warning: could not open log file {} ({e}) — continuing without a file log",
                path.display()
            );
            None
        }
    }
}

/// Install the global `tracing` subscriber exactly once — a console fmt
/// layer always, plus a file fmt layer when `command` is a logging command
/// with file logging available (see `open_file_log`). Returns the log
/// file's path when one was opened, for `main`'s success/failure
/// discoverability lines (`log: <path>` / `See the full run log: <path>`).
pub fn init(command: &Command, log_dir_arg: Option<&Path>, log_keep: u32) -> Option<PathBuf> {
    let console_layer =
        tracing_subscriber::fmt::layer().with_filter(EnvFilter::new(base_directives()));

    match open_file_log(command, log_dir_arg, log_keep) {
        Some((path, file)) => {
            let file_layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file)
                .with_filter(EnvFilter::new(file_filter_directives(&base_directives())));
            Registry::default()
                .with(console_layer)
                .with(file_layer)
                .init();
            Some(path)
        }
        None => {
            Registry::default().with(console_layer).init();
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // --- filename / collision -----------------------------------------

    #[test]
    fn log_file_path_has_the_documented_shape() {
        let dir = std::env::temp_dir().join("datamk_logging_test_shape");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = log_file_path(&dir, "run", 0);
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "datamk_run_1970-01-01T00-00-00Z.log"
        );
    }

    #[test]
    fn log_file_path_suffixes_on_a_same_second_collision() {
        let dir = std::env::temp_dir().join("datamk_logging_test_collision");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let now = 1_751_500_680;

        let first = log_file_path(&dir, "release", now);
        std::fs::write(&first, b"").unwrap();
        let second = log_file_path(&dir, "release", now);
        assert_ne!(first, second, "a second same-second call must not collide");
        assert!(
            second
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("-2.log"),
            "got: {second:?}"
        );
        std::fs::write(&second, b"").unwrap();
        let third = log_file_path(&dir, "release", now);
        assert!(
            third
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("-3.log"),
            "got: {third:?}"
        );
    }

    // --- prune: glob-scoped, never touches foreign files ---------------

    #[test]
    fn prune_keeps_only_the_newest_n_datamk_logs_and_never_touches_foreign_files() {
        let dir = std::env::temp_dir().join("datamk_logging_test_prune");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A foreign file that happens to share the directory (the
        // `--log-dir` repointed-at-a-shared-directory case).
        std::fs::write(dir.join("other.log"), b"not ours").unwrap();
        std::fs::write(dir.join("datamk_run_notes.txt"), b"not a .log").unwrap();

        for i in 0..5 {
            let name = format!("datamk_run_1970-01-01T00-00-0{i}Z.log");
            std::fs::write(dir.join(name), b"x").unwrap();
            // Force distinct mtimes so "newest" is deterministic even on
            // filesystems with coarse mtime resolution.
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        prune(&dir, 2).unwrap();

        let remaining: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();

        assert!(
            remaining.contains(&"other.log".to_string()),
            "a foreign .log file must survive pruning: {remaining:?}"
        );
        assert!(
            remaining.contains(&"datamk_run_notes.txt".to_string()),
            "a non-.log datamk_ file must survive pruning: {remaining:?}"
        );
        let datamk_logs: Vec<&String> = remaining
            .iter()
            .filter(|n| n.starts_with("datamk_run_1970"))
            .collect();
        assert_eq!(
            datamk_logs.len(),
            2,
            "expected exactly 2 datamk_*.log files kept: {remaining:?}"
        );
    }

    #[test]
    fn prune_of_a_nonexistent_dir_is_a_silent_noop() {
        let dir = std::env::temp_dir().join("datamk_logging_test_prune_missing_xyz");
        let _ = std::fs::remove_dir_all(&dir);
        prune(&dir, 5).unwrap();
    }

    // --- command eligibility --------------------------------------------

    #[test]
    fn only_the_four_producer_commands_log_to_file() {
        use crate::cli::{AttachArgs, DeployArgs, FileArgs, RollbackArgs, RunArgs, ServeArgs};
        let run = Command::Run(RunArgs {
            file: "cell.yaml".into(),
            profile: "local".into(),
            retention_days: 30,
            full_refresh: false,
            verify_replay: false,
        });
        let deploy = Command::Deploy(DeployArgs {
            file: "cell.yaml".into(),
            profile: "prod".into(),
            target: None,
            dry_run: false,
            skip_init: false,
            init_timeout: 300,
        });
        let rollback = Command::Rollback(RollbackArgs {
            file: "cell.yaml".into(),
            profile: "prod".into(),
            execution: None,
        });
        let release = Command::Release(FileArgs {
            file: "cell.yaml".into(),
            profile: "local".into(),
        });
        let verify = Command::Verify(FileArgs {
            file: "cell.yaml".into(),
            profile: "local".into(),
        });
        let status = Command::Status(FileArgs {
            file: "cell.yaml".into(),
            profile: "local".into(),
        });
        let attach = Command::Attach(AttachArgs {
            file: "cell.yaml".into(),
            profile: "local".into(),
            execution: None,
            download: false,
        });
        let serve = Command::Serve(ServeArgs {
            file: "cell.yaml".into(),
            profile: "local".into(),
            port: 8080,
            poll_interval: 15,
        });

        assert_eq!(command_name(&run), Some("run"));
        assert_eq!(command_name(&deploy), Some("deploy"));
        assert_eq!(command_name(&rollback), Some("rollback"));
        assert_eq!(command_name(&release), Some("release"));
        assert_eq!(command_name(&verify), None, "verify must not log to file");
        assert_eq!(command_name(&status), None, "status must not log to file");
        assert_eq!(command_name(&attach), None, "attach must not log to file");
        assert_eq!(command_name(&serve), None, "serve is deferred");
    }

    #[test]
    fn disabled_by_env_reads_datamk_log_off_case_insensitively() {
        // Nothing else in this binary reads `DATAMK_LOG`, so mutating the
        // real var here is safe against the multi-threaded test runner.
        std::env::remove_var("DATAMK_LOG");
        assert!(!disabled_by_env());
        std::env::set_var("DATAMK_LOG", "OFF");
        assert!(disabled_by_env());
        std::env::set_var("DATAMK_LOG", "off");
        assert!(disabled_by_env());
        std::env::set_var("DATAMK_LOG", "on");
        assert!(!disabled_by_env());
        std::env::remove_var("DATAMK_LOG");
    }

    // --- log dir resolution ---------------------------------------------

    #[test]
    fn log_dir_defaults_to_dot_cell_logs_under_the_cell_dir() {
        assert_eq!(
            log_dir(None, Path::new("/cell/cell.yaml")),
            PathBuf::from("/cell/.cell/logs")
        );
        assert_eq!(
            log_dir(None, Path::new("cell.yaml")),
            PathBuf::from("./.cell/logs")
        );
    }

    #[test]
    fn log_dir_prefers_the_explicit_override() {
        assert_eq!(
            log_dir(
                Some(Path::new("/var/log/datamk")),
                Path::new("/cell/cell.yaml")
            ),
            PathBuf::from("/var/log/datamk")
        );
    }

    // --- filter directives -------------------------------------------

    #[test]
    fn file_filter_directives_appends_the_credential_pins_last() {
        assert_eq!(file_filter_directives("info"), "info,aws_config=warn");
        assert_eq!(
            file_filter_directives("debug,aws_config=debug"),
            "debug,aws_config=debug,aws_config=warn"
        );
    }

    /// The load-bearing test: build the FILE filter from a hostile
    /// `RUST_LOG=debug,aws_config=debug` and prove the pin still wins for
    /// the file sink — an `aws_config` INFO event must be disabled, a
    /// `datamk`-targeted INFO event must still pass.
    #[test]
    fn file_filter_pins_aws_config_to_warn_even_under_a_hostile_rust_log() {
        let directives = file_filter_directives("debug,aws_config=debug");

        assert!(
            !event_passes_filter(&directives, "aws_config", tracing::Level::INFO),
            "aws_config INFO must be filtered out of the file sink even when RUST_LOG asks for \
             aws_config=debug"
        );
        assert!(
            event_passes_filter(&directives, "datamk_probe", tracing::Level::INFO),
            "a non-credential target's INFO event must still pass the file filter"
        );
        // And the hostile override IS honored for a non-credential target
        // at debug, proving the filter isn't just globally stuck at info.
        assert!(
            event_passes_filter(&directives, "datamk_probe", tracing::Level::DEBUG),
            "the base RUST_LOG=debug must still apply to non-pinned targets"
        );
    }

    /// A `Layer` that just flips a flag when it observes an event — the
    /// probe for whether an `EnvFilter`-gated layer let an event through.
    struct FlagLayer(Arc<AtomicBool>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for FlagLayer {
        fn on_event(
            &self,
            _event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// `tracing`'s `target:` argument must be a string *literal* — it's
    /// baked into the callsite's static metadata — so this can't take
    /// `target`/`level` and forward them into one macro call; it matches
    /// onto the handful of (target, level) pairs the test above actually
    /// needs.
    fn event_passes_filter(directives: &str, target: &str, level: tracing::Level) -> bool {
        let seen = Arc::new(AtomicBool::new(false));
        let filter = EnvFilter::new(directives);
        let subscriber = Registry::default().with(FlagLayer(seen.clone()).with_filter(filter));
        tracing::subscriber::with_default(subscriber, || match (target, level) {
            ("aws_config", tracing::Level::INFO) => tracing::info!(target: "aws_config", "x"),
            ("aws_config", tracing::Level::DEBUG) => tracing::debug!(target: "aws_config", "x"),
            ("datamk_probe", tracing::Level::INFO) => tracing::info!(target: "datamk_probe", "x"),
            ("datamk_probe", tracing::Level::DEBUG) => tracing::debug!(target: "datamk_probe", "x"),
            other => unreachable!("add a case for test target/level {other:?}"),
        });
        seen.load(Ordering::SeqCst)
    }
}
