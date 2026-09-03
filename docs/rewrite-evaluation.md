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
