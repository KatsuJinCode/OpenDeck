<script lang="ts">
	import { invoke } from "@tauri-apps/api/core";
	import { onDestroy, onMount } from "svelte";
	import Tooltip from "./Tooltip.svelte";

	type DiskWriteRate = { bytes_per_sec: number | null; cumulative_bytes: number };

	export let pollIntervalMs = 2000;

	let bytesPerSec: number | null = null;
	let timer: ReturnType<typeof setInterval> | null = null;

	async function poll() {
		try {
			const result = (await invoke("get_disk_write_rate")) as DiskWriteRate;
			bytesPerSec = result.bytes_per_sec;
		} catch {
			bytesPerSec = null;
		}
	}

	onMount(() => {
		poll();
		timer = setInterval(poll, pollIntervalMs);
	});

	onDestroy(() => {
		if (timer) clearInterval(timer);
	});

	function formatRate(bps: number | null): string {
		if (bps === null) return "—";
		if (bps < 1024) return `${bps} B/s`;
		if (bps < 1024 * 1024) return `${(bps / 1024).toFixed(1)} KB/s`;
		return `${(bps / (1024 * 1024)).toFixed(2)} MB/s`;
	}

	$: label = bytesPerSec === null
		? "Disk: —"
		: bytesPerSec === 0
			? "Disk: idle"
			: `Disk: ${formatRate(bytesPerSec)}`;

	// Severity colours: idle is muted, low rates are normal, sustained
	// kilobytes are amber, sustained megabytes are red. Single-frame spikes
	// are common (a profile save can flash a few KB) so this is for
	// at-a-glance health, not precision diagnosis.
	$: colourClass = bytesPerSec === null || bytesPerSec === 0
		? "text-neutral-400"
		: bytesPerSec < 50_000
			? "text-neutral-300"
			: bytesPerSec < 500_000
				? "text-amber-400"
				: "text-red-400";
</script>

<div class="inline-flex items-center space-x-1 text-xs {colourClass}" data-disk-io-indicator>
	<span>{label}</span>
	<Tooltip>
		Live disk-write rate from this OpenDeck process. Idle (0 B/s) is the goal — the only writes that should happen during normal use are when you change a profile or plugin setting. If you see a sustained rate above zero, a plugin is pushing image updates that are being persisted to disk. To stop that, open Settings and toggle "Skip image persistence by default" on. See OpenDeck-fork/docs/DISK-WRITE-POLICY.md.
	</Tooltip>
</div>
