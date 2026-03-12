/* tslint:disable */
/* eslint-disable */

/**
 * Main entry point for the browser presence layer.
 *
 * Manages server connection, voice model, and presence state.
 * All WebSocket protocols are handled in Rust; JS only handles
 * DOM updates and audio I/O.
 */
export class PresenceWeb {
    free(): void;
    [Symbol.dispose](): void;
    connect_server(url: string): void;
    connect_voice(provider: string, token: string, model?: string | null, input_sample_rate?: number | null): void;
    disconnect_voice(): void;
    dispatch_tool(tool_name: string, args: any): any;
    /**
     * Get presence system prompt (from presence-core).
     */
    get_prompt(): string;
    get_state(): any;
    /**
     * Get presence tools as JS array (from presence-core).
     */
    get_tools(): any;
    /**
     * Handle a server event by injecting system text into the voice model.
     * Returns true if a message was sent to the voice model.
     */
    handle_server_event(evt: any): boolean;
    /**
     * Handle a voice model tool call end-to-end.
     *
     * - Dispatches the tool via presence-core
     * - Sends voice log to server
     * - For `TextResult` and action types: sends voice tool response, dispatches
     *   server action if needed, returns `JsValue::NULL`
     * - For `NeedsIO`: returns `{ needs_io: true, tool_name, args }` so JS can
     *   do the async server roundtrip and call `send_voice_tool_response` itself
     */
    handle_voice_tool_call(call: any): any;
    has_pending_approval(): boolean;
    /**
     * If the agent has a pending approval, inject it into the voice model.
     * Returns true if a message was sent.
     */
    inject_pending_approval_if_any(): boolean;
    constructor();
    phase(): string;
    reconnect_server(url: string): void;
    send_audio(base64_pcm: string): void;
    send_key(key: string, ctrl: boolean, alt: boolean, shift: boolean): void;
    /**
     * Send a presence checkpoint to the server.
     */
    send_presence_checkpoint(summary: string): void;
    send_resize(cols: number, rows: number): void;
    send_server_action(action: any): void;
    send_text(text: string): void;
    /**
     * Send a tool_request to the server, with a JS callback for the response.
     */
    send_tool_request(tool: string, args: any, on_result: Function): void;
    /**
     * Send a voice diagnostic to the server (errors, silence, disconnects).
     */
    send_voice_diagnostic(kind: string, detail: string): void;
    /**
     * Send a voice transcript log entry to the server.
     */
    send_voice_log(text: string, tool_context?: string | null): void;
    send_voice_tool_response(call: any, result: any): void;
    set_on_error(f: Function): void;
    set_on_server_event(f: Function): void;
    set_on_server_state(f: Function): void;
    set_on_state_snapshot(f: Function): void;
    set_on_term(f: Function): void;
    set_on_voice_audio(f: Function): void;
    set_on_voice_interrupted(f: Function): void;
    set_on_voice_ready(f: Function): void;
    set_on_voice_text(f: Function): void;
    set_on_voice_tool_call(f: Function): void;
    set_state(state: any): void;
    update_from_event(event: any): any;
}

/**
 * Browser-side presence state.
 *
 * Wraps `AgentStateSnapshot` and exposes tool dispatch, event formatting,
 * and state queries to JavaScript.
 */
export class WasmPresence {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Dispatch a tool call using local agent state.
     *
     * Returns a `PresenceAction` JS object:
     * - `{ type: "TextResult", data: "..." }` — resolved locally
     * - `{ type: "SubmitTask", data: { task, force_direct, context_hints } }`
     * - `{ type: "Approve", data: { id } }`
     * - `{ type: "Deny", data: { id } }`
     * - `{ type: "Skip", data: { id } }`
     * - `{ type: "Respond", data: { text } }`
     * - `{ type: "SetAutonomy", data: { level } }`
     * - `{ type: "NeedsIO", data: { tool_name, args } }` — needs server round-trip
     */
    dispatch(tool_name: string, args: any): any;
    /**
     * Get the current agent state as a JS object.
     */
    get_state(): any;
    /**
     * Check if there is a pending approval.
     */
    has_pending_approval(): boolean;
    /**
     * Create a new presence instance with default (empty) agent state.
     */
    constructor();
    /**
     * Get the current phase.
     */
    phase(): string;
    /**
     * Replace the entire agent state (e.g. from a bootstrap `state_snapshot`).
     */
    set_state(state: any): void;
    /**
     * Update state from a server-sent event (OutboundEvent JSON).
     *
     * Returns a formatted narration string if the event should be narrated
     * to the live model, or `null` if the event is not narration-worthy.
     */
    update_from_event(event: any): any;
}

