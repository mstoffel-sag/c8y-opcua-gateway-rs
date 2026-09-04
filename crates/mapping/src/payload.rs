//! thin-edge telemetry payload construction.
//!
//! Ported from `mappingsexecution/tasks/ThinEdge*Task.java`, which is the closest thing to a spec
//! for the `te/` payloads.

use opcua_types::{DataValue, DateTime};
use serde_json::{Map, Value, json};

use crate::model::{AlarmCreation, EventCreation, MeasurementCreation};
use crate::value;

/// Fragment prefix carrying the originating OPC UA node, as the Java gateway emits it.
const NODE_ID_FRAGMENT_PREFIX: &str = "c8y_ua_SourceNodeId_";

const VALUE_PLACEHOLDER: &str = "${value}";

/// Timestamp of a data value: source, then server, then now.
pub fn timestamp(value: &DataValue) -> String {
    value
        .source_timestamp
        .or(value.server_timestamp)
        .unwrap_or_else(DateTime::now)
        .to_rfc3339()
}

/// One measurement message, grouping every series read in the same cycle.
///
/// A cyclic read returns all of a device type's nodes at once, so they belong in one message —
/// the local broker and the mapper both see one publish per cycle instead of one per node.
#[derive(Debug, Default)]
pub struct MeasurementBuilder {
    fragments: Map<String, Value>,
    time: Option<String>,
    series_count: usize,
}

impl MeasurementBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one series. Returns false when the value could not be read as a number.
    pub fn add(
        &mut self,
        creation: &MeasurementCreation,
        node_id: &str,
        data_value: &DataValue,
    ) -> bool {
        let Some(variant) = data_value.value.as_ref() else {
            return false;
        };
        let Some(number) = value::as_number(variant) else {
            return false;
        };
        let Some(number) = serde_json::Number::from_f64(number) else {
            return false;
        };

        let fragment = creation.fragment_name(node_id);
        let series = creation.series_name(node_id);
        self.fragments
            .entry(fragment)
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .map(|f| f.insert(series, Value::Number(number)));
        self.series_count += 1;

        // A cyclic read hands back one timestamp per node; the first good one dates the batch.
        if self.time.is_none() {
            self.time = Some(timestamp(data_value));
        }
        for fragment in creation.static_fragments.iter().flatten() {
            self.fragments
                .entry(fragment.clone())
                .or_insert_with(|| Value::Object(Map::new()));
        }
        true
    }

    pub fn is_empty(&self) -> bool {
        self.series_count == 0
    }

    /// Finish the message, or `None` when nothing publishable was added.
    pub fn build(self) -> Option<Value> {
        if self.series_count == 0 {
            return None;
        }
        let mut out = self.fragments;
        out.insert(
            "time".into(),
            Value::String(self.time.unwrap_or_else(|| DateTime::now().to_rfc3339())),
        );
        Some(Value::Object(out))
    }
}

/// Retained `m/<type>/meta` payload declaring the unit of each series.
pub fn measurement_meta(series_units: &[(String, String)]) -> Option<Value> {
    if series_units.is_empty() {
        return None;
    }
    let mut out = Map::new();
    for (series, unit) in series_units {
        out.insert(series.clone(), json!({ "unit": unit }));
    }
    Some(Value::Object(out))
}

/// `te/…/e/<type>` payload. `${value}` in the configured text is replaced by the value.
pub fn event(creation: &EventCreation, node_id: &str, data_value: &DataValue) -> Value {
    let variant = data_value.value.clone().unwrap_or_default();
    let text = creation
        .text
        .as_deref()
        .unwrap_or_default()
        .replace(VALUE_PLACEHOLDER, &value::as_text(&variant));

    let mut out = Map::new();
    out.insert("time".into(), Value::String(timestamp(data_value)));
    out.insert("text".into(), Value::String(text));
    out.insert(
        format!("{NODE_ID_FRAGMENT_PREFIX}{node_id}"),
        Value::Object(Map::new()),
    );
    out.insert(
        "c8y_DataValue".into(),
        json!({
            "value": value::as_json(&variant),
            "statusCode": data_value.status.map(|s| s.bits()),
            "sourceTimestamp": data_value.source_timestamp.map(|t| t.to_rfc3339()),
            "serverTimestamp": data_value.server_timestamp.map(|t| t.to_rfc3339()),
        }),
    );
    add_static_fragments(&mut out, creation.static_fragments.as_deref());
    Value::Object(out)
}

