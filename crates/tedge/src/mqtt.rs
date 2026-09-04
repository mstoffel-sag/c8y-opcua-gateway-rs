//! Publishing to the local thin-edge MQTT broker.
//!
//! Everything the gateway emits goes through here at QoS 1. mosquitto's persistent queue and the
//! thin-edge bridge own store-and-forward, so there is no offline buffer of our own — an
//! unreachable broker simply back-pressures the bounded channel this client publishes from.

use std::time::Duration;

use rumqttc::{AsyncClient, ConnectionError, Event, EventLoop, LastWill, MqttOptions, QoS};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::topic::EntityTopicId;

/// Entity `type` reported for the gateway's own service entity.
const SERVICE_TYPE: &str = "service";

/// Entity `type` reported for each OPC UA server.
pub const OPCUA_SERVER_ENTITY_TYPE: &str = "opcua-server";

/// Entity `type` reported for each device type applied to a server.
///
/// The Java gateway's equivalent managed object type is `c8y_OpcuaDevice`.
pub const OPCUA_DEVICE_ENTITY_TYPE: &str = "opcua-device";

/// Cumulocity managed object types, used as the entity `type` when the gateway device is enabled.
///
/// thin-edge passes a child device's `type` straight through to the managed object, so these are
/// what make the existing OPC UA user interface recognise the objects this gateway registers.
/// Nothing else about them is special.
pub const GATEWAY_MO_TYPE: &str = "c8y_OPCUA_Device_Agent";
pub const OPCUA_SERVER_MO_TYPE: &str = "c8y_OpcuaServer";
pub const OPCUA_DEVICE_MO_TYPE: &str = "c8y_OpcuaDevice";

/// The bridged Cumulocity topics this gateway uses when the gateway device is enabled.
///
/// Operations are delivered here rather than on a `te/…/cmd/` topic because `c8y_OpcuaConfiguration`
/// is a Cumulocity operation name with no thin-edge command equivalent, and declaring one would
/// mean writing a file under `/etc/tedge/operations/c8y/<external-id>/` — a runtime path this
/// gateway will not create. Reading the bridged topic needs neither.
pub const C8Y_OPERATION_TOPIC: &str = "c8y/devicecontrol/notifications";

/// SmartREST publish topic for a child entity, addressed by external id.
fn smartrest_topic(external_id: &str) -> String {
    format!("c8y/s/us/{external_id}")
}

