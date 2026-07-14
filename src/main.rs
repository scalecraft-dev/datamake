mod cli;
mod config;
mod deploy;
mod engine;
mod init;
mod logging;
mod manifest;
mod ops;
mod release;
mod serve;
mod store;
mod timeutil;
mod verify;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    // Two rustls CryptoProviders live in the dependency graph (kube -> ring,
    // aws-config -> aws-lc-rs), so rustls cannot infer a process default —
    // pin ring (the security-reviewed choice, see Cargo.toml's kube block)
    // before any TLS client is built, or kube handshakes panic at runtime.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("installing the rustls ring CryptoProvider");

    let cli = Cli::parse();

    // Console layer always; a second file layer for producer commands
    // (run/release/rollback/deploy) unless DATAMK_LOG=off or the log dir
    // can't be opened — see `logging` for the filter policy (the file sink
    // pins credential-narrating targets even under a hostile RUST_LOG).
    // Installed exactly once, before dispatch, as before.
    let log_path = logging::init(&cli.command, cli.log_dir.as_deref(), cli.log_keep);

    let result = dispatch(cli.command).await;

    // Discoverability (one stderr line per file-logging invocation): the
    // error itself already reached stderr via the branch below, or (on
    // success) nothing has been said about the log file yet.
    match &result {
        Ok(()) => {
            if let Some(path) = &log_path {
                eprintln!("log: {}", path.display());
            }
        }
        Err(e) => {
            // Mirrors the default `Termination` impl's error rendering so
            // failure output is unchanged when there's no file log; printed
            // here (rather than left to the runtime) so the "See the full
            // run log" line can follow it.
            eprintln!("Error: {e:?}");
            if let Some(path) = &log_path {
                eprintln!("See the full run log: {}", path.display());
            }
            std::process::exit(1);
        }
    }

    result
}

async fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Init(a) => init::run(a),
        Command::Run(a) => {
            // Hidden test hook: DATAMK_RETENTION_SECONDS lets harnesses
            // exercise compaction without waiting out day-granularity windows.
            let retention_secs = std::env::var("DATAMK_RETENTION_SECONDS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .or(match a.retention_days {
                    0 => None,
                    d => Some(d * 86_400),
                });
            let opts = engine::RunOptions {
                full_refresh: a.full_refresh,
                verify_replay: a.verify_replay,
            };
            engine::run(&a.file, &a.profile, retention_secs, opts)
        }
        Command::Verify(a) => verify::run(&a.file, &a.profile),
        Command::Release(a) => release::run(&a.file, &a.profile),
        Command::Deploy(a) => deploy::run(&a).await,
        Command::Serve(a) => serve::run(&a.file, &a.profile, a.port, a.poll_interval).await,
        Command::Status(a) => ops::status(&a.file, &a.profile),
        Command::Attach(a) => ops::attach(&a.file, &a.profile, a.execution, a.download),
        Command::Rollback(a) => ops::rollback(&a.file, &a.profile, a.execution),
        Command::Publish(a) => {
            eprintln!("publish has been renamed to `release` (it pins the supported snapshot).");
            eprintln!("Run `datamk release` instead.");
            release::run(&a.file, &a.profile)
        }
    }
}
