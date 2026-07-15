# Dashboard

FerrumKV ships with a built-in web dashboard — no extra dependencies, no separate
service to deploy. It listens on `:6381` by default (override with `--dashboard-addr`,
or set it to `off` to disable).

## Features

| Feature | Description |
|---------|-------------|
| Key browser | Glob search (`user:*`, `session?`), paginated list, TTL badges |
| Inline editor | View, edit, set TTL, and delete keys in the browser |
| Live stats | Keys, memory, hit ratio, eviction policy — auto-refresh every 2s |
| Command console | Run any RESP command (`GET`, `SET`, `INFO`, …) with syntax highlighting & autocomplete |
| Namespace overview | Empty selection shows a clickable grid of key namespaces |
| Theme toggle | Switch between a dark theme and a macOS-native light theme (remembered per browser) |

## Opening it

```bash
# default address is 127.0.0.1:6381
open http://127.0.0.1:6381
```

The dashboard shares the same `KvEngine` instance as the RESP server, so changes made
in the UI are immediately visible to connected clients.
