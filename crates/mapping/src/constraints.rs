//! Scan-free `applyConstraints` evaluation.
//!
//! Ported from the `removeDeviceTypesThatDoNotMatch*` methods in
//! `mappings/BaseDeviceTypeMatchingService.java`, minus everything that needed a scanned address
//! space.

use tracing::warn;

use crate::model::{ApplyConstraints, DeviceType};
use crate::namespace::{self, NamespaceTable};

/// Whether a device type applies to a given server.
///
/// `known_server_ids` is every server this gateway is configured with. It is needed because
/// servers are configured locally and need no `c8y_OpcuaServer` object: a `matchesServerIds`
/// authored in the OPC UA UI holds Cumulocity managed object ids, which will never equal a local
/// server id. Scoping such a device type out entirely would make every UI-authored device type
/// unusable, so a constraint that names no server we know is reported and ignored instead.
///
/// Constraints this gateway cannot evaluate without an address space are likewise reported and
/// then ignored, so a device type is never silently dropped for a reason the operator cannot see.
pub fn applies(
    device_type_id: &str,
    device_type: &DeviceType,
    server_id: &str,
    known_server_ids: &[String],
    table: &NamespaceTable,
) -> bool {
    if !device_type.enabled {
        return false;
    }

    // `referencedServerId` records the server a device type was authored against; it is a hint,
    // not a filter. Only `matchesServerIds` scopes a device type to servers — see
    // `removeDeviceTypesThatDoNotMatchServerId` in the Java gateway.
    let Some(constraints) = device_type.apply_constraints.as_ref() else {
        return true;
    };
    report_unsupported(device_type_id, constraints);

    matches_server_ids(device_type_id, constraints, server_id, known_server_ids)
        && matches_node_ids(constraints, device_type, table)
}

fn matches_server_ids(
    device_type_id: &str,
    constraints: &ApplyConstraints,
    server_id: &str,
    known_server_ids: &[String],
) -> bool {
    let wanted: Vec<&str> = non_blank(&constraints.matches_server_ids);
    if wanted.is_empty() || wanted.contains(&server_id) {
        return true;
    }
    // Nothing in the constraint names a server this gateway knows, so it was authored against a
    // Cumulocity server registry we do not use. Scope locally instead, with the per-server device
    // type list.
    if !wanted
        .iter()
        .any(|id| known_server_ids.iter().any(|known| known == id))
    {
        warn!(
            device_type_id,
            server_id,
            ?wanted,
            "applyConstraints.matchesServerIds names no configured server; the constraint is \
             ignored — scope this device type with the server's own device_types list instead"
        );
        return true;
    }
    false
}

/// `matchesNodeIds` is compared against the root the device type was authored against.
fn matches_node_ids(
    constraints: &ApplyConstraints,
    device_type: &DeviceType,
    table: &NamespaceTable,
) -> bool {
    let wanted = non_blank(&constraints.matches_node_ids);
    if wanted.is_empty() {
        return true;
    }
    let Some(root) = device_type.referenced_root_node_id.as_deref() else {
        return false;
    };
    let Ok(root) = namespace::parse_node_id(table, root) else {
        return false;
    };
    wanted
        .iter()
        .any(|candidate| namespace::parse_node_id(table, candidate).is_ok_and(|id| id == root))
}

fn non_blank(values: &[String]) -> Vec<&str> {
    values
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect()
}

fn report_unsupported(device_type_id: &str, constraints: &ApplyConstraints) {
    if constraints
        .browse_path_matches_regex
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
    {
        warn!(
            device_type_id,
            "applyConstraints.browsePathMatchesRegex needs an address space scan and is not \
             supported; the constraint is ignored"
        );
    }
    if constraints
        .server_object_has_fragment
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
    {
        warn!(
            device_type_id,
            "applyConstraints.serverObjectHasFragment is not implemented yet; the constraint is \
             ignored"
        );
    }
    if constraints.server_has_node_with_values.is_some() {
        warn!(
            device_type_id,
            "applyConstraints.serverHasNodeWithValues is not implemented yet; the constraint is \
             ignored"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> NamespaceTable {
        NamespaceTable::new(vec!["http://opcfoundation.org/UA/".into()])
    }

    fn device_type(json: &str) -> DeviceType {
        serde_json::from_str(json).expect("parses")
    }

    fn known(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn empty_constraints_apply_everywhere() {
        let dt = device_type(
            r#"{"name":"x","applyConstraints":{"matchesServerIds":[],"matchesNodeIds":[]}}"#,
        );
        assert!(applies(
            "1",
            &dt,
            "server-a",
            &known(&["server-a"]),
            &table()
        ));
    }

    #[test]
    fn server_id_constraint_is_honoured() {
        let dt =
            device_type(r#"{"name":"x","applyConstraints":{"matchesServerIds":["server-b"]}}"#);
        let known = known(&["server-a", "server-b"]);
        assert!(!applies("1", &dt, "server-a", &known, &table()));
        assert!(applies("1", &dt, "server-b", &known, &table()));
    }

    #[test]
    fn referenced_server_id_is_a_hint_not_a_filter() {
        let dt = device_type(r#"{"name":"x","referencedServerId":"4368970342"}"#);
        assert!(applies(
            "1",
            &dt,
            "4368970342",
            &known(&["4368970342"]),
            &table()
        ));
        assert!(applies(
            "1",
            &dt,
            "another-server",
            &known(&["another-server"]),
            &table()
        ));
    }

    #[test]
    fn matches_server_ids_naming_only_unknown_servers_is_ignored() {
        // A device type authored in the OPC UA UI carries Cumulocity managed object ids here.
        // Those never equal a locally configured server id, so the constraint must not scope the
        // device type out of existence.
        let dt = device_type(r#"{"name":"x","applyConstraints":{"matchesServerIds":["34703"]}}"#);
        assert!(applies(
            "1",
            &dt,
            "plc-1",
            &known(&["plc-1", "plc-2"]),
            &table()
        ));
    }

    #[test]
    fn matches_server_ids_still_scopes_between_known_servers() {
        let dt = device_type(r#"{"name":"x","applyConstraints":{"matchesServerIds":["plc-2"]}}"#);
        let ids = known(&["plc-1", "plc-2"]);
        assert!(!applies("1", &dt, "plc-1", &ids, &table()));
        assert!(applies("1", &dt, "plc-2", &ids, &table()));
    }

    #[test]
    fn disabled_device_types_never_apply() {
        let dt = device_type(r#"{"name":"x","enabled":false}"#);
        assert!(!applies(
            "1",
            &dt,
            "server-a",
            &known(&["server-a"]),
            &table()
        ));
    }

    #[test]
    fn node_id_constraint_compares_against_the_authored_root() {
        let dt = device_type(
            r#"{"name":"x","referencedRootNodeId":"nsu=http://opcfoundation.org/UA/;i=84",
                "applyConstraints":{"matchesNodeIds":["i=84"]}}"#,
        );
        assert!(applies(
            "1",
            &dt,
            "server-a",
            &known(&["server-a"]),
            &table()
        ));
        let dt = device_type(
            r#"{"name":"x","referencedRootNodeId":"i=85",
                "applyConstraints":{"matchesNodeIds":["i=84"]}}"#,
        );
        assert!(!applies(
            "1",
            &dt,
            "server-a",
            &known(&["server-a"]),
            &table()
        ));
    }
}
