MODEL (
  name sqlmesh_example.comment_forms,
  kind VIEW
);

WITH base AS (
  SELECT
    item_id, -- cte comment: should NOT count
    num_orders
  FROM sqlmesh_example.full_model
)
SELECT
  item_id, -- trailing: the item
  -- leading: orders before the projection
  num_orders,
  /* block trailing */ num_orders * 2 AS doubled, /* block after */
  CAST(num_orders AS DOUBLE) AS as_double,
  base.item_id AS "Quoted, Alias", -- quoted alias
  'a -- not a comment' AS lit -- string with dashes
  -- final line comment after last projection
FROM base
UNION ALL
SELECT item_id, num_orders, 0, 0.0, 'x', 'y' -- right side: should NOT count
FROM base
