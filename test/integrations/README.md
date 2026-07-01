# Integration test fixtures

These are **test artifacts**, not user-facing examples. Each subdirectory is a
self-contained `datamk` cell that integration tests drive the binary against
(`run` → `verify` → `release` → `serve` → `deploy --dry-run`). They build offline
from synthesized rows — no object store, catalog server, or network required.

Unlike a real cell, the `prod` profile and `deploy/` overlay are **committed**
(they carry placeholder values, no real secrets) so CI has a fixed target. Only
generated `.cell/` state is gitignored.

| cell             | shape                                  | exercises                                            |
|------------------|----------------------------------------|------------------------------------------------------|
| `orders`         | open (`shareable`, no `roles`)         | run/verify/release pin, serve, deploy happy-path     |
| `orders-secured` | role-gated (`roles: [analyst]`)        | auth allow/deny, `load_principals` fail-loud (§8)    |

`orders` deploys cleanly because its `deploy/prod.yaml` sets `allow_anonymous:
true` — the deliberate, reviewed decision the open-endpoint pre-flight (§8f)
requires. `orders-secured` carries `profiles/missing-principals.yaml` whose
`principals:` points at a nonexistent file, to drive the fail-loud serve path.
