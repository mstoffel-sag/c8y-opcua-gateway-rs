# c8y-opcua-gateway-rs

An OPC UA data source for [thin-edge.io](https://thin-edge.github.io/thin-edge.io/), written in
Rust for a small footprint on edge hardware.

It connects to OPC UA servers, subscribes to or cyclically reads configured nodes, applies local
mapping rules, and publishes to the local thin-edge MQTT broker on `te/` topics. thin-edge's
mapper and bridge carry the data to the cloud.

**No cloud integration of its own.** No Cumulocity REST client, no credentials, no inventory, no
operations polling — thin-edge owns all of it. The only outbound socket besides OPC UA is MQTT to
`localhost:1883`. Publishing only `te/` topics also makes the gateway cloud-agnostic: thin-edge
2.0 configurable bridges can route the data anywhere.

**No address space scanning.** Nodes are addressed by NodeId, or by browse path resolved against a
configured root via `TranslateBrowsePathsToNodeIds`. Mappings are local configuration files, not
cloud-managed device types.

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
