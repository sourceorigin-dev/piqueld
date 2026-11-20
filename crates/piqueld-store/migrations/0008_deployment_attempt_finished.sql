-- Deployment analytics select attempts by when they finished, which only
-- their outcome records: index that, so a window reads its attempts instead
-- of every one retained.
CREATE INDEX deployment_attempts_finished
ON deployment_attempts(json_extract(outcome_json, '$.finished_at_ms'));