/**
 * Return the compiled-in presence system prompt.
 */
export function get_presence_prompt(): string;

/**
 * Return all presence tool definitions as a JS array.
 */
export function get_presence_tools(): any;

/**
 * Unicode-safe string truncation (appends "..." if truncated).
 */
export function wasm_truncate(s: string, max: number): string;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_presenceweb_free: (a: number, b: number) => void;
    readonly presenceweb_connect_server: (a: number, b: number, c: number) => void;
    readonly presenceweb_connect_voice: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number) => void;
    readonly presenceweb_disconnect_voice: (a: number) => void;
    readonly presenceweb_dispatch_tool: (a: number, b: number, c: number, d: any) => any;
    readonly presenceweb_get_prompt: (a: number) => [number, number];
    readonly presenceweb_get_state: (a: number) => any;
    readonly presenceweb_get_tools: (a: number) => any;
    readonly presenceweb_handle_server_event: (a: number, b: any) => number;
    readonly presenceweb_handle_voice_tool_call: (a: number, b: any) => any;
    readonly presenceweb_has_pending_approval: (a: number) => number;
    readonly presenceweb_inject_pending_approval_if_any: (a: number) => number;
    readonly presenceweb_new: () => number;
    readonly presenceweb_phase: (a: number) => [number, number];
    readonly presenceweb_reconnect_server: (a: number, b: number, c: number) => void;
    readonly presenceweb_send_audio: (a: number, b: number, c: number) => void;
    readonly presenceweb_send_key: (a: number, b: number, c: number, d: number, e: number, f: number) => void;
    readonly presenceweb_send_presence_checkpoint: (a: number, b: number, c: number) => void;
    readonly presenceweb_send_resize: (a: number, b: number, c: number) => void;
    readonly presenceweb_send_server_action: (a: number, b: any) => void;
    readonly presenceweb_send_text: (a: number, b: number, c: number) => void;
    readonly presenceweb_send_tool_request: (a: number, b: number, c: number, d: any, e: any) => void;
    readonly presenceweb_send_voice_diagnostic: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly presenceweb_send_voice_log: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly presenceweb_send_voice_tool_response: (a: number, b: any, c: any) => void;
    readonly presenceweb_set_on_error: (a: number, b: any) => void;
    readonly presenceweb_set_on_server_event: (a: number, b: any) => void;
    readonly presenceweb_set_on_server_state: (a: number, b: any) => void;
    readonly presenceweb_set_on_state_snapshot: (a: number, b: any) => void;
    readonly presenceweb_set_on_term: (a: number, b: any) => void;
    readonly presenceweb_set_on_voice_audio: (a: number, b: any) => void;
    readonly presenceweb_set_on_voice_interrupted: (a: number, b: any) => void;
    readonly presenceweb_set_on_voice_ready: (a: number, b: any) => void;
    readonly presenceweb_set_on_voice_text: (a: number, b: any) => void;
    readonly presenceweb_set_on_voice_tool_call: (a: number, b: any) => void;
    readonly presenceweb_set_state: (a: number, b: any) => void;
    readonly presenceweb_update_from_event: (a: number, b: any) => any;
    readonly __wbg_wasmpresence_free: (a: number, b: number) => void;
    readonly get_presence_prompt: () => [number, number];
    readonly get_presence_tools: () => any;
    readonly wasm_truncate: (a: number, b: number, c: number) => [number, number];
    readonly wasmpresence_dispatch: (a: number, b: number, c: number, d: any) => any;
    readonly wasmpresence_get_state: (a: number) => any;
    readonly wasmpresence_has_pending_approval: (a: number) => number;
    readonly wasmpresence_new: () => number;
    readonly wasmpresence_phase: (a: number) => [number, number];
    readonly wasmpresence_set_state: (a: number, b: any) => void;
    readonly wasmpresence_update_from_event: (a: number, b: any) => any;
    readonly wasm_bindgen__closure__destroy__h83c8c2db16b120ac: (a: number, b: number) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h02c82abf5f4209d1: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__ha067de4be952b5b6: (a: number, b: number) => void;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
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
