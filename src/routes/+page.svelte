<script lang="ts">
	import type { DeviceInfo } from "$lib/DeviceInfo";
	import type { Profile } from "$lib/Profile";

	import { initPortBase } from "$lib/ports";
	import { inspectedInstance, inspectedParentAction } from "$lib/propertyInspector";
	import { actionList, deviceSelector, profileManager } from "$lib/singletons";
	import { uiScale } from "$lib/uiScale";

	import ActionList from "../components/ActionList.svelte";
	import DeviceSelector from "../components/DeviceSelector.svelte";
	import DeviceView from "../components/DeviceView.svelte";
	import DiskIOIndicator from "../components/DiskIOIndicator.svelte";
	import NoDevicesDetected from "../components/NoDevicesDetected.svelte";
	import ParentActionView from "../components/ParentActionView.svelte";
	import PluginManager from "../components/PluginManager.svelte";
	import ProfileManager from "../components/ProfileManager.svelte";
	import PropertyInspectorView from "../components/PropertyInspectorView.svelte";
	import SettingsView from "../components/SettingsView.svelte";

	let devices: { [id: string]: DeviceInfo } = {};
	let selectedDevice: string;
	let selectedProfiles: { [id: string]: Profile } = {};

	initPortBase();
</script>

<svelte:window on:dragover={(event) => event.preventDefault()} on:drop={(event) => event.preventDefault()} />

<div class="flex flex-row h-screen">
	<div class="flex flex-col grow min-w-0 relative">
		<nav class="flex flex-row justify-between items-center p-3" class:hidden={$inspectedParentAction}>
			<div class="flex flex-col items-start space-y-1">
				<DeviceSelector
					bind:devices
					bind:value={selectedDevice}
					bind:selectedProfiles
					bind:this={$deviceSelector}
				/>
				{#key selectedDevice}
					{#if selectedDevice && devices[selectedDevice]}
						<ProfileManager
							bind:device={devices[selectedDevice]}
							bind:profile={selectedProfiles[selectedDevice]}
							bind:this={$profileManager}
						/>
					{/if}
				{/key}
			</div>

			<div class="flex flex-row items-center space-x-2" class:mr-4={Object.keys(devices).length > 0}>
				<DiskIOIndicator />
				<div class="flex items-center gap-1 text-xs text-neutral-400">
					<span class="select-none">Scale</span>
					<input
						type="range"
						min="0.5"
						max="1.5"
						step="0.05"
						bind:value={$uiScale}
						class="w-20 h-1 accent-neutral-400 cursor-pointer"
					/>
					<span class="w-8 text-right">{$uiScale.toFixed(2)}x</span>
				</div>
				<PluginManager />
				<SettingsView />
			</div>
		</nav>

		{#if Object.keys(devices).length > 0 && selectedProfiles}
			{#if $inspectedParentAction}
				<ParentActionView bind:profile={selectedProfiles[selectedDevice]} />
			{/if}

			{#each Object.entries(devices) as [id, device]}
				{#if device && selectedProfiles[id]}
					<DeviceView bind:device bind:profile={selectedProfiles[id]} bind:selectedDevice />
				{/if}
			{/each}

			{#if selectedProfiles[selectedDevice] && $inspectedInstance}
				<div class="absolute bottom-0 left-0 right-0 z-30">
					<PropertyInspectorView bind:device={devices[selectedDevice]} bind:profile={selectedProfiles[selectedDevice]} />
				</div>
			{/if}
		{:else}
			<NoDevicesDetected />
		{/if}
	</div>

	<ActionList bind:this={$actionList} />
</div>
