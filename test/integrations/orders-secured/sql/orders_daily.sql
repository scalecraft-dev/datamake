-- Public export `orders_daily@2`. Grain (order_date, region) is unique.
SELECT
    order_date,
    region,
    SUM(amount)::DECIMAL(18,2) AS revenue
FROM stg_orders
GROUP BY order_date, region
