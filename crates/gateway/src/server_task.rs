//! One supervised task per OPC UA server: connect, resolve, subscribe and read, publish.
//!
//! A server gets exactly one OPC UA subscription, shared by every device type that asks for
//! monitored items — the same shape as the Java gateway. Cyclic-read nodes are grouped by
//! schedule, so nodes read on the same interval travel in one Read request.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use mapping::constraints;
use mapping::model::{DataChangeTrigger as MappingTrigger, Deadband};
use mapping::namespace::NamespaceTable;
use mapping::resolve::{self, ResolvedMapping, ResolvedNode};
use opcua::types::{DataChangeTrigger, DeadbandType, NodeId};
use opcua_conn::{Connection, DataChange, MonitoredItemSpec};
use tedge::mqtt::{
    OPCUA_DEVICE_ENTITY_TYPE, OPCUA_DEVICE_MO_TYPE, OPCUA_SERVER_ENTITY_TYPE, OPCUA_SERVER_MO_TYPE,
};
use tedge::topic::sanitize_id;
use tedge::{EntityTopicId, TedgeMqtt};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::{Config, ServerConfig, SubscriptionConfig};
use crate::device_types::DeviceTypes;
use crate::publish::{self, Publisher};

/// Consecutive read failures tolerated before the session is treated as lost.
const MAX_CONSECUTIVE_READ_FAILURES: u32 = 3;

/// A device type applied to a server, and the thin-edge device it publishes as.
///
/// Each applied device type is its own device below the server. Without that, two device types on
/// one server share every telemetry topic: cyclic-read measurements arrive twice, subscribed
/// series overwrite each other, and their alarms collapse into one because Cumulocity dedupes by
/// source and type.
struct AppliedDeviceType {
    entity: EntityTopicId,
    name: String,
    mapping: ResolvedMapping,
}

impl AppliedDeviceType {
    /// Build the device for one resolved device type below `server`.
    ///
    /// The topic id is derived from the server and device type ids, and so is stable across
    /// restarts — it decides the device's identity in the cloud. The display name follows the Java
    /// gateway's `"<deviceTypeName> (<rootNodeId>)"`.
    fn new(server: &ServerConfig, mapping: ResolvedMapping) -> Self {
        let entity = EntityTopicId::child_device(&format!(
            "{}-{}",
            server.topic_id(),
            sanitize_id(&mapping.device_type_id)
        ));
        let name = if mapping.device_type_name.trim().is_empty() {
            format!("{} ({})", mapping.device_type_id, mapping.root_node_id)
        } else {
            format!("{} ({})", mapping.device_type_name, mapping.root_node_id)
        };
        Self {
            entity,
            name,
            mapping,
        }
    }
}

