//! Configuration: file, then environment (`OPCUA_GW__…`), then CLI flags.
//!
//! Validated once here and passed down as a typed struct; nothing reads configuration again at
//! runtime, and there are no globals.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};

pub const ENV_PREFIX: &str = "OPCUA_GW__";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub mqtt: MqttConfig,
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub subscription: SubscriptionConfig,
    #[serde(default)]
    pub mappings: MappingsConfig,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
}

/// The Cumulocity gateway device.
///
/// With this enabled the gateway registers one more child device of the thin-edge main device,
/// typed `c8y_OPCUA_Device_Agent`, and parents every OPC UA server below it. That is the shape the
/// existing OPC UA user interface recognises, so servers can be created there and the gateway
/// picks them up — a third server source alongside inline `[[servers]]` and pushed server files.
///
/// It changes nothing about the data path: telemetry still goes out on `te/` topics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    /// Disable to run purely thin-edge-native: no gateway device, servers registered directly
    /// below the main device, and no Cumulocity server objects. Required behind a non-Cumulocity
    /// bridge.
    pub enabled: bool,
    /// Topic-id segment of the gateway child device, and half of its external id. Stable across
    /// restarts by definition — it decides the gateway's identity in the cloud.
    pub id: String,
    /// Display name, which becomes the managed object name.
    pub name: String,
    /// thin-edge device id, the other half of every external id this gateway derives. Empty means
    /// read it from `tedge config get device.id`, the same way the proxy bind address is meant to
    /// be read rather than hard-coded.
    #[serde(default)]
    pub device_id: String,
    /// How often the gateway's own child devices are read back to find servers created in the
    /// user interface. Matches the Java gateway's `gateway.detectServersAddedOrRemoveInterval`.
    pub server_poll_interval_secs: u64,
    /// Reported as `c8y_SupportedOperations` on the gateway device. The user interface uses this
    /// to decide the device is an OPC UA gateway. Operations that arrive and are not implemented
    /// are failed with a reason rather than left pending.
    pub supported_operations: Vec<String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            id: "opcua-gateway".to_owned(),
            name: "OPC UA Gateway".to_owned(),
            device_id: String::new(),
            server_poll_interval_secs: 60,
            supported_operations: vec!["c8y_OpcuaConfiguration".to_owned()],
        }
    }
}

impl GatewayConfig {
    /// External id of the gateway device, in thin-edge's own `<device>:device:<child>` shape.
    pub fn external_id(&self) -> String {
        format!("{}:device:{}", self.device_id, self.id)
    }

