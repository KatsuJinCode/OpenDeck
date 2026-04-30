// Telemetry module -- observe what's happening inside the webview.
// Written after a 4GB/hour memory leak in the WebKitWebProcess was discovered.
// Logs a snapshot every 30 seconds to the webview console with prefix
// [OPENDECK_TELEMETRY] and also invokes the Rust `log_telemetry` command
// which appends to ~/.local/share/opendeck/telemetry.jsonl for persistent
// analysis outside the webview.
//
// The module wraps Tauri's `listen()` and `window.addEventListener` with
// counters so we can see listener accumulation across the session.
// Components that receive high-rate events (feedback_changed, update_state)
// can call `recordEventReceived()` and `recordReactiveRun()` to track
// event throughput and reactive re-render rate.

import { listen as tauriListen, type UnlistenFn, type Event } from "@tauri-apps/api/event";

type Counts = Record<string, number>;

// --- Tauri listener tracking ---
let tauriListenersActive = 0;
let tauriListenersCreated = 0;
const tauriListenersByEvent: Counts = {};

export async function trackedListen<T>(
	event: string,
	handler: (event: Event<T>) => void,
): Promise<UnlistenFn> {
	tauriListenersCreated++;
	tauriListenersActive++;
	tauriListenersByEvent[event] = (tauriListenersByEvent[event] || 0) + 1;
	const unlisten = await tauriListen<T>(event, handler);
	let called = false;
	return () => {
		if (called) return;
		called = true;
		tauriListenersActive--;
		tauriListenersByEvent[event] = Math.max(0, (tauriListenersByEvent[event] || 0) - 1);
		return unlisten();
	};
}

// --- window.addEventListener tracking ---
let windowListenersActive = 0;
let windowListenersCreated = 0;
const windowListenersByType: Counts = {};

// Wrap window.addEventListener/removeEventListener defensively. If wrapping
// fails for any reason (frozen window, CSP, etc.), leave the originals alone
// and proceed with zero window-listener tracking — better than crashing the app.
try {
	const originalAdd = window.addEventListener.bind(window);
	const originalRemove = window.removeEventListener.bind(window);

	window.addEventListener = function (type: string, listener: any, options?: any): void {
		windowListenersActive++;
		windowListenersCreated++;
		windowListenersByType[type] = (windowListenersByType[type] || 0) + 1;
		return originalAdd(type, listener, options);
	} as any;

	window.removeEventListener = function (type: string, listener: any, options?: any): void {
		windowListenersActive = Math.max(0, windowListenersActive - 1);
		windowListenersByType[type] = Math.max(0, (windowListenersByType[type] || 0) - 1);
		return originalRemove(type, listener, options);
	} as any;
} catch (e) {
	console.warn("[OPENDECK_TELEMETRY] failed to wrap window.addEventListener", e);
}

// --- Event receive & reactive run counters ---
const eventReceiveCounts: Counts = {};
const reactiveRunCounts: Counts = {};

export function recordEventReceived(name: string): void {
	eventReceiveCounts[name] = (eventReceiveCounts[name] || 0) + 1;
}

export function recordReactiveRun(label: string): void {
	reactiveRunCounts[label] = (reactiveRunCounts[label] || 0) + 1;
}

// --- Assignment tracking for leak diagnosis ---
// Wrap reactive assignments: `slot = trackAssign("slot", newValue)`.
// Lets us see which vars are actually being written at what rate, and which
// var was most recently written when a reactive block fires.
let lastAssignedVar: string = "none";
export function trackAssign<T>(varName: string, value: T): T {
	recordReactiveRun(`assign.${varName}`);
	lastAssignedVar = varName;
	return value;
}
export function getLastAssignedVar(): string {
	return lastAssignedVar;
}

