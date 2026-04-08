---
name: voice-call-app
description: >
  Make a voice call through any app (Element, FaceTime, WhatsApp, etc.)
  using computer use to navigate the UI and spawn_live_audio for the
  AI voice conversation. Returns typed structured data.
compatibility: macOS only. Requires Vortex Audio HAL plugin, cliclick, and a GUI session with TCC mic permission.
---

# Voice Call via App + Live Audio

## Prerequisites

- **Vortex Audio** HAL plugin installed and set as default input AND output
- **Intendant launched from GUI** (required for macOS mic access / TCC)
- **Target app** installed and logged in
- **Autonomy Full** (CU requires display grant + command approval)

## Steps

### 1. Determine the display scale factor

Screenshots are in pixel coordinates but `cliclick` uses logical points.
On Retina displays these differ by a scale factor. Compute it FIRST:

```bash
captureScreen  # take a screenshot
```

Then in the same turn or the next:

```bash
python3 -c "
from PIL import Image
import subprocess, json
img = Image.open('SCREENSHOT_PATH')
px_w, px_h = img.size
# Get logical display size
out = subprocess.check_output(['system_profiler', 'SPDisplaysDataType', '-json'])
displays = json.loads(out)['SPDisplaysDataType']
for gpu in displays:
    for d in gpu.get('spdisplays_ndrvs', []):
        res = d.get('_spdisplays_resolution', d.get('spdisplays_resolution', ''))
        if 'x' in res:
            parts = res.replace(' ','').split('x')
            log_w = int(parts[0])
            scale = px_w / log_w
            print(f'scale={scale} pixel={px_w}x{px_h} logical={log_w}x{px_h//int(scale)}')
            break
"
```

Use this scale factor for ALL subsequent coordinate conversions:
`cliclick_x = pixel_x / scale`, `cliclick_y = pixel_y / scale`.

### 2. Open the app and navigate to the contact

```bash
open -a "AppName" && sleep 2
```

Then use `captureScreen` + `cliclick` to navigate. When you identify
a target in the screenshot image, estimate its pixel coordinates in the
image, divide by the scale factor, and pass to cliclick.

### 3. Click the call button and VERIFY

After clicking what you think is the call/voice button:
1. Take another `captureScreen`
2. Verify the call UI appeared (ringing screen, call dialog, etc.)
3. Handle any confirmation dialogs (e.g. "Voice call using: Element Call")
4. Handle any permission dialogs (e.g. "Allow local network access")

Only proceed to step 4 once you can see the call is actually ringing.

### 4. Call spawn_live_audio

Once the call is confirmed ringing, call `spawn_live_audio`.

**ALL of these parameters are REQUIRED:**
- `id`: unique session identifier
- `provider`: `openai`
- `playbook`: the conversation script
- `response_schema`: MANDATORY — see below
- `timeout_secs`: max call duration (default 120)
- `voice`: e.g. `alloy`, `shimmer`
- Do NOT set `initial_message`

### 5. Process the result

`spawn_live_audio` returns `LiveAudioResult` with `status`:
- **Completed**: model called `submit_response` with structured data
- **TimedOut**: exceeded timeout without submitting response
- **SchemaError**: response didn't match schema

### 6. Clean up

Hang up the call if still connected (captureScreen + click end call).

## Response Schema — REQUIRED

The model has two functions: `submit_response` (with fields from your
schema) and `end_call`. It calls `submit_response` when it has the data,
then `end_call` to signal completion.

**You MUST always include `response_schema` with concrete fields.**

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