/// Supervise one server until it is stopped or removed.
///
/// `known_server_ids` is the server set as it stood when this task started, used only to report a
/// `matchesServerIds` that names no configured server.
///
/// Two tokens, because stopping and removing are different things. `stop` is a child of the
/// process-wide token and also fires when the server is merely being restarted with new settings:
/// the task ends and leaves its retained registrations alone. `remove` means the server is gone,
/// and the registrations have to go with it — a registration that outlives its managed object makes
/// the mapper recreate the object on the next restart.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: Arc<Config>,
    server: ServerConfig,
    known_server_ids: Vec<String>,
    mqtt: TedgeMqtt,
    mut device_types: tokio::sync::watch::Receiver<DeviceTypes>,
    stop: CancellationToken,
    remove: CancellationToken,
) {
    let entity = EntityTopicId::child_device(&server.topic_id());
    let parent = parent_entity(&config);
    if let Err(error) = mqtt
        .register_child_device(
            &entity,
            &parent,
            &server.name,
            server_entity_type(&config),
            server.external_id.as_deref(),
        )
        .await
    {
        warn!(server_id = server.id, %error, "failed to register the server child device");
    }

    // Every device entity this task has registered, so removal can take them with it.
    let mut registered: Vec<EntityTopicId> = Vec::new();

    let mut backoff = Duration::from_secs(1);
    loop {
        if stop.is_cancelled() {
            return;
        }
        if remove.is_cancelled() {
            deregister(&mqtt, &server, &entity, &registered).await;
            return;
        }

        let endpoint = server.endpoint();
        let connect =
            Connection::connect(&endpoint, &config.mqtt.client_id, config.connect_timeout());
        let connection = match tokio::select! {
            _ = stop.cancelled() => return,
            _ = remove.cancelled() => {
                deregister(&mqtt, &server, &entity, &registered).await;
                return;
            }
            result = connect => result,
        } {
            Ok(connection) => {
                backoff = Duration::from_secs(1);
                Arc::new(connection)
            }
            Err(error) => {
                warn!(
                    server_id = server.id,
                    url = server.url,
                    %error,
                    retry_in_secs = backoff.as_secs(),
                    "cannot connect to the OPC UA server"
                );
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = remove.cancelled() => {
                        deregister(&mqtt, &server, &entity, &registered).await;
                        return;
                    }
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(config.max_backoff());
                continue;
            }
        };

        // A session generation: everything resolved against this session and this device type set.
        let generation = stop.child_token();
        let mut readers = JoinSet::new();

        let current = Arc::clone(&device_types.borrow_and_update());
        match resolve_all(&connection, &server, &known_server_ids, current).await {
            Ok(mappings) => {
                let applied: Vec<AppliedDeviceType> = mappings
                    .into_iter()
                    .map(|mapping| AppliedDeviceType::new(&server, mapping))
                    .collect();

                for device in &applied {
                    if !registered.contains(&device.entity) {
                        registered.push(device.entity.clone());
                    }
                    if let Err(error) = mqtt
                        .register_child_device(
                            &device.entity,
                            &entity,
                            &device.name,
                            device_entity_type(&config),
                            None,
                        )
                        .await
                    {
                        warn!(
                            server_id = server.id,
                            device_type_id = device.mapping.device_type_id,
                            %error,
                            "failed to register the device for this device type"
                        );
                    }
                    publish::publish_units(&mqtt, &device.entity, &device.mapping).await;
                }

                start_subscription(
                    &config,
                    &connection,
                    &server,
                    &applied,
                    &mqtt,
                    &mut readers,
                    &generation,
                )
                .await;
                start_read_loops(
                    &connection,
                    &server,
                    &applied,
                    &mqtt,
                    &mut readers,
                    &generation,
                );
            }
            Err(error) => warn!(
                server_id = server.id,
                %error,
                "cannot resolve mappings against this session; waiting for the next reload"
            ),
        }

        if readers.is_empty() {
            info!(
                server_id = server.id,
                "connected but nothing to read yet: no device type applies to this server"
            );
        }

        let reconnect = tokio::select! {
            _ = stop.cancelled() => {
                generation.cancel();
                readers.shutdown().await;
                connection.disconnect().await;
                return;
            }
            _ = remove.cancelled() => {
                generation.cancel();
                readers.shutdown().await;
                connection.disconnect().await;
                deregister(&mqtt, &server, &entity, &registered).await;
                return;
            }
            changed = device_types.changed() => {
                if changed.is_err() {
                    generation.cancel();
                    readers.shutdown().await;
                    connection.disconnect().await;
                    return;
                }
                info!(server_id = server.id, "device types changed; re-resolving mappings");
                false
            }
            _ = wait_for_reader_exit(&mut readers) => {
                // A reader also exits when this generation is cancelled, which is not a fault.
                let faulted = !stop.is_cancelled();
                if faulted {
                    warn!(server_id = server.id, "a read loop gave up; reconnecting");
                }
                faulted
            }
        };

        generation.cancel();
        readers.shutdown().await;
        connection.disconnect().await;

        if reconnect {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = remove.cancelled() => {
                    deregister(&mqtt, &server, &entity, &registered).await;
                    return;
                }
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(config.max_backoff());
        }
    }
}

/// Where servers hang: below the gateway device when there is one, below the main device otherwise.
fn parent_entity(config: &Config) -> EntityTopicId {
    if config.gateway.enabled {
        crate::gateway_device::entity(&config.gateway)
    } else {
        EntityTopicId::main_device()
    }
}

/// Entity types follow the same switch: Cumulocity managed object types when the gateway device is
/// on, so the OPC UA user interface recognises the objects, and plain thin-edge types otherwise.
fn server_entity_type(config: &Config) -> &'static str {
    if config.gateway.enabled {
        OPCUA_SERVER_MO_TYPE
    } else {
        OPCUA_SERVER_ENTITY_TYPE
    }
}

