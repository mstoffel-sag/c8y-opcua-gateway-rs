//! OPC UA session lifecycle, node resolution and cyclic read.
//!
//! Reconnect is `async-opcua`'s own: the session event loop retries indefinitely and re-activates
//! the session, so callers see requests fail while a server is away and succeed again afterwards
//! without any supervision of their own.
#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use opcua::client::{Client, ClientBuilder, DataChangeCallback, IdentityToken, Session};
use opcua::types::{
    AttributeId, BrowsePath, DataChangeFilter, DataChangeTrigger, DataValue, DeadbandType,
    EndpointDescription, ExpandedNodeId, ExtensionObject, MessageSecurityMode,
    MonitoredItemCreateRequest, MonitoringMode, MonitoringParameters, NodeId, NumericRange,
    QualifiedName, ReadValueId, RelativePath, RelativePathElement, StatusCode, TimestampsToReturn,
    UserTokenPolicy, VariableId, Variant,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Reference type used to walk a browse path: `HierarchicalReferences`, subtypes included.
const HIERARCHICAL_REFERENCES: u32 = 33;

/// `remaining_path_index` value a server reports for a fully resolved browse path.
const PATH_FULLY_RESOLVED: u32 = u32::MAX;

/// Rate limit on the "notifications dropped" warning.
const DROP_REPORT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum ConnError {
    #[error("invalid OPC UA client configuration: {0}")]
    Config(String),
    #[error("OPC UA request failed: {0}")]
    Request(#[from] opcua::types::Error),
    #[error("server did not return a namespace array")]
    NoNamespaceArray,
    #[error("timed out after {0:?} waiting for the OPC UA session to activate")]
    ConnectTimeout(Duration),
    #[error("invalid numeric range `{0}`")]
    InvalidRange(String),
}

/// How to reach and authenticate against one OPC UA server.
#[derive(Debug, Clone)]
pub struct ServerEndpoint {
    /// Server id as used in device types (`referencedServerId`).
    pub id: String,
    pub url: String,
    /// `None`, `Basic256Sha256`, … — the OPC UA security policy short name.
    pub security_policy: String,
    pub message_security_mode: MessageSecurityMode,
    pub user: Option<Credentials>,
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub user: String,
    pub password: String,
}

/// A live session plus the event loop task keeping it alive.
pub struct Connection {
    server_id: String,
    session: Arc<Session>,
    event_loop: JoinHandle<opcua::types::StatusCode>,
}

impl Connection {
    /// Connect and wait for the session to activate.
    pub async fn connect(
        endpoint: &ServerEndpoint,
        application_name: &str,
        connect_timeout: Duration,
    ) -> Result<Self, ConnError> {
        let mut client: Client = ClientBuilder::new()
            .application_name(application_name)
            .application_uri(format!("urn:{application_name}"))
            // The gateway writes nothing to disk, so it brings no keypair of its own. Secured
            // endpoints need one; configure `pki_dir` and a certificate when you use them.
            .create_sample_keypair(false)
            .trust_server_certs(true)
            .verify_server_certs(false)
            .session_retry_limit(-1)
            .session_retry_initial(Duration::from_secs(1))
            .session_retry_max(Duration::from_secs(30))
            .session_name(application_name)
            .client()
            .map_err(|errors| ConnError::Config(errors.join(", ")))?;

        let identity = match &endpoint.user {
            Some(credentials) => {
                IdentityToken::new_user_name(credentials.user.clone(), credentials.password.clone())
            }
            None => IdentityToken::Anonymous,
        };

        let description: EndpointDescription = (
            endpoint.url.as_str(),
            endpoint.security_policy.as_str(),
            endpoint.message_security_mode,
            UserTokenPolicy::anonymous(),
        )
            .into();

        // `session_retry_limit(-1)` also governs the *first* connection, so this call retries a
        // bad endpoint forever and never returns. That has to be bounded here, or the caller can
        // neither report the failure nor be cancelled — an unresolvable host would pin the task
        // for the lifetime of the process.
        let (session, event_loop) = tokio::time::timeout(
            connect_timeout,
            client.connect_to_matching_endpoint(description, identity),
        )
        .await
        .map_err(|_| ConnError::ConnectTimeout(connect_timeout))??;
        let event_loop = event_loop.spawn();

        if tokio::time::timeout(connect_timeout, session.wait_for_connection())
            .await
            .is_err()
        {
            event_loop.abort();
            return Err(ConnError::ConnectTimeout(connect_timeout));
        }

        info!(
            server_id = endpoint.id,
            url = endpoint.url,
            "OPC UA session active"
        );
        Ok(Self {
            server_id: endpoint.id.clone(),
            session,
            event_loop,
        })
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    /// Read `Server_NamespaceArray`, which every browse path and `nsu=` NodeId resolves against.
    pub async fn namespace_array(&self) -> Result<Vec<String>, ConnError> {
        let node = ReadValueId::from(NodeId::from(VariableId::Server_NamespaceArray));
        let values = self
            .session
            .read(&[node], TimestampsToReturn::Neither, 0.0)
            .await?;

        match values.into_iter().next().and_then(|v| v.value) {
            Some(Variant::Array(array)) => Ok(array
                .values
                .iter()
                .map(|v| match v {
                    Variant::String(s) => s.as_ref().to_owned(),
                    other => format!("{other:?}"),
                })
                .collect()),
            _ => Err(ConnError::NoNamespaceArray),
        }
    }

    /// Resolve browse paths below `root` in a single `TranslateBrowsePathsToNodeIds` request.
    ///
    /// The returned vector is positionally aligned with `paths`; a path the server could not
    /// resolve comes back as `None` rather than failing the whole batch.
    pub async fn translate_browse_paths(
        &self,
        root: &NodeId,
        paths: &[Vec<QualifiedName>],
    ) -> Result<Vec<Option<NodeId>>, ConnError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }

        let requests: Vec<BrowsePath> = paths
            .iter()
            .map(|path| BrowsePath {
                starting_node: root.clone(),
                relative_path: RelativePath {
                    elements: Some(
                        path.iter()
                            .map(|name| RelativePathElement {
                                reference_type_id: NodeId::new(0, HIERARCHICAL_REFERENCES),
                                is_inverse: false,
                                include_subtypes: true,
                                target_name: name.clone(),
                            })
                            .collect(),
                    ),
                },
            })
            .collect();

        let results = self
            .session
            .translate_browse_paths_to_node_ids(&requests)
            .await?;
        debug!(
            server_id = self.server_id,
            requested = requests.len(),
            returned = results.len(),
            "translated browse paths"
        );

        Ok(paths
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let result = results.get(i)?;
                if !result.status_code.is_good() {
                    return None;
                }
                let resolved: Vec<&NodeId> = result
                    .targets
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .filter(|t| t.remaining_path_index == PATH_FULLY_RESOLVED)
                    .map(|t| &t.target_id.node_id)
                    .collect();
                // A browse path may legitimately resolve to several nodes. This gateway binds one
                // node per mapping entry, so the extra targets are dropped — but silently
                // choosing one of several is worth saying out loud, because it usually means the
                // path is shorter or less specific than the author intended.
                if resolved.len() > 1 {
                    warn!(
                        server_id = self.server_id,
                        targets = resolved.len(),
                        chosen = %resolved[0],
                        "browse path resolved to more than one node; using the first and ignoring \
                         the rest — make the path more specific to bind the intended node"
                    );
                }
                result
                    .targets
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .find(|t| t.remaining_path_index == PATH_FULLY_RESOLVED)
                    .and_then(|t| to_node_id(&t.target_id))
            })
            .collect())
    }

    /// Read every node in one request. Positionally aligned with `node_ids`.
    pub async fn read_values(
        &self,
        node_ids: &[NodeId],
        max_age: f64,
    ) -> Result<Vec<DataValue>, ConnError> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        let nodes: Vec<ReadValueId> = node_ids.iter().cloned().map(ReadValueId::from).collect();
        Ok(self
            .session
            .read(&nodes, TimestampsToReturn::Both, max_age)
            .await?)
    }

    /// Close the session and stop its event loop.
    pub async fn disconnect(&self) {
        if let Err(error) = self.session.disconnect().await {
            warn!(server_id = self.server_id, %error, "error closing OPC UA session");
        }
        self.event_loop.abort();
    }
}

