use super::Error;

use crate::built_info;

use tauri::command;
#[cfg(not(debug_assertions))]
use tauri_plugin_autostart::ManagerExt;

#[command]
pub async fn get_settings() -> Result<crate::store::Settings, Error> {
	let store = crate::store::get_settings();
	match store {
		Ok(store) => Ok(store.value),
		Err(error) => Err(error.into()),
	}
}

#[command]
pub async fn set_settings(_app: tauri::AppHandle, settings: crate::store::Settings) -> Result<(), Error> {
	#[cfg(not(debug_assertions))]
	let _ = match settings.autolaunch {
		true => _app.autolaunch().enable(),
		false => _app.autolaunch().disable(),
	};

	crate::events::outbound::devices::set_brightness(settings.brightness).await?;
	crate::device_sleep::update_timeout_minutes(settings.sleep_timeout_minutes);
	let mut store = match crate::store::get_settings() {
		Ok(store) => store,
		Err(error) => return Err(error.into()),
	};

	store.value = settings;
	store.save()?;
	Ok(())
}

#[command]
pub fn open_config_directory() -> Result<(), Error> {
	if let Err(error) = open::that_detached(crate::shared::config_dir()) {
		return Err(anyhow::Error::from(error).into());
	}
	Ok(())
}

#[command]
pub fn open_log_directory() -> Result<(), Error> {
	if let Err(error) = open::that_detached(crate::shared::log_dir()) {
		return Err(anyhow::Error::from(error).into());
	}
	Ok(())
}

/// Frontend handle for the debug-logging toggle. `mode` is one of:
/// `"off"`, `"1h"`, `"4h"`, `"24h"`, `"permanent"`. Persists choice to
/// settings and applies the log-level change in-process. See
/// docs/DEBUG-LOGGING.md.
#[command]
pub async fn set_debug_log_window(mode: String) -> Result<(), Error> {
	use std::time::{SystemTime, UNIX_EPOCH};
	let mut store = match crate::store::get_settings() {
		Ok(s) => s,
		Err(e) => return Err(e.into()),
	};
	let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
	let (until, permanent) = match mode.as_str() {
		"off" => (None, false),
		"1h" => (Some(now + 3600), false),
		"4h" => (Some(now + 4 * 3600), false),
		"24h" => (Some(now + 24 * 3600), false),
		"permanent" => (None, true),
		other => return Err(anyhow::anyhow!("unknown debug-log mode: {other}").into()),
	};
	store.value.debug_log_until_ts = until;
	store.value.debug_log_permanent = permanent;
	store.save()?;
	crate::debug_log_gate::reapply();
	Ok(())
}

#[command]
pub fn get_build_info() -> String {
	format!(
		r#"
		<details>
			<summary> {} v{} ({}) on {} </summary>
			{}
		</details>
		"#,
		crate::shared::PRODUCT_NAME,
		built_info::PKG_VERSION,
		built_info::GIT_COMMIT_HASH_SHORT.unwrap_or("commit hash unknown"),
		built_info::TARGET,
		built_info::DIRECT_DEPENDENCIES_STR
	)
}
