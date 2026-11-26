-- The release channel operators chose to follow, through the API, when the
-- daemon configuration's `updates.channel` doesn't set one. Existing
-- installations have no row, and follow their build's channel, or stable,
-- until someone chooses.
CREATE TABLE update_channel (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    channel TEXT NOT NULL
);

-- Existing notification destinations receive the new `update_available`
-- category, as new ones do, from events after this migration on.
INSERT INTO notification_destination_categories(destination_id, category, after_event_id)
SELECT id, 'update_available', COALESCE((SELECT MAX(id) FROM events), 0)
FROM notification_destinations;
