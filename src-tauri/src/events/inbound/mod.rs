pub(crate) mod devices;
mod misc;
mod property_inspector;
mod settings;
mod states;

use crate::{
	shared::ActionContext,
	store::profiles::{acquire_locks, get_instance},
};

use tokio_tungstenite::tungstenite::{Error, Message};

use log::warn;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(tag = "event")]
#[serde(rename_all = "camelCase")]
pub enum RegisterEvent {
	RegisterPlugin { uuid: String },
	RegisterPropertyInspector { uuid: String },
}

#[derive(Deserialize)]
pub struct ContextEvent<C = ActionContext> {
	pub context: C,
}

#[derive(Deserialize)]
pub struct PayloadEvent<T> {
	pub payload: T,
}

#[derive(Deserialize)]
pub struct ContextAndPayloadEvent<T, C = ActionContext> {
	pub context: C,
	pub payload: T,
}

#[derive(Deserialize)]
#[serde(tag = "event")]
#[serde(rename_all = "camelCase")]
pub enum InboundEventType {
	RegisterDevice(PayloadEvent<crate::shared::DeviceInfo>),
	DeregisterDevice(PayloadEvent<String>),
	RerenderImages(PayloadEvent<String>),
	KeyDown(PayloadEvent<devices::PressPayload>),
	KeyUp(PayloadEvent<devices::PressPayload>),
	EncoderChange(PayloadEvent<devices::TicksPayload>),
	EncoderDown(PayloadEvent<devices::PressPayload>),
	EncoderUp(PayloadEvent<devices::PressPayload>),
	SetSettings(ContextAndPayloadEvent<serde_json::Value>),
	GetSettings(ContextEvent),
	SetGlobalSettings(ContextAndPayloadEvent<serde_json::Value, String>),
	GetGlobalSettings(ContextEvent<String>),
	OpenUrl(PayloadEvent<misc::OpenUrlEvent>),
	LogMessage(PayloadEvent<misc::LogMessageEvent>),
	SetTitle(ContextAndPayloadEvent<states::SetTitlePayload>),
	SetImage(ContextAndPayloadEvent<states::SetImagePayload>),
	SetFeedback(ContextAndPayloadEvent<serde_json::Value>),
	SetFeedbackLayout(ContextAndPayloadEvent<serde_json::Value>),
	SetState(ContextAndPayloadEvent<states::SetStatePayload>),
	ShowAlert(ContextEvent),
	ShowOk(ContextEvent),
	SendToPropertyInspector(ContextAndPayloadEvent<serde_json::Value>),
	SendToPlugin(ContextAndPayloadEvent<serde_json::Value>),
	// SDK event name is `switchToProfile`, not the camelCase derivation
	// `switchProfile`. Without this rename, plugin-sent profile-switch
	// messages silently fail to deserialize and the entire feature is dead.
	#[serde(rename = "switchToProfile")]
	SwitchProfile(misc::SwitchProfileEvent),
	DeviceBrightness(misc::DeviceBrightnessEvent),
	TriggerChildPress(misc::TriggerChildPressEvent),
	CreateChild(misc::CreateChildEvent),
	RemoveChild(misc::RemoveChildEvent),
	OpenChildPI(misc::OpenChildPIEvent),
}