fn device_entity_type(config: &Config) -> &'static str {
    if config.gateway.enabled {
        OPCUA_DEVICE_MO_TYPE
    } else {
        OPCUA_DEVICE_ENTITY_TYPE
    }
}

/// Report a server that is gone.
///
/// It would be tempting to clear the retained registrations here so the entity goes with the
/// managed object. Do not: thin-edge's entity store is persistent and append-only, and clearing a
/// registration makes the agent auto-register the entity again from a default payload — without
/// our `type` and `@id`, which renames the managed object and strips what the OPC UA user
/// interface keys on. There is nothing to defend against either, because a known entity is never
/// forgotten, so no object is resurrected. Leaving the registration in place is the correct,
/// harmless thing to do.
async fn deregister(
    _mqtt: &TedgeMqtt,
    server: &ServerConfig,
    entity: &EntityTopicId,
    devices: &[EntityTopicId],
) {
    info!(
        server_id = server.id,
        entity = entity.as_str(),
        devices = devices.len(),
        "server is gone; its thin-edge entities are left registered, because thin-edge cannot \
         forget an entity and clearing the registration would corrupt the managed object"
    );
}

/// `JoinSet::join_next` returns immediately on an empty set, which would spin the select loop.
async fn wait_for_reader_exit(readers: &mut JoinSet<()>) {
    if readers.is_empty() {
        std::future::pending::<()>().await;
    }
    readers.join_next().await;
}

/// Resolve every applicable device type against a live session.
async fn resolve_all(
    connection: &Connection,
    server: &ServerConfig,
    known_server_ids: &[String],
    device_types: DeviceTypes,
) -> anyhow::Result<Vec<ResolvedMapping>> {
    let table = NamespaceTable::new(connection.namespace_array().await?);
    debug!(
        server_id = server.id,
        namespaces = table.uris().len(),
        "read namespace array"
    );

    let mut out = Vec::new();
    for loaded in device_types.iter() {
        // The local allow-list is checked first: it is the operator's own scoping decision, so it
        // should not be second-guessed by, or produce warnings from, the Cumulocity constraints.
        if !server.accepts_device_type(&loaded.id) {
            debug!(
                server_id = server.id,
                device_type_id = loaded.id,
                "device type is not in this server's device_types list"
            );
            continue;
        }
        if !constraints::applies(
            &loaded.id,
            &loaded.device_type,
            &server.id,
            known_server_ids,
            &table,
        ) {
            debug!(
                server_id = server.id,
                device_type_id = loaded.id,
                "device type does not apply to this server"
            );
            continue;
        }

        let mut plan = match resolve::plan(&loaded.id, &loaded.device_type, &table) {
            Ok(plan) => plan,
            Err(error) => {
                warn!(device_type_id = loaded.id, %error, "cannot plan this device type");
                continue;
            }
        };

        let pending = plan.pending_paths();
        if !pending.is_empty() {
            let paths: Vec<_> = pending.iter().map(|(_, path)| path.clone()).collect();
            let resolved = connection
                .translate_browse_paths(&plan.root_node_id, &paths)
                .await?;
            for ((index, _), node_id) in pending.iter().zip(resolved) {
                match node_id {
                    Some(node_id) => plan.set_resolved(*index, node_id),
                    None => warn!(
                        server_id = server.id,
                        device_type_id = loaded.id,
                        mapping_entry = plan.entry_name(*index),
                        "browse path not found on this server"
                    ),
                }
            }
        }

        let mapping = plan.finish();
        if mapping.nodes.is_empty() {
            warn!(
                server_id = server.id,
                device_type_id = loaded.id,
                "no mapping entry of this device type resolved on this server"
            );
            continue;
        }
        info!(
            server_id = server.id,
            device_type_id = mapping.device_type_id,
            device_type = mapping.device_type_name,
            subscribed = mapping.subscribed_nodes().count(),
            cyclic_read = mapping.read_groups().values().map(Vec::len).sum::<usize>(),
            origin = ?loaded.origin,
            "device type applied"
        );
        out.push(mapping);
    }
    Ok(out)
}

