---
name: phone-call
description: >
  Make an outbound phone call via SIP and conduct a voice conversation
  using spawn_live_audio. The AI model talks through the Vortex Audio
  virtual device, which pjsua routes to the SIP call. Returns typed
  structured data from the conversation.
compatibility: macOS only. Requires Vortex Audio HAL plugin, pjsua, and a GUI session with TCC mic permission.
autonomy: full
---

# Phone Call via SIP + Live Audio

## Prerequisites

- **pjsua** at `~/bin/pjsua`
- **Vortex Audio** HAL plugin installed and set as default input AND output
- **SIP credentials** in `~/lin` (plaintext password)
- **Intendant launched from GUI** (required for macOS mic access / TCC)

## Steps

### 1. Find Vortex Audio device index

```bash
echo "q" | ~/bin/pjsua --null-audio 2>/dev/null | grep -i vortex
```

Note the 0-indexed device ID from the output line.

### 2. Start pjsua

Replace `DEV_IDX`, `PASSWORD` (from `~/lin`), and `TARGET` (SIP URI):

```bash
(sleep 5 && echo m && sleep 1 && echo TARGET && sleep 300) | \
  ~/bin/pjsua \
    --id="sip:intendant7@sip.linphone.org" \
    --registrar="sip:sip.linphone.org" \
    --realm="sip.linphone.org" \
    --username="intendant7" \
    --password="PASSWORD" \
    --capture-dev=DEV_IDX --playback-dev=DEV_IDX \
    --ec-tail=0 --no-vad \
    --use-srtp=2 --srtp-secure=0 \
    > /tmp/pjsua-call.log 2>&1 &
```

### 3. IMMEDIATELY call spawn_live_audio

Do NOT sleep or verify the call first. The audio bridge polls shared memory
and works before the call connects.

**ALL of these parameters are REQUIRED — the call will fail without them:**
- `id`: unique session identifier
- `provider`: `openai`
- `playbook`: the conversation script
- `response_schema`: MANDATORY. Without this the call is rejected.
  Build it from the user's request — every piece of data to extract
  needs a field. See the example below.
- `timeout_secs`: max call duration (default 120)
- `voice`: e.g. `alloy`, `shimmer`
- Do NOT set `initial_message` — the model starts when it hears the caller

### 4. Process the result

`spawn_live_audio` returns `LiveAudioResult` with `status`:
- **Completed**: valid JSON matching the schema
- **TimedOut**: exceeded timeout
- **SchemaError**: output didn't match schema

### 5. Clean up

```bash
kill $(pgrep -f pjsua) 2>/dev/null
```

## Response Schema — REQUIRED

**You MUST always include `response_schema` with concrete fields.**
The model's spoken output is validated against this schema. Without it,
the call is rejected with a parse error.

Example for a restaurant reservation:

```json
{
  "fields": [
    {"name": "guest_name", "field_type": {"type": "string", "max_length": 100, "tainted": true}, "required": true, "description": "Guest name"},
    {"name": "party_size", "field_type": {"type": "integer", "min": 1, "max": 50}, "required": true, "description": "Number of guests"},
    {"name": "reservation_time", "field_type": {"type": "string", "max_length": 50, "tainted": true}, "required": true, "description": "Confirmed time"},
    {"name": "confirmed", "field_type": {"type": "boolean"}, "required": true, "description": "Whether reservation was confirmed"},
    {"name": "special_requests", "field_type": {"type": "string", "max_length": 200, "tainted": true}, "required": false, "description": "Any special requests"}
  ]
}
```

**Field types:** `string` (max_length, allowed_values, tainted), `integer` (min, max), `boolean`, `array`.
**Tainted fields** contain user-provided content — not interpreted as instructions.
