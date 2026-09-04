//! thin-edge entity topic identifiers.
//!
//! See <https://thin-edge.github.io/thin-edge.io/references/mqtt-api/>. Only the 2.x `te/` scheme
//! exists here; the legacy `tedge/` topics were removed in thin-edge 2.0.

use std::fmt;

/// Root of the thin-edge MQTT API.
pub const TE_ROOT: &str = "te";

/// The four-segment entity identifier of the default topic scheme.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntityTopicId(String);

impl EntityTopicId {
    /// The main device: `device/main//`.
    pub fn main_device() -> Self {
        Self("device/main//".to_owned())
    }

    /// A child device: `device/<id>//`. Its parent is set at registration, not in the topic.
    pub fn child_device(id: &str) -> Self {
        Self(format!("device/{id}//"))
    }

    /// A service on the main device: `device/main/service/<name>`.
    pub fn main_service(name: &str) -> Self {
        Self(format!("device/main/service/{name}"))
    }

    /// The entity's own registration topic.
    pub fn registration_topic(&self) -> String {
        format!("{TE_ROOT}/{}", self.0)
    }

    /// A channel below the entity, e.g. `m/flow` or `status/health`.
    pub fn channel_topic(&self, channel: &str) -> String {
        format!("{TE_ROOT}/{}/{channel}", self.0)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Make an arbitrary string usable as a topic identifier segment.
///
/// A device type id is a Cumulocity managed object id when pulled but a file name when pushed, and
/// a topic identifier decides the device's identity in the cloud. Anything outside a conservative
/// set becomes `-`, and the result is lower-cased, so the same input always yields the same
/// entity across restarts.
pub fn sanitize_id(input: &str) -> String {
    let mut out: String = input
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' | '_' => c,
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => '-',
        })
        .collect();
    // Collapse runs and trim, so "Pump 01 // x" does not become "pump-01----x".
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "unnamed".to_owned()
    } else {
        trimmed.to_owned()
    }
}

impl fmt::Display for EntityTopicId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_ids_into_stable_topic_segments() {
        assert_eq!(sanitize_id("4168968253"), "4168968253");
        assert_eq!(sanitize_id("Pump01 / cooling+line"), "pump01-cooling-line");
        assert_eq!(sanitize_id("--weird--"), "weird");
        assert_eq!(sanitize_id("///"), "unnamed");
        // Stability is the point: identity in the cloud follows the topic id.
        assert_eq!(sanitize_id("Pump#01"), sanitize_id("Pump#01"));
    }

    #[test]
    fn builds_the_documented_topics() {
        let child = EntityTopicId::child_device("plc-1");
        assert_eq!(child.registration_topic(), "te/device/plc-1//");
        assert_eq!(child.channel_topic("m/flow"), "te/device/plc-1///m/flow");

        let service = EntityTopicId::main_service("opcua-gateway");
        assert_eq!(
            service.channel_topic("status/health"),
            "te/device/main/service/opcua-gateway/status/health"
        );
    }
}
