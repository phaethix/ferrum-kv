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
  over the RESP2 protocol.
- Data corruption or loss not covered by documented AOF durability
  semantics.
- Memory-safety issues (in practice `unsafe` is forbidden in the
  codebase; any `unsafe` block is itself a report).
- Authentication/authorization bypass (currently FerrumKV has no auth —
  see below).

Out of scope:

- The intentional lack of authentication / TLS. FerrumKV is designed to
  run behind a trusted network boundary, like a single-node Redis
  without `requirepass`. If you need auth or TLS, open a feature
  request.
- Resource-exhaustion attacks from clients you have already admitted
  beyond the documented `max-connections` cap.
- Issues in upstream dependencies — report those to the upstream crate.

## Hardening Notes for Operators

FerrumKV does not authenticate clients and does not support TLS. Bind it
to a loopback or private interface only, and firewall the listening port
(`127.0.0.1:6380` by default). Treat any client that can reach the port
as fully trusted.
