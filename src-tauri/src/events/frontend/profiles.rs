use super::Error;

use crate::shared::DEVICES;
use crate::store::profiles::{PROFILE_SAVE_DEBOUNCE, PROFILE_STORES, acquire_locks_mut, get_device_profiles, save_profile};

use tauri::{AppHandle, Emitter, Manager, command};

#[command]
pub fn get_profiles(device: &str) -> Result<Vec<String>, Error> {
	Ok(get_device_profiles(device)?)
}

#[command]
pub async fn get_selected_profile(device: String) -> Result<crate::shared::Profile, Error> {
	let mut locks = acquire_locks_mut().await;
	if !DEVICES.contains_key(&device) {
		return Err(Error::new(format!("device {device} not found")));
	}

	let selected_profile = locks.device_stores.get_selected_profile(&device)?;
	let profile = locks.profile_stores.get_profile_store(&DEVICES.get(&device).unwrap(), &selected_profile)?;

	Ok(profile.value.clone())
}

#[allow(clippy::flat_map_identity)]
#[command]
pub async fn set_selected_profile(device: String, id: String) -> Result<(), Error> {
	let mut locks = acquire_locks_mut().await;
	if !DEVICES.contains_key(&device) {
		return Err(Error::new(format!("device {device} not found")));
	}

	// If a profile save is pending for this device, save it immediately to prevent losing profile data
	let entries = PROFILE_SAVE_DEBOUNCE
		.iter()
		.filter(|entry| entry.key().device == device)
		.map(|entry| entry.key().clone())
		.collect::<Vec<_>>();
	if !entries.is_empty() {
		for context in &entries {
			if let Some((_, handle)) = PROFILE_SAVE_DEBOUNCE.remove(context) {
				handle.abort();
			}
		}
		if let Err(error) = save_profile(&device, &mut locks).await {
			log::error!("Failed to save profile for device {device}: {error}");
		}
	}

	let selected_profile = locks.device_stores.get_selected_profile(&device)?;

	// Compute carry slots: positions where the OLD active instance (anchor-resolved)
	// has the same action UUID + settings as NEW's native instance. Those slots skip
	// willDisappear/willAppear so the plugin keeps its state across the switch
	// (e.g. resource-monitor graph buffers, claude-monitor cached sessions).
	let mut carry_keypad: std::collections::HashSet<u8> = std::collections::HashSet::new();
	let mut carry_encoder: std::collections::HashSet<u8> = std::collections::HashSet::new();
	struct PendingCarry {
		controller: &'static str,
		position: u8,
		anchor_profile: String,
	}
	let mut pending_carries: Vec<PendingCarry> = Vec::new();

	if selected_profile != id {
		let device_info = DEVICES.get(&device).unwrap().clone();
		// Snapshot new profile's native slots and old profile's slots so we don't
		// hold the profile_stores guard across the carry::anchor_or_self awaits.
		let new_keys = locks.profile_stores.get_profile_store(&device_info, &id)?.value.keys.clone();
		let new_sliders = locks.profile_stores.get_profile_store(&device_info, &id)?.value.sliders.clone();

		for (i, new_slot) in new_keys.iter().enumerate() {
			let pos = i as u8;
			let anchor = crate::carry::anchor_or_self(&device, &selected_profile, "Keypad", pos).await;
			let old_active: Option<crate::shared::ActionInstance> = locks.profile_stores.get_profile_store(&device_info, &anchor).ok().and_then(|s| s.value.keys.get(i).cloned().flatten());
			if let (Some(old), Some(new)) = (old_active.as_ref(), new_slot.as_ref())
				&& old.action.uuid == new.action.uuid
				&& old.settings == new.settings
			{
				carry_keypad.insert(pos);
				// Skip the self-anchor case (anchor == new profile) — installing
				// (X → X) is a no-op redirect that just clutters logs.
				if anchor != id {
					pending_carries.push(PendingCarry {
						controller: "Keypad",
						position: pos,
						anchor_profile: anchor,
					});
				}
			}
		}
		for (i, new_slot) in new_sliders.iter().enumerate() {
			let pos = i as u8;
			let anchor = crate::carry::anchor_or_self(&device, &selected_profile, "Encoder", pos).await;
			let old_active: Option<crate::shared::ActionInstance> = locks
				.profile_stores
				.get_profile_store(&device_info, &anchor)
				.ok()
				.and_then(|s| s.value.sliders.get(i).cloned().flatten());
			if let (Some(old), Some(new)) = (old_active.as_ref(), new_slot.as_ref())
				&& old.action.uuid == new.action.uuid
				&& old.settings == new.settings
			{
				carry_encoder.insert(pos);
				if anchor != id {
					pending_carries.push(PendingCarry {
						controller: "Encoder",
						position: pos,
						anchor_profile: anchor,
					});
				}
			}
		}
		log::info!("[carry] {} → {}: {} keypad carries, {} encoder carries", selected_profile, id, carry_keypad.len(), carry_encoder.len());

		let old_profile = &locks.profile_stores.get_profile_store(&device_info, &selected_profile)?.value;
		for (i, slot) in old_profile.keys.iter().enumerate() {
			if carry_keypad.contains(&(i as u8)) {
				continue;
			}
			let Some(instance) = slot else { continue };
			if !matches!(instance.action.uuid.as_str(), "opendeck.multiaction" | "opendeck.toggleaction") {
				let _ = crate::events::outbound::will_appear::will_disappear(instance, false).await;
			} else {
				for child in instance.children.as_ref().unwrap() {
					let _ = crate::events::outbound::will_appear::will_disappear(child, false).await;
				}
			}
		}
		for (i, slot) in old_profile.sliders.iter().enumerate() {
			if carry_encoder.contains(&(i as u8)) {
				continue;
			}
			let Some(instance) = slot else { continue };
			if !matches!(instance.action.uuid.as_str(), "opendeck.multiaction" | "opendeck.toggleaction") {
				let _ = crate::events::outbound::will_appear::will_disappear(instance, false).await;
			} else {
				for child in instance.children.as_ref().unwrap() {
					let _ = crate::events::outbound::will_appear::will_disappear(child, false).await;
				}
			}
		}

		// Skip clear_screen entirely if any carries — the carried slots' device
		// images must persist. Non-carry slots will be redrawn by their plugin
		// shortly after willAppear, briefly showing stale content. With zero
		// carries, fall through to the original full-clear behavior.
		if carry_keypad.is_empty() && carry_encoder.is_empty() {
			let _ = crate::events::outbound::devices::clear_screen(device.clone()).await;
		}

		// Stale carries on the profile we're leaving become irrelevant; drop them.
		crate::carry::break_all_for(&device, &selected_profile).await;
		// Refresh carries on the destination so it reflects the just-computed diff,
		// not whatever was installed by an earlier session.
		crate::carry::break_all_for(&device, &id).await;
		for c in &pending_carries {
			crate::carry::install(&device, &id, c.controller, c.position, &c.anchor_profile).await;
		}
	}

	// We must use the mutable version of get_profile_store in order to create the store if it does not exist.
	let store = locks.profile_stores.get_profile_store_mut(&DEVICES.get(&device).unwrap(), &id).await?;
	let new_profile = &store.value;

	// Lazy plugin activation: collect the plugin UUIDs referenced by the new
	// profile and ensure each has a running subprocess before firing
	// will_appear events. Plugins that were metadata-registered at startup
	// but never spawned (because no profile referenced them at boot) are
	// started here on first use.
	let mut needed_plugins = std::collections::HashSet::<String>::new();
	for instance in new_profile.keys.iter().flatten().chain(&mut new_profile.sliders.iter().flatten()) {
		needed_plugins.insert(instance.action.plugin.clone());
		if let Some(children) = &instance.children {
			for child in children {
				needed_plugins.insert(child.action.plugin.clone());
			}
		}
	}
	for uuid in &needed_plugins {
		crate::plugins::ensure_plugin_spawned(uuid).await;
	}

	for (i, slot) in new_profile.keys.iter().enumerate() {
		if carry_keypad.contains(&(i as u8)) {
			continue;
		}
		let Some(instance) = slot else { continue };
		if !matches!(instance.action.uuid.as_str(), "opendeck.multiaction" | "opendeck.toggleaction") {
			let _ = crate::events::outbound::will_appear::will_appear(instance).await;
		} else {
			for child in instance.children.as_ref().unwrap() {
				let _ = crate::events::outbound::will_appear::will_appear(child).await;
			}
		}
	}
	for (i, slot) in new_profile.sliders.iter().enumerate() {
		if carry_encoder.contains(&(i as u8)) {
			continue;
		}
		let Some(instance) = slot else { continue };
		if !matches!(instance.action.uuid.as_str(), "opendeck.multiaction" | "opendeck.toggleaction") {
			let _ = crate::events::outbound::will_appear::will_appear(instance).await;
		} else {
			for child in instance.children.as_ref().unwrap() {
				let _ = crate::events::outbound::will_appear::will_appear(child).await;
			}
		}
	}
	store.save()?;

	locks.device_stores.set_selected_profile(&device, id)?;

	// After a settle period, deactivate any plugin no longer needed by the
	// current profile on this device (or on any other connected device).
	// Rapid profile switching cancels and restarts the timer, so flipping
	// through profiles does not thrash plugin processes.
	drop(locks);
	crate::plugins::schedule_deactivation_sweep();

	Ok(())
}

