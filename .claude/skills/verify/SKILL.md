---
name: verify
description: Build and drive the datamk CLI to verify changes end-to-end.
---

# Verifying datamk changes

The surface is the `datamk` CLI (`target/debug/datamk` after `cargo build`).
There is no GUI or long-lived service beyond `datamk serve`.

## Fast loop

```bash
cargo build                                  # binary at target/debug/datamk
datamk init <name>                           # scaffold a runnable cell in cwd (use a temp dir)
datamk run -f <cell>/cell.yaml               # end-to-end: bind sources, transforms, verify interface
datamk attach -f <cell>/cell.yaml -p <prof>  # prints attach SQL to stdout (notes on stderr)
```

The scaffolded cell runs offline with the default `local` profile (file
catalog + local storage) — good regression baseline.

## Driving profile-dependent paths

Profiles are just YAML in `<cell>/profiles/<name>.yaml`; write throwaway ones
to reach specific branches:

- **Published-artifact mode**: omit `catalog:` and set `storage:` to a remote
  URI. `datamk run` probes conditional PUT against the store *first thing*, so
  a nonexistent bucket fails fast with a real HTTP error — useful to prove the
  store client is built and reaches the network without needing live creds.
- **Direct-attach mode** (`catalog: ./.cell/catalog.ducklake`): `datamk attach`
  prints the secret + ATTACH SQL without any network contact. Pipe the printed
  secret statement into the Homebrew `duckdb` CLI (same 1.5.4 as vendored) and
  check `duckdb_secrets()` to validate the SQL against real DuckDB.
- Config warnings surface via `tracing` on stderr during any command.

## Gotchas

- `datamk run` leaves `.cell/` state in the cell dir; reuse it for attach
  tests (direct-attach requires the catalog file to exist).
- The kind_e2e suite (MinIO/k8s) is heavyweight — don't reach for it to verify
  non-deploy changes; unit + CLI driving covers the engine/store seams.
