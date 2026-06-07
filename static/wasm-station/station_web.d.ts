/* tslint:disable */
/* eslint-disable */

export class StationWeb {
    free(): void;
    [Symbol.dispose](): void;
    debug_state(): string;
    focus_on(id: string): void;
    constructor(scene_canvas: HTMLCanvasElement, hud_canvas: HTMLCanvasElement);
    register_display_source(source_id: string, host_id: string, display_id: string, label: string, kind: string, video: HTMLVideoElement): void;
    resize(): void;
    select_by_id(id?: string | null): void;
    set_action_callback(callback: Function): void;
    set_active(active: boolean): void;
    set_layout(layout: string): void;
    set_visuals(mood: string, fov_deg: number, motion: number, ar_strength: number, density: number): void;
    unregister_display_source(source_id: string): void;
    update_snapshot(snapshot: any): void;
}

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_stationweb_free: (a: number, b: number) => void;
    readonly stationweb_debug_state: (a: number) => [number, number];
    readonly stationweb_focus_on: (a: number, b: number, c: number) => void;
    readonly stationweb_new: (a: any, b: any) => [number, number, number];
    readonly stationweb_register_display_source: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number, j: number, k: number, l: any) => void;
    readonly stationweb_resize: (a: number) => void;
    readonly stationweb_select_by_id: (a: number, b: number, c: number) => void;
    readonly stationweb_set_action_callback: (a: number, b: any) => void;
    readonly stationweb_set_active: (a: number, b: number) => void;
    readonly stationweb_set_layout: (a: number, b: number, c: number) => void;
    readonly stationweb_set_visuals: (a: number, b: number, c: number, d: number, e: number, f: number, g: number) => void;
    readonly stationweb_unregister_display_source: (a: number, b: number, c: number) => void;
    readonly stationweb_update_snapshot: (a: number, b: any) => [number, number];
    readonly wasm_bindgen__closure__destroy__h8ff424c0d9b28f90: (a: number, b: number) => void;
    readonly wasm_bindgen__closure__destroy__h4b3df25dd6724f54: (a: number, b: number) => void;
    readonly wasm_bindgen__convert__closures_____invoke__hae2356e5e579fe4c: (a: number, b: number, c: number) => void;
    readonly wasm_bindgen__convert__closures_____invoke__hd862b17e0006a99f: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h5328ece5e87418f0: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h9daa85ed2f27c1ee: (a: number, b: number) => void;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
