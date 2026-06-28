-- Public export `orders_daily@2`. Grain (order_date, region) must be unique --
-- `datamk verify` enforces that against this actual output.
CREATE OR REPLACE TABLE orders_daily AS
SELECT
    order_date,
    region,
    SUM(amount)::DECIMAL(18,2) AS revenue
FROM stg_orders
GROUP BY order_date, region;
