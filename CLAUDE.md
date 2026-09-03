# AGENTS.md — c8y-opcua-gateway-rs

> Agent instructions for the `c8y-opcua-gateway-rs` repository.
> This file is the primary AI coding agent reference. It is symlinked as `CLAUDE.md`.
>
> Sibling repository: [`c8y-opcua`](../c8y-opcua) — the Java implementation this project
> replaces for the gateway component. Read `../c8y-opcua/device-gateway/AGENTS.md` when you need
> the behaviour of the component being ported, but **do not** port its structure verbatim.

---

## 1. What this repo is

A **Rust rewrite of the Cumulocity IoT OPC UA device gateway**, targeting a small runtime
footprint for edge and constrained deployments.

It connects to one or more OPC UA servers, subscribes to (or cyclically reads) a configured set of
nodes, applies Cumulocity device-type mappings, and forwards measurements, events and alarms to
the Cumulocity IoT platform. It registers itself as a gateway device and executes a subset of
Cumulocity operations.

It is **not** a drop-in replacement for `opcua-device-gateway`. See [Section 3](#3-explicit-non-goals).

### Why Rust

The Java gateway runs on a JVM sized `-Xms128m -Xmx384m` and ships as an ~200 MB Alpine + JDK
image. Target for this implementation: a single static binary in the 10–20 MB range with a
resident set in the tens of megabytes under a few thousand monitored items.

### Upstream OPC UA stack

[`async-opcua`](https://github.com/FreeOpcUa/async-opcua) (`async-opcua-client`), a tokio-based
OPC UA implementation, **MPL-2.0**. This replaces the proprietary Prosys SDK used by the Java
gateway.

Pin an exact version in the workspace `Cargo.toml`. `async-opcua` is pre-1.0 and its minor
releases carry breaking changes; it also declares no MSRV and tracks latest stable Rust, so CI
must pin the toolchain via `rust-toolchain.toml`.

---

## 2. Architecture

```
crates/
├── gateway/       # binary `c8y-opcua-gateway`: wiring, config, supervision, main loop
├── opcua-conn/    # async-opcua session lifecycle, reconnect, subscriptions, cyclic read
├── mapping/       # DeviceType model, node resolution, mapping execution → C8Y payloads
├── c8y-client/    # Cumulocity REST + MQTT client (bootstrap, inventory, data, operations)
└── store/         # local persistence: credentials, subscription state, offline buffer
```

Data flow:

```
OPC UA server ──subscription/cyclic read──▶ opcua-conn
                                              │ DataValue + NodeId
                                              ▼
                                           mapping  ──▶ measurement / event / alarm
                                              │
                                              ▼
                                          c8y-client ──▶ Cumulocity (MQTT preferred, REST fallback)
```

Cumulocity → gateway direction: operations are polled over REST (or received over MQTT) by
`gateway` and dispatched to handlers that call into `opcua-conn`.

### Key decisions

- **Tokio single multi-threaded runtime**, sized from config, defaulting to a low worker count.
  Do not spawn per-server runtimes.
- **No dependency-injection framework.** Wire components explicitly in `gateway::main`. The Java
  version's Spring `ApplicationEventPublisher` fan-out maps to `tokio::sync::broadcast` /
  `mpsc` channels — declare the channel topology in one place rather than letting listeners
  register themselves.
- **Device types are read from Cumulocity inventory**, exactly as the Java gateway does:
  managed objects of type `c8y_OpcuaDeviceType` carrying a `c8y_OpcuaDeviceType` fragment,
  polled for changes with an inventory `$filter` on `lastUpdated.date`. The management
  microservice is *not* a runtime dependency of the gateway.
- **Data path over MQTT** (Cumulocity SmartREST 2.0 / JSON-over-MQTT) wherever possible;
  REST is used for inventory, device types, binaries and operations. This is both the smaller
  footprint and the smaller bandwidth path.
- **Batching is mandatory, not optional.** Port the Java `BaseQueuedRepository` / `FlushExecutor`
  behaviour: bounded queues per data kind, flush on size or interval, backpressure on overflow.
  This is what keeps the platform from being overrun and must not be dropped.
- **Persistence is minimal.** With address-space caching gone, only device credentials, resolved
  subscription state and the offline buffer need to survive a restart. Use `redb`; do not pull in
  RocksDB.

---

## 3. Explicit non-goals

**Address space scanning is out of scope.** This is a deliberate product decision, not a backlog
item. Do not add it, and do not add anything that implies a cached copy of a server's address
space.

Consequences you must respect when porting:

| Java gateway feature | Status here |
|---|---|
| `AddressSpaceScanner` / `AddressSpaceCleaner` / `addressspace/**` | Not ported |
| RocksDB address-space datastore and its inventory synchronisation | Not ported |
| `c8y_ScanAddressSpace` operation | Not supported — reject with a clear message |
| Address-space browsing in the management UI | Not supported by this gateway |
| `BaseDeviceTypeMatchingService` and friends — matching device types by walking a scanned tree | Not ported |
| Regex browse paths (`RegexDeviceTypeMatchingService`, `RegexBrowsePathMatcher`) | Not supported |
| `c8y_TestDeviceTypeMatching`, `c8y_FindMatchingDeviceTypes`, dry-run matching operations | Not supported |
| `BaseModelChangeEvent` subscription driving re-scans | Not ported |

**Node resolution replaces matching.** A device type is bound to a server and a root node by
explicit configuration. Its mapping entries' `browsePath` values are resolved against that root
with the OPC UA **`TranslateBrowsePathsToNodeIds`** service — one request, no tree walk — and
mapping entries may also carry an absolute `referencedNodeId` that is used directly. Resolved
NodeIds are cached in `store` and re-resolved on reconnect or device-type change.

This keeps existing `c8y_OpcuaDeviceType` documents usable as long as they use literal browse
paths, which is the common case. Regex browse paths and value-based `applyConstraints` matching
are the incompatibility; detect them at load time and log a single actionable warning per device
type rather than failing silently.

---

## 4. Porting map

Use this when you need to find the behaviour to replicate. Paths are relative to
`../c8y-opcua/device-gateway/src/main/java/com/cumulocity/opcua/client/gateway/`.

| Concern | Java source | Rust home |
|---|---|---|
| Gateway lifecycle, server identifiers | `GatewayManager.java`, `GatewayDetails.java` | `gateway` |
| Device bootstrap + credentials polling | `bootstrap/service/BootstrapServiceStandAlone.java` | `c8y-client`, `store` |
| ThinEdge / JWT bootstrap | `bootstrap/service/BootstrapServiceThinEdge*.java` | `c8y-client` (feature-gated) |
| OPC UA connect, reconnect, keystore | `connection/ConnectionManager.java`, `connection/security/` | `opcua-conn` |
| Subscription creation and refresh | `subscription/**` | `opcua-conn` + `mapping` |
| Cyclic read | `cyclicreader/**` | `opcua-conn` |
| Device type model | `../common-services/.../common/model/mapping/**` | `mapping` |
| Mapping execution → C8Y payloads | `mappingsexecution/**` | `mapping` |
| Queued/aggregated platform writes | `platform/repository/**`, `platform/repository/strategy/**` | `c8y-client` |
| Operation dispatch | `operation/OperationExecutor.java`, `operation/handler/**` | `gateway` |
| Value maps | `valuemap/**`, `../common-services/.../common/valuemap/` | `mapping` |
| Alarm status mapping (SpEL) | `../common-services/.../common/expression/` | `mapping` — see below |
| JMX monitoring beans | `jmx/**` | Not ported; expose Prometheus metrics instead |

### Things with no Rust equivalent — decide deliberately

- **SpEL.** `BooleanExpression` / `ServerAlarmStatusMapper` evaluate Spring Expression Language
  strings stored in `clientConfig.alarmStatusMappings`. Usage is narrow (alarm status mapping
  only). Implement a small restricted evaluator covering the comparison/boolean subset actually
  used; do not embed a general scripting engine. Unparseable expressions must be reported, not
  silently skipped.
- **Cumulocity Java SDK.** There is no official Cumulocity Rust client. `c8y-client` is written
  here against the REST and MQTT APIs. Keep it a plain typed client — no code generation from the
  OpenAPI spec unless the surface grows past what is maintainable by hand.
- **Kryo serialisation.** Local store formats are ours; use a stable, versioned encoding
  (`serde` + `postcard` or CBOR) and tolerate reading an older version.

---

## 5. Coding conventions

- **Edition 2024**, workspace-level `[workspace.dependencies]`, `[workspace.lints]` for shared
  lint config. `#![forbid(unsafe_code)]` in every crate.
- **Errors:** `thiserror` for library crates, `anyhow` only in the `gateway` binary. No
  `unwrap()`/`expect()` outside tests and `main` startup; a panic in a task must not be the
  mechanism by which a connection failure is handled.
- **Async:** `tokio`. Every long-lived task owns a `CancellationToken` and shuts down cleanly.
  Bound every channel. Prefer `tokio::select!` loops over detached tasks with no supervision.
- **Logging:** `tracing`, structured fields (`server_id`, `node_id`, `device_type_id`), never
  string interpolation into the message. `tracing-subscriber` with env filter in the binary only.
- **Config:** layered — file, then environment (`C8Y_OPCUA__` prefix), then CLI flags. Deserialize
  into typed structs with `serde`; validate once at startup and pass the validated struct down.
  Do not read config from a global.
- **Naming:** keep Cumulocity and OPC UA domain vocabulary intact (`DeviceType`,
  `MappingEntry`, `MonitoredItem`, `ServerId`) so the Java source stays greppable against this one.
- **No inline comments** describing what the code does. Comment only a non-obvious *why* —
  a spec constraint, a server-side quirk, a workaround.
- **No over-engineering.** Three similar lines beat a premature trait. Do not add traits for
  single implementations; add them when a second implementation actually lands.
- **Footprint is a requirement, not a preference.** Before adding a dependency, check what it
  pulls in. Reject anything that duplicates a tree already present. Keep an eye on binary size and
  RSS in CI.

## 6. Testing

- Unit tests alongside the code (`#[cfg(test)]`), integration tests in each crate's `tests/`.
- **OPC UA server under test:** use `async-opcua-server` from the same workspace version to stand
  up an in-process server. Do not require an external Milo instance for the default test run.
- **Cumulocity under test:** `wiremock` for REST, an embedded broker or a mock transport for MQTT.
  No test in `cargo test` may require a live tenant.
- End-to-end tests against a real tenant live behind a `--features e2e` flag and are not part of
  the default run.

---

## 7. Build

```bash
cargo build --release            # binary at target/release/c8y-opcua-gateway
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Release binaries are built for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` and their
`-musl` variants. The container image is `FROM scratch` or `distroless/static` — if a base image
with a shell is ever needed, that is a regression to justify in the PR.

---

## 8. PR and branch conventions

Inherited from `c8y-opcua`:

- **Branch:** `<type>/<JIRA-ID>/<short-description>` — e.g. `feat/DM-1234/subscription-refresh`
- **PR title:** `<semantic-tag>: [DM-<id>] [<ChangeType>] <description>`
  - tags: `feat`, `fix`, `refactor`, `perf`, `ci`, `docs`, `test`, `build`, `style`
  - change types: `[Feature]`, `[Fix]`, `[Improvement]`, `[Internal]`, `[API change]`, `[Preview]`
- **Squash-merge only.**
- Do not modify or transition non-DM Jira tickets (L2S, CST, MTM …) — read-only context.
- You are the author of all code you produce. Disclose non-trivial AI tooling use in the PR
  description.

---

## 9. Don't

- **Don't add address space scanning, browsing-for-discovery, or an address-space cache.** See
  [Section 3](#3-explicit-non-goals).
- **Don't reach for RocksDB, a DI container, or a Spring-shaped event bus** because the Java
  version had one.
- **Don't add a dependency without checking its transitive tree and its licence.** `async-opcua`
  is MPL-2.0; anything copyleft beyond that needs clearance before it lands.
- **Don't unwrap in library code**, and don't let a task die silently — supervise or propagate.
- **Don't block the async runtime.** Certificate parsing, file I/O and any CPU-bound mapping work
  go through `spawn_blocking` or a dedicated pool.
- **Don't force-push to `develop` or `release/*`.** Never `--force` / `--force-with-lease` on
  shared branches. Never `--no-verify`.
- **Don't bump versions by hand** once release automation is in place.
