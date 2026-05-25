<script lang="ts">
	import type { ActionInstance } from "$lib/ActionInstance";
	import type { Context } from "$lib/Context";
	import type { DeviceInfo } from "$lib/DeviceInfo";
	import type { Profile } from "$lib/Profile";
	import type { CopiedItem } from "$lib/propertyInspector";

	import EncoderDial from "./EncoderDial.svelte";
	import Key from "./Key.svelte";

	import { inspectedInstance, inspectedParentAction } from "$lib/propertyInspector";
	import { dragAction, hoveredSlot } from "$lib/dragState";
	import { uiScale } from "$lib/uiScale";

	import { invoke } from "@tauri-apps/api/core";

	export let device: DeviceInfo;
	export let profile: Profile;

	export let selectedDevice: string;

	let allProfileIds: string[] = [];
	$: if (device) {
		invoke<string[]>("get_profiles", { device: device.id }).then(ids => allProfileIds = ids);
	}

	// Resolve swipe neighbors: explicit config or alphabetical fallback
	function resolveSwipeNeighbor(direction: "left" | "right"): string {
		const explicit = direction === "left" ? profile.swipe_left : profile.swipe_right;
		if (explicit) return explicit;
		if (allProfileIds.length < 2) return profile.id;
		const sorted = [...allProfileIds].sort();
		const idx = sorted.indexOf(profile.id);
		if (idx === -1) return sorted[0];
		if (direction === "left") return sorted[(idx + sorted.length - 1) % sorted.length];
		return sorted[(idx + 1) % sorted.length];
	}
	$: resolvedLeft = resolveSwipeNeighbor("left");
	$: resolvedRight = resolveSwipeNeighbor("right");

	async function setSwipeNeighbor(direction: "left" | "right", value: string) {
		if (direction === "left") profile.swipe_left = value;
		else profile.swipe_right = value;
		await invoke("set_swipe_neighbor", {
			device: device.id,
			profile: profile.id,
			direction,
			target: value,
		});
	}

	function handleDragStart({ dataTransfer }: DragEvent, controller: string, position: number) {
		if (!dataTransfer) return;
		dataTransfer.effectAllowed = "move";
		dataTransfer.setData("controller", controller);
		dataTransfer.setData("position", position.toString());
	}

	function handleDragOver(event: DragEvent, controller?: string, position?: number) {
		event.preventDefault();
		if (!event.dataTransfer) return;
		if (event.dataTransfer.types.includes("action")) event.dataTransfer.dropEffect = "copy";
		else if (event.dataTransfer.types.includes("controller")) event.dataTransfer.dropEffect = "move";
		if (controller != null && position != null) hoveredSlot.set({ controller, position });
	}

	async function handleDrop({ dataTransfer }: DragEvent, controller: string, position: number) {
		hoveredSlot.set(null);
		let context = { device: device.id, profile: profile.id, controller, position };
		let array = controller == "Encoder" ? profile.sliders : profile.keys;
		if (dataTransfer?.getData("action")) {
			let action = JSON.parse(dataTransfer?.getData("action"));
			if (array[position]) {
				await invoke("remove_instance", { context: array[position].context });
				array[position] = null;
			}
			array[position] = await invoke("create_instance", { context, action });
			profile = profile;
		} else if (dataTransfer?.getData("controller")) {
			let oldArray = dataTransfer?.getData("controller") == "Encoder" ? profile.sliders : profile.keys;
			let oldPosition = parseInt(dataTransfer?.getData("position"));
			let response: ActionInstance = await invoke("move_instance", {
				source: { device: device.id, profile: profile.id, controller: dataTransfer?.getData("controller"), position: oldPosition },
				destination: context,
				retain: false,
			});
			if (response) {
				array[position] = response;
				oldArray[oldPosition] = null;
				profile = profile;
			}
		}
	}

	async function handlePaste(item: CopiedItem, destination: Context) {
		let array = destination.controller == "Encoder" ? profile.sliders : profile.keys;

		if (item.type == "action") {
			if (array[destination.position]) return;
			array[destination.position] = await invoke("create_instance", { context: destination, action: item.action });
			profile = profile;
			return;
		}

		let response: ActionInstance = await invoke("move_instance", { source: item.source, destination, retain: true });
		if (response) {
			array[destination.position] = response;
			profile = profile;
		}
	}

	// Grid navigation: track focused cell and compute row lengths for arrow key movement.
	let focusedRow = 0;
	let focusedCol = 0;

	$: gridRowLengths = [
		...Array(device.rows).fill(device.columns),
		...(device.encoders > 0 ? [device.encoders] : []),
		...(device.touchpoints > 0 ? [device.touchpoints] : []),
	];
	$: encoderRowIndex = device.rows;
	$: touchpointRowIndex = device.rows + (device.encoders > 0 ? 1 : 0);

	function flatIndexFromRowCol(row: number, col: number): number {
		let index = 0;
		for (let r = 0; r < row; r++) index += gridRowLengths[r];
		return index + col;
	}

	function rowColFromFlatIndex(flatIndex: number): [number, number] {
		let remaining = flatIndex;
		for (let r = 0; r < gridRowLengths.length; r++) {
			if (remaining < gridRowLengths[r]) return [r, remaining];
			remaining -= gridRowLengths[r];
		}
		return [0, 0];
	}

	function handleGridKeydown(event: KeyboardEvent) {
		const target = event.target as HTMLElement;
		if (target.getAttribute("role") !== "gridcell") return;
		if (!["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;

		event.preventDefault();
		event.stopPropagation();

		let newRow = focusedRow;
		let newCol = focusedCol;

		switch (event.key) {
			case "ArrowRight":
				newCol = Math.min(focusedCol + 1, gridRowLengths[focusedRow] - 1);
				break;
			case "ArrowLeft":
				newCol = Math.max(focusedCol - 1, 0);
				break;
			case "ArrowDown":
				newRow = Math.min(focusedRow + 1, gridRowLengths.length - 1);
				newCol = Math.min(focusedCol, gridRowLengths[newRow] - 1);
				break;
			case "ArrowUp":
				newRow = Math.max(focusedRow - 1, 0);
				newCol = Math.min(focusedCol, gridRowLengths[newRow] - 1);
				break;
			case "Home":
				newCol = 0;
				break;
			case "End":
				newCol = gridRowLengths[focusedRow] - 1;
				break;
		}

		if (newRow === focusedRow && newCol === focusedCol) return;

		focusedRow = newRow;
		focusedCol = newCol;

		const grid = event.currentTarget as HTMLElement;
		const cells = grid.querySelectorAll("[role='gridcell']");
		(cells[flatIndexFromRowCol(newRow, newCol)] as HTMLElement)?.focus();
	}

	function handleGridFocusin(event: FocusEvent) {
		const grid = event.currentTarget as HTMLElement;
		const cells = Array.from(grid.querySelectorAll("[role='gridcell']"));
		const index = cells.indexOf(event.target as Element);
		if (index === -1) return;
		[focusedRow, focusedCol] = rowColFromFlatIndex(index);
	}
</script>


{#key device}
	<span id="grid-description" class="sr-only">Use arrow keys to navigate between keys. Moving to a key will display its property inspector.</span>
	<div
		class="flex flex-col justify-center grow px-16 py-6 overflow-auto"
		class:items-center={device.columns <= 8 || device.columns === 9}
		class:hidden={$inspectedParentAction || selectedDevice != device.id}
		role="grid"
		aria-label={device.name}
		aria-describedby="grid-description"
		tabindex="-1"
		on:click={() => inspectedInstance.set(null)}
		on:keyup={() => inspectedInstance.set(null)}
		on:keydown|capture={handleGridKeydown}
		on:focusin={handleGridFocusin}
	>
		<div style="transform: scale({$uiScale}); transform-origin: center top;">
		<div class="flex flex-col" role="rowgroup">
			{#each { length: device.rows } as _, r}
				<div class="flex flex-row" role="row">
					{#each { length: device.columns } as _, c}
						{@const pos = (r * device.columns) + c}
						{@const isCompat = $dragAction ? $dragAction.controllers.includes("Keypad") : false}
						{@const isHovered = $hoveredSlot?.controller === "Keypad" && $hoveredSlot?.position === pos}
						{@const isEmpty = !profile.keys[pos]}
						<Key
							context={{ device: device.id, profile: profile.id, controller: "Keypad", position: pos }}
							bind:inslot={profile.keys[pos]}
							on:dragover={(event) => handleDragOver(event, "Keypad", pos)}
							on:dragleave={() => hoveredSlot.set(null)}
							on:drop={(event) => handleDrop(event, "Keypad", pos)}
							on:dragstart={(event) => handleDragStart(event, "Keypad", pos)}
							{handlePaste}
							size={device.rows == 4 && device.columns == 9 ? 120 : device.id.startsWith("sd-") && device.rows == 4 && device.columns == 8 ? 192 : 144}
							label="Key {String.fromCharCode(65 + r)}{c + 1}"
							tabindex={focusedRow === r && focusedCol === c ? 0 : -1}
							dragHighlight={$dragAction ? (isCompat ? (isHovered ? "hovered" : (isEmpty ? "empty" : "occupied")) : "incompatible") : null}
						/>
					{/each}
				</div>
			{/each}
		</div>

		{#if device.encoders > 0}
			{#if allProfileIds.length > 1}
				<div class="flex justify-between items-center mx-auto mt-1 mb-0.5" style="width: {device.encoders > 4 ? (device.encoders * 132) : (device.columns <= 8 ? (device.columns * 132) : (device.columns * 144))}px;">
					<label class="flex items-center gap-1.5 px-2 py-1 rounded cursor-pointer hover:bg-neutral-800 transition-colors">
						<span class="text-sm text-neutral-500">&#x2190;</span>
						<div class="select-profile-wrapper" style="padding-right: 12px;">
							<select
								value={profile.swipe_left || resolvedLeft}
								on:change={(e) => setSwipeNeighbor("left", e.currentTarget.value)}
							>
								{#each allProfileIds.filter(p => p !== profile.id) as pid}
									<option value={pid}>{pid}</option>
								{/each}
							</select>
						</div>
					</label>
					<span class="text-[10px] text-neutral-500 uppercase tracking-wider">Swipe to profile</span>
					<label class="flex items-center gap-1.5 px-2 py-1 rounded cursor-pointer hover:bg-neutral-800 transition-colors">
						<div class="select-profile-wrapper" style="padding-right: 12px;">
							<select
								value={profile.swipe_right || resolvedRight}
								on:change={(e) => setSwipeNeighbor("right", e.currentTarget.value)}
							>
								{#each allProfileIds.filter(p => p !== profile.id) as pid}
									<option value={pid}>{pid}</option>
								{/each}
							</select>
						</div>
						<span class="text-sm text-neutral-500">&#x2192;</span>
					</label>
				</div>
			{/if}
			<div class="flex justify-center" role="row">
			<div class="flex flex-row items-start justify-center gap-0" style="width: {device.columns * 132}px;">
			<div class="flex flex-row items-start justify-center gap-0 flex-1">
				{#each { length: device.encoders } as _, i}
					{@const isCompat = $dragAction ? $dragAction.controllers.includes("Encoder") : false}
					{@const isHovered = $hoveredSlot?.controller === "Encoder" && $hoveredSlot?.position === i}
					{@const isEmpty = !profile.sliders[i]}
					{@const encHighlight = $dragAction ? (isCompat ? (isHovered ? "hovered" : (isEmpty ? "empty" : "occupied")) : "incompatible") : null}
					<div
						class="flex flex-col items-center transition-all duration-150"
						class:opacity-30={$dragAction && !isCompat}
						class:brightness-125={isHovered}
						style="flex: 1;"
						on:dragover|preventDefault={(event) => handleDragOver(event, "Encoder", i)}
						on:dragleave={() => hoveredSlot.set(null)}
						on:drop={(event) => handleDrop(event, "Encoder", i)}
					>
						<div class="encoder-strip flex flex-row w-full">
							<Key
								context={{ device: device.id, profile: profile.id, controller: "Encoder", position: i }}
								bind:inslot={profile.sliders[i]}
								on:dragstart={(event) => handleDragStart(event, "Encoder", i)}
								{handlePaste}
								encoderStrip
								encoderPosition={i}
								encoderCount={device.encoders}
								label="Encoder {i + 1}"
								tabindex={focusedRow === encoderRowIndex && focusedCol === i ? 0 : -1}
								dragHighlight={encHighlight}
							/>
						</div>
						<EncoderDial context={{ device: device.id, profile: profile.id, controller: "Encoder", position: i }} {encHighlight} />
					</div>
				{/each}
			</div>
			</div>
			</div>
		{/if}

		<div class="flex flex-row" role="row">
			{#each { length: device.touchpoints } as _, i}
				{@const tpos = (device.rows * device.columns) + i}
				{@const isCompat = $dragAction ? $dragAction.controllers.includes("Keypad") : false}
				{@const isHovered = $hoveredSlot?.controller === "Keypad" && $hoveredSlot?.position === tpos}
				{@const isEmpty = !profile.keys[tpos]}
				<Key
					context={{ device: device.id, profile: profile.id, controller: "Keypad", position: tpos }}
					bind:inslot={profile.keys[tpos]}
					on:dragover={(event) => handleDragOver(event, "Keypad", tpos)}
					on:dragleave={() => hoveredSlot.set(null)}
					on:drop={(event) => handleDrop(event, "Keypad", tpos)}
					on:dragstart={(event) => handleDragStart(event, "Keypad", tpos)}
					dragHighlight={$dragAction ? (isCompat ? (isHovered ? "hovered" : (isEmpty ? "empty" : "occupied")) : "incompatible") : null}
					{handlePaste}
					size={device.id.startsWith("sd-") && device.rows == 4 && device.columns == 8 ? 192 : 144}
					isTouchPoint
					label="Touch point {i + 1}"
					tabindex={focusedRow === touchpointRowIndex && focusedCol === i ? 0 : -1}
				/>
			{/each}
		</div>
		</div>
	</div>
{/key}
