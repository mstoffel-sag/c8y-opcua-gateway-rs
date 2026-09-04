//! The Cumulocity gateway device.
//!
//! One more thin-edge child device of the main device, typed `c8y_OPCUA_Device_Agent`, with the
//! fragments the existing OPC UA user interface reads. Everything here goes out over MQTT: thin-edge
//! passes a child device's `type` straight through to the managed object, and `twin/<fragment>`
//! becomes an inventory fragment of that name, so no inventory is written over HTTP.
//!
//! One fragment is deliberately absent. The Java gateway sets `com_cumulocity_model_Agent`, which
//! makes Cumulocity treat the gateway object as its own agent and route the gateway's operations to
//! it — which is why the Java gateway has to poll `/devicecontrol/operations` over the proxy.
//! Without it the operations are routed to the thin-edge agent instead and arrive on the bridged
//! `c8y/devicecontrol/notifications` topic, so there is nothing to poll. See [`crate::operations`].

use serde_json::json;
use tedge::mqtt::GATEWAY_MO_TYPE;
use tedge::{EntityTopicId, TedgeMqtt};
use tracing::{error, info, warn};

use crate::config::GatewayConfig;

/// Topic id of the gateway device.
pub fn entity(gateway: &GatewayConfig) -> EntityTopicId {
    EntityTopicId::child_device(&tedge::topic::sanitize_id(&gateway.id))
}

/// Register the gateway device and publish its fragments. Retained, before any server appears.
///
/// The external id is set explicitly rather than left to thin-edge to derive, because the same
/// value has to be recognisable when the gateway later looks its own managed object up to read the
/// servers registered on it.
///
/// Registering an entity thin-edge already knows is a no-op, and there is no safe way to undo it
/// from here — see [`missing_object_help`].
pub async fn register(gateway: &GatewayConfig, mqtt: &TedgeMqtt) {
    let entity = entity(gateway);
    let external_id = gateway.external_id();

    if let Err(error) = mqtt
        .register_child_device(
            &entity,
            &EntityTopicId::main_device(),
            &gateway.name,
            GATEWAY_MO_TYPE,
            Some(&external_id),
        )
        .await
    {
        warn!(%error, "cannot register the gateway device");
        return;
    }

    // thin-edge child devices carry no `c8y_IsDevice`, and without it the object does not appear
    // in a device list — only in the hierarchy below the main device.
    let fragments = [
        ("c8y_IsDevice", json!({})),
        (
            "c8y_SupportedOperations",
            json!(gateway.supported_operations),
        ),
        (
            "c8y_Firmware",
            json!({
                "name": "c8y-opcua-gateway-rs",
                "version": env!("CARGO_PKG_VERSION"),
            }),
        ),
    ];
    for (fragment, payload) in fragments {
        if let Err(error) = mqtt.publish_twin(&entity, fragment, &payload).await {
            warn!(fragment, %error, "cannot publish a gateway device fragment");
        }
    }

    info!(
        entity = entity.as_str(),
        external_id,
        r#type = GATEWAY_MO_TYPE,
        "registered the gateway device"
    );
}

/// Report a gateway object that Cumulocity no longer has and thin-edge will not recreate.
///
/// Deleting the gateway device in Cumulocity leaves the entity registered on the device, and
/// thin-edge only creates a managed object when it first learns of an entity. Publishing the same
/// registration again is a no-op, so the object stays gone and everything below it is orphaned.
///
/// There is no fix the gateway can apply. Clearing the registration makes the agent auto-register
/// the entity from a default payload, which drops the `c8y_OPCUA_Device_Agent` type and the
/// external id; and thin-edge's entity store is append-only with no deletion records, so entities
/// removed through its HTTP API come back the next time `tedge-agent` restarts. What does work is
/// giving the gateway a topic id thin-edge has never seen, which is one line of configuration.
pub fn missing_object_help(gateway: &GatewayConfig) {
    error!(
        entity = entity(gateway).as_str(),
        external_id = gateway.external_id(),
        "the gateway managed object does not exist and thin-edge will not recreate it: the entity \
         is already registered, and a repeated registration is ignored. Set a new `gateway.id` in \
         the configuration and restart — a topic id thin-edge has not seen registers cleanly. \
         Servers registered on the old object have to be recreated"
    );
}
