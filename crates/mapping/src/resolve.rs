//! Node resolution: device type mapping entries to concrete NodeIds and a delivery mode.
//!
//! This replaces the Java gateway's address space scanning and device type *matching*. A device
//! type already records the root it was authored against (`referencedRootNodeId`), so every
//! mapping entry is either an absolute `referencedNodeId` or one `TranslateBrowsePathsToNodeIds`
//! lookup away — no tree walk, nothing cached.

use std::collections::BTreeMap;

use opcua_types::{NodeId, QualifiedName};
use tracing::warn;

use crate::MappingError;
use crate::model::{
    AlarmCreation, DeviceType, EventCreation, MappingEntry, MeasurementCreation, SubscriptionKind,
    SubscriptionParameters,
};
use crate::namespace::{self, NamespaceTable};

/// Default cyclic read rate when a cyclic-read device type does not specify one.
pub const DEFAULT_READ_RATE_MS: u64 = 5_000;

/// What to publish when a node's value arrives.
#[derive(Debug, Clone)]
pub enum Action {
    Measurement(MeasurementCreation),
    Event(EventCreation),
    Alarm(AlarmCreation),
}

/// How a node's values reach the gateway.
#[derive(Debug, Clone)]
pub enum Delivery {
    /// A monitored item on the server's subscription.
    Subscription(SubscriptionParameters),
    /// A periodic Read, grouped with every other node on the same schedule.
    CyclicRead { rate_ms: u64, max_age: f64 },
}

/// Cyclic read groups are keyed by their schedule, so nodes read together share one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReadSchedule {
    pub rate_ms: u64,
    /// `maxAge` in milliseconds, kept as an integer so the schedule can be a map key.
    pub max_age_ms: u64,
}

impl ReadSchedule {
    pub fn max_age(&self) -> f64 {
        self.max_age_ms as f64
    }
}

/// A node whose NodeId is known, with everything to publish for it.
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    pub node_id: NodeId,
    /// String form used in payload fragments and log fields.
    pub node_id_str: String,
    pub actions: Vec<Action>,
    pub delivery: Delivery,
}

impl ResolvedNode {
    /// The read schedule of a cyclic-read node.
    pub fn read_schedule(&self) -> Option<ReadSchedule> {
        match &self.delivery {
            Delivery::CyclicRead { rate_ms, max_age } => Some(ReadSchedule {
                rate_ms: *rate_ms,
                max_age_ms: max_age.max(0.0) as u64,
            }),
            Delivery::Subscription(_) => None,
        }
    }

    pub fn subscription_parameters(&self) -> Option<&SubscriptionParameters> {
        match &self.delivery {
            Delivery::Subscription(params) => Some(params),
            Delivery::CyclicRead { .. } => None,
        }
    }
}

/// A device type resolved against one server.
#[derive(Debug, Clone)]
pub struct ResolvedMapping {
    pub device_type_id: String,
    pub device_type_name: String,
    /// The root this device type was authored against, part of the device's display name.
    pub root_node_id: String,
    pub nodes: Vec<ResolvedNode>,
}

