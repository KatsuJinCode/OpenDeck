use super::{ContextEvent, PayloadEvent};

use tauri::{Emitter, Manager};

use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct OpenUrlEvent {
	pub url: String,
}

pub async fn open_url(event: PayloadEvent<OpenUrlEvent>) -> Result<(), anyhow::Error> {
	log::debug!("Opening URL {}", event.payload.url);
	open::that_detached(event.payload.url)?;
	Ok(())
}

#[derive(Deserialize)]
pub struct LogMessageEvent {
	pub message: String,
}

pub async fn log_message(uuid: Option<&str>, mut event: PayloadEvent<LogMessageEvent>) -> Result<(), anyhow::Error> {
	if let Some(uuid) = uuid
		&& let Ok(manifest) = crate::plugins::manifest::read_manifest(&crate::shared::config_dir().join("plugins").join(uuid))
	{
		event.payload.message = format!("[{}] {}", manifest.name, event.payload.message);
	}
	log::info!("{}", event.payload.message.trim());
	Ok(())
}

pub async fn show_alert(event: ContextEvent) -> Result<(), anyhow::Error> {
	let app = crate::APP_HANDLE.get().unwrap();
	app.get_webview_window("main").unwrap().emit("show_alert", event.context)?;
	Ok(())
}

pub async fn show_ok(event: ContextEvent) -> Result<(), anyhow::Error> {
	let app = crate::APP_HANDLE.get().unwrap();
	app.get_webview_window("main").unwrap().emit("show_ok", event.context)?;
	Ok(())
}

