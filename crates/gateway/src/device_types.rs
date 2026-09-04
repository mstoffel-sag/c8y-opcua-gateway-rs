//! The device-type provider: both mapping sources, polled into one `watch` channel.
//!
//! Nothing is written to disk. Everything here lives in memory and is rebuilt on restart, so a
//! cold start with no proxy comes up unmapped and simply waits — it never exits, and it never
//! stops OPC UA collection for the servers it can already reach.

use std::sync::Arc;
use std::time::Duration;

use mapping::model::{DEVICE_TYPE_FRAGMENT, DEVICE_TYPE_MO_TYPE, ManagedObject, ManagedObjectPage};
use mapping::source::{self, LoadedDeviceType, Revision, RevisionDiff};
use tedge::{C8yProxy, ProxyError};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::Config;

/// Inventory page size, matching the Java gateway's device type fetch.
const PAGE_SIZE: u32 = 1000;

/// Upper bound on pages fetched per poll, so a misbehaving tenant cannot spin us forever.
const MAX_PAGES: u32 = 50;

pub type DeviceTypes = Arc<Vec<LoadedDeviceType>>;

/// Poll both mapping sources until cancelled, publishing every change on `tx`.
pub async fn run(
    config: Arc<Config>,
    proxy: Option<C8yProxy>,
    tx: watch::Sender<DeviceTypes>,
    cancel: CancellationToken,
) {
    let poll_interval = Duration::from_secs(config.proxy.poll_interval_secs.max(1));
    let mut revision = Revision::new();
    let mut backoff = Duration::from_secs(1);
    // Device types already fetched survive a cloud outage: they are only replaced by a good fetch.
    let mut pulled: Vec<LoadedDeviceType> = Vec::new();

    loop {
        let mut delay = poll_interval;

        if let Some(proxy) = proxy.as_ref() {
            match fetch(proxy, &config).await {
                Ok(fresh) => {
                    pulled = fresh;
                    backoff = Duration::from_secs(1);
                }
                Err(error) if error.is_transient() => {
                    warn!(
                        %error,
                        retry_in_secs = backoff.as_secs(),
                        kept = pulled.len(),
                        "device type fetch failed; keeping the device types already in memory"
                    );
                    delay = backoff;
                    backoff = (backoff * 2).min(config.max_backoff());
                }
                Err(error) => {
                    warn!(%error, "device type fetch failed and will not be retried differently");
                }
            }
        }

        let pushed = source::load_dir(&config.mappings.dir);
        let merged = source::merge(pulled.clone(), pushed);
        let fresh_revision = source::revision(&merged);
        let diff = RevisionDiff::between(&revision, &fresh_revision);

        if !diff.is_empty() {
            info!(
                device_types = merged.len(),
                pulled = pulled.len(),
                added = ?diff.added,
                removed = ?diff.removed,
                changed = ?diff.changed,
                "device types changed; reloading mappings"
            );
            // An edit in Cumulocity to a device type a pushed file shadows changes nothing on the
            // device. Saying so is the difference between "no effect" and "no idea".
            let shadowed = source::shadowed(&pulled, &merged);
            for id in diff.added.iter().chain(&diff.changed) {
                if shadowed.contains(id) {
                    info!(
                        device_type_id = id,
                        "this device type is served by a pushed mapping file, so the version in \
                         Cumulocity is ignored"
                    );
                }
            }
            revision = fresh_revision;
            if tx.send(Arc::new(merged)).is_err() {
                debug!("no mapping consumers left; stopping the device type provider");
                return;
            }
        }

        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// Fetch device types through the thin-edge proxy.
///
/// This re-fetches everything each poll rather than tracking `lastUpdated` incrementally. It costs
/// one request per minute against a page of at most [`PAGE_SIZE`] managed objects, and it detects
/// deletions for free — which the incremental path needs a separate total-count check for.
async fn fetch(proxy: &C8yProxy, config: &Config) -> Result<Vec<LoadedDeviceType>, ProxyError> {
    if !config.proxy.device_type_ids.is_empty() {
        let mut out = Vec::new();
        for id in &config.proxy.device_type_ids {
            let value = proxy.managed_object(id).await?;
            out.extend(read_managed_object(value));
        }
        return Ok(out);
    }

    let mut out = Vec::new();
    for page in 1..=MAX_PAGES {
        let value = proxy
            .device_types(DEVICE_TYPE_MO_TYPE, DEVICE_TYPE_FRAGMENT, page, PAGE_SIZE)
            .await?;
        let page_body: ManagedObjectPage = match serde_json::from_value(value) {
            Ok(body) => body,
            Err(error) => {
                warn!(%error, page, "cannot read the device type page; stopping this poll");
                break;
            }
        };

        let count = page_body.managed_objects.len();
        for value in page_body.managed_objects {
            out.extend(read_managed_object(value));
        }

        let last_page = page_body
            .statistics
            .and_then(|s| s.total_pages)
            .is_some_and(|total| page >= total);
        if last_page || count < PAGE_SIZE as usize {
            break;
        }
    }
    Ok(out)
}

/// Read one managed object into a device type, reporting rather than propagating a failure.
///
/// Each object is parsed on its own so that a device type this gateway cannot read costs only
/// itself. A tenant carrying one malformed device type must not lose the rest.
fn read_managed_object(value: serde_json::Value) -> Option<LoadedDeviceType> {
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<unknown>")
        .to_owned();

    match serde_json::from_value::<ManagedObject>(value) {
        Ok(mo) => match source::from_managed_object(mo) {
            Some(loaded) => Some(loaded),
            None => {
                warn!(
                    device_type_id = id,
                    "managed object carries no {DEVICE_TYPE_FRAGMENT} fragment; ignoring"
                );
                None
            }
        },
        Err(error) => {
            warn!(device_type_id = id, %error, "cannot read managed object as a device type; ignoring it");
            None
        }
    }
}
