use super::ContextAndPayloadEvent;

use crate::events::frontend::instances::update_state;
use crate::store::profiles::{acquire_locks_mut, debounce_profile_save, get_instance_mut, save_profile};

use serde::Deserialize;

#[derive(Deserialize)]
pub struct SetTitlePayload {
	title: Option<String>,
	state: Option<u16>,
}

#[derive(Deserialize)]
pub struct SetImagePayload {
	image: Option<String>,
	state: Option<u16>,
}

#[derive(Deserialize)]
pub struct SetStatePayload {
	state: u16,
}

pub async fn set_title(event: ContextAndPayloadEvent<SetTitlePayload>) -> Result<(), anyhow::Error> {
	let mut locks = acquire_locks_mut().await;
	let mut skip = false;

	if let Some(instance) = get_instance_mut(&event.context, &mut locks).await? {
		let global_default = crate::store::get_settings().map(|s| s.value.skip_persistence_default).unwrap_or(false);
		skip = instance.skip_persistence.unwrap_or(global_default);

		if let Some(state) = event.payload.state {
			if state as usize >= instance.states.len() {
				return Err(anyhow::anyhow!("State index out of bounds ({} > {})", state, instance.states.len() - 1));
			}

			let text = event.payload.title.unwrap_or(instance.action.states[state as usize].text.clone());
			if instance.states[state as usize].text == text {
				return Ok(());
			}
			instance.states[state as usize].text = text;
		} else {
			if instance
				.states
				.iter()
				.enumerate()
				.all(|(index, state)| state.text == event.payload.title.clone().unwrap_or(instance.action.states[index].text.clone()))
			{
				return Ok(());
			}

			for (index, state) in instance.states.iter_mut().enumerate() {
				state.text = event.payload.title.clone().unwrap_or(instance.action.states[index].text.clone());
			}
		}
		update_state(crate::APP_HANDLE.get().unwrap(), instance.context.clone(), &mut locks).await?;
	}
	if !skip {
		save_profile(&event.context.device, &mut locks).await?;
	}

	Ok(())
}

