//! The SQLMesh adapter (ADR 0016): reads the state store's `_versions`,
//! `_environments`, `_snapshots` and `_intervals` tables — read-only, through
//! DuckDB's own scanners, no Python — and produces `ir::DeployedModel`s for
//! one environment.

pub mod comments;
pub mod identifier;
pub mod names;
pub mod state;
