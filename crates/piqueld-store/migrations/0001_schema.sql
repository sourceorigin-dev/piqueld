-- The control-plane schema. Later migrations change it in numbered steps;
-- see docs/dev/architecture/persistence.md.

-- Instance ------------------------------------------------------------------

-- The database's identity and the schema version it was migrated to.
CREATE TABLE instance_metadata (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    instance_id TEXT NOT NULL CHECK (
        length(instance_id) BETWEEN 1 AND 64 AND
        instance_id NOT GLOB '*[^a-z0-9-]*' AND
        substr(instance_id, 1, 1) GLOB '[a-z0-9]' AND
        substr(instance_id, -1, 1) GLOB '[a-z0-9]'
    ),
    schema_version INTEGER NOT NULL CHECK (schema_version > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0)
);

-- Applications and environments ----------------------------------------------

-- An application: its saved manifest and the revision of it. Editing the
-- manifest bumps `generation`.
CREATE TABLE applications (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    desired_json TEXT NOT NULL CHECK (json_valid(desired_json)),
    generation INTEGER NOT NULL DEFAULT 1 CHECK (generation > 0),
    delete_intent INTEGER NOT NULL DEFAULT 0 CHECK (delete_intent IN (0, 1)),
    -- The manifest last read from the application's repository. While the
    -- manifest there is unchanged, deployments keep deploying the saved
    -- one, edits included. NULL for applications without a repository.
    repository_json TEXT CHECK (repository_json IS NULL OR json_valid(repository_json)),
    -- The last listing of the application's branches for sync, and why it
    -- failed.
    sync_checked_at_ms INTEGER,
    sync_error TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

-- The deployable environments and previews of an application. Runtime
-- records belong to them. Environments are removed before their
-- application, never cascaded.
CREATE TABLE environments (
    id TEXT PRIMARY KEY,
    application_id TEXT NOT NULL REFERENCES applications(id),
    -- A preview is named by its slug, so this name's unique index keeps
    -- preview slugs and environment names of one application apart.
    name TEXT NOT NULL,
    -- Previews are disposable deployments of one branch of the application's
    -- manifest repository, configured by `[spec.previews]`.
    kind TEXT NOT NULL DEFAULT 'environment'
        CHECK (kind = 'environment' OR (kind = 'preview' AND branch IS NOT NULL AND pinned_commit IS NULL)),
    -- Distinguishes several previews of one branch.
    preview_slot TEXT CHECK (preview_slot IS NULL OR kind = 'preview'),
    -- The branch of the application's manifest repository the environment
    -- follows, optionally pinned to a commit; NULL for environments that
    -- deploy the saved manifest.
    branch TEXT,
    pinned_commit TEXT CHECK (pinned_commit IS NULL OR branch IS NOT NULL),
    -- The manifest last fetched from `branch`, with references unresolved;
    -- NULL before the first fetch and for saved-manifest environments.
    manifest_json TEXT CHECK (manifest_json IS NULL OR json_valid(manifest_json)),
    -- Promoted environments never build or fetch: they only receive releases
    -- promoted from another environment of the same application, stored by
    -- ID so renames keep it. Deleting a source with live dependents is
    -- refused; when both go (deleting the application), the dependent no
    -- longer names it.
    promoted_from TEXT REFERENCES environments(id) ON DELETE SET NULL
        CHECK (promoted_from IS NULL OR (kind = 'environment' AND branch IS NULL AND promoted_from != id)),
    -- Whether the environment follows its application's sync; previews
    -- follow their application's setting and ignore it.
    sync INTEGER NOT NULL DEFAULT 0 CHECK (sync IN (0, 1)),
    -- The branch head as of the last deployment, sync's or one of the
    -- environment's own branch: sync deploys only once the branch moves
    -- past it, and nothing follows before the first deployment. Changing the
    -- branch or the application's repository URL clears it.
    synced_commit TEXT,
    synced_at_ms INTEGER CHECK ((synced_commit IS NULL) = (synced_at_ms IS NULL)),
    -- Granted the daemon's telemetry: its receiving service, if it deploys
    -- one, gets every application's telemetry over a socket in
    -- `<data_dir>/telemetry/<environment-id>/`. Previews are never granted it.
    telemetry INTEGER NOT NULL DEFAULT 0
        CHECK (telemetry IN (0, 1) AND (telemetry = 0 OR kind = 'environment')),
    -- Stopped environments' services run no replicas and the gateway serves
    -- none of their routes, until they are started or deployed again.
    stopped INTEGER NOT NULL DEFAULT 0 CHECK (stopped IN (0, 1)),
    -- The resolved runtime target and the application revision it came from.
    resolved_json TEXT CHECK (resolved_json IS NULL OR json_valid(resolved_json)),
    resolved_generation INTEGER,
    delete_intent INTEGER NOT NULL DEFAULT 0 CHECK (delete_intent IN (0, 1)),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE UNIQUE INDEX environment_name ON environments(application_id, name);
CREATE INDEX environment_promotion_source ON environments(promoted_from) WHERE promoted_from IS NOT NULL;
-- Creating a preview is idempotent on its branch and slot. Creation never
-- needs the application revision; this index is what keeps it unique.
CREATE UNIQUE INDEX preview_key ON environments(application_id, branch, COALESCE(preview_slot, ''))
    WHERE kind = 'preview';

CREATE TABLE environment_status (
    environment_id TEXT PRIMARY KEY REFERENCES environments(id) ON DELETE CASCADE,
    state TEXT NOT NULL CHECK (state IN ('not_deployed','pending','deploying','ready','degraded','deleting','failed')),
    message TEXT,
    runtime_health TEXT,
    health_observed_at_ms INTEGER,
    updated_at_ms INTEGER NOT NULL
);

-- Every Docker volume a preview's deployments created, recorded before they
-- create it, including volumes later manifests dropped. Deleting the preview
-- removes them all and verifies they are gone before the preview row goes.
CREATE TABLE preview_volumes (
    environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (environment_id, name)
) STRICT;

-- Operations and deployments --------------------------------------------------

-- An accepted change to an environment, executed asynchronously. An accepted
-- manifest may have no resolved target yet; preparing an operation publishes
-- it after planning checks. `promoted` is durable, so restart recovery knows
-- whether to keep the old target during preparation or continue the new
-- rollout. The actor columns name who caused it: an account and its
-- credential, the host operator's Unix user, or a system actor such as
-- `sync:poll`.
CREATE TABLE operations (
    id TEXT PRIMARY KEY,
    environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('apply','refresh','delete')),
    state TEXT NOT NULL CHECK (state IN ('requested','running','succeeded','failed','cancelled','superseded')),
    generation INTEGER NOT NULL CHECK (generation > 0),
    attempt INTEGER NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    consecutive_failures INTEGER NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
    target_json TEXT CHECK (target_json IS NULL OR json_valid(target_json)),
    phase TEXT,
    resource TEXT,
    promoted INTEGER NOT NULL DEFAULT 0 CHECK (promoted IN (0, 1)),
    error_code TEXT,
    error_message TEXT,
    actor_user_id TEXT,
    actor_credential_id TEXT,
    actor_operator_uid INTEGER,
    actor_system TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    CHECK ((error_code IS NULL) = (error_message IS NULL)),
    CHECK ((state IN ('succeeded','failed','cancelled','superseded')) = (finished_at_ms IS NOT NULL))
);
CREATE INDEX operation_environment ON operations(environment_id, created_at_ms DESC, id DESC);
CREATE UNIQUE INDEX operation_running ON operations(environment_id) WHERE state = 'running';
CREATE INDEX operation_finished ON operations(finished_at_ms) WHERE finished_at_ms IS NOT NULL;

-- Immutable releases: what a successful preparation in a tracking
-- environment deployed, with each service's source provenance and prepared
-- image. They belong to the application, so they survive the environment
-- that recorded them.
CREATE TABLE releases (
    id TEXT PRIMARY KEY,
    application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
    -- Preparations with the same content share one release.
    content_hash TEXT NOT NULL,
    release_json TEXT NOT NULL CHECK (json_valid(release_json)),
    created_at_ms INTEGER NOT NULL,
    UNIQUE (application_id, content_hash)
);
CREATE INDEX release_application ON releases(application_id, id DESC);

-- The snapshot an apply operation executes. Applications remain editable
-- while it runs. Attempt outcomes survive event pruning, and a deployment's
-- success is kept regardless of later runtime repair.
CREATE TABLE deployments (
    id TEXT PRIMARY KEY REFERENCES operations(id) ON DELETE CASCADE,
    environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL CHECK (generation > 0),
    -- The manifest as captured, with `${{ namespace.name }}` references
    -- unresolved, and the values they rendered to.
    template_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(template_json)),
    variables_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(variables_json)),
    -- The rendered manifest; JSON null until a repository-backed manifest is
    -- fetched.
    manifest_json TEXT NOT NULL CHECK (json_valid(manifest_json)),
    -- Problems found while fetching the manifest that did not stop the
    -- deployment, such as a `spec.manifest` that differs from the
    -- application's connection.
    warnings_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(warnings_json)),
    -- Where the deployment came from: `{"type":"build"}` when built from its
    -- environment's source, or a promoted or redeployed release.
    origin_json TEXT NOT NULL DEFAULT '{"type":"build"}' CHECK (json_valid(origin_json)),
    -- The release its prepared target runs; NULL until prepared.
    release_id TEXT REFERENCES releases(id),
    -- Requested with `rebuild`: builds every Git source, whatever could be
    -- reused.
    rebuild INTEGER NOT NULL DEFAULT 0 CHECK (rebuild IN (0, 1)),
    created_at_ms INTEGER NOT NULL,
    succeeded_at_ms INTEGER
);
-- Deployment cursors and ordering use operation IDs, not creation timestamps.
CREATE INDEX deployment_environment ON deployments(environment_id, id DESC);
CREATE INDEX deployment_release ON deployments(release_id) WHERE release_id IS NOT NULL;

