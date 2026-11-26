//! Routes' DNS records, managed in the zones of providers that set
//! `manage_records`:
//!
//! ```text
//! private route           A/AAAA -> the apps node's tailnet addresses
//! public route, tunnel    CNAME (proxied) -> <tunnel-id>.cfargotunnel.com
//! public route, direct    A/AAAA -> [ingress] public_addresses (else manual)
//! ```
//!
//! **Ordering.** Records follow the routes the gateway has acknowledged: written
//! once it serves a route, deleted or repointed only after it withdrew it.
//!
//! **Ownership.** Only names with a `_piqueld.<hostname>` TXT record holding the
//! installation ID are changed. A name with A/AAAA/CNAME records or another
//! installation's ownership record is a `dns_conflict` and never written.
//!
//! **Reconciliation.** Every 10 seconds the desired records are recomputed from
//! the store; a hostname is reconciled when its plan changed and every 5 minutes.
//! Changes are daemon-scoped journal actions with `ingress_dns_*` phases.
use super::{
    Ingress,
    backoff::{Backoff, BackoffPolicy},
    wire::Journaled,
};
use super::{IngressError, Result};
use crate::ticker::Ticker;
use piqueld_config::dns::provider::{DnsProvider, Found, Record, RecordId, Zone};
use piqueld_core::ErrorChain as _;
use piqueld_core::Phase;
use piqueld_core::{
    api::DnsRecordState,
    manifest::{Hostname, RouteHostname, Visibility},
    observability::DiagnosticCode,
};
use piqueld_store::ingress::HostnameRoutes;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// Passes run at least this often, repairing drift.
const REPAIR: Duration = Duration::from_mins(5);
/// How often [`Ingress::run_records`] checks which records are due.
const RECORDS_PERIOD: Duration = Duration::from_secs(10);
/// Retries of a hostname whose records failed to change.
const RETRY: BackoffPolicy = BackoffPolicy {
    first_ms: 60 * 1000,
    max_ms: 3600 * 1000,
};

/// What one hostname's records should be.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Desired {
    /// Exactly these A/AAAA/CNAME records.
    Records(BTreeSet<Record>),
    /// Not known yet; current records are kept.
    Pending(&'static str),
    /// No route needs the hostname: the records piqueld owns are deleted.
    Removed,
    /// The operator manages them, such as a direct public route's without
    /// `[ingress] public_addresses`.
    Manual,
}

impl Desired {
    /// Whether it includes a proxied CNAME record.
    fn proxied(&self) -> bool {
        matches!(self, Self::Records(records)
            if records.iter().any(|record| matches!(record, Record::Cname { proxied: true, .. })))
    }
}

/// What piqueld does with one hostname's records.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Plan {
    /// The operator manages them.
    Manual,
    /// piqueld manages them in `zone` of provider `provider`.
    Managed {
        /// Index of the DNS provider that manages the records.
        provider: usize,
        /// Zone holding the records.
        zone: Zone,
        /// The records piqueld keeps for the hostname.
        desired: Desired,
    },
}

/// The DNS state of one route hostname, as route status reports it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct RouteDns {
    /// Whether the records are in place, pending or conflicting.
    pub(super) state: DnsRecordState,
    /// Safe diagnostic while records are pending or conflict.
    pub(super) message: Option<String>,
}

impl RouteDns {
    /// Creates the DNS status `state`, with its optional diagnostic `message`.
    fn new(state: DnsRecordState, message: impl Into<Option<String>>) -> Self {
        Self {
            state,
            message: message.into(),
        }
    }
}

/// Which hostnames the records loop reconciles on each tick.
#[derive(Default)]
struct Schedule {
    /// The plan of the latest pass.
    plan: BTreeMap<RouteHostname, Plan>,
    /// When every hostname was last reconciled, which repairs drift.
    repaired: Option<Instant>,
    /// Hostnames whose latest attempt failed, and when each is retried.
    failing: BTreeMap<RouteHostname, Backoff>,
}

impl Schedule {
    /// Whether every hostname is reconciled at `now`, to repair drift.
    fn repairs(&self, now: Instant) -> bool {
        self.repaired
            .is_none_or(|at| now.saturating_duration_since(at) >= REPAIR)
    }