pub async fn set_image(mut event: ContextAndPayloadEvent<SetImagePayload>) -> Result<(), anyhow::Error> {
	let mut locks = acquire_locks_mut().await;
	let mut skip = false;

	// For child instances (index > 0) with a different controller than the
	// parent (e.g. Keypad child on an Encoder slot), the normal lookup by
	// controller fails. Search all slots' children as fallback.
	let lookup_result = get_instance_mut(&event.context, &mut locks).await;
	let found = match lookup_result {
		Ok(Some(inst)) => Some(inst),
		Ok(None) | Err(_) if event.context.index > 0 => {
			let ctx = &event.context;
			let selected = locks.device_stores.get_selected_profile(&ctx.device)?;
			let device_info = crate::shared::DEVICES.get(&ctx.device).ok_or_else(|| anyhow::anyhow!("device not found"))?;
			let profile = &mut locks.profile_stores.get_profile_store_mut(&device_info, &selected).await?.value;
			let mut result = None;
			for slot in profile.keys.iter_mut().chain(profile.sliders.iter_mut()) {
				if let Some(inst) = slot {
					if let Some(children) = &mut inst.children {
						if let Some(child) = children.iter_mut().find(|c| c.context == *ctx) {
							result = Some(child as &mut crate::shared::ActionInstance);
							break;
						}
					}
				}
			}
			result
		}
		Ok(None) => None,
		Err(e) => {
			log::warn!("[set_image] lookup error for {}: {}", event.context.to_string(), e);
			None
		}
	};
	if let Some(instance) = found {
		let global_default = crate::store::get_settings().map(|s| s.value.skip_persistence_default).unwrap_or(false);
		skip = instance.skip_persistence.unwrap_or(global_default);

		if let Some(image) = &event.payload.image {
			if image.trim().is_empty() {
				event.payload.image = None;
			} else if !image.trim().starts_with("data:") {
				event.payload.image = Some(crate::shared::convert_icon(
					crate::shared::config_dir()
						.join("plugins")
						.join(&instance.action.plugin)
						.join(image.trim())
						.to_str()
						.unwrap()
						.to_owned(),
				));
			}
		}

		if let Some(state) = event.payload.state {
			if state as usize >= instance.states.len() {
				return Err(anyhow::anyhow!("State index out of bounds ({} > {})", state, instance.states.len() - 1));
			}
			instance.states[state as usize].image = event.payload.image.clone().unwrap_or(instance.action.states[state as usize].image.clone());
		} else {
			for (index, state) in instance.states.iter_mut().enumerate() {
				state.image = event.payload.image.clone().unwrap_or(instance.action.states[index].image.clone());
			}
		}
		// Capture child info before releasing the borrow on instance
		let child_notify = if instance.context.index > 0 {
			Some((
				instance.context.clone(),
				instance.states[instance.current_state as usize].image.clone(),
				// Parent is always on Encoder controller. Derive parent position
				// from virtual position formula: child_pos = 100 + parent_pos * 10 + idx
				crate::shared::Context {
					device: instance.context.device.clone(),
					profile: instance.context.profile.clone(),
					controller: "Encoder".to_owned(),
					position: (instance.context.position.saturating_sub(100)) / 10,
				},
			))
		} else {
			None
		};

		// Capture data for a direct device push. Owned values only so the
		// spawned task below doesn't borrow `instance` (which is borrowed
		// from `locks`). For non-child instances only — children (index > 0)
		// are handled by the parent-forward path further down.
		let direct_push_info: Option<(crate::shared::Context, Option<String>)> = if instance.context.index == 0 {
			let ctx: crate::shared::Context = (&instance.context).into();
			let image = instance.states.get(instance.current_state as usize).map(|s| s.image.clone());
			Some((ctx, image))
		} else {
			None
		};

		if let Err(e) = update_state(crate::APP_HANDLE.get().unwrap(), instance.context.clone(), &mut locks).await {
			// Non-fatal for children — parent forward path below still runs.
			let _ = e;
		}

		// Direct device push for keypad slots. Old code relied on the
		// webview's Key.svelte to observe update_state and invoke
		// update_image — which fails when the UI is hidden (--hide) and the
		// Key components don't mount/paint. We mirror the setFeedback fast
		// path: read the selected profile from the already-held locks (NO
		// new lock acquisition — that's what deadlocked the earlier attempt)
		// and tokio::spawn the actual push so it runs after `locks` drops.
		if let Some((ctx, Some(image))) = direct_push_info {
			let selected_profile = locks.device_stores.get_selected_profile(&ctx.device).ok();
			let is_active_native = selected_profile.as_deref() == Some(&ctx.profile);
			tokio::spawn(async move {
				let is_carry_anchor = match &selected_profile {
					Some(active) if !is_active_native => crate::carry::is_anchor_for_active(&ctx.device, active, &ctx.controller, ctx.position, &ctx.profile).await,
					_ => false,
				};
				if !is_active_native && !is_carry_anchor {
					return;
				}
				if let Err(error) = crate::events::outbound::devices::update_image(ctx, Some(image)).await {
					log::warn!("set_image direct device push failed: {}", error);
				}
			});
		}

		// Notify parent plugin with child's image for grid compositing
		if let Some((child_ctx, child_image, parent_context)) = child_notify {
			let forward_info = if let Ok(Some(parent)) = get_instance_mut(&crate::shared::ActionContext::from_context(parent_context, 0), &mut locks).await {
				let child_index = parent.children.as_ref().and_then(|c| c.iter().position(|ch| ch.context == child_ctx));
				child_index.map(|idx| (parent.action.plugin.clone(), parent.action.uuid.clone(), parent.context.to_string(), idx))
			} else {
				None
			};
			drop(locks);

			if let Some((plugin, action_uuid, context_str, idx)) = forward_info {
				let send_result = crate::events::outbound::send_to_plugin(
					&plugin,
					&serde_json::json!({
						"event": "sendToPlugin",
						"action": action_uuid,
						"context": context_str,
						"payload": {
							"childImageUpdate": {
								"index": idx,
								"image": child_image
							}
						}
					}),
				)
				.await;
				if let Err(e) = &send_result {
					log::warn!("[set_image] forward to {} FAILED: {}", plugin, e);
				}
			}
			// Child: locks dropped, skip save, return early
			return Ok(());
		}
	}

	if !skip {
		if let Some(image) = &event.payload.image
			&& image.trim().starts_with("data:")
		{
			debounce_profile_save(event.context);
		} else {
			save_profile(&event.context.device, &mut locks).await?;
		}
	}

	Ok(())
}

