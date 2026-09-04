//! Servers registered on the gateway device in Cumulocity — the third server source.
//!
//! `opcua-mgmt-service` creates a `c8y_OpcuaServer` managed object below the gateway object when a
//! server is added in the user interface, exactly as it does for the Java gateway. This reads them
//! back and hands them to the supervisor, which starts and stops server tasks to match.
//!
//! Two things are worth knowing about the shape of this.
//!
//! A server object arrives with no external id, so thin-edge cannot see it and telemetry published
//! for it would create a second device. One external id is planted on the object to adopt it — the
//! only write this gateway makes, idempotent and once per server. From then on the server's
//! registration, telemetry and twin data all travel over MQTT.
//!
//! The server's local id becomes its managed object id, which makes
//! `applyConstraints.matchesServerIds` work as authored: a device type authored in Cumulocity names
//! server object ids, and now so does this gateway.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tedge::{C8yProxy, ProxyError};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::{Config, ServerConfig};
use crate::gateway_device;

/// Cumulocity identity type used for every external id in this gateway, matching thin-edge.
const ID_TYPE: &str = "c8y_Serial";

/// Managed object type of a server registered on the gateway.
const SERVER_MO_TYPE: &str = "c8y_OpcuaServer";

/// Fragment holding the OPC UA client configuration of a server object.
const CLIENT_CONFIG_FRAGMENT: &str = "c8y_ua_ClientConfig";

const PAGE_SIZE: u32 = 100;

/// Upper bound on pages read per poll, so a misbehaving tenant cannot spin us forever.
const MAX_PAGES: u32 = 20;

/// Consecutive failed lookups of the gateway's own managed object before saying so out loud.
///
/// A few misses are normal on a cold start: the mapper may not have created the object yet, or may
/// not be able to reach Cumulocity. Persistent misses mean it is not coming — most likely because
/// the object was deleted in Cumulocity, which thin-edge cannot undo. See
/// [`gateway_device::missing_object_help`].
const MISSES_BEFORE_HELP: u32 = 5;

pub type CloudServers = Arc<Vec<ServerConfig>>;

/// Poll the gateway device's child devices until cancelled, publishing every change on `tx`.
///
/// Nothing here is fatal. A missing gateway object simply means the mapper has not created it yet;
/// a cloud outage means the servers already known keep running.
pub async fn run(
    config: Arc<Config>,
    proxy: C8yProxy,
    tx: watch::Sender<CloudServers>,
    cancel: CancellationToken,
) {
    let poll_interval = config.server_poll_interval();
    let mut backoff = Duration::from_secs(1);
    let mut gateway_mo_id: Option<String> = None;
    let mut revision = String::new();
    let mut misses = 0u32;

    loop {
        let mut delay = poll_interval;

        if gateway_mo_id.is_none() {
            match proxy
                .managed_object_id(ID_TYPE, &config.gateway.external_id())
                .await
            {
                Ok(Some(id)) => {
                    info!(
                        gateway_mo_id = id,
                        external_id = config.gateway.external_id(),
                        "found the gateway managed object"
                    );
                    gateway_mo_id = Some(id);
                    misses = 0;
                    backoff = Duration::from_secs(1);
                }
                Ok(None) => {
                    misses += 1;
                    // Said once, at the point where waiting has stopped being plausible.
                    if misses == MISSES_BEFORE_HELP {
                        gateway_device::missing_object_help(&config.gateway);
                    } else {
                        debug!(
                            external_id = config.gateway.external_id(),
                            misses,
                            retry_in_secs = backoff.as_secs(),
                            "the gateway managed object does not exist yet"
                        );
                    }
                    delay = backoff;
                    backoff = (backoff * 2).min(config.max_backoff());
                }
                Err(error) => {
                    warn!(%error, retry_in_secs = backoff.as_secs(), "cannot look up the gateway managed object");
                    delay = backoff;
                    backoff = (backoff * 2).min(config.max_backoff());
                }
            }
        }

        if let Some(id) = gateway_mo_id.as_deref() {
            match fetch(&proxy, &config, id).await {
                Ok(servers) => {
                    backoff = Duration::from_secs(1);
                    let fresh = revision_of(&servers);
                    if fresh != revision {
                        info!(
                            servers = servers.len(),
                            "servers registered on the gateway changed"
                        );
                        revision = fresh;
                        if tx.send(Arc::new(servers)).is_err() {
                            debug!("no consumers left; stopping the cloud server provider");
                            return;
                        }
                    }
                }
                Err(error) if error.is_transient() => {
                    warn!(
                        %error,
                        retry_in_secs = backoff.as_secs(),
                        "cannot read the servers registered on the gateway; keeping the current set"
                    );
                    delay = backoff;
                    backoff = (backoff * 2).min(config.max_backoff());
                }
                Err(ProxyError::Status { status: 404, .. }) => {
                    // The gateway object was deleted in the cloud. Look it up again rather than
                    // polling a dead id forever.
                    warn!("the gateway managed object is gone; looking it up again");
                    gateway_mo_id = None;
                }
                Err(error) => warn!(%error, "cannot read the servers registered on the gateway"),
            }
        }

        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
    }
}

