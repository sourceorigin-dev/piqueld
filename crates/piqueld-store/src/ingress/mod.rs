//! Transactional hostname ownership and the gateway's durable routing projection.
use super::{Store, StoreError, auth::access::scope_json};
use piqueld_core::{
    EnvironmentId,
    access::Scope,
    api::RouteStatus,
    manifest::{Hostname, RouteHostname, ValidatedRoute},
};
use sqlx::{SqliteConnection, SqliteExecutor};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// Routes the gateway serves, keyed by owning environment in a stable order.
pub type RoutingTable = BTreeMap<EnvironmentId, Vec<ValidatedRoute>>;

/// What the routing tables say about one route hostname, which its DNS
/// records follow.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostnameRoutes {
    /// The visibility of the route the gateway last acknowledged for it.
    pub applied: Option<piqueld_core::manifest::Visibility>,
    /// Whether a route in the [`Store::routing_table`] names it.
    pub desired: bool,
    /// Whether piqueld manages its DNS records ([`Store::dns_records`]).
    pub tracked: bool,
}

/// A DNS-01 challenge TXT record piqueld creates, saved before it exists and
/// kept until it is deleted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcmeChallenge {
    /// Record name, `_acme-challenge.<domain>`.
    pub name: String,
    /// Name of the zone holding it.
    pub zone: Hostname,
    /// TXT value, which finds the record when its ID is unknown.
    pub value: String,
    /// The provider's ID for the record, once it is created.
    pub record_id: Option<String>,
}

impl AcmeChallenge {
    /// A stored `acme_challenges` row.
    fn read(
        name: String,
        zone: &str,
        value: String,
        record_id: Option<String>,
    ) -> Result<Self, StoreError> {
        Ok(Self {
            name,
            zone: Hostname::parse(zone).map_err(StoreError::corrupt)?,
            value,
            record_id,
        })
    }
}