CREATE TABLE deployment_attempts (
    deployment_id TEXT NOT NULL REFERENCES deployments(id) ON DELETE CASCADE,
    attempt INTEGER NOT NULL,
    outcome_json TEXT NOT NULL CHECK (json_valid(outcome_json)),
    PRIMARY KEY (deployment_id, attempt)
);

-- A manual Deploy owns a snapshot separate from accepted application intent.
-- Fetched manifests and the commit they came from survive restart, are
-- accepted only after preparation, and are removed with their operation.
CREATE TABLE deployment_inputs (
    operation_id TEXT PRIMARY KEY REFERENCES operations(id) ON DELETE CASCADE,
    application_json TEXT NOT NULL CHECK (json_valid(application_json)),
    repository_commit TEXT,
    fetched INTEGER NOT NULL DEFAULT 0 CHECK (fetched IN (0, 1)),
    -- The branch the environment or preview tracked, unpinned, when the
    -- deployment was requested: the only one whose head its fetch may record.
    tracked_branch TEXT,
    -- The application's repository manifest when requested, and the manifest
    -- fetched; when they differ, a push changed it.
    repository_json TEXT CHECK (repository_json IS NULL OR json_valid(repository_json)),
    fetched_json TEXT CHECK (fetched_json IS NULL OR json_valid(fetched_json))
);

