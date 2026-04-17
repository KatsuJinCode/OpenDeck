// Plugin telemetry — per-plugin per-event-type counters, reported every 10s.
// Added to find the plugin hammering the frontend at 360 feedback_changed/sec
// when the designed max is 10Hz and most monitors push every 3-5 seconds.
//
// Usage:
//   plugin_telemetry::record("set_feedback", plugin_uuid);
// Every 10s the reporter dumps aggregate counts to the log:
//   [plugin-rate] event=set_feedback plugin=com.jw.system-monitor count=120 rate=12.0/sec
// Tail the opendeck log and grep for [plugin-rate] to see which plugin hammers.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

static COUNTERS: OnceLock<Mutex<HashMap<(String, String), u64>>> = OnceLock::new();

fn counters() -> &'static Mutex<HashMap<(String, String), u64>> {
	COUNTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record one event of `event_type` coming from `plugin_uuid`.
pub fn record(event_type: &str, plugin_uuid: &str) {
	let key = (event_type.to_owned(), plugin_uuid.to_owned());
	*counters().lock().unwrap().entry(key).or_insert(0) += 1;
}

/// Spawn a tokio task that dumps the counters every 10 seconds and resets.
/// Only events with count > 0 in the last window are logged.
pub fn start_reporter() {
	tokio::spawn(async {
		let mut ticker = tokio::time::interval(Duration::from_secs(10));
		ticker.tick().await; // discard initial immediate tick
		loop {
			ticker.tick().await;
			let snapshot = std::mem::take(&mut *counters().lock().unwrap());
			if snapshot.is_empty() {
				continue;
			}
			let mut rows: Vec<((String, String), u64)> = snapshot.into_iter().collect();
			rows.sort_by(|a, b| b.1.cmp(&a.1));
			for ((event_type, plugin), count) in rows {
				log::info!(
					"[plugin-rate] event={} plugin={} count={} rate={:.1}/sec",
					event_type,
					plugin,
					count,
					count as f64 / 10.0
				);
			}
		}
	});
}