/// A target NodeId is only usable when the server expressed it by namespace index.
fn to_node_id(expanded: &ExpandedNodeId) -> Option<NodeId> {
    if !expanded.namespace_uri.is_empty() {
        warn!(
            namespace_uri = expanded.namespace_uri.as_ref(),
            "server returned a browse path target by namespace uri; ignoring the target"
        );
        return None;
    }
    Some(expanded.node_id.clone())
}

/// Subscription-level parameters, shared by every monitored item on a server.
///
/// One subscription per server, as the Java gateway does; only the monitored items differ per
/// device type. Defaults match `gateway.subscription.*` there.
#[derive(Debug, Clone)]
pub struct SubscriptionParams {
    pub publishing_interval: Duration,
    pub lifetime_count: u32,
    pub max_keep_alive_count: u32,
    pub max_notifications_per_publish: u32,
    pub priority: u8,
}

impl Default for SubscriptionParams {
    fn default() -> Self {
        Self {
            publishing_interval: Duration::from_millis(100),
            lifetime_count: 600,
            max_keep_alive_count: 200,
            max_notifications_per_publish: 500,
            priority: 0,
        }
    }
}

/// One monitored item to create.
#[derive(Debug, Clone)]
pub struct MonitoredItemSpec {
    pub node_id: NodeId,
    /// Requested sampling interval in milliseconds; `None` lets the server pick.
    pub sampling_interval_ms: Option<f64>,
    pub queue_size: u32,
    pub discard_oldest: bool,
    pub data_change_trigger: DataChangeTrigger,
    /// Deadband filter. `None` subscribes without one.
    pub deadband: Option<(DeadbandType, f64)>,
    /// OPC UA numeric range, for monitoring a slice of an array node.
    pub index_range: Option<String>,
}

