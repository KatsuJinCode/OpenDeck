<script lang="ts">
	import type { ActionInstance } from "$lib/ActionInstance";
	import type { ActionState } from "$lib/ActionState";
	import type { Context } from "$lib/Context";
	import type { CopiedItem } from "$lib/propertyInspector";

	import Clipboard from "phosphor-svelte/lib/Clipboard";
	import Copy from "phosphor-svelte/lib/Copy";
	import Pencil from "phosphor-svelte/lib/Pencil";
	import Trash from "phosphor-svelte/lib/Trash";
	import InstanceEditor from "./InstanceEditor.svelte";

	import { copiedItem, inspectedInstance, inspectedParentAction, openContextMenu } from "$lib/propertyInspector";
	import { CanvasLock, renderImage } from "$lib/rendererHelper";
	import { getBuiltIn, type Layout } from "$lib/feedbackLayouts";
	import { renderFeedback } from "$lib/feedbackRenderer";
	import { settings } from "$lib/settings";

	import { invoke } from "@tauri-apps/api/core";
	import { trackedListen as listen } from "$lib/telemetry";
	import { recordEventReceived, recordReactiveRun, trackAssign, getLastAssignedVar, markInFlightEnter, markInFlightExit } from "$lib/telemetry";
	import { onDestroy, tick, beforeUpdate, afterUpdate, onMount } from "svelte";

	export let context: Context | null;
	export let label: string = "";
	export let tabindex: number = 0;
	export let role: string = "gridcell";

	// One-way binding for slot data.
	export let inslot: ActionInstance | null;
	let slot: ActionInstance | null;
	let prevSlotContext: string | null = null;
	const update = (inslot: ActionInstance | null) => {
		if (inslot && context && inslot.context.split(".")[0] != context.device) return;
		const newCtx = inslot?.context ?? null;
		if (newCtx !== prevSlotContext && context?.controller === "Encoder" && inslot) {
			awaitingFirstFeedback = trackAssign("awaitingFirstFeedback", true);
			initialRenderDone = trackAssign("initialRenderDone", false);
		}
		prevSlotContext = trackAssign("prevSlotContext", newCtx);
		slot = trackAssign("slot.fromUpdate", inslot);
	};
	$: { recordReactiveRun("Key.update"); update(inslot); }

	export let active: boolean = true;
	export let scale: number = 1;
	export let isTouchPoint: boolean = false;
	export let encoderStrip: boolean = false;
	export let dragHighlight: "empty" | "occupied" | "hovered" | "incompatible" | null = null;
	export let encoderPosition: number = 0;
	export let encoderCount: number = 4;
	let pressed: boolean = false;

	let state: ActionState | undefined;
	$: {
		recordReactiveRun("Key.state");
		if (!slot) {
			state = trackAssign("state", undefined);
		} else {
			state = trackAssign("state", slot.states[slot.current_state]);
		}
	}

	// Per-context event name: Rust emits to `update_state::${ctx}` so this
	// Key receives only events for its own context. No broadcast waste.
	// Key's context prop has {device, profile, controller, position}; the full
	// backend context includes `.{index}` — main slots are always index 0.
	// Tauri event names disallow dots, so ":" is used as separator to match backend.
	const ownContextString = context
		? `${context.device}:${context.profile}:${context.controller}:${context.position}:0`
		: "";
	const unlistenUpdateState = listen(`update_state::${ownContextString}`, ({ payload }: { payload: { context: string; contents: ActionInstance | null } }) => {
		recordEventReceived("update_state");
		if (payload.context == slot?.context) slot = trackAssign("slot.fromUpdateState", payload.contents);
	});

	// Plugin updates to setFeedback / setFeedbackLayout are broadcast from the
	// backend so any encoder slot listening for its own context can re-render
	// the layout and push the resulting pixmap to the device.
	type FeedbackEvent = { context: string; plugin: string; layout: string | null; feedback: Record<string, unknown> | null };
	// After a profile switch, suppress device pushes until the plugin sends
	// its first real setFeedback. One initial render (stale image) is allowed
	// so the user sees which page they're on, then further renders are held
	// until the plugin is live. Prevents the multi-frame flash during the
	// gap between willAppear and the plugin's first push.
	let awaitingFirstFeedback = false;
	let initialRenderDone = false;
	const unlistenFeedbackChanged = listen(`feedback_changed::${ownContextString}`, async ({ payload }: { payload: FeedbackEvent }) => {
		recordEventReceived("feedback_changed");
		if (!slot || payload.context !== slot.context) return;
		if (awaitingFirstFeedback && payload.feedback) {
			awaitingFirstFeedback = trackAssign("awaitingFirstFeedback", false);
		}
		slot = trackAssign("slot.fromFeedback", { ...slot, feedback_layout: payload.layout ?? slot.feedback_layout, feedback: payload.feedback ?? undefined });
	});

	// Resolve the current layout definition. Built-ins are defined client-side;
	// custom layouts are loaded from the plugin folder via a Tauri command.
	let resolvedLayout: Layout | null = null;
	let resolvedLayoutId: string | null = null;
	async function resolveLayout(plugin: string, layoutId: string | null | undefined): Promise<Layout | null> {
		if (!layoutId) return null;
		if (layoutId.startsWith("$")) return getBuiltIn(layoutId) ?? null;
		try {
			const raw = await invoke<Layout>("get_feedback_layout", { plugin, layout: layoutId });
			return raw ?? null;
		} catch (err) {
			console.warn(`[OpenDeck] failed to load custom layout ${layoutId} for ${plugin}:`, err);
			return null;
		}
	}
	// Per Elgato SDK, encoder feedback renders via a layout. When the slot
	// has no layout set (plugin manifest omitted Encoder.layout, or a key
	// action was dropped on an encoder slot), fall back to the built-in
	// $X1 "Icon" layout so the LCD shows title+icon unstretched.
	$: if (slot && context?.controller === "Encoder" && (slot.feedback_layout ?? "$X1") !== resolvedLayoutId) {
		recordReactiveRun("Key.layoutResolve");
		const pluginId = slot.action.plugin;
		const layoutId = slot.feedback_layout ?? "$X1";
		resolvedLayoutId = trackAssign("resolvedLayoutId", layoutId);
		resolveLayout(pluginId, layoutId).then((layout) => {
			if (resolvedLayoutId === layoutId) resolvedLayout = trackAssign("resolvedLayout", layout);
		});
	}

	const unlistenKeyMoved = listen("key_moved", ({ payload }: { payload: { context: Context; pressed: boolean } }) => {
		if (JSON.stringify(context) == JSON.stringify(payload.context)) pressed = trackAssign("pressed", payload.pressed);
	});

	function select(event: MouseEvent | KeyboardEvent) {
		if (event instanceof MouseEvent && event.ctrlKey) return;
		$openContextMenu = null;
		if (!slot) {
			$inspectedInstance = context;
			return;
		}
		if (slot.action.uuid == "opendeck.multiaction" || slot.action.uuid == "opendeck.toggleaction") {
			$inspectedParentAction = context;
		} else {
			$inspectedInstance = slot.context;
		}
	}

	function onfocus() {
		$openContextMenu = null;
		if (!slot) {
			$inspectedInstance = context;
			return;
		}
		if (slot.action.uuid != "opendeck.multiaction" && slot.action.uuid != "opendeck.toggleaction") {
			$inspectedInstance = slot.context;
		} else {
			$inspectedInstance = context;
		}
	}

	let contextMenuEl: HTMLDivElement;
	async function contextMenu(event: MouseEvent | KeyboardEvent) {
		event.preventDefault();
		if (!active || !context) return;
		const rect = canvas.getBoundingClientRect();
		let x = (event instanceof MouseEvent && event.x) ? event.x : rect.left;
		let y = (event instanceof MouseEvent && event.y) ? event.y : rect.bottom;
		$openContextMenu = { context, x, y };
		await tick();
		contextMenuEl?.querySelector("button")?.focus();
	}

	let showEditor = false;
	function edit() {
		$openContextMenu = null;
		showEditor = trackAssign("showEditor.true", true);
	}

	function copy() {
		$openContextMenu = null;
		if (!context || !slot) return;
		copiedItem.set({ type: "instance", source: context });
	}

	export let handlePaste: ((item: CopiedItem, destination: Context) => Promise<void>) | undefined = undefined;
	async function paste() {
		$openContextMenu = null;
		if (!$copiedItem || !context || !handlePaste) return;
		await handlePaste($copiedItem, context);
		await tick();
		$inspectedInstance = `${context.device}.${context.profile}.${context.controller}.${context.position}.0`;
	}

	async function toggleSkipPersistence() {
		if (!slot || !context) return;
		const newValue: boolean | null = await invoke("toggle_skip_persistence", { context });
		slot.skip_persistence = newValue;
	}

	async function clear() {
		$openContextMenu = null;
		if (!slot) return;
		await invoke("remove_instance", { context: slot.context });
		showEditor = trackAssign("showEditor.false", false);
		slot = null;
		inslot = slot;
		await tick();
		$inspectedInstance = context;
	}

	let showAlert: boolean = false;
	let showOk: boolean = false;
	let timeouts: number[] = [];
	const unlistenShowAlert = listen("show_alert", ({ payload }: { payload: string }) => {
		if (!slot || payload != slot.context) return;
		timeouts.forEach(clearTimeout);
		showOk = trackAssign("showOk.false.alert", false);
		showAlert = trackAssign("showAlert.true", true);
		timeouts.push(setTimeout(() => showAlert = trackAssign("showAlert.false.timeout", false), 1.5e3));
	});
	const unlistenShowOk = listen("show_ok", ({ payload }: { payload: string }) => {
		if (!slot || payload != slot.context) return;
		timeouts.forEach(clearTimeout);
		showAlert = trackAssign("showAlert.false.ok", false);
		showOk = trackAssign("showOk.true", true);
		timeouts.push(setTimeout(() => showOk = trackAssign("showOk.false.timeout", false), 1.5e3));
	});

	onDestroy(async () => {
		(await unlistenUpdateState)();
		(await unlistenFeedbackChanged)();
		(await unlistenKeyMoved)();
		(await unlistenShowAlert)();
		(await unlistenShowOk)();
	});

	let canvas: HTMLCanvasElement;
	let previewComposeCanvas: HTMLCanvasElement | undefined;
	let lock = new CanvasLock();
	export let size = 144;
	// Instrumentation to find the 1200/sec untracked trigger.
	// Each prop/store gets its own reactive block — Svelte fires these only
	// when that specific dep changes. Rates tell us which dep is changing 1200/sec.
	// Svelte lifecycle hooks — count every component update cycle
	beforeUpdate(() => recordReactiveRun("Key.lifecycle.beforeUpdate"));
	afterUpdate(() => recordReactiveRun("Key.lifecycle.afterUpdate"));
	onMount(() => recordReactiveRun("Key.lifecycle.mount"));
	onDestroy(() => recordReactiveRun("Key.lifecycle.destroy"));

	// Every prop gets its own reactive tracker
	$: { recordReactiveRun("Key.prop.context"); context; }
	$: { recordReactiveRun("Key.prop.label"); label; }
	$: { recordReactiveRun("Key.prop.tabindex"); tabindex; }
	$: { recordReactiveRun("Key.prop.role"); role; }
	$: { recordReactiveRun("Key.prop.inslot"); inslot; }
	$: { recordReactiveRun("Key.prop.active"); active; }
	$: { recordReactiveRun("Key.prop.scale"); scale; }
	$: { recordReactiveRun("Key.prop.isTouchPoint"); isTouchPoint; }
	$: { recordReactiveRun("Key.prop.encoderStrip"); encoderStrip; }
	$: { recordReactiveRun("Key.prop.dragHighlight"); dragHighlight; }
	$: { recordReactiveRun("Key.prop.encoderPosition"); encoderPosition; }
	$: { recordReactiveRun("Key.prop.encoderCount"); encoderCount; }
	$: { recordReactiveRun("Key.prop.handlePaste"); handlePaste; }
	$: { recordReactiveRun("Key.prop.size"); size; }

	// Every component-local reactive let gets its own tracker
	$: { recordReactiveRun("Key.local.slot"); slot; }
	$: { recordReactiveRun("Key.local.prevSlotContext"); prevSlotContext; }
	$: { recordReactiveRun("Key.local.pressed"); pressed; }
	$: { recordReactiveRun("Key.local.state"); state; }
	$: { recordReactiveRun("Key.local.awaitingFirstFeedback"); awaitingFirstFeedback; }
	$: { recordReactiveRun("Key.local.initialRenderDone"); initialRenderDone; }
	$: { recordReactiveRun("Key.local.resolvedLayout"); resolvedLayout; }
	$: { recordReactiveRun("Key.local.resolvedLayoutId"); resolvedLayoutId; }
	$: { recordReactiveRun("Key.local.contextMenuEl"); contextMenuEl; }
	$: { recordReactiveRun("Key.local.showEditor"); showEditor; }
	$: { recordReactiveRun("Key.local.showAlert"); showAlert; }
	$: { recordReactiveRun("Key.local.showOk"); showOk; }
	$: { recordReactiveRun("Key.local.timeouts"); timeouts; }
	$: { recordReactiveRun("Key.local.canvas"); canvas; }
	$: { recordReactiveRun("Key.local.previewComposeCanvas"); previewComposeCanvas; }
	$: { recordReactiveRun("Key.local.lock"); lock; }
	$: { recordReactiveRun("Key.local.accessibleLabel"); accessibleLabel; }

	// Every store read gets its own tracker
	$: { recordReactiveRun("Key.store.settings"); $settings; }
	$: { recordReactiveRun("Key.store.openContextMenu"); $openContextMenu; }
	$: { recordReactiveRun("Key.store.inspectedInstance"); $inspectedInstance; }
	$: (async () => {
		recordReactiveRun("Key.render");
		recordReactiveRun(`Key.render.trigger.${getLastAssignedVar()}`);
		markInFlightEnter("Key.render");
		try {
		const sl = structuredClone(slot);
		if (!sl) {
			const unlock = await lock.lock();
			try {
				const ctx = canvas?.getContext("2d");
				if (ctx) ctx.clearRect(0, 0, canvas.width, canvas.height);
				// Encoder slots: profile switch already calls clear_screen
				// on the backend. Individual null pushes here are redundant
				// and cause flash when they race with the new profile's
				// plugin pushing real content via the fast path.
				if (active && context?.controller !== "Encoder") {
					await invoke("update_image", { context, image: null });
				}
			} finally {
				unlock();
			}
		} else if (context?.controller === "Encoder" && resolvedLayout) {
			// Encoder slot with a feedback layout: composite the layout items and
			// push the rendered 200x100 pixmap to the device. The layout's icon
			// slot falls back to the action's state image when the plugin hasn't
			// provided its own `icon`, matching Elgato's default behaviour where
			// the user-assigned icon takes precedence (guides_dials.md line 440).
			const unlock = await lock.lock();
			try {
				const feedback = { ...(sl.feedback ?? {}) } as Record<string, unknown>;
				// Populate the $X1 "icon" slot from the action's state image
				// when no feedback has been pushed yet. This matches Elgato's
				// default rendering of the user-assigned icon for key-style
				// encoder layouts. Other layouts ($A0 canvas, $B1 indicator,
				// etc.) are plugin-driven -- the plugin is expected to push
				// setFeedback with the keys it wants populated.
				if (feedback.icon == null) {
					const fallback = sl.action.states[sl.current_state]?.image ?? sl.action.icon;
					if (fallback) feedback.icon = fallback;
				}
				if (feedback.title == null && state?.text) feedback.title = state.text;
				const isFullCanvasFastPath =
					typeof feedback["full-canvas"] === "string" &&
					(feedback["full-canvas"] as string).startsWith("data:");
				const shouldPushDevice = !awaitingFirstFeedback || !initialRenderDone;
				if (context?.controller === "Encoder") {
					console.log(`[enc-tel] FRONTEND ctx=${slot?.context} fastPath=${isFullCanvasFastPath} shouldPush=${shouldPushDevice} awaiting=${awaitingFirstFeedback} initDone=${initialRenderDone} hasFullCanvas=${"full-canvas" in feedback}`);
				}
				if (isFullCanvasFastPath) {
					if (!previewComposeCanvas) previewComposeCanvas = trackAssign("previewComposeCanvas", document.createElement("canvas"));
					await renderFeedback(previewComposeCanvas, resolvedLayout, feedback);
					const ctx = canvas?.getContext("2d");
					if (ctx) {
						// NOTE: do NOT set canvas.width/height here — Svelte 4 treats
						// `canvas.width = X` in the script as a mutation and invalidates
						// canvas, triggering the reactive block to re-run, causing a
						// self-amplifying ~2254/sec render loop. Template attributes
						// width=200 height=100 already size the element. clearRect + scaled
						// drawImage achieves the same visual effect without the invalidation.
						ctx.clearRect(0, 0, canvas.width, canvas.height);
						ctx.drawImage(previewComposeCanvas, 0, 0, canvas.width, canvas.height);
					}
				} else {
					await renderFeedback(canvas, resolvedLayout, feedback);
					if (active && shouldPushDevice) {
						if (context?.controller === "Encoder") console.log(`[enc-tel] FRONTEND_PUSH_DEVICE pos=${context.position}`);
						await invoke("update_image", { context, image: canvas.toDataURL("image/png") });
					}
				}
				if (awaitingFirstFeedback && !initialRenderDone) initialRenderDone = trackAssign("initialRenderDone", true);
			} finally {
				unlock();
			}
		} else if (context?.controller !== "Encoder") {
			// Non-encoder slots: render via renderImage (key-style full-square).
			// Encoder slots skip this branch entirely -- they wait for layout
			// resolution before rendering. The backend fast path handles device
			// updates in the meantime, so the encoder LCD isn't blank.
			const unlock = await lock.lock();
			try {
				let fallback = sl.action.states[sl.current_state]?.image ?? sl.action.icon;
				if (state) await renderImage(canvas, context, state, fallback, showOk, showAlert, true, active, pressed, $settings?.rotation);
			} finally {
				unlock();
			}
		}
		} finally {
			markInFlightExit("Key.render");
		}
	})();

	function clearAndRedraw() {
		canvas?.getContext("2d")?.clearRect(0, 0, canvas.width, canvas.height);
		slot = trackAssign("slot.clearAndRedraw", slot);
	}
	$: if ($settings?.rotation != undefined) {
		recordReactiveRun("Key.rotation");
		clearAndRedraw();
	}

	async function triggerVirtualPress() {
		if (!active || !context || !slot) return;
		await invoke("trigger_virtual_press", { context });
	}

	let accessibleLabel: string;
	$: { recordReactiveRun("Key.accessibleLabel"); accessibleLabel = label + (slot ? ": " + slot.action.name + (state?.show && state?.text ? " - " + state.text : "") : ""); }