pub async fn process_incoming_message(data: Result<Message, Error>, uuid: &str, skip_auth: bool) {
	if let Ok(Message::Text(text)) = data {
		let decoded: InboundEventType = match serde_json::from_str(&text) {
			Ok(event) => event,
			Err(e) => {
				if text.contains("Child") || text.contains("child") {
					log::warn!("Custom event parse error: {} -- {}", e, &text[..text.len().min(300)]);
				}
				return;
			}
		};

		if !(uuid.is_empty() && skip_auth) {
			if let Some(context) = match &decoded {
				InboundEventType::SetSettings(event) => Some(&event.context),
				InboundEventType::GetSettings(event) => Some(&event.context),
				InboundEventType::SetTitle(event) => Some(&event.context),
				InboundEventType::SetImage(event) => Some(&event.context),
				InboundEventType::SetFeedback(event) => Some(&event.context),
				InboundEventType::SetFeedbackLayout(event) => Some(&event.context),
				InboundEventType::SetState(event) => Some(&event.context),
				InboundEventType::ShowAlert(event) => Some(&event.context),
				InboundEventType::ShowOk(event) => Some(&event.context),
				InboundEventType::SendToPropertyInspector(event) => Some(&event.context),
				_ => None,
			} {
				if let Ok(Some(instance)) = get_instance(context, &acquire_locks().await).await {
					if instance.action.plugin != uuid {
						return;
					}
				} else if context.index > 0 {
					// Child instance at a virtual position -- skip auth.
					// The child was created by createChild and lives in a
					// parent slot's children array, not as a top-level slot.
				} else {
					return;
				}
			} else if let InboundEventType::SetGlobalSettings(event) = &decoded {
				if event.context != uuid {
					return;
				}
			} else if let InboundEventType::GetGlobalSettings(event) = &decoded {
				if event.context != uuid {
					return;
				}
			} else if matches!(decoded, InboundEventType::SwitchProfile(_) | InboundEventType::DeviceBrightness(_))
				&& uuid != "com.amansprojects.starterpack.sdPlugin"
				&& uuid != "opendeck_alternative_elgato_implementation"
			{
				// Upstream restricts SwitchProfile + DeviceBrightness to two
				// specific plugins. That blocks legitimate use cases like
				// auto-switching profiles based on app context (browser tab
				// focus, foreground app, etc.). On a self-hosted install the
				// trust boundary doesn't exist — every plugin is one the user
				// installed themselves. Log the call but allow it through.
				log::info!("[switch_profile] allowing SwitchProfile/DeviceBrightness from plugin uuid={}", uuid);
			}
		}

		if let Err(error) = match decoded {
			InboundEventType::RegisterDevice(event) => devices::register_device(uuid, event).await,
			InboundEventType::DeregisterDevice(event) => devices::deregister_device(uuid, event).await,
			InboundEventType::RerenderImages(event) => devices::rerender_images(event).await,
			InboundEventType::KeyDown(event) => devices::key_down(event).await,
			InboundEventType::KeyUp(event) => devices::key_up(event).await,
			InboundEventType::EncoderChange(event) => devices::encoder_change(event).await,
			InboundEventType::EncoderDown(event) => devices::encoder_down(event).await,
			InboundEventType::EncoderUp(event) => devices::encoder_up(event).await,
			InboundEventType::SetSettings(event) => settings::set_settings(event, false).await,
			InboundEventType::GetSettings(event) => settings::get_settings(event, false).await,
			InboundEventType::SetGlobalSettings(event) => settings::set_global_settings(event, false).await,
			InboundEventType::GetGlobalSettings(event) => settings::get_global_settings(event, false).await,
			InboundEventType::OpenUrl(event) => misc::open_url(event).await,
			InboundEventType::LogMessage(event) => misc::log_message(Some(uuid), event).await,
			InboundEventType::SetTitle(event) => states::set_title(event).await,
			InboundEventType::SetImage(event) => states::set_image(event).await,
			InboundEventType::SetFeedback(event) => states::set_feedback(event).await,
			InboundEventType::SetFeedbackLayout(event) => states::set_feedback_layout(event).await,
			InboundEventType::SetState(event) => states::set_state(event).await,
			InboundEventType::ShowAlert(event) => misc::show_alert(event).await,
			InboundEventType::ShowOk(event) => misc::show_ok(event).await,
			InboundEventType::SendToPropertyInspector(event) => property_inspector::send_to_property_inspector(event).await,
			InboundEventType::SendToPlugin(_) => Ok(()),
			InboundEventType::SwitchProfile(event) => misc::switch_profile(event).await,
			InboundEventType::DeviceBrightness(event) => misc::device_brightness(event).await,
			InboundEventType::TriggerChildPress(event) => misc::trigger_child_press(event).await,
			InboundEventType::CreateChild(event) => misc::create_child(event).await,
			InboundEventType::RemoveChild(event) => misc::remove_child(event).await,
			InboundEventType::OpenChildPI(event) => misc::open_child_pi(event).await,
		} && !error.to_string().contains("closed connection")
		{
			warn!("Failed to process incoming event from plugin: {}", error);
		}
	}
}

pub async fn process_incoming_message_pi(data: Result<Message, Error>, uuid: &str) {
	if let Ok(Message::Text(text)) = data {
		let decoded: InboundEventType = match serde_json::from_str(&text) {
			Ok(event) => event,
			Err(_) => return,
		};

		if let Some(context) = match &decoded {
			InboundEventType::SetSettings(event) => Some(event.context.to_string()),
			InboundEventType::GetSettings(event) => Some(event.context.to_string()),
			InboundEventType::SetGlobalSettings(event) => Some(event.context.clone()),
			InboundEventType::GetGlobalSettings(event) => Some(event.context.clone()),
			InboundEventType::SendToPlugin(event) => Some(event.context.to_string()),
			_ => None,
		} && context != uuid
		{
			return;
		}

		if let Err(error) = match decoded {
			InboundEventType::SetSettings(event) => settings::set_settings(event, true).await,
			InboundEventType::GetSettings(event) => settings::get_settings(event, true).await,
			InboundEventType::SetGlobalSettings(event) => settings::set_global_settings(event, true).await,
			InboundEventType::GetGlobalSettings(event) => settings::get_global_settings(event, true).await,
			InboundEventType::OpenUrl(event) => misc::open_url(event).await,
			InboundEventType::LogMessage(event) => misc::log_message(None, event).await,
			InboundEventType::SendToPlugin(event) => property_inspector::send_to_plugin(event).await,
			_ => Ok(()),
		} && !error.to_string().contains("closed connection")
		{
			warn!("Failed to process incoming event from property inspector: {}", error);
		}
	}
}
