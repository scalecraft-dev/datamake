# `datamk mcp`: the interface as an MCP server

`datamk mcp` serves a cell (or a project of cells) to an MCP client over
stdio. It is the context document and the closed query grammar, exposed
through the protocol a 2026 agent already speaks — nothing more. Every tool
and resource is a skin over the exact code the REST routes call, on the same
serving state `datamk serve` would build, so the two doors can never
disagree about what is mounted, what a query accepts, or which snapshot a
supported route serves.

```bash
datamk mcp -f cell.yaml              # one cell, profile `local`
datamk mcp -f cell.yaml -p prod      # a published profile: the poller runs, same as serve
datamk mcp                           # datamk.yaml in cwd -> every listed cell; else cell.yaml
datamk mcp -f cell.yaml --no-data    # meaning without rows (ADR 0012 §4)
```

## Three tools, whatever the export count

The REST surface has three route shapes, so the MCP surface has three
tools. A cell with 300 exports still has three tools; what grows is the
`list_exports` response, which the agent fetches when it needs it.

| Tool | Backed by | Returns |
|---|---|---|
| `list_exports()` | `GET /context` | Every export: route, what one row means, grain columns, contract, declared freshness, whether it is queryable, and its resource URI. |
| `describe_export(route)` | `GET /context/{route}?include=docs` | One export's full contract: schema and column meanings, the exact filters accepted, limits, sample request, definitions, long-form docs inlined. |
| `query_export(route, filters?, limit?, offset?)` | `GET /{route}?…` | Rows, ordered by grain, plus the page served: `row_count`, `limit`, `offset`, `truncated`, `next`. |

`filters` is an object of grain column → value, exact equality only. It is
flattened to the same `?k=v` map the REST door parses and validated by the
same function, so an unknown filter fails with the same sentence `serve`
returns, never silently ignored:

```
unknown query parameter 'revenue' — this export accepts grain filters (order_date, region), plus `limit` and `offset` (exact equality only; no ranges, no operators, no non-grain columns)
```

Tool failures are MCP `isError` results carrying REST's own 400/404 text,
so the model can self-correct. Protocol misuse (an unknown tool, an argument
the schema does not declare, a malformed URI) is a JSON-RPC error.

A `query_export` result is an object, not a bare array, because the wrapper
is where the cap is disclosed. `truncated: true` means "a full page; there
may be more" — not a total-count claim the server cannot make. Follow
`next.offset`.

```json
{"route":"orders_daily@2","rows":[…],"row_count":100,"limit":100,"offset":0,
 "truncated":true,"next":{"offset":100},
 "resource":"datamk://orders/context/orders_daily@2"}
```

## Resources

| URI | Content |
|---|---|
| `datamk://<mount>/context` | The context document (`GET /context`). |
| `datamk://<mount>/context/<route>` | Narrowed to one export, docs inlined. |
| `datamk://<mount>/docs/<target>` | One long-form docs page, at its declared media type. Targets are the ones the document already names: `cell`, `<route>`, `definition:<term>`. |

`<mount>` is the cell name for a single cell and the mount segment in a
project — the same segment as the URL and the qualified route.

## Projects

`datamk mcp` with no `-f`, or `-f datamk.yaml`, mounts every cell the
project file lists, exactly as `serve` does ([ADR 0014](../adr/0014-multi-cell-serving.md)).
Routes are qualified by mount on every tool — `orders/orders_daily@2` — and
`list_exports` says which cell each came from. A bare route in project mode
is an error that names the mounts. The tool count is still three.

It does **not** read the mesh manifest. The manifest is a hint an operator
hosts anywhere, never a registry `serve` or `mcp` reads from — see
`src/mesh.rs`. Aggregating other teams' hosted cells behind one MCP endpoint
is the client-side aggregator ADR 0012 §6 defers, with its token fan-out
threat model; it is not this command.

## Trust

A stdio server runs as you, with whatever the profile grants — the same
trust as `datamk context -p <profile>`. There is no bearer token; the
`access:` roles `serve` enforces at its socket are not consulted, because
there is no socket. The visibility filter still applies: a `private` export
appears nowhere, in any form. `--no-data` is the withholding lever:
`list_exports` and `describe_export` work, `query_export` reports that rows
are not served here and the profile's `channels:` say where they live.

Tool descriptions are generated from the interface (`description`,
`grain`, `contract`, `freshness`, the query block) — never hand-written —
so the anti-rot ratchet in the [context guide](context.md) reaches them.

## Point a client at it

```json
{
  "mcpServers": {
    "orders": {
      "command": "datamk",
      "args": ["mcp", "-f", "/abs/path/to/cell.yaml", "-p", "prod"]
    }
  }
}
```

The server's `initialize` reply carries `instructions` telling the model to
call `list_exports` first, `describe_export` before querying, and that the
grammar is closed.

## What it is not

- Not a SQL tool. No `where`, `order_by`, `columns`, `format`, or
  free-form query — the grammar is the REST grammar. An agent that needs
  real SQL attaches the storage plane read-only (`datamk attach`).
- Not one tool per export. That duplicates `/context` into `tools/list`
  and makes the agent's context cost scale with the cell.
- Not a network door. HTTP transport (`POST /mcp` on `serve`, inheriting
  bearer auth, throttle, and drain) is a later step on the same module.
- No sessions, subscriptions, or server-initiated messages: the subset a
  closed grammar needs is stateless request/response.