    /// The hostnames of `plan` due at `now` (`now_ms` on the wall clock):
    /// those whose plan changed, failed ones whose backoff has elapsed, and
    /// every other one when a repair is due.
    fn due(
        &self,
        plan: &BTreeMap<RouteHostname, Plan>,
        now: Instant,
        now_ms: i64,
    ) -> BTreeSet<RouteHostname> {
        let repairs = self.repairs(now);
        plan.iter()
            .filter(|(hostname, entry)| {
                self.plan.get(*hostname) != Some(*entry)
                    || self
                        .failing
                        .get(*hostname)
                        .map_or(repairs, |backoff| backoff.ready(now_ms))
            })
            .map(|(hostname, _)| hostname.clone())
            .collect()
    }

    /// Records a pass over the `due` hostnames of `plan`, of which `failed`
    /// failed. It started at `started`, and finished at `finished_ms` on the
    /// wall clock: provider calls are slow, so a failure's backoff starts
    /// when the pass ends.
    fn record(
        &mut self,
        plan: BTreeMap<RouteHostname, Plan>,
        due: &BTreeSet<RouteHostname>,
        failed: BTreeSet<RouteHostname>,
        started: Instant,
        finished_ms: i64,
    ) {
        if self.repairs(started) {
            self.repaired = Some(started);
        }
        self.failing.retain(|hostname, _| {
            plan.contains_key(hostname) && (!due.contains(hostname) || failed.contains(hostname))
        });
        for hostname in failed {
            self.failing
                .entry(hostname)
                .or_default()
                .fail(RETRY, finished_ms);
        }
        self.plan = plan;
    }
}

/// Changes turning a name's current records into the desired ones.
#[derive(Debug, Default, Eq, PartialEq)]
struct Changes {
    /// Records replaced in place by one of the same type.
    update: Vec<(RecordId, Record)>,
    /// Records to remove, by ID.
    delete: Vec<RecordId>,
    /// Records to add.
    create: Vec<Record>,
}

impl Changes {
    /// Pairs surplus records with missing ones of the same type, which are
    /// updated in place; the rest are deleted, then the missing created. A
    /// CNAME cannot coexist with A/AAAA records, so deletions come first. A
    /// record whose proxying was changed counts as surplus.
    fn between(current: &[Found], desired: &BTreeSet<Record>) -> Self {
        let mut missing: Vec<&Record> = desired
            .iter()
            .filter(|record| !current.iter().any(|found| found.exact() == Some(*record)))
            .collect();
        let mut kept = BTreeSet::new();
        let mut changes = Self::default();
        for found in current {
            if let Some(record) = found.exact()
                && desired.contains(record)
                && kept.insert(record)
            {
                continue;
            }
            match missing
                .iter()
                .position(|record| record.kind() == found.record.kind())
            {
                Some(index) => changes
                    .update
                    .push((found.id.clone(), missing.remove(index).clone())),
                None => changes.delete.push(found.id.clone()),
            }
        }
        changes.create = missing.into_iter().cloned().collect();
        changes
    }

    /// Whether no change is needed.
    fn is_empty(&self) -> bool {
        self.update.is_empty() && self.delete.is_empty() && self.create.is_empty()
    }
}

impl Ingress {
    /// Every 10 seconds, plans each hostname's records and reconciles those
    /// the [`Schedule`] says are due. Does nothing while no provider manages
    /// records.
    pub(super) async fn run_records(&self, cancellation: &CancellationToken) {
        let dns = &self.certificates.dns;
        if !dns.manages_any_records() {
            return;
        }
        let mut ticks = Ticker::new(RECORDS_PERIOD);
        let mut schedule = Schedule::default();
        while ticks.tick(cancellation).await {
            if cancellation
                .run_until_cancelled(dns.refresh())
                .await
                .is_none()
            {
                return;
            }
            let plan = match self.plan_records().await {
                Ok(plan) => plan,
                Err(error) => {
                    tracing::warn!(error=?error, "could not plan route DNS records");
                    continue;
                }
            };
            let (now, now_ms) = (Instant::now(), piqueld_store::now_ms());
            let due = schedule.due(&plan, now, now_ms);
            let Some(failed) = self.reconcile_records(&plan, &due, cancellation).await else {
                return;
            };
            schedule.record(plan, &due, failed, now, piqueld_store::now_ms());
        }
    }

