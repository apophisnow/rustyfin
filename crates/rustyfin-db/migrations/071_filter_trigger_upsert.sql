-- Saving an item now updates it in place with INSERT ... ON CONFLICT(id) DO UPDATE; INSERT OR
-- REPLACE deleted it first, and the delete cascaded to the rows that reference it. SQLite runs
-- that update with ABORT on conflict, which overrides the OR REPLACE of the statements in this
-- trigger, so recounting a year or rating already counted failed with "UNIQUE constraint failed".
-- The recounts now say what to do on a conflict themselves; otherwise the trigger is that of 061.
DROP TRIGGER IF EXISTS trg_media_items_update;
CREATE TRIGGER trg_media_items_update
AFTER UPDATE OF production_year, official_rating ON media_items
BEGIN
    -- Recount old year if it changed
    INSERT INTO filter_year_index (year, item_count)
    SELECT OLD.production_year, COUNT(*)
    FROM media_items
    WHERE production_year = OLD.production_year
    GROUP BY OLD.production_year
    ON CONFLICT(year) DO UPDATE SET item_count = excluded.item_count;

    DELETE FROM filter_year_index WHERE year = OLD.production_year AND item_count = 0;

    -- Recount new year
    INSERT INTO filter_year_index (year, item_count)
    SELECT NEW.production_year, COUNT(*)
    FROM media_items
    WHERE production_year = NEW.production_year
    GROUP BY NEW.production_year
    ON CONFLICT(year) DO UPDATE SET item_count = excluded.item_count;

    -- Recount old rating if it changed
    INSERT INTO filter_rating_index (rating, item_count)
    SELECT OLD.official_rating, COUNT(*)
    FROM media_items
    WHERE official_rating = OLD.official_rating
    GROUP BY OLD.official_rating
    ON CONFLICT(rating) DO UPDATE SET item_count = excluded.item_count;

    DELETE FROM filter_rating_index WHERE rating = OLD.official_rating AND item_count = 0;

    -- Recount new rating
    INSERT INTO filter_rating_index (rating, item_count)
    SELECT NEW.official_rating, COUNT(*)
    FROM media_items
    WHERE official_rating = NEW.official_rating
    GROUP BY NEW.official_rating
    ON CONFLICT(rating) DO UPDATE SET item_count = excluded.item_count;
END;