impl Store {
    /// Finalizes writes to application intent, deployment inputs/targets, or
    /// gateway state, given the environments whose hostnames they may change.
    /// Refresh ownership from the final transaction state before committing, so a
    /// hostname conflict rolls back the mutation, events, and replay receipt together.
    /// Write helpers must leave this to their transaction owner rather than checking
    /// intermediate state (a save can also replace the pending deployment).
    pub(super) async fn commit_environment_changes<'a>(
        mut tx: crate::WriteTransaction<'_>,
        environments: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), StoreError> {
        for id in environments {
            Self::reserve_hostnames_on(&mut tx, id).await?;
        }
        tx.commit().await.map_err(StoreError::database)
    }

    /// Replaces the hostnames the installation serves itself. Routes may not
    /// claim them or their subdomains. Returns route hostnames saved before the
    /// reservation that now conflict; the gateway never publishes them.
    /// Existing reservations are reported rather than rejected, so a newly
    /// configured website hostname never fails daemon startup.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn reserve_installation_hostnames(
        &self,
        hostnames: &[Hostname],
    ) -> Result<Vec<RouteHostname>, StoreError> {
        let mut tx = self.begin_immediate().await?;
        sqlx::query!("DELETE FROM installation_hostnames")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        for hostname in hostnames {
            let hostname = hostname.as_str();
            sqlx::query!(
                "INSERT INTO installation_hostnames(hostname) VALUES(?1)",
                hostname
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        }
        let routed = sqlx::query_scalar!("SELECT hostname FROM hostname_reservations")
            .fetch_all(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)?;
        let mut conflicts = Vec::new();
        for hostname in routed {
            let hostname = RouteHostname::parse(hostname).map_err(StoreError::corrupt)?;
            if hostnames.iter().any(|domain| hostname.is_within(domain)) {
                conflicts.push(hostname);
            }
        }
        Ok(conflicts)
    }

    /// Whether `hostname` is exact and another application than the
    /// environment's reserves the wildcard matching it. Names that wildcard
    /// serves stay its own, so an exact route never takes them over.
    async fn under_foreign_wildcard(
        connection: &mut SqliteConnection,
        hostname: &RouteHostname,
        environment_id: &str,
    ) -> Result<bool, StoreError> {
        let Some(parent) = hostname
            .wildcard()
            .is_none()
            .then(|| hostname.domain().parent())
            .flatten()
        else {
            return Ok(false);
        };
        let wildcard = format!("*.{parent}");
        Ok(sqlx::query_scalar!(
            r#"SELECT 1 AS "covered!: bool" FROM hostname_reservations r JOIN environments owner ON owner.id=r.environment_id JOIN environments contender ON contender.id=?2 WHERE r.hostname=?1 AND owner.application_id<>contender.application_id"#,
            wildcard,
            environment_id
        )
        .fetch_optional(connection)
        .await
        .map_err(StoreError::database)?
        .is_some())
    }

    /// The hostnames reserved for the installation itself, which wildcard
    /// routes leave alone.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn reserved_hostnames(&self) -> Result<Vec<Hostname>, StoreError> {
        Self::installation_hostnames(&self.pool).await
    }

    /// Loads the hostnames reserved for the installation itself.
    async fn installation_hostnames<'e>(
        executor: impl SqliteExecutor<'e>,
    ) -> Result<Vec<Hostname>, StoreError> {
        sqlx::query_scalar!("SELECT hostname FROM installation_hostnames")
            .fetch_all(executor)
            .await
            .map_err(StoreError::database)?
            .into_iter()
            .map(|hostname| Hostname::parse(hostname).map_err(StoreError::corrupt))
            .collect()
    }

    /// Routes the gateway last accepted for an application; empty when none were
    /// ever acknowledged.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn applied_routes(
        &self,
        id: &EnvironmentId,
    ) -> Result<Vec<ValidatedRoute>, StoreError> {
        let id = id.as_str();
        let json = sqlx::query_scalar!(
            "SELECT applied_json FROM environment_routes WHERE environment_id=?1",
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?;
        json.as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(StoreError::corrupt)
            .map(Option::unwrap_or_default)
    }

    /// Whether the application has desired or applied routes, i.e. whether the
    /// gateway may still hold state for it.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn has_routes(&self, id: &EnvironmentId) -> Result<bool, StoreError> {
        let id = id.as_str();
        Ok(sqlx::query_scalar!("SELECT EXISTS(SELECT 1 FROM environment_routes WHERE environment_id=?1 AND (json_array_length(desired_json)>0 OR json_array_length(applied_json)>0))",id)
            .fetch_one(&self.pool).await.map_err(StoreError::database)? != 0)
    }

    /// Recomputes reservations inside the transaction changing their source.
    /// Captured deployments also reserve names while a newer save is pending.
    /// Collects every hostname the environment could still serve, rejects any
    /// newly within an installation hostname or exactly below another
    /// application's wildcard, then replaces the environment's
    /// `hostname_reservations` rows. Names reserved before the installation
    /// claimed them stay reserved, though never routed. A unique violation means
    /// another environment owns the name; sibling conflicts identify that
    /// environment of the same application.
    async fn reserve_hostnames_on(
        connection: &mut SqliteConnection,
        environment_id: &str,
    ) -> Result<(), StoreError> {
        let mut names = sqlx::query_scalar!(r#"
            SELECT DISTINCT json_extract(r.value, '$.hostname') AS "hostname!: String" FROM (
                SELECT json_extract(resolved_json,'$.routes') AS routes FROM environments WHERE id=?1
                UNION ALL SELECT json_extract(target_json,'$.routes') FROM operations WHERE id=(SELECT id FROM operations WHERE environment_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1)
                UNION ALL SELECT json_extract(manifest_json,'$.spec.routes') FROM deployments WHERE id=(SELECT id FROM operations WHERE environment_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1)
                UNION ALL SELECT desired_json FROM environment_routes WHERE environment_id=?1
                UNION ALL SELECT applied_json FROM environment_routes WHERE environment_id=?1
            ) AS sources, json_each(sources.routes) AS r
        "#, environment_id).fetch_all(&mut *connection).await.map_err(StoreError::database)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        if let Some(environment) = Self::environment_on(&mut *connection, environment_id).await?
            && let Some(manifest) = environment.manifest()
        {
            names.extend(
                manifest
                    .hostnames(&environment.environment.target())
                    .into_iter()
                    .map(String::from),
            );
        }
        let installation = Self::installation_hostnames(&mut *connection).await?;
        let reserved: BTreeSet<String> = sqlx::query_scalar!(
            "SELECT hostname FROM hostname_reservations WHERE environment_id=?1",
            environment_id
        )
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .collect();
        for hostname in names.difference(&reserved) {
            let parsed = RouteHostname::parse(hostname.as_str()).map_err(StoreError::corrupt)?;
            if installation.iter().any(|domain| parsed.is_within(domain))
                || Self::under_foreign_wildcard(&mut *connection, &parsed, environment_id).await?
            {
                return Err(StoreError::HostnameConflict {
                    hostname: hostname.clone(),
                });
            }
        }
        sqlx::query!(
            "DELETE FROM hostname_reservations WHERE environment_id=?1",
            environment_id
        )
        .execute(&mut *connection)
        .await
        .map_err(StoreError::database)?;
        for hostname in names {
            let result = sqlx::query!(
                "INSERT INTO hostname_reservations(hostname,environment_id) VALUES(?1,?2)",
                hostname,
                environment_id
            )
            .execute(&mut *connection)
            .await;
            if let Err(error) = result {
                if !error
                    .as_database_error()
                    .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
                {
                    return Err(StoreError::database(error));
                }
                let sibling = sqlx::query_scalar!(
                    r#"SELECT owner.name AS "name!" FROM hostname_reservations r JOIN environments owner ON owner.id=r.environment_id JOIN environments contender ON contender.id=?2 WHERE r.hostname=?1 AND owner.application_id=contender.application_id"#,
                    hostname,
                    environment_id
                ).fetch_optional(&mut *connection).await.map_err(StoreError::database)?;
                return Err(match sibling {
                    Some(name) => StoreError::SharedHostnameConflict {
                        hostname,
                        environment: piqueld_core::EnvironmentName::parse(name)
                            .map_err(StoreError::corrupt)?,
                    },
                    None => StoreError::HostnameConflict { hostname },
                });
            }
        }
        Ok(())
    }

    /// Persists a ready cutover, or withdraws removed routes while retaining
    /// existing destinations until their replacements are ready. A route whose
    /// visibility changes counts as removed, so it leaves its old listener as
    /// the deployment starts and joins the new one once backends are ready.
    /// When `operation_id` is given, fails with `StoreError::IllegalTransition`
    /// unless it is the application's latest operation and still running, so a
    /// superseded deployment cannot publish stale routes.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn stage_routes(
        &self,
        environment_id: &EnvironmentId,
        routes: &[ValidatedRoute],
        ready: bool,
        operation_id: Option<&str>,
    ) -> Result<(), StoreError> {
        let mut tx = self.begin_immediate().await?;
        let id = environment_id.as_str();
        if let Some(operation_id) = operation_id {
            let current = sqlx::query_scalar!("SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND environment_id=?2 AND state='running' AND id=(SELECT id FROM operations WHERE environment_id=?2 ORDER BY created_at_ms DESC,id DESC LIMIT 1))",operation_id,id)
                .fetch_one(&mut *tx).await.map_err(StoreError::database)?;
            if current == 0 {
                return Err(StoreError::IllegalTransition);
            }
        }
        let previous = sqlx::query_scalar!(
            "SELECT desired_json FROM environment_routes WHERE environment_id=?1",
            id
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let mut desired: Vec<ValidatedRoute> = previous
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(StoreError::corrupt)?
            .unwrap_or_default();
        if ready {
            desired = routes.to_vec();
        } else {
            desired.retain(|old| routes.iter().any(|new| new.same_listener(old)));
        }
        let json = serde_json::to_string(&desired).map_err(StoreError::corrupt)?;
        sqlx::query!("INSERT INTO environment_routes(environment_id,desired_json) VALUES(?1,?2) ON CONFLICT(environment_id) DO UPDATE SET desired_json=excluded.desired_json",id,json)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        Self::commit_environment_changes(tx, [id]).await
    }

    /// Every environment's desired routes, stopped ones' included, so their
    /// certificates and DNS records stay in place until they start again.
    /// Hostnames reserved by the installation are withheld even if they were
    /// saved before the reservation.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn routing_table(&self) -> Result<RoutingTable, StoreError> {
        self.table(false).await
    }

    /// Routes the gateway should serve: [`Self::routing_table`], with no
    /// route of a stopped environment.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn served_table(&self) -> Result<RoutingTable, StoreError> {
        self.table(true).await
    }

    /// [`Self::served_table`] when `served`, else [`Self::routing_table`].
    async fn table(&self, served: bool) -> Result<RoutingTable, StoreError> {
        let installation = Self::installation_hostnames(&self.pool).await?;
        sqlx::query!(
            r#"SELECT r.environment_id,CASE WHEN ?1 AND e.stopped=1 THEN '[]' ELSE r.desired_json END AS "desired_json!: String" FROM environment_routes r LEFT JOIN environments e ON e.id=r.environment_id ORDER BY r.environment_id"#,
            served
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .map(|row| {
            let mut routes: Vec<ValidatedRoute> =
                serde_json::from_str(&row.desired_json).map_err(StoreError::corrupt)?;
            routes.retain(|route| {
                !installation
                    .iter()
                    .any(|domain| route.hostname.is_within(domain))
            });
            Ok((
                EnvironmentId::parse(row.environment_id).map_err(StoreError::corrupt)?,
                routes,
            ))
        })
        .collect()
    }

    /// The `routes` whose environment belongs to a `readable` application.
    /// Route statuses name hostnames and backends, so other applications'
    /// routes stay hidden like the applications themselves.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn readable_routes(
        &self,
        readable: &Scope,
        mut routes: Vec<RouteStatus>,
    ) -> Result<Vec<RouteStatus>, StoreError> {
        let Some(applications) = scope_json(readable) else {
            return Ok(routes);
        };
        let environments: HashSet<String> = sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM environments WHERE application_id IN (SELECT value FROM json_each(?1))"#,
            applications
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .collect();
        routes.retain(|route| environments.contains(&route.environment_id));
        Ok(routes)
    }

    /// Records `table` as each application's applied routes and recomputes the
    /// hostname reservations of the environments whose routes changed. Returns
    /// whether any did; when none did, nothing is written.
    /// Called only after this exact table is accepted (or the gateway is stopped).
    ///
    /// Acknowledgements are serialized by the store's own lock, from reading
    /// the applied routes to writing them, so the comparison needs no writer
    /// lock and a steady pass takes none.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn acknowledge_routes(&self, table: &RoutingTable) -> Result<bool, StoreError> {
        let _acknowledging = self.acknowledgements.lock().await;
        let applied = self.applied_table().await?;
        let changed: Vec<_> = table
            .iter()
            .filter(|(id, routes)| applied.get(*id) != Some(*routes))
            .collect();
        if changed.is_empty() {
            return Ok(false);
        }
        let mut tx = self.begin_immediate().await?;
        for (environment_id, routes) in &changed {
            let id = environment_id.as_str();
            let json = serde_json::to_string(routes).map_err(StoreError::corrupt)?;
            sqlx::query!(
                "UPDATE environment_routes SET applied_json=?1 WHERE environment_id=?2",
                json,
                id
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        }
        let changed: Vec<&str> = changed.iter().map(|(id, _)| id.as_str()).collect();
        Self::commit_environment_changes(tx, changed).await?;
        Ok(true)
    }

    /// The routes the gateway last acknowledged, for every environment. DNS
    /// records follow this table, so they change only after the gateway has
    /// applied or withdrawn a route.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn applied_table(&self) -> Result<RoutingTable, StoreError> {
        sqlx::query!(
            "SELECT environment_id,applied_json FROM environment_routes ORDER BY environment_id"
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .map(|row| {
            Ok((
                EnvironmentId::parse(row.environment_id).map_err(StoreError::corrupt)?,
                serde_json::from_str(&row.applied_json).map_err(StoreError::corrupt)?,
            ))
        })
        .collect()
    }

    /// What the routing tables say about `hostname`, read without loading
    /// every route: the same as [`Self::applied_table`],
    /// [`Self::routing_table`] and [`Self::dns_records`] say of it.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn hostname_routes(
        &self,
        hostname: &RouteHostname,
    ) -> Result<HostnameRoutes, StoreError> {
        let name = hostname.as_str();
        let applied = sqlx::query_scalar!(
            r#"SELECT json(r.value) AS "route!: String" FROM environment_routes e JOIN json_each(e.applied_json) r WHERE json_extract(r.value,'$.hostname')=?1 ORDER BY e.environment_id LIMIT 1"#,
            name
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .map(|route| serde_json::from_str::<ValidatedRoute>(&route))
        .transpose()
        .map_err(StoreError::corrupt)?;
        let desired = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM environment_routes e JOIN json_each(e.desired_json) r WHERE json_extract(r.value,'$.hostname')=?1"#,
            name
        )
        .fetch_one(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let tracked = sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM dns_records WHERE hostname=?1) AS "tracked!: bool""#,
            name
        )
        .fetch_one(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let desired = desired > 0
            && !Self::installation_hostnames(&self.pool)
                .await?
                .iter()
                .any(|domain| hostname.is_within(domain));
        Ok(HostnameRoutes {
            applied: applied.map(|route| route.visibility),
            desired,
            tracked,
        })
    }

    /// Hostnames whose DNS records piqueld manages, including those of
    /// removed routes until their records are deleted.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn dns_records(&self) -> Result<BTreeSet<RouteHostname>, StoreError> {
        sqlx::query_scalar!("SELECT hostname FROM dns_records")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::database)?
            .into_iter()
            .map(|hostname| RouteHostname::parse(hostname).map_err(StoreError::corrupt))
            .collect()
    }

    /// Records that piqueld manages `hostname`'s DNS records, before it writes
    /// the first one, and whether changes it staged are `unpublished`.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn track_dns_records(
        &self,
        hostname: &RouteHostname,
        unpublished: bool,
    ) -> Result<(), StoreError> {
        let hostname = hostname.as_str();
        self.write(sqlx::query!(
            "INSERT INTO dns_records(hostname,unpublished) VALUES(?1,?2) ON CONFLICT(hostname) DO UPDATE SET unpublished=excluded.unpublished",
            hostname,
            unpublished
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Whether changes staged for `hostname` may not be published yet.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn dns_records_unpublished(
        &self,
        hostname: &RouteHostname,
    ) -> Result<bool, StoreError> {
        let hostname = hostname.as_str();
        Ok(sqlx::query_scalar!(
            "SELECT unpublished FROM dns_records WHERE hostname=?1",
            hostname
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
            == Some(1))
    }

    /// Every challenge record not deleted yet.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn acme_challenges(&self) -> Result<Vec<AcmeChallenge>, StoreError> {
        sqlx::query!("SELECT name,zone,value,record_id FROM acme_challenges ORDER BY name")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::database)?
            .into_iter()
            .map(|row| AcmeChallenge::read(row.name, &row.zone, row.value, row.record_id))
            .collect()
    }

    /// The challenge record named `name`, if it is not deleted yet.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn acme_challenge(&self, name: &str) -> Result<Option<AcmeChallenge>, StoreError> {
        sqlx::query!(
            "SELECT name,zone,value,record_id FROM acme_challenges WHERE name=?1",
            name
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .map(|row| AcmeChallenge::read(row.name, &row.zone, row.value, row.record_id))
        .transpose()
    }

    /// Saves `challenge` before its record is created, or its record's ID
    /// once it is.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn save_acme_challenge(&self, challenge: &AcmeChallenge) -> Result<(), StoreError> {
        let zone = challenge.zone.as_str();
        self.write(sqlx::query!(
            "INSERT INTO acme_challenges(name,zone,value,record_id) VALUES(?1,?2,?3,?4) ON CONFLICT(name) DO UPDATE SET zone=excluded.zone,value=excluded.value,record_id=excluded.record_id",
            challenge.name,
            zone,
            challenge.value,
            challenge.record_id
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Forgets the challenge record named `name` once it is deleted.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn forget_acme_challenge(&self, name: &str) -> Result<(), StoreError> {
        self.write(sqlx::query!(
            "DELETE FROM acme_challenges WHERE name=?1",
            name
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Forgets `hostname` once its DNS records are deleted.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn release_dns_records(&self, hostname: &RouteHostname) -> Result<(), StoreError> {
        let hostname = hostname.as_str();
        self.write(sqlx::query!(
            "DELETE FROM dns_records WHERE hostname=?1",
            hostname
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
