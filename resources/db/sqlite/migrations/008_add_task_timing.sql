-- Runtime timing is observability metadata, separate from application
-- checkpoint state stored in task.info.
ALTER TABLE task ADD COLUMN timing TEXT;
