# OpenDeck Debug Logging

## Default state: Off

When OpenDeck starts, the log subsystem is gagged. `log::set_max_level(LevelFilter::Off)` is set during `setup`, so every call to `log::info!`, `log::debug!`, `log::trace!`, `log::warn!`, and `log::error!` becomes a no-op at the filter level. tauri-plugin-log's stdout target still exists; the journal still scrapes stdout if anything bypasses `log::*`; but no log macro reaches them. Net disk write from the log subsystem: zero.

Why: see `DISK-WRITE-POLICY.md`. Short version: a fork-only `[set_image]` debug log produced 45 GB of system journal data in 8 hours.

## Three states

| State | Effective level | Auto-disables? |
|---|---|---|
| `off` (default) | `Off` | n/a |
| `1h` / `4h` / `24h` | `Trace` | yes, after the chosen duration |
| `permanent` | `Trace` | **no** — has to be turned off manually |

Choosing `permanent` prints a stderr warning at startup so you can't forget about it.

## Two ways to turn it on

### A. Settings UI (persistent)

Settings → "Debug logging" dropdown.

The choice is written to `~/.config/opendeck/settings.json` as:

```json
{
  "debug_log_until_ts": 1717900000,   // unix seconds; null = no window
  "debug_log_permanent": false        // true = never auto-expire
}
```

The change applies immediately (`debug_log_gate::reapply()`). The expiry watcher runs every 60 seconds; when it sees `now >= debug_log_until_ts` it sets the level back to Off and clears the timestamp from settings. This survives restarts because the timestamp is the source of truth — a crash mid-window doesn't reset it.

### B. CLI override (this run only)

```bash
opendeck --debug-log=4h
opendeck --debug-log=permanent
opendeck --debug-log=off
```

The CLI override is captured at startup and is the only signal `debug_log_gate` looks at if present. It does **not** mutate persisted settings — when OpenDeck next starts without the flag, the persisted settings take over again.

## Where logs go when On

Both targets are configured by upstream `tauri-plugin-log`:

- **`~/.local/share/opendeck/logs/opendeck.log`** — rotating file. tauri-plugin-log handles rotation; you don't have to.
- **stdout** → systemd journal. Inspect with `journalctl _COMM=opendeck --since '15 minutes ago'`.

Steady-state rate when On (with the spam removed in commit that introduced this doc) is on the order of **kilobytes per minute** during active use. The 45 GB / 8 h scenario only happened because of unconditional per-event debug logs — those are gone.

## How to read the logs

```bash
# Live tail of journal output.
journalctl -f _COMM=opendeck

# Live tail of the on-disk file.
tail -F ~/.local/share/opendeck/logs/opendeck.log

# Filter by source module.
journalctl _COMM=opendeck --since '15 minutes ago' | grep 'opendeck::events::'

# Find error-class entries.
journalctl _COMM=opendeck -p warning --since today
```

## Forcibly disable if the UI is broken

If the toggle dropdown isn't responsive (e.g. the frontend won't load), you can disable logging by editing the settings JSON directly:

```bash
# 1. Quit OpenDeck (SIGTERM, see ../CLAUDE.md).
pkill opendeck && sleep 4
# 2. Clear both fields.
python3 -c "
import json, pathlib
p = pathlib.Path.home()/'.config/opendeck/settings.json'
d = json.loads(p.read_text())
d.pop('debug_log_until_ts', None)
d['debug_log_permanent'] = False
p.write_text(json.dumps(d, indent=2))
"
# 3. Start OpenDeck. It will read the cleaned settings and start in Off.
/usr/bin/opendeck
```

You can also pass `--debug-log=off` as a one-shot guarantee for that run.

## What logs when On (with the spam removed)

After the 2026-04-29 cleanup, the only `log::*` calls in the fork's hot paths are:

- `log::warn!` on lookup errors and forward failures in `set_image` (rare).
- `log::warn!` for orphan-plugin detection (rare; only fires if PDEATHSIG regresses).
- Encoder telemetry `log::info!` lines (`[enc-tel] FAST_PATH_*`) which fire on first-render conditions and profile-blocked sends, not per frame.
- Whatever upstream logs at `Info` and above.

If you find a per-frame log macro in our fork: it's a regression of this policy. See `DISK-WRITE-POLICY.md`.

## Implementation pointers

| File | Purpose |
|---|---|
| `src-tauri/src/debug_log_gate.rs` | Module entry: `capture_cli_override`, `apply_initial_state`, `reapply`, `effective_mode`, expiry watcher task. |
| `src-tauri/src/main.rs` | Calls `capture_cli_override()` before `Builder::default()` and `apply_initial_state()` inside the tauri `setup` closure. |
| `src-tauri/src/events/frontend/settings.rs` | `set_debug_log_window(mode)` Tauri command — what the dropdown invokes. |
| `src-tauri/src/store/mod.rs` | `Settings.debug_log_until_ts` and `.debug_log_permanent` fields. |
| `src/components/SettingsView.svelte` | The dropdown. |
| `src/lib/settings.ts` | TS types matching the Rust struct. |
