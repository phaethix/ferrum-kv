# Security Policy

## Supported Versions

FerrumKV is early-stage software (`0.x`). Security fixes are applied to
the latest `master` branch and the most recent release tag only. There
are no separate backport branches yet.

| Version | Supported |
|---|---|
| latest `master` | ✅ |
| latest release tag | ✅ |
| older tags | ❌ |

## Reporting a Vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

Please report suspected vulnerabilities privately by emailing
**security@phaethix.dev**. Include:

1. A description of the issue and its potential impact.
2. The FerrumKV version or git commit you tested against.
3. A minimal reproduction (server start flags + `redis-cli` commands, or
   a short script).
4. Any known mitigations.

You should receive an acknowledgement within **72 hours**. We will work
with you to validate the report, agree on a fix timeline, and coordinate
public disclosure once a release patches the issue. Please do not
disclose the vulnerability publicly before a fix is released.

## Scope

In scope:

- Crashes, panics, or undefined behaviour reachable from client input
  over the RESP protocol (RESP2, or RESP3 after `HELLO 3`).
- Data corruption or loss not covered by documented AOF durability
  semantics.
- Memory-safety issues (in practice `unsafe` is forbidden in the
  codebase; the only exception is the documented `libc::listen` call used
  to size the accept queue).
- Authentication or authorization bypass: `AUTH` gate, `--requirepass`
  handling, or the dashboard token / origin checks.
- Secret disclosure through a reply or command output (`CONFIG GET`,
  `MONITOR`, `SLOWLOG`, `INFO`).

Out of scope:

- The intentional lack of TLS. Terminate TLS in front of FerrumKV
  (stunnel, Envoy, a cloud load balancer) and keep the listening socket
  on a private interface. See "Transport security" below.
- The intentional lack of per-user ACLs. FerrumKV supports a single
  shared `requirepass`; multi-tenant isolation is a roadmap item
  (`v0.9`), not a bug.
- Resource-exhaustion attacks from clients you have already admitted
  beyond the documented `maxclients` cap and request-size limits.
- Issues in upstream dependencies — report those to the upstream crate.

## Authentication

Authentication is opt-in and mirrors Redis' `requirepass`:

- `--requirepass <secret>` (or the `requirepass` config directive, or
  `CONFIG SET requirepass <secret>`) gates every command behind `AUTH`.
- The comparison is constant-time; the password is never echoed back
  (`CONFIG GET requirepass` returns `(redacted)`), never broadcast to
  `MONITOR` clients, and never written to the slow log.
- `CONFIG SET requirepass ""` disables authentication, matching Redis.
- There is no per-user model, so any authenticated client is fully
  privileged. Run one FerrumKV instance per trust boundary.

Passwords supplied on the command line are visible in the process table;
prefer the configuration file (mode `0600`) or a process supervisor that
injects them at runtime.

## Dashboard

The built-in dashboard (`--dashboard-addr`, `:6381` by default) can read
and write every key, so it is treated as an administrative surface:

- It binds to loopback by default. Binding it to a non-loopback address
  **requires** `--dashboard-token <secret>`; otherwise the dashboard
  refuses to start and logs an error.
- With a token configured, every `/api/*` request must present it
  (`X-Ferrum-Token` or `Authorization: Bearer`), compared in constant
  time. The token is independent of `requirepass`.
- Without a token, state-changing requests are additionally checked
  against `Origin`/`Host` so a malicious page cannot drive the dashboard
  through the user's browser (CSRF).
- Requests are bounded (64 KiB headers, 1 MiB body, 10 s read timeout)
  and responses carry `X-Content-Type-Options: nosniff`,
  `X-Frame-Options: DENY` and `Cache-Control: no-store`.
- Binary keys and values are shown read-only; the UI round-trips the
  original bytes instead of re-encoding a lossy UTF-8 string.

## Transport security

FerrumKV speaks plain TCP. This is a deliberate scope decision, not an
oversight: a from-scratch single-node store should not grow its own
certificate lifecycle, and the ecosystem already solves it well.

Recommended deployment:

```text
client --TLS--> stunnel / Envoy / cloud LB --loopback--> ferrum-kv
```

If you must expose the port directly, bind to a private interface, keep
`requirepass` enabled, and firewall everything else. Do not run the
dashboard on the same interface without a token.

## Hardening Notes for Operators

- Bind to loopback or a private interface (`127.0.0.1:6380` by default).
- Set `requirepass` whenever more than one person (or process) can reach
  the port, and set `--dashboard-token` whenever the dashboard is not on
  loopback.
- Keep `maxclients`, `--client-timeout` and `maxmemory` bounded; each
  connection may buffer up to 17 MiB while a frame is incomplete.
- `appendfsync always` is the only mode that survives a crash without
  losing acknowledged writes. `everysec` can lose up to one second;
  `no` can lose arbitrarily much. Check `INFO stats:aof_write_errors`
  and `aof_fsyncs` to confirm the log is actually reaching the disk.
- Treat any client that can reach the port as fully trusted.