impl ResolvedMapping {
    /// Every `(series, unit)` pair per measurement type, for the retained `m/<type>/meta` topics.
    pub fn measurement_units(&self) -> BTreeMap<String, Vec<(String, String)>> {
        let mut out: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for node in &self.nodes {
            for action in &node.actions {
                if let Action::Measurement(m) = action {
                    let Some(unit) = m.unit.as_deref().map(str::trim).filter(|u| !u.is_empty())
                    else {
                        continue;
                    };
                    let Some(r#type) = measurement_type(m) else {
                        continue;
                    };
                    out.entry(r#type.to_owned())
                        .or_default()
                        .push((m.series_name(&node.node_id_str), unit.to_owned()));
                }
            }
        }
        out
    }

    /// Cyclic-read nodes grouped by schedule.
    pub fn read_groups(&self) -> BTreeMap<ReadSchedule, Vec<&ResolvedNode>> {
        let mut out: BTreeMap<ReadSchedule, Vec<&ResolvedNode>> = BTreeMap::new();
        for node in &self.nodes {
            if let Some(schedule) = node.read_schedule() {
                out.entry(schedule).or_default().push(node);
            }
        }
        out
    }

    /// Nodes delivered by subscription.
    pub fn subscribed_nodes(&self) -> impl Iterator<Item = &ResolvedNode> {
        self.nodes
            .iter()
            .filter(|n| matches!(n.delivery, Delivery::Subscription(_)))
    }
}

/// Measurement type, which becomes the `m/<type>` topic segment.
pub fn measurement_type(m: &MeasurementCreation) -> Option<&str> {
    m.r#type.as_deref().map(str::trim).filter(|t| !t.is_empty())
}

/// How an entry's NodeId is obtained.
#[derive(Debug, Clone)]
enum NodeRef {
    Known(NodeId),
    Path(Vec<QualifiedName>),
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    actions: Vec<Action>,
    node_ref: NodeRef,
    delivery: Delivery,
}

/// A device type prepared for resolution against a live session.
#[derive(Debug, Clone)]
pub struct Plan {
    pub device_type_id: String,
    pub device_type_name: String,
    pub root_node_id: NodeId,
    entries: Vec<Entry>,
}

/// Prepare a device type for resolution against a server's namespace table.
///
/// Entries this gateway cannot support are dropped with one warning each; the rest of the device
/// type still applies. Returns `Err` only when the device type itself is unusable.
pub fn plan(
    device_type_id: &str,
    device_type: &DeviceType,
    table: &NamespaceTable,
) -> Result<Plan, MappingError> {
    let root = device_type
        .referenced_root_node_id
        .as_deref()
        .map(|s| namespace::parse_node_id(table, s))
        .transpose()?
        .unwrap_or_else(|| NodeId::new(0, 84u32));

    let default_delivery = delivery_of(
        device_type.subscription_type.as_ref(),
        device_type_id,
        "<device type default>",
    );
    let overrides = overrides_of(device_type, device_type_id, table);

    let mut entries = Vec::with_capacity(device_type.mappings.len());
    for entry in &device_type.mappings {
        let name = entry.name.clone().unwrap_or_else(|| "<unnamed>".to_owned());
        let actions = actions_of(entry);
        if actions.is_empty() {
            continue;
        }

        if entry.browse_path.iter().any(|s| namespace::is_regex(s)) {
            warn!(
                device_type_id,
                mapping_entry = name,
                "browse path uses regex(...), which needs an address space scan; entry ignored"
            );
            continue;
        }

        let path = namespace::to_qualified_names(table, &entry.browse_path);
        let node_ref = match entry
            .referenced_node_id
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            Some(node_id) => match namespace::parse_node_id(table, node_id) {
                Ok(id) => NodeRef::Known(id),
                Err(error) => {
                    warn!(device_type_id, mapping_entry = name, %error, "unusable referencedNodeId; entry ignored");
                    continue;
                }
            },
            None if entry.browse_path.is_empty() => {
                warn!(
                    device_type_id,
                    mapping_entry = name,
                    "entry has neither referencedNodeId nor browsePath; ignored"
                );
                continue;
            }
            None => NodeRef::Path(path.clone()),
        };

        // An overriddenSubscriptions entry whose browse path matches wins over the device type's
        // own subscriptionType, which is exactly what the field is for.
        let delivery = overrides
            .iter()
            .find(|(override_path, _)| *override_path == path)
            .map(|(_, delivery)| delivery.clone())
            .unwrap_or_else(|| default_delivery.clone());

        let Some(delivery) = delivery else {
            warn!(
                device_type_id,
                mapping_entry = name,
                "no usable subscriptionType for this entry; ignored"
            );
            continue;
        };

        entries.push(Entry {
            name,
            actions,
            node_ref,
            delivery,
        });
    }

    Ok(Plan {
        device_type_id: device_type_id.to_owned(),
        device_type_name: device_type.name.clone(),
        root_node_id: root,
        entries,
    })
}

