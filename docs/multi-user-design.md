# Multi-user shared-server design

Status: implemented. User-facing CLI is unified — every user runs the same
`adb-hub` / `adb-hub --daemon` commands. Internally, Linux uses a shared
`:5037` daemon plus a per-user agent; other platforms silently use the classic
in-process aggregator.

This document describes how many OS users on one Linux server can share the
default `127.0.0.1:5037` endpoint while keeping pair codes and remote backends
private. The public workflow does not distinguish “single-user” vs “multi-user”.

## Requirements

1. Every user continues to use the original ADB commands and the default
   `127.0.0.1:5037` endpoint.
2. A user sees only:
   - remote devices paired in that user's own `adb-hub` configuration; and
   - ADB devices physically attached to the shared server.
3. Pair codes remain in the owning user's home directory and process. A common
   daemon must not read other users' configuration files or store their pair
   codes.
4. One user's remote backends, device list, sessions, and configuration changes
   must not affect another user's remote backends.
5. The server's physical ADB backend is started only once and is visible to all
   users.
6. Users should not need separate “daemon” vs “agent” commands for normal use.

## Core decision

Internally there are still two roles (daemon + agent), but one CLI starts both
when needed:

```text
                         one shared listener (first adb-hub on the host)
                    +----------------------+
original adb :5037 ->|   shared data plane  |
                    | accept + UID routing |
                    | shared local devices |
                    +----------+-----------+
                               |
                  custom, authenticated IPC
                     /                     \
            embedded in each user's   embedded in each user's
            `adb-hub --daemon`        `adb-hub --daemon`
            +------------------+     +------------------+
            | Alice backends   |     | Bob backends     |
            | Alice pair codes |     | Bob pair codes   |
            +------------------+     +------------------+
```

The shared data plane owns `:5037` and the server-local ADB backend, identifies
the OS user behind each local ADB connection, and routes private-device requests
to that user's agent.

Each user's agent (started automatically by `adb-hub --daemon`) reads only that
user's configuration, authenticates to paired `adb-proxy` backends, polls their
device lists, and opens private backend sessions on behalf of the shared plane.

Pair codes never need to cross the agent/daemon boundary.

If `:5037` is already owned by another user's hub, later `adb-hub --daemon`
processes only register their agent and join the existing listener.

## Why an agent is required

A daemon started by one unprivileged user normally cannot and must not read
configuration files in another user's home directory. Sending all pair codes
to the common daemon would also turn it into a central credential store.

The per-user agent avoids both problems:

- it inherits the user's normal filesystem permissions;
- it reads `~/.config/adb-hub/config.toml` (or the platform-specific existing
  location);
- it performs `auth:<pair-code>` directly against `adb-proxy`;
- it sends only sanitized device metadata, route identifiers, status, and
  proxied ADB stream data to the shared data plane.

An agent must never send a pair code in registration, logs, device snapshots,
or error messages.

## Device view

The daemon maintains a separate logical registry for each user:

```text
visible_devices(uid) =
    shared_local_devices
    + private_devices_reported_by(agent(uid))
```

Serial conflict rewriting is performed independently for every user view.
Alice's backend names and serial conflicts therefore cannot change the serials
shown to Bob.

If a user has no registered agent or no paired backends, that user sees only
the shared server devices.

If an agent disconnects, only that user's private devices are removed. Shared
devices and other users remain available.

## Connection and identity flow

### Agent control connection

The agent connects to a local control endpoint and registers its protocol
version and capabilities. The daemon derives the real OS identity from the IPC
transport. It must not trust a UID, username, PID, or SID supplied in the
protocol payload.

On Linux the control endpoint is an `AF_UNIX` socket and the daemon uses
`SO_PEERCRED`. An abstract Unix socket is preferred when the shared listener is
started without administrator-managed `/run` storage.

Only one live agent is allowed for a UID by default. A reconnect atomically
replaces a stale connection after an instance-token/heartbeat check.

### Original ADB connection

The shared data plane accepts the original ADB client's TCP connection on
`127.0.0.1:5037`. On Linux it obtains the peer tuple and queries
`NETLINK_SOCK_DIAG`/`inet_diag`. The matching client socket contains
`idiag_uid`, which selects the tenant.

Requirements for this lookup:

