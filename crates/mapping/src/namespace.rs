//! Browse path and NodeId string handling.
//!
//! Device types are authored against namespace *URIs* (`referencedNamespaceTable`), while a live
//! session addresses namespaces by *index*. Everything here translates between the two using the
//! namespace table read from the server, so nothing downstream needs to know about URIs.
//!
//! Ported from `client-lib/.../NodeIds.java`.

use opcua_types::{ByteString, Identifier, NodeId, QualifiedName, UAString};

use crate::MappingError;

const REGEX_PREFIX: &str = "regex(";
const REGEX_SUFFIX: &str = ")";

/// The `Server_NamespaceArray` of a live session: namespace index to URI.
#[derive(Debug, Clone, Default)]
pub struct NamespaceTable(Vec<String>);

impl NamespaceTable {
    pub fn new(uris: Vec<String>) -> Self {
        Self(uris)
    }

    pub fn index_of(&self, uri: &str) -> Option<u16> {
        self.0
            .iter()
            .position(|u| u == uri)
            .and_then(|i| u16::try_from(i).ok())
    }

    pub fn uri_of(&self, index: u16) -> Option<&str> {
        self.0.get(usize::from(index)).map(String::as_str)
    }

    pub fn uris(&self) -> &[String] {
        &self.0
    }
}

/// True when a browse path segment uses the unsupported `regex(...)` form.
pub fn is_regex(segment: &str) -> bool {
    segment.starts_with(REGEX_PREFIX) && segment.ends_with(REGEX_SUFFIX)
}

/// Convert one `<nsUriOrIndex>:<name>` browse path segment into a [`QualifiedName`].
///
/// A namespace URI contains colons of its own, so the delimiter is found by walking backwards from
/// the last colon until the prefix resolves to a namespace — the same reverse-float the Java
/// gateway does. A segment whose prefix never resolves is treated as namespace 0, which keeps
/// plain names such as `Objects` working.
pub fn to_qualified_name(table: &NamespaceTable, segment: &str) -> QualifiedName {
    let Some(mut delim) = segment.rfind(':') else {
        return QualifiedName::new(0, segment);
    };

    loop {
        let (prefix, value) = (&segment[..delim], &segment[delim + 1..]);
        let index = match prefix.parse::<u16>() {
            Ok(index) => Some(index),
            Err(_) => table.index_of(prefix),
        };
        if let Some(index) = index {
            return QualifiedName::new(index, value);
        }
        match prefix.rfind(':') {
            Some(next) => delim = next,
            None => return QualifiedName::new(0, segment),
        }
    }
}

/// Convert a whole browse path into qualified names.
pub fn to_qualified_names(table: &NamespaceTable, browse_path: &[String]) -> Vec<QualifiedName> {
    browse_path
        .iter()
        .map(|segment| to_qualified_name(table, segment))
        .collect()
}

/// Parse a NodeId string, accepting both the `ns=<index>;…` and the `nsu=<uri>;…` forms.
///
/// The `nsu=` form is what device types store in `referencedRootNodeId`; it is resolved against the
/// live namespace table.
pub fn parse_node_id(table: &NamespaceTable, s: &str) -> Result<NodeId, MappingError> {
    let mut namespace = 0u16;
    let mut rest = s;

    for _ in 0..3 {
        let Some((head, tail)) = rest.split_once(';') else {
            break;
        };
        let lower = head.to_ascii_lowercase();
        if let Some(uri) = lower.strip_prefix("nsu=") {
            // Take the URI from the original slice: lowercasing must not leak into it.
            let uri = &head[head.len() - uri.len()..];
            namespace = table
                .index_of(uri)
                .ok_or_else(|| MappingError::UnknownNamespace(uri.to_owned()))?;
        } else if let Some(index) = lower.strip_prefix("ns=") {
            namespace = index
                .parse()
                .map_err(|_| MappingError::InvalidNodeId(s.to_owned()))?;
        } else if lower.starts_with("svr=") {
            // Server index: this gateway only ever talks to the local server.
        } else {
            break;
        }
        rest = tail;
    }

    let identifier =
        parse_identifier(rest).ok_or_else(|| MappingError::InvalidNodeId(s.to_owned()))?;
    Ok(NodeId::new(namespace, identifier))
}

fn parse_identifier(s: &str) -> Option<Identifier> {
    let (kind, value) = s.split_at_checked(2)?;
    match kind {
        "i=" => value.parse::<u32>().ok().map(Identifier::Numeric),
        "s=" => Some(Identifier::String(UAString::from(value))),
        "g=" => value.parse().ok().map(Identifier::Guid),
        "b=" => Some(Identifier::ByteString(ByteString::from_base64(value)?)),
        _ => None,
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

    #[test]
    fn resolves_namespace_uri_containing_colons() {
        let qn = to_qualified_name(&table(), "http://www.cumulocity.com:Pump01");
        assert_eq!(qn.namespace_index, 2);
        assert_eq!(qn.name.as_ref(), "Pump01");
    }

    #[test]
    fn resolves_urn_style_namespace_uri() {
        let qn = to_qualified_name(&table(), "urn:freeopcua:python:server:Boiler");
        assert_eq!(qn.namespace_index, 1);
        assert_eq!(qn.name.as_ref(), "Boiler");
    }

    #[test]
    fn accepts_numeric_namespace_prefix_and_plain_name() {
        assert_eq!(to_qualified_name(&table(), "2:Pump01").namespace_index, 2);
        let plain = to_qualified_name(&table(), "Objects");
        assert_eq!(plain.namespace_index, 0);
        assert_eq!(plain.name.as_ref(), "Objects");
    }

    #[test]
    fn unresolvable_prefix_falls_back_to_namespace_zero() {
        let qn = to_qualified_name(&table(), "http://unknown.example/:Thing");
        assert_eq!(qn.namespace_index, 0);
        assert_eq!(qn.name.as_ref(), "http://unknown.example/:Thing");
    }

    #[test]
    fn parses_root_node_id_with_namespace_uri() {
        let id = parse_node_id(&table(), "nsu=http://www.cumulocity.com;i=84").expect("parses");
        assert_eq!(id, NodeId::new(2, 84u32));
    }

    #[test]
    fn parses_plain_and_indexed_node_ids() {
        assert_eq!(
            parse_node_id(&table(), "i=85").expect("parses"),
            NodeId::new(0, 85u32)
        );
        assert_eq!(
            parse_node_id(&table(), "ns=2;s=Temp").expect("parses"),
            NodeId::new(2, "Temp")
        );
    }

    #[test]
    fn rejects_unknown_namespace_uri() {
        assert!(matches!(
            parse_node_id(&table(), "nsu=http://nope/;i=1"),
            Err(MappingError::UnknownNamespace(_))
        ));
    }
}
