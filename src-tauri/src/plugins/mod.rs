pub mod info_param;
pub mod manifest;
mod webserver;

use crate::APP_HANDLE;
use crate::built_info::TARGET;
use crate::shared::{CATEGORIES, Category, config_dir, convert_icon, is_flatpak, log_dir};
use crate::store::get_settings;

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::LazyLock;
use std::{fs, path};

use tauri::{AppHandle, Manager};

use futures::StreamExt;
use tokio::net::{TcpListener, TcpStream};

use anyhow::anyhow;
use log::{error, warn};
use tokio::sync::{Mutex, RwLock};

enum PluginInstance {
	Webview,
	Wine(Child),
	Native(Child),
	Node(Child),
}

pub static DEVICE_NAMESPACES: LazyLock<RwLock<HashMap<String, String>>> = LazyLock::new(|| RwLock::new(HashMap::new()));
static INSTANCES: LazyLock<Mutex<HashMap<String, PluginInstance>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// UUIDs currently in-flight through the spawn path — specifically, between
/// the "we decided to spawn" point and the `INSTANCES.insert` at the end.
/// Prevents duplicate subprocesses when the startup parallel-spawn loop is
/// racing against a device register or profile switch that both reach for
/// the same plugin. Sync mutex so the claim can be released from Drop.
static SPAWNING_UUIDS: LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> = LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

fn try_claim_spawn(uuid: &str) -> bool {
	// HashSet::insert returns true iff the value was newly added — matches
	// our "succeeded if we claimed it first" semantic in one lookup.
	SPAWNING_UUIDS.lock().unwrap().insert(uuid.to_owned())
}

/// RAII guard: releases the SPAWNING_UUIDS claim when dropped. Applied after
/// a successful `try_claim_spawn` so every exit from the spawn path (return,
/// `?`, panic unwind) releases the claim.
struct SpawnClaimGuard<'a>(&'a str);
impl Drop for SpawnClaimGuard<'_> {
	fn drop(&mut self) {
		SPAWNING_UUIDS.lock().unwrap().remove(self.0);
	}
}

/// Telemetry: detect any plugin processes whose parent is NOT this OpenDeck.
/// Runs on startup (catches orphans left by a previous crash) and on clean
/// shutdown (catches anything that escaped the shutdown path). Does NOT kill.
/// Kernel PDEATHSIG and lazy-load should make orphans architecturally
/// impossible; this is pure telemetry so any regression becomes visible
/// immediately. Linux-only — other platforms have no /proc to scan.
#[cfg(target_os = "linux")]
pub fn log_orphan_plugin_telemetry(phase: &'static str) {
	let self_pid = std::process::id();
	let mut orphans: Vec<(u32, u32, String, u64)> = Vec::new();

	let Ok(entries) = fs::read_dir("/proc") else { return };
	for entry in entries.flatten() {
		let name = entry.file_name();
		let Some(pid_str) = name.to_str() else { continue };
		let Ok(pid) = pid_str.parse::<u32>() else { continue };
		if pid == self_pid {
			continue;
		}
		let cmdline_path = format!("/proc/{}/cmdline", pid);
		let Ok(cmdline_bytes) = fs::read(&cmdline_path) else { continue };
		// argv is NUL-separated; flag and UUID appear as adjacent tokens.
		let argv: Vec<&[u8]> = cmdline_bytes.split(|b| *b == 0).collect();
		let mut has_flag = false;
		let mut uuid = String::new();
		for (i, tok) in argv.iter().enumerate() {
			if tok == b"-pluginUUID" {
				has_flag = true;
				if let Some(next) = argv.get(i + 1) {
					uuid = String::from_utf8_lossy(next).into_owned();
				}
				break;
			}
		}
		if !has_flag {
			continue;
		}

		let Ok(stat) = fs::read_to_string(format!("/proc/{}/stat", pid)) else { continue };
		let ppid: u32 = (|| -> Option<u32> {
			let closing = stat.rfind(')')?;
			let rest = &stat[closing + 1..];
			let fields: Vec<&str> = rest.split_whitespace().collect();
			fields.get(1)?.parse().ok()
		})()
		.unwrap_or(0);
		if ppid == self_pid {
			continue;
		}
		let rss_kb: u64 = fs::read_to_string(format!("/proc/{}/status", pid))
			.ok()
			.and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1).and_then(|n| n.parse().ok())))
			.unwrap_or(0);
		orphans.push((pid, ppid, uuid, rss_kb));
	}

	if orphans.is_empty() {
		return;
	}

	// File persistence (orphan-plugin-telemetry.jsonl) was removed 2026-04-29
	// per the no-regular-disk-writes policy. The condition below is a
	// PDEATHSIG regression — visible via `pgrep -af sdPlugin` independently of
	// any log. We still emit a warn so it shows up when debug logging is on.
	for (pid, ppid, uuid, rss_kb) in &orphans {
		log::warn!("Orphan plugin ({}): pid={} uuid={} rss_kb={} (ppid={}, not this opendeck {})", phase, pid, uuid, rss_kb, ppid, self_pid);
	}
}

