---
lep: 2026-08-18-configurable-public-service-mounting
title: Configurable public gRPC service mounting
authors:
  - blake.holifield
status: Approved
created: 2026-08-18
updated: 2026-08-27
---

# Configurable public gRPC service mounting

## Summary

Give every public gRPC service its own settings block under
`[server.grpc_public_services]`. Each block carries an `enabled` flag defaulting
to `true`, a `general` namespace for common settings, and any service-specific
settings. An absent block means enabled, so every existing configuration keeps
its current service set. Setting `enabled = false` prevents the router from
mounting that service. Configuration can therefore restrict one `loreserver`
binary to a subset, such as a read-only deployment that mounts only
`ThinClientService/v1`, without a build flag, environment name, or role.

## Motivation

`GrpcServerBuilder::with_jwt_verifier` (`lore-server/src/grpc/server.rs`) builds
one router and adds every public service to it. Only `LockService` and
`NotificationService` mount conditionally: the former requires a lock store,
while the latter registers only for the local notification mode. Nothing lets an
operator restrict the set.

Every other listener already has that control, spelled the same way.
`[server.grpc_internal].enabled` gates the replication and forwarded-request
services, `[server.quic].enabled` and `[server.quic_internal].enabled` gate the
ALPN service stores, and `[server.http].enabled` gates the HTTP listener. The
public gRPC router is the one multiplexed endpoint with no per-service control.

The immediate consumer is a read-only `ThinClientService/v1` deployment. Its four
RPCs (`ContentDiff`, `RevisionInfo`, `RevisionDiff`, `RevisionTree`) are reads.
Its only consumer lives outside this tree. Serving these RPCs from the full
server gives unrelated workloads one blast radius and one scaling unit.

Per-service settings have the same problem. A setting only one service reads has
no home on that service, so it lands in `[feature]`:
`revision_diff_source_cap` and `revision_diff_history_walk_concurrency` are read
by `ThinClientService` alone, and `history_step_size` by `RevisionService` alone.
The one setting that does sit under `[server.grpc_public_services]`,
`lock_service.max_encoding_message_size`, is not service-specific at all — every
gRPC service could accept it.

## Goals / Non-Goals

### Goals

- Let configuration restrict which services the public gRPC router mounts.
- Spell that control `enabled`, matching the flag the transports already use.
- Give each service one struct to hold everything it needs, so a specific setting
  sits beside that service's `enabled` flag.
- Keep every existing configuration working, except the one shipped key that
  moves.
- Report the services a process actually registered, so a restricted deployment
  can be verified at runtime.

### Non-Goals

- Adding a second gate for the QUIC ALPN services, the internal gRPC router, or
  the HTTP listener. Each already has a listener-level `enabled` flag.
- Per-version granularity. One block registers both the legacy and the `_v1`
  server for a proto family.
- Moving the existing `[feature]` keys into the service blocks that own them.
  The blocks make that possible; this proposal does not spend the compatibility
  break.

## Proposed Design

### One settings block per service

Every public gRPC service owns a table under `[server.grpc_public_services]`
carrying an `enabled` flag and a `general` sub-table, and may carry more:

```rust
/// Settings every public gRPC service accepts.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ServiceSettings {
    /// Max size of response payloads.
    pub max_encoding_message_size: Option<usize>,
}

/// A service with nothing to configure beyond the shared pair. Every service
/// uses this today.
#[derive(Clone, Debug, Deserialize)]
pub struct GenericServiceSettings {
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default)]
    pub general: ServiceSettings,
}

/// The shape a service takes once it needs settings of its own: the same pair,
/// plus its own. Illustrative — no service needs this yet.
#[derive(Clone, Debug, Deserialize)]
pub struct ThinClientServiceSettings {
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default)]
    pub general: ServiceSettings,
    pub revision_diff_source_cap: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct GrpcPublicServicesSettings {
    #[serde(default)]
    pub admin_service: GenericServiceSettings,
    #[serde(default)]
    pub storage_service: GenericServiceSettings,
    // ... revision, repository, environment, thin_client, lock, notification
    pub forwarded_requests: Option<ForwardedRequestsSettings>,
}
```

`general` is a nested field, so a service's own settings and the shared
ones stay distinguishable in both the TOML and the type. `forwarded_requests`
stays where it is, because it configures a cross-cutting mechanism rather than a
service.