/// Read every `c8y_OpcuaServer` below the gateway object and adopt it.
async fn fetch(
    proxy: &C8yProxy,
    config: &Config,
    gateway_mo_id: &str,
) -> Result<Vec<ServerConfig>, ProxyError> {
    let mut out = Vec::new();
    for page in 1..=MAX_PAGES {
        let body = proxy.child_devices(gateway_mo_id, page, PAGE_SIZE).await?;
        let references = body
            .get("references")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if references.is_empty() {
            break;
        }

        for reference in references {
            let Some(id) = reference
                .get("managedObject")
                .and_then(|mo| mo.get("id"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            // The reference carries only id, name and self, so the object itself has to be read
            // for its client configuration.
            let mo = match proxy.managed_object(id).await {
                Ok(mo) => mo,
                Err(error) => {
                    warn!(server_mo_id = id, %error, "cannot read a child device of the gateway");
                    continue;
                }
            };
            if mo.get("type").and_then(Value::as_str) != Some(SERVER_MO_TYPE) {
                continue;
            }
            if let Some(server) = to_server_config(config, id, &mo) {
                adopt(proxy, config, id).await;
                out.push(server);
            }
        }

        if references.len() < PAGE_SIZE as usize {
            break;
        }
        if page == MAX_PAGES {
            warn!(
                pages = MAX_PAGES,
                "stopped reading gateway child devices at the page limit"
            );
        }
    }
    Ok(out)
}

/// Plant the external id that lets thin-edge address this managed object.
///
/// Skipped when the external id already resolves. If it resolves to a *different* object something
/// is badly wrong — two gateways sharing an id, or a hand-made external id — and overwriting would
/// send this server's data to a stranger, so it is reported and left alone.
async fn adopt(proxy: &C8yProxy, config: &Config, mo_id: &str) {
    let external_id = config.gateway.server_external_id(mo_id);
    match proxy.managed_object_id(ID_TYPE, &external_id).await {
        Ok(Some(existing)) if existing == mo_id => return,
        Ok(Some(existing)) => {
            warn!(
                server_mo_id = mo_id,
                external_id,
                claimed_by = existing,
                "the external id for this server already points at another managed object; not \
                 touching it, so this server's data will not reach Cumulocity"
            );
            return;
        }
        Ok(None) => {}
        Err(error) => {
            warn!(server_mo_id = mo_id, %error, "cannot check the external id of this server");
            return;
        }
    }

    match proxy.create_external_id(mo_id, ID_TYPE, &external_id).await {
        Ok(()) => info!(
            server_mo_id = mo_id,
            external_id, "adopted a server registered in Cumulocity"
        ),
        Err(error) => warn!(
            server_mo_id = mo_id,
            external_id,
            %error,
            "cannot adopt this server; its telemetry would create a second device, so it is skipped"
        ),
    }
}

/// Convert a `c8y_OpcuaServer` object into a server this gateway can run.
///
/// Returns `None` for a server that must not be started, always with one warning saying why.
fn to_server_config(config: &Config, mo_id: &str, mo: &Value) -> Option<ServerConfig> {
    let name = mo
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(mo_id)
        .to_owned();

    let Some(client_config) = mo.get(CLIENT_CONFIG_FRAGMENT) else {
        warn!(
            server_mo_id = mo_id,
            "server object has no {CLIENT_CONFIG_FRAGMENT}; skipping it"
        );
        return None;
    };

    let Some(url) = client_config
        .get("serverUrl")
        .and_then(Value::as_str)
        .filter(|url| !url.trim().is_empty())
    else {
        warn!(
            server_mo_id = mo_id,
            "server object has no serverUrl; skipping it"
        );
        return None;
    };

    if client_config
        .get("targetConnectionState")
        .and_then(Value::as_str)
        .is_some_and(|state| state.eq_ignore_ascii_case("disabled"))
    {
        debug!(
            server_mo_id = mo_id,
            "server is disabled in Cumulocity; not connecting"
        );
        return None;
    }

    // `securityMode` is Cumulocity's name for what OPC UA calls the security policy. Only `NONE`
    // is supported here, and anything else is passed through unchanged so the message names what
    // was actually asked for.
    let security_policy = match client_config.get("securityMode").and_then(Value::as_str) {
        None => "None".to_owned(),
        Some(mode) if mode.eq_ignore_ascii_case("none") => "None".to_owned(),
        Some(mode) => {
            warn!(
                server_mo_id = mo_id,
                security_mode = mode,
                "server asks for a secured endpoint, which needs an application instance \
                 certificate this gateway does not have; skipping it"
            );
            return None;
        }
    };

    let (user, password) = match credentials(mo_id, client_config) {
        Ok(credentials) => credentials,
        Err(()) => return None,
    };

    Some(ServerConfig {
        id: mo_id.to_owned(),
        name,
        url: url.to_owned(),
        security_policy,
        user,
        password,
        // Scoping is done by `matchesServerIds` for a cloud-registered server, which works because
        // the local id is the managed object id the device type was authored against.
        device_types: Vec::new(),
        external_id: Some(config.gateway.server_external_id(mo_id)),
        topic_id: Some(mo_id.to_owned()),
    })
}

/// User credentials from the client configuration, if it asks for them.
///
/// `Err(())` means the server must not be started: it wants a password this gateway cannot produce.
/// The Java gateway stores that password encrypted with a key in its local database, and this
/// gateway has no local database — so a password authored in the user interface for the Java
/// gateway is unreadable here by construction, and connecting anonymously instead would fail in a
/// far less obvious way.
#[allow(clippy::result_unit_err)]
fn credentials(mo_id: &str, client_config: &Value) -> Result<(Option<String>, Option<String>), ()> {
    let mode = client_config
        .get("userIdentityMode")
        .and_then(Value::as_str)
        .unwrap_or("Anonymous");
    if mode.eq_ignore_ascii_case("anonymous") {
        return Ok((None, None));
    }

    if client_config
        .get("passwordEncrypted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        warn!(
            server_mo_id = mo_id,
            "server has an encrypted password, which only the Java gateway's local key store can \
             decrypt; skipping it. Configure this server on the device instead"
        );
        return Err(());
    }

    let user = client_config
        .get("userName")
        .and_then(Value::as_str)
        .filter(|user| !user.trim().is_empty())
        .map(str::to_owned);
    let password = client_config
        .get("userPassword")
        .and_then(Value::as_str)
        .filter(|password| !password.is_empty())
        .map(str::to_owned);

    if user.is_none() || password.is_none() {
        warn!(
            server_mo_id = mo_id,
            user_identity_mode = mode,
            "server asks for user authentication but carries no usable user name and password; \
             skipping it"
        );
        return Err(());
    }
    Ok((user, password))
}

/// Fingerprint of the current server set, so an unchanged poll publishes nothing.
fn revision_of(servers: &[ServerConfig]) -> String {
    let mut out = String::new();
    for server in servers {
        out.push_str(&server.id);
        out.push('\u{1}');
        out.push_str(&server.name);
        out.push('\u{1}');
        out.push_str(&server.url);
        out.push('\u{1}');
        out.push_str(&server.security_policy);
        out.push('\u{1}');
        out.push_str(server.user.as_deref().unwrap_or_default());
        out.push('\u{1}');
        out.push_str(server.password.as_deref().unwrap_or_default());
        out.push('\u{2}');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> Config {
        let mut config = Config::default();
        config.gateway.device_id = "tedge001".into();
        config
    }

    #[test]
    fn a_server_object_becomes_a_runnable_server() {
        let mo = json!({
            "id": "3769051265",
            "name": "opcserver",
            "type": "c8y_OpcuaServer",
            "c8y_ua_ClientConfig": {
                "serverUrl": "opc.tcp://opcserver:4840",
                "securityMode": "NONE",
                "userIdentityMode": "Anonymous",
                "targetConnectionState": "enabled",
            },
        });
        let server = to_server_config(&config(), "3769051265", &mo).expect("runnable");

        // The local id is the managed object id, which is what `matchesServerIds` names.
        assert_eq!(server.id, "3769051265");
        assert_eq!(server.name, "opcserver");
        assert_eq!(server.url, "opc.tcp://opcserver:4840");
        assert_eq!(server.security_policy, "None");
        assert_eq!(server.topic_id(), "3769051265");
        assert_eq!(
            server.external_id.as_deref(),
            Some("tedge001:device:opcua-gateway:device:3769051265")
        );
        assert!(server.is_adopted());
    }

    #[test]
    fn a_disabled_server_is_not_started() {
        let mo = json!({
            "c8y_ua_ClientConfig": {
                "serverUrl": "opc.tcp://opcserver:4840",
                "targetConnectionState": "disabled",
            },
        });
        assert!(to_server_config(&config(), "1", &mo).is_none());
    }

    #[test]
    fn a_secured_endpoint_is_reported_rather_than_half_connected() {
        let mo = json!({
            "c8y_ua_ClientConfig": {
                "serverUrl": "opc.tcp://opcserver:4840",
                "securityMode": "SIGN_AND_ENCRYPT",
            },
        });
        assert!(to_server_config(&config(), "1", &mo).is_none());
    }

    #[test]
    fn an_encrypted_password_cannot_be_used_here() {
        let mo = json!({
            "c8y_ua_ClientConfig": {
                "serverUrl": "opc.tcp://opcserver:4840",
                "userIdentityMode": "UsernamePassword",
                "userName": "operator",
                "userPassword": "AAAA...",
                "passwordEncrypted": true,
            },
        });
        assert!(to_server_config(&config(), "1", &mo).is_none());
    }

    #[test]
    fn a_plain_password_is_carried_through() {
        let mo = json!({
            "c8y_ua_ClientConfig": {
                "serverUrl": "opc.tcp://opcserver:4840",
                "userIdentityMode": "UsernamePassword",
                "userName": "operator",
                "userPassword": "secret",
                "passwordEncrypted": false,
            },
        });
        let server = to_server_config(&config(), "1", &mo).expect("runnable");
        assert_eq!(server.user.as_deref(), Some("operator"));
        assert_eq!(server.password.as_deref(), Some("secret"));
    }

    #[test]
    fn a_missing_client_config_is_skipped() {
        let mo = json!({ "id": "1", "type": "c8y_OpcuaServer" });
        assert!(to_server_config(&config(), "1", &mo).is_none());
    }

    #[test]
    fn the_revision_changes_only_when_something_relevant_does() {
        let mo = json!({
            "name": "opcserver",
            "c8y_ua_ClientConfig": { "serverUrl": "opc.tcp://a:4840" },
        });
        let renamed = json!({
            "name": "opcserver-2",
            "c8y_ua_ClientConfig": { "serverUrl": "opc.tcp://a:4840" },
        });
        let one = to_server_config(&config(), "1", &mo).expect("runnable");
        let again = to_server_config(&config(), "1", &mo).expect("runnable");
        let other = to_server_config(&config(), "1", &renamed).expect("runnable");

        assert_eq!(
            revision_of(std::slice::from_ref(&one)),
            revision_of(&[again])
        );
        assert_ne!(revision_of(&[one]), revision_of(&[other]));
    }
}
