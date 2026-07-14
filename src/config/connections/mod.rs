//! Per-connector config: the serde shape for a `connections:` entry plus its
//! resolve-time (`${VAR}` expansion) logic. One module per connector, mirrored
//! by `src/engine/connectors/` on the engine side (ADR 0003's enum-plus-match
//! seam) — adding a connector is two new files (one here, one there) plus one
//! `Connection`/`ResolvedConnection` variant and one match arm per dispatch
//! point, never a trait or a cargo feature.

pub mod bigquery;
