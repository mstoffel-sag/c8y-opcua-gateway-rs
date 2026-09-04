# Evaluation: rewriting the OPC UA device gateway in Rust

Assessment of porting `c8y-opcua/device-gateway` to Rust on top of
[`async-opcua`](https://github.com/FreeOpcUa/async-opcua), with address space scanning dropped.

Date: 2026-09-03. Java baseline: `c8y-opcua` @ `develop` (1023.2.14).

---

## 1. What is being replaced

Measured on the Java source:

| | Files | Lines |
|---|---|---|
| `device-gateway` | 228 | ~19,100 |
| `common-services` (mapping model, value maps, queued repositories) | 37 | ~2,000 |
| `client-lib` + `client-lib-prosys` (OPC UA abstraction over Prosys) | 34 + n | — |

Largest areas inside `device-gateway`:

| Package | Lines |
|---|---|
| `subscription` | 1,884 |
| `mappings` (device-type matching) | 1,593 |
| `monitoring` | 927 |
| `mappingsexecution` | 883 |
| `configuration` | 648 |
| `connection` | 452 |
| `jmx` | 453 |
| `operation` (+ ~30 handlers) | 378 |

Dropping address space scanning removes `addressspace/**`, `AddressSpaceScanner`,
`AddressSpaceCleaner`, the RocksDB address-space store and its inventory synchronisation, the
model-change subscription, and the entire `mappings` matching engine — roughly a quarter to a
third of the module, and the part with the worst complexity-to-value ratio.

`client-lib-prosys` disappears entirely: `async-opcua` *is* the client abstraction, so the
vendor-isolation layer and its enforcer rule are no longer needed.

## 2. Footprint — the case for doing this

Current runtime: `-Xms128m -Xmx384m` on `alpine:3` + `openjdk11` (note: the Dockerfile still
installs JDK 11 while the project builds for Java 17). Real resident set lands in the
400–600 MB range once JVM overhead, Netty buffers and the Prosys stack are counted; the image is
north of 200 MB.

A Rust build with tokio, rustls and `async-opcua` should land at a 10–20 MB static binary and
tens of megabytes RSS at a few thousand monitored items. **A 10–20× reduction in memory and
roughly 10× in image size is a realistic target**, plus no GC pauses in the data path and a
sub-second cold start. This is a genuine enabler for gateways on constrained edge hardware, which
is the strongest argument for the project.

## 3. `async-opcua` viability

Current version **0.19.0** (2026-07-18), ~234k downloads lifetime, actively developed
(~2,500 commits), MPL-2.0.

Service coverage on `Session` is complete for what the gateway needs: `read`, `write`,
`browse` / `browse_next`, `translate_browse_paths_to_node_ids`, `call` / `call_one`,
`history_read` / `history_update`, `create_subscription` / `modify_subscription`,
`create_monitored_items` / `modify_monitored_items` / `set_monitoring_mode` / `set_triggering`,
`republish`, `transfer_subscriptions`, plus a configurable session retry policy and async data
change and event callbacks. Nothing the Java gateway does is missing at the protocol level.

Risks, in order of seriousness:

1. **Licence.** MPL-2.0 is file-level copyleft. It is generally compatible with shipping a
   commercial binary, but it has to clear the third-party compliance process before any code is
   written against it. **Resolve this first — it can kill the project.**
2. **Loss of a certified, supported stack.** Prosys is a commercial SDK with OPC Foundation
   certification and a support contract behind it. `async-opcua` is community-maintained, states
   "use at your own risk", and carries no certification. Interop bugs against a customer's PLC
   become our problem to fix upstream.
3. **Pre-1.0 with no MSRV.** Breaking changes land in minor releases (0.16 → 0.19 in ~14 months),
   and the project explicitly tracks latest stable Rust. Pin both the crate and the toolchain.
4. **Documentation is thin.** The client tutorial is marked work-in-progress and covers only
   sessions, subscriptions and monitored items; method calls, event filters and historical read
   are undocumented despite being implemented. Expect to read source.

## 4. What has to be built from scratch

- **A Cumulocity client.** There is no official Rust SDK — only Java, JavaScript, .NET and an
  iOS community library. `c8y-client` must cover device bootstrap
  (`/devicecontrol/deviceCredentials` polling), identity, inventory CRUD + `$filter` queries,
  measurements / events / alarms including bulk, operations polling and status updates, and
  binaries. Sizeable but mechanical, and MQTT SmartREST 2.0 can carry most of the data path
  cheaply. The gateway already reads device types straight from inventory rather than from the
  management microservice, so no new server-side surface is needed.
- **The queued-write layer.** `BaseQueuedRepository`, `FlushExecutor`, `DataAggregator` — the
  backpressure machinery that keeps the platform from being overrun. Must be ported faithfully;
  it is not optional infrastructure.
- **A SpEL substitute.** `BooleanExpression` and `ServerAlarmStatusMapper` evaluate Spring
  Expression Language from `clientConfig.alarmStatusMappings`. Usage is narrow — alarm status
  mapping only — so a restricted comparison/boolean evaluator suffices. Existing tenant data
  containing anything richer will not evaluate.
- **Replacements for Kryo, RocksDB and JMX** — a versioned `serde` encoding, `redb`, and
  Prometheus metrics respectively. All simpler than what they replace once the address-space
  cache is gone.

## 5. Functional cost of dropping the scan

This is the part that needs a product decision, not an engineering one.

The Cumulocity OPC UA device-type model is built on *browse-path matching against a scanned
address space*. Remove the scan and the following stop working:

- Address-space browsing in the management UI (`c8y_ScanAddressSpace`).
- Automatic discovery of which device types apply to which nodes on a server
  (`c8y_FindMatchingDeviceTypes`, `c8y_TestDeviceTypeMatching`, dry-run matching).
- Regex browse paths.
- Value-based `applyConstraints` (`matchAll` / `matchOneOf` over node values).

**Mitigation:** bind a device type to a server and root node explicitly, then resolve each
mapping entry's `browsePath` with `TranslateBrowsePathsToNodeIds` — one round trip, no tree walk —
and honour `referencedNodeId` when present. Existing `c8y_OpcuaDeviceType` documents that use
literal browse paths keep working unchanged, which covers the common case.

The consequence to accept: this gateway is configuration-driven, not discovery-driven. Onboarding
a server means knowing its NodeIds or browse paths up front, and the existing UI workflow does not
support that today.

## 6. Effort

One focused engineer, feature-comparable minus scanning:

| Phase | Scope | Estimate |
|---|---|---|
| 1 | Bootstrap/registration, connect, subscribe by NodeId, measurements to C8Y | 3–5 weeks |
| 2 | Events, alarms, cyclic read, reconnect and store-and-forward, operations subset | 6–10 weeks |
| 3 | Browse-path resolution, expression subset, security policies and cert management, ThinEdge/MQTT, packaging, CI, TCA | 8–12 weeks |

**≈ 4–7 months**, excluding integration-test parity against the Cucumber suite and excluding any
management-UI work needed to configure a non-discovering gateway.

## 7. Recommendation

Proceed, but scope it as a **new lightweight gateway that coexists with the Java one**, not as its
replacement. It should target edge and constrained deployments where the Java gateway does not
fit, and where NodeIds are known in advance.

Positioning it as a successor would mean owning the discovery UX gap, the loss of a certified OPC
UA stack, and a from-scratch Cumulocity client all at once.

Sequence before writing code:

1. Clear MPL-2.0 through third-party compliance. Blocking.
2. Get a product decision on configuration-driven onboarding — confirm the UI can express it, or
   accept file-based configuration for the first release.
3. Prototype phase 1 against a real customer server to smoke out `async-opcua` interop early,
   and measure RSS to confirm the footprint claim before committing to the full port.

---

# Revision, 2026-09-04: thin-edge-only scope

The gateway will not integrate with Cumulocity at all. It publishes to the local thin-edge MQTT
broker on `te/` topics and thin-edge carries the data to the cloud. This revises Sections 4, 5
and 6 above substantially. Rust remains the chosen language.

## What this removes

The largest single line item in the original estimate — writing a Cumulocity client from scratch
because no Rust SDK exists — disappears. So does most of what surrounded it:

| Dropped | Java source |
|---|---|
| Cumulocity REST client and SDK usage | `platform/**` |
| Device bootstrap and credentials polling | `bootstrap/**` |
| Inventory managed objects, external IDs | `platform/repository/`, `identitiy/` |
| Operations polling and ~30 handlers | `operation/**` |
| Binaries / file upload | `platform/repository/BinariesRepository.java` |
| Queued REST repositories, flush strategies, processing modes | `platform/repository/**` |
| Offline buffering and local persistence | `datastore/**` |
| JWT and the HTTP proxy path | `mqtt-jwt-lib`, `PlatformFactoryThinEdgeProxy` |
| JMX monitoring beans | `jmx/**` |

Each has a thin-edge equivalent that is already built, tested and maintained: entity registration
and `twin/` topics for inventory, `cmd/` topics for operations, `status/health` for monitoring,
mosquitto's persistent queue plus the bridge for store-and-forward, and `tedge cert` for identity.

The crate layout drops from five to four — `c8y-client` and `store` are both gone.

## Verified integration surface

thin-edge.io **2.0.1** (2026-05-28), Apache-2.0, written in Rust, actively developed. Targeting
2.x is right; the legacy `tedge/` topics were removed in 2.0.

The Java gateway's `ThinEdge*Task` classes already publish to exactly the topics this design needs
— `te/device/<id>///m/<type>`, `.../e/<type>`, `.../a/<type>` — so the payload contract is not
speculative, it is running code we can port field for field, including the empty-retained-message
alarm clear and the `${value}` substitution in event text.

What the Java path does *not* use, and this design does:

- **Entity registration** — retained `@type` / `@parent` messages, one child device per OPC UA
  server. The Java ThinEdge path still created child devices over the REST inventory API.
- **`status/health`** — replaces JMX.
- **`cmd/` topics** — capability declared retained at startup, then the executor state machine
  `init` → `executing` → `successful` | `failed`. This is how read/write/method-call operations
  reach the gateway without any REST polling.

`rumqttc` 0.25.1 is the client; thin-edge uses it too. The `tedge-*` crates are workspace-internal
and unpublished, so there is no library to reuse — we speak MQTT like any other client, which is
the documented integration path.

## What the scope change costs

**Mapping definitions have nowhere to come from.** The Java gateway reads device types as managed
objects from Cumulocity inventory. With no inventory client, mappings become **local
configuration files** under `/etc/tedge/opcua/`.

This is less of a regression than it looks: thin-edge configuration management already exposes
device files in the Cumulocity configuration repository UI for remote download and upload, so
mappings stay remotely manageable without us building a distribution mechanism. But it is a
different operational model from editing device types in the OPC UA UI, and it should be a
conscious product decision rather than a side effect. Combined with dropping address space
scanning, this gateway is fully configuration-driven — there is no discovery step at all.

## What the scope change gains, beyond effort

**Cloud independence.** Publishing only to `te/` topics means thin-edge 2.0's configurable bridges
can route the data to Cumulocity or to any other MQTT cloud with no change here. The component
stops being a Cumulocity OPC UA gateway and becomes an OPC UA source for thin-edge. That is worth
weighing against the repository's current `c8y-` name.

**Attack surface and operational simplicity.** One outbound socket besides OPC UA, to
`localhost:1883`. No credentials to store, rotate, or leak. No certificate handling on the cloud
side. Identity, TLS and reconnection to the cloud are thin-edge's problem.

**Ecosystem fit.** thin-edge is Rust and Apache-2.0, with an established packaging story — musl
static builds, deb/rpm, systemd units, riscv64 support. This project can follow it exactly rather
than inventing a deployment model.

## Revised effort

| Phase | Scope | Estimate |
|---|---|---|
| 1 | Config model, connect, subscribe by NodeId, entity registration, health, measurements to `te/` | 3–4 weeks |
| 2 | Events, alarms incl. retained clear, UA event subscriptions with `EventFilter`, cyclic read, reconnect, local batching | 4–6 weeks |
| 3 | Browse-path resolution, alarm-status expression subset, `opcua_read` / `opcua_write` / `opcua_call_method` commands, security policies and certs, deb/rpm + systemd, CI | 4–6 weeks |

**≈ 3–4 months**, down from 4–7. The reduction is real work removed, not optimism: roughly half
the remaining Java gateway — everything under `platform/`, `bootstrap/`, `operation/`,
`datastore/` and `jmx/` — has a thin-edge equivalent rather than a Rust port.

## What has not changed

**`async-opcua` is MPL-2.0 and that is still the blocking risk.** File-level copyleft, and it must
clear third-party compliance before code is written against it. Scoping the gateway down to MQTT
does not touch this. If compliance rejects MPL-2.0, the options are gopcua (MIT, but no OPC UA 1.04
AES security policies) or paying for a commercial stack.

Also unchanged: `async-opcua` is pre-1.0 with breaking minor releases and no MSRV, its docs are
thin, and it carries no OPC Foundation certification. Pin the crate and the toolchain, and
prototype phase 1 against a real customer server early to smoke out interop before committing.

## Revised recommendation

Proceed on Rust + `async-opcua` + thin-edge, subject to the MPL-2.0 clearance. The narrowed scope
makes this a materially better proposition than the original: a ~3–4 month build of a focused,
cloud-agnostic component, rather than a ~4–7 month reimplementation of a platform client that
thin-edge already provides.

Sequence before writing code:

1. Clear MPL-2.0 through third-party compliance. Still blocking.
2. Confirm the product decision on file-based, configuration-driven mappings delivered through
   thin-edge configuration management.
3. Prototype phase 1 against a real customer server, and measure RSS against the footprint claim.

---

# Revision 2, 2026-09-04: device types via the thin-edge Cumulocity proxy

The previous revision assumed no HTTP to Cumulocity at all, which forced mappings to be local
files only. The Java gateway's existing thin-edge **proxy mode** offers a better option, and it
recovers compatibility with the existing device-type model at very low cost.

## What proxy mode already does

`PlatformFactoryThinEdgeProxy` — active when `gateway.thinEdge.enabled=true` **and**
`gateway.thinEdge.useHttpProxy=true` — points the Cumulocity Java SDK at
`http://localhost:8001/c8y` using `CumulocityAnonymousCredentials`, i.e. no authentication of its
own. `tedge-mapper-c8y` injects the device's JWT.

Verified against thin-edge's
[Cumulocity proxy reference](https://thin-edge.github.io/thin-edge.io/references/cumulocity-proxy/):

- Binds `127.0.0.1:8001` by default (`c8y.proxy.bind.address` / `c8y.proxy.bind.port`).
- URL shape `http://{host}:{port}/c8y/{c8y-endpoint}`.
- Injects device credentials; no `Authorization` header needed. A client-supplied header wins if
  present.
- Forwards **all public Cumulocity REST APIs and all methods**.
- Documented failure modes: occasional spurious `401` from Cumulocity JWT handling, forwarded
  verbatim, and `502` when the mapper cannot reach Cumulocity.

So the full inventory API is reachable from the gateway with zero credential management.

## What this buys

**Device types work unchanged.** Fetch `c8y_OpcuaDeviceType` managed objects through the proxy and
convert them to mappings. Device types authored in the existing OPC UA UI keep working, and the
Java gateway's change-detection logic in `DeviceTypeFetcherService` — incremental fetch by
`lastUpdated.date`, full re-fetch when the total count drops to catch deletions — ports directly.

**A device type already carries its own binding.** `referencedRootNodeId` and
`referencedNamespaceTable` record the root the device type was authored against. That is exactly
the anchor `TranslateBrowsePathsToNodeIds` needs, so a device type is self-describing without a
scan.

## Correction to Revision 1

Revision 1 listed value-based `applyConstraints` as unsupported without scanning. **That was
wrong.** Reading `NodeMatcher` and the `removeDeviceTypesThatDoNotMatch*` methods shows most
constraints never touch the address space:

| Constraint | Needs a scan? | Mechanism |
|---|---|---|
| `matchesServerIds` | no | string comparison |
| `serverHasNodeWithValues` | no | OPC UA Read / existence check of the explicit NodeIds in `MatchingNode` |
| `matchesNodeIds` | no | comparison against `referencedRootNodeId` |
| `serverObjectHasFragment` | no | one inventory GET, available through the proxy |
| `browsePathMatchesRegex` | **yes** | needs a tree to match paths against |

Only `browsePathMatchesRegex`, and regex inside a mapping entry's `browsePath`, genuinely require
the scan. Compatibility with existing device types is therefore much broader than Revision 1
claimed — a device type using literal browse paths and any non-regex constraint works as-is.

## The trade-off to make consciously

This re-adds a slice of the cloud integration that Revision 1 removed, and it partly gives up the
cloud-agnostic property that revision highlighted: a gateway that fetches `c8y_OpcuaDeviceType`
managed objects is coupled to Cumulocity and to the OPC UA UI's data model, where a gateway
reading local TOML works behind any thin-edge bridge.

The slice is small and bounded — read-only GETs, no credentials, no bootstrap, no SDK, roughly one
request a minute, and no writes ever. It is nothing like the client Section 4 originally scoped.

**Recommendation: support both sources behind one internal model**, with local files winning on
conflict. The proxy path is the compatibility and migration story; the local-file path keeps the
component usable behind a non-Cumulocity bridge. This is one trait with two implementations, not
an abstraction built on speculation.

**Keep readings on MQTT regardless.** Publishing measurements through the proxy — which the Java
proxy mode does — would put the gateway back in charge of buffering, retry and backpressure
against a remote endpoint. Over MQTT to localhost, mosquitto and the bridge own all of that.
Use each transport for what it is good at: MQTT for the high-rate data path, HTTP for
low-frequency configuration reads.

## Required behaviour that follows

- **Retry a proxy `401` once** after short backoff before treating it as an error. It is a
  documented thin-edge quirk, not a credential problem — there are no credentials.
- **A `502` must not stop OPC UA collection.** Keep running on the last known device types and
  retry with backoff.
- **Cache the last good device-type set on disk** so a restart during a cloud outage still comes
  up mapped. This is the only thing worth persisting.
- **Read the proxy address from `tedge config`** rather than hard-coding 8001.

## Revised effort

Adds roughly 2–3 weeks over Revision 1 for the proxy HTTP client, the `c8y_OpcuaDeviceType`
deserialisation, the change-detection logic and the scan-free constraint evaluation.

| Phase | Scope | Estimate |
|---|---|---|
| 1 | Config, connect, subscribe by NodeId, entity registration, health, measurements to `te/` | 3–4 weeks |
| 2 | Device-type fetch through the proxy, conversion, constraint evaluation, browse-path resolution | 3–4 weeks |
| 3 | Events, alarms incl. retained clear, UA event subscriptions with `EventFilter`, cyclic read, reconnect, local batching | 4–6 weeks |
| 4 | Alarm-status expression subset, `opcua_read` / `opcua_write` / `opcua_call_method` commands, security policies and certs, deb/rpm + systemd, CI | 4–5 weeks |

**≈ 3.5–4.5 months.** Still well below the original 4–7, and now with a real compatibility story
rather than a clean break.

## Still open

**`async-opcua` is MPL-2.0 and remains the blocking risk** — file-level copyleft, needs
third-party compliance clearance before code is written against it. Unchanged by any of this.

**Authoring new device types still wants a scan somewhere.** Existing device types work, but the
OPC UA UI builds `browsePath` arrays from a scanned tree. A tenant whose only gateway is this one
can consume device types but cannot conveniently author new ones. Either accept that authoring
happens against a Java gateway or by hand, or treat UI support for scan-free authoring as separate
product work.

---

# Revision 3, 2026-09-04: stateless gateway, two first-class mapping sources

Two decisions, both narrowing.

## The gateway writes nothing to disk

No device-type cache, no offline buffer, no resolved-NodeId store, no local database. Readings go
straight to MQTT; mosquitto's persistent queue and the thin-edge bridge own store-and-forward.
Fetched device types, resolved NodeIds and subscription state live in memory and are rebuilt on
restart.

This supersedes the disk cache proposed in Revision 2. The only files the gateway touches are
read-only inputs: its own configuration, mapping files, and OPC UA certificates.

**What it costs:** a cold start with no cloud connectivity, in a proxy-only deployment, comes up
unmapped. The gateway connects to its servers, registers its entities, publishes health and waits
for the proxy — it does not exit, and it does not degrade into a half-mapped state. That is the
accepted trade, and the pushed-config source below is the mitigation for deployments where it
matters.

**What it buys:** no cache invalidation, no schema versioning for persisted data, no corrupt-state
recovery path, no writable directory to provision or permission, no disk wear on flash-backed edge
hardware. It also removes the last reason to pick a local key-value store, so `redb`, Kryo-style
serialisation and every question they carry drop out of the design entirely.

## Both mapping sources are first-class

Confirmed as a deliberate pair rather than a primary and a fallback:

| | Pull | Push |
|---|---|---|
| Source | `c8y_OpcuaDeviceType` managed objects via the thin-edge Cumulocity proxy | mapping files in `/etc/tedge/opcua/`, delivered by `tedge-configuration-management` |
| Authoring | existing OPC UA UI | configuration repository, versioned |
| Propagation | polled, within one interval | pushed on change |
| Needs cloud at boot | yes | no |
| Cloud-agnostic | no — Cumulocity data model | yes — works behind any thin-edge bridge |

They cover each other's weaknesses exactly. Pull gives compatibility and a migration path for
tenants with existing device types; push gives versioned, reviewable configuration, boots without
connectivity, and keeps the component usable behind a non-Cumulocity bridge. Pushed files win on
conflict, and the gateway watches the directory so configuration-management updates apply without
a restart.

Both deserialise into one internal `ResolvedMapping`; nothing downstream knows which source
produced it. This is the one place in the design where a trait with two implementations is
justified up front rather than speculative.

## Effect on effort

Neutral to slightly negative. Dropping persistence removes work from phases 1 and 3; adding a
file watcher and a second deserialiser to phase 2 adds a little back. The estimate stays at
**≈ 3.5–4.5 months**.

## Unchanged

`async-opcua` is MPL-2.0 and still needs third-party compliance clearance before code is written
against it. Authoring *new* device types still wants a scanned address space somewhere, so a
tenant whose only gateway is this one can consume device types but not conveniently author them.
