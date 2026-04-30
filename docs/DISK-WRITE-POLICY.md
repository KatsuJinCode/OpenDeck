# OpenDeck Disk-Write Policy

Effective 2026-04-29. This is the contract that defines what is and is not allowed to write to disk on a recurring basis from this fork.

## Rule

Nothing in OpenDeck or any plugin we own writes to disk on a recurring schedule. The only writes that may happen during normal operation are **event-driven by the user**:

- The user moves a button → profile JSON saved.
- The user changes a plugin setting → that plugin's settings JSON saved.
- The user installs/removes a plugin → plugin assets unpacked / removed.

Anything that writes "every N seconds" or "on every event of type X" is forbidden unless gated by the debug-logging toggle (see `DEBUG-LOGGING.md`) which is **off by default and auto-expires**.

## Why this rule exists

On 2026-04-29 we discovered our system journal had absorbed roughly 45 GB of OpenDeck logs in 8 hours. Two fork-only writers were responsible:

1. A `log::debug!("[set_image] context=… index=…")` line we had added to `src-tauri/src/events/inbound/states.rs` during multi-key debugging (commit `cd1edb2`). It fired on every `set_image` call, which during active key animation can be hundreds per second.
2. A multi-key forwarding chain of `log::info!` calls in the same function — 5–7 lines per child set_image.

The same investigation found a 161 MB `~/.local/share/opendeck/logs/telemetry.jsonl` file that the frontend telemetry library appended to on every event, plus a 22 MB `~/.local/share/opendeck/logs/claude-monitor-rust-telemetry.jsonl` that the claude-monitor plugin wrote one heartbeat to every 10 seconds.

None of these were ever read. They were diagnostic artifacts left behind after the bug they were hunting was fixed.

## What was removed in this cleanup

| Where | What was there | What's there now |
|---|---|---|
| `src-tauri/src/events/inbound/states.rs:133` | `log::debug!("[set_image] context=…")` per-call | gone |
| `src-tauri/src/events/inbound/states.rs:152` | `log::debug!` on update_state error in child path | converted to silent ignore (the parent-forward path covers it) |
| `src-tauri/src/events/inbound/states.rs:157,159,162,165,168,172,188` | `log::info!`/`warn!` chain on every multi-key forward | only the actual error case kept (`forward FAILED`) |
| `src-tauri/src/events/inbound/states.rs:30,102,238,314,371` | `crate::plugin_telemetry::record(...)` per-call | gone |
| `src-tauri/src/plugin_telemetry.rs` | 10-second per-plugin-event-rate reporter | file deleted |
| `src-tauri/src/main.rs` `log_telemetry` Tauri command | append-to-disk for every frontend telemetry sample | gone (the in-memory counters in `src/lib/telemetry.ts` remain) |
| `src-tauri/src/plugins/mod.rs:124-133` | append to `~/.local/state/opendeck/orphan-plugin-telemetry.jsonl` if orphan plugins detected | gone — replaced with `log::warn!` only (which is itself gated by the debug-log toggle) |
| `src/lib/telemetry.ts:188` | `invoke("log_telemetry", ...)` for every frontend snapshot | gone (console emission stays, in-memory counters stay) |
| `claude-code-monitor-linux/rust-port/src/telemetry.rs` | 10-second JSONL heartbeat to `~/.local/share/opendeck/logs/claude-monitor-rust-telemetry.jsonl` | function reduced to a no-op `start_reporter()`; atomic counters retained for any future on-demand dump |

## What still writes to disk, and why

These are upstream and event-driven by the user. Untouched.

| Path | What writes it | When |
|---|---|---|
| `~/.config/opendeck/profiles/<device>/<profile>.json` | `src-tauri/src/store/profiles.rs` (debounced save) | When the user changes button layout, button state, or any per-slot setting |
| `~/.config/opendeck/settings/<plugin>.json` | `src-tauri/src/events/inbound/settings.rs` | When a plugin's `setSettings` is invoked (almost always in response to user action in a Property Inspector) |
| `~/.config/opendeck/settings.json` | `src-tauri/src/store/mod.rs` `Store::save()` | When global settings change (brightness, language, debug-log toggle, etc.) |
| `~/.config/opendeck/plugins/<plugin>/...` | `src-tauri/src/plugins/mod.rs` install path | Once when a plugin is installed |

## What's allowed when debug logging is ON

When the user has explicitly enabled debug logging via the toggle (and only for the chosen window), the upstream `tauri-plugin-log` pipeline is allowed to:

- Write to `~/.local/share/opendeck/logs/opendeck.log` (rotating; bounded by tauri-plugin-log's internal logic).
- Emit to stdout, which systemd captures into the journal.

When debug logging is OFF (default), `log::set_max_level(LevelFilter::Off)` makes every `log::info!`/`debug!`/`warn!`/`error!` macro a no-op at the filter level. The targets are still registered, but nothing reaches them. Disk writes from the log subsystem: zero.

See `DEBUG-LOGGING.md` for how the toggle works.

## How to verify the policy is being honoured

```bash
# 1. With debug logging Off (default), watch /proc IO counters for OpenDeck.
#    Expect write_bytes to stay flat between user actions.
watch -n 5 'cat /proc/$(pgrep opendeck | head -1)/io | grep write_bytes'

# 2. List every regular file under the OpenDeck data dirs and check sizes.
find ~/.local/share/opendeck ~/.config/opendeck -type f -size +1M

# 3. Journal volume from OpenDeck specifically.
journalctl _COMM=opendeck --since '1 hour ago' | wc -l
```

If any of these show growth that doesn't correlate to a user action, a regression has been introduced. Bisect against this doc's commit.

## Restoring something we removed

Everything in the "What was removed" table is recoverable from git history of `local/running` before this cleanup commit. Specifically: search `git log -p --all -S "plugin_telemetry::record"` for the original calls, or `git log -p --all -- src-tauri/src/plugin_telemetry.rs` for the deleted module.

If a future bug hunt genuinely needs continuous instrumentation, add it behind the `--debug-log` toggle (gate it on `effective_mode()` returning anything other than `Off`) so it auto-expires.
