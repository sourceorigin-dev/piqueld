//! Cloudflare API v4 with a bearer token scoped to Zone:Read and DNS:Edit.
use super::{ApiError, Found, Provider, Record, RecordId, Zone, send};
use crate::{Credential, CredentialError, CredentialFile, schema_from_raw};
use piqueld_core::manifest::Hostname;
use serde::Deserialize;
use serde_json::json;

/// API base URL.
const API: &str = "https://api.cloudflare.com/client/v4";
/// Pages followed on a list endpoint: 5,000 zones, or 10,000 records of one
/// name.
const MAX_PAGES: u32 = 100;

/// One Cloudflare account, configured as:
///
/// ```toml
/// [[dns.providers]]
/// kind = "cloudflare"
/// api_token_file = "cloudflare-dns-token"
/// ```
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(try_from = "RawCloudflare")]
pub struct Cloudflare {
    api_token: Credential,
    /// API base URL; tests point it at recorded responses.
    api: String,
}

/// A Cloudflare account.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct RawCloudflare {
    /// An API token scoped to Zone:Read and DNS:Edit on the zones piqueld
    /// may use.
    api_token_file: CredentialFile,
}

schema_from_raw!(Cloudflare => RawCloudflare);

impl TryFrom<RawCloudflare> for Cloudflare {
    type Error = CredentialError;
    fn try_from(raw: RawCloudflare) -> Result<Self, Self::Error> {
        Ok(Self {
            api_token: Credential::read("api_token", raw.api_token_file)?,
            api: API.into(),
        })
    }
}

/// Response envelope shared by every endpoint.
#[derive(Deserialize)]
struct Envelope<T> {
    result: T,
    result_info: Option<ResultInfo>,
}

/// Pagination of list endpoints.
#[derive(Deserialize)]
struct ResultInfo {
    page: u32,
    total_pages: u32,
}

/// An object identified by `id`, such as a zone or a record.
#[derive(Deserialize)]
struct Object {
    id: String,
    #[serde(default)]
    name: String,
}

/// A DNS record as listed.
#[derive(Deserialize)]
struct ListedRecord {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    content: String,
    #[serde(default)]
    proxied: bool,
}

impl Cloudflare {
    /// The token file, as shown by read-only settings.
    #[must_use]
    pub fn api_token(&self) -> &Credential {
        &self.api_token
    }

    /// Sends an authenticated request and unwraps the response envelope.
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<Envelope<T>, ApiError> {
        send(request.bearer_auth(self.api_token.expose())).await
    }

    /// Follows every page of a list endpoint, up to [`MAX_PAGES`]; `path`
    /// ends with its query. A page that is not the one asked for, or empty
    /// before the last, fails rather than looping.
    async fn list<T: serde::de::DeserializeOwned>(
        &self,
        http: &reqwest::Client,
        path: &str,
    ) -> Result<Vec<T>, ApiError> {
        let mut items = Vec::new();
        for page in 1..=MAX_PAGES {
            let response: Envelope<Vec<T>> = self
                .call(http.get(format!("{}{path}&page={page}", self.api)))
                .await?;
            let empty = response.result.is_empty();
            items.extend(response.result);
            match response.result_info {
                None => return Ok(items),
                Some(info) if info.page != page => return Err(ApiError::Pagination(page)),
                Some(info) if info.page >= info.total_pages => return Ok(items),
                Some(_) if empty => return Err(ApiError::Pagination(page)),
                Some(_) => {}
            }
        }
        Err(ApiError::TooLarge("more than 100 pages"))
    }
}

impl Provider for Cloudflare {
    fn kind(&self) -> &'static str {
        "cloudflare"
    }

    fn proxies(&self) -> bool {
        true
    }

    /// Lists zones 50 per page. Names that are not public hostnames are skipped.
    async fn zones(&self, http: &reqwest::Client) -> Result<Vec<Zone>, ApiError> {
        let zones: Vec<Object> = self.list(http, "/zones?per_page=50").await?;
        Ok(zones
            .into_iter()
            .filter_map(|zone| {
                Some(Zone {
                    name: Hostname::parse(&zone.name).ok()?,
                    id: zone.id,
                })
            })
            .collect())
    }

    /// Lists the records named exactly `name`, 100 per page.
    async fn records(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        name: &str,
    ) -> Result<Vec<Found>, ApiError> {
        let listed: Vec<ListedRecord> = self
            .list(
                http,
                &format!(
                    "/zones/{}/dns_records?name.exact={name}&per_page=100",
                    zone.id
                ),
            )
            .await?;
        let mut found = Vec::new();
        for record in listed {
            if let Some(parsed) = Record::parse(&record.kind, &record.content, record.proxied)? {
                found.push(Found {
                    id: RecordId(record.id),
                    record: parsed,
                    proxied: record.proxied,
                });
            }
        }
        Ok(found)
    }

    /// Creates a record, or replaces record `id`. TXT content is quoted, as
    /// Cloudflare expects, and proxied records use its automatic TTL.
    async fn upsert(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        name: &str,
        id: Option<&RecordId>,
        record: &Record,
    ) -> Result<RecordId, ApiError> {
        let body = match record {
            Record::Txt(text) => {
                json!({"type":"TXT","name":name,"content":format!("\"{text}\""),"ttl":record.ttl()})
            }
            Record::Cname { proxied: true, .. } => {
                json!({"type":"CNAME","name":name,"content":record.content(),"ttl":1,"proxied":true})
            }
            _ => {
                json!({"type":record.kind(),"name":name,"content":record.content(),"ttl":record.ttl(),"proxied":false})
            }
        };
        let records = format!("{}/zones/{}/dns_records", self.api, zone.id);
        let request = match id {
            Some(id) => http.put(format!("{records}/{}", id.0)),
            None => http.post(records),
        };
        let response: Envelope<Object> = self.call(request.json(&body)).await?;
        Ok(RecordId(response.result.id))
    }

    /// Deletes a record by its ID.
    async fn delete_record(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        id: &RecordId,
    ) -> Result<(), ApiError> {
        self.call::<serde_json::Value>(http.delete(format!(
            "{}/zones/{}/dns_records/{}",
            self.api, zone.id, id.0
        )))
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