    /// Each hostname's plan: applied routes' records, routes waiting for the
    /// gateway kept as they are, and claimed names without a route removed.
    /// Disabled ingress removes every record. Until the gateway has converged
    /// (or stopped) with this daemon's configuration, such as a new ingress
    /// mode after a restart, every record is kept. Hostnames outside the zones
    /// of providers managing records are manual.
    async fn plan_records(&self) -> Result<BTreeMap<RouteHostname, Plan>> {
        let mut plan = BTreeMap::new();
        for (hostname, desired) in self.wanted_records().await? {
            let entry = self.plan_hostname(&hostname, desired).await;
            plan.insert(hostname, entry);
        }
        Ok(plan)
    }

    /// Each hostname's desired records, [`Desired::Manual`] when the operator
    /// manages them, before their zones are looked up; see
    /// [`Self::plan_records`].
    async fn wanted_records(&self) -> Result<BTreeMap<RouteHostname, Desired>> {
        let mut routes: BTreeMap<RouteHostname, HostnameRoutes> = BTreeMap::new();
        for route in self.store.applied_table().await?.into_values().flatten() {
            routes.entry(route.hostname).or_default().applied = Some(route.visibility);
        }
        for route in self.store.routing_table().await?.into_values().flatten() {
            routes.entry(route.hostname).or_default().desired = true;
        }
        for hostname in self.store.dns_records().await? {
            routes.entry(hostname).or_default().tracked = true;
        }
        for hostname in (self.webhook_hostname().into_iter()).chain(self.dashboard_hostname()) {
            routes.entry(hostname.clone().into()).or_default();
        }
        let addresses = self.node_addresses().await;
        Ok(routes
            .into_iter()
            .filter_map(|(hostname, routes)| {
                let wanted = self.wanted(&hostname, &routes, &addresses)?;
                Some((hostname, wanted))
            })
            .collect())
    }

    /// The desired records of `hostname`, from what the routing tables say of
    /// it (`routes`) and the apps node's `addresses`: an applied route's, the
    /// webhook or dashboard hostname's, pending while a route waits for the
    /// gateway, and removed for a tracked name without a route. Disabled
    /// ingress removes every record, and until the gateway has converged (or
    /// stopped) with this daemon's configuration, such as a new ingress mode
    /// after a restart, every record is kept. `None` is a hostname nothing
    /// wants.
    fn wanted(
        &self,
        hostname: &RouteHostname,
        routes: &HostnameRoutes,
        addresses: &[IpAddr],
    ) -> Option<Desired> {
        let is =
            |name: Option<&Hostname>| name.is_some_and(|name| name.as_str() == hostname.as_str());
        let served = if is(self.dashboard_hostname()) {
            Some(Visibility::Private)
        } else if is(self.webhook_hostname()) {
            Some(Visibility::Public)
        } else {
            routes.applied.filter(|_| self.enabled)
        };
        let wanted = match served {
            Some(visibility) => self.desired_records(visibility, addresses),
            None if self.enabled && routes.desired => {
                Desired::Pending("Waiting for the gateway to apply the route")
            }
            None if routes.tracked => Desired::Removed,
            None => return None,
        };
        let converged = self
            .gateway_converged
            .load(std::sync::atomic::Ordering::SeqCst);
        Some(match wanted {
            Desired::Manual => Desired::Manual,
            _ if !converged => {
                Desired::Pending("Waiting for the gateway to apply this daemon's configuration")
            }
            wanted => wanted,
        })
    }

    /// The apps node's tailnet addresses.
    async fn node_addresses(&self) -> Vec<IpAddr> {
        (self.health.read().await.private.addresses.iter())
            .filter_map(|address| address.parse().ok())
            .collect()
    }