/// A value delivered by a subscription.
#[derive(Debug, Clone)]
pub struct DataChange {
    /// Handle of the monitored item that produced this value.
    ///
    /// The handle, not the NodeId, identifies the item: several monitored items may watch the
    /// same node — one per device type that maps it — and each belongs to a different thin-edge
    /// device.
    pub client_handle: u32,
    pub node_id: NodeId,
    pub value: DataValue,
}

impl Connection {
    /// Create this server's subscription and stream its data changes into `sink`.
    ///
    /// The `async-opcua` notification callback is synchronous, so it hands values to `sink`
    /// without blocking: a full channel means the publisher cannot keep up, and the value is
    /// dropped with a rate-limited warning rather than growing a queue without bound.
    pub async fn create_subscription(
        &self,
        params: &SubscriptionParams,
        sink: mpsc::Sender<DataChange>,
    ) -> Result<u32, ConnError> {
        let server_id = self.server_id.clone();
        let mut dropped: u64 = 0;
        let mut last_report = std::time::Instant::now();

        let callback = DataChangeCallback::new(move |value, item| {
            let change = DataChange {
                client_handle: item.client_handle(),
                node_id: item.item_to_monitor().node_id.clone(),
                value,
            };
            if sink.try_send(change).is_err() {
                dropped += 1;
                if last_report.elapsed() >= DROP_REPORT_INTERVAL {
                    warn!(
                        server_id,
                        dropped,
                        "dropping subscription notifications: the thin-edge publisher cannot keep up"
                    );
                    dropped = 0;
                    last_report = std::time::Instant::now();
                }
            }
        });

        let id = self
            .session
            .create_subscription(
                params.publishing_interval,
                params.lifetime_count,
                params.max_keep_alive_count,
                params.max_notifications_per_publish,
                params.priority,
                true,
                callback,
            )
            .await?;
        info!(
            server_id = self.server_id,
            subscription_id = id,
            publishing_interval_ms = params.publishing_interval.as_millis(),
            "created OPC UA subscription"
        );
        Ok(id)
    }

    /// Add monitored items to a subscription.
    ///
    /// The result is positionally aligned with `items`: a created item yields the client handle
    /// its notifications will carry, and an item the server refused yields its status code, so
    /// one bad node cannot cost the whole device type its subscription.
    pub async fn create_monitored_items(
        &self,
        subscription_id: u32,
        items: &[MonitoredItemSpec],
    ) -> Result<Vec<Result<u32, StatusCode>>, ConnError> {
        if items.is_empty() {
            return Ok(Vec::new());
        }

        let mut requests = Vec::with_capacity(items.len());
        for item in items {
            let index_range = match item.index_range.as_deref() {
                Some(range) => range
                    .parse::<NumericRange>()
                    .map_err(|_| ConnError::InvalidRange(range.to_owned()))?,
                None => NumericRange::None,
            };
            requests.push(MonitoredItemCreateRequest {
                item_to_monitor: ReadValueId {
                    node_id: item.node_id.clone(),
                    attribute_id: AttributeId::Value as u32,
                    index_range,
                    data_encoding: QualifiedName::null(),
                },
                monitoring_mode: MonitoringMode::Reporting,
                requested_parameters: MonitoringParameters {
                    // Left at zero so the session assigns a unique client handle.
                    client_handle: 0,
                    sampling_interval: item.sampling_interval_ms.unwrap_or(-1.0),
                    filter: data_change_filter(item),
                    queue_size: item.queue_size,
                    discard_oldest: item.discard_oldest,
                },
            });
        }

        let created = self
            .session
            .create_monitored_items(subscription_id, TimestampsToReturn::Both, requests)
            .await?;

        Ok(items
            .iter()
            .enumerate()
            .map(|(i, _)| match created.get(i) {
                Some(item) if item.result.status_code.is_good() => {
                    Ok(item.requested_parameters.client_handle)
                }
                Some(item) => Err(item.result.status_code),
                None => Err(StatusCode::BadUnexpectedError),
            })
            .collect())
    }
}

/// The item's `DataChangeFilter`, or an empty extension object when no filter is needed.
///
/// A `StatusValue` trigger with no deadband is the OPC UA default, so sending no filter at all
/// keeps the request smaller and avoids servers that reject filters they consider redundant.
fn data_change_filter(item: &MonitoredItemSpec) -> ExtensionObject {
    let default_trigger =
        item.data_change_trigger == DataChangeTrigger::StatusValue && item.deadband.is_none();
    if default_trigger {
        return ExtensionObject::null();
    }
    let (deadband_type, deadband_value) = item.deadband.unwrap_or((DeadbandType::None, 0.0));
    ExtensionObject::from_message(DataChangeFilter {
        trigger: item.data_change_trigger,
        deadband_type: deadband_type as u32,
        deadband_value,
    })
}