#[command]
pub async fn delete_profile(device: String, profile: String) {
	let mut profile_stores = PROFILE_STORES.write().await;
	profile_stores.delete_profile(&device, &profile);
}

#[command]
pub async fn rename_profile(device: String, old_id: String, new_id: String, retain: bool) -> Result<(), Error> {
	let mut locks = acquire_locks_mut().await;
	if !DEVICES.contains_key(&device) {
		return Err(Error::new(format!("device {device} not found")));
	}

	locks.profile_stores.rename_profile(&DEVICES.get(&device).unwrap(), &old_id, &new_id, retain).await?;

	Ok(())
}

/// Set one swipe neighbor with bidirectional enforcement.
/// direction = "left" or "right". target = profile to link to.
/// When profile X sets swipe_right = Y, profile Y's swipe_left is
/// automatically set to X. The old neighbor's reciprocal is cleared.
#[command]
pub async fn set_swipe_neighbor(device: String, profile: String, direction: String, target: String) -> Result<(), Error> {
	let mut locks = acquire_locks_mut().await;
	if !DEVICES.contains_key(&device) {
		return Err(Error::new(format!("device {device} not found")));
	}
	let device_info = DEVICES.get(&device).unwrap();
	let is_left = direction == "left";

	let store = locks.profile_stores.get_profile_store_mut(&device_info, &profile).await?;
	let old_target = if is_left { store.value.swipe_left.clone() } else { store.value.swipe_right.clone() };

	// Set the new value
	if is_left {
		store.value.swipe_left = Some(target.clone());
	} else {
		store.value.swipe_right = Some(target.clone());
	}
	store.save()?;

	// Clear old neighbor's reciprocal (if it pointed back to us)
	if let Some(ref old) = old_target {
		if *old != target {
			if let Ok(neighbor) = locks.profile_stores.get_profile_store_mut(&device_info, old).await {
				let recip = if is_left { &mut neighbor.value.swipe_right } else { &mut neighbor.value.swipe_left };
				if recip.as_ref() == Some(&profile) {
					*recip = None;
					let _ = neighbor.save();
				}
			}
		}
	}

	// Set new neighbor's reciprocal
	if let Ok(neighbor) = locks.profile_stores.get_profile_store_mut(&device_info, &target).await {
		let recip = if is_left { &mut neighbor.value.swipe_right } else { &mut neighbor.value.swipe_left };
		*recip = Some(profile.clone());
		let _ = neighbor.save();
	}

	Ok(())
}

pub async fn rerender_images(app: &AppHandle) -> Result<(), anyhow::Error> {
	let window = app.get_webview_window("main").unwrap();
	window.emit("rerender_images", ())?;
	Ok(())
}
