-- Tracks volumes as objects: each Docker volume piqueld manages gets a
-- record, and an environment binds each manifest volume name to one. Every
-- volume a kept target names is adopted under its current Docker name and
-- bound to its environment under its manifest name, so no data moves. The
-- preview inventory moves here: its volumes are those a preview created,
-- and an entry no kept target names any more is adopted unbound, with the
-- preview it came from. Volumes of environments deleted earlier are
-- unknown to the database; the daemon adopts them from their Docker labels.

-- A Docker volume piqueld manages. It is attached while an environment
-- binds a name to it, and detached otherwise. Volumes created since this
-- migration are named after their ID.
CREATE TABLE volumes (
    id TEXT PRIMARY KEY,
    docker_name TEXT NOT NULL UNIQUE,
    -- Where the volume lives: Docker local volumes for now.
    pool TEXT NOT NULL CHECK (pool IN ('local')),
    -- The environment the volume's Docker labels name: the one that created
    -- it. Docker can't relabel a volume, so it stays when the volume is
    -- attached to another environment, and outlives the environment. A
    -- preview's volumes are those it created, deleted with it.
    owner_environment_id TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    -- The disk space it used when the daemon last measured it, if Docker
    -- could measure it.
    size_bytes INTEGER CHECK (size_bytes IS NULL OR size_bytes >= 0),
    measured_at_ms INTEGER CHECK (measured_at_ms IS NULL OR size_bytes IS NOT NULL),
    -- Where it was bound when its environment was deleted, by name, since
    -- the environment's record goes with it.
    detached_application TEXT,
    detached_environment TEXT,
    detached_name TEXT
) STRICT;
CREATE INDEX volumes_owner ON volumes(owner_environment_id);

-- What a manifest volume name of an environment points at. Rebinding a name
-- is one update of `volume_id`; the volume it pointed at before is detached.
-- Bindings outlive the manifest declaring the name, as Docker volumes always
-- did, and go with their environment, which detaches their volumes.
CREATE TABLE volume_bindings (
    environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    volume_id TEXT NOT NULL UNIQUE REFERENCES volumes(id) ON DELETE CASCADE,
    PRIMARY KEY (environment_id, name)
) STRICT;

CREATE TEMP TABLE adopted_volumes (
    environment_id TEXT NOT NULL,
    name TEXT,
    docker_name TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);
-- Current targets first, then operations' newest first, so a name binds to
-- the volume its environment mounts now.
INSERT INTO adopted_volumes
SELECT e.id, json_extract(v.value, '$.logical_name'),
    json_extract(v.value, '$.name'), e.created_at_ms
FROM environments e, json_each(e.resolved_json, '$.volumes') v
WHERE e.resolved_json IS NOT NULL;
INSERT INTO adopted_volumes
SELECT o.environment_id, json_extract(v.value, '$.logical_name'),
    json_extract(v.value, '$.name'), o.created_at_ms
FROM operations o, json_each(o.target_json, '$.volumes') v
WHERE o.target_json IS NOT NULL
ORDER BY o.created_at_ms DESC, o.id DESC;
INSERT INTO adopted_volumes
SELECT environment_id, NULL, name, created_at_ms FROM preview_volumes;

INSERT INTO volumes(id, docker_name, pool, owner_environment_id, created_at_ms)
SELECT 'volume-' || lower(hex(randomblob(16))), docker_name, 'local',
    environment_id, MIN(created_at_ms)
FROM adopted_volumes
GROUP BY docker_name;

INSERT INTO volume_bindings(environment_id, name, volume_id)
SELECT a.environment_id, a.name, v.id
FROM adopted_volumes a JOIN volumes v ON v.docker_name = a.docker_name
WHERE a.name IS NOT NULL
ORDER BY a.rowid
ON CONFLICT DO NOTHING;

UPDATE volumes SET
    detached_application = (SELECT a.name FROM environments e JOIN applications a ON a.id = e.application_id WHERE e.id = volumes.owner_environment_id),
    detached_environment = (SELECT name FROM environments WHERE id = volumes.owner_environment_id)
WHERE NOT EXISTS (SELECT 1 FROM volume_bindings WHERE volume_id = volumes.id);

DROP TABLE adopted_volumes;
DROP TABLE preview_volumes;