#[cfg(not(target_os = "linux"))]
pub fn log_orphan_plugin_telemetry(_phase: &'static str) {}

// Attach a kernel-enforced "die when parent dies" signal to a plugin child
// process. When OpenDeck crashes or is SIGKILLed, the kernel delivers SIGTERM
// to every plugin child that had this set in pre_exec. Without this, children
// reparent to PID 1 / user-systemd and run forever — the classic orphan leak
// documented in CLAUDE.md. Linux-only; PR_SET_PDEATHSIG has no macOS/Windows
// equivalent, those platforms need their own containment (see L2 cgroup work).
#[cfg(target_os = "linux")]
fn attach_parent_death_signal(cmd: &mut Command) {
	use std::os::unix::process::CommandExt;
	// Safety: pre_exec runs in the forked child between fork() and execve().
	// Only async-signal-safe calls are permitted. prctl is async-signal-safe.
	unsafe {
		cmd.pre_exec(|| {
			if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong, 0, 0, 0) != 0 {
				return Err(std::io::Error::last_os_error());
			}
			Ok(())
		});
	}
}

#[cfg(not(target_os = "linux"))]
fn attach_parent_death_signal(_cmd: &mut Command) {}

pub static PORT_BASE: LazyLock<u16> = LazyLock::new(|| {
	let mut base = 57116;
	loop {
		let websocket_result = std::net::TcpListener::bind(format!("0.0.0.0:{}", base));
		let webserver_result = std::net::TcpListener::bind(format!("0.0.0.0:{}", base + 2));
		if websocket_result.is_ok() && webserver_result.is_ok() {
			log::debug!("Using ports {} and {}", base, base + 2);
			break;
		}
		base += 1;
	}
	base
});

/// Walk the profiles directory and return the set of plugin UUIDs referenced
/// by any profile on disk. Used at startup by `initialise_plugins` to decide
/// which plugins to spawn. Plugins not in this set have their metadata
/// registered (so their actions appear in the profile editor UI) but their
/// subprocess is never started, avoiding the memory leak of idle plugins
/// running forever. Startup-only — lazy spawn during normal operation is
/// driven by the in-memory profile state via `ensure_plugin_spawned`.
fn compute_referenced_plugin_uuids() -> std::collections::HashSet<String> {
	use serde_json::Value;
	fn collect(v: &Value, out: &mut std::collections::HashSet<String>) {
		match v {
			Value::Object(map) => {
				if let Some(Value::String(s)) = map.get("plugin") {
					out.insert(s.clone());
				}
				for (_, child) in map {
					collect(child, out);
				}
			}
			Value::Array(arr) => {
				for item in arr {
					collect(item, out);
				}
			}
			_ => {}
		}
	}
	fn walk(p: &path::Path, out: &mut std::collections::HashSet<String>) {
		let Ok(entries) = fs::read_dir(p) else { return };
		for entry in entries.flatten() {
			let ep = entry.path();
			if ep.is_dir() {
				walk(&ep, out);
			} else if ep.extension().and_then(|e| e.to_str()) == Some("json") {
				let bytes = match fs::read(&ep) {
					Ok(b) => b,
					Err(error) => {
						warn!("Skipping profile file {} (read failed): {}", ep.display(), error);
						continue;
					}
				};
				match serde_json::from_slice::<Value>(&bytes) {
					Ok(v) => collect(&v, out),
					Err(error) => {
						warn!(
							"Skipping profile file {} (malformed JSON): {}. Plugins referenced only by this profile will not be spawned at startup.",
							ep.display(),
							error
						);
					}
				}
			}
		}
	}
	let mut uuids = std::collections::HashSet::new();
	walk(&config_dir().join("profiles"), &mut uuids);
	uuids
}