/// Create the server's single subscription and its monitored items.
#[allow(clippy::too_many_arguments)]
async fn start_subscription(
    config: &Config,
    connection: &Arc<Connection>,
    server: &ServerConfig,
    applied: &[AppliedDeviceType],
    mqtt: &TedgeMqtt,
    readers: &mut JoinSet<()>,
    generation: &CancellationToken,
) {
    // One monitored item per (device type, node). Two device types mapping the same node are two
    // logical devices, so they get two items rather than a merged one — and each item's own
    // parameters are honoured exactly as authored.
    let mut targets: Vec<SubscribedTarget> = Vec::new();
    let mut specs: Vec<MonitoredItemSpec> = Vec::new();
    for device in applied {
        for node in device.mapping.subscribed_nodes() {
            specs.push(item_spec(node, &config.subscription));
            targets.push(SubscribedTarget {
                entity: device.entity.clone(),
                device_type_id: device.mapping.device_type_id.clone(),
                node: node.clone(),
            });
        }
    }
    if specs.is_empty() {
        return;
    }

    let (tx, rx) = mpsc::channel(config.subscription.channel_capacity);
    let subscription_id = match connection
        .create_subscription(&(&config.subscription).into(), tx)
        .await
    {
        Ok(id) => id,
        Err(error) => {
            warn!(server_id = server.id, %error, "cannot create the OPC UA subscription");
            return;
        }
    };

    let results = match connection
        .create_monitored_items(subscription_id, &specs)
        .await
    {
        Ok(results) => results,
        Err(error) => {
            warn!(server_id = server.id, %error, "cannot create monitored items");
            return;
        }
    };

    // Notifications are routed by client handle, so an item the server refused simply has no
    // route and its never-arriving values cannot be mistaken for a mapping problem later.
    let mut routes: HashMap<u32, SubscribedTarget> = HashMap::new();
    let mut refused = 0usize;
    for (target, result) in targets.into_iter().zip(&results) {
        match result {
            Ok(handle) => {
                routes.insert(*handle, target);
            }
            Err(status) => {
                refused += 1;
                warn!(
                    server_id = server.id,
                    device_type_id = target.device_type_id,
                    node_id = target.node.node_id_str,
                    %status,
                    "server refused this monitored item; the node is not monitored"
                );
            }
        }
    }
    if routes.is_empty() {
        warn!(
            server_id = server.id,
            "the server refused every monitored item"
        );
        return;
    }

    info!(
        server_id = server.id,
        subscription_id,
        monitored_items = routes.len(),
        refused,
        "monitored items created"
    );

    readers.spawn(subscription_loop(
        rx,
        routes,
        mqtt.clone(),
        server.id.clone(),
        config.subscription.clone(),
        generation.child_token(),
    ));
}

/// Where one monitored item's notifications are published.
struct SubscribedTarget {
    entity: EntityTopicId,
    device_type_id: String,
    node: ResolvedNode,
}

/// Translate a resolved node's device type parameters into a monitored item request.
fn item_spec(node: &ResolvedNode, config: &SubscriptionConfig) -> MonitoredItemSpec {
    let params = node.subscription_parameters();
    MonitoredItemSpec {
        node_id: node.node_id.clone(),
        sampling_interval_ms: params.and_then(|p| p.sampling_rate),
        queue_size: params
            .and_then(|p| p.queue_size)
            .unwrap_or(config.default_queue_size),
        discard_oldest: params.is_none_or(|p| p.discard_oldest()),
        data_change_trigger: params.map_or(DataChangeTrigger::StatusValue, |p| {
            match p.data_change_trigger() {
                MappingTrigger::Status => DataChangeTrigger::Status,
                MappingTrigger::StatusValue => DataChangeTrigger::StatusValue,
                MappingTrigger::StatusValueTimestamp => DataChangeTrigger::StatusValueTimestamp,
            }
        }),
        deadband: params.and_then(|p| p.deadband()).map(|(kind, value)| {
            let kind = match kind {
                Deadband::Absolute => DeadbandType::Absolute,
                Deadband::Percent => DeadbandType::Percent,
            };
            (kind, value)
        }),
        index_range: params.and_then(|p| p.index_range()).map(str::to_owned),
    }
}

