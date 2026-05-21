use super::Error;

use crate::shared::{Action, ActionContext, ActionInstance, ActionState, CATEGORIES, Context, config_dir};
use crate::store::profiles::{LocksMut, acquire_locks, acquire_locks_mut, get_instance_mut, get_slot, get_slot_mut, save_profile};

use tauri::{AppHandle, Emitter, Manager, command};
use tokio::fs::remove_dir_all;

#[command]
pub async fn create_instance(app: AppHandle, action: Action, context: Context) -> Result<Option<ActionInstance>, Error> {
	if !action.controllers.contains(&context.controller) {
		return Ok(None);
	}
	crate::plugins::ensure_plugin_spawned(&action.plugin).await;

	// Per Elgato SDK (https://docs.elgato.com/streamdeck/sdk/guides/dials/),
	// encoder feedback renders via layouts. Default to $X1 when the manifest
	// omits Encoder.layout, or when a non-encoder action is dropped on an
	// encoder slot, so the LCD shows the title+icon unstretched instead of
	// being rendered as a full-rect key image.
	let default_encoder_layout = |action: &Action| -> Option<String> {
		let explicit = action.encoder.as_ref().and_then(|e| e.layout.clone());
		if explicit.is_some() {
			explicit
		} else if context.controller == "Encoder" {
			Some("$X1".to_owned())
		} else {
			None
		}
	};

	let mut locks = acquire_locks_mut().await;
	let slot = get_slot_mut(&context, &mut locks).await?;

	if let Some(parent) = slot {
		let Some(children) = &mut parent.children else { return Ok(None) };
		let index = match children.last() {
			None => 1,
			Some(instance) => instance.context.index + 1,
		};

		let instance = ActionInstance {
			action: action.clone(),
			context: ActionContext::from_context(context.clone(), index),
			states: action.states.clone(),
			current_state: 0,
			settings: serde_json::Value::Object(serde_json::Map::new()),
			children: None,
			feedback_layout: default_encoder_layout(&action),
			feedback: serde_json::Value::Null,
			skip_persistence: None,
		};
		children.push(instance.clone());

		if parent.action.uuid == "opendeck.toggleaction" && parent.states.len() < children.len() {
			parent.states.push(crate::shared::ActionState {
				image: "opendeck/toggle-action.png".to_owned(),
				..Default::default()
			});
			let _ = update_state(&app, parent.context.clone(), &mut locks).await;
		}

		save_profile(&context.device, &mut locks).await?;
		drop(locks);
		let _ = crate::events::outbound::will_appear::will_appear(&instance).await;

		let locks = acquire_locks().await;
		let slot = get_slot(&context, &locks).await?.clone();
		Ok(slot)
	} else {
		let instance = ActionInstance {
			action: action.clone(),
			context: ActionContext::from_context(context.clone(), 0),
			states: action.states.clone(),
			current_state: 0,
			settings: serde_json::Value::Object(serde_json::Map::new()),
			children: if matches!(action.uuid.as_str(), "opendeck.multiaction" | "opendeck.toggleaction" | "com.jw.encoder-multi-key.grid") {
				Some(vec![])
			} else {
				None
			},
			feedback_layout: default_encoder_layout(&action),
			feedback: serde_json::Value::Null,
			skip_persistence: None,
		};

		*slot = Some(instance.clone());
		let slot = slot.clone();

		save_profile(&context.device, &mut locks).await?;
		let _ = crate::events::outbound::will_appear::will_appear(&instance).await;

		Ok(slot)
	}
}

#[command]
pub async fn place_instance(app: AppHandle, action_uuid: String, context: Context, replace: bool) -> Result<ActionInstance, Error> {
	let action = CATEGORIES
		.read()
		.await
		.values()
		.flat_map(|category| category.actions.iter())
		.find(|action| action.uuid == action_uuid)
		.cloned()
		.ok_or_else(|| Error::new(format!("action {action_uuid} not found")))?;

	let existing_context = {
		let locks = acquire_locks().await;
		get_slot(&context, &locks).await?.as_ref().map(|instance| instance.context.clone())
	};

	if let Some(existing) = existing_context {
		if !replace {
			return Err(Error::new(format!(
				"slot {}.{}.{}.{} is occupied",
				context.device, context.profile, context.controller, context.position
			)));
		}
		remove_instance(existing).await?;
	}

	create_instance(app, action, context)
		.await?
		.ok_or_else(|| Error::new(format!("action {action_uuid} cannot be placed there")))
}

fn instance_images_dir(context: &ActionContext) -> std::path::PathBuf {
	config_dir()
		.join("images")
		.join(&context.device)
		.join(&context.profile)
		.join(format!("{}.{}.{}", context.controller, context.position, context.index))
}

