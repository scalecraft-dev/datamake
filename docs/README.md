# Datamake docs

**Build**
- [Sources](guides/sources.md): files, other cells, warehouse connections, `query:`
- [Incremental loading](guides/incremental.md): `incremental:` and `materialize:`
- [Postgres setup](guides/postgres.md) · [Snowflake setup](guides/snowflake.md)
- [Discovered cells](guides/discover.md): interface from a SQLMesh project
- [Semantic models](guides/semantic-model.md): business meaning ingested from Apache Ossie

**Serve**
- [Serving](guides/serving.md): query grammar, status codes, multi-cell projects
- [Context document](guides/context.md): schema with meaning, docs pages, definitions, semantic models
- [MCP server](guides/mcp.md): the same interface over stdio

**Deploy**
- [Kubernetes](guides/kubernetes.md)

**Why**
- [Composable data products](concepts/composable-data-products.md)
- [ADRs](adr/): the reasoning behind each design decision
