-- Private internal. Synthesizes rows so the fixture builds with zero external
-- setup. One language for transforms (ADR 0008): SELECT-only, no trailing
-- `;` — this is a bare-path entry in cell.yaml, so `materialize: replace`
-- is implied.
SELECT * FROM (VALUES
    (1, DATE '2026-06-01', 'us-east', 120.50),
    (2, DATE '2026-06-01', 'us-east',  80.00),
    (3, DATE '2026-06-01', 'us-west', 200.25),
    (4, DATE '2026-06-02', 'us-east',  59.99),
    (5, DATE '2026-06-02', 'eu-west', 410.00)
) AS t(order_id, order_date, region, amount)
