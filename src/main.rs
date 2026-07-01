mod cli;
mod config;
mod deploy;
mod engine;
mod init;
mod manifest;
mod release;
mod serve;
mod verify;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Init(a) => init::run(a),
        Command::Run(a) => engine::run(&a.file, &a.profile),
        Command::Verify(a) => verify::run(&a.file, &a.profile),
        Command::Release(a) => release::run(&a.file, &a.profile),
        Command::Deploy(a) => deploy::run(&a),
        Command::Serve(a) => serve::run(&a.file, &a.profile, a.port).await,
        Command::Publish(a) => {
            eprintln!("publish has been renamed to `release` (it pins the supported snapshot).");
            eprintln!("Run `datamk release` instead.");
            release::run(&a.file, &a.profile)
        }
    }
}
