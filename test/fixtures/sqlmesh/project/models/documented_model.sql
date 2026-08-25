MODEL (
  name sqlmesh_example.documented_model,
  kind FULL,
  owner finance,
  tags (invoicing, gold),
  description 'Orders per item with explicit columns and descriptions',
  grain item_id,
  columns (
    item_id INT,
    num_orders BIGINT
  ),
  column_descriptions (
    item_id = 'The item identifier',
    num_orders = 'Distinct orders for the item'
  )
);

SELECT
  item_id, -- inline comment: the item
  COUNT(DISTINCT id) AS num_orders -- inline comment: distinct orders
FROM sqlmesh_example.incremental_model
GROUP BY item_id
