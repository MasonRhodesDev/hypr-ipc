# hypr-ipc

Fail-closed Hyprland instance discovery, socket2 events, and hyprctl for the
hypr suite.

Discovery order:

1. `$HYPRLAND_INSTANCE_SIGNATURE` when `$XDG_RUNTIME_DIR/hypr/$HIS/.socket2.sock` exists
2. Scan `$XDG_RUNTIME_DIR/hypr/*/hyprland.lock` for a PID whose `/proc/<pid>/comm` is `Hyprland` and whose `.socket2.sock` exists

There is no `/run/user/<uid>` fallback. Runtime comes from `hypr-paths`.

Listen on `.socket2.sock` (`event>>payload` lines). Reconnect by re-resolving
the instance; suggested backoff is 2s.

`hyprctl` success for mutating commands is stdout exactly `ok`. JSON helpers
parse stdout. Lua dialect is a boolean: `hyprland.lua` exists under the XDG
hypr config dir.

Published from CI on a `v*` tag (`CARGO_REGISTRY_TOKEN` org secret). Do not
`cargo publish` from a laptop.

```toml
hypr-ipc = "0.1.0"
```