-- Job container cleanup survives restarts and is independent of the
-- operation's outcome. Retention keeps these operations until their job
-- containers have stopped.
CREATE TABLE job_cleanup (
    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES operations(id) ON DELETE CASCADE
);

-- Acceptance receipts replay retried requests. They expire after 24 hours,
-- independently of operation and event retention, and contain no manifest
-- values. A receipt replays only for the account that made the request
-- (`user_id`, NULL for the daemon), so a reused key cannot reveal another
-- account's outcome.
CREATE TABLE request_receipts (
    request_id TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    response_json TEXT NOT NULL CHECK (json_valid(response_json)),
    user_id TEXT,
    expires_at_ms INTEGER NOT NULL
);
CREATE INDEX receipt_expiry ON request_receipts(expires_at_ms);

-- Builds and job runs ---------------------------------------------------------

-- Build records and one-shot job runs (`job` set), with their bounded output.
-- They belong to their environment, not their operation, so pruning
-- operation history keeps them. Builds still running when the daemon stops
-- are marked `interrupted` on startup, and expiring output keeps the build's
-- metadata.
CREATE TABLE builds (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
    operation_id TEXT NOT NULL,
    service TEXT NOT NULL,
    job TEXT,
    source_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('running','succeeded','failed','interrupted')),
    commit_hash TEXT,
    image_id TEXT,
    -- The build whose image this one reused instead of building it again.
    -- No foreign key: build records are informational history, and the
    -- reused one may be gone.
    reused_build_id INTEGER,
    exit_code INTEGER,
    log_bytes INTEGER NOT NULL DEFAULT 0,
    log_truncated INTEGER NOT NULL DEFAULT 0,
    log_expired INTEGER NOT NULL DEFAULT 0,
    started_at_ms INTEGER NOT NULL,
    finished_at_ms INTEGER
);
CREATE INDEX builds_environment ON builds(environment_id, id);
CREATE INDEX builds_operation ON builds(operation_id);
CREATE INDEX builds_unexpired_finished ON builds(finished_at_ms) WHERE log_expired = 0;
-- Reuse looks earlier builds up by the commit they checked out.
CREATE INDEX builds_commit ON builds(commit_hash) WHERE commit_hash IS NOT NULL;