    /// The plan for `hostname`, whose records should be `desired`: managed in
    /// its zone when that zone's provider manages records, otherwise manual.
    async fn plan_hostname(&self, hostname: &RouteHostname, desired: Desired) -> Plan {
        if desired == Desired::Manual {
            return Plan::Manual;
        }
        let dns = &self.certificates.dns;
        match dns.zone_for(&hostname.domain()).await {
            Ok((provider, zone)) if dns.manages_records(provider) => {
                let desired = if desired.proxied() && !dns.provider(provider).proxies() {
                    Desired::Pending(
                        "Tunnel routes need a proxied CNAME, which only a Cloudflare zone can hold. Move the zone to Cloudflare",
                    )
                } else {
                    desired
                };
                Plan::Managed {
                    provider,
                    zone,
                    desired,
                }
            }
            _ => Plan::Manual,
        }
    }

    /// The records a hostname served with `visibility` needs, manual for a
    /// direct public one without `[ingress] public_addresses`. Private ones
    /// only ever get the apps node's tailnet `addresses`.
    fn desired_records(&self, visibility: Visibility, addresses: &[IpAddr]) -> Desired {
        let addresses = match (visibility, &self.tunnel) {
            (Visibility::Private, _) if self.node.is_none() => {
                return Desired::Pending("Private ingress is disabled, so the route is not served");
            }
            (Visibility::Private, _) if addresses.is_empty() => {
                return Desired::Pending("Waiting for the apps node's tailnet addresses");
            }
            (Visibility::Private, _) => addresses,
            (Visibility::Public, Some(tunnel)) => {
                let Ok(target) = Hostname::parse(tunnel.hostname()) else {
                    return Desired::Manual;
                };
                return Desired::Records(BTreeSet::from([Record::Cname {
                    target,
                    proxied: true,
                }]));
            }
            (Visibility::Public, None) if self.public_addresses.is_empty() => {
                return Desired::Manual;
            }
            (Visibility::Public, None) => &self.public_addresses,
        };
        Desired::Records(addresses.iter().copied().map(Record::address).collect())
    }

    /// Reconciles the `due` hostnames of `plan`, one at a time, and publishes
    /// the states of every hostname in it for route status; the others keep
    /// their latest state. Returns the hostnames that failed, or `None` once
    /// shutdown stops the pass between hostnames.
    ///
    /// Provider calls are slow, so the gateway may change routes during a
    /// pass. Each hostname's plan is checked again just before its records
    /// change, and a hostname whose plan changed waits for the next pass.
    async fn reconcile_records(
        &self,
        plan: &BTreeMap<RouteHostname, Plan>,
        due: &BTreeSet<RouteHostname>,
        cancellation: &CancellationToken,
    ) -> Option<BTreeSet<RouteHostname>> {
        let mut states = BTreeMap::new();
        let mut failed = BTreeSet::new();
        for (hostname, plan) in plan {
            if !due.contains(hostname) {
                states.insert(hostname.clone(), self.route_dns(hostname));
                continue;
            }
            if cancellation.is_cancelled() {
                return None;
            }
            let state = self.converge(hostname, plan).await.unwrap_or_else(|error| {
                tracing::warn!(%hostname, error=error.chain(), "route DNS records could not be updated; will retry");
                failed.insert(hostname.clone());
                RouteDns::new(
                    DnsRecordState::Pending,
                    "The DNS provider could not update the records; piqueld retries after a minute, then less often. See daemon logs for details.".to_owned(),
                )
            });
            states.insert(hostname.clone(), state);
        }
        *self.records.write() = states;
        Some(failed)
    }

    /// Applies one hostname's `plan`, unless it changed since it was made.
    async fn converge(&self, hostname: &RouteHostname, plan: &Plan) -> Result<RouteDns> {
        let Plan::Managed {
            provider,
            zone,
            desired,
        } = plan
        else {
            return Ok(RouteDns::default());
        };
        let desired = match desired {
            Desired::Pending(reason) => {
                return Ok(RouteDns::new(DnsRecordState::Pending, (*reason).to_owned()));
            }
            Desired::Records(records) => Some(records),
            Desired::Removed => None,
            Desired::Manual => return Ok(RouteDns::default()),
        };
        let routes = self.store.hostname_routes(hostname).await?;
        let addresses = self.node_addresses().await;
        let current = match self.wanted(hostname, &routes, &addresses) {
            Some(desired) => Some(self.plan_hostname(hostname, desired).await),
            None => None,
        };
        if current.as_ref() != Some(plan) {
            return Ok(RouteDns::new(
                DnsRecordState::Pending,
                "The route changed during reconciliation; piqueld retries shortly".to_owned(),
            ));
        }
        self.converge_records(hostname, *provider, zone, desired)
            .await
    }