/// `te/…/a/<type>` payload for a raised alarm. Clearing is an empty retained publish instead.
pub fn alarm(creation: &AlarmCreation, node_id: &str, data_value: &DataValue) -> Value {
    let mut out = Map::new();
    out.insert("time".into(), Value::String(timestamp(data_value)));
    out.insert(
        "text".into(),
        Value::String(creation.text.clone().unwrap_or_default()),
    );
    out.insert("severity".into(), Value::String(severity(creation)));
    out.insert(
        format!("{NODE_ID_FRAGMENT_PREFIX}{node_id}"),
        Value::Object(Map::new()),
    );
    add_static_fragments(&mut out, creation.static_fragments.as_deref());
    Value::Object(out)
}

/// thin-edge expects a lower-case severity; device types store the Cumulocity spelling.
fn severity(creation: &AlarmCreation) -> String {
    creation
        .severity
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("major")
        .to_lowercase()
}

fn add_static_fragments(out: &mut Map<String, Value>, fragments: Option<&[String]>) {
    for fragment in fragments.unwrap_or_default() {
        out.entry(fragment.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opcua_types::{StatusCode, Variant};

    fn good(variant: Variant) -> DataValue {
        DataValue {
            value: Some(variant),
            status: Some(StatusCode::Good),
            source_timestamp: Some(DateTime::ymd_hms(2026, 9, 4, 10, 0, 0)),
            source_picoseconds: None,
            server_timestamp: None,
            server_picoseconds: None,
        }
    }

    fn measurement_creation(fragment: &str, series: &str) -> MeasurementCreation {
        MeasurementCreation {
            r#type: Some("pump".into()),
            series: Some(series.into()),
            fragment_name: Some(fragment.into()),
            unit: Some("l/m".into()),
            static_fragments: None,
        }
    }

    #[test]
    fn groups_series_by_fragment_in_one_message() {
        let mut b = MeasurementBuilder::new();
        assert!(b.add(
            &measurement_creation("pump", "flow"),
            "ns=2;i=1",
            &good(Variant::Double(3.5))
        ));
        assert!(b.add(
            &measurement_creation("pump", "power"),
            "ns=2;i=2",
            &good(Variant::Int32(42))
        ));
        let out = b.build().expect("has series");
        assert_eq!(out["pump"]["flow"], json!(3.5));
        assert_eq!(out["pump"]["power"], json!(42.0));
        assert_eq!(out["time"], json!("2026-09-04T10:00:00.000Z"));
    }

    #[test]
    fn non_numeric_series_is_skipped_not_published() {
        let mut b = MeasurementBuilder::new();
        assert!(!b.add(
            &measurement_creation("pump", "state"),
            "ns=2;i=1",
            &good(Variant::from("running")),
        ));
        assert!(b.build().is_none());
    }

    #[test]
    fn event_text_substitutes_value() {
        let creation = EventCreation {
            r#type: Some("pumpState".into()),
            text: Some("Pump state ${value}".into()),
            static_fragments: None,
        };
        let out = event(&creation, "ns=2;i=9", &good(Variant::from("RUNNING")));
        assert_eq!(out["text"], json!("Pump state RUNNING"));
        assert!(out.get("c8y_ua_SourceNodeId_ns=2;i=9").is_some());
    }

    #[test]
    fn alarm_severity_is_lowercased_for_thin_edge() {
        let creation = AlarmCreation {
            r#type: Some("pumpAlert".into()),
            text: Some("Pump in alert state".into()),
            severity: Some("CRITICAL".into()),
            static_fragments: None,
        };
        let out = alarm(&creation, "ns=2;i=9", &good(Variant::Boolean(true)));
        assert_eq!(out["severity"], json!("critical"));
    }
}
