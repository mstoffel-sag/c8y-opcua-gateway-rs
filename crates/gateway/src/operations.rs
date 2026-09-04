//! Cumulocity operations addressed at the gateway or at one of its servers.
//!
//! `c8y_OpcuaConfiguration` is a Cumulocity operation name with no thin-edge command equivalent,
//! and the user interface enables the OPC UA controls from its presence in
//! `c8y_SupportedOperations`. Declaring it as a thin-edge command would mean writing a file under
//! `/etc/tedge/operations/c8y/<external-id>/`, a runtime-derived path this gateway will not create.
//! Reading the bridged `c8y/devicecontrol/notifications` topic needs no such file, and the status
//! goes back as one SmartREST record on `c8y/s/us/<external-id>`.
//!
//! This works only because the gateway object is deliberately not marked as a Cumulocity agent:
//! see [`crate::gateway_device`]. Otherwise Cumulocity routes its operations to that object rather
//! than to the thin-edge agent, and nothing arrives on this topic at all.
//!
//! No operation is implemented yet. Advertising an operation and then leaving it pending forever is
//! worse than failing it, so every operation that arrives is failed with a reason naming it.

use std::sync::Arc;

use serde_json::Value;
use tedge::mqtt::C8Y_OPERATION_TOPIC;
use tedge::{IncomingMessage, TedgeMqtt};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::cloud_servers::CloudServers;
use crate::config::Config;

/// Envelope fields of an operation, none of which name the operation itself.
const ENVELOPE_FIELDS: &[&str] = &[
    "id",
    "status",
    "deviceId",
    "agentId",
    "creationTime",
    "description",
    "delivery",
    "externalSource",
    "self",
    "failureReason",
];

/// Answer operations until cancelled.
pub async fn run(
    config: Arc<Config>,
    mqtt: TedgeMqtt,
    mut incoming: mpsc::Receiver<IncomingMessage>,
    cloud: watch::Receiver<CloudServers>,
    cancel: CancellationToken,
) {
    let gateway_external_id = config.gateway.external_id();

    loop {
        let message = tokio::select! {
            () = cancel.cancelled() => return,
            message = incoming.recv() => match message {
                Some(message) => message,
                None => return,
            },
        };
        if message.topic != C8Y_OPERATION_TOPIC {
            continue;
        }

        let Ok(operation) = serde_json::from_slice::<Value>(&message.payload) else {
            warn!(topic = message.topic, "ignoring an unparseable operation");
            continue;
        };
        if operation.get("status").and_then(Value::as_str) != Some("PENDING") {
            continue;
        }

        let Some(external_id) = operation
            .get("externalSource")
            .and_then(|source| source.get("externalId"))
            .and_then(Value::as_str)
        else {
            continue;
        };

        let ours = external_id == gateway_external_id
            || cloud
                .borrow()
                .iter()
                .any(|server| server.external_id.as_deref() == Some(external_id));
        if !ours {
            continue;
        }

        let Some(fragment) = operation_fragment(&operation) else {
            debug!(
                external_id,
                "an operation for us names no operation fragment"
            );
            continue;
        };

        let reason = format!("{fragment} is not implemented by c8y-opcua-gateway-rs");
        info!(
            external_id,
            fragment,
            operation_id = operation
                .get("id")
                .and_then(|id| id.as_str())
                .unwrap_or("?"),
            "failing an unsupported operation"
        );
        if let Err(error) = mqtt
            .publish_smartrest(external_id, &failure_record(fragment, &reason))
            .await
        {
            warn!(external_id, fragment, %error, "cannot report the operation as failed");
        }
    }
}

/// The one field naming what the operation asks for.
fn operation_fragment(operation: &Value) -> Option<&str> {
    operation
        .as_object()?
        .keys()
        .map(String::as_str)
        .find(|key| !ENVELOPE_FIELDS.contains(key))
}

/// SmartREST 502: set the oldest pending operation of this type to failed, with a reason.
fn failure_record(fragment: &str, reason: &str) -> String {
    format!("502,{fragment},{}", smartrest_field(reason))
}

/// Quote a SmartREST field. Commas and quotes would otherwise shift or truncate the record.
fn smartrest_field(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_operation_fragment_is_the_field_that_is_not_envelope() {
        let operation = json!({
            "delivery": { "status": "PENDING" },
            "agentId": "9569051262",
            "creationTime": "2026-09-04T11:24:54.849Z",
            "deviceId": "1069052109",
            "id": "69050137",
            "status": "PENDING",
            "c8y_OpcuaConfiguration": { "spike": 3 },
            "description": "spike op 3",
            "externalSource": { "externalId": "tedge001:device:opcua-gateway", "type": "c8y_Serial" },
        });
        assert_eq!(
            operation_fragment(&operation),
            Some("c8y_OpcuaConfiguration")
        );
    }

    #[test]
    fn an_envelope_only_operation_names_nothing() {
        let operation = json!({ "id": "1", "status": "PENDING", "deviceId": "2" });
        assert!(operation_fragment(&operation).is_none());
    }

    #[test]
    fn failure_records_are_quoted() {
        assert_eq!(
            failure_record("c8y_OpcuaConfiguration", "not implemented"),
            "502,c8y_OpcuaConfiguration,\"not implemented\""
        );
        assert_eq!(
            smartrest_field("says \"no\", loudly"),
            "\"says \"\"no\"\", loudly\""
        );
    }
}