/// One message received from the broker.
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub topic: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct MqttConfig {
    pub host: String,
    pub port: u16,
    pub client_id: String,
    /// Bound on in-flight publishes; also the point at which publishing back-pressures.
    pub capacity: usize,
    pub keep_alive: Duration,
    /// Service name of the gateway on the main device.
    pub service_name: String,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: 1883,
            client_id: "c8y-opcua-gateway".to_owned(),
            capacity: 1024,
            keep_alive: Duration::from_secs(60),
            service_name: "opcua-gateway".to_owned(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MqttError {
    #[error("failed to publish to {topic}: {source}")]
    Publish {
        topic: String,
        #[source]
        source: rumqttc::ClientError,
    },
    #[error("failed to serialize payload: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Publisher for the `te/` topics.
#[derive(Clone)]
pub struct TedgeMqtt {
    client: AsyncClient,
    service: EntityTopicId,
}

impl TedgeMqtt {
    /// Create the client. The returned [`EventLoop`] must be driven by [`run_event_loop`].
    ///
    /// The last will publishes health `down` retained, so a crashed gateway does not leave a
    /// stale `up` behind.
    pub fn new(config: &MqttConfig) -> (Self, EventLoop) {
        let service = EntityTopicId::main_service(&config.service_name);

        let mut options = MqttOptions::new(&config.client_id, &config.host, config.port);
        options.set_keep_alive(config.keep_alive);
        options.set_clean_session(false);
        options.set_last_will(LastWill::new(
            service.channel_topic("status/health"),
            json!({ "status": "down" }).to_string(),
            QoS::AtLeastOnce,
            true,
        ));

        let (client, event_loop) = AsyncClient::new(options, config.capacity);
        (Self { client, service }, event_loop)
    }

    pub fn service(&self) -> &EntityTopicId {
        &self.service
    }

    /// Register the gateway as a service on the main device. Retained, before any data.
    pub async fn register_service(&self) -> Result<(), MqttError> {
        let name = self
            .service
            .as_str()
            .rsplit('/')
            .next()
            .unwrap_or("opcua-gateway")
            .to_owned();
        self.publish_retained(
            &self.service.registration_topic(),
            &json!({
                "@type": SERVICE_TYPE,
                "@parent": EntityTopicId::main_device().as_str(),
                "name": name,
                "type": "systemd",
            }),
        )
        .await
    }

    /// Register a child device below `parent`. Retained, before any data.
    ///
    /// `parent` may itself be a child device: thin-edge nests entities by `@parent`, which is how
    /// a device type applied to a server becomes a device below that server.
    ///
    /// `external_id` sets `@id`. Leaving it `None` lets thin-edge derive the external id from the
    /// topic path, which is what a device-configured server wants. Setting it is how an existing
    /// managed object is adopted: the same external id is planted on that object, so Cumulocity
    /// resolves the mapper's child-device creation to the object that is already there instead of
    /// making a second one.
    pub async fn register_child_device(
        &self,
        entity: &EntityTopicId,
        parent: &EntityTopicId,
        name: &str,
        r#type: &str,
        external_id: Option<&str>,
    ) -> Result<(), MqttError> {
        let mut payload = json!({
            "@type": "child-device",
            "@parent": parent.as_str(),
            "name": name,
            "type": r#type,
        });
        if let (Some(external_id), Some(map)) = (external_id, payload.as_object_mut()) {
            map.insert("@id".to_owned(), Value::String(external_id.to_owned()));
        }
        self.publish_retained(&entity.registration_topic(), &payload)
            .await
    }

    /// Retained `twin/<fragment>`, which the Cumulocity mapper turns into an inventory fragment of
    /// that name. This is how every `c8y_ua_*` fragment the user interface reads is published
    /// without writing to inventory over HTTP.
    pub async fn publish_twin(
        &self,
        entity: &EntityTopicId,
        fragment: &str,
        payload: &Value,
    ) -> Result<(), MqttError> {
        self.publish_retained(&entity.channel_topic(&format!("twin/{fragment}")), payload)
            .await
    }

    /// Remove a twin fragment: an empty payload, retained.
    pub async fn clear_twin(
        &self,
        entity: &EntityTopicId,
        fragment: &str,
    ) -> Result<(), MqttError> {
        let topic = entity.channel_topic(&format!("twin/{fragment}"));
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, Vec::new())
            .await
            .map_err(|source| MqttError::Publish { topic, source })
    }

    /// Publish one SmartREST record for the entity with this external id.
    ///
    /// Used only to close out Cumulocity operations, which have no thin-edge equivalent. Telemetry
    /// never goes this way — it goes on `te/` topics so the mapper and bridge own it.
    pub async fn publish_smartrest(
        &self,
        external_id: &str,
        record: &str,
    ) -> Result<(), MqttError> {
        let topic = smartrest_topic(external_id);
        debug!(topic, record, "publishing SmartREST");
        self.client
            .publish(&topic, QoS::AtLeastOnce, false, record.as_bytes().to_vec())
            .await
            .map_err(|source| MqttError::Publish { topic, source })
    }

    /// Publish health `up`. Retained, on startup.
    pub async fn publish_health_up(&self) -> Result<(), MqttError> {
        self.publish_retained(
            &self.service.channel_topic("status/health"),
            &json!({
                "status": "up",
                "pid": std::process::id(),
                "time": unix_time(),
            }),
        )
        .await
    }

    pub async fn publish_measurement(
        &self,
        entity: &EntityTopicId,
        r#type: &str,
        payload: &Value,
    ) -> Result<(), MqttError> {
        self.publish(&entity.channel_topic(&format!("m/{type}")), payload, false)
            .await
    }

    /// Retained `m/<type>/meta`, declaring the unit of each series.
    pub async fn publish_measurement_meta(
        &self,
        entity: &EntityTopicId,
        r#type: &str,
        payload: &Value,
    ) -> Result<(), MqttError> {
        self.publish_retained(&entity.channel_topic(&format!("m/{type}/meta")), payload)
            .await
    }

    pub async fn publish_event(
        &self,
        entity: &EntityTopicId,
        r#type: &str,
        payload: &Value,
    ) -> Result<(), MqttError> {
        self.publish(&entity.channel_topic(&format!("e/{type}")), payload, false)
            .await
    }

    pub async fn raise_alarm(
        &self,
        entity: &EntityTopicId,
        r#type: &str,
        payload: &Value,
    ) -> Result<(), MqttError> {
        self.publish(&entity.channel_topic(&format!("a/{type}")), payload, false)
            .await
    }

    /// Clear an alarm: an empty payload, retained, exactly as the Java gateway does.
    pub async fn clear_alarm(&self, entity: &EntityTopicId, r#type: &str) -> Result<(), MqttError> {
        let topic = entity.channel_topic(&format!("a/{type}"));
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, Vec::new())
            .await
            .map_err(|source| MqttError::Publish { topic, source })
    }

    async fn subscribe(&self, topic: &str) -> Result<(), MqttError> {
        self.client
            .subscribe(topic, QoS::AtLeastOnce)
            .await
            .map_err(|source| MqttError::Publish {
                topic: topic.to_owned(),
                source,
            })
    }

    async fn publish_retained(&self, topic: &str, payload: &Value) -> Result<(), MqttError> {
        self.publish(topic, payload, true).await
    }

    async fn publish(&self, topic: &str, payload: &Value, retain: bool) -> Result<(), MqttError> {
        let body = serde_json::to_vec(payload)?;
        debug!(topic, bytes = body.len(), retain, "publishing to thin-edge");
        self.client
            .publish(topic, QoS::AtLeastOnce, retain, body)
            .await
            .map_err(|source| MqttError::Publish {
                topic: topic.to_owned(),
                source,
            })
    }
}

/// Drive the MQTT connection until cancelled.
///
/// rumqttc reconnects on its own; this only logs the transitions so a broker outage is visible
/// without being fatal. `subscriptions` are re-issued on every successful connect, because
/// rumqttc does not replay them, and matching messages are forwarded to `incoming` — dropped with
/// a warning if that bounded channel is full, never queued without limit.
pub async fn run_event_loop(
    mut event_loop: EventLoop,
    mqtt: TedgeMqtt,
    subscriptions: Vec<String>,
    incoming: Option<mpsc::Sender<IncomingMessage>>,
    reconnect_delay: Duration,
    cancel: CancellationToken,
) {
    let mut connected = false;
    loop {
        let event = tokio::select! {
            () = cancel.cancelled() => return,
            event = event_loop.poll() => event,
        };
        match event {
            Ok(Event::Incoming(rumqttc::Incoming::ConnAck(_))) => {
                connected = true;
                info!("connected to the thin-edge MQTT broker");
                for topic in &subscriptions {
                    match mqtt.subscribe(topic).await {
                        Ok(()) => info!(topic, "subscribed"),
                        Err(error) => warn!(topic, %error, "cannot subscribe"),
                    }
                }
            }
            Ok(Event::Incoming(rumqttc::Incoming::Publish(publish))) => {
                if let Some(incoming) = incoming.as_ref() {
                    let message = IncomingMessage {
                        topic: publish.topic.clone(),
                        payload: publish.payload.to_vec(),
                    };
                    if incoming.try_send(message).is_err() {
                        warn!(
                            topic = publish.topic,
                            "dropping an incoming message: the handler is not keeping up"
                        );
                    }
                }
            }
            Ok(_) => {}
            Err(error) => {
                if connected || !matches!(error, ConnectionError::MqttState(_)) {
                    warn!(%error, "thin-edge MQTT connection lost; retrying");
                }
                connected = false;
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(reconnect_delay) => {}
                }
            }
        }
    }
}

fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}
