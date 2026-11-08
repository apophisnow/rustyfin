-- Older versions dated each item by the time of its scan. This row marks the one-time backfill
-- that dates the items already stored by their files; the backfill deletes it when it finishes.
INSERT OR IGNORE INTO configuration (key, value) VALUES ('date_added_backfill', '');