#[command]
pub async fn move_instance(source: Context, destination: Context, retain: bool) -> Result<Option<ActionInstance>, Error> {
	if source.controller != destination.controller {
		return Ok(None);
	}

	{
		let locks = crate::store::profiles::acquire_locks().await;
		let dst = crate::store::profiles::get_slot(&destination, &locks).await?;
		if dst.is_some() {
			return Ok(None);
		}
	}

	let mut locks = acquire_locks_mut().await;
	let src = get_slot_mut(&source, &mut locks).await?;

	let Some(mut new) = src.clone() else {
		return Ok(None);
	};
	new.context = ActionContext::from_context(destination.clone(), 0);
	if let Some(children) = &mut new.children {
		for (index, instance) in children.iter_mut().enumerate() {
			instance.context = ActionContext::from_context(destination.clone(), index as u16 + 1);
			for (i, state) in instance.states.iter_mut().enumerate() {
				if !instance.action.states[i].image.is_empty() {
					state.image = instance.action.states[i].image.clone();
				} else {
					state.image = instance.action.icon.clone();
				}
			}
		}
	}

	let old_dir = instance_images_dir(&src.as_ref().unwrap().context);
	let new_dir = instance_images_dir(&new.context);
	let _ = tokio::fs::create_dir_all(&new_dir).await;
	if let Ok(files) = old_dir.read_dir() {
		for file in files.flatten() {
			let _ = tokio::fs::copy(file.path(), new_dir.join(file.file_name())).await;
		}
	}
	for state in new.states.iter_mut() {
		let path = std::path::Path::new(&state.image);
		if path.starts_with(&old_dir) {
			state.image = new_dir.join(path.strip_prefix(&old_dir).unwrap()).to_string_lossy().into_owned();
		}
	}

	let dst = get_slot_mut(&destination, &mut locks).await?;
	*dst = Some(new.clone());

	if !retain {
		let src = get_slot_mut(&source, &mut locks).await?;
		if let Some(old) = src {
			let _ = crate::events::outbound::will_appear::will_disappear(old, true).await;
			let _ = remove_dir_all(instance_images_dir(&old.context)).await;
		}
		*src = None;
	}

	let _ = crate::events::outbound::will_appear::will_appear(&new).await;

	save_profile(&destination.device, &mut locks).await?;

	Ok(Some(new))
}

#[command]
pub async fn remove_instance(context: ActionContext) -> Result<(), Error> {
	let mut locks = acquire_locks_mut().await;
	let slot = get_slot_mut(&(&context).into(), &mut locks).await?;
	let Some(instance) = slot else {
		return Ok(());
	};

	if instance.context == context {
		let _ = crate::events::outbound::will_appear::will_disappear(instance, true).await;
		if let Some(children) = &instance.children {
			for child in children {
				let _ = crate::events::outbound::will_appear::will_disappear(child, true).await;
				let _ = remove_dir_all(instance_images_dir(&child.context)).await;
			}
		}
		let _ = remove_dir_all(instance_images_dir(&instance.context)).await;
		*slot = None;
	} else {
		let children = instance.children.as_mut().unwrap();
		for (index, instance) in children.iter().enumerate() {
			if instance.context == context {
				let _ = crate::events::outbound::will_appear::will_disappear(instance, true).await;
				let _ = remove_dir_all(instance_images_dir(&instance.context)).await;
				children.remove(index);
				break;
			}
		}
		if instance.action.uuid == "opendeck.toggleaction" {
			if instance.current_state as usize >= children.len() {
				instance.current_state = if children.is_empty() { 0 } else { children.len() as u16 - 1 };
			}
			if !children.is_empty() {
				instance.states.pop();
				let _ = update_state(crate::APP_HANDLE.get().unwrap(), instance.context.clone(), &mut locks).await;
			}
		}
	}

	save_profile(&context.device, &mut locks).await?;

	Ok(())
}

#[derive(Clone, serde::Serialize)]
struct UpdateStateEvent {
	context: ActionContext,
	contents: Option<ActionInstance>,
}

pub async fn update_state(app: &AppHandle, context: ActionContext, locks: &mut LocksMut<'_>) -> Result<(), anyhow::Error> {
	let window = app.get_webview_window("main").unwrap();
	// Per-context event name so Tauri delivers only to the specific Key that
	// owns this context, rather than broadcasting to all N Key listeners.
	// Tauri event names disallow dots, so use ":" as separator in place of ".".
	let event_name = format!("update_state::{}", context.to_string().replace('.', ":"));
	window.emit(
		&event_name,
		UpdateStateEvent {
			contents: get_instance_mut(&context, locks).await?.cloned(),
			context,
		},
	)?;
	Ok(())
}

#[command]
pub async fn set_state(context: ActionContext, index: u16, state: ActionState) -> Result<(), Error> {
	let mut locks = acquire_locks_mut().await;
	let reference = get_instance_mut(&context, &mut locks).await?.unwrap();
	reference.states[index as usize] = state;
	let clone = reference.clone();
	save_profile(&context.device, &mut locks).await?;
	crate::events::outbound::states::title_parameters_did_change(&clone, index).await?;
	Ok(())
}

