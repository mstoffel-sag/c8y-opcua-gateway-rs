//! The two mapping sources, reduced to one internal type.
//!
//! Both produce [`LoadedDeviceType`]; nothing downstream knows which source a device type came
//! from. Pushed files win on conflict, because a file on the device is a deliberate local
//! override of whatever inventory says.

use std::collections::BTreeMap;
use std::path::Path;

use tracing::{debug, warn};

use crate::MappingError;
use crate::model::{DeviceType, ManagedObject};

/// Where a device type was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Fetched from Cumulocity inventory through the thin-edge proxy.
    Pull,
    /// Read from a mapping file delivered by thin-edge configuration management.
    Push,
}

#[derive(Debug, Clone)]
pub struct LoadedDeviceType {
    pub id: String,
    pub origin: Origin,
    /// Cheap change marker: inventory `lastUpdated` for a pulled type, file size and mtime for a
    /// pushed one. Comparing these is how a reload is detected without diffing whole models.
    pub fingerprint: String,
    pub device_type: DeviceType,
}

/// Take a managed object fetched from inventory and keep it only if it carries a device type.
pub fn from_managed_object(mo: ManagedObject) -> Option<LoadedDeviceType> {
    let device_type = mo.device_type?;
    Some(LoadedDeviceType {
        id: mo.id,
        origin: Origin::Pull,
        fingerprint: mo.last_updated.unwrap_or_default(),
        device_type,
    })
}

/// Read every `*.json` mapping file in `dir`.
///
/// A file holds either a whole managed object as exported from Cumulocity, or just the device type
/// fragment. Accepting both means a mapping authored in the OPC UA UI can be pushed to a device
/// verbatim. A directory that does not exist is not an error — the push source is simply unused.
pub fn load_dir(dir: &Path) -> Vec<LoadedDeviceType> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            debug!(dir = %dir.display(), "no mapping directory; push source unused");
            return Vec::new();
        }
        Err(error) => {
            warn!(dir = %dir.display(), %error, "cannot read mapping directory");
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        match load_file(&path) {
            Ok(loaded) => out.push(loaded),
            Err(error) => warn!(path = %path.display(), %error, "ignoring mapping file"),
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn load_file(path: &Path) -> Result<LoadedDeviceType, MappingError> {
    let text = std::fs::read_to_string(path).map_err(|source| MappingError::ReadFile {
        path: path.to_owned(),
        source,
    })?;

    let fingerprint = std::fs::metadata(path)
        .map(|m| {
            let modified = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis());
            format!("{}:{modified}", m.len())
        })
        .unwrap_or_default();

    let fallback_id = || {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed")
            .to_owned()
    };

    // A managed object export first; a bare device type fragment second.
    if let Ok(mo) = serde_json::from_str::<ManagedObject>(&text)
        && let Some(device_type) = mo.device_type
    {
        let id = if mo.id.is_empty() {
            fallback_id()
        } else {
            mo.id
        };
        return Ok(LoadedDeviceType {
            id,
            origin: Origin::Push,
            fingerprint,
            device_type,
        });
    }

    let device_type =
        serde_json::from_str::<DeviceType>(&text).map_err(|source| MappingError::ParseFile {
            path: path.to_owned(),
            source,
        })?;
    Ok(LoadedDeviceType {
        id: fallback_id(),
        origin: Origin::Push,
        fingerprint,
        device_type,
    })
}

/// Change marker for a whole set, used to decide whether a reload is needed.
pub fn revision(loaded: &[LoadedDeviceType]) -> Revision {
    loaded
        .iter()
        .map(|l| (l.id.clone(), (l.origin, l.fingerprint.clone())))
        .collect()
}

/// Every loaded device type's change marker, keyed by id.
pub type Revision = BTreeMap<String, (Origin, String)>;

/// What changed between two revisions, so a reload can say which device type caused it.
///
/// Without this the only signal is "something changed", which is impossible to attribute when a
/// device type is edited in Cumulocity and a mapping file is pushed at about the same time — or
/// when an edit lands on a device type that a pushed file shadows and therefore has no effect.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RevisionDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Ids whose fingerprint moved, or whose winning source changed.
    pub changed: Vec<String>,
}

