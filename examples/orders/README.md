# orders

An example [datamk](../../) cell. This tree is exactly what `datamk init orders` writes.

```
datamk run     -f cell.yaml   # execute the pipeline -> snapshot -> verify
datamk serve   -f cell.yaml   # GET /orders_daily@2 , /openapi.json , /interface
datamk publish -f cell.yaml   # pin the current snapshot as the supported contract
```

Try it:

```bash
datamk run -f cell.yaml
datamk serve -f cell.yaml &
curl 'http://localhost:8080/orders_daily@2?region=us-east'
curl 'http://localhost:8080/openapi.json'
```

## Files

- `cell.yaml` — the contract: transforms, interface, bindings, lifecycle.
- `sql/stg_orders.sql` — private staging (synthesizes sample rows).
- `sql/orders_daily.sql` — the exported object `orders_daily@2`.
- `.cell/` — generated catalog + Parquet + publish manifest (gitignored).

## Shipping a breaking change (side-by-side versions)

To add `@v3` without breaking `@v2` consumers: add a transform that builds
`orders_daily_v3`, then add a second interface entry pointing at it
(`version: 3.0.0`, `source: orders_daily_v3`). Both routes serve simultaneously;
consumers migrate on their own clock.
