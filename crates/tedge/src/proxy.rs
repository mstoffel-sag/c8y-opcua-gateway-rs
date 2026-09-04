//! Cumulocity access through the thin-edge proxy.
//!
//! `tedge-mapper-c8y` exposes the Cumulocity REST API on localhost and injects the device's JWT,
//! so there is no `Authorization` header here and no credentials anywhere in this gateway. This is
//! the same thing the Java gateway's `PlatformFactoryThinEdgeProxy` does, with `reqwest` in place
//! of the Cumulocity SDK.
//!
//! Reads only, with exactly one exception: [`C8yProxy::create_external_id`]. A `c8y_OpcuaServer`
//! managed object is created cloud-side by `opcua-mgmt-service` with no external id, which leaves
//! it invisible to thin-edge — telemetry published for it would create a second device. Planting
//! one external id on that object adopts it, after which everything about that server travels over
//! MQTT. It is idempotent and happens once per server, ever.
//!
//! Readings never go this way, and nothing else here writes.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use tracing::{debug, warn};

/// Documented default of `c8y.proxy.bind.address` / `c8y.proxy.bind.port`.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8001/c8y";

/// How long to wait before retrying the proxy's spurious 401 once.
const UNAUTHORIZED_RETRY_DELAY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Base URL of the proxy, including the `/c8y` prefix.
    pub base_url: String,
    pub timeout: Duration,
    /// Client certificate, for a thin-edge deployment that protects its local endpoints with
    /// mTLS (`c8y.proxy.cert_path` / `key_path` on the device).
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
    /// CA that signed the proxy's server certificate (`c8y.proxy.ca_path`).
    pub ca_cert: Option<PathBuf>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            timeout: Duration::from_secs(30),
            client_cert: None,
            client_key: None,
            ca_cert: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("failed to build the proxy HTTP client: {0}")]
    Client(#[source] reqwest::Error),
    #[error("cannot read {path}: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("request to {url} failed: {source}")]
    Request {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    /// The mapper could not reach Cumulocity. Keep running on what is already in memory.
    #[error("the thin-edge mapper cannot reach Cumulocity (502)")]
    CloudUnreachable,
    #[error("{url} returned {status}")]
    Status { url: String, status: u16 },
}

impl ProxyError {
    /// Whether the gateway should keep its current device types and simply retry later.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::CloudUnreachable | Self::Request { .. } => true,
            Self::Status { status, .. } => *status == 401 || *status >= 500,
            Self::Client(_) | Self::ReadFile { .. } => false,
        }
    }
}

#[derive(Clone)]
pub struct C8yProxy {
    http: reqwest::Client,
    base_url: String,
}

impl C8yProxy {
    pub fn new(config: &ProxyConfig) -> Result<Self, ProxyError> {
        let mut builder = reqwest::Client::builder()
            .timeout(config.timeout)
            .user_agent(concat!("c8y-opcua-gateway/", env!("CARGO_PKG_VERSION")));

        if let (Some(cert), Some(key)) = (&config.client_cert, &config.client_key) {
            let mut pem = read(cert)?;
            pem.extend_from_slice(&read(key)?);
            builder =
                builder.identity(reqwest::Identity::from_pem(&pem).map_err(ProxyError::Client)?);
        }
        if let Some(ca) = &config.ca_cert {
            for cert in
                reqwest::Certificate::from_pem_bundle(&read(ca)?).map_err(ProxyError::Client)?
            {
                builder = builder.add_root_certificate(cert);
            }
        }

        Ok(Self {
            http: builder.build().map_err(ProxyError::Client)?,
            base_url: config.base_url.trim_end_matches('/').to_owned(),
        })
    }

    /// One page of managed objects of the given type, with the device type fragment.
    pub async fn device_types(
        &self,
        mo_type: &str,
        fragment_type: &str,
        page: u32,
        page_size: u32,
    ) -> Result<Value, ProxyError> {
        let url = format!(
            "{}/inventory/managedObjects?type={mo_type}&fragmentType={fragment_type}\
             &pageSize={page_size}&currentPage={page}&withTotalPages=true",
            self.base_url
        );
        self.get_json(&url).await
    }

    /// One managed object by id, for the `serverObjectHasFragment` constraint.
    pub async fn managed_object(&self, id: &str) -> Result<Value, ProxyError> {
        let url = format!("{}/inventory/managedObjects/{id}", self.base_url);
        self.get_json(&url).await
    }

