/* tslint:disable */
/* eslint-disable */

export class StationWeb {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Programmatically trigger the action a click on the named
     * hotspot/hit-zone would dispatch (same path as a pointer click,
     * including the JS action callback). Returns false for unknown
     * names. Names are the ones `debug_json`/`hotspot_rects` report.
     */
    activate(name: string): boolean;
    /**
     * Close the transcript viewer (dashboard-side counterpart of the
     * panel's close pill / Escape).
     */
    close_transcript(): void;
    /**
     * Composer overlay geometry + state for the dashboard's DOM input:
     * `{open, mode, rect: {x,y,w,h} | null}`. The rect is the input
     * slot inside the drawn composer strip (CSS px), present only after
     * the strip painted.
     */
    composer_state(): string;
    /**
     * Structured introspection for agents driving the canvas UI: render
     * health, snapshot counters, view state, and every named clickable
     * rect in CSS px. `hitZones` lists all named zones in draw order
     * (`{name, action, x, y, w, h}`); `systemTargets` is the deduped
     * system/layout hotspot set (same shape minus `action`) that
     * `hotspot_rects` returns. The flat `debug_state` token format is
     * frozen for the validator probe; new fields land here instead.
     */
    debug_json(): string;
    debug_state(): string;
    focus_on(id: string): void;
    /**
     * JSON array of the system/layout hotspot targets currently drawn,
     * `[{name, x, y, w, h}]` in CSS px, exported from the real draw
     * geometry (the dashboard positions its accessibility overlay from
     * this instead of hand-mirroring panel math). Rects reflect the last
     * painted HUD; empty until the first paint.
     */
    hotspot_rects(): string;
    constructor(scene_canvas: HTMLCanvasElement, hud_canvas: HTMLCanvasElement);
    register_display_source(source_id: string, host_id: string, _display_id: string, label: string, _kind: string, video: HTMLVideoElement): void;
    resize(): void;
    /**
     * Select a node (or clear the selection with `null`); the scene halo
     * and HUD focus panel follow `selected_id` on the next paint.
     */
    select_by_id(id?: string | null): void;
    set_action_callback(callback: Function): void;
    set_active(active: boolean): void;
    /**
     * Open/close the composer strip. `mode` is `send` or `launch`.
     * The dashboard calls this from its short-circuit replacements
     * (e.g. the legacy new-session route) and when its input overlay
     * loses relevance (Escape inside the input).
     */
    set_composer(open: boolean, mode: string): void;
    set_layout(layout: string): void;
    /**
     * Feed (or refresh) the transcript/diff viewer. Payload shape is
     * `model::StationTranscript`. A `refresh: true` payload is only
     * applied while the viewer is still open on the same session —
     * returns false otherwise so the dashboard stops live-refreshing.
     * A non-refresh payload always opens the viewer.
     */
    set_transcript(payload: any): boolean;
    set_visuals(mood: string, fov_deg: number, motion: number, ar_strength: number, density: number): void;
    unregister_display_source(source_id: string): void;
    update_snapshot(snapshot: any): void;
}

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_stationweb_free: (a: number, b: number) => void;
    readonly stationweb_activate: (a: number, b: number, c: number) => number;
    readonly stationweb_close_transcript: (a: number) => void;
    readonly stationweb_composer_state: (a: number) => [number, number];
    readonly stationweb_debug_json: (a: number) => [number, number];
    readonly stationweb_debug_state: (a: number) => [number, number];
    readonly stationweb_focus_on: (a: number, b: number, c: number) => void;
    readonly stationweb_hotspot_rects: (a: number) => [number, number];
    readonly stationweb_new: (a: any, b: any) => [number, number, number];
    readonly stationweb_register_display_source: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number, j: number, k: number, l: any) => void;
    readonly stationweb_resize: (a: number) => void;
    readonly stationweb_select_by_id: (a: number, b: number, c: number) => void;
    readonly stationweb_set_action_callback: (a: number, b: any) => void;
    readonly stationweb_set_active: (a: number, b: number) => void;
    readonly stationweb_set_composer: (a: number, b: number, c: number, d: number) => void;
    readonly stationweb_set_layout: (a: number, b: number, c: number) => void;
    readonly stationweb_set_transcript: (a: number, b: any) => [number, number, number];
    readonly stationweb_set_visuals: (a: number, b: number, c: number, d: number, e: number, f: number, g: number) => void;
    readonly stationweb_unregister_display_source: (a: number, b: number, c: number) => void;
    readonly stationweb_update_snapshot: (a: number, b: any) => [number, number];
    readonly wasm_bindgen__closure__destroy__hccf83c1ad0c1d3f3: (a: number, b: number) => void;
    readonly wasm_bindgen__closure__destroy__h544cef30fa11676b: (a: number, b: number) => void;
    readonly wasm_bindgen__convert__closures_____invoke__he59c630b1b02e0f5: (a: number, b: number, c: number) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h63b5eb8c1813e0ac: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__hfa5895c89262f4d9: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__he6ccb7f2bccd870d: (a: number, b: number) => void;
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
