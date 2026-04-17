import { writable } from "svelte/store";

export type DragInfo = {
	controllers: string[];
} | null;

export const dragAction = writable<DragInfo>(null);
export const hoveredSlot = writable<{ controller: string; position: number } | null>(null);
export const openDialPosition = writable<number | null>(null);