/// Spawn a plugin's subprocess if it isn't already running. Idempotent; safe
/// to call from the profile-switch path when a newly-selected profile
/// references a plugin whose process wasn't started at boot. Also checks
/// SPAWNING_UUIDS so a concurrent startup spawn of the same UUID isn't
/// duplicated.
pub async fn ensure_plugin_spawned(uuid: &str) {
	if INSTANCES.lock().await.contains_key(uuid) {
		return;
	}
	if SPAWNING_UUIDS.lock().unwrap().contains(uuid) {
		return;
	}
	let path = config_dir().join("plugins").join(uuid);
	if !path.exists() {
		return;
	}
	if let Err(error) = initialise_plugin(&path, true).await {
		warn!("Failed to lazily spawn plugin {}: {:#}", uuid, error);
	}
}

/// Debounce window for deactivation-on-settle. If the user is rapidly flipping
/// through profiles, no deactivation happens. Once they've settled on a
/// profile for this long, plugins used only by profiles they've left are
/// deactivated so memory reflects what's actually on screen.
const DEACTIVATION_SETTLE_SECS: u64 = 30;

static DEACTIVATION_SWEEP: LazyLock<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> = LazyLock::new(|| std::sync::Mutex::new(None));

/// Schedule a delayed deactivation sweep. Any pending sweep is cancelled and
/// replaced — the new timer starts from zero. Call this from profile switch,
/// device connect, and device disconnect; the set of "needed" plugins only
/// changes on those events. Synchronous (uses std::sync::Mutex) so the
/// scheduling itself doesn't span async boundaries and callers aren't pulled
/// into the sweep's Send bounds.
pub fn schedule_deactivation_sweep() {
	let new_handle = tokio::spawn(async {
		tokio::time::sleep(std::time::Duration::from_secs(DEACTIVATION_SETTLE_SECS)).await;
		run_deactivation_sweep().await;
	});
	let mut guard = DEACTIVATION_SWEEP.lock().unwrap();
	let old = guard.replace(new_handle);
	drop(guard);
	if let Some(h) = old {
		h.abort();
	}
}

/// Compute the set of plugin UUIDs referenced by the currently-selected
/// profile of each connected device. Used by the deactivation sweep to
/// decide what stays running. Copies out owned DeviceInfo values before any
/// await so DashMap Refs don't span await points (they aren't Send).
async fn compute_active_plugin_uuids() -> std::collections::HashSet<String> {
	use crate::shared::DEVICES;
	let mut needed = std::collections::HashSet::new();
	let devices: Vec<(String, crate::shared::DeviceInfo)> = DEVICES.iter().map(|e| (e.key().clone(), e.value().clone())).collect();
	if devices.is_empty() {
		return needed;
	}
	let mut locks = crate::store::profiles::acquire_locks_mut().await;
	for (device_id, device_info) in &devices {
		let Ok(profile_id) = locks.device_stores.get_selected_profile(device_id) else { continue };
		let Ok(store) = locks.profile_stores.get_profile_store(device_info, &profile_id) else { continue };
		for instance in store.value.keys.iter().flatten().chain(store.value.sliders.iter().flatten()) {
			needed.insert(instance.action.plugin.clone());
			if let Some(children) = &instance.children {
				for child in children {
					needed.insert(child.action.plugin.clone());
				}
			}
		}
	}
	needed
}

async fn run_deactivation_sweep() {
	let needed = compute_active_plugin_uuids().await;
	if needed.is_empty() {
		log::info!("[sweep] no connected devices — skipping");
		return;
	}
	let (running, to_remove): (Vec<String>, Vec<String>) = {
		let instances = INSTANCES.lock().await;
		let running: Vec<String> = instances.keys().cloned().collect();
		let to_remove: Vec<String> = running.iter().filter(|uuid| !needed.contains(uuid.as_str())).cloned().collect();
		(running, to_remove)
	};
	log::info!("[sweep] needed={:?} running={:?} to_remove={:?}", needed, running, to_remove);
	if to_remove.is_empty() {
		return;
	}
	log::info!("[sweep] removing {} unused plugin(s), keeping {} needed", to_remove.len(), needed.len());
	let Some(app) = APP_HANDLE.get() else { return };
	for uuid in &to_remove {
		log::info!("Deactivation sweep: stopping unused plugin {}", uuid);
		if let Err(error) = deactivate_plugin(app, uuid).await {
			warn!("Deactivation sweep failed to stop {}: {:#}", uuid, error);
		}
	}
}