#[command]
pub async fn update_image(context: Context, image: Option<String>) {
	// Encoder slots are driven by the backend fast path (setFeedback) and
	// clear_screen (profile switch). The frontend must never push null to
	// encoder slots -- doing so races with the fast path and causes flash.
	if context.controller == "Encoder" && image.is_none() {
		return;
	}
	let selected = crate::store::profiles::DEVICE_STORES.write().await.get_selected_profile(&context.device).ok();
	let is_active_native = selected.as_ref() == Some(&context.profile);
	// Carry-aware: if context.profile is the anchor of a slot in the active profile, push anyway.
	let is_carry_anchor = match &selected {
		Some(active) => crate::carry::is_anchor_for_active(&context.device, active, &context.controller, context.position, &context.profile).await,
		None => false,
	};
	if !is_active_native && !is_carry_anchor {
		return;
	}

	if let Err(error) = crate::events::outbound::devices::update_image(context, image).await {
		log::warn!("Failed to update device image: {}", error);
	}
}

#[command]
pub async fn trigger_virtual_press(context: Context) -> Result<(), Error> {
	let event = || crate::events::inbound::PayloadEvent {
		payload: crate::events::inbound::devices::PressPayload {
			device: context.device.clone(),
			position: context.position,
		},
	};
	match context.controller.as_str() {
		"Keypad" => {
			crate::events::inbound::devices::key_down(event()).await?;
			tokio::time::sleep(std::time::Duration::from_millis(100)).await;
			crate::events::inbound::devices::key_up(event()).await?;
		}
		"Encoder" => {
			crate::events::inbound::devices::encoder_down(event()).await?;
			tokio::time::sleep(std::time::Duration::from_millis(100)).await;
			crate::events::inbound::devices::encoder_up(event()).await?;
		}
		_ => {}
	}

	Ok(())
}

#[command]
pub async fn trigger_virtual_rotate(context: Context, ticks: i16) -> Result<(), Error> {
	if context.controller != "Encoder" {
		return Ok(());
	}
	crate::events::inbound::devices::encoder_change(crate::events::inbound::PayloadEvent {
		payload: crate::events::inbound::devices::TicksPayload {
			device: context.device.clone(),
			position: context.position,
			ticks,
		},
	})
	.await?;
	Ok(())
}

#[command]
pub async fn trigger_virtual_encoder_down(context: Context) -> Result<(), Error> {
	if context.controller != "Encoder" {
		return Ok(());
	}
	crate::events::inbound::devices::encoder_down(crate::events::inbound::PayloadEvent {
		payload: crate::events::inbound::devices::PressPayload {
			device: context.device.clone(),
			position: context.position,
		},
	})
	.await?;
	Ok(())
}

#[command]
pub async fn trigger_virtual_encoder_up(context: Context) -> Result<(), Error> {
	if context.controller != "Encoder" {
		return Ok(());
	}
	crate::events::inbound::devices::encoder_up(crate::events::inbound::PayloadEvent {
		payload: crate::events::inbound::devices::PressPayload {
			device: context.device.clone(),
			position: context.position,
		},
	})
	.await?;
	Ok(())
}

#[command]
pub async fn trigger_virtual_touch(context: Context, hold: bool) -> Result<(), Error> {
	if context.controller != "Encoder" {
		return Ok(());
	}
	crate::events::inbound::devices::touch_tap(crate::events::inbound::PayloadEvent {
		payload: crate::events::inbound::devices::TouchPayload {
			device: context.device.clone(),
			position: context.position,
			tap_pos: (100, 50),
			hold,
		},
	})
	.await?;
	Ok(())
}

#[command]
pub async fn toggle_skip_persistence(context: Context) -> Result<Option<bool>, Error> {
	let mut locks = acquire_locks_mut().await;
	let global_default = crate::store::get_settings().map(|s| s.value.skip_persistence_default).unwrap_or(false);
	let slot = get_slot_mut(&context, &mut locks).await?;
	if let Some(instance) = slot {
		instance.skip_persistence = match instance.skip_persistence {
			None => Some(!global_default),
			Some(_) => None,
		};
		let new_value = instance.skip_persistence;
		save_profile(&context.device, &mut locks).await?;
		Ok(new_value)
	} else {
		Ok(None)
	}
}

#[derive(Clone, serde::Serialize)]
struct KeyMovedEvent {
	context: Context,
	pressed: bool,
}

pub async fn key_moved(app: &AppHandle, context: Context, pressed: bool) -> Result<(), anyhow::Error> {
	let window = app.get_webview_window("main").unwrap();
	window.emit("key_moved", KeyMovedEvent { context, pressed })?;
	Ok(())
}