/// Read `overriddenSubscriptions` into browse-path-keyed delivery modes.
fn overrides_of(
    device_type: &DeviceType,
    device_type_id: &str,
    table: &NamespaceTable,
) -> Vec<(Vec<QualifiedName>, Option<Delivery>)> {
    device_type
        .overridden_subscriptions
        .iter()
        .filter(|o| !o.browse_path.is_empty())
        .map(|o| {
            let path = namespace::to_qualified_names(table, &o.browse_path);
            let label = o.browse_path.join("/");
            (
                path,
                delivery_of(o.subscription_type.as_ref(), device_type_id, &label),
            )
        })
        .collect()
}

/// Turn a `subscriptionType` into a delivery mode, reporting anything unusable.
fn delivery_of(
    subscription_type: Option<&crate::model::SubscriptionType>,
    device_type_id: &str,
    label: &str,
) -> Option<Delivery> {
    let subscription_type = subscription_type?;
    match subscription_type.kind() {
        SubscriptionKind::Subscription => {
            let params = subscription_type
                .subscription_parameters
                .clone()
                .unwrap_or_default();
            if !params.is_valid() {
                warn!(
                    device_type_id,
                    mapping_entry = label,
                    ?params,
                    "subscriptionParameters are not valid OPC UA parameters; ignored"
                );
                return None;
            }
            Some(Delivery::Subscription(params))
        }
        SubscriptionKind::CyclicRead => {
            let params = subscription_type.cyclic_read_parameters.as_ref();
            Some(Delivery::CyclicRead {
                rate_ms: params
                    .and_then(|p| p.rate_ms())
                    .unwrap_or(DEFAULT_READ_RATE_MS),
                max_age: params.map_or(0.0, |p| p.max_age()),
            })
        }
        SubscriptionKind::None => None,
    }
}

fn actions_of(entry: &MappingEntry) -> Vec<Action> {
    let mut actions = Vec::new();
    if let Some(m) = &entry.measurement_creation
        && measurement_type(m).is_some()
    {
        actions.push(Action::Measurement(m.clone()));
    }
    if let Some(e) = &entry.event_creation
        && e.r#type.as_deref().is_some_and(|t| !t.trim().is_empty())
    {
        actions.push(Action::Event(e.clone()));
    }
    if let Some(a) = &entry.alarm_creation
        && a.r#type.as_deref().is_some_and(|t| !t.trim().is_empty())
    {
        actions.push(Action::Alarm(a.clone()));
    }
    actions
}

impl Plan {
    /// Entries still needing a `TranslateBrowsePathsToNodeIds` round trip, by entry index.
    pub fn pending_paths(&self) -> Vec<(usize, Vec<QualifiedName>)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match &e.node_ref {
                NodeRef::Path(path) => Some((i, path.clone())),
                NodeRef::Known(_) => None,
            })
            .collect()
    }

    /// Record the NodeId a translation produced for one entry.
    pub fn set_resolved(&mut self, index: usize, node_id: NodeId) {
        if let Some(entry) = self.entries.get_mut(index) {
            entry.node_ref = NodeRef::Known(node_id);
        }
    }

    /// Name of the entry at `index`, for log fields.
    pub fn entry_name(&self, index: usize) -> &str {
        self.entries.get(index).map_or("<unknown>", |e| &e.name)
    }

    /// Drop still-unresolved entries and group the rest by NodeId and delivery mode.
    pub fn finish(self) -> ResolvedMapping {
        let mut nodes: Vec<ResolvedNode> = Vec::new();
        for entry in self.entries {
            let NodeRef::Known(node_id) = entry.node_ref else {
                warn!(
                    device_type_id = self.device_type_id,
                    mapping_entry = entry.name,
                    "browse path did not resolve on this server; entry ignored"
                );
                continue;
            };

            // Two entries share a node only when they also share a delivery mode: the same node
            // read cyclically and monitored would otherwise collapse into one arbitrary mode.
            let existing = nodes.iter_mut().find(|n| {
                n.node_id == node_id
                    && std::mem::discriminant(&n.delivery)
                        == std::mem::discriminant(&entry.delivery)
                    && n.read_schedule() == read_schedule_of(&entry.delivery)
            });
            match existing {
                Some(node) => node.actions.extend(entry.actions),
                None => nodes.push(ResolvedNode {
                    node_id_str: node_id.to_string(),
                    node_id,
                    actions: entry.actions,
                    delivery: entry.delivery,
                }),
            }
        }

        ResolvedMapping {
            device_type_id: self.device_type_id,
            device_type_name: self.device_type_name,
            root_node_id: self.root_node_id.to_string(),
            nodes,
        }
    }
}