impl RevisionDiff {
    pub fn between(before: &Revision, after: &Revision) -> Self {
        let mut diff = Self::default();
        for (id, marker) in after {
            match before.get(id) {
                None => diff.added.push(id.clone()),
                Some(previous) if previous != marker => diff.changed.push(id.clone()),
                Some(_) => {}
            }
        }
        for id in before.keys() {
            if !after.contains_key(id) {
                diff.removed.push(id.clone());
            }
        }
        diff
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// Ids that were fetched from Cumulocity but discarded because a pushed file wins.
///
/// An edit made in the OPC UA user interface to one of these has no effect on the device, which is
/// worth saying out loud rather than leaving as silence.
pub fn shadowed(pulled: &[LoadedDeviceType], merged: &[LoadedDeviceType]) -> Vec<String> {
    pulled
        .iter()
        .filter(|p| {
            merged
                .iter()
                .any(|m| m.id == p.id && m.origin == Origin::Push)
        })
        .map(|p| p.id.clone())
        .collect()
}

/// Merge both sources into one set keyed by device type id, pushed files winning.
pub fn merge(
    pulled: Vec<LoadedDeviceType>,
    pushed: Vec<LoadedDeviceType>,
) -> Vec<LoadedDeviceType> {
    let mut by_id: BTreeMap<String, LoadedDeviceType> = BTreeMap::new();
    for loaded in pulled {
        by_id.insert(loaded.id.clone(), loaded);
    }
    for loaded in pushed {
        if let Some(replaced) = by_id.insert(loaded.id.clone(), loaded)
            && replaced.origin == Origin::Pull
        {
            debug!(
                device_type_id = replaced.id,
                "pushed mapping file overrides the device type fetched from inventory"
            );
        }
    }
    by_id.into_values().collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_revision_diff_names_what_moved() {
        use super::*;

        let before: Revision = [
            ("a".to_owned(), (Origin::Pull, "t1".to_owned())),
            ("b".to_owned(), (Origin::Pull, "t1".to_owned())),
        ]
        .into_iter()
        .collect();
        let after: Revision = [
            ("a".to_owned(), (Origin::Pull, "t2".to_owned())),
            ("c".to_owned(), (Origin::Push, "t1".to_owned())),
        ]
        .into_iter()
        .collect();

        let diff = RevisionDiff::between(&before, &after);
        assert_eq!(diff.changed, vec!["a".to_owned()]);
        assert_eq!(diff.added, vec!["c".to_owned()]);
        assert_eq!(diff.removed, vec!["b".to_owned()]);
        assert!(!diff.is_empty());

        assert!(RevisionDiff::between(&after, &after).is_empty());
    }

    /// The same fingerprint served by the other source is still a change: which file won moved.
    #[test]
    fn taking_over_a_device_type_counts_as_a_change() {
        use super::*;

        let before: Revision = [("a".to_owned(), (Origin::Pull, "t1".to_owned()))]
            .into_iter()
            .collect();
        let after: Revision = [("a".to_owned(), (Origin::Push, "t1".to_owned()))]
            .into_iter()
            .collect();
        assert_eq!(
            RevisionDiff::between(&before, &after).changed,
            vec!["a".to_owned()]
        );
    }

    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/pump01-device-type.json");

    #[test]
    fn reads_a_managed_object_export_from_disk() {
        let dir = std::env::temp_dir().join(format!("opcua-gw-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("pump.json"), FIXTURE).expect("write");
        std::fs::write(dir.join("notes.txt"), "ignored").expect("write");

        let loaded = load_dir(&dir);
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "4168968253");
        assert_eq!(loaded[0].origin, Origin::Push);
        assert_eq!(loaded[0].device_type.mappings.len(), 10);
    }

    #[test]
    fn missing_directory_is_not_an_error() {
        assert!(load_dir(Path::new("/nonexistent/opcua-mappings")).is_empty());
    }

    #[test]
    fn pushed_files_win_over_pulled_device_types() {
        let mo: ManagedObject = serde_json::from_str(FIXTURE).expect("parses");
        let pulled = vec![from_managed_object(mo.clone()).expect("has device type")];
        let mut pushed = pulled.clone();
        pushed[0].origin = Origin::Push;
        pushed[0].device_type.name = "local override".into();

        let merged = merge(pulled, pushed);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].origin, Origin::Push);
        assert_eq!(merged[0].device_type.name, "local override");
    }
}