// --- In-flight counter for async reactive blocks ---
const inFlightCounts: Record<string, { current: number; peak: number }> = {};
export function markInFlightEnter(label: string): void {
	if (!inFlightCounts[label]) inFlightCounts[label] = { current: 0, peak: 0 };
	inFlightCounts[label].current++;
	if (inFlightCounts[label].current > inFlightCounts[label].peak) {
		inFlightCounts[label].peak = inFlightCounts[label].current;
	}
}
export function markInFlightExit(label: string): void {
	if (!inFlightCounts[label]) return;
	inFlightCounts[label].current = Math.max(0, inFlightCounts[label].current - 1);
}
export function getInFlightSnapshot(): Record<string, { current: number; peak: number }> {
	const snap: Record<string, { current: number; peak: number }> = {};
	for (const [k, v] of Object.entries(inFlightCounts)) {
		snap[k] = { current: v.current, peak: v.peak };
		// Reset peak after snapshot so we see per-window peaks
		v.peak = v.current;
	}
	return snap;
}

// --- Snapshot machinery ---
const START_TIME = Date.now();
let lastSnapshotTime = START_TIME;
let lastEventCounts: Counts = {};
let lastReactiveCounts: Counts = {};

function delta(current: Counts, previous: Counts): Counts {
	const d: Counts = {};
	for (const k of Object.keys(current)) {
		d[k] = (current[k] || 0) - (previous[k] || 0);
	}
	return d;
}

function ratePerSec(d: Counts, elapsedSec: number): Counts {
	const r: Counts = {};
	for (const k of Object.keys(d)) {
		r[k] = +(d[k] / elapsedSec).toFixed(2);
	}
	return r;
}

function snapshot(): void {
	const now = Date.now();
	const elapsedSec = (now - lastSnapshotTime) / 1000;

	// WebKit/Chromium heap info (may be undefined in some WebKit builds)
	const mem = (performance as any).memory as
		| { usedJSHeapSize: number; totalJSHeapSize: number; jsHeapSizeLimit: number }
		| undefined;

	const eventDelta = delta(eventReceiveCounts, lastEventCounts);
	const reactiveDelta = delta(reactiveRunCounts, lastReactiveCounts);

	const payload = {
		ts: new Date().toISOString(),
		uptime_sec: Math.round((now - START_TIME) / 1000),
		in_flight: getInFlightSnapshot(),
		heap: mem
			? {
					used_mb: Math.round(mem.usedJSHeapSize / 1024 / 1024),
					total_mb: Math.round(mem.totalJSHeapSize / 1024 / 1024),
					limit_mb: Math.round(mem.jsHeapSizeLimit / 1024 / 1024),
				}
			: null,
		listeners: {
			tauri_active: tauriListenersActive,
			tauri_created: tauriListenersCreated,
			tauri_by_event: { ...tauriListenersByEvent },
			window_active: windowListenersActive,
			window_created: windowListenersCreated,
			window_by_type: { ...windowListenersByType },
		},
		dom: {
			total_nodes: document.getElementsByTagName("*").length,
			canvas_count: document.querySelectorAll("canvas").length,
			iframe_count: document.querySelectorAll("iframe").length,
		},
		events_last_window: eventDelta,
		events_rate_per_sec: ratePerSec(eventDelta, elapsedSec),
		reactive_last_window: reactiveDelta,
		reactive_rate_per_sec: ratePerSec(reactiveDelta, elapsedSec),
	};

	console.log("[OPENDECK_TELEMETRY]", JSON.stringify(payload));

	// Disk persistence (telemetry.jsonl) was removed 2026-04-29: the file grew
	// to 161 MB, nothing read it, and the in-memory counters plus the console
	// emission above are sufficient for live debugging via DevTools. See
	// OpenDeck-fork/docs/DISK-WRITE-POLICY.md.

	lastSnapshotTime = now;
	lastEventCounts = { ...eventReceiveCounts };
	lastReactiveCounts = { ...reactiveRunCounts };
}

// Initial snapshot after brief settle, then every 30s
window.setTimeout(snapshot, 2_000);
window.setInterval(snapshot, 30_000);
