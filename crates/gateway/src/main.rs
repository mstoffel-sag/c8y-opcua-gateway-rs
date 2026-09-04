//! `c8y-opcua-gateway` — an OPC UA data source for thin-edge.io.
//!
//! The channel topology is declared once, here: one `watch` channel carries the current device
//! types to every server task, and each server task owns its own OPC UA session. Nothing else
//! talks to anything else.
#![forbid(unsafe_code)]

mod cloud_servers;
mod config;
mod device_types;
mod gateway_device;
mod operations;
mod publish;
mod server_task;
mod supervisor;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tedge::{C8yProxy, TedgeMqtt, mqtt};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use config::Config;

/// Default log filter.
///
/// `async-opcua` always builds a certificate store and reports at ERROR that encrypted endpoints
/// will not work when it finds no application instance certificate. This gateway has none by
/// design — it writes nothing to disk and configuration only accepts `SecurityPolicy::None`
/// endpoints — so the targets that only ever talk about encrypted endpoints are silenced, and the
/// same fact is stated once at startup instead.
const DEFAULT_LOG_FILTER: &str = "info,opcua=warn,rumqttc=warn,\
     opcua_crypto::certificate_store=off,opcua_core::comms::secure_channel=off,\
     opcua_client::config=off,opcua_client::session::client=off";

/// Delay between MQTT reconnect attempts; rumqttc does the reconnecting, this only paces it.
const MQTT_RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

/// Bound on undelivered incoming messages. Operations arrive at human pace; a full channel means
/// something is wrong, and dropping with a warning beats growing without limit.
const INCOMING_CAPACITY: usize = 64;

#[derive(Debug, Parser)]
#[command(name = "c8y-opcua-gateway", version, about)]
struct Cli {
    /// Configuration file. Values may be overridden with `OPCUA_GW__<SECTION>__<KEY>`.
    #[arg(short, long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Log filter, e.g. `info`, `debug`, `c8y_opcua_gateway=debug,opcua=warn`.
    #[arg(long, value_name = "FILTER")]
    log: Option<String>,

    /// Validate the configuration and exit.
    #[arg(long)]
    check: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            cli.log
                .clone()
                .or_else(|| std::env::var("OPCUA_GW_LOG").ok())
                .map(EnvFilter::new)
                .unwrap_or_else(|| EnvFilter::new(DEFAULT_LOG_FILTER)),
        )
        .with_target(true)
        .init();

    let config = Arc::new(Config::load(cli.config.as_deref())?);
    if cli.check {
        info!(servers = config.servers.len(), "configuration is valid");
        return Ok(());
    }

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.runtime.worker_threads)
        .enable_all()
        .thread_name("opcua-gw")
        .build()
        .context("cannot start the tokio runtime")?
        .block_on(run(config))
}

async fn run(config: Arc<Config>) -> anyhow::Result<()> {
    info!(
        version = env!("CARGO_PKG_VERSION"),
        servers = config.servers.len(),
        mapping_dir = %config.mappings.dir.display(),
        proxy = config.proxy.enabled,
        gateway_device = config.gateway.enabled,
        "starting c8y-opcua-gateway"
    );
    info!(
        "OPC UA endpoints are contacted with SecurityPolicy::None; this build carries no application instance certificate"
    );

    let cancel = CancellationToken::new();
    let mut tasks = JoinSet::new();

    let (tedge_mqtt, event_loop) = TedgeMqtt::new(&(&config.mqtt).into());
    let (incoming_tx, incoming_rx) = mpsc::channel(INCOMING_CAPACITY);
    let subscriptions = if config.gateway.enabled {
        vec![mqtt::C8Y_OPERATION_TOPIC.to_owned()]
    } else {
        Vec::new()
    };
    tasks.spawn(mqtt::run_event_loop(
        event_loop,
        tedge_mqtt.clone(),
        subscriptions,
        Some(incoming_tx),
        MQTT_RECONNECT_DELAY,
        cancel.clone(),
    ));

    // Registration and health go out before any data, as the thin-edge API requires.
    tedge_mqtt
        .register_service()
        .await
        .context("cannot register the gateway service with thin-edge")?;
    tedge_mqtt
        .publish_health_up()
        .await
        .context("cannot publish gateway health")?;
    if config.gateway.enabled {
        gateway_device::register(&config.gateway, &tedge_mqtt).await;
    }

    let proxy = if config.proxy.enabled {
        match C8yProxy::new(&(&config.proxy).into()) {
            Ok(proxy) => {
                info!(base_url = config.proxy.base_url, "device type pull enabled");
                Some(proxy)
            }
            Err(error) => {
                // A missing or misconfigured proxy is never fatal: pushed mapping files may well
                // be the only source this deployment needs.
                warn!(%error, "cannot use the thin-edge Cumulocity proxy; running without it");
                None
            }
        }
    } else {
        None
    };

    let (dt_tx, dt_rx) = watch::channel(device_types::DeviceTypes::default());
    tasks.spawn(device_types::run(
        Arc::clone(&config),
        proxy.clone(),
        dt_tx,
        cancel.clone(),
    ));

    // Servers registered in Cumulocity arrive while the process runs, so the server set is a
    // channel rather than a startup constant. Without the gateway device, or without the proxy to
    // read it through, this stays empty and only configured servers run.
    let (cloud_tx, cloud_rx) = watch::channel(cloud_servers::CloudServers::default());
    match (config.gateway.enabled, proxy) {
        (true, Some(proxy)) => {
            tasks.spawn(cloud_servers::run(
                Arc::clone(&config),
                proxy,
                cloud_tx,
                cancel.clone(),
            ));
            tasks.spawn(operations::run(
                Arc::clone(&config),
                tedge_mqtt.clone(),
                incoming_rx,
                cloud_rx.clone(),
                cancel.clone(),
            ));
        }
        (true, None) => warn!(
            "the gateway device is enabled but the Cumulocity proxy is not usable, so servers \
             cannot be read from Cumulocity; only configured servers will run"
        ),
        (false, _) => {}
    }

    tasks.spawn(supervisor::run(
        Arc::clone(&config),
        tedge_mqtt.clone(),
        dt_rx,
        cloud_rx,
        cancel.clone(),
    ));

    shutdown_signal().await;
    info!("shutting down");
    cancel.cancel();

    // Give the tasks a moment to close their sessions cleanly, then stop regardless.
    if tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        warn!("some tasks did not stop in time");
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = term.recv() => {}
                }
            }
            Err(error) => {
                error!(%error, "cannot listen for SIGTERM; only Ctrl-C will stop the gateway");
                let _ = ctrl_c.await;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}
