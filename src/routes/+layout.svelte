<script>
	import "../app.css";
	// Load telemetry module at app startup. It wraps window.addEventListener
	// and exports trackedListen for Tauri events. Logs snapshots every 30s
	// to console with prefix [OPENDECK_TELEMETRY].
	import "$lib/telemetry";
</script>

<slot />