/// Register a plugin's actions, categories, and device namespaces in
/// OpenDeck's in-memory maps, and optionally spawn its subprocess.
///
/// When `spawn_process` is `false`, only the metadata is registered — the
/// plugin's actions appear in the profile-editor UI but no subprocess is
/// started. This is used at startup for plugins not referenced by any
/// profile on disk, and matches the lazy-load behavior of the real Elgato
/// Stream Deck. When `spawn_process` is `true`, the full path runs: platform
/// detection, code_path resolution, subprocess spawn, registration in
/// `INSTANCES`. Call sites in user actions (install, reload) always pass
/// `true`; the startup loop passes `true` only for referenced plugins.
pub async fn initialise_plugin(path: &path::Path, spawn_process: bool) -> anyhow::Result<()> {
	let plugin_uuid = path.file_name().unwrap().to_str().unwrap();

	let mut manifest = manifest::read_manifest(path)?;

	if let Some(icon) = manifest.category_icon {
		let category_icon_path = path.join(icon);
		manifest.category_icon = Some(convert_icon(category_icon_path.to_string_lossy().to_string()));
	}

	for action in &mut manifest.actions {
		plugin_uuid.clone_into(&mut action.plugin);

		let action_icon_path = path.join(action.icon.clone());
		action.icon = convert_icon(action_icon_path.to_str().unwrap().to_owned());

		if !action.property_inspector.is_empty() {
			action.property_inspector = path.join(&action.property_inspector).to_string_lossy().to_string();
		} else if let Some(ref property_inspector) = manifest.property_inspector_path {
			action.property_inspector = path.join(property_inspector).to_string_lossy().to_string();
		}

		for state in &mut action.states {
			if state.image == "actionDefaultImage" {
				state.image.clone_from(&action.icon);
			} else {
				let state_icon = path.join(state.image.clone());
				state.image = convert_icon(state_icon.to_str().unwrap().to_owned());
			}

			match state.family.clone().to_lowercase().trim() {
				"arial" => "Liberation Sans",
				"arial black" => "Archivo Black",
				"comic sans ms" => "Comic Neue",
				"courier" | "Courier New" => "Courier Prime",
				"georgia" => "Tinos",
				"impact" => "Anton",
				"microsoft sans serif" | "Times New Roman" => "Liberation Serif",
				"tahoma" | "Verdana" => "Open Sans",
				"trebuchet ms" => "Fira Sans",
				_ => continue,
			}
			.clone_into(&mut state.family);
		}
	}

	{
		let mut categories = CATEGORIES.write().await;
		if let Some(category) = categories.get_mut(&manifest.category) {
			for action in manifest.actions {
				if let Some(index) = category.actions.iter().position(|v| v.uuid == action.uuid) {
					category.actions.remove(index);
				}
				category.actions.push(action);
			}
		} else {
			let mut category: Category = Category {
				icon: manifest.category_icon,
				actions: vec![],
			};
			for action in manifest.actions {
				category.actions.push(action);
			}
			if !category.actions.is_empty() {
				categories.insert(manifest.category, category);
			}
		}
	}

	if let Some(namespace) = manifest.device_namespace {
		DEVICE_NAMESPACES.write().await.insert(namespace, plugin_uuid.to_owned());
	}

	// Metadata-only path: if the caller isn't asking us to spawn the plugin's
	// subprocess (startup filter for unreferenced plugins), we're done.
	// Actions are in CATEGORIES so the UI can still show them; the plugin
	// will be spawned lazily by ensure_plugin_spawned when a profile that
	// references it is activated.
	if !spawn_process {
		return Ok(());
	}

	// Race guard: if this plugin is already running OR another invocation is
	// mid-spawn for the same UUID, bail out. Startup does N parallel
	// tokio::spawn(initialise_plugin) — if a device register fires during
	// that window, it would call ensure_plugin_spawned → initialise_plugin
	// for an in-flight UUID and we'd end up with two subprocesses, one of
	// which becomes orphaned when its Child handle is dropped (Unix: dropping
	// Child does NOT kill the process). The atomic claim below prevents it.
	if INSTANCES.lock().await.contains_key(plugin_uuid) {
		log::info!("[initialise_plugin] {plugin_uuid} already in INSTANCES — skipping spawn");
		return Ok(());
	}
	if !try_claim_spawn(plugin_uuid) {
		log::info!("[initialise_plugin] {plugin_uuid} spawn already in progress — skipping duplicate");
		return Ok(());
	}
	let _claim = SpawnClaimGuard(plugin_uuid);
	log::info!("[initialise_plugin] {plugin_uuid} claimed spawn, proceeding");

	#[cfg(target_os = "windows")]
	let platform = "windows";
	#[cfg(target_os = "macos")]
	let platform = "mac";
	#[cfg(target_os = "linux")]
	let platform = "linux";

	let mut code_path = manifest.code_path;
	let mut use_wine = false;
	let mut supported = false;

	// Determine the method used to run the plugin based on its supported operating systems and the current operating system.
	for os in manifest.os {
		if os.platform == platform {
			#[cfg(target_os = "windows")]
			if manifest.code_path_windows.is_some() {
				code_path = manifest.code_path_windows.clone();
			}
			#[cfg(target_os = "macos")]
			if manifest.code_path_macos.is_some() {
				code_path = manifest.code_path_macos;
			}
			#[cfg(target_os = "linux")]
			if manifest.code_path_linux.is_some() {
				code_path = manifest.code_path_linux;
			}
			code_path = manifest.code_paths.and_then(|p| p.get(TARGET).cloned()).or(code_path);

			use_wine = false;

			supported = true;
			break;
		} else if os.platform == "windows" {
			use_wine = true;
			supported = true;
		}
	}

	if code_path.is_none() && use_wine {
		code_path = manifest.code_path_windows;
	}

	if !supported || code_path.is_none() {
		return Err(anyhow!("unsupported on platform {}", platform));
	}

	let code_path = code_path.unwrap();
	let port_string = PORT_BASE.to_string();
	let args = ["-port", port_string.as_str(), "-pluginUUID", plugin_uuid, "-registerEvent", "registerPlugin", "-info"];

	if code_path.to_lowercase().ends_with(".html") || code_path.to_lowercase().ends_with(".htm") || code_path.to_lowercase().ends_with(".xhtml") {
		let url = format!("http://localhost:{}/", *PORT_BASE + 2) + path.join(code_path).to_str().unwrap();
		let window = tauri::WebviewWindowBuilder::new(APP_HANDLE.get().unwrap(), plugin_uuid.replace('.', "_"), tauri::WebviewUrl::External(url.parse()?))
			.title(plugin_uuid)
			.visible(false)
			.build()?;

		if fs::exists(path.join("debug")).unwrap_or(false) {
			let _ = window.show();
			window.open_devtools();
		}

		let info = info_param::make_info(plugin_uuid.to_owned(), manifest.version, false).await;
		window.eval(format!(
			r#"const opendeckInit = () => {{
				try {{
					if (document.readyState !== "complete") throw new Error("not ready");
					if (typeof connectOpenActionSocket === "function") connectOpenActionSocket({port}, "{uuid}", "{event}", `{info}`);
					else connectElgatoStreamDeckSocket({port}, "{uuid}", "{event}", `{info}`);
				}} catch (e) {{
					setTimeout(opendeckInit, 10);
				}}
			}};
			opendeckInit();
			"#,
			port = *PORT_BASE,
			uuid = plugin_uuid,
			event = "registerPlugin",
			info = serde_json::to_string(&info)?
		))?;

		INSTANCES.lock().await.insert(plugin_uuid.to_owned(), PluginInstance::Webview);
	} else if code_path.to_lowercase().ends_with(".js") || code_path.to_lowercase().ends_with(".mjs") || code_path.to_lowercase().ends_with(".cjs") {
		// Check for Node.js installation and version in one go.
		let command = if is_flatpak() { "flatpak-spawn" } else { "node" };
		let extra_args = if is_flatpak() { vec!["--host", "node"] } else { vec![] };
		let version_output = Command::new(command).args(&extra_args).arg("--version").output();
		if version_output.is_err() || String::from_utf8(version_output.unwrap().stdout).unwrap().trim() < "v20.0.0" {
			return Err(anyhow!("Node.js version 20.0.0 or higher is required"));
		}

		let info = info_param::make_info(plugin_uuid.to_owned(), manifest.version, true).await;
		let log_file = fs::File::create(log_dir().join("plugins").join(format!("{plugin_uuid}.log")))?;

		#[cfg(target_os = "windows")]
		{
			use std::os::windows::process::CommandExt;
			let child = Command::new(command)
				.current_dir(path)
				.args(extra_args)
				.arg(code_path)
				.args(args)
				.arg(serde_json::to_string(&info)?)
				.stdout(Stdio::from(log_file.try_clone()?))
				.stderr(Stdio::from(log_file))
				.creation_flags(0x08000000)
				.spawn()?;

			INSTANCES.lock().await.insert(plugin_uuid.to_owned(), PluginInstance::Node(child));
		}

		#[cfg(not(target_os = "windows"))]
		{
			let mut cmd = Command::new(command);
			cmd.current_dir(path)
				.args(extra_args)
				.arg(code_path)
				.args(args)
				.arg(serde_json::to_string(&info)?)
				.stdout(Stdio::from(log_file.try_clone()?))
				.stderr(Stdio::from(log_file));
			attach_parent_death_signal(&mut cmd);
			let child = cmd.spawn()?;
			let child_pid = child.id();
			log::info!("[initialise_plugin] {plugin_uuid} Node spawn ok pid={child_pid}");

			INSTANCES.lock().await.insert(plugin_uuid.to_owned(), PluginInstance::Node(child));
		}
	} else if use_wine {
		let command = if is_flatpak() { "flatpak-spawn" } else { "wine" };
		let extra_args = if is_flatpak() { vec!["--host", "wine"] } else { vec![] };
		let result = Command::new(command)
			.args(&extra_args)
			.arg("--version")
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()
			.and_then(|mut child| child.wait())
			.map(|status| status.success());
		if !matches!(result, Ok(true)) {
			return Err(anyhow!("failed to detect an installation of Wine"));
		}

		let info = info_param::make_info(plugin_uuid.to_owned(), manifest.version, true).await;
		let log_file = fs::File::create(log_dir().join("plugins").join(format!("{plugin_uuid}.log")))?;

		let mut command = Command::new(command);
		command
			.current_dir(path)
			.args(extra_args)
			.arg(code_path)
			.args(args)
			.arg(serde_json::to_string(&info)?)
			.stdout(Stdio::from(log_file.try_clone()?))
			.stderr(Stdio::from(log_file));
		if get_settings()?.value.separatewine {
			command.env("WINEPREFIX", path.join("wineprefix").to_string_lossy().to_string());
		} else {
			let _ = fs::remove_dir_all(path.join("wineprefix"));
		}
		attach_parent_death_signal(&mut command);
		let child = command.spawn()?;

		INSTANCES.lock().await.insert(plugin_uuid.to_owned(), PluginInstance::Wine(child));
	} else {
		let info = info_param::make_info(plugin_uuid.to_owned(), manifest.version, false).await;
		let log_file = fs::File::create(log_dir().join("plugins").join(format!("{plugin_uuid}.log")))?;

		#[cfg(target_os = "windows")]
		{
			use std::os::windows::process::CommandExt;
			let child = Command::new(path.join(code_path))
				.current_dir(path)
				.args(args)
				.arg(serde_json::to_string(&info)?)
				.stdout(Stdio::from(log_file.try_clone()?))
				.stderr(Stdio::from(log_file))
				.creation_flags(0x08000000)
				.spawn()?;

			INSTANCES.lock().await.insert(plugin_uuid.to_owned(), PluginInstance::Native(child));
		}

		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			fs::set_permissions(path.join(&code_path), fs::Permissions::from_mode(0o755))?;
		}

		#[cfg(not(target_os = "windows"))]
		{
			let mut cmd = Command::new(path.join(code_path));
			cmd.current_dir(path)
				.args(args)
				.arg(serde_json::to_string(&info)?)
				.stdout(Stdio::from(log_file.try_clone()?))
				.stderr(Stdio::from(log_file));
			attach_parent_death_signal(&mut cmd);
			let child = cmd.spawn()?;

			INSTANCES.lock().await.insert(plugin_uuid.to_owned(), PluginInstance::Native(child));
		}
	}

	if let Some(applications) = manifest.applications_to_monitor
		&& let Some(applications) = applications.get(platform)
	{
		crate::application_watcher::start_monitoring(plugin_uuid, applications).await;
	}

	Ok(())
}