- the ADB client connection must be local;
- the client and daemon must be in the same network namespace;
- both IPv4 and IPv6 loopback must be handled;
- the query must match the client-side tuple, not the accepted server socket;
- failure or ambiguity must fail closed and must not fall back to another
  user's tenant.

The daemon must reject non-loopback clients in multi-user mode.

## Agent/daemon protocol

The protocol is versioned and multiplexed over one long-lived authenticated
local connection per user. Exact binary framing is an implementation detail,
but the first version needs these logical messages:

- `RegisterAgent(version, capabilities)`
- `DeviceSnapshot(generation, devices)`
- `DeviceSnapshotChanged(generation, devices)`
- `OpenPrivateStream(stream_id, route_id, service)`
- `OpenResult(stream_id, okay | fail_reason)`
- `StreamData(stream_id, bytes)`
- `StreamClose(stream_id, reason)`
- `Ping` / `Pong`

`route_id` is opaque outside the owning agent. It must not contain a pair code.

Flow control and bounded per-user queues are required so one stalled agent
cannot exhaust daemon memory or delay other users. Stream IDs are scoped to one
agent connection.

## ADB request routing

For each accepted ADB connection, the shared data plane:

1. resolves the client UID;
2. reads only that UID's combined registry;
3. answers aggregated `devices` and `track-devices` requests;
4. routes a shared-device request to the common local ADB backend;
5. sends `OpenPrivateStream` to that UID's agent for a private-device request;
6. bridges bytes after the agent has authenticated to the selected
   `adb-proxy`.

The agent, rather than the daemon, owns private-backend connection setup and
pair-code authentication.

Requests naming a private serial not present in the caller's registry return
`device not found`. The daemon must never search other tenants as a fallback.

## Shared server devices

Only the shared data plane starts or adopts the real server-local ADB server, on an
internal side port such as `5039`. Per-user agents must not call the current
`LocalAdb::prepare()` path.

The daemon polls the shared backend once and publishes the resulting devices
into every tenant view. Shared-device sessions are routed by the daemon and do
not require a user pair code.

Global ADB host commands need explicit policy:

- `host:kill` must not terminate the common daemon or shared local ADB server;
- daemon shutdown is a separate owner/administrator control operation;
- host-global requests must not be opaquely forwarded to the shared backend
  unless they are explicitly known to be safe;
- device-scoped requests continue to be routed normally.

Shared visibility is not device-operation isolation. If Alice runs `adb reboot`,
installs an APK, changes properties, or modifies forwards on a shared device,
Bob can be affected. Preventing that requires a separate device lease/locking
or command-authorization design and is not part of this proposal.

## Pairing and lifecycle

The existing per-user configuration location remains authoritative. User
commands:

```text
adb-hub pair <addr> <code> [--name <name>]
adb-hub unpair <name>
adb-hub list
adb-hub --daemon          # recommended; same as plain `adb-hub`
adb-hub                    # identical service entry
```

`adb-hubd` remains a compatibility alias for `adb-hub --daemon`. The hidden
`adb-hub agent` subcommand is for advanced/testing use only.

Roles:

- pairing commands update only the caller's configuration;
- the caller's agent reloads that configuration;
- the shared data plane never opens another user's configuration;
- the shared `:5037` listener is a singleton for the network namespace;
- if the listen port is already taken by a compatible hub, a new process joins
  as an agent instead of replacing the listener.

An unprivileged first user may start the shared listener because port 5037 is
not a privileged port. Simultaneous startup races are handled with atomic
socket binding and control-socket connect retries.

For reliable operation across logout and crashes, a platform service manager
is recommended but is not required by the protocol design.

## Security boundary

Per-user agents protect pair codes at rest and avoid central credential
storage. They do not make an arbitrarily malicious shared daemon harmless:
the shared data plane sees and routes users' ADB traffic and can always deny
service.

Two deployment profiles are therefore defined:

- **Cooperative host:** an unprivileged first user may auto-start the shared
  listener via `adb-hub --daemon`. This protects against accidental cross-user
  routing and pair-code disclosure, but the process that owns `:5037` is trusted.