    /// External id for a `c8y_OpcuaServer` managed object adopted below this gateway.
    ///
    /// Keyed by managed object id, not by name: the object is the identity, and a name can be
    /// edited in the user interface at any time.
    pub fn server_external_id(&self, mo_id: &str) -> String {
        format!("{}:device:{}:device:{mo_id}", self.device_id, self.id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    /// Tokio worker threads. Low by default: this workload is I/O bound and runs on edge hardware.
    pub worker_threads: usize,
    /// How long to wait for an OPC UA session to activate.
    pub connect_timeout_secs: u64,
    /// Backoff cap for reconnect attempts and for device-type retries.
    pub max_backoff_secs: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            worker_threads: 2,
            connect_timeout_secs: 30,
            max_backoff_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MqttConfig {
    pub host: String,
    pub port: u16,
    pub client_id: String,
    /// Service name of the gateway on the thin-edge main device.
    pub service_name: String,
    pub capacity: usize,
    pub keep_alive_secs: u64,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: 1883,
            client_id: "c8y-opcua-gateway".to_owned(),
            service_name: "opcua-gateway".to_owned(),
            capacity: 1024,
            keep_alive_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    /// The pull mapping source. Disable it to run purely on pushed mapping files.
    pub enabled: bool,
    pub base_url: String,
    /// Device-type poll interval; the Java gateway's `gateway.subscriptionUpdate.interval`.
    pub poll_interval_secs: u64,
    pub timeout_secs: u64,
    /// Restrict the fetch to these managed object ids. Empty means every `c8y_OpcuaDeviceType`.
    #[serde(default)]
    pub device_type_ids: Vec<String>,
    /// mTLS material, for a thin-edge deployment whose local endpoints require a client
    /// certificate (`tedge config get c8y.proxy.cert_path` and friends).
    #[serde(default)]
    pub client_cert: Option<PathBuf>,
    #[serde(default)]
    pub client_key: Option<PathBuf>,
    #[serde(default)]
    pub ca_cert: Option<PathBuf>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: tedge::proxy::DEFAULT_BASE_URL.to_owned(),
            poll_interval_secs: 60,
            timeout_secs: 30,
            device_type_ids: Vec::new(),
            client_cert: None,
            client_key: None,
            ca_cert: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionConfig {
    /// Publishing interval of the server's subscription; `gateway.subscription.reportingRate`.
    pub publishing_interval_ms: u64,
    pub lifetime_count: u32,
    pub max_keep_alive_count: u32,
    pub max_notifications_per_publish: u32,
    pub priority: u8,
    /// Monitored item queue size when a device type does not specify one.
    pub default_queue_size: u32,
    /// Bound on undelivered notifications. On overflow they are dropped with a rate-limited
    /// warning; there is no unbounded queue and no offline buffer anywhere in this gateway.
    pub channel_capacity: usize,
    /// Batched measurements are flushed at least this often.
    pub flush_interval_ms: u64,
    /// …and immediately once this many series are waiting.
    pub flush_max_series: usize,
}

impl Default for SubscriptionConfig {
    fn default() -> Self {
        Self {
            publishing_interval_ms: 100,
            lifetime_count: 600,
            max_keep_alive_count: 200,
            max_notifications_per_publish: 500,
            priority: 0,
            default_queue_size: 10,
            channel_capacity: 4096,
            flush_interval_ms: 1_000,
            flush_max_series: 200,
        }
    }
}

impl From<&SubscriptionConfig> for opcua_conn::SubscriptionParams {
    fn from(c: &SubscriptionConfig) -> Self {
        Self {
            publishing_interval: Duration::from_millis(c.publishing_interval_ms),
            lifetime_count: c.lifetime_count,
            max_keep_alive_count: c.max_keep_alive_count,
            max_notifications_per_publish: c.max_notifications_per_publish,
            priority: c.priority,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingsConfig {
    /// The pushed-configuration directory thin-edge configuration management writes into, and
    /// which this gateway only ever reads. Mapping files sit at its root; server definitions sit
    /// in the `servers/` subdirectory (see [`SERVERS_SUBDIR`]).
    pub dir: PathBuf,
}

/// Subdirectory of [`MappingsConfig::dir`] holding one TOML file per OPC UA server.
pub const SERVERS_SUBDIR: &str = "servers";

impl Default for MappingsConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("/etc/tedge/opcua"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Free-form local identifier for this server.
    ///
    /// This is *not* a Cumulocity managed object id: servers are configured on the device and
    /// need no `c8y_OpcuaServer` object. It is what `applyConstraints.matchesServerIds` is
    /// compared against, and it appears in every log line for this server.
    pub id: String,
    /// thin-edge child device topic id and display name for this server.
    pub name: String,
    pub url: String,
    #[serde(default = "default_security_policy")]
    pub security_policy: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    /// Device type ids allowed on this server; empty accepts every applicable device type.
    ///
    /// This is how a device type is scoped to a server now that Cumulocity is not the server
    /// registry: `matchesServerIds` cannot name a local server id, so the decision is made here,
    /// next to the server definition.
    #[serde(default)]
    pub device_types: Vec<String>,

    /// External id to register this server's entity under, set only for a server adopted from a
    /// `c8y_OpcuaServer` managed object. `None` lets thin-edge derive it from the topic path,
    /// which is what a device-configured server wants.
    ///
    /// Not configurable: it is derived from the managed object, and inventing one by hand would
    /// point telemetry at the wrong object.
    #[serde(skip)]
    pub external_id: Option<String>,

    /// Topic-id segment override, again only for an adopted server, where the managed object id
    /// is the stable identity and the name is not.
    #[serde(skip)]
    pub topic_id: Option<String>,
}

impl ServerConfig {
    /// Topic-id segment of this server's thin-edge entity.
    pub fn topic_id(&self) -> String {
        tedge::topic::sanitize_id(self.topic_id.as_deref().unwrap_or(&self.name))
    }

    /// Whether this server came from a Cumulocity `c8y_OpcuaServer` object rather than from
    /// configuration on the device.
    pub fn is_adopted(&self) -> bool {
        self.external_id.is_some()
    }

    /// Whether this server accepts the given device type.
    pub fn accepts_device_type(&self, device_type_id: &str) -> bool {
        self.device_types.is_empty()
            || self
                .device_types
                .iter()
                .any(|allowed| allowed.trim() == device_type_id)
    }
}

fn default_security_policy() -> String {
    "None".to_owned()
}

impl Config {
    /// Load from an optional file, then the environment, then pushed server definitions.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let mut figment = Figment::from(Serialized::defaults(Config::default()));
        if let Some(path) = path {
            if !path.exists() {
                bail!("configuration file {} does not exist", path.display());
            }
            figment = figment.merge(Toml::file(path));
        }
        let mut config: Config = figment
            .merge(Env::prefixed(ENV_PREFIX).split("__"))
            .extract()
            .context("invalid configuration")?;

        let pushed = load_server_dir(&config.mappings.dir.join(SERVERS_SUBDIR))?;
        config.servers = merge_servers(std::mem::take(&mut config.servers), pushed);

        if config.gateway.enabled && config.gateway.device_id.trim().is_empty() {
            config.gateway.device_id = discover_device_id().context(
                "cannot determine the thin-edge device id; set gateway.device_id explicitly",
            )?;
        }

        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        // With the gateway device on, servers may arrive later from Cumulocity, so starting with
        // none is a legitimate state — the gateway comes up, registers, and waits. Without it,
        // nothing would ever give this process a server to talk to.
        if self.servers.is_empty() && !self.gateway.enabled {
            bail!(
                "no OPC UA servers configured and gateway.enabled is false, so no server can ever \
                 arrive: add a [[servers]] entry, a file in {}/{SERVERS_SUBDIR}, or enable the \
                 gateway device",
                self.mappings.dir.display()
            );
        }
        if self.gateway.enabled {
            if self.gateway.id.trim().is_empty() {
                bail!("gateway.id must not be empty: it is half of the gateway's external id");
            }
            if self.gateway.device_id.trim().is_empty() {
                bail!("gateway.device_id must not be empty when the gateway device is enabled");
            }
            if self.gateway.server_poll_interval_secs == 0 {
                bail!("gateway.server_poll_interval_secs must be greater than zero");
            }
        }
        if self.runtime.worker_threads == 0 {
            bail!("runtime.worker_threads must be at least 1");
        }
        if self.subscription.channel_capacity == 0 {
            bail!("subscription.channel_capacity must be at least 1");
        }
        if self.subscription.publishing_interval_ms == 0 {
            bail!("subscription.publishing_interval_ms must be greater than zero");
        }
        if self.subscription.flush_interval_ms == 0 {
            bail!("subscription.flush_interval_ms must be greater than zero");
        }
        let mut seen: Vec<&str> = Vec::new();
        for server in &self.servers {
            if server.id.trim().is_empty() {
                bail!("every server needs a non-empty id");
            }
            if seen.contains(&server.id.as_str()) {
                bail!(
                    "two servers share the id {:?}; ids must be unique",
                    server.id
                );
            }
            seen.push(&server.id);
            if !server.url.starts_with("opc.tcp://") {
                bail!(
                    "server {} has url {:?}: only opc.tcp:// endpoints are supported",
                    server.id,
                    server.url
                );
            }
            if server.name.trim().is_empty() {
                bail!(
                    "server {} needs a name for its thin-edge child device",
                    server.id
                );
            }
            // A secured endpoint needs an application instance certificate, and this gateway
            // deliberately writes nothing to disk to create one. Fail loudly rather than
            // half-connect.
            if !server.security_policy.eq_ignore_ascii_case("None") {
                bail!(
                    "server {} requests security_policy {:?}, but only \"None\" is supported: \
                     secured endpoints need an application instance certificate, which this \
                     gateway does not yet accept from configuration",
                    server.id,
                    server.security_policy
                );
            }
        }
        if !self.proxy.enabled && !self.mappings.dir.exists() {
            bail!(
                "proxy.enabled is false and the mapping directory {} does not exist, so there is \
                 no mapping source at all",
                self.mappings.dir.display()
            );
        }
        Ok(())
    }

    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.runtime.connect_timeout_secs)
    }

    pub fn max_backoff(&self) -> Duration {
        Duration::from_secs(self.runtime.max_backoff_secs)
    }

    pub fn server_poll_interval(&self) -> Duration {
        Duration::from_secs(self.gateway.server_poll_interval_secs.max(1))
    }
}

/// Ask thin-edge for the device id.
///
/// Nothing on localhost reports it otherwise: the entity store does not carry the main device's
/// external id, and the proxy will not forward `/user/currentUser`. Section 5 of the agent
/// guidelines already says to read the proxy bind address from `tedge config` rather than
/// hard-coding it, and this gateway is packaged as a thin-edge service, so the CLI is present.
fn discover_device_id() -> anyhow::Result<String> {
    let output = std::process::Command::new("tedge")
        .args(["config", "get", "device.id"])
        .output()
        .context("cannot run `tedge config get device.id`")?;
    if !output.status.success() {
        bail!(
            "`tedge config get device.id` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if id.is_empty() {
        bail!("`tedge config get device.id` returned nothing");
    }
    Ok(id)
}

/// Read one TOML file per server from `dir`.
///
/// This is the same delivery path as pushed mapping files: `tedge-configuration-management`
/// writes here, so servers are remotely manageable as versioned config without a Cumulocity
/// server object and behind any thin-edge bridge. A directory that does not exist is not an
/// error — the whole mechanism is optional.
///
/// A malformed file *is* an error, unlike a malformed mapping file. A mapping we cannot read costs
/// one device type; a server we cannot read means data silently never arrives from it.
fn load_server_dir(dir: &Path) -> anyhow::Result<Vec<ServerConfig>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(anyhow::Error::new(error)
                .context(format!("cannot read server directory {}", dir.display())));
        }
    };

    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    // Deterministic order, so a duplicate id is always reported against the same file.
    paths.sort();

    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read server file {}", path.display()))?;
        let server: ServerConfig = toml::from_str(&text)
            .with_context(|| format!("cannot parse server file {}", path.display()))?;
        out.push(server);
    }
    Ok(out)
}

/// Merge inline `[[servers]]` with pushed server files, the pushed file winning on a shared id.
///
/// Same precedence as the two mapping sources: a file on the device is a deliberate local
/// override of whatever the built-in configuration says.
pub(crate) fn merge_servers(
    inline: Vec<ServerConfig>,
    pushed: Vec<ServerConfig>,
) -> Vec<ServerConfig> {
    let mut out = inline;
    for server in pushed {
        match out.iter_mut().find(|s| s.id == server.id) {
            Some(existing) => *existing = server,
            None => out.push(server),
        }
    }
    out
}

impl From<&MqttConfig> for tedge::MqttConfig {
    fn from(c: &MqttConfig) -> Self {
        Self {
            host: c.host.clone(),
            port: c.port,
            client_id: c.client_id.clone(),
            capacity: c.capacity,
            keep_alive: Duration::from_secs(c.keep_alive_secs),
            service_name: c.service_name.clone(),
        }
    }
}

impl From<&ProxyConfig> for tedge::ProxyConfig {
    fn from(c: &ProxyConfig) -> Self {
        Self {
            base_url: c.base_url.clone(),
            timeout: Duration::from_secs(c.timeout_secs),
            client_cert: c.client_cert.clone(),
            client_key: c.client_key.clone(),
            ca_cert: c.ca_cert.clone(),
        }
    }
}

impl ServerConfig {
    pub fn endpoint(&self) -> opcua_conn::ServerEndpoint {
        use opcua::types::MessageSecurityMode;
        let mode = if self.security_policy.eq_ignore_ascii_case("None") {
            MessageSecurityMode::None
        } else {
            MessageSecurityMode::SignAndEncrypt
        };

        opcua_conn::ServerEndpoint {
            id: self.id.clone(),
            url: self.url.clone(),
            security_policy: self.security_policy.clone(),
            message_security_mode: mode,
            user: match (&self.user, &self.password) {
                (Some(user), Some(password)) => Some(opcua_conn::Credentials {
                    user: user.clone(),
                    password: password.clone(),
                }),
                _ => None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A default configuration has the gateway device on, so no server yet is a valid state:
    /// servers arrive from Cumulocity. Turning it off removes every possible source.
    #[test]
    fn starting_without_servers_is_allowed_only_with_the_gateway_device() {
        let mut config = Config::default();
        config.gateway.device_id = "tedge001".into();
        config.validate().expect("no servers yet is fine");

        config.gateway.enabled = false;
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("no server can ever arrive"));
    }

    #[test]
    fn the_gateway_device_needs_a_device_id() {
        let err = Config::default().validate().expect_err("must fail");
        assert!(err.to_string().contains("gateway.device_id"));
    }

    #[test]
    fn external_ids_follow_thin_edges_own_shape() {
        let gateway = GatewayConfig {
            device_id: "tedge001".into(),
            id: "opcua-gateway".into(),
            ..GatewayConfig::default()
        };
        assert_eq!(gateway.external_id(), "tedge001:device:opcua-gateway");
        assert_eq!(
            gateway.server_external_id("3769051265"),
            "tedge001:device:opcua-gateway:device:3769051265"
        );
    }

    #[test]
    fn an_adopted_server_is_addressed_by_managed_object_id() {
        let mut s = server("3769051265");
        assert_eq!(s.topic_id(), "3769051265");
        assert!(!s.is_adopted());

        s.name = "Cooling line PLC".into();
        s.topic_id = Some("3769051265".into());
        s.external_id = Some("tedge001:device:opcua-gateway:device:3769051265".into());
        assert_eq!(s.topic_id(), "3769051265");
        assert!(s.is_adopted());
    }

    fn server(id: &str) -> ServerConfig {
        ServerConfig {
            id: id.into(),
            name: id.into(),
            url: "opc.tcp://localhost:4840".into(),
            security_policy: "None".into(),
            user: None,
            password: None,
            device_types: Vec::new(),
            external_id: None,
            topic_id: None,
        }
    }

    #[test]
    fn an_empty_device_type_list_accepts_everything() {
        let s = server("plc-1");
        assert!(s.accepts_device_type("4168968253"));
        assert!(s.accepts_device_type("anything"));
    }

    #[test]
    fn a_device_type_list_scopes_the_server() {
        let mut s = server("plc-1");
        s.device_types = vec!["4168968253".into(), " padded ".into()];
        assert!(s.accepts_device_type("4168968253"));
        assert!(s.accepts_device_type("padded"));
        assert!(!s.accepts_device_type("9999999999"));
    }

    #[test]
    fn pushed_server_files_override_inline_entries_by_id() {
        let mut pushed = server("plc-1");
        pushed.url = "opc.tcp://pushed:4840".into();
        let merged = merge_servers(vec![server("plc-1"), server("plc-2")], vec![pushed]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].url, "opc.tcp://pushed:4840");
        assert_eq!(merged[1].id, "plc-2");
    }

    #[test]
    fn pushed_server_files_add_servers_the_config_does_not_mention() {
        let merged = merge_servers(vec![server("plc-1")], vec![server("plc-9")]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[1].id, "plc-9");
    }

    #[test]
    fn a_missing_server_directory_is_not_an_error() {
        let servers = load_server_dir(Path::new("/nonexistent/opcua/servers")).expect("ok");
        assert!(servers.is_empty());
    }

    #[test]
    fn server_files_are_read_and_a_broken_one_is_fatal() {
        let dir = std::env::temp_dir().join(format!("opcua-gw-servers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("plc-1.toml"),
            "id = \"plc-1\"\nname = \"Cooling line PLC\"\nurl = \"opc.tcp://10.0.0.7:4840\"\n\
             device_types = [\"4168968253\"]\n",
        )
        .expect("write");
        std::fs::write(dir.join("notes.md"), "ignored").expect("write");

        let servers = load_server_dir(&dir).expect("reads");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].id, "plc-1");
        assert_eq!(servers[0].security_policy, "None");
        assert_eq!(servers[0].device_types, vec!["4168968253".to_owned()]);

        // A server we cannot parse means data silently never arrives, so it must not be skipped.
        std::fs::write(dir.join("broken.toml"), "id = ").expect("write");
        let err = load_server_dir(&dir).expect_err("must fail");
        std::fs::remove_dir_all(&dir).ok();
        assert!(err.to_string().contains("broken.toml"), "{err}");
    }

    /// A configuration that passes validation, to isolate what each test is actually asserting.
    fn valid_config() -> Config {
        let mut config = Config::default();
        config.gateway.device_id = "tedge001".into();
        config.servers.push(server("plc-1"));
        config.validate().expect("valid to begin with");
        config
    }

    #[test]
    fn duplicate_server_ids_are_rejected() {
        let mut config = valid_config();
        config.servers.push(server("plc-1"));
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("share the id"), "{err}");
    }

    #[test]
    fn rejects_non_opc_tcp_urls() {
        let mut config = valid_config();
        let mut bad = server("2");
        bad.url = "http://localhost:4840".into();
        config.servers.push(bad);
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("opc.tcp://"), "{err}");
    }
}