/// Stream Deck SDK shape for the `switchToProfile` event:
///   { event: "switchToProfile", context: pluginUUID, device: deviceId,
///     payload: { profile: name, page: int } }
/// `device` is at top level; `profile` lives in `payload`. The previous flat
/// definition `{ device, profile }` couldn't deserialize anything the SDK
/// actually sent — every plugin call was silently dropped at decode.
#[derive(Clone, Serialize, Deserialize)]
pub struct SwitchProfileEvent {
	pub device: String,
	pub payload: SwitchProfilePayload,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SwitchProfilePayload {
	#[serde(default)]
	pub profile: Option<String>,
	#[serde(default)]
	pub page: Option<u32>,
}

impl SwitchProfileEvent {
	pub fn new(device: String, profile: String) -> Self {
		Self {
			device,
			payload: SwitchProfilePayload { profile: Some(profile), page: None },
		}
	}
}

pub async fn switch_profile(event: SwitchProfileEvent) -> Result<(), anyhow::Error> {
	let app_handle = crate::APP_HANDLE.get().unwrap();
	let Some(profile_name) = event.payload.profile.clone() else {
		// SDK allows omitting profile to "switch to previous"; not implemented
		// here because we have no previous-profile tracking. Ignore for now.
		log::info!("[switch_profile] skip: no profile name in payload");
		return Ok(());
	};
	log::info!("[switch_profile] device={} profile={}", event.device, profile_name);
	// Drive the actual profile change on the backend. Previously this only
	// emitted a webview event, which only worked when the UI was open. Calling
	// set_selected_profile directly makes the switch happen headless too.
	if let Err(e) = crate::events::frontend::profiles::set_selected_profile(
		event.device.clone(),
		profile_name,
	).await {
		log::warn!("[switch_profile] set_selected_profile failed: {}", e);
	}
	app_handle.get_webview_window("main").unwrap().emit("switch_profile", event)?;
	Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceBrightnessEvent {
	action: String,
	value: u8,
}

pub async fn device_brightness(event: DeviceBrightnessEvent) -> Result<(), anyhow::Error> {
	let app_handle = crate::APP_HANDLE.get().unwrap();
	app_handle.get_webview_window("main").unwrap().emit("device_brightness", event)?;
	Ok(())
}

/// Trigger a virtual keyDown + keyUp on a specific child of an action instance.
/// Plugins use this to dispatch tap events to virtual sub-actions (e.g. multi-key
/// grids where each cell runs a different action).
#[derive(Clone, Deserialize)]
pub struct TriggerChildPressEvent {
	pub context: String,
	pub child_index: usize,
}

pub async fn trigger_child_press(event: TriggerChildPressEvent) -> Result<(), anyhow::Error> {
	use crate::events::outbound::{GenericInstancePayload, send_to_plugin};
	use crate::store::profiles::{acquire_locks_mut, get_instance_mut};

	let action_context: crate::shared::ActionContext = event.context.parse()?;

	let mut locks = acquire_locks_mut().await;
	let instance = get_instance_mut(&action_context, &mut locks).await?
		.ok_or_else(|| anyhow::anyhow!("instance not found"))?;

	let children = instance.children.as_mut()
		.ok_or_else(|| anyhow::anyhow!("instance has no children"))?;

	if event.child_index >= children.len() {
		return Err(anyhow::anyhow!("child index {} out of range ({})", event.child_index, children.len()));
	}

	let child = &mut children[event.child_index];

	#[derive(serde::Serialize)]
	struct KeyEvent {
		event: &'static str,
		action: String,
		context: crate::shared::ActionContext,
		device: String,
		payload: GenericInstancePayload,
	}

	send_to_plugin(
		&child.action.plugin,
		&KeyEvent {
			event: "keyDown",
			action: child.action.uuid.clone(),
			context: child.context.clone(),
			device: child.context.device.clone(),
			payload: GenericInstancePayload::new(child),
		},
	).await?;

	let child_ctx = child.context.clone();
	let child_plugin = child.action.plugin.clone();
	let child_uuid = child.action.uuid.clone();
	drop(locks);

	tokio::time::sleep(std::time::Duration::from_millis(100)).await;

	let mut locks = acquire_locks_mut().await;
	if let Ok(Some(instance)) = get_instance_mut(&action_context, &mut locks).await {
		if let Some(children) = instance.children.as_mut() {
			if event.child_index < children.len() {
				let child = &mut children[event.child_index];
				if child.states.len() == 2 && !child.action.disable_automatic_states {
					child.current_state = (child.current_state + 1) % (child.states.len() as u16);
				}
				send_to_plugin(
					&child.action.plugin,
					&KeyEvent {
						event: "keyUp",
						action: child.action.uuid.clone(),
						context: child.context.clone(),
						device: child.context.device.clone(),
						payload: GenericInstancePayload::new(child),
					},
				).await?;
			}
		}
	}

	Ok(())
}

#[derive(Clone, Deserialize)]
pub struct CreateChildEvent {
	pub context: String,
	pub action_uuid: String,
}

pub async fn create_child(event: CreateChildEvent) -> Result<(), anyhow::Error> {
	use crate::store::profiles::{acquire_locks_mut, get_slot_mut, save_profile};

	log::info!("[multi-key] createChild: context={} action={}", event.context, event.action_uuid);
	let parent_ctx: crate::shared::ActionContext = event.context.parse()?;
	let parent_context: crate::shared::Context = (&parent_ctx).into();

	let categories = crate::shared::CATEGORIES.read().await;
	let action = categories.values()
		.flat_map(|c| c.actions.iter())
		.find(|a| a.uuid == event.action_uuid)
		.cloned()
		.ok_or_else(|| anyhow::anyhow!("action {} not found", event.action_uuid))?;
	drop(categories);

	let mut locks = acquire_locks_mut().await;
	let slot = get_slot_mut(&parent_context, &mut locks).await?;
	let Some(instance) = slot else { return Err(anyhow::anyhow!("slot empty")) };
	let Some(children) = &mut instance.children else { return Err(anyhow::anyhow!("slot doesn't support children")) };

	let index = match children.last() {
		None => 1,
		Some(c) => c.context.index + 1,
	};

	// Keypad controller so plugins render key-style (144x144 square, centered).
	// Virtual position (100+) avoids conflicting with real key positions.
	let child_ctx = crate::shared::Context {
		controller: "Keypad".to_owned(),
		position: 100 + parent_context.position * 10 + index as u8,
		..parent_context.clone()
	};
	let child = crate::shared::ActionInstance {
		action: action.clone(),
		context: crate::shared::ActionContext::from_context(child_ctx, index),
		states: action.states.clone(),
		current_state: 0,
		settings: serde_json::Value::Object(serde_json::Map::new()),
		children: None,
		feedback_layout: None,
		feedback: serde_json::Value::Null,
		skip_persistence: None,
	};
	let child_index = children.len(); // 0-based index in children array
	children.push(child.clone());
	let parent_plugin = instance.action.plugin.clone();
	let parent_uuid = instance.action.uuid.clone();
	let parent_ctx_str = instance.context.to_string();
	save_profile(&parent_context.device, &mut locks).await?;
	drop(locks);
	let _ = crate::events::outbound::will_appear::will_appear(&child).await;

	// Notify the parent plugin of the child's actual index so it can
	// map cells correctly (plugin-side counting can diverge from backend).
	let _ = crate::events::outbound::send_to_plugin(
		&parent_plugin,
		&serde_json::json!({
			"event": "sendToPlugin",
			"action": parent_uuid,
			"context": parent_ctx_str,
			"payload": {
				"childCreated": {
					"index": child_index,
					"actionUUID": event.action_uuid,
					"actionName": child.action.name,
				}
			}
		}),
	).await;

	// Send the child's default state image to the parent plugin immediately.
	let default_image = &child.states[child.current_state as usize].image;
	if !default_image.is_empty() {
		let _ = crate::events::outbound::send_to_plugin(
			&parent_plugin,
			&serde_json::json!({
				"event": "sendToPlugin",
				"action": parent_uuid,
				"context": parent_ctx_str,
				"payload": {
					"childImageUpdate": {
						"index": child_index,
						"image": default_image
					}
				}
			}),
		).await;
	}

	Ok(())
}

#[derive(Clone, Deserialize)]
pub struct RemoveChildEvent {
	pub context: String,
	pub child_index: usize,
}

pub async fn remove_child(event: RemoveChildEvent) -> Result<(), anyhow::Error> {
	use crate::store::profiles::{acquire_locks_mut, get_slot_mut, save_profile};

	let parent_ctx: crate::shared::ActionContext = event.context.parse()?;
	let parent_context: crate::shared::Context = (&parent_ctx).into();

	let mut locks = acquire_locks_mut().await;
	let slot = get_slot_mut(&parent_context, &mut locks).await?;
	let Some(instance) = slot else { return Err(anyhow::anyhow!("slot empty")) };
	let Some(children) = &mut instance.children else { return Err(anyhow::anyhow!("no children")) };

	if event.child_index >= children.len() {
		return Err(anyhow::anyhow!("child index out of range"));
	}

	let child = children.remove(event.child_index);
	let _ = crate::events::outbound::will_appear::will_disappear(&child, false).await;
	save_profile(&parent_context.device, &mut locks).await?;
	Ok(())
}

#[derive(Clone, Deserialize)]
pub struct OpenChildPIEvent {
	pub context: String,
	pub child_index: usize,
}

pub async fn open_child_pi(event: OpenChildPIEvent) -> Result<(), anyhow::Error> {
	use crate::store::profiles::{acquire_locks, get_slot};

	let parent_ctx: crate::shared::ActionContext = event.context.parse()?;
	let parent_context: crate::shared::Context = (&parent_ctx).into();

	let locks = acquire_locks().await;
	let slot = get_slot(&parent_context, &locks).await?;
	let Some(instance) = slot else { return Err(anyhow::anyhow!("slot empty")) };
	let Some(children) = &instance.children else { return Err(anyhow::anyhow!("no children")) };

	if event.child_index >= children.len() {
		return Err(anyhow::anyhow!("child index out of range"));
	}

	let child_context = children[event.child_index].context.to_string();
	log::info!("[multi-key] openChildPI: child_context={}", child_context);

	let app = crate::APP_HANDLE.get().unwrap();
	app.get_webview_window("main").unwrap().emit("open_child_pi", child_context)?;
	Ok(())
}