-- Output chunks record their stream and capture time; `build_log_stream`
-- filters by stream before paging backwards.
CREATE TABLE build_log_chunks (
    build_id INTEGER NOT NULL REFERENCES builds(id) ON DELETE CASCADE,
    offset INTEGER NOT NULL,
    data BLOB NOT NULL,
    stream TEXT NOT NULL CHECK (stream IN ('stdout','stderr')),
    timestamp_ms INTEGER NOT NULL,
    PRIMARY KEY (build_id, offset)
);
CREATE INDEX build_log_stream ON build_log_chunks(build_id, stream, offset);

-- Secrets ---------------------------------------------------------------------

-- Generated secrets, the ones an environment's manifest declares: metadata,
-- encrypted versions, and the versions each deployment pins. Changing a value
-- does not change the application's configuration or request a deployment.
CREATE TABLE environment_secrets (
    environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation > 0),
    updated_at_ms INTEGER NOT NULL,
    -- Cleanup reservations survive cancellation, partial Docker failures and
    -- restarts.
    deletion_id TEXT,
    PRIMARY KEY (environment_id, name)
);
CREATE TABLE secret_versions (
    environment_id TEXT NOT NULL,
    name TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation > 0),
    swarm_name TEXT NOT NULL UNIQUE,
    nonce BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    -- Preserve immutable identities when lost-key recovery discards their
    -- values.
    available INTEGER NOT NULL DEFAULT 1 CHECK (available IN (0, 1)),
    PRIMARY KEY (environment_id, name, generation),
    FOREIGN KEY (environment_id, name) REFERENCES environment_secrets(environment_id, name) ON DELETE CASCADE
);
CREATE TABLE deployment_secret_pins (
    operation_id TEXT NOT NULL REFERENCES operations(id) ON DELETE CASCADE,
    environment_id TEXT NOT NULL,
    name TEXT NOT NULL,
    generation INTEGER NOT NULL,
    PRIMARY KEY (operation_id, name),
    FOREIGN KEY (environment_id, name, generation) REFERENCES secret_versions(environment_id, name, generation)
);
CREATE INDEX deployment_secret_pin_version ON deployment_secret_pins(environment_id, name, generation);
-- An empty pin set must also remain immutable across retries.
CREATE TABLE deployment_secrets_prepared (
    operation_id TEXT PRIMARY KEY REFERENCES operations(id) ON DELETE CASCADE
);

-- Manually set secrets, in one store per application, where each secret
-- lists the environments allowed to mount it.
CREATE TABLE application_secrets (
    application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation > 0),
    updated_at_ms INTEGER NOT NULL,
    -- Cleanup reservations survive cancellation, partial Docker failures and
    -- restarts.
    deletion_id TEXT,
    -- Every environment, including later ones, or only those in
    -- application_secret_access.
    all_environments INTEGER NOT NULL CHECK (all_environments IN (0, 1)),
    -- Whether previews may mount it.
    previews INTEGER NOT NULL CHECK (previews IN (0, 1)),
    PRIMARY KEY (application_id, name)
);
-- Environments are listed by ID: renaming one keeps its access, deleting one
-- removes it from every list.
CREATE TABLE application_secret_access (
    application_id TEXT NOT NULL,
    name TEXT NOT NULL,
    environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
    PRIMARY KEY (application_id, name, environment_id),
    FOREIGN KEY (application_id, name) REFERENCES application_secrets(application_id, name) ON DELETE CASCADE
);
CREATE TABLE application_secret_versions (
    application_id TEXT NOT NULL,
    name TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation > 0),
    nonce BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    available INTEGER NOT NULL DEFAULT 1 CHECK (available IN (0, 1)),
    PRIMARY KEY (application_id, name, generation),
    FOREIGN KEY (application_id, name) REFERENCES application_secrets(application_id, name) ON DELETE CASCADE
);
-- Docker secrets belong to one environment, so each environment that mounts
-- a stored version gets its own Docker secret.
CREATE TABLE application_secret_copies (
    environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
    application_id TEXT NOT NULL,
    name TEXT NOT NULL,
    generation INTEGER NOT NULL,
    swarm_name TEXT NOT NULL UNIQUE,
    PRIMARY KEY (environment_id, name, generation),
    FOREIGN KEY (application_id, name, generation) REFERENCES application_secret_versions(application_id, name, generation) ON DELETE CASCADE
);
CREATE INDEX application_secret_copy_version ON application_secret_copies(application_id, name, generation);
CREATE TABLE deployment_stored_secret_pins (
    operation_id TEXT NOT NULL REFERENCES operations(id) ON DELETE CASCADE,
    environment_id TEXT NOT NULL,
    name TEXT NOT NULL,
    generation INTEGER NOT NULL,
    PRIMARY KEY (operation_id, name),
    FOREIGN KEY (environment_id, name, generation) REFERENCES application_secret_copies(environment_id, name, generation)
);
CREATE INDEX deployment_stored_secret_pin_copy ON deployment_stored_secret_pins(environment_id, name, generation);

