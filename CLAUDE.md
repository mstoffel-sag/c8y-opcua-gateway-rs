# AGENTS.md — c8y-opcua-gateway-rs

> Agent instructions for the `c8y-opcua-gateway-rs` repository.
> This file is the primary AI coding agent reference. It is symlinked as `CLAUDE.md`.
>
> Sibling repository: [`c8y-opcua`](../c8y-opcua) — the Java implementation. Read
> `../c8y-opcua/device-gateway/AGENTS.md` for the behaviour being ported, but do not port its
> structure. The scope here is far narrower; see [Section 3](#3-explicit-non-goals).

---

## 1. What this repo is

An **OPC UA data source for [thin-edge.io](https://thin-edge.github.io/thin-edge.io/)**, written in
Rust, targeting a small runtime footprint on edge hardware.

It connects to one or more OPC UA servers, subscribes to (or cyclically reads) a configured set of
nodes, applies device-type mappings, and publishes to the **local thin-edge MQTT broker** on `te/`
topics. thin-edge's mapper and bridge carry the data to the cloud.

The gateway runs as a thin-edge **service** alongside `tedge-agent`, packaged as a systemd unit.

### It holds no cloud credentials

Everything cloud-facing goes through thin-edge on localhost:

- **Data out** — MQTT to `127.0.0.1:1883` on `te/` topics.
- **Device types and servers in** — HTTP GETs to the thin-edge Cumulocity proxy at
  `127.0.0.1:8001/c8y/...`, which injects the device's JWT for us. See
  [Section 5](#5-cumulocity-reads-through-the-thin-edge-proxy).

No bootstrap, no device credentials, no Cumulocity SDK, no operations polling, no inventory
writes. Two sockets to localhost, plus OPC UA.

The proxy is read-only with **one** exception, added deliberately and bounded to one endpoint:
`POST /identity/globalIds/<id>/externalIds`, to adopt a `c8y_OpcuaServer` managed object created
cloud-side. See [Section 5](#5-cumulocity-reads-through-the-thin-edge-proxy).

### Why Rust

The Java gateway runs on a JVM sized `-Xms128m -Xmx384m` and ships as an ~200 MB Alpine + JDK
image. Target here: a 10–20 MB static binary with a resident set in the tens of megabytes at a few
thousand monitored items. thin-edge.io is itself Rust under Apache-2.0, so this fits its
deployment model — musl static builds, deb/rpm packages, systemd units.

### Upstream OPC UA stack

[`async-opcua`](https://github.com/FreeOpcUa/async-opcua) (`async-opcua-client`), tokio-based,
**MPL-2.0**. This replaces the proprietary Prosys SDK.

Pin an exact version in the workspace `Cargo.toml`. `async-opcua` is pre-1.0, its minor releases
carry breaking changes, and it declares no MSRV and tracks latest stable Rust — so CI must pin the
toolchain via `rust-toolchain.toml`.

> **Open blocker:** MPL-2.0 is file-level copyleft and must clear third-party compliance before
> any code is written against it.

---

## 2. Architecture

```
crates/
├── gateway/       # binary `c8y-opcua-gateway`: config, wiring, supervision, command dispatch
├── opcua-conn/    # async-opcua session lifecycle, reconnect, subscriptions, cyclic read
├── mapping/       # DeviceType model, constraint evaluation, node resolution, value → payload
└── tedge/         # everything on localhost: MQTT publishing + Cumulocity proxy HTTP client
```

Four crates. `tedge` owns **both** thin-edge transports — the MQTT broker and the Cumulocity
proxy — because both are localhost thin-edge endpoints with thin-edge's auth model.

```
                    ┌─ tedge::proxy ◀── GET 127.0.0.1:8001/c8y/inventory/… (device types)
                    │        │
                    │        ▼
OPC UA ──▶ opcua-conn ──▶ mapping ──▶ tedge::mqtt ──▶ 127.0.0.1:1883 te/… ──▶ tedge-mapper ──▶ cloud
   ▲                          │
   └──────────────────────────┘
        node resolution, reads/writes/method calls
```

Commands travel back the other way: `tedge::mqtt` subscribes to `te/<id>/cmd/<type>/+`, `gateway`
dispatches to a handler, the handler calls into `opcua-conn`, and `tedge::mqtt` publishes the
status transitions. Cumulocity operations for the gateway device take the bridged path instead —
see [Section 6](#6-thin-edge-mqtt-contract).

Inside `gateway`: `gateway_device` registers the Cumulocity gateway device, `cloud_servers` reads
the servers registered on it, `supervisor` keeps one `server_task` running per server as that set
changes, `device_types` polls both mapping sources, and `operations` answers Cumulocity operations.

### Key decisions

- **Tokio single multi-threaded runtime**, sized from config, low worker count by default. Do not
  spawn per-server runtimes.
- **No dependency-injection framework.** Wire components explicitly in `gateway::main`. The Java
  version's Spring `ApplicationEventPublisher` fan-out maps to `tokio::sync::broadcast` / `mpsc`
  channels — declare the channel topology in one place.
- **MQTT client is `rumqttc`** — the same client thin-edge uses. Publish with QoS 1.
- **HTTP client is `reqwest`** with `rustls`, plain HTTP to localhost, no auth header.
- **The data path never uses HTTP.** Readings go over MQTT so that mosquitto and the bridge own
  buffering. The proxy is for device-type fetch only — read-only, roughly one request per minute.
- **The gateway is stateless. It writes nothing to disk.** No offline buffer, no device-type
  cache, no resolved-NodeId store, no local database. mosquitto's persistent queue and the
  thin-edge bridge own store-and-forward; do not reimplement it. Everything else — fetched device
  types, resolved NodeIds, subscription state — lives in memory and is rebuilt on restart:
  re-fetch, then one `TranslateBrowsePathsToNodeIds` call per server.
  The only files the gateway touches are read-only inputs: its own config, the mapping TOML in
  `/etc/tedge/opcua/`, and OPC UA certificates.
- **Batch locally anyway.** Publishing every data change as its own MQTT message will drown the
  local broker and the mapper on a fast server. Port the *intent* of the Java
  `BaseQueuedRepository` / `FlushExecutor`: bounded queues, flush on size or interval, group
  several series into one measurement message. Drop the retry and offline machinery, keep the
  bounding and the batching. On overflow, drop with a rate-limited warning — never grow unbounded.

---

## 3. Explicit non-goals

### 3.1 No cloud client of our own

| Java gateway feature | Status here |
|---|---|
| Cumulocity SDK (`java-client`), direct platform connection | Not ported — HTTP goes through the thin-edge proxy |
| Device bootstrap, device credentials, `bootstrap/**` | Not ported — thin-edge owns device identity |
| JWT retrieval, `mqtt-jwt-lib` | Not ported — the proxy injects auth |
| Writes to inventory, external IDs, `IdentityRepository` | Replaced by `te/` entity registration |
| Cumulocity operations polling, `OperationExecutor`, ~30 handlers | Replaced by thin-edge commands on `cmd/` topics |
| `BinariesRepository`, file upload operations | Not supported |
| Queued REST repositories, processing modes, offline buffer | Not ported — batching stays, persistence does not |
| JMX monitoring beans | Replaced by `status/health` |

### 3.2 No address space scanning

| Java gateway feature | Status here |
|---|---|
| `AddressSpaceScanner` / `AddressSpaceCleaner` / `addressspace/**` | Not ported |
| RocksDB address-space datastore and its inventory synchronisation | Not ported |
| `c8y_ScanAddressSpace`, UI address-space browsing | Not supported |
| Discovery of *which* nodes on a server match a device type | Not supported — binding is explicit |
| Regex browse paths, `applyConstraints.browsePathMatchesRegex` | Not supported |
| `c8y_TestDeviceTypeMatching`, `c8y_FindMatchingDeviceTypes`, dry-run matching | Not supported |
| `BaseModelChangeEvent` subscription driving re-scans | Not ported |

**Node resolution replaces matching.** A device type already carries `referencedRootNodeId` and
`referencedNamespaceTable` — the root it was authored against. Resolve each mapping entry's
`browsePath` against that root with **`TranslateBrowsePathsToNodeIds`** — one request, no tree
walk — or use the entry's absolute `referencedNodeId` when present.

`client-lib-prosys` disappears entirely: `async-opcua` *is* the client abstraction, so the vendor
isolation layer and its enforcer rule are gone.

---

## 3.3 The gateway device

The Java gateway registers itself as an extra device below the thin-edge device, typed
`c8y_OPCUA_Device_Agent` with `c8y_SupportedOperations: ["c8y_OpcuaConfiguration"]`, and hangs one
`c8y_OpcuaServer` per server below that. The existing OPC UA user interface keys on those types, so
reproducing the shape reproduces the user interface — including its "add a server" flow, where
`opcua-mgmt-service` creates the server object and the gateway picks it up.

**That shape is ported. The Java mechanism behind it is not.** thin-edge passes a child device's
`type` straight through to the managed object and turns `twin/<fragment>` into an inventory
fragment, so all of it is MQTT:

```
te/device/main//                        the thin-edge main device
te/device/main/service/opcua-gateway    this gateway, as a service
te/device/opcua-gateway//               the gateway device        → c8y_OPCUA_Device_Agent
te/device/<server>//                    one per server            → c8y_OpcuaServer
te/device/<server>-<deviceTypeId>//     one per applied type      → c8y_OpcuaDevice
```

`[gateway] enabled = false` drops back to servers registered directly below the main device with
plain thin-edge entity types, which is the only supported mode behind a non-Cumulocity bridge.

Two things that look like details and are not:

- **Do not set `com_cumulocity_model_Agent`** on the gateway object, even though the Java gateway
  does. It makes Cumulocity treat that object as its own agent and route its operations there,
  which is precisely why the Java gateway has to poll `/devicecontrol/operations`. Without it the
  operations route to the thin-edge agent and arrive on the bridged
  `c8y/devicecontrol/notifications` topic. Verified both ways on a live device.
- **Do publish `c8y_IsDevice`.** thin-edge child devices do not get it, and without it the object
  never appears in a device list — only in the hierarchy below the main device.

Still not ported, and still out of scope: the local key store behind `c8y_ua_IdentityConfig` and
encrypted server passwords (so a server whose password was authored for the Java gateway is
skipped with a warning — configure it on the device instead), and everything in
[Section 3.2](#32-no-address-space-scanning), which is what the Address space tab and the
device-type authoring tree need.

---

## 3.4 thin-edge cannot forget an entity

Measured on thin-edge 2.0.1, and it constrains the design:

- The entity store is persisted to `/etc/tedge/.agent/entity_store.jsonl`, an append-only log with
  **no deletion records**. `DELETE /te/v1/entities/<topic-id>` removes an entity from memory and
  cascades to its children, but the entity is back the next time `tedge-agent` restarts.
- Registering an entity thin-edge already knows is a **no-op**. It does not re-issue the
  child-device creation.
- Clearing a registration (empty retained payload) does **not** remove the entity. The agent
  auto-registers it again from a default payload — `name` set to the topic id, no `type`, no `@id`
  — and appends that to the log. Do it while another component still holds the entity and the two
  oscillate, appending a record per cycle. The managed object is renamed and loses the type the
  OPC UA user interface keys on.

Two consequences:

- **Deleting a gateway or server object in Cumulocity is not recoverable by the gateway.** The
  entity is still registered, so nothing is recreated and everything below it is orphaned. The
  gateway detects this — its own object stops resolving — and reports it. The cheap remedy is to
  give `gateway.id` a value thin-edge has not seen. Reusing the same id is possible but needs the
  entity removed by hand, in this order, or the agent re-creates it from what is left behind:

  1. Clear every retained *channel* topic of the entity (`twin/…`, `status/…`, `m/…/meta`) while
     the agent is up. Missing this is what makes a purge come back: on start the agent sees a
     message for an unknown entity and auto-registers it.
  2. Stop `tedge-agent` and `tedge-mapper-c8y`.
  3. Clear the entity's registration topic. Only with the agent down — doing it while the agent
     holds the entity is what starts the oscillation described above.
  4. Remove the entity's lines from `/etc/tedge/.agent/entity_store.jsonl`.
  5. Start both services, then let the gateway register again.

  Verified end to end: the same external id yields a fresh managed object with its type and
  fragments intact. Servers that were registered on the old object still have to be recreated.
- **A removed server keeps its entity registration.** There is nothing to clean up and no
  resurrection to defend against, because a known entity is never forgotten.

---

## 4. Mapping and server sources

Two mapping sources, one internal model. Both produce the same `ResolvedMapping` type; nothing
downstream of `mapping` knows which one was used. **Both are first-class — neither is a fallback
for the other.**

1. **Pull — device types fetched from Cumulocity inventory through the thin-edge proxy**
   (Section 5). The compatibility path: `c8y_OpcuaDeviceType` managed objects authored in the
   existing OPC UA UI work unchanged, and changes reach the device within a poll interval.
2. **Push — mapping files delivered to the device by thin-edge configuration management**, read
   from `/etc/tedge/opcua/`. `tedge-configuration-management` already surfaces device files in the
   Cumulocity configuration repository UI, so mappings stay remotely manageable — as versioned
   config, pushed on change, rather than polled. This path works behind any thin-edge bridge, not
   just the Cumulocity one, and it is the one that survives a cold start with no cloud
   connectivity.

Either may be enabled alone or both together; pushed files win on conflict. Watch
`/etc/tedge/opcua/` for changes and reload without a restart — configuration management updates
the files in place. Do not build a third distribution mechanism.

**Servers have three sources**, again one internal model — `ServerConfig`, whatever it came from:
inline `[[servers]]`, pushed `<mappings.dir>/servers/*.toml`, and `c8y_OpcuaServer` objects
registered on the gateway device ([Section 3.3](#33-the-gateway-device)). Servers configured on the
device win on a shared id, on the same principle as pushed mappings: the operator standing next to
the hardware gets the last word. Starting with no server at all is a valid state when the gateway
device is on — the gateway registers and waits.

The server set is therefore **dynamic**, and `gateway::supervisor` owns the difference: it starts,
stops and restarts server tasks as the set changes. Note the distinction it draws, because getting
it wrong is silently destructive — a *restarted* server keeps its retained registrations, a
*removed* one has them cleared, because a registration that outlives its managed object makes the
mapper recreate the object on the next restart.

A cloud-registered server's local id is its managed object id, which is the one thing that makes
`applyConstraints.matchesServerIds` work as authored — device types name server object ids. A
device-configured server has a free-form local id and is scoped with `device_types` instead.

---

## 5. Cumulocity reads through the thin-edge proxy

`tedge-mapper-c8y` exposes `http://127.0.0.1:8001/c8y/<any-c8y-endpoint>` and injects the device's
JWT, so **no `Authorization` header and no credentials are needed**. It forwards all public
Cumulocity REST APIs and all methods. Configurable via `c8y.proxy.bind.address` /
`c8y.proxy.bind.port`; read those from `tedge config` rather than hard-coding, and fall back to the
documented defaults.

This is exactly what the Java gateway's proxy mode does — `PlatformFactoryThinEdgeProxy` points
the Cumulocity SDK at `http://localhost:8001/c8y` with `CumulocityAnonymousCredentials`. We do the
same thing with `reqwest` and hand-written types instead of the SDK.

**Use it read-only, with exactly one exception.** Do not post measurements, do not poll
operations, do not update inventory. The whole surface is:

```
GET  /c8y/inventory/managedObjects?type=c8y_OpcuaDeviceType&fragmentType=c8y_OpcuaDeviceType&pageSize=1000
GET  /c8y/inventory/managedObjects?query=$filter=(type eq c8y_OpcuaDeviceType and lastUpdated.date gt '<ts>')
GET  /c8y/inventory/managedObjects/<moId>                  # server objects, and serverObjectHasFragment
GET  /c8y/inventory/managedObjects/<gatewayMoId>/childDevices   # servers registered on the gateway
GET  /c8y/identity/externalIds/c8y_Serial/<externalId>     # resolve our own objects
POST /c8y/identity/globalIds/<moId>/externalIds            # the one write — see below
```

### Device type polling

Poll on the same schedule as the Java gateway (`gateway.subscriptionUpdate.interval`, default
60 s): fetch all on startup, then incrementally by `lastUpdated.date`, and re-fetch all when the
total count drops, which is how deletions are detected. Port that logic from
`subscription/DeviceTypeFetcherService.java` — it is subtle and correct.

**Required error handling:**

- The proxy is documented to occasionally return a spurious `401` from Cumulocity's JWT handling
  and to forward it verbatim. **Retry a 401 once after a short backoff** before treating it as an
  error. Do not treat it as a credential problem — there are no credentials.
- `502` means the mapper cannot reach Cumulocity. Keep running on the device types already in
  memory and retry with backoff — a cloud outage must never stop OPC UA collection or MQTT
  publishing.
- A missing or unreachable proxy is never fatal. Retry with backoff, publish health as `up`, and
  log at most one warning per backoff step.
- **A cold start with no proxy comes up unmapped**, because nothing is cached. The gateway
  connects to its servers, publishes its entity registrations and health, and waits — it does not
  exit. This is the accepted cost of statelessness; pushed mapping files
  (Section 4) are the answer for deployments that must boot mapped without connectivity.

### The one write

`opcua-mgmt-service` creates a `c8y_OpcuaServer` object with **no external id**, which leaves it
invisible to thin-edge: telemetry published for that server would create a second device rather
than land on the object the user interface is showing. Planting one external id on the object
adopts it, after which its registration, telemetry and twin data all travel over MQTT.

This is the same bridge the Java gateway builds — `IdentityUtils.buildThinEdgeExternalId` plants
`<deviceId>:device:<moId>` on each generated device before publishing to
`te/device/<moId>///m/<type>`, and Cumulocity resolves the mapper's child-device creation to the
existing object. Cumulocity does not re-parent on that resolution.

Keep it to this: one endpoint, idempotent, once per server. `409` is success. If the external id
already resolves to a *different* object, report it and leave it alone — overwriting would send a
server's data to a stranger. Anything beyond this endpoint needs the same argument made again.

### Constraint compatibility

Most of `ApplyConstraints` needs no address space, which is why the compatibility path works:

| Constraint | Supported | How |
|---|---|---|
| `matchesServerIds` | yes | string comparison against the configured server id |
| `serverHasNodeWithValues` | yes | plain OPC UA Read / existence check of the explicit NodeIds in `MatchingNode` |
| `matchesNodeIds` | yes | compare against the device type's `referencedRootNodeId` |
| `serverObjectHasFragment` | yes | one `GET /c8y/inventory/managedObjects/<id>` through the proxy |
| `browsePathMatchesRegex` | **no** | needs a scanned tree |

Regex inside a mapping entry's `browsePath` is likewise unsupported. Detect both at device-type
load time and log one actionable warning per device type — never fail silently, and never drop the
whole device type when only one entry is affected.

---

## 6. thin-edge MQTT contract

thin-edge's published MQTT API
([reference](https://thin-edge.github.io/thin-edge.io/references/mqtt-api/)). Target **thin-edge
2.x**; the legacy `tedge/` topics were removed in 2.0 and must not be used.

**Entity registration** — retained, on startup, before any data:

```
te/device/main/service/opcua-gateway   {"@type":"service","@parent":"device/main//","name":"opcua-gateway","type":"systemd"}
te/device/plc-1//                      {"@type":"child-device","@parent":"device/main//","name":"plc-1","type":"opcua-server"}
```

One child device per OPC UA server; the gateway itself is a service on the main device.

**Health** — retained, on startup, with an MQTT last will for the `down` case:

```
te/device/main/service/opcua-gateway/status/health   {"status":"up","pid":1234,"time":1674739912}
```

**Telemetry:**

| Kind | Topic | Payload |
|---|---|---|
| Measurement | `te/device/<id>///m/<type>` | `{"time":"…","<fragment>":{"<series>":23.4}}` |
| Event | `te/device/<id>///e/<type>` | `{"time":"…","text":"…"}` |
| Alarm raise | `te/device/<id>///a/<type>` | `{"time":"…","text":"…","severity":"major"}` |
| Alarm clear | `te/device/<id>///a/<type>` | empty payload, **retained** |
| Static metadata | `te/device/<id>///twin/<name>` | retained JSON |
| Measurement units | `te/device/<id>///m/<type>/meta` | retained `{"<series>":{"unit":"°C"}}` |

**Commands** — the replacement for Cumulocity operations. Declare capability retained at startup,
then subscribe to the instance topic:

```
te/device/plc-1///cmd/opcua_read           {"description":"Read an OPC UA node value"}
te/device/plc-1///cmd/opcua_read/<cmd-id>  {"status":"init","node_id":"ns=2;s=Temp"}
```

The gateway is the **executor**: `init` → `executing` → `successful` | `failed`, republishing the
full payload retained at each step and adding result fields on success. It never clears the
command topic — the requester does.

Port only `opcua_read`, `opcua_write`, `opcua_call_method` initially. Historic-data, file-upload
and address-space operations are out of scope.

### The two bridged Cumulocity topics

`c8y_OpcuaConfiguration` ([Section 3.3](#33-the-gateway-device)) is a Cumulocity operation name with
no thin-edge command equivalent, and the user interface enables its controls from
`c8y_SupportedOperations`. Declaring it as a thin-edge command would mean writing a file under
`/etc/tedge/operations/c8y/<external-id>/` — a runtime-derived path, so a runtime disk write, which
this gateway will not do. Two topics on the c8y bridge replace it:

```
c8y/devicecontrol/notifications     subscribe: operations for the thin-edge agent and its children
c8y/s/us/<external-id>              publish:   SmartREST 50x, the operation's status
```

`c8y_SupportedOperations` itself goes out as a retained twin fragment, and survives the mapper
restarting with no operations directory of ours — verified.

This is the only place the gateway speaks to the bridge rather than the `te/` API, and it is for
operations only. **Telemetry never goes this way.** Anything that has a `te/` equivalent uses it.

---

## 7. Porting map

Paths are relative to `../c8y-opcua/device-gateway/src/main/java/com/cumulocity/opcua/client/gateway/`.

| Concern | Java source | Rust home |
|---|---|---|
| OPC UA connect, reconnect, keystore | `connection/ConnectionManager.java`, `connection/security/` | `opcua-conn` |
| Subscription creation and refresh | `subscription/**` | `opcua-conn` + `mapping` |
| UA event subscriptions, `EventFilter` select/where clauses | `../client-lib-prosys/.../OpcuaEventItemBuilder.java`, `subscription/UaSubscriptionService.java` | `opcua-conn` |
| Cyclic read | `cyclicreader/**` | `opcua-conn` |
| **Device type fetch and change detection** | `subscription/DeviceTypeFetcherService.java`, `mappings/DeviceTypeRepository.java` | `tedge::proxy` + `mapping` |
| **Device type model and JSON conversion** | `../common-services/.../common/model/mapping/**`, `subscription/DeviceTypeConverter.java` | `mapping` |
| **Scan-free constraint evaluation** | `mappings/NodeMatcher.java`, the `removeDeviceTypesThatDoNotMatch*` methods in `mappings/BaseDeviceTypeMatchingService.java` | `mapping` |
| **thin-edge payload construction** | `mappingsexecution/tasks/ThinEdge*Task.java` | `tedge::mqtt` — **read these first, they are the closest thing to a spec** |
| Proxy platform setup (reference for the HTTP client) | `platform/configuration/PlatformFactoryThinEdgeProxy.java` | `tedge::proxy` |
| Value maps | `valuemap/**`, `../common-services/.../common/valuemap/` | `mapping` |
| Alarm status mapping (SpEL) | `../common-services/.../common/expression/` | `mapping` — see below |
| Bad-quality / limit handling | `ValueLimitsValidator`, `isDataValueBad()` in `BaseTask` | `mapping` |
| Everything in the Section 3 tables | — | not ported |

The `ThinEdge*Task` classes already do what this gateway does, in Java, against the same topics.
They are the reference implementation for `tedge::mqtt` — including details worth not
rediscovering: bad-quality values are dropped silently, a timestamp falls back to now when the
server does not supply one, and event text supports `${value}` substitution.

### Things with no Rust equivalent — decide deliberately

- **SpEL.** `BooleanExpression` / `ServerAlarmStatusMapper` evaluate Spring Expression Language
  from `alarmStatusMappings`. Usage is narrow — alarm status mapping only. Implement a small
  restricted comparison/boolean evaluator; do not embed a scripting engine. Report unparseable
  expressions at load time.
- **Cumulocity representations.** No Rust SDK exists, and we need only a handful of read-only
  shapes. Hand-write `serde` structs for the managed-object envelope and the `c8y_OpcuaDeviceType`
  fragment. Deserialize permissively — unknown fields must not break a fetch.
- **No reusable thin-edge crates.** `tedge-api`, `tedge-actors` and friends are workspace-internal
  and unpublished. Talk to the broker and proxy as any other client would; do not add a git
  dependency on the thin-edge workspace.
- **Consider `tedge flows` instead of code.** thin-edge 2.0 added a sandboxed JavaScript runtime in
  the mapper for remapping, filtering and aggregation. If a transformation is cloud-shaped rather
  than OPC-UA-shaped, it belongs in a flow.

---

## 8. Coding conventions

- **Edition 2024**, `[workspace.dependencies]` and `[workspace.lints]`. `#![forbid(unsafe_code)]`
  in every crate.
- **Errors:** `thiserror` in library crates, `anyhow` only in the `gateway` binary. No
  `unwrap()`/`expect()` outside tests and startup; a panic must not be how a connection failure is
  handled.
- **Async:** `tokio`. Every long-lived task owns a `CancellationToken` and shuts down cleanly.
  Bound every channel. Prefer supervised `tokio::select!` loops over detached tasks.
- **Logging:** `tracing`, structured fields (`server_id`, `node_id`, `device_type_id`, `topic`),
  never interpolation into the message. `tracing-subscriber` in the binary only. Log to stdout —
  journald captures it.
- **Config:** file, then environment (`OPCUA_GW__` prefix), then CLI flags. Deserialize into typed
  structs with `serde`, validate once at startup, pass the validated struct down. No globals.
- **Naming:** keep OPC UA, Cumulocity and thin-edge vocabulary intact (`NodeId`, `MonitoredItem`,
  `DeviceType`, `ApplyConstraints`, `Entity`, `Channel`, `Command`) so all three upstreams stay
  greppable against this code.
- **No inline comments** describing what the code does. Comment a non-obvious *why* only.
- **No over-engineering.** Three similar lines beat a premature trait. The two mapping sources in
  Section 4 are a real case for one; most things are not.
- **Footprint is a requirement.** Check what a new dependency drags in; reject anything
  duplicating a tree already present. Track binary size and RSS in CI.

## 9. Testing

- Unit tests alongside the code, integration tests in each crate's `tests/`.
- **OPC UA under test:** an in-process `async-opcua-server` at the same workspace version. No
  external Milo instance in the default run.
- **thin-edge under test:** an embedded MQTT broker for the `te/` assertions, and `wiremock` for
  the proxy — including a 401-then-200 case, since retrying that is required behaviour.
- Fixture device types: copy real `c8y_OpcuaDeviceType` JSON from a tenant into `tests/fixtures/`
  and assert the conversion and constraint evaluation against them.
- No test in `cargo test` may require a running `tedge-agent` or a cloud tenant. End-to-end tests
  live behind `--features e2e`.

## 10. Build and packaging

```bash
cargo build --release            # target/release/c8y-opcua-gateway
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Targets: `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` primarily — match
thin-edge's own target set. Ship **deb and rpm packages with a systemd unit**, ordered after
`mosquitto.service` and `tedge-mapper-c8y.service`. A container image is secondary; if built, it is
`FROM scratch` or `distroless/static`.

## 11. PR and branch conventions

Inherited from `c8y-opcua`:

- **Branch:** `<type>/<JIRA-ID>/<short-description>` — e.g. `feat/DM-1234/device-type-fetch`
- **PR title:** `<semantic-tag>: [DM-<id>] [<ChangeType>] <description>`
  - tags: `feat`, `fix`, `refactor`, `perf`, `ci`, `docs`, `test`, `build`, `style`
  - change types: `[Feature]`, `[Fix]`, `[Improvement]`, `[Internal]`, `[API change]`, `[Preview]`
- **Squash-merge only.**
- Do not modify or transition non-DM Jira tickets (L2S, CST, MTM …) — read-only context.
- You are the author of all code you produce. Disclose non-trivial AI tooling use in the PR
  description.

## 12. Don't

- **Don't add credentials, a bootstrap flow, or a direct connection to Cumulocity.** All HTTP goes
  to the thin-edge proxy on localhost. See [Section 3.1](#31-no-cloud-client-of-our-own).
- **Don't send readings over the proxy.** The data path is MQTT so thin-edge owns buffering. The
  proxy is for reads, plus the one identity write in
  [Section 5](#5-cumulocity-reads-through-the-thin-edge-proxy) — do not add a second write without
  making the same argument.
- **Don't set `com_cumulocity_model_Agent` on the gateway object.** It stops operations reaching the
  device over MQTT and drags operation polling back in. See [Section 3.3](#33-the-gateway-device).
- **Don't clear an entity's registration, ever** — not to re-register it, not when the object is
  gone. thin-edge's entity store is persistent and append-only, and clearing a registration makes
  `tedge-agent` auto-register the entity from a default payload, which drops our `type` and `@id`
  and renames the managed object. See [Section 3.4](#34-thin-edge-cannot-forget-an-entity).
- **Don't write to disk.** No cache, no database, no spool directory, no state file. If something
  seems to need persisting, it either belongs in a pushed config file or does not belong here.
- **Don't add address space scanning**, browsing-for-discovery, or an address-space cache. See
  [Section 3.2](#32-no-address-space-scanning).
- **Don't use the legacy `tedge/` topics.** Removed in thin-edge 2.0.
- **Don't reimplement store-and-forward.** mosquitto and the bridge own it.
- **Don't treat a proxy 401 as fatal** — retry once; there are no credentials to be wrong.
- **Don't reach for RocksDB, a DI container, or a Spring-shaped event bus** because the Java
  version had one.
- **Don't add a dependency without checking its transitive tree and licence.** `async-opcua` is
  MPL-2.0; anything copyleft beyond that needs clearance before it lands.
- **Don't unwrap in library code**, and don't let a task die silently — supervise or propagate.
- **Don't block the async runtime.** Certificate parsing, file I/O and CPU-bound mapping work go
  through `spawn_blocking` or a dedicated pool.
- **Don't force-push to `develop` or `release/*`.** Never `--force` / `--force-with-lease` on
  shared branches. Never `--no-verify`.
