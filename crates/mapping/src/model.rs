//! `c8y_OpcuaDeviceType` model, deserialized permissively from Cumulocity JSON.
//!
//! Unknown fields are ignored on purpose: a tenant may carry fragments this gateway does not
//! implement, and a fetch must not fail because of them.

use serde::{Deserialize, Deserializer};

/// Accept an explicit JSON `null` as the field's default.
///
/// `#[serde(default)]` only applies when a key is *absent*. Cumulocity writes `null` for unset
/// fields, so a device type with `"mappings": null` would otherwise fail to deserialize — and
/// before this was handled, one such device type discarded every device type on the same page.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// As [`null_as_default`], but for a flag whose absent value is `true`.
fn null_as_true<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<bool>::deserialize(deserializer)?.unwrap_or(true))
}

/// Managed object `type` of a device type.
pub const DEVICE_TYPE_MO_TYPE: &str = "c8y_OpcuaDeviceType";

/// Fragment inside the managed object that carries the device type itself.
pub const DEVICE_TYPE_FRAGMENT: &str = "com_cumulocity_opcua_common_model_mapping_DeviceType";

/// The Cumulocity managed-object envelope around a device type.
#[derive(Debug, Clone, Deserialize)]
pub struct ManagedObject {
    #[serde(default, deserialize_with = "null_as_default")]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "lastUpdated", default)]
    pub last_updated: Option<String>,
    #[serde(
        rename = "com_cumulocity_opcua_common_model_mapping_DeviceType",
        default
    )]
    pub device_type: Option<DeviceType>,
    #[serde(flatten)]
    pub fragments: serde_json::Map<String, serde_json::Value>,
}

