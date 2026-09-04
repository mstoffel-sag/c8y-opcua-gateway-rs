//! OPC UA value extraction.
//!
//! Ported from `common-services/.../ValueLimitsValidator.java` and `BaseTask.isDataValueBad()`.

use opcua_types::{DataValue, Variant};

/// A data value that must not be published: bad status code, or no value at all.
///
/// The Java gateway drops these silently apart from a log line; so do we.
pub fn is_bad(value: &DataValue) -> bool {
    let status_ok = value.status.is_none_or(|s| s.is_good());
    !status_ok || matches!(value.value, None | Some(Variant::Empty))
}

/// Read a variant as a measurement value.
///
/// `None` means "not a number, or a number that cannot be represented" — the caller drops the
/// series rather than publishing a broken measurement. Booleans become 1/0, matching the Java
/// gateway so that boolean nodes mapped as measurements keep working.
pub fn as_number(variant: &Variant) -> Option<f64> {
    let n = match variant {
        Variant::Boolean(b) => f64::from(u8::from(*b)),
        Variant::SByte(v) => f64::from(*v),
        Variant::Byte(v) => f64::from(*v),
        Variant::Int16(v) => f64::from(*v),
        Variant::UInt16(v) => f64::from(*v),
        Variant::Int32(v) => f64::from(*v),
        Variant::UInt32(v) => f64::from(*v),
        Variant::Int64(v) => *v as f64,
        Variant::UInt64(v) => *v as f64,
        Variant::Float(v) => f64::from(*v),
        Variant::Double(v) => *v,
        Variant::String(s) => s.as_ref().trim().parse().ok()?,
        _ => return None,
    };
    n.is_finite().then_some(n)
}

/// Read a variant as an alarm active flag.
///
/// Ported from `ThinEdgeAlarmCreationTask.getBooleanValue`: numbers are active above zero and
/// strings are parsed the way `Boolean.parseBoolean` does.
pub fn as_bool(variant: &Variant) -> Option<bool> {
    match variant {
        Variant::Boolean(b) => Some(*b),
        Variant::String(s) => Some(s.as_ref().eq_ignore_ascii_case("true")),
        other => as_number(other).map(|n| n > 0.0),
    }
}

/// Render a variant for `${value}` substitution in event text.
pub fn as_text(variant: &Variant) -> String {
    match variant {
        Variant::Empty => String::new(),
        Variant::Boolean(v) => v.to_string(),
        Variant::SByte(v) => v.to_string(),
        Variant::Byte(v) => v.to_string(),
        Variant::Int16(v) => v.to_string(),
        Variant::UInt16(v) => v.to_string(),
        Variant::Int32(v) => v.to_string(),
        Variant::UInt32(v) => v.to_string(),
        Variant::Int64(v) => v.to_string(),
        Variant::UInt64(v) => v.to_string(),
        Variant::Float(v) => v.to_string(),
        Variant::Double(v) => v.to_string(),
        Variant::String(v) => v.as_ref().to_owned(),
        Variant::DateTime(v) => v.to_rfc3339(),
        Variant::LocalizedText(v) => v.text.as_ref().to_owned(),
        other => format!("{other:?}"),
    }
}

/// Render a variant as JSON, for the `c8y_DataValue` fragment on events.
pub fn as_json(variant: &Variant) -> serde_json::Value {
    match variant {
        Variant::Empty => serde_json::Value::Null,
        Variant::Boolean(v) => serde_json::Value::Bool(*v),
        Variant::String(v) => serde_json::Value::String(v.as_ref().to_owned()),
        Variant::DateTime(v) => serde_json::Value::String(v.to_rfc3339()),
        Variant::LocalizedText(v) => serde_json::Value::String(v.text.as_ref().to_owned()),
        other => match as_number(other) {
            Some(n) => serde_json::Number::from_f64(n).map_or_else(
                || serde_json::Value::String(as_text(other)),
                serde_json::Value::Number,
            ),
            None => serde_json::Value::String(as_text(other)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opcua_types::StatusCode;

    #[test]
    fn bad_status_and_empty_values_are_dropped() {
        assert!(is_bad(&DataValue {
            value: Some(Variant::Double(1.0)),
            status: Some(StatusCode::BadNodeIdUnknown),
            ..Default::default()
        }));
        assert!(is_bad(&DataValue::default()));
        assert!(!is_bad(&DataValue {
            value: Some(Variant::Double(1.0)),
            status: Some(StatusCode::Good),
            ..Default::default()
        }));
    }

    #[test]
    fn booleans_become_one_and_zero() {
        assert_eq!(as_number(&Variant::Boolean(true)), Some(1.0));
        assert_eq!(as_number(&Variant::Boolean(false)), Some(0.0));
    }

    #[test]
    fn non_finite_doubles_are_dropped() {
        assert_eq!(as_number(&Variant::Double(f64::NAN)), None);
        assert_eq!(as_number(&Variant::Double(f64::INFINITY)), None);
    }

    #[test]
    fn alarm_flag_reads_numbers_and_strings() {
        assert_eq!(as_bool(&Variant::Int32(3)), Some(true));
        assert_eq!(as_bool(&Variant::Int32(0)), Some(false));
        assert_eq!(as_bool(&Variant::from("TRUE")), Some(true));
        assert_eq!(as_bool(&Variant::from("nope")), Some(false));
    }
}