    /// One page of an object's child devices, for reading the servers registered on the gateway.
    pub async fn child_devices(
        &self,
        id: &str,
        page: u32,
        page_size: u32,
    ) -> Result<Value, ProxyError> {
        let url = format!(
            "{}/inventory/managedObjects/{id}/childDevices\
             ?pageSize={page_size}&currentPage={page}&withTotalPages=true",
            self.base_url
        );
        self.get_json(&url).await
    }

    /// Resolve an external id to a managed object id, or `None` when it does not exist.
    pub async fn managed_object_id(
        &self,
        id_type: &str,
        external_id: &str,
    ) -> Result<Option<String>, ProxyError> {
        let url = format!(
            "{}/identity/externalIds/{id_type}/{}",
            self.base_url,
            urlencode(external_id)
        );
        match self.get_json(&url).await {
            Ok(value) => Ok(value
                .get("managedObject")
                .and_then(|mo| mo.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned)),
            Err(ProxyError::Status { status: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Plant an external id on an existing managed object.
    ///
    /// The one write in this gateway; see the module documentation for why it exists. A `409` is
    /// success: the external id is already there, which is the state this asks for.
    pub async fn create_external_id(
        &self,
        mo_id: &str,
        id_type: &str,
        external_id: &str,
    ) -> Result<(), ProxyError> {
        let url = format!("{}/identity/globalIds/{mo_id}/externalIds", self.base_url);
        let body = serde_json::json!({ "type": id_type, "externalId": external_id });
        match self.post_json(&url, &body).await {
            Ok(()) | Err(ProxyError::Status { status: 409, .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// GET with the proxy's one documented quirk handled: a spurious 401 from Cumulocity's JWT
    /// handling is forwarded verbatim, so it is retried once. There are no credentials here, so a
    /// 401 is never a credential problem.
    async fn get_json(&self, url: &str) -> Result<Value, ProxyError> {
        match self.get_json_once(url).await {
            Err(ProxyError::Status { status: 401, .. }) => {
                debug!(url, "proxy returned 401; retrying once");
                tokio::time::sleep(UNAUTHORIZED_RETRY_DELAY).await;
                self.get_json_once(url).await
            }
            other => other,
        }
    }

    /// POST with the same one-shot 401 retry as [`Self::get_json`].
    async fn post_json(&self, url: &str, body: &Value) -> Result<(), ProxyError> {
        match self.post_json_once(url, body).await {
            Err(ProxyError::Status { status: 401, .. }) => {
                debug!(url, "proxy returned 401; retrying once");
                tokio::time::sleep(UNAUTHORIZED_RETRY_DELAY).await;
                self.post_json_once(url, body).await
            }
            other => other,
        }
    }

    async fn post_json_once(&self, url: &str, body: &Value) -> Result<(), ProxyError> {
        let response = self
            .http
            .post(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(body)
            .send()
            .await
            .map_err(|source| ProxyError::Request {
                url: url.to_owned(),
                source,
            })?;

        let status = response.status();
        if status == reqwest::StatusCode::BAD_GATEWAY {
            return Err(ProxyError::CloudUnreachable);
        }
        if !status.is_success() {
            return Err(ProxyError::Status {
                url: url.to_owned(),
                status: status.as_u16(),
            });
        }
        Ok(())
    }

    async fn get_json_once(&self, url: &str) -> Result<Value, ProxyError> {
        let response = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|source| ProxyError::Request {
                url: url.to_owned(),
                source,
            })?;

        let status = response.status();
        if status == reqwest::StatusCode::BAD_GATEWAY {
            return Err(ProxyError::CloudUnreachable);
        }
        if !status.is_success() {
            return Err(ProxyError::Status {
                url: url.to_owned(),
                status: status.as_u16(),
            });
        }

        response.json().await.map_err(|source| {
            warn!(url, "proxy response was not valid JSON");
            ProxyError::Request {
                url: url.to_owned(),
                source,
            }
        })
    }
}

/// Percent-encode one path segment. External ids carry `:` and whatever the tenant put in a
/// device name, so they cannot go into a URL raw.
fn urlencode(segment: &str) -> String {
    percent_encoding::utf8_percent_encode(segment, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn read(path: &PathBuf) -> Result<Vec<u8>, ProxyError> {
    std::fs::read(path).map_err(|source| ProxyError::ReadFile {
        path: path.clone(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_outages_and_401s_are_transient() {
        assert!(ProxyError::CloudUnreachable.is_transient());
        assert!(
            ProxyError::Status {
                url: "u".into(),
                status: 401
            }
            .is_transient()
        );
        assert!(
            !ProxyError::Status {
                url: "u".into(),
                status: 404
            }
            .is_transient()
        );
    }
}