-- A verifier for the master key. Keeps the key binding even after the final
-- secret is deleted.
CREATE TABLE secret_key_verification (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    nonce BLOB NOT NULL,
    ciphertext BLOB NOT NULL
);

-- The secret GitHub signs an application's push webhooks with, encrypted
-- with the master key. Generating another replaces it, and lost-key recovery
-- deletes them all.
CREATE TABLE webhook_secrets (
    application_id TEXT PRIMARY KEY REFERENCES applications(id) ON DELETE CASCADE,
    nonce BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL
) STRICT;

-- Ingress and DNS -------------------------------------------------------------

-- Each environment's routes and the hostnames they reserve. Routing intent is
-- durable before gateway I/O, so an interrupted gateway update is reapplied
-- from it. The applied projection retains hostname ownership until a removal
-- has actually reached the gateway. Reservation checks cover saved
-- configuration, captured deployment inputs, resolved targets and both
-- projections in one transaction.
CREATE TABLE environment_routes (
    environment_id TEXT PRIMARY KEY REFERENCES environments(id) ON DELETE CASCADE,
    desired_json TEXT NOT NULL CHECK (json_valid(desired_json)),
    applied_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(applied_json))
) STRICT;

CREATE TABLE hostname_reservations (
    hostname TEXT PRIMARY KEY,
    environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE
) STRICT;
CREATE INDEX hostname_reservations_environment ON hostname_reservations(environment_id);

-- Hostnames the installation serves itself, such as the website origin.
-- Routes may not claim them or their subdomains. Replaced from configuration
-- at startup.
CREATE TABLE installation_hostnames (
    hostname TEXT PRIMARY KEY
) STRICT;

-- Hostnames whose DNS records piqueld manages: a name is added before its
-- `_piqueld.<hostname>` ownership record is written, and removed once its
-- records are deleted, so a removed route's records are found after a
-- restart. `unpublished` is set before changes a provider stages (OVH) and
-- cleared once the zone is published, so a failed publish is retried even
-- without changes.
CREATE TABLE dns_records (
    hostname TEXT PRIMARY KEY NOT NULL,
    unpublished INTEGER NOT NULL DEFAULT 0 CHECK (unpublished IN (0, 1))
) STRICT;

-- DNS-01 challenge TXT records piqueld creates, one per record name. A row is
-- saved before its record is created, gets the provider's `record_id` once
-- created, and is removed once the record is deleted, so records left by a
-- failed deletion or a crash are deleted after a restart. `zone` is the
-- zone's name, resolved to its provider again when deleting; `value` finds a
-- record whose creation never returned its ID.
CREATE TABLE acme_challenges (
    name TEXT PRIMARY KEY NOT NULL,
    zone TEXT NOT NULL,
    value TEXT NOT NULL,
    record_id TEXT
) STRICT;

-- History and observability ---------------------------------------------------

