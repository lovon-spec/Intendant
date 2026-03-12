/* @ts-self-types="./presence_web.d.ts" */

/**
 * Main entry point for the browser presence layer.
 *
 * Manages server connection, voice model, and presence state.
 * All WebSocket protocols are handled in Rust; JS only handles
 * DOM updates and audio I/O.
 */
export class PresenceWeb {
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        PresenceWebFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_presenceweb_free(ptr, 0);
    }
    /**
     * @param {string} url
     */
    connect_server(url) {
        const ptr0 = passStringToWasm0(url, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        wasm.presenceweb_connect_server(this.__wbg_ptr, ptr0, len0);
    }
    /**
     * @param {string} provider
     * @param {string} token
     * @param {string | null} [model]
     * @param {number | null} [input_sample_rate]
     */
    connect_voice(provider, token, model, input_sample_rate) {
        const ptr0 = passStringToWasm0(provider, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(token, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        var ptr2 = isLikeNone(model) ? 0 : passStringToWasm0(model, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        var len2 = WASM_VECTOR_LEN;
        wasm.presenceweb_connect_voice(this.__wbg_ptr, ptr0, len0, ptr1, len1, ptr2, len2, isLikeNone(input_sample_rate) ? 0x100000001 : (input_sample_rate) >>> 0);
    }
    disconnect_voice() {
        wasm.presenceweb_disconnect_voice(this.__wbg_ptr);
    }
    /**
     * @param {string} tool_name
     * @param {any} args
     * @returns {any}
     */
    dispatch_tool(tool_name, args) {
        const ptr0 = passStringToWasm0(tool_name, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.presenceweb_dispatch_tool(this.__wbg_ptr, ptr0, len0, args);
        return ret;
    }
    /**
     * Get presence system prompt (from presence-core).
     * @returns {string}
     */
    get_prompt() {
        let deferred1_0;
        let deferred1_1;
        try {
            const ret = wasm.presenceweb_get_prompt(this.__wbg_ptr);
            deferred1_0 = ret[0];
            deferred1_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
        }
    }
    /**
     * @returns {any}
     */
    get_state() {
        const ret = wasm.presenceweb_get_state(this.__wbg_ptr);
        return ret;
    }
    /**
     * Get presence tools as JS array (from presence-core).
     * @returns {any}
     */
    get_tools() {
        const ret = wasm.presenceweb_get_tools(this.__wbg_ptr);
        return ret;
    }
    /**
     * Handle a server event by injecting system text into the voice model.
     * Returns true if a message was sent to the voice model.
     * @param {any} evt
     * @returns {boolean}
     */
    handle_server_event(evt) {
        const ret = wasm.presenceweb_handle_server_event(this.__wbg_ptr, evt);
        return ret !== 0;
    }
    /**
     * Handle a voice model tool call end-to-end.
     *
     * - Dispatches the tool via presence-core
     * - Sends voice log to server
     * - For `TextResult` and action types: sends voice tool response, dispatches
     *   server action if needed, returns `JsValue::NULL`
     * - For `NeedsIO`: returns `{ needs_io: true, tool_name, args }` so JS can
     *   do the async server roundtrip and call `send_voice_tool_response` itself
     * @param {any} call
     * @returns {any}
     */
    handle_voice_tool_call(call) {
        const ret = wasm.presenceweb_handle_voice_tool_call(this.__wbg_ptr, call);
        return ret;
    }
    /**
     * @returns {boolean}
     */
    has_pending_approval() {
        const ret = wasm.presenceweb_has_pending_approval(this.__wbg_ptr);
        return ret !== 0;
    }
    /**
     * If the agent has a pending approval, inject it into the voice model.
     * Returns true if a message was sent.
     * @returns {boolean}
     */
    inject_pending_approval_if_any() {
        const ret = wasm.presenceweb_inject_pending_approval_if_any(this.__wbg_ptr);
        return ret !== 0;
    }
    constructor() {
        const ret = wasm.presenceweb_new();
        this.__wbg_ptr = ret >>> 0;
        PresenceWebFinalization.register(this, this.__wbg_ptr, this);
        return this;
    }
    /**
     * @returns {string}
     */
    phase() {
        let deferred1_0;
        let deferred1_1;
        try {
            const ret = wasm.presenceweb_phase(this.__wbg_ptr);
            deferred1_0 = ret[0];
            deferred1_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
        }
    }
    /**
     * @param {string} url
     */
    reconnect_server(url) {
        const ptr0 = passStringToWasm0(url, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        wasm.presenceweb_reconnect_server(this.__wbg_ptr, ptr0, len0);
    }
    /**
     * @param {string} base64_pcm
     */
    send_audio(base64_pcm) {
        const ptr0 = passStringToWasm0(base64_pcm, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        wasm.presenceweb_send_audio(this.__wbg_ptr, ptr0, len0);
    }
    /**
     * @param {string} key
     * @param {boolean} ctrl
     * @param {boolean} alt
     * @param {boolean} shift
     */
    send_key(key, ctrl, alt, shift) {
        const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        wasm.presenceweb_send_key(this.__wbg_ptr, ptr0, len0, ctrl, alt, shift);
    }
    /**
     * Send a presence checkpoint to the server.
     * @param {string} summary
     */
    send_presence_checkpoint(summary) {
        const ptr0 = passStringToWasm0(summary, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        wasm.presenceweb_send_presence_checkpoint(this.__wbg_ptr, ptr0, len0);
    }
    /**
     * @param {number} cols
     * @param {number} rows
     */
    send_resize(cols, rows) {
        wasm.presenceweb_send_resize(this.__wbg_ptr, cols, rows);
    }
    /**
     * @param {any} action
     */
    send_server_action(action) {
        wasm.presenceweb_send_server_action(this.__wbg_ptr, action);
    }
    /**
     * @param {string} text
     */
    send_text(text) {
        const ptr0 = passStringToWasm0(text, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        wasm.presenceweb_send_text(this.__wbg_ptr, ptr0, len0);
    }
    /**
     * Send a tool_request to the server, with a JS callback for the response.
     * @param {string} tool
     * @param {any} args
     * @param {Function} on_result
     */
    send_tool_request(tool, args, on_result) {
        const ptr0 = passStringToWasm0(tool, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        wasm.presenceweb_send_tool_request(this.__wbg_ptr, ptr0, len0, args, on_result);
    }
    /**
     * Send a voice diagnostic to the server (errors, silence, disconnects).
     * @param {string} kind
     * @param {string} detail
     */
    send_voice_diagnostic(kind, detail) {
        const ptr0 = passStringToWasm0(kind, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(detail, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        wasm.presenceweb_send_voice_diagnostic(this.__wbg_ptr, ptr0, len0, ptr1, len1);
    }
    /**
     * Send a voice transcript log entry to the server.
     * @param {string} text
     * @param {string | null} [tool_context]
     */
    send_voice_log(text, tool_context) {
        const ptr0 = passStringToWasm0(text, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        var ptr1 = isLikeNone(tool_context) ? 0 : passStringToWasm0(tool_context, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        var len1 = WASM_VECTOR_LEN;
        wasm.presenceweb_send_voice_log(this.__wbg_ptr, ptr0, len0, ptr1, len1);
    }
    /**
     * @param {any} call
     * @param {any} result
     */
    send_voice_tool_response(call, result) {
        wasm.presenceweb_send_voice_tool_response(this.__wbg_ptr, call, result);
    }
    /**
     * @param {Function} f
     */
    set_on_error(f) {
        wasm.presenceweb_set_on_error(this.__wbg_ptr, f);
    }
    /**
     * @param {Function} f
     */
    set_on_server_event(f) {
        wasm.presenceweb_set_on_server_event(this.__wbg_ptr, f);
    }
    /**
     * @param {Function} f
     */
    set_on_server_state(f) {
        wasm.presenceweb_set_on_server_state(this.__wbg_ptr, f);
    }
    /**
     * @param {Function} f
     */
    set_on_state_snapshot(f) {
        wasm.presenceweb_set_on_state_snapshot(this.__wbg_ptr, f);
    }
    /**
     * @param {Function} f
     */
    set_on_term(f) {
        wasm.presenceweb_set_on_term(this.__wbg_ptr, f);
    }
    /**
     * @param {Function} f
     */
    set_on_voice_audio(f) {
        wasm.presenceweb_set_on_voice_audio(this.__wbg_ptr, f);
    }
    /**
     * @param {Function} f
     */
    set_on_voice_interrupted(f) {
        wasm.presenceweb_set_on_voice_interrupted(this.__wbg_ptr, f);
    }
    /**
     * @param {Function} f
     */
    set_on_voice_ready(f) {
        wasm.presenceweb_set_on_voice_ready(this.__wbg_ptr, f);
    }
    /**
     * @param {Function} f
     */
    set_on_voice_text(f) {
        wasm.presenceweb_set_on_voice_text(this.__wbg_ptr, f);
    }
    /**
     * @param {Function} f
     */
    set_on_voice_tool_call(f) {
        wasm.presenceweb_set_on_voice_tool_call(this.__wbg_ptr, f);
    }
    /**
     * @param {any} state
     */
    set_state(state) {
        wasm.presenceweb_set_state(this.__wbg_ptr, state);
    }
    /**
     * @param {any} event
     * @returns {any}
     */
    update_from_event(event) {
        const ret = wasm.presenceweb_update_from_event(this.__wbg_ptr, event);
        return ret;
    }
}
if (Symbol.dispose) PresenceWeb.prototype[Symbol.dispose] = PresenceWeb.prototype.free;

/**
 * Browser-side presence state.
 *
 * Wraps `AgentStateSnapshot` and exposes tool dispatch, event formatting,
 * and state queries to JavaScript.
 */
export class WasmPresence {
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        WasmPresenceFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_wasmpresence_free(ptr, 0);
    }
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
     * @param {string} tool_name
     * @param {any} args
     * @returns {any}
     */
    dispatch(tool_name, args) {
        const ptr0 = passStringToWasm0(tool_name, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.wasmpresence_dispatch(this.__wbg_ptr, ptr0, len0, args);
        return ret;
    }
    /**
     * Get the current agent state as a JS object.
     * @returns {any}
     */
    get_state() {
        const ret = wasm.wasmpresence_get_state(this.__wbg_ptr);
        return ret;
    }
    /**
     * Check if there is a pending approval.
     * @returns {boolean}
     */
    has_pending_approval() {
        const ret = wasm.wasmpresence_has_pending_approval(this.__wbg_ptr);
        return ret !== 0;
    }
    /**
     * Create a new presence instance with default (empty) agent state.
     */
    constructor() {
        const ret = wasm.wasmpresence_new();
        this.__wbg_ptr = ret >>> 0;
        WasmPresenceFinalization.register(this, this.__wbg_ptr, this);
        return this;
    }
    /**
     * Get the current phase.
     * @returns {string}
     */
    phase() {
        let deferred1_0;
        let deferred1_1;
        try {
            const ret = wasm.wasmpresence_phase(this.__wbg_ptr);
            deferred1_0 = ret[0];
            deferred1_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
        }
    }
    /**
     * Replace the entire agent state (e.g. from a bootstrap `state_snapshot`).
     * @param {any} state
     */
    set_state(state) {
        wasm.wasmpresence_set_state(this.__wbg_ptr, state);
    }
    /**
     * Update state from a server-sent event (OutboundEvent JSON).
     *
     * Returns a formatted narration string if the event should be narrated
     * to the live model, or `null` if the event is not narration-worthy.
     * @param {any} event
     * @returns {any}
     */
    update_from_event(event) {
        const ret = wasm.wasmpresence_update_from_event(this.__wbg_ptr, event);
        return ret;
    }
}
if (Symbol.dispose) WasmPresence.prototype[Symbol.dispose] = WasmPresence.prototype.free;

/**
 * Return the compiled-in presence system prompt.
 * @returns {string}
 */
export function get_presence_prompt() {
    let deferred1_0;
    let deferred1_1;
    try {
        const ret = wasm.get_presence_prompt();
        deferred1_0 = ret[0];
        deferred1_1 = ret[1];
        return getStringFromWasm0(ret[0], ret[1]);
    } finally {
        wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
    }
}

/**
 * Return all presence tool definitions as a JS array.
 * @returns {any}
 */
export function get_presence_tools() {
    const ret = wasm.get_presence_tools();
    return ret;
}

/**
 * Unicode-safe string truncation (appends "..." if truncated).
 * @param {string} s
 * @param {number} max
 * @returns {string}
 */
export function wasm_truncate(s, max) {
    let deferred2_0;
    let deferred2_1;
    try {
        const ptr0 = passStringToWasm0(s, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.wasm_truncate(ptr0, len0, max);
        deferred2_0 = ret[0];
        deferred2_1 = ret[1];
        return getStringFromWasm0(ret[0], ret[1]);
    } finally {
        wasm.__wbindgen_free(deferred2_0, deferred2_1, 1);
    }
}

function __wbg_get_imports() {
    const import0 = {
        __proto__: null,
        __wbg_Error_8c4e43fe74559d73: function(arg0, arg1) {
            const ret = Error(getStringFromWasm0(arg0, arg1));
            return ret;
        },
        __wbg_Number_04624de7d0e8332d: function(arg0) {
            const ret = Number(arg0);
            return ret;
        },
        __wbg___wbindgen_bigint_get_as_i64_8fcf4ce7f1ca72a2: function(arg0, arg1) {
            const v = arg1;
            const ret = typeof(v) === 'bigint' ? v : undefined;
            getDataViewMemory0().setBigInt64(arg0 + 8 * 1, isLikeNone(ret) ? BigInt(0) : ret, true);
            getDataViewMemory0().setInt32(arg0 + 4 * 0, !isLikeNone(ret), true);
        },
        __wbg___wbindgen_boolean_get_bbbb1c18aa2f5e25: function(arg0) {
            const v = arg0;
            const ret = typeof(v) === 'boolean' ? v : undefined;
            return isLikeNone(ret) ? 0xFFFFFF : ret ? 1 : 0;
        },
        __wbg___wbindgen_debug_string_0bc8482c6e3508ae: function(arg0, arg1) {
            const ret = debugString(arg1);
            const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            const len1 = WASM_VECTOR_LEN;
            getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
            getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
        },
        __wbg___wbindgen_in_47fa6863be6f2f25: function(arg0, arg1) {
            const ret = arg0 in arg1;
            return ret;
        },
        __wbg___wbindgen_is_bigint_31b12575b56f32fc: function(arg0) {
            const ret = typeof(arg0) === 'bigint';
            return ret;
        },
        __wbg___wbindgen_is_function_0095a73b8b156f76: function(arg0) {
            const ret = typeof(arg0) === 'function';
            return ret;
        },
        __wbg___wbindgen_is_null_ac34f5003991759a: function(arg0) {
            const ret = arg0 === null;
            return ret;
        },
        __wbg___wbindgen_is_object_5ae8e5880f2c1fbd: function(arg0) {
            const val = arg0;
            const ret = typeof(val) === 'object' && val !== null;
            return ret;
        },
        __wbg___wbindgen_is_string_cd444516edc5b180: function(arg0) {
            const ret = typeof(arg0) === 'string';
            return ret;
        },
        __wbg___wbindgen_is_undefined_9e4d92534c42d778: function(arg0) {
            const ret = arg0 === undefined;
            return ret;
        },
        __wbg___wbindgen_jsval_eq_11888390b0186270: function(arg0, arg1) {
            const ret = arg0 === arg1;
            return ret;
        },
        __wbg___wbindgen_jsval_loose_eq_9dd77d8cd6671811: function(arg0, arg1) {
            const ret = arg0 == arg1;
            return ret;
        },
        __wbg___wbindgen_number_get_8ff4255516ccad3e: function(arg0, arg1) {
            const obj = arg1;
            const ret = typeof(obj) === 'number' ? obj : undefined;
            getDataViewMemory0().setFloat64(arg0 + 8 * 1, isLikeNone(ret) ? 0 : ret, true);
            getDataViewMemory0().setInt32(arg0 + 4 * 0, !isLikeNone(ret), true);
        },
        __wbg___wbindgen_string_get_72fb696202c56729: function(arg0, arg1) {
            const obj = arg1;
            const ret = typeof(obj) === 'string' ? obj : undefined;
            var ptr1 = isLikeNone(ret) ? 0 : passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            var len1 = WASM_VECTOR_LEN;
            getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
            getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
        },
        __wbg___wbindgen_throw_be289d5034ed271b: function(arg0, arg1) {
            throw new Error(getStringFromWasm0(arg0, arg1));
        },
        __wbg__wbg_cb_unref_d9b87ff7982e3b21: function(arg0) {
            arg0._wbg_cb_unref();
        },
        __wbg_call_389efe28435a9388: function() { return handleError(function (arg0, arg1) {
            const ret = arg0.call(arg1);
            return ret;
        }, arguments); },
        __wbg_call_4708e0c13bdc8e95: function() { return handleError(function (arg0, arg1, arg2) {
            const ret = arg0.call(arg1, arg2);
            return ret;
        }, arguments); },
        __wbg_close_1d08eaf57ed325c0: function() { return handleError(function (arg0) {
            arg0.close();
        }, arguments); },
        __wbg_code_a552f1e91eda69b7: function(arg0) {
            const ret = arg0.code;
            return ret;
        },
        __wbg_data_5330da50312d0bc1: function(arg0) {
            const ret = arg0.data;
            return ret;
        },
        __wbg_done_57b39ecd9addfe81: function(arg0) {
            const ret = arg0.done;
            return ret;
        },
        __wbg_entries_58c7934c745daac7: function(arg0) {
            const ret = Object.entries(arg0);
            return ret;
        },
        __wbg_get_9b94d73e6221f75c: function(arg0, arg1) {
            const ret = arg0[arg1 >>> 0];
            return ret;
        },
        __wbg_get_b3ed3ad4be2bc8ac: function() { return handleError(function (arg0, arg1) {
            const ret = Reflect.get(arg0, arg1);
            return ret;
        }, arguments); },
        __wbg_get_with_ref_key_1dc361bd10053bfe: function(arg0, arg1) {
            const ret = arg0[arg1];
            return ret;
        },
        __wbg_instanceof_ArrayBuffer_c367199e2fa2aa04: function(arg0) {
            let result;
            try {
                result = arg0 instanceof ArrayBuffer;
            } catch (_) {
                result = false;
            }
            const ret = result;
            return ret;
        },
        __wbg_instanceof_Map_53af74335dec57f4: function(arg0) {
            let result;
            try {
                result = arg0 instanceof Map;
            } catch (_) {
                result = false;
            }
            const ret = result;
            return ret;
        },
        __wbg_instanceof_Uint8Array_9b9075935c74707c: function(arg0) {
            let result;
            try {
                result = arg0 instanceof Uint8Array;
            } catch (_) {
                result = false;
            }
            const ret = result;
            return ret;
        },
        __wbg_instanceof_Window_ed49b2db8df90359: function(arg0) {
            let result;
            try {
                result = arg0 instanceof Window;
            } catch (_) {
                result = false;
            }
            const ret = result;
            return ret;
        },
        __wbg_isArray_d314bb98fcf08331: function(arg0) {
            const ret = Array.isArray(arg0);
            return ret;
        },
        __wbg_isSafeInteger_bfbc7332a9768d2a: function(arg0) {
            const ret = Number.isSafeInteger(arg0);
            return ret;
        },
        __wbg_iterator_6ff6560ca1568e55: function() {
            const ret = Symbol.iterator;
            return ret;
        },
        __wbg_length_32ed9a279acd054c: function(arg0) {
            const ret = arg0.length;
            return ret;
        },
        __wbg_length_35a7bace40f36eac: function(arg0) {
            const ret = arg0.length;
            return ret;
        },
        __wbg_new_057993d5b5e07835: function() { return handleError(function (arg0, arg1) {
            const ret = new WebSocket(getStringFromWasm0(arg0, arg1));
            return ret;
        }, arguments); },
        __wbg_new_361308b2356cecd0: function() {
            const ret = new Object();
            return ret;
        },
        __wbg_new_3eb36ae241fe6f44: function() {
            const ret = new Array();
            return ret;
        },
        __wbg_new_dca287b076112a51: function() {
            const ret = new Map();
            return ret;
        },
        __wbg_new_dd2b680c8bf6ae29: function(arg0) {
            const ret = new Uint8Array(arg0);
            return ret;
        },
        __wbg_new_no_args_1c7c842f08d00ebb: function(arg0, arg1) {
            const ret = new Function(getStringFromWasm0(arg0, arg1));
            return ret;
        },
        __wbg_new_with_str_sequence_b67b3919b8b11238: function() { return handleError(function (arg0, arg1, arg2) {
            const ret = new WebSocket(getStringFromWasm0(arg0, arg1), arg2);
            return ret;
        }, arguments); },
        __wbg_next_3482f54c49e8af19: function() { return handleError(function (arg0) {
            const ret = arg0.next();
            return ret;
        }, arguments); },
        __wbg_next_418f80d8f5303233: function(arg0) {
            const ret = arg0.next;
            return ret;
        },
        __wbg_prototypesetcall_bdcdcc5842e4d77d: function(arg0, arg1, arg2) {
            Uint8Array.prototype.set.call(getArrayU8FromWasm0(arg0, arg1), arg2);
        },
        __wbg_push_8ffdcb2063340ba5: function(arg0, arg1) {
            const ret = arg0.push(arg1);
            return ret;
        },
        __wbg_send_bc0336a1b5ce4fb7: function() { return handleError(function (arg0, arg1, arg2) {
            arg0.send(getStringFromWasm0(arg1, arg2));
        }, arguments); },
        __wbg_setTimeout_eff32631ea138533: function() { return handleError(function (arg0, arg1, arg2) {
            const ret = arg0.setTimeout(arg1, arg2);
            return ret;
        }, arguments); },
        __wbg_set_1eb0999cf5d27fc8: function(arg0, arg1, arg2) {
            const ret = arg0.set(arg1, arg2);
            return ret;
        },
        __wbg_set_3f1d0b984ed272ed: function(arg0, arg1, arg2) {
            arg0[arg1] = arg2;
        },
        __wbg_set_binaryType_5bbf62e9f705dc1a: function(arg0, arg1) {
            arg0.binaryType = __wbindgen_enum_BinaryType[arg1];
        },
        __wbg_set_f43e577aea94465b: function(arg0, arg1, arg2) {
            arg0[arg1 >>> 0] = arg2;
        },
        __wbg_set_onclose_d382f3e2c2b850eb: function(arg0, arg1) {
            arg0.onclose = arg1;
        },
        __wbg_set_onerror_377f18bf4569bf85: function(arg0, arg1) {
            arg0.onerror = arg1;
        },
        __wbg_set_onmessage_2114aa5f4f53051e: function(arg0, arg1) {
            arg0.onmessage = arg1;
        },
        __wbg_set_onopen_b7b52d519d6c0f11: function(arg0, arg1) {
            arg0.onopen = arg1;
        },
        __wbg_static_accessor_GLOBAL_12837167ad935116: function() {
            const ret = typeof global === 'undefined' ? null : global;
            return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
        },
        __wbg_static_accessor_GLOBAL_THIS_e628e89ab3b1c95f: function() {
            const ret = typeof globalThis === 'undefined' ? null : globalThis;
            return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
        },
        __wbg_static_accessor_SELF_a621d3dfbb60d0ce: function() {
            const ret = typeof self === 'undefined' ? null : self;
            return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
        },
        __wbg_static_accessor_WINDOW_f8727f0cf888e0bd: function() {
            const ret = typeof window === 'undefined' ? null : window;
            return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
        },
        __wbg_value_0546255b415e96c1: function(arg0) {
            const ret = arg0.value;
            return ret;
        },
        __wbindgen_cast_0000000000000001: function(arg0, arg1) {
            // Cast intrinsic for `Closure(Closure { dtor_idx: 93, function: Function { arguments: [NamedExternref("CloseEvent")], shim_idx: 96, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
            const ret = makeMutClosure(arg0, arg1, wasm.wasm_bindgen__closure__destroy__h83c8c2db16b120ac, wasm_bindgen__convert__closures_____invoke__h02c82abf5f4209d1);
            return ret;
        },
        __wbindgen_cast_0000000000000002: function(arg0, arg1) {
            // Cast intrinsic for `Closure(Closure { dtor_idx: 93, function: Function { arguments: [NamedExternref("MessageEvent")], shim_idx: 96, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
            const ret = makeMutClosure(arg0, arg1, wasm.wasm_bindgen__closure__destroy__h83c8c2db16b120ac, wasm_bindgen__convert__closures_____invoke__h02c82abf5f4209d1);
            return ret;
        },
        __wbindgen_cast_0000000000000003: function(arg0, arg1) {
            // Cast intrinsic for `Closure(Closure { dtor_idx: 93, function: Function { arguments: [], shim_idx: 94, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
            const ret = makeMutClosure(arg0, arg1, wasm.wasm_bindgen__closure__destroy__h83c8c2db16b120ac, wasm_bindgen__convert__closures_____invoke__ha067de4be952b5b6);
            return ret;
        },
        __wbindgen_cast_0000000000000004: function(arg0) {
            // Cast intrinsic for `F64 -> Externref`.
            const ret = arg0;
            return ret;
        },
        __wbindgen_cast_0000000000000005: function(arg0) {
            // Cast intrinsic for `I64 -> Externref`.
            const ret = arg0;
            return ret;
        },
        __wbindgen_cast_0000000000000006: function(arg0, arg1) {
            // Cast intrinsic for `Ref(String) -> Externref`.
            const ret = getStringFromWasm0(arg0, arg1);
            return ret;
        },
        __wbindgen_cast_0000000000000007: function(arg0) {
            // Cast intrinsic for `U64 -> Externref`.
            const ret = BigInt.asUintN(64, arg0);
            return ret;
        },
        __wbindgen_init_externref_table: function() {
            const table = wasm.__wbindgen_externrefs;
            const offset = table.grow(4);
            table.set(0, undefined);
            table.set(offset + 0, undefined);
            table.set(offset + 1, null);
            table.set(offset + 2, true);
            table.set(offset + 3, false);
        },
    };
    return {
        __proto__: null,
        "./presence_web_bg.js": import0,
    };
}

function wasm_bindgen__convert__closures_____invoke__ha067de4be952b5b6(arg0, arg1) {
    wasm.wasm_bindgen__convert__closures_____invoke__ha067de4be952b5b6(arg0, arg1);
}

function wasm_bindgen__convert__closures_____invoke__h02c82abf5f4209d1(arg0, arg1, arg2) {
    wasm.wasm_bindgen__convert__closures_____invoke__h02c82abf5f4209d1(arg0, arg1, arg2);
}


const __wbindgen_enum_BinaryType = ["blob", "arraybuffer"];
const PresenceWebFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_presenceweb_free(ptr >>> 0, 1));
const WasmPresenceFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_wasmpresence_free(ptr >>> 0, 1));

function addToExternrefTable0(obj) {
    const idx = wasm.__externref_table_alloc();
    wasm.__wbindgen_externrefs.set(idx, obj);
    return idx;
}

const CLOSURE_DTORS = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(state => state.dtor(state.a, state.b));

function debugString(val) {
    // primitive types
    const type = typeof val;
    if (type == 'number' || type == 'boolean' || val == null) {
        return  `${val}`;
    }
    if (type == 'string') {
        return `"${val}"`;
    }
    if (type == 'symbol') {
        const description = val.description;
        if (description == null) {
            return 'Symbol';
        } else {
            return `Symbol(${description})`;
        }
    }
    if (type == 'function') {
        const name = val.name;
        if (typeof name == 'string' && name.length > 0) {
            return `Function(${name})`;
        } else {
            return 'Function';
        }
    }
    // objects
    if (Array.isArray(val)) {
        const length = val.length;
        let debug = '[';
        if (length > 0) {
            debug += debugString(val[0]);
        }
        for(let i = 1; i < length; i++) {
            debug += ', ' + debugString(val[i]);
        }
        debug += ']';
        return debug;
    }
    // Test for built-in
    const builtInMatches = /\[object ([^\]]+)\]/.exec(toString.call(val));
    let className;
    if (builtInMatches && builtInMatches.length > 1) {
        className = builtInMatches[1];
    } else {
        // Failed to match the standard '[object ClassName]'
        return toString.call(val);
    }
    if (className == 'Object') {
        // we're a user defined class or Object
        // JSON.stringify avoids problems with cycles, and is generally much
        // easier than looping through ownProperties of `val`.
        try {
            return 'Object(' + JSON.stringify(val) + ')';
        } catch (_) {
            return 'Object';
        }
    }
    // errors
    if (val instanceof Error) {
        return `${val.name}: ${val.message}\n${val.stack}`;
    }
    // TODO we could test for more things here, like `Set`s and `Map`s.
    return className;
}

function getArrayU8FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getUint8ArrayMemory0().subarray(ptr / 1, ptr / 1 + len);
}

let cachedDataViewMemory0 = null;
function getDataViewMemory0() {
    if (cachedDataViewMemory0 === null || cachedDataViewMemory0.buffer.detached === true || (cachedDataViewMemory0.buffer.detached === undefined && cachedDataViewMemory0.buffer !== wasm.memory.buffer)) {
        cachedDataViewMemory0 = new DataView(wasm.memory.buffer);
    }
    return cachedDataViewMemory0;
}

function getStringFromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return decodeText(ptr, len);
}

let cachedUint8ArrayMemory0 = null;
function getUint8ArrayMemory0() {
    if (cachedUint8ArrayMemory0 === null || cachedUint8ArrayMemory0.byteLength === 0) {
        cachedUint8ArrayMemory0 = new Uint8Array(wasm.memory.buffer);
    }
    return cachedUint8ArrayMemory0;
}

function handleError(f, args) {
    try {
        return f.apply(this, args);
    } catch (e) {
        const idx = addToExternrefTable0(e);
        wasm.__wbindgen_exn_store(idx);
    }
}

function isLikeNone(x) {
    return x === undefined || x === null;
}

function makeMutClosure(arg0, arg1, dtor, f) {
    const state = { a: arg0, b: arg1, cnt: 1, dtor };
    const real = (...args) => {

        // First up with a closure we increment the internal reference
        // count. This ensures that the Rust closure environment won't
        // be deallocated while we're invoking it.
        state.cnt++;
        const a = state.a;
        state.a = 0;
        try {
            return f(a, state.b, ...args);
        } finally {
            state.a = a;
            real._wbg_cb_unref();
        }
    };
    real._wbg_cb_unref = () => {
        if (--state.cnt === 0) {
            state.dtor(state.a, state.b);
            state.a = 0;
            CLOSURE_DTORS.unregister(state);
        }
    };
    CLOSURE_DTORS.register(real, state, state);
    return real;
}

function passStringToWasm0(arg, malloc, realloc) {
    if (realloc === undefined) {
        const buf = cachedTextEncoder.encode(arg);
        const ptr = malloc(buf.length, 1) >>> 0;
        getUint8ArrayMemory0().subarray(ptr, ptr + buf.length).set(buf);
        WASM_VECTOR_LEN = buf.length;
        return ptr;
    }

    let len = arg.length;
    let ptr = malloc(len, 1) >>> 0;

    const mem = getUint8ArrayMemory0();

    let offset = 0;

    for (; offset < len; offset++) {
        const code = arg.charCodeAt(offset);
        if (code > 0x7F) break;
        mem[ptr + offset] = code;
    }
    if (offset !== len) {
        if (offset !== 0) {
            arg = arg.slice(offset);
        }
        ptr = realloc(ptr, len, len = offset + arg.length * 3, 1) >>> 0;
        const view = getUint8ArrayMemory0().subarray(ptr + offset, ptr + len);
        const ret = cachedTextEncoder.encodeInto(arg, view);

        offset += ret.written;
        ptr = realloc(ptr, len, offset, 1) >>> 0;
    }

    WASM_VECTOR_LEN = offset;
    return ptr;
}

let cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
cachedTextDecoder.decode();
const MAX_SAFARI_DECODE_BYTES = 2146435072;
let numBytesDecoded = 0;
function decodeText(ptr, len) {
    numBytesDecoded += len;
    if (numBytesDecoded >= MAX_SAFARI_DECODE_BYTES) {
        cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
        cachedTextDecoder.decode();
        numBytesDecoded = len;
    }
    return cachedTextDecoder.decode(getUint8ArrayMemory0().subarray(ptr, ptr + len));
}

const cachedTextEncoder = new TextEncoder();

if (!('encodeInto' in cachedTextEncoder)) {
    cachedTextEncoder.encodeInto = function (arg, view) {
        const buf = cachedTextEncoder.encode(arg);
        view.set(buf);
        return {
            read: arg.length,
            written: buf.length
        };
    };
}

let WASM_VECTOR_LEN = 0;

let wasmModule, wasm;
function __wbg_finalize_init(instance, module) {
    wasm = instance.exports;
    wasmModule = module;
    cachedDataViewMemory0 = null;
    cachedUint8ArrayMemory0 = null;
    wasm.__wbindgen_start();
    return wasm;
}

async function __wbg_load(module, imports) {
    if (typeof Response === 'function' && module instanceof Response) {
        if (typeof WebAssembly.instantiateStreaming === 'function') {
            try {
                return await WebAssembly.instantiateStreaming(module, imports);
            } catch (e) {
                const validResponse = module.ok && expectedResponseType(module.type);

                if (validResponse && module.headers.get('Content-Type') !== 'application/wasm') {
                    console.warn("`WebAssembly.instantiateStreaming` failed because your server does not serve Wasm with `application/wasm` MIME type. Falling back to `WebAssembly.instantiate` which is slower. Original error:\n", e);

                } else { throw e; }
            }
        }

        const bytes = await module.arrayBuffer();
        return await WebAssembly.instantiate(bytes, imports);
    } else {
        const instance = await WebAssembly.instantiate(module, imports);

        if (instance instanceof WebAssembly.Instance) {
            return { instance, module };
        } else {
            return instance;
        }
    }

    function expectedResponseType(type) {
        switch (type) {
            case 'basic': case 'cors': case 'default': return true;
        }
        return false;
    }
}

function initSync(module) {
    if (wasm !== undefined) return wasm;


    if (module !== undefined) {
        if (Object.getPrototypeOf(module) === Object.prototype) {
            ({module} = module)
        } else {
            console.warn('using deprecated parameters for `initSync()`; pass a single object instead')
        }
    }

    const imports = __wbg_get_imports();
    if (!(module instanceof WebAssembly.Module)) {
        module = new WebAssembly.Module(module);
    }
    const instance = new WebAssembly.Instance(module, imports);
    return __wbg_finalize_init(instance, module);
}

async function __wbg_init(module_or_path) {
    if (wasm !== undefined) return wasm;


    if (module_or_path !== undefined) {
        if (Object.getPrototypeOf(module_or_path) === Object.prototype) {
            ({module_or_path} = module_or_path)
        } else {
            console.warn('using deprecated parameters for the initialization function; pass a single object instead')
        }
    }

    if (module_or_path === undefined) {
        module_or_path = new URL('presence_web_bg.wasm', import.meta.url);
    }
    const imports = __wbg_get_imports();

    if (typeof module_or_path === 'string' || (typeof Request === 'function' && module_or_path instanceof Request) || (typeof URL === 'function' && module_or_path instanceof URL)) {
        module_or_path = fetch(module_or_path);
    }

    const { instance, module } = await __wbg_load(await module_or_path, imports);

    return __wbg_finalize_init(instance, module);
}

export { initSync, __wbg_init as default };
