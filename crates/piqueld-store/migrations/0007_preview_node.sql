-- The worker node a preview runs on: `previews.node` when it was created, so
-- changing the setting never moves a preview and its volumes. NULL runs it on
-- the host, as every preview created before this migration does.
ALTER TABLE environments ADD COLUMN preview_node TEXT CHECK (preview_node IS NULL OR kind = 'preview');