/// Per-context last-emit timestamp for throttling the webview preview emit
/// on fast-path (full-canvas) updates. The device LCD is updated directly by
/// the fast path at plugin rate (10Hz for claude-monitor); the webview preview
/// in the OpenDeck GUI window doesn't need that rate — 1Hz is plenty for
/// visualisation and drops ~90% of the webview allocation pressure.
static FAST_PATH_PREVIEW_THROTTLE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> = std::sync::OnceLock::new();
const FAST_PATH_PREVIEW_THROTTLE_MS: u64 = 1000;

fn should_emit_fast_path_preview(context_str: &str) -> bool {
	let map = FAST_PATH_PREVIEW_THROTTLE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
	let mut m = map.lock().unwrap();
	let now = std::time::Instant::now();
	if let Some(last) = m.get(context_str)
		&& now.duration_since(*last) < std::time::Duration::from_millis(FAST_PATH_PREVIEW_THROTTLE_MS)
	{
		return false;
	}
	m.insert(context_str.to_owned(), now);
	true
}

/// Merge a setFeedback payload into the instance's persistent feedback state
/// and notify the frontend to re-render the layout. Keys not present in the
/// current layout are still stored (they have no visual effect, but the spec
/// says unrecognised keys are ignored rather than an error).
pub async fn set_feedback(event: ContextAndPayloadEvent<serde_json::Value>) -> Result<(), anyhow::Error> {
	let mut locks = acquire_locks_mut().await;
	if let Some(instance) = get_instance_mut(&event.context, &mut locks).await? {
		merge_feedback(&mut instance.feedback, event.payload);
		let snapshot = instance.clone();
		drop(locks);
		// Fast path: plugins that pre-render a full-screen 200x100 PNG and
		// push it in `full-canvas` don't need the webview compositor -- the
		// image is already final. Push it straight to the device alongside
		// the frontend notification so preview updates in parallel but the
		// device no longer waits on a webview round-trip (compose + encode +
		// IPC back). Partial-update plugins (title / bar / value / icon)
		// still fall through to the compositor because those payloads need
		// items merged into a layout before reaching device-ready pixels.
		let took_fast_path = snapshot.feedback.get("full-canvas").and_then(|v| v.as_str()).map(|s| s.starts_with("data:")).unwrap_or(false);

		if let Some(full_canvas) = snapshot.feedback.get("full-canvas").and_then(|v| v.as_str())
			&& full_canvas.starts_with("data:")
		{
			let ctx_str = snapshot.context.to_string();
			let _was_warming = crate::events::outbound::will_appear::clear_warming_up(&ctx_str).await;
			// Never skip the push. The old gate dropped the first frame to
			// dedupe with the webview's initial render, but that strands any
			// plugin whose data source can't produce a second update — encoder
			// stays stuck on a stale cached frame. Webview push (when UI is
			// visible) sends identical bytes; double-push is idempotent.
			if false {
				log::info!("[enc-tel] FAST_PATH_SKIP_WARMUP ctx={} (disabled)", ctx_str);
			} else {
				let context = snapshot.context.clone();
				let image = full_canvas.to_owned();
				tokio::spawn(async move {
					let ctx: crate::shared::Context = context.into();
					let selected = crate::store::profiles::DEVICE_STORES.write().await.get_selected_profile(&ctx.device).ok();
					let is_active_native = selected.as_deref() == Some(&ctx.profile);
					// Carry-aware: anchor profile pushes when its slot is being shown in the active profile.
					let is_carry_anchor = match &selected {
						Some(active) => crate::carry::is_anchor_for_active(&ctx.device, active, &ctx.controller, ctx.position, &ctx.profile).await,
						None => false,
					};
					if !is_active_native && !is_carry_anchor {
						log::info!("[enc-tel] FAST_PATH_BLOCKED_PROFILE ctx_profile={} selected={:?}", ctx.profile, selected);
						return;
					}
					log::info!("[enc-tel] FAST_PATH_PUSH pos={}", ctx.position);
					if let Err(error) = crate::events::outbound::devices::update_image(ctx, Some(image)).await {
						log::warn!("Failed to fast-path full-canvas image: {}", error);
					}
				});
			}
		}

		// Webview preview emit throttle. For fast-path full-canvas pushes, the
		// device LCD is already updated directly at plugin rate. The webview
		// preview in the OpenDeck GUI window doesn't need that rate — throttle
		// to 1Hz per context to drop allocation pressure on the webview.
		// Non-fast-path updates (partial: title/bar/indicator) still emit at
		// every change because the webview compositor owns the device update.
		let should_emit = if took_fast_path { should_emit_fast_path_preview(&snapshot.context.to_string()) } else { true };
		if should_emit {
			emit_feedback_changed(&snapshot);
		}
	}
	Ok(())
}

