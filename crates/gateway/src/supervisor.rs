//! One server task per configured server, started and stopped as the server set changes.
//!
//! Servers used to be read once and spawned once, which was fine while every server came from a
//! file read at startup. A server created in the user interface arrives while the process is
//! running, so the set is now dynamic and something has to own the difference.
//!
//! Removal is not the same as shutdown. A server that disappeared from Cumulocity should take its
//! thin-edge entities with it, or a retained registration would outlive the object and the mapper
//! would recreate it on the next restart. A server whose process is stopping should leave its
//! retained state exactly where it is. The two cases are separate cancellation tokens.

use std::collections::HashMap;
use std::sync::Arc;

use tedge::TedgeMqtt;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::cloud_servers::CloudServers;
use crate::config::{Config, ServerConfig, merge_servers};
use crate::device_types::DeviceTypes;
use crate::server_task;

struct Running {
    server: ServerConfig,
    /// Ends the task and leaves its retained registrations in place. A child of the process-wide
    /// token, so shutdown reaches it without the supervisor doing anything.
    stop: CancellationToken,
    /// Ends the task and clears its retained registrations, for a server that is gone.
    remove: CancellationToken,
    handle: JoinHandle<()>,
}

/// Keep the running server tasks equal to the configured set until cancelled.
pub async fn run(
    config: Arc<Config>,
    mqtt: TedgeMqtt,
    device_types: watch::Receiver<DeviceTypes>,
    mut cloud: watch::Receiver<CloudServers>,
    cancel: CancellationToken,
) {
    let mut running: HashMap<String, Running> = HashMap::new();

    loop {
        // Servers configured on the device win over servers registered in Cumulocity, the same way
        // a pushed mapping file wins over a pulled device type: the operator standing next to the
        // hardware gets the last word.
        let desired = merge_servers(
            cloud.borrow_and_update().as_ref().clone(),
            config.servers.clone(),
        );
        reconcile(
            &config,
            &mqtt,
            &device_types,
            &cancel,
            &mut running,
            desired,
        )
        .await;

        tokio::select! {
            () = cancel.cancelled() => break,
            changed = cloud.changed() => {
                if changed.is_err() {
                    // The provider is gone; keep what is running and wait for shutdown.
                    cancel.cancelled().await;
                    break;
                }
            }
        }
    }

    for (id, task) in running {
        join(&id, task.handle).await;
    }
}

async fn join(server_id: &str, handle: JoinHandle<()>) {
    if let Err(error) = handle.await {
        warn!(server_id, %error, "a server task did not stop cleanly");
    }
}

/// Start what is missing, stop what is gone or changed.
async fn reconcile(
    config: &Arc<Config>,
    mqtt: &TedgeMqtt,
    device_types: &watch::Receiver<DeviceTypes>,
    cancel: &CancellationToken,
    running: &mut HashMap<String, Running>,
    desired: Vec<ServerConfig>,
) {
    let known_server_ids: Vec<String> = desired.iter().map(|s| s.id.clone()).collect();

    // Gone from the desired set: the managed object was deleted, or the file was removed. Its
    // thin-edge registrations go too.
    let removed: Vec<String> = running
        .keys()
        .filter(|id| !desired.iter().any(|server| &&server.id == id))
        .cloned()
        .collect();
    // Still wanted, but with different settings. Restarted in place, keeping its registrations.
    let restarted: Vec<String> = desired
        .iter()
        .filter(|server| {
            running
                .get(&server.id)
                .is_some_and(|task| &task.server != *server)
        })
        .map(|server| server.id.clone())
        .collect();

    for id in removed {
        if let Some(task) = running.remove(&id) {
            info!(
                server_id = id,
                "server is gone; stopping it and clearing its registrations"
            );
            task.remove.cancel();
            join(&id, task.handle).await;
        }
    }
    for id in restarted {
        if let Some(task) = running.remove(&id) {
            info!(
                server_id = id,
                "server configuration changed; restarting it"
            );
            task.stop.cancel();
            join(&id, task.handle).await;
        }
    }

    for server in desired {
        if running.contains_key(&server.id) {
            continue;
        }
        info!(
            server_id = server.id,
            url = server.url,
            adopted = server.is_adopted(),
            "starting a server task"
        );
        let stop = cancel.child_token();
        let remove = CancellationToken::new();
        let handle = tokio::spawn(server_task::run(
            Arc::clone(config),
            server.clone(),
            known_server_ids.clone(),
            mqtt.clone(),
            device_types.clone(),
            stop.clone(),
            remove.clone(),
        ));
        running.insert(
            server.id.clone(),
            Running {
                server,
                stop,
                remove,
                handle,
            },
        );
    }
}
