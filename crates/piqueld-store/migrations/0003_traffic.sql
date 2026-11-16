-- Adds the traffic the gateway counts for each environment's routes. Existing
-- databases start with no counts.

-- Requests one route of an environment served in one hour, counted from the
-- gateway's access log (apps/piqueld/src/ingress/traffic.rs). A route is the
-- service and hostname it sends requests to. Visitors are those first seen
-- that day on the route, so an hour's visitors add up to the day's.
CREATE TABLE traffic_hours (
    environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
    service TEXT NOT NULL,
    hostname TEXT NOT NULL,
    hour_ms INTEGER NOT NULL CHECK (hour_ms % 3600000 = 0),
    requests INTEGER NOT NULL CHECK (requests >= 0),
    page_views INTEGER NOT NULL CHECK (page_views >= 0),
    visitors INTEGER NOT NULL CHECK (visitors >= 0),
    client_errors INTEGER NOT NULL CHECK (client_errors >= 0),
    server_errors INTEGER NOT NULL CHECK (server_errors >= 0),
    bytes_sent INTEGER NOT NULL CHECK (bytes_sent >= 0),
    -- Summed time to answer the hour's requests, in microseconds.
    duration_us INTEGER NOT NULL CHECK (duration_us >= 0),
    PRIMARY KEY (environment_id, service, hostname, hour_ms)
) STRICT;

-- Page views of each page path, or from each referring site's hostname, on
-- one route in one UTC day. At most `TRAFFIC_RANK_LIMIT` values are kept per
-- route, day and kind; later new values are not counted.
CREATE TABLE traffic_ranks (
    environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
    service TEXT NOT NULL,
    hostname TEXT NOT NULL,
    day_ms INTEGER NOT NULL CHECK (day_ms % 86400000 = 0),
    kind TEXT NOT NULL CHECK (kind IN ('page', 'referrer')),
    value TEXT NOT NULL,
    page_views INTEGER NOT NULL CHECK (page_views > 0),
    PRIMARY KEY (environment_id, service, hostname, day_ms, kind, value)
) STRICT;