/// A page of managed objects as returned by the inventory API, still unparsed.
///
/// The items stay as raw JSON so each is deserialized on its own: one device type this gateway
/// cannot read must cost only itself, never the rest of the page.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedObjectPage {
    #[serde(default, deserialize_with = "null_as_default")]
    pub managed_objects: Vec<serde_json::Value>,
    #[serde(default)]
    pub statistics: Option<Statistics>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Statistics {
    #[serde(rename = "totalPages", default)]
    pub total_pages: Option<u32>,
    #[serde(rename = "currentPage", default)]
    pub current_page: Option<u32>,
    #[serde(rename = "pageSize", default)]
    pub page_size: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceType {
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_true", deserialize_with = "null_as_true")]
    pub enabled: bool,
    #[serde(default, deserialize_with = "null_as_default")]
    pub mappings: Vec<MappingEntry>,
    #[serde(default)]
    pub apply_constraints: Option<ApplyConstraints>,
    #[serde(default)]
    pub subscription_type: Option<SubscriptionType>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub overridden_subscriptions: Vec<OverriddenSubscription>,
    #[serde(default)]
    pub referenced_root_node_id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub referenced_namespace_table: Vec<String>,
    #[serde(default)]
    pub referenced_server_id: Option<String>,
    #[serde(default)]
    pub referenced_server_name: Option<String>,
    #[serde(default)]
    pub ua_event_mappings: Option<serde_json::Value>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MappingEntry {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub browse_path: Vec<String>,
    #[serde(default)]
    pub referenced_node_id: Option<String>,
    #[serde(default)]
    pub measurement_creation: Option<MeasurementCreation>,
    #[serde(default)]
    pub event_creation: Option<EventCreation>,
    #[serde(default)]
    pub alarm_creation: Option<AlarmCreation>,
    #[serde(default)]
    pub custom_action: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeasurementCreation {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub series: Option<String>,
    #[serde(default)]
    pub fragment_name: Option<String>,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub static_fragments: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventCreation {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub static_fragments: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlarmCreation {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub static_fragments: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyConstraints {
    #[serde(default, deserialize_with = "null_as_default")]
    pub matches_server_ids: Vec<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub matches_node_ids: Vec<String>,
    #[serde(default)]
    pub server_object_has_fragment: Option<String>,
    #[serde(default)]
    pub browse_path_matches_regex: Option<String>,
    #[serde(default)]
    pub server_has_node_with_values: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverriddenSubscription {
    #[serde(default, deserialize_with = "null_as_default")]
    pub browse_path: Vec<String>,
    #[serde(default)]
    pub subscription_type: Option<SubscriptionType>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionType {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub cyclic_read_parameters: Option<CyclicReadParameters>,
    #[serde(default)]
    pub subscription_parameters: Option<SubscriptionParameters>,
}

/// How a device type or a single mapping entry wants its values delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionKind {
    /// OPC UA monitored items on a subscription.
    Subscription,
    /// Periodic Read service calls.
    CyclicRead,
    /// Explicitly no delivery.
    None,
}

impl SubscriptionType {
    pub fn kind(&self) -> SubscriptionKind {
        match self.r#type.as_deref().map(str::trim) {
            Some(t) if t.eq_ignore_ascii_case("Subscription") => SubscriptionKind::Subscription,
            Some(t) if t.eq_ignore_ascii_case("CyclicRead") => SubscriptionKind::CyclicRead,
            _ => SubscriptionKind::None,
        }
    }
}

/// Per-monitored-item parameters, as authored in the OPC UA UI.
///
/// Ported from `ua-device-capability-model/.../SubscriptionParameters.java`, including its
/// defaults: `dataChangeTrigger` is `StatusValue` and `discardOldest` is true.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionParameters {
    /// Requested sampling interval in milliseconds. `None` lets the server choose.
    #[serde(default)]
    pub sampling_rate: Option<f64>,
    #[serde(default)]
    pub queue_size: Option<u32>,
    /// `None`, `Absolute` or `Percent`.
    #[serde(default)]
    pub deadband_type: Option<String>,
    #[serde(default)]
    pub deadband_value: Option<f64>,
    /// OPC UA numeric range, for subscribing to a slice of an array node.
    #[serde(default)]
    pub ranges: Option<String>,
    /// `Status`, `StatusValue` or `StatusValueTimestamp`.
    #[serde(default)]
    pub data_change_trigger: Option<String>,
    #[serde(default)]
    pub discard_oldest: Option<bool>,
}

impl SubscriptionParameters {
    /// Whether every parameter is one the OPC UA services accept.
    ///
    /// The Java gateway refuses to subscribe an entry whose parameters are invalid rather than
    /// letting the server reject the monitored item; `SubscriptionParameters.isValid()`.
    pub fn is_valid(&self) -> bool {
        if self.deadband_type.is_some()
            && self.deadband_value.is_some()
            && self.deadband().is_none()
        {
            return false;
        }
        if let Some(trigger) = self.data_change_trigger.as_deref()
            && !matches!(trigger, "Status" | "StatusValue" | "StatusValueTimestamp")
        {
            return false;
        }
        if let Some(ranges) = self.ranges.as_deref().map(str::trim)
            && !ranges.is_empty()
            && !is_numeric_range(ranges)
        {
            return false;
        }
        true
    }

    /// The deadband filter, or `None` when no usable one is configured.
    ///
    /// `Absolute`/`Percent` need a value; `None` means no filter at all.
    pub fn deadband(&self) -> Option<(Deadband, f64)> {
        let value = self.deadband_value?;
        match self.deadband_type.as_deref()? {
            "Absolute" => Some((Deadband::Absolute, value)),
            "Percent" => Some((Deadband::Percent, value)),
            _ => None,
        }
    }

    pub fn data_change_trigger(&self) -> DataChangeTrigger {
        match self.data_change_trigger.as_deref() {
            Some("Status") => DataChangeTrigger::Status,
            Some("StatusValueTimestamp") => DataChangeTrigger::StatusValueTimestamp,
            _ => DataChangeTrigger::StatusValue,
        }
    }

    pub fn discard_oldest(&self) -> bool {
        self.discard_oldest.unwrap_or(true)
    }

    pub fn index_range(&self) -> Option<&str> {
        self.ranges
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
    }
}

/// OPC UA `DeadbandType`, restricted to the two forms that carry a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deadband {
    Absolute,
    Percent,
}

/// OPC UA `DataChangeTrigger`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataChangeTrigger {
    Status,
    StatusValue,
    StatusValueTimestamp,
}

/// An OPC UA NumericRange: `n`, `n:m`, or those joined by commas for each dimension.
fn is_numeric_range(s: &str) -> bool {
    !s.is_empty()
        && s.split(',').all(|dim| {
            let mut parts = dim.split(':');
            let ok = |p: Option<&str>| p.is_some_and(|p| !p.is_empty() && p.parse::<u32>().is_ok());
            let first = ok(parts.next());
            let second = match parts.next() {
                Some(p) => !p.is_empty() && p.parse::<u32>().is_ok(),
                None => true,
            };
            first && second && parts.next().is_none()
        })
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CyclicReadParameters {
    #[serde(default)]
    pub rate: Option<u64>,
    #[serde(default)]
    pub max_age: Option<f64>,
}

impl MeasurementCreation {
    /// Series name, falling back to a node-derived name exactly as the Java gateway does.
    pub fn series_name(&self, node_id: &str) -> String {
        non_blank(self.series.as_deref())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("series_{}", simplify(node_id)))
    }

    /// Fragment name, falling back to a node-derived name exactly as the Java gateway does.
    pub fn fragment_name(&self, node_id: &str) -> String {
        non_blank(self.fragment_name.as_deref())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("measurement_{}", simplify(node_id)))
    }
}

fn non_blank(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

fn simplify(node_id: &str) -> String {
    node_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

impl CyclicReadParameters {
    pub fn rate_ms(&self) -> Option<u64> {
        self.rate.filter(|r| *r > 0)
    }

    pub fn max_age(&self) -> f64 {
        self.max_age.unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(json: &str) -> SubscriptionParameters {
        serde_json::from_str(json).expect("parses")
    }

    #[test]
    fn subscription_parameter_defaults_match_the_java_gateway() {
        let p = params("{}");
        assert_eq!(p.data_change_trigger(), DataChangeTrigger::StatusValue);
        assert!(p.discard_oldest());
        assert_eq!(p.deadband(), None);
        assert_eq!(p.index_range(), None);
        assert!(p.is_valid());
    }

    #[test]
    fn deadband_needs_both_a_type_and_a_value() {
        assert_eq!(
            params(r#"{"deadbandType":"Absolute","deadbandValue":2.5}"#).deadband(),
            Some((Deadband::Absolute, 2.5))
        );
        assert_eq!(
            params(r#"{"deadbandType":"Percent","deadbandValue":10.0}"#).deadband(),
            Some((Deadband::Percent, 10.0))
        );
        // The UI writes "None" with a null value for "no deadband"; that is not a filter.
        assert_eq!(params(r#"{"deadbandType":"None"}"#).deadband(), None);
        assert_eq!(params(r#"{"deadbandValue":5.0}"#).deadband(), None);
    }

    #[test]
    fn invalid_parameters_are_rejected_before_subscribing() {
        assert!(!params(r#"{"deadbandType":"Sideways","deadbandValue":1.0}"#).is_valid());
        assert!(!params(r#"{"dataChangeTrigger":"Whenever"}"#).is_valid());
        assert!(!params(r#"{"ranges":"not-a-range"}"#).is_valid());
        assert!(params(r#"{"ranges":"1:5"}"#).is_valid());
        assert!(params(r#"{"ranges":"0:3,2:4"}"#).is_valid());
        assert!(params(r#"{"ranges":""}"#).is_valid());
    }

    #[test]
    fn index_range_ignores_blank_values() {
        assert_eq!(params(r#"{"ranges":"  "}"#).index_range(), None);
        assert_eq!(params(r#"{"ranges":" 1:5 "}"#).index_range(), Some("1:5"));
    }

    #[test]
    fn subscription_kind_reads_the_device_type_field() {
        let of = |json: &str| {
            serde_json::from_str::<SubscriptionType>(json)
                .expect("parses")
                .kind()
        };
        assert_eq!(
            of(r#"{"type":"Subscription"}"#),
            SubscriptionKind::Subscription
        );
        assert_eq!(of(r#"{"type":"cyclicread"}"#), SubscriptionKind::CyclicRead);
        assert_eq!(of(r#"{"type":"None"}"#), SubscriptionKind::None);
        assert_eq!(of("{}"), SubscriptionKind::None);
    }

    #[test]
    fn series_name_falls_back_to_node_id() {
        let m = MeasurementCreation {
            r#type: None,
            series: Some("  ".into()),
            fragment_name: None,
            unit: None,
            static_fragments: None,
        };
        assert_eq!(m.series_name("ns=2;s=Temp"), "series_ns2sTemp");
        assert_eq!(m.fragment_name("ns=2;s=Temp"), "measurement_ns2sTemp");
    }

    #[test]
    fn explicit_nulls_are_accepted_for_list_fields() {
        // Cumulocity writes null for unset fields. Before this was handled, one such device type
        // failed to deserialize and discarded every device type on the same inventory page.
        let dt: DeviceType = serde_json::from_str(
            r#"{"name":null,"enabled":null,"mappings":null,"overriddenSubscriptions":null,
                "referencedNamespaceTable":null,"applyConstraints":null,"uaEventMappings":null,
                "referencedRootNodeId":null,"referencedServerId":null}"#,
        )
        .expect("parses despite the nulls");
        assert_eq!(dt.name, "");
        assert!(
            dt.enabled,
            "a null enabled must stay true, not become false"
        );
        assert!(dt.mappings.is_empty());
        assert!(dt.overridden_subscriptions.is_empty());
        assert!(dt.referenced_namespace_table.is_empty());
    }

    #[test]
    fn a_page_keeps_items_it_cannot_parse_as_raw_json() {
        let page: ManagedObjectPage = serde_json::from_str(
            r#"{"managedObjects":[{"id":"1"},{"id":"2","broken":true}],
                "statistics":{"totalPages":1,"currentPage":1,"pageSize":1000}}"#,
        )
        .expect("parses");
        assert_eq!(page.managed_objects.len(), 2);
        assert_eq!(page.statistics.and_then(|s| s.total_pages), Some(1));
    }

    #[test]
    fn a_page_with_a_null_item_list_is_empty_not_an_error() {
        let page: ManagedObjectPage =
            serde_json::from_str(r#"{"managedObjects":null}"#).expect("parses");
        assert!(page.managed_objects.is_empty());
    }

    #[test]
    fn enabled_defaults_to_true_when_absent() {
        let dt: DeviceType = serde_json::from_str(r#"{"name":"x"}"#).expect("parses");
        assert!(dt.enabled);
    }
}
