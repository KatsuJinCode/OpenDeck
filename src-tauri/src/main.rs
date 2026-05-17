// Prevents additional console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod application_watcher;
mod carry;
mod debug_log_gate;
mod device_sleep;
mod disk_io;
mod dump;
mod elgato;
mod events;
mod plugins;
mod shared;
mod store;
mod zip_extract;

mod built_info {
	include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

use events::frontend;
use shared::PRODUCT_NAME;

use std::sync::OnceLock;

use tauri::{
	AppHandle, Builder, Manager, WindowEvent,
	menu::{IconMenuItemBuilder, MenuBuilder, MenuItemBuilder, PredefinedMenuItem},
	tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tauri_plugin_log::{Target, TargetKind};

static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

// Frontend used to invoke a `log_telemetry` command that appended every event
// to `~/.local/share/opendeck/logs/telemetry.jsonl`. Removed 2026-04-29 — the
// file was unbounded (grew to 161 MB) and nothing read it. The in-memory
// counters in `src/lib/telemetry.ts` still expose live data via dev tools.
// See docs/DISK-WRITE-POLICY.md.

fn show_window(app: &AppHandle) -> Result<(), tauri::Error> {
	#[cfg(target_os = "macos")]
	{
		use tauri::ActivationPolicy;
		let _ = app.set_activation_policy(ActivationPolicy::Regular);
	}

	let window = app.get_webview_window("main").ok_or_else(|| tauri::Error::WebviewNotFound)?;
	window.show().and_then(|_| window.set_focus())
}

fn hide_window(app: &AppHandle) -> Result<(), tauri::Error> {
	let window = app.get_webview_window("main").ok_or_else(|| tauri::Error::WebviewNotFound)?;
	window.hide()?;

	#[cfg(target_os = "macos")]
	{
		use tauri::ActivationPolicy;
		let _ = app.set_activation_policy(ActivationPolicy::Accessory);
	}

	Ok(())
}

#[tokio::main]
async fn main() {
	log_panics::init();
	let _ = fix_path_env::fix();
	debug_log_gate::capture_cli_override();

	#[cfg(target_os = "linux")]
	// SAFETY: std::env::set_var can cause race conditions in multithreaded contexts. We have not spawned any other threads at this point.
	unsafe {
		std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
		std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
	}

	let app = match Builder::default()
		.invoke_handler(tauri::generate_handler![
			frontend::restart,
			frontend::get_devices,
			frontend::get_port_base,
			frontend::get_categories,
			frontend::get_localisations,
			frontend::get_applications,
			frontend::get_application_profiles,
			frontend::set_application_profiles,
			frontend::get_fonts,
			frontend::instances::create_instance,
			frontend::instances::move_instance,
			frontend::instances::remove_instance,
			frontend::instances::set_state,
			frontend::instances::update_image,
			frontend::instances::trigger_virtual_press,
			frontend::instances::trigger_virtual_rotate,
			frontend::instances::trigger_virtual_encoder_down,
			frontend::instances::trigger_virtual_encoder_up,
			frontend::instances::trigger_virtual_touch,
			frontend::instances::toggle_skip_persistence,
			frontend::profiles::get_profiles,
			frontend::profiles::get_selected_profile,
			frontend::profiles::set_selected_profile,
			frontend::profiles::delete_profile,
			frontend::profiles::rename_profile,
			frontend::profiles::set_swipe_neighbor,
			frontend::property_inspector::make_info,
			frontend::property_inspector::switch_property_inspector,
			frontend::property_inspector::open_url,
			frontend::plugins::list_plugins,
			frontend::plugins::install_plugin,
			frontend::plugins::remove_plugin,
			frontend::plugins::reload_plugin,
			frontend::plugins::show_settings_interface,
			frontend::plugins::get_feedback_layout,
			frontend::settings::get_settings,
			frontend::settings::set_settings,
			frontend::settings::open_config_directory,
			frontend::settings::open_log_directory,
			frontend::settings::get_build_info,
			frontend::settings::set_debug_log_window,
			disk_io::get_disk_write_rate,
			dump::dump_preview_state,
			dump::get_slot_feedback,
		])
		.setup(|app| {
			APP_HANDLE.set(app.handle().clone()).unwrap();
			dump::spawn_trigger_watcher();
			debug_log_gate::apply_initial_state();

			#[cfg(windows)]
			if !std::env::args().any(|v| v == "--hide") {
				let _ = app.get_webview_window("main").unwrap().show();
			}
			#[cfg(not(windows))]
			if std::env::args().any(|v| v == "--hide") {
				let _ = hide_window(app.handle());
			}

			let old = app.path().config_dir().unwrap().join("com.amansprojects.opendeck");
			if old.exists() {
				let _ = std::fs::rename(old, app.path().app_config_dir().unwrap());
			}

			let mut settings = store::get_settings()?;
			use std::cmp::Ordering;
			use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
			let current_version = semver::Version::parse(built_info::PKG_VERSION)?;
			let settings_version = semver::Version::parse(&settings.value.version)?;
			let cmp = (current_version.major, current_version.minor).cmp(&(settings_version.major, settings_version.minor));
			match cmp {
				Ordering::Less => {
					app.get_webview_window("main").unwrap().close().unwrap();
					app.dialog()
						.message(format!(
							"A newer version of {PRODUCT_NAME} created configuration files on this device. This version is v{}; please upgrade to v{} or newer.",
							built_info::PKG_VERSION,
							settings.value.version
						))
						.title(format!("{PRODUCT_NAME} upgrade required"))
						.kind(MessageDialogKind::Error)
						.show(|_| APP_HANDLE.get().unwrap().exit(1));
					return Ok(());
				}
				Ordering::Greater => {
					let old_version = settings.value.version.clone();
					settings.value.version = built_info::PKG_VERSION.to_owned();
					settings.save()?;
					if old_version == "0.0.0" {
						app.dialog()
							.message(format!(
								r#"Thanks for installing {PRODUCT_NAME}!
If you have any issues, please reach out on any of the support channels listed on GitHub (and make sure to star the project while you're there!).

Some minimal statistics (such as operating system and plugins installed) will be collected from the next time the app starts.
If you do not wish to support development in this way, please disable statistics in the settings.

Enjoy!"#,
							))
							.title(format!("{PRODUCT_NAME} has successfully been installed"))
							.kind(MessageDialogKind::Info)
							.show(|_| ());
						settings.value.statistics = false;
					} else {
						app.dialog()
							.message(format!(
								r#"{PRODUCT_NAME} has been updated to v{}!
Every update brings features, bug fixes, and other improvements, which I spend my time implementing for free.

If you spent $125 on your hardware, please consider spending $10 on the software that makes it work.
You can donate to support development with just a few clicks on GitHub Sponsors.
If you have already donated, thank you so much for your support!"#,
								built_info::PKG_VERSION
							))
							.title(format!("{PRODUCT_NAME} has successfully been updated"))
							.kind(MessageDialogKind::Info)
							.show(|_| ());
					}
				}
				_ => {}
			}

			use tauri_plugin_aptabase::{Builder, EventTracker, InitOptions};
			app.handle().plugin(
				Builder::new(if settings.value.statistics { "A-SH-3841489320" } else { "" })
					.with_options(InitOptions {
						host: Some("https://aptabase.amankhanna.me".to_owned()),
						flush_interval: None,
					})
					.build(),
			)?;
			let _ = app.track_event("app_started", None);

			tokio::spawn(async {
				loop {
					elgato::initialise_devices().await;
					tokio::time::sleep(std::time::Duration::from_secs(10)).await;
				}
			});
			plugins::initialise_plugins();
			application_watcher::init_application_watcher();
			device_sleep::init_device_sleep();

			let label = IconMenuItemBuilder::with_id("label", PRODUCT_NAME)
				.icon(app.default_window_icon().unwrap().clone())
				.enabled(false)
				.build(app)?;
			let show = MenuItemBuilder::with_id("show", "Show").build(app)?;
			let hide = MenuItemBuilder::with_id("hide", "Hide").build(app)?;
			let restart = MenuItemBuilder::with_id("restart", "Restart").build(app)?;
			let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
			let separator = PredefinedMenuItem::separator(app)?;
			let menu = MenuBuilder::new(app).items(&[&label, &separator, &show, &hide, &separator, &restart, &quit]).build()?;
			let _tray = TrayIconBuilder::with_id("opendeck")
				.menu(&menu)
				.icon(app.default_window_icon().unwrap().clone())
				.show_menu_on_left_click(false)
				.on_tray_icon_event(move |icon, event| {
					if let TrayIconEvent::Click { button, button_state, .. } = event {
						if button != MouseButton::Left || button_state != MouseButtonState::Down {
							return;
						}

						let app_handle = icon.app_handle();
						let window = app_handle.get_webview_window("main").unwrap();
						let _ = if window.is_visible().unwrap_or(false) { hide_window(app_handle) } else { show_window(app_handle) };
					}
				})
				.on_menu_event(move |app, event| {
					let _ = match event.id().as_ref() {
						"show" => show_window(app),
						"hide" => hide_window(app),
						"restart" => app.restart(),
						"quit" => {
							app.exit(0);
							Ok(())
						}
						_ => Ok(()),
					};
				})
				.build(app)?;

			#[cfg(any(target_os = "linux", all(debug_assertions, windows)))]
			{
				use tauri_plugin_deep_link::DeepLinkExt;
				let _ = app.deep_link().register_all();
			}

			// Upstream update-check disabled at compile time in this fork. We
			// track upstream manually via `git fetch upstream && git merge`; an
			// in-app nag that pops on every launch is exactly the kind of toil
			// that has no place running unattended. If we ever want to re-enable
			// it, revert this block, not flip a setting.
			//
			// Original code (the `update()` async fn + `if settings.value.updatecheck { ... }`
			// dispatch) lives in git history. Do not restore without explicit
			// Julian approval — see project issue 260516-opendeck-update-nag-must-never-appear-again.md.

			Ok(())
		})
		.plugin(
			tauri_plugin_log::Builder::default()
				.targets([Target::new(TargetKind::LogDir { file_name: None }), Target::new(TargetKind::Stdout)])
				.level(log::LevelFilter::Info)
				.level_for("opendeck", log::LevelFilter::Trace)
				.build(),
		)
		.plugin(tauri_plugin_cors_fetch::init())
		.plugin(
			tauri_plugin_single_instance::Builder::new()
				.callback(|app, args, _| {
					if let Some(pos) = args.iter().position(|x| x.starts_with("openaction://") || x.starts_with("streamdeck://"))
						&& let Ok(url) = reqwest::Url::parse(&args[pos])
						&& let Some(mut path) = url.path_segments()
					{
						if url.host_str() == Some("plugins")
							&& path.next() == Some("message")
							&& let Some(plugin_id) = path.next()
						{
							if !url.query_pairs().any(|(k, v)| k == url.scheme() && v == "hidden") {
								let _ = show_window(app);
							}

							let plugin_id = if url.scheme() == "streamdeck" { format!("{plugin_id}.sdPlugin") } else { plugin_id.to_owned() };
							let url = args[pos].clone();
							std::thread::spawn(move || {
								tauri::async_runtime::block_on(async move {
									if let Err(error) = events::outbound::deep_link::did_receive_deep_link(&plugin_id, url).await {
										log::error!("Failed to process deep link for plugin {plugin_id}: {error}");
									}
								});
							});
						}
					} else if let Some(pos) = args.iter().position(|x| x.to_lowercase().trim() == "--reload-plugin") {
						if args.len() > pos + 1 {
							let app = app.clone();
							let plugin_id = args[pos + 1].clone();
							// MUST spawn on tauri's async runtime (tokio worker threads that
							// live as long as the app), NOT std::thread::spawn + block_on.
							// Rationale: `initialise_plugin` forks a child process with
							// PR_SET_PDEATHSIG=SIGTERM. Per prctl(2), PDEATHSIG fires when
							// the THREAD that forked the child exits — not when the parent
							// process exits. A std::thread that dies the moment block_on
							// returns kills the new plugin child a few milliseconds later.
							// Observed empirically: plugin spawned with valid PID, then
							// SIGTERM'd before it could exec. Use async_runtime so the
							// forking thread (a tokio worker) outlives the child.
							tauri::async_runtime::spawn(async move {
								frontend::plugins::reload_plugin(app, plugin_id).await;
							});
						}
					} else if let Some(pos) = args.iter().position(|x| x.to_lowercase().trim() == "--sleep-device") {
						if args.len() > pos + 1 {
							let device_id = args[pos + 1].clone();
							std::thread::spawn(move || {
								if let Err(error) = tauri::async_runtime::block_on(device_sleep::sleep_device(device_id)) {
									log::error!("Failed to sleep device: {error}");
								}
							});
						}
					} else if let Some(pos) = args.iter().position(|x| x.to_lowercase().trim() == "--process-message") {
						if args.len() > pos + 1 {
							let message = args[pos + 1].clone();
							std::thread::spawn(move || {
								tauri::async_runtime::block_on(events::inbound::process_incoming_message(Ok(tokio_tungstenite::tungstenite::Message::Text(message.into())), "", true));
							});
						}
					} else {
						let _ = show_window(app);
					}
				})
				.dbus_id("me.amankhanna.opendeck")
				.build(),
		)
		.plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, Some(vec!["--hide"])))
		.plugin(tauri_plugin_dialog::init())
		.plugin(tauri_plugin_deep_link::init())
		.on_window_event(|window, event| {
			if window.label() != "main" {
				return;
			}
			if let WindowEvent::CloseRequested { api, .. } = event {
				if let Ok(true) = store::get_settings().map(|store| store.value.background) {
					let _ = hide_window(window.app_handle());
					api.prevent_close();
				} else {
					window.app_handle().exit(0);
				}
			}
		})
		.build(tauri::generate_context!())
	{
		Ok(app) => app,
		Err(error) => panic!("failed to build Tauri application: {}", error),
	};

	app.run(|app, event| {
		if let tauri::RunEvent::Exit = event {
			futures::executor::block_on(plugins::deactivate_plugins());
			tokio::spawn(elgato::reset_devices());
			use tauri_plugin_aptabase::EventTracker;
			app.flush_events_blocking();
			// Telemetry AFTER flush so a slow /proc scan on a busy system
			// can't make the aptabase flush race its grace window.
			plugins::log_orphan_plugin_telemetry("post-shutdown");
		}
	});
}
