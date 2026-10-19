-- TEXT length counts Unicode characters; storage admission counts UTF-8 bytes.
-- Keep the established fixed per-call overhead and include persisted errors.
UPDATE call SET size =
    COALESCE(length(CAST(args AS BLOB)), 0) +
    COALESCE(length(CAST(return_value AS BLOB)), 0) +
    COALESCE(length(CAST(flow AS BLOB)), 0) +
    COALESCE(length(CAST(error AS BLOB)), 0) + 50;