-- Informational history. It deliberately has no foreign keys to prunable
-- records, and carries its own diagnostic and action context. Events name
-- their application as well as their environment, so an application's
-- history spans every environment, including deleted ones; application-wide
-- events have no environment.
CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope TEXT NOT NULL DEFAULT 'application' CHECK (scope IN ('application','daemon')),
    application_id TEXT,
    environment_id TEXT,
    operation_id TEXT,
    action_id TEXT,
    generation INTEGER,
    attempt INTEGER,
    retry INTEGER,
    retry_delay_ms INTEGER,
    duration_ms INTEGER,
    kind TEXT NOT NULL,
    message TEXT,
    error_code TEXT,
    phase TEXT,
    resource TEXT,
    diagnostic_id TEXT,
    request_id TEXT,
    diagnostic_json TEXT CHECK (diagnostic_json IS NULL OR json_valid(diagnostic_json)),
    actor_user_id TEXT,
    actor_credential_id TEXT,
    actor_operator_uid INTEGER,
    actor_system TEXT,
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX event_application ON events(application_id, id);
CREATE INDEX event_environment ON events(environment_id, id);
CREATE INDEX event_operation ON events(operation_id, id);
CREATE INDEX event_action ON events(action_id, id);
CREATE INDEX event_scope ON events(scope, id);
-- Scoped age pruning; without statistics SQLite would otherwise walk
-- event_scope.
CREATE INDEX event_scope_created ON events(scope, created_at_ms);
CREATE INDEX event_created_at ON events(created_at_ms);
CREATE INDEX event_error ON events(error_code, id);
CREATE INDEX event_diagnostic ON events(diagnostic_id) WHERE diagnostic_id IS NOT NULL;
-- Recent burst notifications by address and account, checked on each
-- refusal.
CREATE INDEX security_denial_bursts ON events(resource, actor_user_id, created_at_ms) WHERE kind = 'access_denial_burst';
-- Other events about an operation carry the actor that requested it, so work
-- done long after the request stays attributed.
CREATE TRIGGER events_operation_actor AFTER INSERT ON events
WHEN NEW.operation_id IS NOT NULL AND NEW.action_id IS NULL AND NEW.actor_user_id IS NULL
    AND NEW.actor_operator_uid IS NULL AND NEW.actor_system IS NULL
BEGIN
    UPDATE events SET
        actor_user_id = (SELECT actor_user_id FROM operations WHERE id = NEW.operation_id),
        actor_credential_id = (SELECT actor_credential_id FROM operations WHERE id = NEW.operation_id),
        actor_operator_uid = (SELECT actor_operator_uid FROM operations WHERE id = NEW.operation_id),
        actor_system = (SELECT actor_system FROM operations WHERE id = NEW.operation_id)
    WHERE id = NEW.id;
END;

-- What retention pruned.
CREATE TABLE history_coverage (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    started_at_ms INTEGER NOT NULL,
    pruned_through_ms INTEGER,
    pruned_through_id INTEGER NOT NULL DEFAULT 0
);
INSERT INTO history_coverage(singleton, started_at_ms) VALUES (1, CAST(strftime('%s', 'now') AS INTEGER) * 1000);

-- Only executing actions, kept for recovery. History remains in immutable
-- events. Runtime requests record their actor while they run: their
-- operation's actor when they started, or the caller of a request outside
-- operations, like removing a deleted secret's versions.
CREATE TABLE active_actions (
    id TEXT PRIMARY KEY,
    operation_id TEXT,
    environment_id TEXT,
    generation INTEGER,
    phase TEXT NOT NULL,
    resource TEXT,
    attempt INTEGER,
    retry INTEGER,
    actor_user_id TEXT,
    actor_credential_id TEXT,
    actor_operator_uid INTEGER,
    actor_system TEXT,
    started_at_ms INTEGER NOT NULL
);

CREATE TABLE dependency_observations (
    key TEXT PRIMARY KEY,
    failed INTEGER NOT NULL,
    observed_at_ms INTEGER NOT NULL
);

-- Where container log export stopped reading, so it resumes after a restart:
-- the JSON form of `ContainerCursors` (apps/piqueld/src/telemetry/containers.rs).
CREATE TABLE container_log_cursors (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    state_json TEXT NOT NULL CHECK (json_valid(state_json))
) STRICT;

-- Notifications ---------------------------------------------------------------

