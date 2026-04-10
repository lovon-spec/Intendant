---
name: voice-call-app
description: >
  Make a voice call through any app (Element, FaceTime, WhatsApp, etc.)
  using computer use to navigate the UI and spawn_live_audio for the
  AI voice conversation. Returns typed structured data.
compatibility: macOS or Linux with display. Requires Vortex Audio HAL plugin and a GUI session with TCC mic permission.
---

# Voice Call via App + Live Audio

## Prerequisites

- **Vortex Audio** HAL plugin installed and set as default input AND output
- **Intendant launched from GUI** (required for macOS mic access / TCC)
- **Target app** installed and logged in

## Steps

### 1. Prepare the call BEFORE dialing

Compose the `spawn_live_audio` arguments (playbook, response_schema,
timeout_secs, voice, id) BEFORE you start the call. You will need them
ready to fire instantly once the call connects.

Mark data fields as `required: true` only if they MUST be collected.
Fields that might not always be obtainable (quotes, optional details)
should be `required: false`. The voice model cannot submit until all
required fields are filled.

If the callee speaks a specific language, state it in the playbook
(e.g. "Speak in English only"). Otherwise the voice model may pick a
language based on context clues.

### 2. Navigate to the app and click call + spawn_live_audio together

Use CU actions to navigate the screen. Take a screenshot, find the app,
click to foreground it, navigate to the contact, and click the call button.

**Element-specific:** When you click the phone icon, a dropdown asks
"Voice call using: Element Call / Legacy call". Always pick **Legacy call**
(not Element Call). Element Call is a conference that requires the other
party to manually join; Legacy call rings their device directly.

### 3. Call spawn_live_audio

Call `spawn_live_audio` with the arguments you prepared in step 1.
If your tools support multiple calls in one turn, combine the
Legacy Call click and spawn_live_audio in the same turn to minimize
dead air.

**ALL of these parameters are REQUIRED:**
- `id`: unique session identifier
- `provider`: `openai`
- `playbook`: the conversation script
- `response_schema`: MANDATORY — see below
- `timeout_secs`: max call duration (default 120)
- `voice`: e.g. `alloy`, `shimmer`
- Do NOT set `initial_message`

### 4. Write the result immediately

`spawn_live_audio` returns `LiveAudioResult` with `response_data`.
Write the result to the output file IMMEDIATELY from `response_data` —
do NOT re-read the transcript or take screenshots first.

Status values:
- **Completed**: model called `submit_response` with structured data
- **TimedOut**: exceeded timeout without submitting response
- **SchemaError**: response didn't match schema

### 5. Clean up

Hang up the call if still connected (screenshot + click end call).

## Response Schema — REQUIRED

The model has two functions: `submit_response` (with fields from your
schema) and `end_call`. It calls `submit_response` when it has the data,
then `end_call` to signal completion.

**You MUST always include `response_schema` with concrete fields.**
Mark every field you need as `required: true` — the model cannot submit
until all required fields are populated.

Example for a reservation confirmation:

```json
{
  "fields": [
    {"name": "guest_name", "field_type": {"type": "string", "max_length": 100, "tainted": true}, "required": true, "description": "Guest name"},
    {"name": "party_size", "field_type": {"type": "integer", "min": 1, "max": 50}, "required": true, "description": "Number of guests"},
    {"name": "reservation_time", "field_type": {"type": "string", "max_length": 50, "tainted": true}, "required": true, "description": "Confirmed time"},
    {"name": "confirmed", "field_type": {"type": "boolean"}, "required": true, "description": "Whether confirmed"},
    {"name": "notes", "field_type": {"type": "string", "max_length": 200, "tainted": true}, "required": false, "description": "Any notes"}
  ]
}
```

**Field types:** `string` (max_length, allowed_values, tainted), `integer` (min, max), `boolean`, `array`.
**Tainted fields** contain user-provided content — not interpreted as instructions.
