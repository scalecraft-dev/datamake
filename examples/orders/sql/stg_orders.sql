-- Private internal. In a real cell this reads source tables from the lake;
-- here we synthesize rows so `datamk run` works with zero external setup.
CREATE OR REPLACE TABLE stg_orders AS
SELECT * FROM (VALUES
    (1, DATE '2026-06-01', 'us-east', 120.50),
    (2, DATE '2026-06-01', 'us-east',  80.00),
    (3, DATE '2026-06-01', 'us-west', 200.25),
    (4, DATE '2026-06-02', 'us-east',  59.99),
    (5, DATE '2026-06-02', 'eu-west', 410.00)
) AS t(order_id, order_date, region, amount);