-- Notification destinations. Each URL is encrypted with the master key;
-- lost-key recovery deletes the destinations.
CREATE TABLE notification_destinations (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL CHECK (kind IN ('json', 'discord')),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    nonce BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    -- Every application, or only those in
    -- notification_destination_applications.
    all_applications INTEGER NOT NULL CHECK (all_applications IN (0, 1)),
    previews INTEGER NOT NULL CHECK (previews IN (0, 1)),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

-- The categories each destination receives, with the text sent alongside.
-- `after_event_id` is the last event before the destination started
-- receiving the category, so adding a category, enabling the destination or
-- changing its URL never replays history.
CREATE TABLE notification_destination_categories (
    destination_id TEXT NOT NULL REFERENCES notification_destinations(id) ON DELETE CASCADE,
    category TEXT NOT NULL,
    message TEXT,
    after_event_id INTEGER NOT NULL,
    PRIMARY KEY (destination_id, category)
) STRICT;

CREATE TABLE notification_destination_applications (
    destination_id TEXT NOT NULL REFERENCES notification_destinations(id) ON DELETE CASCADE,
    application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
    PRIMARY KEY (destination_id, application_id)
) STRICT;

-- Which events each destination is notified of: daemon history, and events
-- of the applications it selects, of a preview only when it takes previews.
-- Queries narrow it to one event or destination.
CREATE VIEW notification_audience AS
SELECT d.id AS destination_id, e.id AS event_id
FROM notification_destinations d JOIN events e
WHERE e.scope = 'daemon' OR (
    (d.all_applications = 1 OR EXISTS (
        SELECT 1 FROM notification_destination_applications a
        WHERE a.destination_id = d.id AND a.application_id = e.application_id))
    AND (d.previews = 1 OR NOT EXISTS (
        SELECT 1 FROM environments v WHERE v.id = e.environment_id AND v.kind = 'preview'))
);

-- A durable cursor makes event processing and outbox creation atomic and
-- replayable.
CREATE TABLE notification_cursor (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    event_id INTEGER NOT NULL
);
INSERT INTO notification_cursor VALUES (1, 0);

-- Observed health conditions, so a condition raises one alert and one
-- recovery.
CREATE TABLE notification_conditions (
    key TEXT PRIMARY KEY,
    category TEXT NOT NULL,
    environment_id TEXT,
    event_id INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    first_seen_ms INTEGER NOT NULL,
    last_seen_ms INTEGER NOT NULL,
    notified INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX notification_condition_event ON notification_conditions(event_id);

-- The webhook delivery outbox. A delivery belongs to the destination it was
-- made for until that destination is deleted or gets a new URL or kind,
-- which detaches it: it keeps the name for history, and recoveries of its
-- failures are never sent.
CREATE TABLE notification_deliveries (
    id TEXT PRIMARY KEY,
    event_id INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    destination TEXT NOT NULL,
    destination_id TEXT REFERENCES notification_destinations(id) ON DELETE SET NULL,
    category TEXT NOT NULL,
    -- What the delivery is about, by name, as of when it was queued: a JSON
    -- `NotificationSubject`, so notifications name the application,
    -- environment or preview even after a rename or deletion.
    subject_json TEXT NOT NULL CHECK (json_valid(subject_json)),
    state TEXT NOT NULL CHECK (state IN ('pending','delivered','failed','cancelled')),
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at_ms INTEGER NOT NULL,
    retry_started_at_ms INTEGER NOT NULL,
    next_attempt_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE (event_id, destination, category)
);
CREATE INDEX delivery_pending ON notification_deliveries(state, next_attempt_ms);
CREATE INDEX delivery_destination ON notification_deliveries(destination_id);

-- A recovery is eligible only at destinations that received its failure
-- alerts.
CREATE TABLE notification_recovery_sources (
    recovery_id TEXT NOT NULL REFERENCES notification_deliveries(id) ON DELETE CASCADE,
    failure_id TEXT NOT NULL REFERENCES notification_deliveries(id) ON DELETE CASCADE,
    condition_key TEXT NOT NULL,
    PRIMARY KEY (recovery_id, failure_id)
);
CREATE INDEX recovery_failure ON notification_recovery_sources(failure_id);

-- Authentication and authorization --------------------------------------------

-- Whether the first account has been set up; there is no anonymous access.
CREATE TABLE auth_setup (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    initialized INTEGER NOT NULL DEFAULT 0,
    secret_hash TEXT
);
INSERT INTO auth_setup(singleton) VALUES (1);

CREATE TABLE auth_users (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE COLLATE NOCASE,
    display_name TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE auth_passkeys (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    credential TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX auth_passkeys_user ON auth_passkeys(user_id);

-- Hashed browser, CLI and API token credentials. Scoped credentials act with
-- their own grants, limited by their account's current access; others act
-- with the account's access. Scoped credentials without grants can do
-- nothing, and no scoped credential can create credentials.
CREATE TABLE auth_credentials (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
    secret_hash TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL CHECK (kind IN ('browser','cli','token')),
    name TEXT NOT NULL,
    scoped INTEGER NOT NULL DEFAULT 0 CHECK (scoped IN (0, 1)),
    -- The tailnet user or tag an API token is bound to, e.g. `tag:ci`.
    tailnet TEXT,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL,
    expires_at INTEGER
);
CREATE INDEX auth_credentials_user ON auth_credentials(user_id);

-- Network addresses each credential was used from, so use from a new one can
-- raise a security notification.
CREATE TABLE auth_credential_addresses (
    credential_id TEXT NOT NULL REFERENCES auth_credentials(id) ON DELETE CASCADE,
    address TEXT NOT NULL,
    first_seen_ms INTEGER NOT NULL,
    PRIMARY KEY (credential_id, address)
) WITHOUT ROWID;

-- With `user_id`, an invitation adds a passkey to that existing account
-- instead of creating a new one.
CREATE TABLE auth_invitations (
    id TEXT PRIMARY KEY,
    issuer_id TEXT NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
    user_id TEXT REFERENCES auth_users(id) ON DELETE CASCADE,
    secret_hash TEXT NOT NULL UNIQUE,
    expires_at INTEGER NOT NULL
);

-- An account's access is a set of grants. Each row grants one permission on
-- one application, and so on each of its environments, or on every
-- application when application_id is NULL. A row belongs to exactly one
-- account, credential, or invitation; deleting any of them, or the
-- application, removes its grants.
CREATE TABLE auth_grants (
    user_id TEXT REFERENCES auth_users(id) ON DELETE CASCADE,
    credential_id TEXT REFERENCES auth_credentials(id) ON DELETE CASCADE,
    invitation_id TEXT REFERENCES auth_invitations(id) ON DELETE CASCADE,
    permission TEXT NOT NULL,
    application_id TEXT REFERENCES applications(id) ON DELETE CASCADE,
    CHECK ((user_id IS NOT NULL) + (credential_id IS NOT NULL) + (invitation_id IS NOT NULL) = 1)
);
-- Holders are selected with `IS`, which only full indexes serve.
CREATE INDEX auth_grants_user ON auth_grants(user_id);
CREATE INDEX auth_grants_credential ON auth_grants(credential_id);
CREATE INDEX auth_grants_invitation ON auth_grants(invitation_id);
CREATE INDEX auth_grants_application ON auth_grants(application_id);

-- The single outstanding admin recovery link, issued over the Unix socket to
-- root or the daemon's own user. Redeeming it creates an administrator.
CREATE TABLE auth_recovery (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    secret_hash TEXT NOT NULL UNIQUE,
    expires_at INTEGER NOT NULL
);

-- Sign-in links and browser sessions of the host operator: root or the
-- daemon's own Unix user, identified by the kernel over the Unix socket.
-- They belong to no account. Redeeming a link turns it into a session,
-- replacing its secret hash with the session's.
CREATE TABLE auth_operator_sessions (
    id TEXT PRIMARY KEY,
    secret_hash TEXT NOT NULL UNIQUE,
    uid INTEGER NOT NULL,
    redeemed INTEGER NOT NULL DEFAULT 0 CHECK (redeemed IN (0, 1)),
    expires_at INTEGER NOT NULL
);

-- Audit -----------------------------------------------------------------------

-- Who did what through the API, kept independently of application history:
-- application deletion keeps these rows, and they have their own retention.
-- Accounts and credentials are copied, not referenced, so records outlive
-- them. For tamper evidence, records get consecutive IDs, and each `link` is
-- a SHA-256 over its predecessor's link and its own ID and fields;
-- `tailnet` and `operator_uid` are covered only when present.
CREATE TABLE audit_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at_ms INTEGER NOT NULL,
    action TEXT NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN ('allowed', 'denied', 'failed')),
    status INTEGER NOT NULL,
    user_id TEXT,
    username TEXT,
    credential_id TEXT,
    credential_kind TEXT,
    scoped INTEGER CHECK (scoped IS NULL OR scoped IN (0, 1)),
    peer TEXT,
    -- Who the request came from on the tailnet.
    tailnet TEXT,
    -- The host operator's Unix user, for requests it made.
    operator_uid INTEGER,
    request_id TEXT,
    application_id TEXT,
    environment_id TEXT,
    permission TEXT,
    link TEXT
);
CREATE INDEX audit_created ON audit_events(created_at_ms);
CREATE INDEX audit_user ON audit_events(user_id, id);
CREATE INDEX audit_credential ON audit_events(credential_id, id);
-- Recent refusals by caller, counted on each refusal to detect bursts.
CREATE INDEX audit_refusals ON audit_events(user_id, peer, created_at_ms) WHERE outcome = 'denied';

-- The newest pruned record, which the oldest retained one extends.
CREATE TABLE audit_chain (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    pruned_through INTEGER,
    pruned_link TEXT,
    CHECK ((pruned_through IS NULL) = (pruned_link IS NULL))
);
INSERT INTO audit_chain(singleton) VALUES (1);