/// Publish subscription notifications, batching measurements on size or interval.
///
/// One publisher per device: batching groups series per measurement type, and two devices must
/// never share a batch or one would overwrite the other's series.
async fn subscription_loop(
    mut rx: mpsc::Receiver<DataChange>,
    routes: HashMap<u32, SubscribedTarget>,
    mqtt: TedgeMqtt,
    server_id: String,
    config: SubscriptionConfig,
    cancel: CancellationToken,
) {
    let mut publishers: HashMap<String, Publisher> = HashMap::new();
    for target in routes.values() {
        publishers
            .entry(target.entity.as_str().to_owned())
            .or_insert_with(|| {
                // A notification is already a change, so events are published as they arrive.
                Publisher::new(
                    mqtt.clone(),
                    target.entity.clone(),
                    server_id.clone(),
                    false,
                )
            });
    }

    let mut flush = tokio::time::interval(Duration::from_millis(config.flush_interval_ms));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                flush_all(&mut publishers).await;
                return;
            }
            _ = flush.tick() => flush_all(&mut publishers).await,
            change = rx.recv() => {
                let Some(change) = change else {
                    // The session dropped the subscription; the supervisor reconnects.
                    flush_all(&mut publishers).await;
                    return;
                };
                match routes.get(&change.client_handle) {
                    Some(target) => {
                        if let Some(publisher) = publishers.get_mut(target.entity.as_str()) {
                            publisher.accept(&target.node, &change.value).await;
                        }
                    }
                    None => debug!(
                        server_id,
                        client_handle = change.client_handle,
                        node_id = %change.node_id,
                        "notification for an item with no route; ignoring"
                    ),
                }
                let pending: usize = publishers.values().map(Publisher::pending_series).sum();
                if pending >= config.flush_max_series {
                    flush_all(&mut publishers).await;
                }
            }
        }
    }
}

async fn flush_all(publishers: &mut HashMap<String, Publisher>) {
    for publisher in publishers.values_mut() {
        publisher.flush().await;
    }
}

/// Spawn one cyclic read loop per distinct read schedule.
#[allow(clippy::too_many_arguments)]
fn start_read_loops(
    connection: &Arc<Connection>,
    server: &ServerConfig,
    applied: &[AppliedDeviceType],
    mqtt: &TedgeMqtt,
    readers: &mut JoinSet<()>,
    generation: &CancellationToken,
) {
    for device in applied {
        for (schedule, nodes) in device.mapping.read_groups() {
            readers.spawn(read_loop(
                Arc::clone(connection),
                nodes.into_iter().cloned().collect(),
                schedule,
                device.mapping.device_type_id.clone(),
                mqtt.clone(),
                device.entity.clone(),
                server.id.clone(),
                generation.child_token(),
            ));
        }
    }
}

/// Cyclically read one group of nodes and publish what they map to.
#[allow(clippy::too_many_arguments)]
async fn read_loop(
    connection: Arc<Connection>,
    nodes: Vec<ResolvedNode>,
    schedule: resolve::ReadSchedule,
    device_type_id: String,
    mqtt: TedgeMqtt,
    entity: EntityTopicId,
    server_id: String,
    cancel: CancellationToken,
) {
    let node_ids: Vec<NodeId> = nodes.iter().map(|n| n.node_id.clone()).collect();
    let mut ticker = tokio::time::interval(Duration::from_millis(schedule.rate_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // A cyclic read re-delivers unchanged values, so repeated event values are suppressed.
    let mut publisher = Publisher::new(mqtt, entity, server_id.clone(), true);
    let mut failures = 0u32;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticker.tick() => {}
        }

        let values = match connection.read_values(&node_ids, schedule.max_age()).await {
            Ok(values) => {
                failures = 0;
                values
            }
            Err(error) => {
                failures += 1;
                warn!(
                    server_id,
                    device_type_id,
                    %error,
                    failures,
                    "cyclic read failed"
                );
                if failures >= MAX_CONSECUTIVE_READ_FAILURES {
                    return;
                }
                continue;
            }
        };

        for (node, value) in nodes.iter().zip(&values) {
            publisher.accept(node, value).await;
        }
        // One flush per cycle: every series read together lands in one message per type.
        publisher.flush().await;
    }
}
