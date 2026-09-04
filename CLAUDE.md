# AGENTS.md — c8y-opcua-gateway-rs

> Agent instructions for the `c8y-opcua-gateway-rs` repository.
> This file is the primary AI coding agent reference. It is symlinked as `CLAUDE.md`.
>
> Sibling repository: [`c8y-opcua`](../c8y-opcua) — the Java implementation. Read
> `../c8y-opcua/device-gateway/AGENTS.md` when you need the behaviour of the component being
> ported, but **do not** port its structure. The scope here is far narrower; see
> [Section 3](#3-explicit-non-goals).

---

## 1. What this repo is

An **OPC UA data source for [thin-edge.io](https://thin-edge.github.io/thin-edge.io/)**, written in
Rust, targeting a small runtime footprint on edge hardware.

It connects to one or more OPC UA servers, subscribes to (or cyclically reads) a configured set of
nodes, applies local mapping rules, and publishes the results to the **local thin-edge MQTT
broker** on `te/` topics. thin-edge's mapper and bridge carry the data to the cloud.

The gateway runs as a thin-edge **service** alongside `tedge-agent`, packaged as a systemd unit.

### It has no cloud integration of its own

No Cumulocity REST client, no device bootstrap, no credentials, no inventory, no operations
polling, no HTTP proxy. The only outbound socket besides OPC UA is MQTT to `localhost:1883`.
Everything cloud-facing is thin-edge's job.

A consequence worth stating plainly: because the gateway only speaks the `te/` topic scheme, it is
**cloud-agnostic**. thin-edge 2.0's configurable bridges route `te/` data to Cumulocity or to any
other MQTT cloud without a change here.

### Why Rust

The Java gateway runs on a JVM sized `-Xms128m -Xmx384m` and ships as an ~200 MB Alpine + JDK
image. Target here: a single static binary in the 10–20 MB range with a resident set in the tens
of megabytes under a few thousand monitored items. thin-edge.io is itself Rust under Apache-2.0,
so this fits its deployment model — musl static builds, deb/rpm packages, systemd units.

### Upstream OPC UA stack

[`async-opcua`](https://github.com/FreeOpcUa/async-opcua) (`async-opcua-client`), tokio-based,
**MPL-2.0**. This replaces the proprietary Prosys SDK.

Pin an exact version in the workspace `Cargo.toml`. `async-opcua` is pre-1.0, its minor releases
carry breaking changes, and it declares no MSRV and tracks latest stable Rust — so CI must pin the
toolchain via `rust-toolchain.toml`.

> **Open blocker:** MPL-2.0 is file-level copyleft and must clear third-party compliance before
> any code is written against it. Nothing about the thin-edge scoping changes this.

---

## 2. Architecture

```
crates/
├── gateway/       # binary `c8y-opcua-gateway`: config, wiring, supervision, command dispatch
├── opcua-conn/    # async-opcua session lifecycle, reconnect, subscriptions, cyclic read
├── mapping/       # mapping model loaded from local config, node resolution, value → payload
└── tedge/         # thin-edge MQTT client: entity registration, te/ publishing, commands, health
```

Four crates, no `c8y-client`, no `store`. Data flow:

```
OPC UA server ──subscription / cyclic read──▶ opcua-conn
                                                 │ DataValue + NodeId
                                                 ▼
                                              mapping  ──▶ measurement / event / alarm
                                                 │
                                                 ▼
                                              tedge ──▶ mosquitto :1883 on te/ topics
                                                            │
                                                            ▼
                                                   tedge-mapper ──▶ cloud
```

Commands travel the other way: `tedge` subscribes to `te/<id>/cmd/<type>/+`, `gateway` dispatches
to a handler, the handler calls into `opcua-conn`, and `tedge` publishes the status transitions.

### Key decisions

- **Tokio single multi-threaded runtime**, sized from config, low worker count by default. Do not
  spawn per-server runtimes.
- **No dependency-injection framework.** Wire components explicitly in `gateway::main`. The Java
  version's Spring `ApplicationEventPublisher` fan-out maps to `tokio::sync::broadcast` / `mpsc`
  channels — declare the channel topology in one place.
- **MQTT client is `rumqttc`** — the same client thin-edge itself uses. Publish with QoS 1.
- **Mapping definitions come from local configuration files**, not from cloud inventory. See
  [Section 4](#4-mapping-configuration).
- **No local persistence, no offline buffer.** The gateway publishes to a broker on localhost;
  mosquitto's persistent queue and the thin-edge bridge own store-and-forward. Do not reimplement
  it. Resolved NodeIds live in memory and are re-resolved on reconnect — one
  `TranslateBrowsePathsToNodeIds` call, not a cache to invalidate.
- **Batch locally anyway.** Publishing every data change as its own MQTT message will drown the
  local broker and the mapper on high-rate servers. Port the *intent* of the Java
  `BaseQueuedRepository` / `FlushExecutor`: bounded queues, flush on size or interval, and
  grouping of several series into one measurement message. Drop the retry/offline machinery,
  keep the bounding and the batching. On overflow, drop with a rate-limited warning — never grow
  unbounded.

---

## 3. Explicit non-goals

Two things are deliberately out of scope. Neither is a backlog item; do not add them, and do not
add anything that implies them.

### 3.1 No cloud integration

| Java gateway feature | Status here |
|---|---|
| Cumulocity REST client, `java-client` SDK | Not ported |
| Device bootstrap, device credentials, `platform/**`, `bootstrap/**` | Not ported — thin-edge owns device identity |
| Inventory managed objects, external IDs, `IdentityRepository` | Replaced by `te/` entity registration and `twin/` topics |
| Cumulocity operations polling, `OperationExecutor` | Replaced by thin-edge commands on `cmd/` topics |
| `BinariesRepository`, file upload operations | Not supported |
| Queued REST repositories, processing-mode interceptors, offline buffer | Not ported — batching stays, persistence does not |
| ThinEdge HTTP proxy path (`gateway.thinEdge.useHttpProxy`) | Not ported — MQTT only |
| JWT / `mqtt-jwt-lib` | Not ported — the local broker needs no auth |

### 3.2 No address space scanning

| Java gateway feature | Status here |
|---|---|
| `AddressSpaceScanner` / `AddressSpaceCleaner` / `addressspace/**` | Not ported |
| RocksDB address-space datastore and its inventory synchronisation | Not ported |
| `c8y_ScanAddressSpace` operation, UI address-space browsing | Not supported |
| `BaseDeviceTypeMatchingService` — matching device types by walking a scanned tree | Not ported |
| Regex browse paths, value-based `applyConstraints` | Not supported |
| `c8y_TestDeviceTypeMatching`, `c8y_FindMatchingDeviceTypes`, dry-run matching | Not supported |
| `BaseModelChangeEvent` subscription driving re-scans | Not ported |

**Node resolution replaces matching.** Each mapping is bound to a server and a root node by
configuration. `browse_path` entries are resolved against that root with
**`TranslateBrowsePathsToNodeIds`** — one request, no tree walk — and a mapping may instead carry
an absolute `node_id`, used directly.

`client-lib-prosys` disappears entirely: `async-opcua` *is* the client abstraction, so the vendor
isolation layer and its enforcer rule are gone.

---

## 4. Mapping configuration

With no inventory to read device types from, mappings are **local files**, loaded at startup and
on reload. TOML, one file per server or one directory of them, under `/etc/tedge/opcua/`.

Deliberately: these files are managed by **thin-edge configuration management**, which already
exposes them in the Cumulocity configuration repository UI for remote download and upload. That
is the replacement for editing device types in the OPC UA UI — do not build a config
distribution mechanism of our own.

Shape (illustrative, not final):

```toml
[[server]]
id = "plc-1"
url = "opc.tcp://192.168.1.50:4840"
security_policy = "Basic256Sha256"
security_mode = "SignAndEncrypt"
entity = "device/plc-1//"          # te/ identifier this server's data is published under

[[server.mapping]]
node_id = "ns=2;s=Machine.Temperature"
publish_interval_ms = 1000
[server.mapping.measurement]
type = "environment"
fragment = "temperature"
series = "T"
unit = "°C"

[[server.mapping]]
browse_path = ["Objects", "Machine", "Status"]
root_node_id = "i=85"
[server.mapping.alarm]
type = "machineFault"
severity = "major"
text = "Machine reported a fault"
```

Reject an unparseable or contradictory config at startup with a specific error naming the file and
the entry. A config that references a node the server does not expose is a runtime warning per
node, not a fatal error — one unreachable node must not stop the other 500.

---

## 5. thin-edge integration contract

Everything in this section is thin-edge's published MQTT API
([reference](https://thin-edge.github.io/thin-edge.io/references/mqtt-api/)). Target **thin-edge
2.x**; the legacy `tedge/` topics were removed in 2.0 and must not be used.

**Entity registration** — retained, published on startup before any data:

```
te/device/main/service/opcua-gateway   {"@type":"service","@parent":"device/main//","name":"opcua-gateway","type":"systemd"}
te/device/plc-1//                      {"@type":"child-device","@parent":"device/main//","name":"plc-1","type":"opcua-server"}
```

One child device per OPC UA server. The gateway itself is a service on the main device.

**Health** — retained, on startup and on shutdown (use an MQTT last will for the `down` case):

```
te/device/main/service/opcua-gateway/status/health   {"status":"up","pid":1234,"time":1674739912}
```

This replaces the Java gateway's JMX beans and Spring actuator health.

**Telemetry:**

| Kind | Topic | Payload |
|---|---|---|
| Measurement | `te/device/<id>///m/<type>` | `{"time":"…","<fragment>":{"<series>":23.4}}` |
| Event | `te/device/<id>///e/<type>` | `{"time":"…","text":"…"}` |
| Alarm raise | `te/device/<id>///a/<type>` | `{"time":"…","text":"…","severity":"major"}` |
| Alarm clear | `te/device/<id>///a/<type>` | empty payload, **retained** |
| Static metadata | `te/device/<id>///twin/<name>` | retained JSON |
| Measurement units | `te/device/<id>///m/<type>/meta` | retained `{"<series>":{"unit":"°C"}}` |

The empty-retained-message alarm clear matches `ThinEdgeAlarmCreationTask` in the Java gateway —
keep that behaviour. Prefer `twin/` topics over ad-hoc fragments for anything static about a
server or node.

**Commands** — the replacement for Cumulocity operations. Declare capability with a retained
message at startup, then subscribe to the instance topic:

```
te/device/plc-1///cmd/opcua_read           {"description":"Read an OPC UA node value"}   # retained capability
te/device/plc-1///cmd/opcua_read/<cmd-id>  {"status":"init","node_id":"ns=2;s=Temp"}     # request
```

The gateway is the **executor**: it transitions `init` → `executing` → `successful` | `failed`,
republishing the full payload retained at each step, and adding its result fields on success. It
never clears the command topic — the requester does that.

Port only these command types initially: `opcua_read`, `opcua_write`, `opcua_call_method`. The
historic-data, file-upload and address-space operations from the Java gateway are out of scope.

---

## 6. Porting map

Paths are relative to `../c8y-opcua/device-gateway/src/main/java/com/cumulocity/opcua/client/gateway/`.

| Concern | Java source | Rust home |
|---|---|---|
| OPC UA connect, reconnect, keystore | `connection/ConnectionManager.java`, `connection/security/` | `opcua-conn` |
| Subscription creation and refresh | `subscription/**` | `opcua-conn` + `mapping` |
| UA event subscriptions, `EventFilter` select/where clauses | `../client-lib-prosys/.../OpcuaEventItemBuilder.java`, `subscription/UaSubscriptionService.java` | `opcua-conn` |
| Cyclic read | `cyclicreader/**` | `opcua-conn` |
| Mapping actions (measurement/event/alarm) | `../common-services/.../common/model/mapping/action/**` | `mapping` |
| **thin-edge payload construction** | `mappingsexecution/tasks/ThinEdge*Task.java` | `tedge` — **read these first, they are the closest thing to a spec** |
| Value maps | `valuemap/**`, `../common-services/.../common/valuemap/` | `mapping` |
| Alarm status mapping (SpEL) | `../common-services/.../common/expression/` | `mapping` — see below |
| Bad-quality / limit handling | `ValueLimitsValidator`, `isDataValueBad()` in `BaseTask` | `mapping` |
| Everything else in the table in Section 3 | — | not ported |

The `ThinEdge*Task` classes in `mappingsexecution/tasks/` already do exactly what this gateway
does, in Java, against the same topics. They are the reference implementation for the `tedge`
crate — including the details worth not rediscovering: bad-quality values are dropped silently,
a timestamp falls back to now when the server does not supply one, and event text supports
`${value}` substitution.

### Things with no Rust equivalent — decide deliberately

- **SpEL.** `BooleanExpression` / `ServerAlarmStatusMapper` evaluate Spring Expression Language
  from `alarmStatusMappings`. Usage is narrow — alarm status mapping only. Implement a small
  restricted comparison/boolean evaluator; do not embed a scripting engine. Unparseable
  expressions are reported at config load, not skipped at runtime.
- **No reusable thin-edge crates.** `tedge-api`, `tedge-actors` and friends are workspace-internal
  and not published to crates.io. Talk to the broker with `rumqttc` like any other client; do not
  add a git dependency on the thin-edge workspace.
- **Consider pushing work into `tedge flows` instead of code.** thin-edge 2.0 added a sandboxed
  JavaScript runtime in the mapper for remapping, filtering and aggregation. If a transformation
  is cloud-shaped rather than OPC-UA-shaped, it probably belongs in a flow, not here.

---

## 7. Coding conventions

- **Edition 2024**, `[workspace.dependencies]` and `[workspace.lints]`. `#![forbid(unsafe_code)]`
  in every crate.
- **Errors:** `thiserror` in library crates, `anyhow` only in the `gateway` binary. No
  `unwrap()`/`expect()` outside tests and startup; a panic must not be how a connection failure
  is handled.
- **Async:** `tokio`. Every long-lived task owns a `CancellationToken` and shuts down cleanly.
  Bound every channel. Prefer supervised `tokio::select!` loops over detached tasks.
- **Logging:** `tracing`, structured fields (`server_id`, `node_id`, `topic`), never interpolation
  into the message. `tracing-subscriber` in the binary only. Log to stdout — journald captures it.
- **Config:** file, then environment (`OPCUA_GW__` prefix), then CLI flags. Deserialize into typed
  structs with `serde`, validate once at startup, pass the validated struct down. No globals.
- **Naming:** keep OPC UA and thin-edge vocabulary intact (`NodeId`, `MonitoredItem`, `Entity`,
  `Channel`, `Command`) so both upstreams stay greppable against this code.
- **No inline comments** describing what the code does. Comment a non-obvious *why* only — a spec
  constraint, a server quirk, a workaround.
- **No over-engineering.** Three similar lines beat a premature trait. Add a trait when the second
  implementation actually lands.
- **Footprint is a requirement.** Check what a new dependency drags in; reject anything
  duplicating a tree already present. Track binary size and RSS in CI.

## 8. Testing

- Unit tests alongside the code, integration tests in each crate's `tests/`.
- **OPC UA under test:** stand up an in-process server with `async-opcua-server` at the same
  workspace version. No external Milo instance in the default run.
- **thin-edge under test:** an embedded MQTT broker (or `rumqttc` against a test broker) and
  assertions on published topics and payloads. No test in `cargo test` may require a running
  `tedge-agent` or a cloud tenant.
- End-to-end tests against a real thin-edge install live behind `--features e2e`.

## 9. Build and packaging

```bash
cargo build --release            # target/release/c8y-opcua-gateway
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Targets: `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` primarily — match
thin-edge's own target set. Ship **deb and rpm packages with a systemd unit**, ordered after
`mosquitto.service`. A container image is secondary; if one is built it is `FROM scratch` or
`distroless/static`.

## 10. PR and branch conventions

Inherited from `c8y-opcua`:

- **Branch:** `<type>/<JIRA-ID>/<short-description>` — e.g. `feat/DM-1234/subscription-refresh`
- **PR title:** `<semantic-tag>: [DM-<id>] [<ChangeType>] <description>`
  - tags: `feat`, `fix`, `refactor`, `perf`, `ci`, `docs`, `test`, `build`, `style`
  - change types: `[Feature]`, `[Fix]`, `[Improvement]`, `[Internal]`, `[API change]`, `[Preview]`
- **Squash-merge only.**
- Do not modify or transition non-DM Jira tickets (L2S, CST, MTM …) — read-only context.
- You are the author of all code you produce. Disclose non-trivial AI tooling use in the PR
  description.

## 11. Don't

- **Don't add a cloud client.** No HTTP to Cumulocity, no credentials, no REST. If something
  seems to need it, it belongs in thin-edge or in a flow. See [Section 3.1](#31-no-cloud-integration).
- **Don't add address space scanning**, browsing-for-discovery, or an address-space cache. See
  [Section 3.2](#32-no-address-space-scanning).
- **Don't use the legacy `tedge/` topics.** Removed in thin-edge 2.0.
- **Don't reimplement store-and-forward.** mosquitto and the bridge own it.
- **Don't reach for RocksDB, a DI container, or a Spring-shaped event bus** because the Java
  version had one.
- **Don't add a dependency without checking its transitive tree and licence.** `async-opcua` is
  MPL-2.0; anything copyleft beyond that needs clearance before it lands.
- **Don't unwrap in library code**, and don't let a task die silently — supervise or propagate.
- **Don't block the async runtime.** Certificate parsing, file I/O and CPU-bound mapping work go
  through `spawn_blocking` or a dedicated pool.
- **Don't force-push to `develop` or `release/*`.** Never `--force` / `--force-with-lease` on
  shared branches. Never `--no-verify`.