pub async fn deactivate_plugin(app: &AppHandle, uuid: &str) -> Result<(), anyhow::Error> {
	// Namespace + virtual-device cleanup. Errors here MUST NOT short-circuit
	// the INSTANCES removal below — if we let a deregister_device failure
	// propagate with `?`, the plugin's subprocess stays running while its
	// namespace is already gone, and subsequent sweeps can't find it cleanly.
	// Log any error and continue to the process-kill path.
	let namespace_cleanup: Result<(), anyhow::Error> = async {
		let mut namespaces = DEVICE_NAMESPACES.write().await;
		if let Some((namespace, _)) = namespaces.clone().iter().find(|(_, plugin)| uuid == **plugin) {
			namespaces.remove(namespace);
			drop(namespaces);
			let devices = crate::shared::DEVICES.iter().map(|v| v.key().to_owned()).filter(|id| &id[..2] == namespace).collect::<Vec<_>>();
			for device in devices {
				crate::events::inbound::devices::deregister_device("", crate::events::inbound::PayloadEvent { payload: device }).await?;
			}
			crate::events::frontend::update_devices().await;
		}
		Ok(())
	}
	.await;
	if let Err(error) = namespace_cleanup {
		warn!("deactivate_plugin({}): namespace cleanup failed, continuing to kill subprocess: {:#}", uuid, error);
	}

	crate::application_watcher::stop_monitoring(uuid).await;

	if let Some(instance) = INSTANCES.lock().await.remove(uuid) {
		match instance {
			PluginInstance::Webview => {
				if let Some(window) = app.get_webview_window(&uuid.replace('.', "_")) {
					window.close()?;
					tokio::time::sleep(std::time::Duration::from_millis(10)).await;
				}
			}
			PluginInstance::Node(mut child) | PluginInstance::Wine(mut child) | PluginInstance::Native(mut child) => {
				child.kill()?;
				child.wait()?;
			}
		}
		Ok(())
	} else {
		Err(anyhow!("instance of plugin {} not found", uuid))
	}
}

