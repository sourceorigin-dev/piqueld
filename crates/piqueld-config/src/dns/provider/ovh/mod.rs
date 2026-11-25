//! OVH API v1 with signed requests.
//!
//! Every call is signed with the application secret and consumer key over the
//! method, URL, body and the API's own clock:
//!
//! ```text
//! X-Ovh-Signature: "$1$" + hex(sha1(secret + "+" + consumer + "+" + METHOD + "+" + url + "+" + body + "+" + time))
//! ```
use super::{ApiError, Found, Provider, Record, RecordId, Zone, send};
use crate::{Credential, CredentialError, CredentialFile, schema_from_raw};
use piqueld_core::manifest::Hostname;
use reqwest::Method;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// How long the API's clock is trusted before `/auth/time` is read again.
const CLOCK: Duration = Duration::from_hours(1);
/// The record types an apex read lists one at a time.
const APEX_TYPES: [&str; 4] = ["A", "AAAA", "CNAME", "TXT"];

/// An OVH API region.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OvhEndpoint {
    /// OVH Europe.
    OvhEu,
    /// OVH Canada.
    OvhCa,
    /// OVH US.
    OvhUs,
}

impl OvhEndpoint {
    /// API base URL of the region.
    fn url(self) -> &'static str {
        match self {
            Self::OvhEu => "https://eu.api.ovh.com/1.0",
            Self::OvhCa => "https://ca.api.ovh.com/1.0",
            Self::OvhUs => "https://api.us.ovhcloud.com/1.0",
        }
    }
}

/// `ovh-eu`, as written in daemon TOML.
impl std::fmt::Display for OvhEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::OvhEu => "ovh-eu",
            Self::OvhCa => "ovh-ca",
            Self::OvhUs => "ovh-us",
        })
    }
}

/// One OVH account, configured as:
///
/// ```toml
/// [[dns.providers]]
/// kind = "ovh"
/// endpoint = "ovh-eu"
/// application_key_file = "ovh-application-key"
/// application_secret_file = "ovh-application-secret"
/// consumer_key_file = "ovh-consumer-key"
/// ```
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(try_from = "RawOvh")]
pub struct Ovh {
    /// API region.
    pub endpoint: OvhEndpoint,
    /// Identifies the application, sent with every request.
    pub application_key: Credential,
    /// Signs requests; never sent.
    pub application_secret: Credential,
    /// Authorizes the application on the account.
    pub consumer_key: Credential,
    /// API base URL; tests point it at recorded responses.
    api: String,
    clock: Clock,
}

/// The API's clock: its time when last read, and when that was. Signatures
/// use it, so local skew cannot invalidate them.
#[derive(Clone, Debug, Default)]
struct Clock(Arc<parking_lot::Mutex<Option<(i64, Instant)>>>);

/// A cache, not configuration: every clock is equal.
impl PartialEq for Clock {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Clock {}

/// An OVH account.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct RawOvh {
    /// The API region of the account: `ovh-eu` (Europe), `ovh-ca` (Canada)
    /// or `ovh-us` (United States).
    endpoint: OvhEndpoint,
    /// The application key, which identifies the application.
    application_key_file: CredentialFile,
    /// The application secret, which signs requests.
    application_secret_file: CredentialFile,
    /// The consumer key, which authorizes the application on the account.
    consumer_key_file: CredentialFile,
}

schema_from_raw!(Ovh => RawOvh);

impl TryFrom<RawOvh> for Ovh {
    type Error = CredentialError;
    fn try_from(raw: RawOvh) -> Result<Self, Self::Error> {
        Ok(Self {
            endpoint: raw.endpoint,
            application_key: Credential::read("application_key", raw.application_key_file)?,
            application_secret: Credential::read(
                "application_secret",
                raw.application_secret_file,
            )?,
            consumer_key: Credential::read("consumer_key", raw.consumer_key_file)?,
            api: raw.endpoint.url().into(),
            clock: Clock::default(),
        })
    }
}

/// A record as OVH describes it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OvhRecord {
    id: u64,
    #[serde(default)]
    field_type: String,
    #[serde(default)]
    sub_domain: String,
    #[serde(default)]
    target: String,
}

impl Ovh {
    /// Signature of one request at API time `time`.
    fn signature(&self, method: &Method, url: &str, body: &str, time: i64) -> String {
        let payload = format!(
            "{}+{}+{method}+{url}+{body}+{time}",
            self.application_secret.expose(),
            self.consumer_key.expose()
        );
        format!("$1${}", hex::encode(openssl::sha::sha1(payload.as_bytes())))
    }

    /// The API's current time, read from `/auth/time` at most once per
    /// [`CLOCK`] and advanced by the local monotonic clock in between.
    async fn time(&self, http: &reqwest::Client) -> Result<i64, ApiError> {
        let cached = *self.clock.0.lock();
        let (time, read_at) = match cached {
            Some((time, read_at)) if read_at.elapsed() < CLOCK => (time, read_at),
            _ => {
                let time: i64 = send(http.get(format!("{}/auth/time", self.api))).await?;
                let read = (time, Instant::now());
                *self.clock.0.lock() = Some(read);
                read
            }
        };
        let elapsed = i64::try_from(read_at.elapsed().as_secs()).unwrap_or(i64::MAX);
        Ok(time.saturating_add(elapsed))
    }

