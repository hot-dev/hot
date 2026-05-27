-- Track the full embedding identity used to build vectors for a store.
-- Existing embedding rows are treated as local embeddings because older
-- metadata only recorded model/dims/field.

ALTER TABLE store_map
    ADD COLUMN embedding_conf text;

UPDATE store_map
SET embedding_conf = json_object(
    'provider', 'local',
    'model', embedding_model,
    'dimensions', embedding_dimensions,
    'field', COALESCE(embedding_field, 'content'),
    'version', 1
)
WHERE embedding_model IS NOT NULL;

ALTER TABLE store_map DROP COLUMN embedding_model;
ALTER TABLE store_map DROP COLUMN embedding_dimensions;
ALTER TABLE store_map DROP COLUMN embedding_field;
