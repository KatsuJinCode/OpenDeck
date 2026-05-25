//! Live disk-write rate for the OpenDeck process. Reads `/proc/self/io` —
//! cheap (a single read syscall) and does not itself write to disk.
//!
//! The frontend polls `get_disk_write_rate` every couple of seconds and
//! displays the rate next to the persistence-control settings, so the user
//! can correlate plugin activity to physical disk wear.

use std::sync::Mutex;
use std::time::Instant;

struct Sample {
	at: Instant,
	write_bytes: u64,
}

static LAST: Mutex<Option<Sample>> = Mutex::new(None);

#[derive(serde::Serialize)]
pub struct DiskWriteRate {
	/// Disk-write throughput in bytes per second, averaged across the
	/// interval since the previous call. `None` on the first call (no
	/// previous sample yet) or on platforms without `/proc/self/io`.
	pub bytes_per_sec: Option<u64>,
	/// Cumulative `write_bytes` from `/proc/self/io`. Useful as a sanity
	/// check; an unchanged value across calls means zero disk I/O.
	pub cumulative_bytes: u64,
}

#[cfg(target_os = "linux")]
fn read_write_bytes() -> Option<u64> {
	let s = std::fs::read_to_string("/proc/self/io").ok()?;
	for line in s.lines() {
		if let Some(rest) = line.strip_prefix("write_bytes:") {
			return rest.trim().parse().ok();
		}
	}
	None
}

#[cfg(not(target_os = "linux"))]
fn read_write_bytes() -> Option<u64> {
	None
}

#[tauri::command]
pub fn get_disk_write_rate() -> DiskWriteRate {
	let Some(now_bytes) = read_write_bytes() else {
		return DiskWriteRate {
			bytes_per_sec: None,
			cumulative_bytes: 0,
		};
	};
	let now = Instant::now();
	let mut guard = LAST.lock().unwrap();
	let rate = guard.as_ref().and_then(|prev| {
		let dt = now.duration_since(prev.at).as_secs_f64();
		if dt < 0.001 {
			return None;
		}
		let delta = now_bytes.saturating_sub(prev.write_bytes);
		Some((delta as f64 / dt) as u64)
	});
	*guard = Some(Sample { at: now, write_bytes: now_bytes });
	DiskWriteRate {
		bytes_per_sec: rate,
		cumulative_bytes: now_bytes,
	}
}
