export type Settings = {
	version: string;
	language: string;
	brightness: number;
	sleep_timeout_minutes: number;
	rotation: number;
	background: boolean;
	autolaunch: boolean;
	updatecheck: boolean;
	statistics: boolean;
	separatewine: boolean;
	developer: boolean;
	disableelgato: boolean;
	skip_persistence_default: boolean;
	debug_log_until_ts: number | null;
	debug_log_permanent: boolean;
};

import { invoke } from "@tauri-apps/api/core";
import { type Writable, writable } from "svelte/store";

export const settings: Writable<Settings | null> = writable(null);
(async () => settings.set(await invoke("get_settings")))();
export const localisations: Writable<{ [plugin: string]: any } | null> = writable(null);
settings.subscribe(async (value) => {
	if (value) {
		await invoke("set_settings", { settings: value });
		localisations.set(await invoke("get_localisations", { locale: value.language }));
	}
});