    /// Converges one hostname's records to `desired`, or deletes them when
    /// `None`, owning the name first. Never changes a name piqueld does not
    /// own, nor one another installation also claims. Changes a provider
    /// staged but did not publish, such as after a failed OVH refresh, are
    /// published even when nothing else changed.
    async fn converge_records(
        &self,
        hostname: &RouteHostname,
        provider: usize,
        zone: &Zone,
        desired: Option<&BTreeSet<Record>>,
    ) -> Result<RouteDns> {
        let dns = &self.certificates.dns;
        let site = Site {
            provider: dns.provider(provider),
            http: dns.http(),
            zone,
            hostname,
        };
        let ours = Record::Txt(self.instance_id.clone());
        let (current, ownership) = site.read().await?;
        let (ours_found, foreign): (Vec<&Found>, Vec<&Found>) =
            ownership.iter().partition(|found| found.record == ours);
        let owned = !ours_found.is_empty() && foreign.is_empty();
        let republish = self.store.dns_records_unpublished(hostname).await?;
        let withdrawing =
            self.withdrawing.lock().contains(hostname) && (!ours_found.is_empty() || republish);
        if !withdrawing {
            self.withdrawing.lock().remove(hostname);
        }
        let Some(desired) = desired else {
            let delete: Vec<RecordId> = current
                .iter()
                .filter(|_| owned)
                .chain(ours_found)
                .map(|found| found.id.clone())
                .collect();
            if !delete.is_empty() || republish {
                let changes = Changes {
                    delete,
                    ..Changes::default()
                };
                self.change_records(Phase::IngressDnsDelete, &site, None, &changes)
                    .await?;
            }
            self.store.release_dns_records(hostname).await?;
            return Ok(RouteDns::default());
        };
        if withdrawing {
            let claims = ours_found.iter().map(|found| found.id.clone()).collect();
            return self.withdraw(&site, claims).await;
        }
        if !owned && (!current.is_empty() || !ownership.is_empty()) {
            let existing: Vec<String> = current
                .iter()
                .chain(&ownership)
                .map(|found| found.record.to_string())
                .collect();
            return Ok(RouteDns::new(
                DnsRecordState::DnsConflict,
                format!(
                    "Records piqueld does not own already exist ({}). piqueld never overwrites them; remove them to let it manage {hostname}",
                    existing.join(", ")
                ),
            ));
        }
        let changes = Changes::between(&current, desired);
        if !owned {
            let written = self
                .change_records(Phase::IngressDnsCreate, &site, Some(&ours), &changes)
                .await?;
            if let Written::ClaimTaken(claim) = written {
                self.withdrawing.lock().insert(hostname.clone());
                return self.withdraw(&site, vec![claim]).await;
            }
        } else if !changes.is_empty() || republish {
            self.change_records(Phase::IngressDnsUpdate, &site, None, &changes)
                .await?;
        }
        Ok(RouteDns::new(DnsRecordState::Managed, None))
    }

    /// Deletes this installation's `claims` on a name another installation
    /// claimed at the same time, if still listed, and publishes the zone,
    /// then forgets the withdrawal. On failure the name stays in
    /// `withdrawing`, so the next pass retries.
    async fn withdraw(&self, site: &Site<'_>, claims: Vec<RecordId>) -> Result<RouteDns> {
        let changes = Changes {
            delete: claims,
            ..Changes::default()
        };
        self.change_records(Phase::IngressDnsDelete, site, None, &changes)
            .await?;
        self.withdrawing.lock().remove(site.hostname);
        Ok(RouteDns::new(
            DnsRecordState::DnsConflict,
            "Another installation claimed this name at the same time, so piqueld withdrew its claim and never overwrites its records".to_owned(),
        ))
    }

