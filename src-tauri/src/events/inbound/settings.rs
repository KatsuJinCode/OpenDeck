use crate::events::outbound::settings as outbound;
use crate::shared::ActionContext;
use crate::store::profiles::{acquire_locks, acquire_locks_mut, get_instance, get_instance_mut, save_profile};

use std::io::Write;
use std::str::FromStr;

pub async fn set_settings(event: super::ContextAndPayloadEvent<serde_json::Value>, from_property_inspector: bool) -> Result<(), anyhow::Error> {
	let mut locks = acquire_locks_mut().await;

	// Carry break-on-edit. If the user edits a slot that's currently carry-redirected
	// to an anchor profile, the user wants this profile's slot to diverge — drop the
	// redirect and let the plugin spin up a fresh instance for the now-native context.
	let mut broke_carry = false;
	if from_property_inspector {
		if let Some(_anchor) = crate::carry::anchor_for(&event.context.device, &event.context.profile, &event.context.controller, event.context.position).await {
			crate::carry::break_for(&event.context.device, &event.context.profile, &event.context.controller, event.context.position).await;
			log::info!(
				"[carry] BREAK on user edit: device={} profile={} {} pos={}",
				event.context.device, event.context.profile, event.context.controller, event.context.position
			);
			broke_carry = true;
		}
	}

	if let Some(instance) = get_instance_mut(&event.context, &mut locks).await? {
		instance.settings = event.payload;
		outbound::did_receive_settings(instance, !from_property_inspector).await?;
		save_profile(&event.context.device, &mut locks).await?;
	}

	// If we just broke a carry on the active profile, fire willAppear so the plugin
	// creates a fresh instance for this newly-divergent context.
	if broke_carry {
		let should_appear = match locks.device_stores.get_selected_profile(&event.context.device) {
			Ok(p) => p == event.context.profile,
			Err(_) => false,
		};
		if should_appear {
			let snapshot: Option<crate::shared::ActionInstance> = match get_instance_mut(&event.context, &mut locks).await {
				Ok(Some(inst)) => Some(inst.clone()),
				_ => None,
			};
			drop(locks);
			if let Some(snap) = snapshot {
				let _ = crate::events::outbound::will_appear::will_appear(&snap).await;
			}
		}
	}

	Ok(())
}

pub async fn get_settings(event: super::ContextEvent, from_property_inspector: bool) -> Result<(), anyhow::Error> {
	let locks = acquire_locks().await;

	if let Some(instance) = get_instance(&event.context, &locks).await? {
		outbound::did_receive_settings(instance, from_property_inspector).await?;
	}

	Ok(())
}

pub async fn set_global_settings(event: super::ContextAndPayloadEvent<serde_json::Value, String>, from_property_inspector: bool) -> Result<(), anyhow::Error> {
	let uuid = if from_property_inspector {
		if let Some(instance) = get_instance(&ActionContext::from_str(&event.context)?, &acquire_locks().await).await? {
			instance.action.plugin.clone()
		} else {
			return Ok(());
		}
	} else {
		event.context.clone()
	};

	{
		let settings_dir = crate::shared::config_dir().join("settings");
		tokio::fs::create_dir_all(&settings_dir).await?;

		let mut file = std::fs::OpenOptions::new().write(true).truncate(true).create(true).open(settings_dir.join(uuid.clone() + ".json"))?;
		file.lock()?;
		file.write_all(event.payload.to_string().as_bytes())?;
		file.sync_data()?;
		file.unlock()?;
	}

	outbound::did_receive_global_settings(&uuid, !from_property_inspector).await?;

	Ok(())
}

pub async fn get_global_settings(event: super::ContextEvent<String>, from_property_inspector: bool) -> Result<(), anyhow::Error> {
	let uuid = if from_property_inspector {
		if let Some(instance) = get_instance(&ActionContext::from_str(&event.context)?, &acquire_locks().await).await? {
			instance.action.plugin.clone()
		} else {
			return Ok(());
		}
	} else {
		event.context.clone()
	};

	outbound::did_receive_global_settings(&uuid, from_property_inspector).await
}
