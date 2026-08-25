MODEL (
  name sqlmesh_example.inline_comment_model,
  kind VIEW,
  tags (gold),
  description 'View with inline column comments only'
);

SELECT
  item_id, -- the item id
  num_orders -- number of orders
FROM sqlmesh_example.full_model