    /// Writes `changes` in one journal action, after the ownership record
    /// `claim` when given (see [`Site::apply`]). The hostname is tracked in
    /// the store first, with changes a provider stages marked unpublished
    /// until it publishes them.
    async fn change_records(
        &self,
        phase: Phase,
        site: &Site<'_>,
        claim: Option<&Record>,
        changes: &Changes,
    ) -> Result<Written> {
        let stages = site.provider.stages_changes();
        self.store.track_dns_records(site.hostname, stages).await?;
        let journal = self
            .journal_as(
                DiagnosticCode::DnsRecordsFailed,
                phase,
                site.hostname.as_str(),
            )
            .await
            .map_err(|error| IngressError::JournalDns(Box::new(error)))?;
        let result = site.apply(&journal, claim, changes).await;
        let written = journal.finish(result).await?;
        if stages {
            self.store.track_dns_records(site.hostname, false).await?;
        }
        tracing::info!(hostname=%site.hostname, phase=%phase, "changed route DNS records");
        Ok(written)
    }

    /// The DNS state of `hostname` from the latest pass.
    pub(super) fn route_dns(&self, hostname: &RouteHostname) -> RouteDns {
        self.records
            .read()
            .get(hostname)
            .cloned()
            .unwrap_or_default()
    }
}

/// What [`Site::apply`] wrote.
#[derive(Debug, Eq, PartialEq)]
enum Written {
    /// The claim, if any, and every change.
    Changes,
    /// Only the claim, whose record ID this is: another installation
    /// claimed the name at the same time, so it must be withdrawn.
    ClaimTaken(RecordId),
}

/// Where one hostname's records live.
struct Site<'a> {
    /// DNS provider that holds the zone.
    provider: &'a DnsProvider,
    /// HTTP client its API requests go through.
    http: &'a reqwest::Client,
    /// Zone holding the records.
    zone: &'a Zone,
    /// Route hostname whose records are managed.
    hostname: &'a RouteHostname,
}

impl Site<'_> {
    /// The ownership record's name: `_piqueld.<hostname>`, or
    /// `_piqueld._wildcard.<parent>` for a wildcard, since providers only
    /// accept `*` as a name's first label. Hostnames cannot contain `_`, so
    /// the two never collide.
    fn owner(&self) -> String {
        match self.hostname.wildcard() {
            Some(parent) => format!("_piqueld._wildcard.{parent}"),
            None => format!("_piqueld.{}", self.hostname),
        }
    }

    /// The hostname's A/AAAA/CNAME records, and the TXT records of its
    /// ownership name.
    async fn read(&self) -> Result<(Vec<Found>, Vec<Found>)> {
        let (provider, http, zone) = (self.provider, self.http, self.zone);
        let mut current = provider.records(http, zone, self.hostname.as_str()).await?;
        current.retain(|found| !matches!(found.record, Record::Txt(_)));
        let mut ownership = provider.records(http, zone, &self.owner()).await?;
        ownership.retain(|found| matches!(found.record, Record::Txt(_)));
        Ok((current, ownership))
    }

    /// Writes the ownership record `claim`, then `changes` in order, then
    /// publishes the zone. Each request is committed to `journal` first.
    /// Writes and publishes nothing more when another installation claimed
    /// the name at the same time: the caller withdraws `claim`.
    async fn apply(
        &self,
        journal: &Journaled<'_>,
        claim: Option<&Record>,
        changes: &Changes,
    ) -> Result<Written> {
        let (provider, http, zone) = (self.provider, self.http, self.zone);
        let name = self.hostname.as_str();
        if let Some(claim) = claim {
            journal.request().await?;
            let id = provider
                .upsert(http, zone, &self.owner(), None, claim)
                .await?;
            let ownership = provider.records(http, zone, &self.owner()).await?;
            if ownership
                .iter()
                .any(|found| matches!(found.record, Record::Txt(_)) && found.record != *claim)
            {
                return Ok(Written::ClaimTaken(id));
            }
        }
        for (id, record) in &changes.update {
            journal.request().await?;
            provider.upsert(http, zone, name, Some(id), record).await?;
        }
        for id in &changes.delete {
            journal.request().await?;
            provider.delete_record(http, zone, id).await?;
        }
        for record in &changes.create {
            journal.request().await?;
            provider.upsert(http, zone, name, None, record).await?;
        }
        journal.request().await?;
        provider.publish(http, zone).await?;
        Ok(Written::Changes)
    }
}

#[cfg(test)]
mod tests;
