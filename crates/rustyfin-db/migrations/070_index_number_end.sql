-- The last episode of a file that holds several, such as `Show S01E03-E04.mkv`: Jellyfin's
-- IndexNumberEnd. NULL for other items. Episodes stored before this get theirs at the next scan.
ALTER TABLE media_items ADD COLUMN index_number_end INTEGER;
