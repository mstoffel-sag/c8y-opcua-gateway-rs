# c8y-opcua-gateway-rs

A Rust implementation of the Cumulocity IoT OPC UA device gateway, built on
[`async-opcua`](https://github.com/FreeOpcUa/async-opcua), targeting a small runtime footprint for
edge and constrained deployments.

It connects to OPC UA servers, subscribes to or cyclically reads configured nodes, applies
Cumulocity device-type mappings, and forwards measurements, events and alarms to the platform.

**Address space scanning is deliberately not supported.** Nodes are addressed by NodeId, or by
browse path resolved against a configured root node via `TranslateBrowsePathsToNodeIds`. This is
not a drop-in replacement for the Java `opcua-device-gateway`.

## Status

Design stage. No implementation yet.

- [Rewrite evaluation](docs/rewrite-evaluation.md) — scope, footprint targets, risks, effort
- [AGENTS.md](AGENTS.md) / [CLAUDE.md](CLAUDE.md) — architecture and contributor/agent guidelines

## Related

- [`c8y-opcua`](../c8y-opcua) — the Java implementation (`device-gateway`, `management-service`)
