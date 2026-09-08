# Bypass Tool - Route Overlay + Adapter Reconfig

Rust workspace per README.md. Two binaries: `bypass-core` (Windows service, SYSTEM) and `bypass-ui` (tray + slint window, user perms), IPC over named pipe JSON-RPC.

## Build

```powershell
# Path setup (needed in every new shell)
$env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path

cargo build              # dev
cargo build --release    # release
cargo clippy --workspace --all-targets
cargo test  --workspace
```

MSVC Build Tools are installed (VS2022 BuildTools, VC.Tools.x86.x64).

## Architecture (from README.md §9)

```
crates/
  core-lib/       # traits: SwitchStrategy, NetInspector; health monitor state machine; state store
  core-win/       # windows-rs impls: IP Helper (Create/DeleteIpForwardEntry2, GetIpForwardTable2),
                  #   adapter reconfig (netsh static IP, DNS), ICMP ping, adapter listing
  core-bin/       # bypass-core.exe: windows-service host + tokio named-pipe IPC server (hardened pipe DACL)
  ipc-protocol/   # serde types: requests/responses/events (GetStatus, EnableBypass, HealthEvent push...)
  ui-bin/         # bypass-ui.exe: tray-icon + tao + slint settings window; toast notifications
installer/        # Inno Setup .iss (runs bypass-core.exe --install-service during install)
.github/workflows/release.yml   # tag v* -> build -> iscc -> gh-release
```

## Conventions

- Async: tokio everywhere in core; `async_trait` for the strategy traits.
- Persistence: `%ProgramData%\RouteTool\config.json` + `runtime_state.json`, temp-file+fsync+rename atomic writes.
- Pipe name `\\.\pipe\RouteToolCore`; line-delimited JSON-RPC 2.0, server pushes events after `SubscribeEvents`; pipe DACL restricted to SYSTEM/Administrators/Authenticated Users.
- Log: tracing + tracing-appender daily rolling, 7 days retention (`core_lib::log_prune`), `%ProgramData%\RouteTool\logs\`.
- IPv4 only (MVP); no IPv6.
- RouteOverlay adds `0.0.0.0/1` + `128.0.0.0/1` via bypass_ip (longest-prefix-match wins; not metric-based).
- UI is optional by design — core must run and auto-fallback with UI closed.
- Health monitor: ping bypass_ip every N secs (default 5), 3 consecutive failures -> disable strategy + emit AutoFallbackTriggered.
- cargo fmt style; errors via thiserror; no anyhow in library crates (bin crates may use it).
