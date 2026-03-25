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
    /**
     * Get the active voice model name from the server connection.
     */
    active_voice_model(): string;
    /**
     * Get the active voice provider name (e.g. "gemini", "openai", or "").
     */
    active_voice_provider(): string;
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
     * Grant agent access to the user's session display.
     */
    grant_user_display(): void;
    /**
     * Handle live model usage from Gemini Live / OpenAI Realtime.
     * Updates dashboard state, sends to server, returns `UiCommand[]`.
     */
    handle_live_usage(usage: any): any;
    /**
     * Handle a server event by injecting system text into the voice model.
     * Returns true if a message was sent to the voice model.
     */
    handle_server_event(evt: any): boolean;
    /**
     * Route a raw server message through the dashboard state machine.
     * Returns `UiCommand[]` as a JS array for the rendering layer.
     */
    handle_server_message(msg: any): any;
    /**
     * Handle a voice model tool call end-to-end.
     *
     * ALL tools respond instantly — no server roundtrip blocks the voice model.
     *
     * - `TextResult` (check_status): answered from cached state, immediate response
     * - Action tools (approve, deny, submit_task, etc.): immediate "ok", fire-and-forget to server
     * - `NeedsIO` (query_detail, recall_memory): immediate "querying..." response,
     *   async query to server, result injected as text when it arrives
     */
    handle_voice_tool_call(call: any): any;
    has_pending_approval(): boolean;
    /**
     * If the agent has a pending approval, inject it into the voice model.
     * Returns true if a message was sent.
     */
    inject_pending_approval_if_any(): boolean;
    constructor();
    /**
     * Get pending approval ID (for keyboard shortcut routing).
     */
    pending_approval_id(): any;
    phase(): string;
    reconnect_server(url: string): void;
    /**
     * Release control of a display.
     */
    release_display(display_id: bigint, note?: string | null): void;
    /**
     * Revoke agent access to the user's session display.
     */
    revoke_user_display(): void;
    /**
     * Approve/skip/deny/approve_all a pending action.
     * Returns `UiCommand[]` for UI updates. Sends the action to the server.
     */
    send_approval(action: string): any;
    send_audio(base64_pcm: string): void;
    /**
     * Send a follow-up message.
     */
    send_follow_up(text: string): any;
    /**
     * Send a video frame to the active live provider.
     * `base64_jpeg` is the 768x768 live-resolution frame.
     * `frame_id` is the client-assigned ID (e.g. "cam0-f00047").
     */
    send_frame(base64_jpeg: string, frame_id: string): void;
    /**
     * Send a frame ID context annotation to the live provider as system text.
     * Called alongside send_frame so the model knows the ID of the image it just received.
     */
    send_frame_context(frame_id: string): void;
    /**
     * Send a human response (askHuman).
     */
    send_human_response(text: string): any;
    send_key(key: string, ctrl: boolean, alt: boolean, shift: boolean): void;
    /**
     * Send live model usage to the server for tracking/broadcast.
     */
    send_live_usage(input: bigint, output: bigint, cached: bigint, total: bigint, thinking: bigint): void;
    /**
     * Request to become the active voice owner (triggers handover from current active).
     */
    send_make_active(): boolean;
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
     * Send raw PCM16 audio (base64-encoded) to the server for transcription.
     */
    send_user_audio(base64_pcm: string): void;
    /**
     * Send a video frame to the server for HQ archival.
     * `base64_jpeg` is the original resolution frame.
     * `frame_id` is the client-assigned ID.
     * `stream` is the source stream name (e.g. "cam0").
     */
    send_video_frame_to_server(base64_jpeg: string, frame_id: string, stream: string): void;
    /**
     * Send a voice diagnostic to the server (errors, silence, disconnects).
     */
    send_voice_diagnostic(kind: string, detail: string): void;
    /**
     * Send a voice transcript log entry to the server.
     */
    send_voice_log(text: string, tool_context?: string | null): void;
    send_voice_tool_response(call: any, result: any): void;
    /**
     * Notify which tab is active (for badge logic).
     */
    set_active_tab(tab: string): any;
    set_on_active_granted(f: Function): void;
    set_on_diagnostic(f: Function): void;
    set_on_error(f: Function): void;
    set_on_force_disconnect(f: Function): void;
    set_on_inject_voice_image(f: Function): void;
    set_on_inject_voice_text(f: Function): void;
    set_on_live_usage(f: Function): void;
    set_on_raw_message(f: Function): void;
    set_on_server_event(f: Function): void;
    set_on_server_state(f: Function): void;
    set_on_session_changed(f: Function): void;
    set_on_state_snapshot(f: Function): void;
    set_on_term(f: Function): void;
    set_on_voice_audio(f: Function): void;
    set_on_voice_interrupted(f: Function): void;
    set_on_voice_ready(f: Function): void;
    set_on_voice_text(f: Function): void;
    set_on_voice_tool_call(f: Function): void;
    set_on_voice_transcript(f: Function): void;
    /**
     * Set passive mode — this browser will never request active status.
     * Use for observer/follow-along mode.
     */
    set_passive_mode(passive: boolean): void;
    set_state(state: any): void;
    /**
     * Change log verbosity and return commands to re-filter.
     */
    set_verbosity(level: string): any;
    /**
     * Take control of a display.
     */
    take_display(display_id: bigint): void;
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
    readonly presenceweb_active_voice_model: (a: number) => [number, number];
    readonly presenceweb_active_voice_provider: (a: number) => [number, number];
    readonly presenceweb_connect_server: (a: number, b: number, c: number) => void;
    readonly presenceweb_connect_voice: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number) => void;
    readonly presenceweb_disconnect_voice: (a: number) => void;
    readonly presenceweb_dispatch_tool: (a: number, b: number, c: number, d: any) => any;
    readonly presenceweb_get_prompt: (a: number) => [number, number];
    readonly presenceweb_get_state: (a: number) => any;
    readonly presenceweb_get_tools: (a: number) => any;
    readonly presenceweb_grant_user_display: (a: number) => void;
    readonly presenceweb_handle_live_usage: (a: number, b: any) => any;
    readonly presenceweb_handle_server_event: (a: number, b: any) => number;
    readonly presenceweb_handle_server_message: (a: number, b: any) => any;
    readonly presenceweb_handle_voice_tool_call: (a: number, b: any) => any;
    readonly presenceweb_has_pending_approval: (a: number) => number;
    readonly presenceweb_inject_pending_approval_if_any: (a: number) => number;
    readonly presenceweb_new: () => number;
    readonly presenceweb_pending_approval_id: (a: number) => any;
    readonly presenceweb_phase: (a: number) => [number, number];
    readonly presenceweb_reconnect_server: (a: number, b: number, c: number) => void;
    readonly presenceweb_release_display: (a: number, b: bigint, c: number, d: number) => void;
    readonly presenceweb_revoke_user_display: (a: number) => void;
    readonly presenceweb_send_approval: (a: number, b: number, c: number) => any;
    readonly presenceweb_send_audio: (a: number, b: number, c: number) => void;
    readonly presenceweb_send_follow_up: (a: number, b: number, c: number) => any;
    readonly presenceweb_send_frame: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly presenceweb_send_frame_context: (a: number, b: number, c: number) => void;
    readonly presenceweb_send_human_response: (a: number, b: number, c: number) => any;
    readonly presenceweb_send_key: (a: number, b: number, c: number, d: number, e: number, f: number) => void;
    readonly presenceweb_send_live_usage: (a: number, b: bigint, c: bigint, d: bigint, e: bigint, f: bigint) => void;
    readonly presenceweb_send_make_active: (a: number) => number;
    readonly presenceweb_send_presence_checkpoint: (a: number, b: number, c: number) => void;
    readonly presenceweb_send_resize: (a: number, b: number, c: number) => void;
    readonly presenceweb_send_server_action: (a: number, b: any) => void;
    readonly presenceweb_send_text: (a: number, b: number, c: number) => void;
    readonly presenceweb_send_tool_request: (a: number, b: number, c: number, d: any, e: any) => void;
    readonly presenceweb_send_user_audio: (a: number, b: number, c: number) => void;
    readonly presenceweb_send_video_frame_to_server: (a: number, b: number, c: number, d: number, e: number, f: number, g: number) => void;
    readonly presenceweb_send_voice_diagnostic: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly presenceweb_send_voice_log: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly presenceweb_send_voice_tool_response: (a: number, b: any, c: any) => void;
    readonly presenceweb_set_active_tab: (a: number, b: number, c: number) => any;
    readonly presenceweb_set_on_active_granted: (a: number, b: any) => void;
    readonly presenceweb_set_on_diagnostic: (a: number, b: any) => void;
    readonly presenceweb_set_on_error: (a: number, b: any) => void;
    readonly presenceweb_set_on_force_disconnect: (a: number, b: any) => void;
    readonly presenceweb_set_on_inject_voice_image: (a: number, b: any) => void;
    readonly presenceweb_set_on_inject_voice_text: (a: number, b: any) => void;
    readonly presenceweb_set_on_live_usage: (a: number, b: any) => void;
    readonly presenceweb_set_on_raw_message: (a: number, b: any) => void;
    readonly presenceweb_set_on_server_event: (a: number, b: any) => void;
    readonly presenceweb_set_on_server_state: (a: number, b: any) => void;
    readonly presenceweb_set_on_session_changed: (a: number, b: any) => void;
    readonly presenceweb_set_on_state_snapshot: (a: number, b: any) => void;
    readonly presenceweb_set_on_term: (a: number, b: any) => void;
    readonly presenceweb_set_on_voice_audio: (a: number, b: any) => void;
    readonly presenceweb_set_on_voice_interrupted: (a: number, b: any) => void;
    readonly presenceweb_set_on_voice_ready: (a: number, b: any) => void;
    readonly presenceweb_set_on_voice_text: (a: number, b: any) => void;
    readonly presenceweb_set_on_voice_tool_call: (a: number, b: any) => void;
    readonly presenceweb_set_on_voice_transcript: (a: number, b: any) => void;
    readonly presenceweb_set_passive_mode: (a: number, b: number) => void;
    readonly presenceweb_set_state: (a: number, b: any) => void;
    readonly presenceweb_set_verbosity: (a: number, b: number, c: number) => any;
    readonly presenceweb_take_display: (a: number, b: bigint) => void;
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
    readonly wasm_bindgen__closure__destroy__h28609f58d1fe129e: (a: number, b: number) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h0038094974226a74: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__hd1cdbc32e70fbdd2: (a: number, b: number) => void;
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
