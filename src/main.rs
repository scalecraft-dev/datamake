mod cli;
mod config;
mod deploy;
mod engine;
mod init;
mod manifest;
mod ops;
mod release;
mod serve;
mod store;
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

    tracing_subscriber::fmt()
        .with_env_filter(
            // aws_config narrates every credential-chain resolution (including
            // access key ids) at INFO; that's RUST_LOG territory, not default output.
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,aws_config=warn".into()),
        )
        .init();

    match Cli::parse().command {
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
        Command::Rollback(a) => ops::rollback(&a.file, &a.profile, a.execution),
        Command::Publish(a) => {
            eprintln!("publish has been renamed to `release` (it pins the supported snapshot).");
            eprintln!("Run `datamk release` instead.");
            release::run(&a.file, &a.profile)
        }
    }
}