pub async fn deactivate_plugins() {
	let uuids = {
		let instances = INSTANCES.lock().await;
		instances.keys().cloned().collect::<Vec<_>>()
	};

	let app = APP_HANDLE.get().unwrap();
	for uuid in uuids {
		let _ = deactivate_plugin(app, &uuid).await;
	}
}

/// Initialise plugins from the plugins directory.
pub fn initialise_plugins() {
	// Telemetry pass: any plugin process alive at startup whose parent isn't
	// us must be a leftover from a previous crash — log it so we know the
	// guarantees still hold.
	log_orphan_plugin_telemetry("startup");

	tokio::spawn(init_websocket_server());
	tokio::spawn(webserver::init_webserver(config_dir()));

	let plugin_dir = config_dir().join("plugins");
	let _ = fs::create_dir_all(&plugin_dir);
	let _ = fs::create_dir_all(log_dir().join("plugins"));

	if let Ok(Ok(entries)) = APP_HANDLE.get().unwrap().path().resolve("plugins", tauri::path::BaseDirectory::Resource).map(fs::read_dir) {
		for entry in entries.flatten() {
			if let Err(error) = (|| -> Result<(), anyhow::Error> {
				let builtin_version = semver::Version::parse(&serde_json::from_slice::<manifest::PluginManifest>(&fs::read(entry.path().join("manifest.json"))?)?.version)?;
				let existing_path = plugin_dir.join(entry.file_name());
				if (|| -> Result<(), anyhow::Error> {
					let existing_version = semver::Version::parse(&serde_json::from_slice::<manifest::PluginManifest>(&fs::read(existing_path.join("manifest.json"))?)?.version)?;
					if existing_version < builtin_version {
						Err(anyhow::anyhow!("builtin version is newer than existing version"))
					} else {
						Ok(())
					}
				})()
				.is_err()
				{
					if existing_path.exists() {
						fs::rename(&existing_path, existing_path.with_extension("old"))?;
					}
					if crate::shared::copy_dir(entry.path(), &existing_path).is_err() && existing_path.with_extension("old").exists() {
						fs::rename(existing_path.with_extension("old"), &existing_path)?;
					}
					let _ = fs::remove_dir_all(existing_path.with_extension("old"));
				}
				Ok(())
			})() {
				error!("Failed to upgrade builtin plugin {}: {}", entry.file_name().to_string_lossy(), error);
			}
		}
	}

	let entries = match fs::read_dir(&plugin_dir) {
		Ok(p) => p,
		Err(error) => {
			error!("Failed to read plugins directory at {}: {}", plugin_dir.display(), error);
			panic!()
		}
	};

	// Compute the set of plugins actually referenced by any profile on disk.
	// Plugins NOT in this set will have metadata registered but their process
	// will not be spawned — this matches real Elgato Stream Deck behavior and
	// stops OpenDeck from eagerly launching every installed plugin regardless
	// of whether it's used. Plugins required by a profile activated after
	// startup are spawned lazily via ensure_plugin_spawned.
	let referenced = std::sync::Arc::new(compute_referenced_plugin_uuids());
	log::info!("Startup: {} plugins referenced by profiles on disk", referenced.len());

	// Iterate through all directory entries in the plugins folder and initialise them as plugins if appropriate
	for entry in entries {
		if let Ok(entry) = entry {
			let path = match entry.metadata().unwrap().is_symlink() {
				true => entry.path().parent().unwrap_or_else(|| path::Path::new(".")).join(fs::read_link(entry.path()).unwrap()),
				false => entry.path(),
			};
			let metadata = fs::metadata(&path).unwrap();
			if metadata.is_dir() {
				let referenced = referenced.clone();
				tokio::spawn(async move {
					let uuid = path.file_name().and_then(|n| n.to_str()).map(String::from).unwrap_or_default();
					let spawn_process = referenced.contains(&uuid);
					if !spawn_process {
						log::info!("Registering {} metadata only (not referenced by any profile)", uuid);
					}
					if let Err(error) = initialise_plugin(&path, spawn_process).await {
						warn!("Failed to initialise plugin at {}: {:#}", path.display(), error);
					}
				});
			}
		} else if let Err(error) = entry {
			warn!("Failed to read entry of plugins directory: {}", error)
		}
	}

	// On macOS, hidden WKWebView windows suspend JavaScript after ~7s.
	// Periodically eval a no-op to keep them alive.
	#[cfg(target_os = "macos")]
	tokio::spawn(async {
		use tauri::Manager;
		let app = APP_HANDLE.get().unwrap();
		loop {
			tokio::time::sleep(std::time::Duration::from_secs(3)).await;
			let instances = INSTANCES.lock().await;
			for (uuid, _) in instances.iter().filter(|(_, instance)| matches!(instance, PluginInstance::Webview)) {
				if let Some(window) = app.get_webview_window(&uuid.replace('.', "_")) {
					let _ = window.eval("void(0);");
				}
			}
		}
	});
}

