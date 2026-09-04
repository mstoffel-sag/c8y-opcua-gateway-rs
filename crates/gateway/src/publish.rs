//! Turning OPC UA values into thin-edge messages.
//!
//! Shared by both delivery paths so a value produces the same message whether it arrived from a
//! cyclic read or from a monitored item. Measurements are batched — one message per measurement
//! type, holding every series accumulated since the last flush — because publishing each data
//! change on its own would drown the local broker and the mapper on a fast server.

use std::collections::BTreeMap;

use mapping::payload::{self, MeasurementBuilder};
use mapping::resolve::{self, Action, ResolvedMapping, ResolvedNode};
use mapping::value;
use opcua::types::DataValue;
use tedge::{EntityTopicId, TedgeMqtt};
use tracing::{debug, warn};

/// Accumulates measurements and publishes events and alarms as they happen.
pub struct Publisher {
    mqtt: TedgeMqtt,
    entity: EntityTopicId,
    server_id: String,
    /// Whether repeated identical event values should be suppressed. A cyclic read re-delivers
    /// the same value every tick, so publishing an event each time would flood the mapper; a
    /// subscription notification, by contrast, already *is* a change.
    dedup_events: bool,
    last_event: BTreeMap<(String, String), String>,
    last_alarm: BTreeMap<(String, String), bool>,
    batch: BTreeMap<String, MeasurementBuilder>,
    pending_series: usize,
}

impl Publisher {
    pub fn new(
        mqtt: TedgeMqtt,
        entity: EntityTopicId,
        server_id: String,
        dedup_events: bool,
    ) -> Self {
        Self {
            mqtt,
            entity,
            server_id,
            dedup_events,
            last_event: BTreeMap::new(),
            last_alarm: BTreeMap::new(),
            batch: BTreeMap::new(),
            pending_series: 0,
        }
    }

    /// Number of measurement series waiting to be flushed.
    pub fn pending_series(&self) -> usize {
        self.pending_series
    }

    /// Handle one value: batch its measurements, publish its events and alarms.
    pub async fn accept(&mut self, node: &ResolvedNode, data_value: &DataValue) {
        if value::is_bad(data_value) {
            debug!(
                server_id = self.server_id,
                node_id = node.node_id_str,
                status = ?data_value.status,
                "dropping bad-quality value"
            );
            return;
        }

        for action in &node.actions {
            match action {
                Action::Measurement(creation) => {
                    let Some(r#type) = resolve::measurement_type(creation) else {
                        continue;
                    };
                    if self.batch.entry(r#type.to_owned()).or_default().add(
                        creation,
                        &node.node_id_str,
                        data_value,
                    ) {
                        self.pending_series += 1;
                    }
                }
                Action::Event(creation) => {
                    let Some(r#type) = creation.r#type.as_deref() else {
                        continue;
                    };
                    let body = payload::event(creation, &node.node_id_str, data_value);
                    if self.dedup_events {
                        let text = body["text"].as_str().unwrap_or_default().to_owned();
                        let key = (node.node_id_str.clone(), r#type.to_owned());
                        if self.last_event.get(&key) == Some(&text) {
                            continue;
                        }
                        self.last_event.insert(key, text);
                    }
                    if let Err(error) = self.mqtt.publish_event(&self.entity, r#type, &body).await {
                        warn!(%error, event_type = r#type, "failed to publish event");
                    }
                }
                Action::Alarm(creation) => {
                    let Some(r#type) = creation.r#type.as_deref() else {
                        continue;
                    };
                    let Some(active) = data_value.value.as_ref().and_then(value::as_bool) else {
                        warn!(
                            server_id = self.server_id,
                            node_id = node.node_id_str,
                            alarm_type = r#type,
                            "alarm node value cannot be read as a boolean; ignoring"
                        );
                        continue;
                    };
                    // Only transitions are published: re-raising an already active alarm on every
                    // notification would churn the broker for no new information.
                    let key = (node.node_id_str.clone(), r#type.to_owned());
                    if self.last_alarm.get(&key) == Some(&active) {
                        continue;
                    }
                    self.last_alarm.insert(key, active);

                    let result = if active {
                        let body = payload::alarm(creation, &node.node_id_str, data_value);
                        self.mqtt.raise_alarm(&self.entity, r#type, &body).await
                    } else {
                        self.mqtt.clear_alarm(&self.entity, r#type).await
                    };
                    if let Err(error) = result {
                        warn!(%error, alarm_type = r#type, active, "failed to publish alarm");
                    }
                }
            }
        }
    }

    /// Publish every batched measurement, one message per measurement type.
    pub async fn flush(&mut self) {
        self.pending_series = 0;
        for (r#type, builder) in std::mem::take(&mut self.batch) {
            let Some(body) = builder.build() else {
                continue;
            };
            if let Err(error) = self
                .mqtt
                .publish_measurement(&self.entity, &r#type, &body)
                .await
            {
                warn!(%error, measurement_type = r#type, "failed to publish measurement");
            }
        }
    }
}

/// Publish the retained `m/<type>/meta` topics declaring each series' unit, for one device.
pub async fn publish_units(mqtt: &TedgeMqtt, entity: &EntityTopicId, mapping: &ResolvedMapping) {
    for (r#type, series) in mapping.measurement_units() {
        if let Some(meta) = payload::measurement_meta(&series)
            && let Err(error) = mqtt.publish_measurement_meta(entity, &r#type, &meta).await
        {
            warn!(%error, measurement_type = r#type, "failed to publish measurement units");
        }
    }
}