fn read_schedule_of(delivery: &Delivery) -> Option<ReadSchedule> {
    match delivery {
        Delivery::CyclicRead { rate_ms, max_age } => Some(ReadSchedule {
            rate_ms: *rate_ms,
            max_age_ms: max_age.max(0.0) as u64,
        }),
        Delivery::Subscription(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> NamespaceTable {
        NamespaceTable::new(vec![
            "http://opcfoundation.org/UA/".into(),
            "urn:freeopcua:python:server".into(),
            "http://www.cumulocity.com".into(),
        ])
    }

    const FIXTURE: &str = include_str!("../tests/fixtures/pump01-device-type.json");

    fn fixture() -> (String, DeviceType) {
        let mo: crate::model::ManagedObject =
            serde_json::from_str(FIXTURE).expect("fixture parses");
        (mo.id.clone(), mo.device_type.expect("has device type"))
    }

    fn resolve_all(plan: &mut Plan) {
        for (index, _) in plan.pending_paths() {
            plan.set_resolved(index, NodeId::new(2, format!("n{index}")));
        }
    }

    #[test]
    fn plans_every_mapped_entry_from_the_real_device_type() {
        let (id, dt) = fixture();
        let plan = plan(&id, &dt, &table()).expect("plans");
        assert_eq!(plan.root_node_id, NodeId::new(0, 84u32));
        // 10 mapping entries, one of which (filterDegradationRate) has no action at all.
        assert_eq!(plan.pending_paths().len(), 9);
    }

    #[test]
    fn overridden_subscriptions_win_over_the_device_type_default() {
        let (id, dt) = fixture();
        let mut plan = plan(&id, &dt, &table()).expect("plans");
        resolve_all(&mut plan);
        let resolved = plan.finish();

        // The fixture defaults to CyclicRead at 3000 ms and overrides three browse paths —
        // operatingLevel, activeAlarm and status — to Subscription.
        let subscribed = resolved.subscribed_nodes().count();
        assert_eq!(subscribed, 3);
        assert_eq!(resolved.nodes.len() - subscribed, 6);

        let groups = resolved.read_groups();
        assert_eq!(groups.len(), 1);
        let schedule = *groups.keys().next().expect("one group");
        assert_eq!(schedule.rate_ms, 3_000);
        assert_eq!(schedule.max_age_ms, 0);
    }

    #[test]
    fn subscribed_nodes_carry_the_overridden_parameters() {
        let (id, dt) = fixture();
        let mut plan = plan(&id, &dt, &table()).expect("plans");
        resolve_all(&mut plan);
        let resolved = plan.finish();

        let params = resolved
            .subscribed_nodes()
            .next()
            .and_then(ResolvedNode::subscription_parameters)
            .expect("a subscribed node");
        assert_eq!(params.sampling_rate, Some(1_000.0));
        assert_eq!(params.queue_size, Some(10));
        assert_eq!(
            params.data_change_trigger(),
            crate::model::DataChangeTrigger::StatusValue
        );
        assert!(params.discard_oldest());
        assert_eq!(params.deadband(), None);
    }

    #[test]
    fn unresolved_entries_are_dropped_not_fatal() {
        let (id, dt) = fixture();
        let mut plan = plan(&id, &dt, &table()).expect("plans");
        plan.set_resolved(0, NodeId::new(2, "flow"));
        let resolved = plan.finish();
        assert_eq!(resolved.nodes.len(), 1);
        assert_eq!(resolved.nodes[0].node_id, NodeId::new(2, "flow"));
    }

    #[test]
    fn measurement_units_are_grouped_by_type() {
        let (id, dt) = fixture();
        let mut plan = plan(&id, &dt, &table()).expect("plans");
        resolve_all(&mut plan);
        let units = plan.finish().measurement_units();
        assert_eq!(units["flow"], vec![("flow".to_owned(), "l/m".to_owned())]);
        assert_eq!(units["power"], vec![("power".to_owned(), "W".to_owned())]);
    }
}
