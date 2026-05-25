//! Debug helper — dump the current encoder/key preview state to PNG files on
//! disk. Two ways to trigger:
//!
//!   1. Tauri command `dump_preview_state(out_dir)` (callable from the webview).
//!   2. Touch the file `/tmp/opendeck-dump.trigger`; a background watcher
//!      polls every 250ms, runs the dump into `/tmp/opendeck-preview/`, and
//!      removes the trigger. CLIs (and external agents) use this path so
//!      they don't need to talk Tauri IPC.
//!
//! Each rendered slot in any device/profile that has `feedback["full-canvas"]`
//! set to a `data:image/png;base64,...` URI is decoded and written as
//! `{device}__{profile}__{Encoder|Keypad}_{position}.png`. Slots without a
//! cached full-canvas (most key actions, encoders that haven't pushed yet)
//! are skipped.
//!
//! This is the canonical "what does OpenDeck currently know it should be
//! showing" snapshot. It does NOT capture the webview's actual rendered
//! pixels — for that, take a window screenshot. But it captures exactly
//! what plugins pushed, which is what the device LCD displays via the
//! full-canvas fast path. Discrepancies between this and the device output
//! point at OpenDeck's outbound side; discrepancies between this and the
//! webview preview tile point at OpenDeck's frontend rendering.

use base64::Engine;
use std::path::{Path, PathBuf};

const TRIGGER_PATH: &str = "/tmp/opendeck-dump.trigger";
const DEFAULT_OUT_DIR: &str = "/tmp/opendeck-preview";
const DATA_URI_PREFIX: &str = "data:image/png;base64,";

#[tauri::command]
pub async fn dump_preview_state(out_dir: String) -> Result<Vec<String>, String> {
	dump_to(&PathBuf::from(out_dir)).await.map_err(|e| e.to_string())
}

/// Return the current in-memory `feedback_layout` and `feedback` for one slot.
/// Called from the frontend on Key.svelte mount so the preview canvas seeds
/// from the live backend state instead of the disk-persisted slot data
/// (which doesn't carry feedback). Without this, after a profile switch the
/// new Key.svelte mounts with empty feedback, paints the empty $A0 layout
/// (= a black panel), and waits for the next setFeedback that may never
/// come because the plugin caches state and only pushes on change.
#[derive(serde::Serialize)]
pub struct SlotFeedback {
	pub layout: Option<String>,
	pub feedback: serde_json::Value,
}

#[tauri::command]
pub async fn get_slot_feedback(context: crate::shared::Context) -> Result<Option<SlotFeedback>, String> {
	let locks = crate::store::profiles::acquire_locks().await;
	let slot = crate::store::profiles::get_slot(&context, &locks).await.map_err(|e| e.to_string())?;
	if let Some(inst) = slot {
		Ok(Some(SlotFeedback {
			layout: inst.feedback_layout.clone(),
			feedback: serde_json::to_value(&inst.feedback).unwrap_or(serde_json::Value::Null),
		}))
	} else {
		Ok(None)
	}
}

async fn dump_to(dir: &Path) -> Result<Vec<String>, anyhow::Error> {
	std::fs::create_dir_all(dir)?;
	let locks = crate::store::profiles::acquire_locks().await;
	let mut written = Vec::new();
	for (canonical_id, store) in locks.profile_stores.iter_all() {
		// canonical_id has the form "device/profile" (or backslash on Windows).
		let safe_id = canonical_id.replace('/', "__").replace('\\', "__");
		for (i, slot) in store.value.keys.iter().enumerate() {
			if let Some(inst) = slot {
				if let Some(name) = write_slot(dir, &safe_id, "Keypad", i, inst)? {
					written.push(name);
				}
			}
		}
		for (i, slot) in store.value.sliders.iter().enumerate() {
			if let Some(inst) = slot {
				if let Some(name) = write_slot(dir, &safe_id, "Encoder", i, inst)? {
					written.push(name);
				}
			}
		}
	}
	Ok(written)
}

fn write_slot(dir: &Path, id_prefix: &str, controller: &str, position: usize, inst: &crate::shared::ActionInstance) -> Result<Option<String>, anyhow::Error> {
	// Encoders carry their rendered pixmap in feedback["full-canvas"] (set
	// via setFeedback). Keys carry it in states[current_state].image (set
	// via setImage). Try full-canvas first, then fall back to the current
	// state image. Either way the resulting PNG is what OpenDeck believes
	// the slot is currently showing.
	let uri = inst.feedback.get("full-canvas").and_then(|v| v.as_str()).or_else(|| {
		let idx = inst.current_state as usize;
		inst.states.get(idx).map(|s| s.image.as_str())
	});
	let Some(uri) = uri else { return Ok(None) };
	if !uri.starts_with(DATA_URI_PREFIX) {
		// Manifest-relative path or empty — not a base64 PNG we can dump.
		return Ok(None);
	}
	let b64 = &uri[DATA_URI_PREFIX.len()..];
	let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
	let fname = format!("{id_prefix}__{controller}_{position}.png");
	std::fs::write(dir.join(&fname), bytes)?;
	Ok(Some(fname))
}

/// Spawn the file-trigger watcher. Runs forever; cheap (one stat() every
/// 250ms). Logs to opendeck.log on each dump.
pub fn spawn_trigger_watcher() {
	tokio::spawn(async move {
		let trigger = PathBuf::from(TRIGGER_PATH);
		let out_dir = PathBuf::from(DEFAULT_OUT_DIR);
		loop {
			tokio::time::sleep(std::time::Duration::from_millis(250)).await;
			if !trigger.exists() {
				continue;
			}
			match dump_to(&out_dir).await {
				Ok(names) => {
					log::info!("[dump] wrote {} slot(s) to {}", names.len(), out_dir.display());
				}
				Err(e) => {
					log::warn!("[dump] failed: {e}");
				}
			}
			let _ = std::fs::remove_file(&trigger);
		}
	});
}