- **Mutually untrusted users:** the shared listener should run under a dedicated
  trusted account or administrator-managed service. Users still retain their
  pair codes in their own agents (started by each user's `adb-hub --daemon`).

Additional requirements:

- user configuration files containing pair codes must be owner-only;
- logs must never contain pair codes;
- agent identity comes from OS credentials, never protocol claims;
- all tenant lookups fail closed;
- per-user connection, stream, memory, and polling limits are required;
- device and backend enable/disable policy remains private to its owning user,
  except for shared-device policy owned by the common daemon.

## Platform support

This table applies to the shared-`:5037` implementation. On unsupported
platforms the same `adb-hub` / `adb-hub --daemon` CLI silently runs the classic
in-process aggregator (one process owns that user's listen port).

| Platform | Shared `5037` multi-user status | Reason / fallback |
| --- | --- | --- |
| Linux, same network namespace | Supported | `NETLINK_SOCK_DIAG` provides the TCP socket owner UID; `AF_UNIX` provides authenticated agent identity. |
| Linux, different network namespaces/containers | Not supported by shared mode | The daemon cannot resolve a client socket outside its network namespace. Run one hub per namespace, or use explicit per-user ports. |
| macOS | Classic aggregator | No portable unprivileged API to resolve the owner UID of an accepted loopback TCP peer. Same `adb-hub` CLI; use one port per user with `ADB_SERVER_SOCKET` if ports conflict. |
| Windows | Classic aggregator | Shared UID routing not implemented. Same `adb-hub` CLI; use one port per user with `ADB_SERVER_SOCKET` if needed. |
| Other Unix/BSD | Classic aggregator | No shared-mode validation. Same CLI fallback. |

Shared mode must not silently expose every user's devices on an unsupported
platform. The classic fallback only serves the calling user's own config.

Fallback when multiple users need isolated ports on a non-shared platform:

```bash
export ADB_SERVER_SOCKET=tcp:127.0.0.1:15037
adb-hub --listen 127.0.0.1:15037 --daemon
adb devices
```

The command syntax remains original ADB, but the endpoint is no longer the
default `5037`.

## Delivery phases

1. Extract the current single-user hub session/router so it can operate against
   a tenant registry. **Done.**
2. Add the Linux daemon, loopback-only listener, and TCP UID resolver. **Done.**
3. Add authenticated per-user agent registration and heartbeat handling. **Done.**
4. Move private backend polling/authentication into the agent protocol. **Done.**
5. Move shared local ADB ownership into the daemon and add safe host-command
   policy. **Done.**
6. Add startup/reconnect behavior and owner-only configuration permissions. **Done.**
7. Unify the CLI (`adb-hub --daemon`) so users do not manage daemon/agent
   separately; unsupported platforms use the classic aggregator. **Done.**
8. Consider native macOS and Windows shared-mode implementations only after
   dedicated security and lifecycle designs are validated. **Open.**

## Acceptance criteria

- Alice sees `shared + Alice`, while Bob sees `shared + Bob`.
- Alice and Bob both use unmodified `adb` against `127.0.0.1:5037`.
- Alice's pair codes never appear in daemon state, protocol payloads, or logs.
- A private serial from another tenant returns `device not found`.
- An agent crash removes only that user's private devices and sessions.
- Shared devices remain visible when a user has no paired backend.
- `adb kill-server` from one user cannot stop the common daemon or shared ADB
  backend.
- Ambiguous/failed UID resolution cannot expose another tenant.
- Per-user device tracking and serial conflict rewriting do not leak changes
  across tenants.
- Unsupported platforms fail with an explicit message and documented fallback.

## Platform API references

- Linux `sock_diag(7)`:
  <https://man7.org/linux/man-pages/man7/sock_diag.7.html>
- Linux `inet_diag` UID population:
  <https://code.googlesource.com/linux/torvalds/linux/+/de758035702576ac0e5ac0f93e3cce77144c3bd3/net/ipv4/inet_diag.c>
- macOS `getpeereid(3)`:
  <https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/getpeereid.3.html>
- Windows `GetExtendedTcpTable`:
  <https://learn.microsoft.com/windows/win32/api/iphlpapi/nf-iphlpapi-getextendedtcptable>
- Windows `WSADuplicateSocket`:
  <https://learn.microsoft.com/windows/win32/api/winsock2/nf-winsock2-wsaduplicatesocketw>