Each flag is a scalar, so the existing `LORE__` environment layer reaches it
without a file edit:
`LORE__SERVER__GRPC_PUBLIC_SERVICES__REPOSITORY_SERVICE__ENABLED=false`.

### Enforcing the pair, and gating registration

A `GrpcServiceSettings` trait states the `enabled` / `general` contract.
`GenericServiceSettings` implements it. A service-specific settings type would
also implement it. There is no enum or name-based lookup. Each mount site in
`with_jwt_verifier` reads its settings field directly and passes the same name
to `check_enabled`. That name appears in the effective-set log (see
[Reporting the effective set](#reporting-the-effective-set)). Adding a service
requires a struct field, a mount call, and a documentation row. No central
registry enforces that set.

The router calls `check_enabled` before each `add_service` and records each
admitted service. Authentication wiring is unchanged. Each branch retains its
existing services and interceptors. A let-chain combines the new guard with the
existing `Option` for `lock_service` and `notification_service`.

### Reporting the effective set

The router logs what it registered, once:

```text
Registered public gRPC services  services="thin_client_service"  authenticated=false
```

The router records this set instead of deriving it from configuration.
`lock_service` also requires a lock store. `notification_service` registers only
for the local notification mode.

### Refusing a configuration that serves nothing

The server permits this configuration. At load time, it is indistinguishable
from one that enables only an unavailable conditional service. For example,
`lock_service` may have no lock store, or `notification_service` may use a
notification plugin. Only the router knows that these configurations mount
nothing. A load-time check would catch one empty-router case but not the others.

The router therefore warns about an empty set instead of failing
`Settings::load`; see
[Reporting the effective set](#reporting-the-effective-set). Refusing the
configuration during router construction would return the wrong process status.
`with_jwt_verifier` runs inside the gRPC endpoint task, where an error drains
gracefully and exits **0**. `Settings::load` failures exit 1.

An all-disabled process is legal: it starts, serves no public gRPC RPC, and
says so in its log. This also permits a deployment with no public gRPC
surface at all — for example, QUIC-only or gRPC-internal-only.

### Shipped changes

- `lore-server/config/default.toml` moves one key:
  `[server.grpc_public_services.lock_service].max_encoding_message_size` becomes
  `[server.grpc_public_services.lock_service.general].max_encoding_message_size`.
- `[lock_store]` gains `mode = "none"`, the only way for a layered configuration
  to opt out of a store `default.toml` already set.
- `lore-server/config/thin.example.toml` ships as a worked read-only deployment:
  the seven exclusions, the QUIC and HTTP listeners disabled, and both stores
  pointed at a separate full server.

No `enabled` key is added to any other shipped file. Writing the full set out as
a shipped default would need editing every time a service is added, and the
defaults already produce exactly the server they produce today.

## Compatibility

- **Wire format** — N/A. No serialization, framing, or byte layout changes.
- **Client/server protocols** — No RPC definitions change. A client calling an
  unmounted service receives gRPC status `12` (`UNIMPLEMENTED`) from `tonic`'s
  router, the same status any server that does not implement a service returns.
  Maintenance mode already ships this shape: `serve_maintenance` registers only
  the environment service, and `scripts/test/test_maintenance_mode.py` asserts
  `UNIMPLEMENTED` for `/urc.rpc.AdminService/ServerInfo` against it.
- **On-disk format** — N/A. No fragment, index, or schema change.
- **CLI and public API** — N/A.
- **Configuration format** — Additive, with one key relocated and `[lock_store]`
  gaining a `none` mode. An absent block, and an absent `enabled` inside a
  present block, both mean enabled, so a file that never mentions
  `[server.grpc_public_services]` behaves exactly as it does today. A file
  disabling every service now loads and starts a process that serves no public
  gRPC RPC; nothing in the shipped configuration exercises that shape.

## Non-Functional Considerations

- **Concurrency** — Unchanged. The gate runs once per service at startup.
- **Memory** — Unchanged by the gate itself. Store modes, not service flags,
  determine a restricted deployment's memory use.
- **Statelessness** — Unchanged. The flags introduce no state that survives an
  operation.
- **Determinism** — Unchanged. Restricting which services are reachable does not
  change what a reachable service computes.

## Migration Plan

The `enabled` flags are additive and default to the current service set.
Deployments using
`[server.grpc_public_services.lock_service].max_encoding_message_size` must move
the key beneath `lock_service.general`. During a mixed-version rollout, set both
paths. Legacy servers read the direct key; current servers read the key beneath
`general`. Remove the direct key after all servers are upgraded.

## Security Considerations

An unmounted service is unreachable, not merely unauthorized, so a flaw in a
disabled handler cannot be reached through that process at all. Disabling
`admin_service` removes `Obliterate`, which deletes content, and `ServerInfo`,
which returns the hostname, CPU, RAM and settings map. Disabling
`storage_service` removes the write path from the public listener.

The flags are not a substitute for authentication. Mounted services keep the same
`JWTInterceptor` and `JWTAuthnInterceptor` wiring, and this proposal changes no
authentication flow. Unknown keys stay ignored, so a misspelled disable leaves
the service registered — a restricted deployment is verified from the
`Registered public gRPC services` line, not from its file.

## Privacy Considerations

No privacy implications. The proposal changes which services a process mounts. It
introduces no new user data, identifier, file path, or metadata, changes nothing
a mounted service returns, and creates no new copy of content, so deletion,
redaction, and expiry behavior is unchanged.

## Risks and Assumptions

**Assumptions**

- **Assumption:** no deployment needs one half of a proto family without the
  other — *invalidated if:* a deployment wants to drop the legacy `urc.rpc`
  surface while keeping `_v1`, or the reverse. One block gates both, so that
  needs a second block rather than a second flag.

**Risks**

- **Risk:** a configuration that enables only `lock_service` without a lock
  store, or only `notification_service` with a notification plugin, registers
  nothing. This result is indistinguishable from a deliberate all-disabled
  configuration — *mitigation:* accepted and not load-time checkable. Only the
  router knows the effective set, so neither shape has a load-time check.
  The router's own `warn!` on an empty `registered` reports both.
- **Risk:** a service added to the router later mounts on a restricted
  deployment, because a block nobody wrote defaults to enabled — *mitigation:*
  partial. The effective-set log reports it. Nothing blocks it, which is the
  accepted cost of a denylist.
- **Risk:** `#[derive(Default)]` reaches `GenericServiceSettings` in a later edit
  and flips every unnamed service to disabled — *mitigation:* the hand-written
  impl carries a comment saying so, and a unit test asserts an empty
  configuration enables every service.
- **Risk:** a typo leaves a service mounted in production — *mitigation:* none at
  load time. Unknown keys are ignored, as they are everywhere else in the
  settings tree.

## Drawbacks

- By default the list only denies routes. A new service therefore reaches every
  deployment, including one intended to serve a single service.
- The file records what is switched off, so answering "what does this process
  serve" from it means knowing the full list and subtracting.
- The design repeats the gating twice, once per authentication branch, because
  those branches duplicate every mount today.

## Alternatives Considered

### An `only_register` allow-list naming the services to mount

One key on the existing table, `only_register = ["thin_client"]`, with an absent
key meaning "mount everything".

*Rejected because:* it introduces a second vocabulary for a decision the server
already calls `enabled`. It also composes poorly with the loader. `config`
replaces arrays instead of merging them, so every layer must restate the full
list. The environment loader does not parse arrays, so changing the list also
requires a file edit. Finally, the list separates a service's selection from its
own settings table. The allow-list does fail closed; this proposal accepts losing
that property.

### A flat `[server].services` list spanning every transport

A single list naming `admin`, `storage`, `replication`, `thin_client` and so on,
covering the gRPC router, the QUIC ALPN stores, the internal gRPC router, and the
HTTP listener together.

*Rejected because:* `storage` names both a public gRPC service and two QUIC ALPN
protocols, so `services = ["storage"]` alongside `quic.enabled = true` has no
defined meaning. Every entry except the public gRPC services already has an
`enabled` flag, so the list would duplicate an existing mechanism and require a
documented conflict-resolution rule.

### A separate binary or a cargo feature

*Rejected because:* it doubles the build and release matrix and moves a
deployment decision to compile time. Diagnosing a restricted deployment would
require identifying its binary. The configured service set and startup log
identify the deployment directly.
