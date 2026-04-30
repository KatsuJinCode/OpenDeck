//! Debug-log gate. Default state: Off. When Off, `log::set_max_level(Off)`
//! makes every `log::info!`/`debug!`/`trace!`/`warn!`/`error!` macro a no-op
//! at the filter level — nothing reaches stdout (journald) or the log file.
//!
//! Three modes, persisted in [`crate::store::Settings`]:
//!   * Off (default after install or after expiry).
//!   * Time-windowed via `debug_log_until_ts: Option<u64>` (Unix seconds).
//!   * Permanent via `debug_log_permanent: bool`.
//!
//! The CLI flag `--debug-log=<1h|4h|24h|permanent|off>` overrides the
//! persisted choice for this run only and DOES NOT mutate settings.
//!
//! Why: the 2026-04-29 incident. A fork-only `log::debug!` in `set_image`
//! plus an info-level multi-key forwarding chain produced 45 GB of journal
//! data in 8 hours. With this gate, even if a future contributor adds a
//! noisy log macro, the log subsystem is gagged unless someone opts in.
//! See docs/DEBUG-LOGGING.md for the full contract.

use log::LevelFilter;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// CLI override captured at startup. None = follow settings.
static CLI_OVERRIDE: OnceLock<Option<RuntimeMode>> = OnceLock::new();

#[derive(Clone, Copy, Debug)]
enum RuntimeMode {
	Off,
	UntilTs(u64),
	Permanent,
}

/// Parse `--debug-log=<value>` from argv. Recognised values:
///   * `off` → force off this run
///   * `1h`, `4h`, `24h` → on for that duration this run
///   * `permanent` → on for this run (no auto-expire)
///   * anything else → ignored, settings used
pub fn capture_cli_override() {
	let mut mode: Option<RuntimeMode> = None;
	for arg in std::env::args() {
		if let Some(value) = arg.strip_prefix("--debug-log=") {
			let now = now_secs();
			mode = match value {
				"off" => Some(RuntimeMode::Off),
				"1h" => Some(RuntimeMode::UntilTs(now + 3600)),
				"4h" => Some(RuntimeMode::UntilTs(now + 4 * 3600)),
				"24h" => Some(RuntimeMode::UntilTs(now + 24 * 3600)),
				"permanent" => Some(RuntimeMode::Permanent),
				_ => None,
			};
			break;
		}
	}
	let _ = CLI_OVERRIDE.set(mode);
}

/// Compute and apply the effective log level for the current state.
/// Called at startup (after tauri-plugin-log builder finishes) and any time
/// the persisted setting changes (via [`reapply`]).
pub fn apply_initial_state() {
	apply(effective_mode());
	spawn_expiry_watcher();
}

/// Re-evaluate after a settings change.
pub fn reapply() {
	apply(effective_mode());
}

fn effective_mode() -> RuntimeMode {
	if let Some(Some(cli)) = CLI_OVERRIDE.get() {
		return *cli;
	}
	let Ok(store) = crate::store::get_settings() else {
		return RuntimeMode::Off;
	};
	if store.value.debug_log_permanent {
		return RuntimeMode::Permanent;
	}
	match store.value.debug_log_until_ts {
		Some(ts) if ts > now_secs() => RuntimeMode::UntilTs(ts),
		_ => RuntimeMode::Off,
	}
}

fn apply(mode: RuntimeMode) {
	match mode {
		RuntimeMode::Off => {
			log::set_max_level(LevelFilter::Off);
		},
		RuntimeMode::Permanent => {
			log::set_max_level(LevelFilter::Trace);
			eprintln!("[opendeck] debug logging is PERMANENT — disk writes ongoing. Disable in Settings → Logging.");
		},
		RuntimeMode::UntilTs(ts) => {
			log::set_max_level(LevelFilter::Trace);
			let remaining = ts.saturating_sub(now_secs());
			eprintln!("[opendeck] debug logging ON for {remaining}s. Auto-disable at unix ts {ts}.");
		},
	}
}

/// Background task: every 60 s, if the persisted window has expired, flip the
/// global level back to Off and clear the timestamp from settings. Idempotent
/// and survives restarts (the timestamp itself is the source of truth).
fn spawn_expiry_watcher() {
	tokio::spawn(async {
		let mut ticker = tokio::time::interval(Duration::from_secs(60));
		ticker.tick().await; // discard immediate fire
		loop {
			ticker.tick().await;
			// CLI override never expires.
			if matches!(CLI_OVERRIDE.get(), Some(Some(_))) {
				continue;
			}
			let Ok(mut store) = crate::store::get_settings() else { continue };
			if store.value.debug_log_permanent {
				continue;
			}
			if let Some(ts) = store.value.debug_log_until_ts {
				if ts <= now_secs() {
					store.value.debug_log_until_ts = None;
					if let Err(e) = store.save() {
						eprintln!("[opendeck] debug-log expiry: failed to persist clear: {e}");
					}
					log::set_max_level(LevelFilter::Off);
					eprintln!("[opendeck] debug logging window expired — back to Off.");
				}
			}
		}
	});
}

fn now_secs() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
