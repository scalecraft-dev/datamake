# MCP server

`datamk mcp` serves a cell, or a project of cells, to an MCP client over
stdio. Same exports, same query grammar, same caps as `serve`.

```bash
datamk mcp -f cell.yaml              # one cell, profile local
datamk mcp -f cell.yaml -p prod      # published profile: the poller runs
datamk mcp                           # datamk.yaml in cwd → every listed cell; else cell.yaml
datamk mcp -f cell.yaml --no-data    # tools describe; query_export reports rows are not served here
```

Client config:

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

## Tools

Always three, whatever the export count.

| Tool | REST equivalent | Returns |
| --- | --- | --- |
| `list_exports()` | `GET /context` | Every export: route, row meaning, grain, contract, freshness, queryable flag, resource URI. |
| `describe_export(route)` | `GET /context/{route}?include=docs` | Schema with column meanings, accepted filters, limits, sample request, definitions, docs pages. |
| `query_export(route, filters?, limit?, offset?)` | `GET /{route}?…` | `rows`, `row_count`, `limit`, `offset`, `truncated`, `next`, `resource`. |

`filters` is `{grain_column: value}`, exact equality only. Invalid filters
return REST's own 400 text as an MCP `isError` result. Protocol misuse is a
JSON-RPC error. `truncated: true` means a full page was served, not a total
count. Follow `next.offset`.

## Resources

| URI | Content |
| --- | --- |
| `datamk://<mount>/context` | The context document. |
| `datamk://<mount>/context/<route>` | One export, docs inlined. |
| `datamk://<mount>/docs/<target>` | One docs page. Targets: `cell`, `<route>`, `definition:<term>`. |
| `datamk://<mount>/semantic/<model>` | One Apache Ossie semantic model in full (ADR 0018) — the same document `?model=<name>` returns. One resource per bound model. |

`<mount>` is the cell name, or the mount segment in a project.

## Gotchas

- In project mode routes are qualified by mount (`orders/orders_daily@2`). A bare route is an error naming the mounts.
- No bearer token. The process runs with whatever the profile grants. `access:` roles are not consulted. `private` exports are still hidden.
- Tool descriptions are generated from the interface, never hand-written.
- Does not read the mesh manifest. No HTTP transport, sessions, subscriptions, or SQL tool.

Design rationale: [ADR 0012](../adr/0012-cell-context-document.md),
[ADR 0014](../adr/0014-multi-cell-serving.md).