/// Start the WebSocket server that plugins communicate with.
async fn init_websocket_server() {
	let listener = match TcpListener::bind(format!("0.0.0.0:{}", *PORT_BASE)).await {
		Ok(listener) => listener,
		Err(error) => {
			error!("Failed to bind plugin WebSocket server to socket: {}", error);
			return;
		}
	};

	#[cfg(windows)]
	{
		use std::os::windows::io::AsRawSocket;
		use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

		unsafe { SetHandleInformation(listener.as_raw_socket() as _, HANDLE_FLAG_INHERIT, 0) };
	}

	while let Ok((stream, _)) = listener.accept().await {
		tokio::spawn(accept_connection(stream));
	}
}

/// Handle incoming data from a WebSocket connection.
async fn accept_connection(stream: TcpStream) {
	let mut socket = match tokio_tungstenite::accept_async(stream).await {
		Ok(socket) => socket,
		Err(error) => {
			warn!("Failed to complete WebSocket handshake: {}", error);
			return;
		}
	};

	let Ok(register_event) = socket.next().await.unwrap() else {
		return;
	};
	match serde_json::from_str(&register_event.clone().into_text().unwrap()) {
		Ok(event) => crate::events::register_plugin(event, socket).await,
		Err(_) => {
			// Non-plugin connection: process the first message and keep
			// reading until the client disconnects. This allows plugins to
			// open secondary connections for custom events (createChild,
			// triggerChildPress, etc.) without registering as a plugin.
			let _ = crate::events::inbound::process_incoming_message(Ok(register_event), "", true).await;
			while let Some(Ok(msg)) = socket.next().await {
				let _ = crate::events::inbound::process_incoming_message(Ok(msg), "", true).await;
			}
		}
	}
}
