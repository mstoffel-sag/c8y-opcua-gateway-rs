# c8y-opcua-gateway-rs

An OPC UA data source for [thin-edge.io](https://thin-edge.github.io/thin-edge.io/), written in
Rust for a small footprint on edge hardware.

It connects to OPC UA servers, subscribes to or cyclically reads configured nodes, applies local
mapping rules, and publishes to the local thin-edge MQTT broker on `te/` topics. thin-edge's
mapper and bridge carry the data to the cloud.

**No credentials of its own.** Data goes out over MQTT to `127.0.0.1:1883`; device types come in
over read-only HTTP to the thin-edge Cumulocity proxy at `127.0.0.1:8001/c8y/...`, which injects
the device's JWT. No bootstrap, no Cumulocity SDK, no operations polling, no inventory writes.

**Two ways to get mappings, both first-class.** Pull `c8y_OpcuaDeviceType` managed objects through
the proxy, so device types authored in the OPC UA UI work as-is; or push mapping files to the
device with thin-edge configuration management, which needs no cloud connectivity at boot and
works behind any thin-edge bridge.

**Stateless.** The gateway writes nothing to disk — no cache, no buffer, no database. Readings go
straight to MQTT and mosquitto owns store-and-forward.

**No address space scanning.** Nodes are addressed by NodeId, or by browse path resolved against a
device type's `referencedRootNodeId` via `TranslateBrowsePathsToNodeIds`. Only regex browse paths
and `browsePathMatchesRegex` are incompatible.

This is not a replacement for the Java `opcua-device-gateway`.

## Status

First working version: it connects, resolves device types to NodeIds, cyclically reads, and
publishes measurements, events and alarms to `te/` topics.

Implemented:

- OPC UA connect and reconnect against `SecurityPolicy::None` endpoints, anonymous or user/password
- Both mapping sources — pull through the thin-edge Cumulocity proxy, push from a mapping directory
- Servers configured on the device, with no `c8y_OpcuaServer` object: inline `[[servers]]` or one
  TOML file per server in `<mappings.dir>/servers/`, so thin-edge configuration management can
  deliver them as versioned config
- Node resolution by `referencedNodeId` or by browse path via `TranslateBrowsePathsToNodeIds`
- Both delivery modes: OPC UA subscriptions with monitored items, and cyclic read — chosen per
  entry by the device type's `subscriptionType` and `overriddenSubscriptions`
- One subscription per server shared across device types, with per-item sampling interval, queue
  size, `dataChangeTrigger`, deadband and index range
- Measurements batched into one message per measurement type, flushed on size or interval
- Measurements, events and alarms, plus entity registration, health with a last will, and
  retained measurement units
- A three-level entity hierarchy — the gateway is a service on the main device, each OPC UA server
  is a child device, and each device type applied to a server is a device below that server
- Scan-free `applyConstraints`: `matchesServerIds` and `matchesNodeIds`, plus a per-server
  `device_types` allow-list for scoping when Cumulocity is not the server registry

Not implemented yet:

- thin-edge commands (`opcua_read`, `opcua_write`, `opcua_call_method`)
- `serverObjectHasFragment` and `serverHasNodeWithValues` constraints
- Value maps, `alarmStatusMappings`, UA event mappings
- Secured OPC UA endpoints (they need an application instance certificate)

Still blocked on MPL-2.0 (`async-opcua`) third-party compliance clearance.

- [Rewrite evaluation](docs/rewrite-evaluation.md) — scope, footprint, risks, effort
- [AGENTS.md](AGENTS.md) / [CLAUDE.md](CLAUDE.md) — architecture, thin-edge contract, guidelines

## Build and run

```bash
cargo build --release            # target/release/c8y-opcua-gateway
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

c8y-opcua-gateway --config /etc/tedge/opcua/gateway.toml
c8y-opcua-gateway --config … --check      # validate configuration and exit
```

See [config/gateway.toml](config/gateway.toml) for the annotated configuration. Values may be
overridden with `OPCUA_GW__<SECTION>__<KEY>` (nested keys use `__`), then by CLI flags.

### Configuring servers

A server needs nothing in Cumulocity. `[[servers]].id` is a free-form local identifier, not a
managed object id — it is only what `applyConstraints.matchesServerIds` is compared against.

Servers can equally be delivered as one TOML file per server under `<mappings.dir>/servers/`,
which is the same path thin-edge configuration management writes to. That makes them remotely
manageable as versioned config, behind any bridge, with nothing written to Cumulocity inventory. A
pushed file overrides an inline `[[servers]]` entry with the same `id`. Changes apply on restart;
mapping files, by contrast, reload on the device-type poll.

Because a device type authored in the OPC UA UI can only put Cumulocity managed object ids in
`matchesServerIds`, such a constraint can never name a local server id. A `matchesServerIds` that
names no configured server is therefore reported and ignored rather than scoping the device type
out of existence, and scoping is instead expressed locally with the server's own `device_types`
list.

### Against the thin-edge demo container

[dev/local.toml](dev/local.toml) points at the
[thin-edge demo](https://github.com/thin-edge/tedge-demo-container) on `localhost:1883` and an OPC
UA server on `localhost:4840`:

```bash
cargo run -- --config dev/local.toml
mosquitto_sub -h 127.0.0.1 -p 1883 -v -t 'te/device/opcua-pump01/#'
```

`dev/local.toml` enables both mapping sources. Remove `dev/mappings/pump01.json` to exercise the
pull source alone — pushed files win on conflict, so with it in place a device type is served from
disk even when the same one is in inventory.

The demo needs three things for a host-side gateway to reach it:

- `FEATURES=nopki`, so the local endpoints are plain HTTP and MQTT rather than mTLS. With the
  `pki` feature the proxy demands a client certificate that only exists inside the container;
  configure `proxy.client_cert`, `proxy.client_key` and `proxy.ca_cert` from `tedge config get
  c8y.proxy.cert_path` and friends if you want to keep it.
- Ports 1883 and 8001 published to the host. Upstream's compose file does not publish them; a
  `docker-compose.override.yaml` next to it is the least invasive way to add them.
- `tedge config set c8y.proxy.bind.address 0.0.0.0`, since the proxy otherwise binds to the
  container's loopback only. (The `pki` bootstrap sets this itself; the `nopki` path does not.)

Note that `FEATURES=nopki` only works on a volume that is *already* provisioned or provisioned by
other means: in the demo image `step-ca-init.sh` is the only first-run script, so a fresh
`main_etc` volume with `nopki` comes up with no device certificate and no cloud connection.

## Built on

- [`async-opcua`](https://github.com/FreeOpcUa/async-opcua) — OPC UA client (MPL-2.0)
- [thin-edge.io](https://github.com/thin-edge/thin-edge.io) 2.x — edge framework (Apache-2.0)
- [`rumqttc`](https://crates.io/crates/rumqttc) — MQTT client, as used by thin-edge

## Related

- [`c8y-opcua`](../c8y-opcua) — the Java implementation (`device-gateway`, `management-service`)
