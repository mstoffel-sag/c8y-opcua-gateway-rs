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

Design stage. No implementation yet. Blocked on MPL-2.0 (`async-opcua`) third-party compliance
clearance.

- [Rewrite evaluation](docs/rewrite-evaluation.md) — scope, footprint, risks, effort
- [AGENTS.md](AGENTS.md) / [CLAUDE.md](CLAUDE.md) — architecture, thin-edge contract, guidelines

## Built on

- [`async-opcua`](https://github.com/FreeOpcUa/async-opcua) — OPC UA client (MPL-2.0)
- [thin-edge.io](https://github.com/thin-edge/thin-edge.io) 2.x — edge framework (Apache-2.0)
- [`rumqttc`](https://crates.io/crates/rumqttc) — MQTT client, as used by thin-edge

## Related

- [`c8y-opcua`](../c8y-opcua) — the Java implementation (`device-gateway`, `management-service`)