/// Handle setFeedbackLayout: record the new layout, reset accumulated feedback
/// state (spec-level keys differ per layout so stale state would be wrong),
/// and notify the frontend.
pub async fn set_feedback_layout(event: ContextAndPayloadEvent<serde_json::Value>) -> Result<(), anyhow::Error> {
	{
		let mut locks = acquire_locks_mut().await;
		let _ = get_instance_mut(&event.context, &mut locks).await?;
	}
	let layout_id = match &event.payload {
		serde_json::Value::String(s) => s.clone(),
		serde_json::Value::Object(obj) => obj.get("layout").and_then(|v| v.as_str()).map(str::to_owned).unwrap_or_default(),
		_ => return Ok(()),
	};
	if layout_id.is_empty() {
		return Ok(());
	}
	let mut locks = acquire_locks_mut().await;
	if let Some(instance) = get_instance_mut(&event.context, &mut locks).await? {
		instance.feedback_layout = Some(layout_id);
		instance.feedback = serde_json::Value::Null;
		let snapshot = instance.clone();
		drop(locks);
		emit_feedback_changed(&snapshot);
	}
	Ok(())
}

fn merge_feedback(target: &mut serde_json::Value, patch: serde_json::Value) {
	if target.is_null() {
		*target = serde_json::Value::Object(serde_json::Map::new());
	}
	let serde_json::Value::Object(target_obj) = target else { return };
	let serde_json::Value::Object(patch_obj) = patch else { return };
	for (key, value) in patch_obj {
		target_obj.insert(key, value);
	}
}

fn emit_feedback_changed(instance: &crate::shared::ActionInstance) {
	use tauri::{Emitter, Manager};
	let Some(app) = crate::APP_HANDLE.get() else { return };
	let Some(window) = app.get_webview_window("main") else { return };
	// Per-context event name so Tauri delivers only to the specific Key that owns
	// this context, instead of broadcasting to every Key listener. Dramatically
	// cuts allocation pressure — N-listener × payload-size reduces to 1 × payload.
	// Tauri event names disallow dots, so replace context's "." separators with ":".
	let event_name = format!("feedback_changed::{}", instance.context.to_string().replace('.', ":"));
	let _ = window.emit(
		&event_name,
		serde_json::json!({
			"context": instance.context.to_string(),
			"plugin": instance.action.plugin,
			"layout": instance.feedback_layout,
			"feedback": instance.feedback,
		}),
	);
}

pub async fn set_state(event: ContextAndPayloadEvent<SetStatePayload>) -> Result<(), anyhow::Error> {
	let mut locks = acquire_locks_mut().await;

	if let Some(instance) = get_instance_mut(&event.context, &mut locks).await? {
		if event.payload.state >= instance.states.len() as u16 {
			return Ok(());
		}
		instance.current_state = event.payload.state;
		update_state(crate::APP_HANDLE.get().unwrap(), instance.context.clone(), &mut locks).await?;
	}
	save_profile(&event.context.device, &mut locks).await?;

	Ok(())
}
