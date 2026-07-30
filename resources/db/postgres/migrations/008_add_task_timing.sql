-- Runtime timing is observability metadata, separate from application
-- checkpoint state stored in task.info.
ALTER TABLE hot.task ADD COLUMN timing jsonb;
ALTER TABLE ONLY hot.task ALTER COLUMN timing SET COMPRESSION lz4;
