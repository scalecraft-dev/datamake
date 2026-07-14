-- Public export `orders_daily@2`. Grain (order_date, region) is unique --
-- `datamk verify` enforces that against this actual output. One language
-- for transforms (ADR 0008): SELECT-only, no trailing `;` — this is a
-- bare-path entry in cell.yaml, so `materialize: replace` is implied.
SELECT
    order_date,
    region,
    SUM(amount)::DECIMAL(18,2) AS revenue
FROM stg_orders
GROUP BY order_date, region