    /// Sends a signed request, using the API's clock so local skew cannot
    /// invalidate signatures.
    async fn call<T: DeserializeOwned>(
        &self,
        http: &reqwest::Client,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T, ApiError> {
        let time = self.time(http).await?;
        let url = format!("{}{path}", self.api);
        let body = body.map_or_default(Value::to_string);
        let signature = self.signature(&method, &url, &body, time);
        send(
            http.request(method, url)
                .header("X-Ovh-Application", self.application_key.expose())
                .header("X-Ovh-Consumer", self.consumer_key.expose())
                .header("X-Ovh-Timestamp", time.to_string())
                .header("X-Ovh-Signature", signature)
                .header("Content-Type", "application/json")
                .body(body),
        )
        .await
    }
}

impl Provider for Ovh {
    fn kind(&self) -> &'static str {
        "ovh"
    }

    /// Lists zone names. Names that are not public hostnames are skipped.
    async fn zones(&self, http: &reqwest::Client) -> Result<Vec<Zone>, ApiError> {
        let names: Vec<String> = self.call(http, Method::GET, "/domain/zone", None).await?;
        Ok(names
            .into_iter()
            .filter_map(|name| {
                Some(Zone {
                    name: Hostname::parse(&name).ok()?,
                    id: name,
                })
            })
            .collect())
    }

    /// Lists the records named exactly `name`: their IDs, then each record.
    /// OVH names records relative to their zone; the apex is the empty name.
    async fn records(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        name: &str,
    ) -> Result<Vec<Found>, ApiError> {
        let subdomain = zone.relative(name);
        let list = format!("/domain/zone/{}/record?subDomain={subdomain}", zone.id);
        let lists = if subdomain.is_empty() {
            APEX_TYPES
                .map(|kind| format!("{list}&fieldType={kind}"))
                .to_vec()
        } else {
            vec![list]
        };
        let mut ids: Vec<u64> = Vec::new();
        for list in lists {
            ids.extend(
                self.call::<Vec<u64>>(http, Method::GET, &list, None)
                    .await?,
            );
        }
        let mut found = Vec::new();
        for id in ids {
            let record: OvhRecord = self
                .call(
                    http,
                    Method::GET,
                    &format!("/domain/zone/{}/record/{id}", zone.id),
                    None,
                )
                .await?;
            if record.sub_domain != subdomain {
                continue;
            }
            if let Some(parsed) = Record::parse(&record.field_type, &record.target, false)? {
                found.push(Found {
                    id: RecordId(record.id.to_string()),
                    record: parsed,
                    proxied: false,
                });
            }
        }
        Ok(found)
    }

    /// Creates a record, or replaces record `id` of the same type, published
    /// by [`Self::publish`]. CNAME targets get a trailing dot, so OVH does not
    /// read them as relative to the zone. OVH cannot proxy records.
    async fn upsert(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        name: &str,
        id: Option<&RecordId>,
        record: &Record,
    ) -> Result<RecordId, ApiError> {
        let target = match record {
            Record::Cname { proxied: true, .. } => {
                return Err(ApiError::Unsupported(
                    "OVH cannot proxy records; a proxied CNAME needs a Cloudflare zone",
                ));
            }
            Record::Cname { target, .. } => format!("{target}."),
            record => record.content(),
        };
        let subdomain = zone.relative(name);
        let records = format!("/domain/zone/{}/record", zone.id);
        if let Some(id) = id {
            self.call::<Value>(
                http,
                Method::PUT,
                &format!("{records}/{}", id.0),
                Some(&json!({"subDomain":subdomain,"target":target,"ttl":record.ttl()})),
            )
            .await?;
            return Ok(id.clone());
        }
        let created: OvhRecord = self
            .call(
                http,
                Method::POST,
                &records,
                Some(&json!({"fieldType":record.kind(),"subDomain":subdomain,"target":target,"ttl":record.ttl()})),
            )
            .await?;
        Ok(RecordId(created.id.to_string()))
    }

    /// Deletes a record by its ID, published by [`Self::publish`].
    async fn delete_record(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        id: &RecordId,
    ) -> Result<(), ApiError> {
        self.call::<Value>(
            http,
            Method::DELETE,
            &format!("/domain/zone/{}/record/{}", zone.id, id.0),
            None,
        )
        .await?;
        Ok(())
    }

    fn stages_changes(&self) -> bool {
        true
    }

    /// Applies pending record changes to the zone's nameservers.
    async fn publish(&self, http: &reqwest::Client, zone: &Zone) -> Result<(), ApiError> {
        self.call::<Value>(
            http,
            Method::POST,
            &format!("/domain/zone/{}/refresh", zone.id),
            None,
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