</script>

{#if encoderStrip}
	<div class="flex-1 relative" style="aspect-ratio: 2 / 1; z-index: {slot && $inspectedInstance == slot.context ? 10 : 0};">
		<canvas
			bind:this={canvas}
			class="absolute inset-0 w-full h-full outline-none outline-offset-2 outline-blue-500 transition-colors duration-150"
			class:border-neutral-700={!dragHighlight || dragHighlight === "incompatible"}
			class:border-green-500={dragHighlight === "empty"}
			class:border-orange-500={dragHighlight === "occupied"}
			class:border-blue-400={dragHighlight === "hovered"}
			class:border-y-3={true}
			class:border-l-3={encoderPosition === 0}
			class:border-r-3={encoderPosition === encoderCount - 1}
			class:border-l-[1.5px]={encoderPosition > 0}
			class:border-r-[1.5px]={encoderPosition < encoderCount - 1}
			class:rounded-l-xl={encoderPosition === 0}
			class:rounded-r-xl={encoderPosition === encoderCount - 1}
			class:outline-solid={active && !dragHighlight && ((slot && $inspectedInstance == slot.context) || (context && $inspectedInstance == context))}
			class:bg-black={slot != null}
			width={200}
			height={100}
			draggable={slot != null}
			{tabindex}
			{role}
			aria-label={accessibleLabel}
			on:dragstart
			on:dragover
			on:dragleave
			on:drop
			on:click|stopPropagation={select}
			on:dblclick|stopPropagation={triggerVirtualPress}
			on:keydown={(e) => {
				if (!active || !context) return;
				if (e.key == "Enter") select(e);
				else if (e.key == "F2") edit();
				else if ((e.ctrlKey || e.metaKey) && e.key == "c") copy();
				else if ((e.ctrlKey || e.metaKey) && e.key == "v") paste();
				else if (e.key == "Delete") clear();
				else if (e.key == "ContextMenu" || (e.shiftKey && e.key == "F10")) contextMenu(e);
			}}
			on:keyup|stopPropagation={(e) => {
				if (!active || !context) return;
				if (e.key == " ") select(e);
			}}
			on:focus={onfocus}
			on:contextmenu={contextMenu}
		/>
	</div>
{:else}
	<div
		class="relative transition-all duration-150"
		class:opacity-30={dragHighlight === "incompatible"}
		class:brightness-125={dragHighlight === "hovered"}
		class:scale-110={dragHighlight === "hovered"}
		style={`transform: scale(${(112 /* desired inner size */ / size) * scale});`}
	>
		<canvas
			bind:this={canvas}
			class="relative border-3 rounded-3xl outline-none transition-colors duration-150"
			class:border-neutral-700={!dragHighlight || dragHighlight === "incompatible"}
			class:border-green-500={dragHighlight === "empty"}
			class:border-orange-500={dragHighlight === "occupied"}
			class:border-blue-400={dragHighlight === "hovered"}
			style={`margin: ${-((size + 3 * 2 /* border */ - 132 /* desired outer size */) / 2)}px;`}
			class:outline-solid={active && !dragHighlight && ((slot && $inspectedInstance == slot.context) || (context && $inspectedInstance == context))}
			class:outline-offset-2={true}
			class:outline-blue-500={!dragHighlight}
			class:rounded-full!={context?.controller == "Encoder"}
			class:bg-black={slot != null}
			width={size}
			height={size}
			draggable={slot != null}
			{tabindex}
			{role}
			aria-label={accessibleLabel}
			on:dragstart
			on:dragover
			on:dragleave
			on:drop
			on:click|stopPropagation={select}
			on:dblclick|stopPropagation={triggerVirtualPress}
			on:keydown={(e) => {
				if (!active || !context) return;
				if (e.key == "Enter") select(e);
				else if (e.key == "F2") edit();
				else if ((e.ctrlKey || e.metaKey) && e.key == "c") copy();
				else if ((e.ctrlKey || e.metaKey) && e.key == "v") paste();
				else if (e.key == "Delete") clear();
				else if (e.key == "ContextMenu" || (e.shiftKey && e.key == "F10")) contextMenu(e);
			}}
			on:keyup|stopPropagation={(e) => {
				if (!active || !context) return;
				if (e.key == " ") select(e);
			}}
			on:focus={onfocus}
			on:contextmenu={contextMenu}
		/>
		{#if isTouchPoint && !slot}
			<div class="absolute left-1/4 top-1/2 w-1/2 border-t-4 border-neutral-700 pointer-events-none"></div>
		{/if}
	</div>
{/if}

{#if $openContextMenu && $openContextMenu?.context == context}
	<div
		bind:this={contextMenuEl}
		class="absolute w-32 font-semibold text-sm text-neutral-300 bg-neutral-700 border border-neutral-600 rounded-lg divide-y divide-neutral-600! z-10"
		style={`left: ${$openContextMenu.x}px; top: ${$openContextMenu.y}px;`}
	>
		{#if !slot}
			<button
				class="flex flex-row items-center w-full p-2 hover:bg-neutral-600 transition-colors rounded-lg cursor-pointer"
				on:click|stopPropagation={paste}
			>
				<Clipboard size="18" class="text-neutral-300" />
				<span class="ml-2"> Paste </span>
			</button>
		{:else}
			<button
				class="flex flex-row items-center w-full p-2 hover:bg-neutral-600 transition-colors rounded-t-lg cursor-pointer"
				on:click|stopPropagation={edit}
			>
				<Pencil size="18" class="text-neutral-300" />
				<span class="ml-2"> Edit </span>
			</button>
			<button
				class="flex flex-row items-center w-full p-2 hover:bg-neutral-600 transition-colors cursor-pointer"
				on:click|stopPropagation={copy}
			>
				<Copy size="18" class="text-neutral-300" />
				<span class="ml-2"> Copy </span>
			</button>
			<button
				class="flex flex-row items-center w-full p-2 hover:bg-neutral-600 transition-colors cursor-pointer"
				on:click|stopPropagation={clear}
			>
				<Trash size="18" class="text-red-400" />
				<span class="ml-2"> Delete </span>
			</button>
			<label
				class="flex flex-row items-center w-full p-2 hover:bg-neutral-600 transition-colors rounded-b-lg cursor-pointer"
				on:click|stopPropagation
			>
				<input
					type="checkbox"
					checked={slot.skip_persistence != null ? slot.skip_persistence : ($settings?.skip_persistence_default || false)}
					on:change|stopPropagation={toggleSkipPersistence}
					class="w-4 h-4 accent-blue-500 cursor-pointer"
				/>
				<span class="ml-2"> Skip disk writes </span>
			</label>
		{/if}
	</div>
{/if}

{#if slot && showEditor}
	<InstanceEditor bind:instance={slot} bind:showEditor />
{/if}
