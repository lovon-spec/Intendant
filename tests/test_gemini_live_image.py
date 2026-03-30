#!/usr/bin/env python3
"""
Standalone test: does Gemini Live (native audio preview) process inline images?

Sends a client_content message with an inlineData JPEG + text annotation,
then asks the model to describe what it sees. Checks if the model can
actually see the image or only the text metadata.

Usage:
    source .env && python3 tests/test_gemini_live_image.py
"""

import asyncio
import base64
import json
import os
import sys
import struct

# Generate a simple test image (red square) as JPEG
# We'll use a minimal JPEG so we don't need PIL
def make_test_png():
    """Create a 100x100 solid red PNG (no dependencies)."""
    width, height = 100, 100
    # RGBA: red pixels
    raw_row = b''
    for _ in range(width):
        raw_row += b'\xff\x00\x00\xff'  # RGBA red

    import zlib
    def make_chunk(chunk_type, data):
        c = chunk_type + data
        crc = struct.pack('>I', zlib.crc32(c) & 0xFFFFFFFF)
        return struct.pack('>I', len(data)) + c + crc

    # PNG signature
    sig = b'\x89PNG\r\n\x1a\n'
    # IHDR
    ihdr_data = struct.pack('>IIBBBBB', width, height, 8, 6, 0, 0, 0)  # 8bit RGBA
    ihdr = make_chunk(b'IHDR', ihdr_data)
    # IDAT
    raw_data = b''
    for _ in range(height):
        raw_data += b'\x00' + raw_row  # filter byte + row
    compressed = zlib.compress(raw_data)
    idat = make_chunk(b'IDAT', compressed)
    # IEND
    iend = make_chunk(b'IEND', b'')

    return sig + ihdr + idat + iend


async def test_gemini_live_image():
    try:
        import websockets
    except ImportError:
        print("Installing websockets...")
        os.system(f"{sys.executable} -m pip install websockets -q")
        import websockets

    api_key = os.environ.get("GEMINI_API_KEY")
    if not api_key:
        print("ERROR: GEMINI_API_KEY not set")
        sys.exit(1)

    model = "gemini-2.5-flash-native-audio-preview-12-2025"
    url = f"wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent?key={api_key}"

    print(f"Model: {model}")
    print(f"Connecting to Gemini Live API...")

    async with websockets.connect(url) as ws:
        # Step 1: Send setup message
        setup = {
            "setup": {
                "model": f"models/{model}",
                "generation_config": {
                    "response_modalities": ["AUDIO"],
                    "speech_config": {
                        "voice_config": {
                            "prebuilt_voice_config": {
                                "voice_name": "Aoede"
                            }
                        }
                    }
                }
            }
        }
        await ws.send(json.dumps(setup))
        resp = await ws.recv()
        setup_resp = json.loads(resp)
        if "setupComplete" in str(resp):
            print("Setup complete")
        else:
            print(f"Setup response: {resp[:200]}")

        # Step 2: Create a test image (red 100x100 PNG)
        test_png = make_test_png()
        img_b64 = base64.b64encode(test_png).decode()
        print(f"Test image: 100x100 red PNG ({len(test_png)} bytes, {len(img_b64)} base64 chars)")

        # Step 3: Send image + text annotation (same format as our send_frame)
        frame_msg = {
            "client_content": {
                "turns": [{
                    "role": "user",
                    "parts": [
                        {
                            "inlineData": {
                                "mimeType": "image/png",
                                "data": img_b64
                            }
                        },
                        {
                            "text": "[frame:test-f00001]"
                        }
                    ]
                }],
                "turn_complete": False
            }
        }
        await ws.send(json.dumps(frame_msg))
        print("Sent frame with image + text annotation")

        # Step 4: Ask the model to describe what it sees
        ask_msg = {
            "client_content": {
                "turns": [{
                    "role": "user",
                    "parts": [{"text": "What color is the image I just sent you? Describe exactly what you see in the image. If you cannot see any image, say 'NO IMAGE VISIBLE'."}]
                }],
                "turn_complete": True
            }
        }
        await ws.send(json.dumps(ask_msg))
        print("Asked model to describe the image")
        print("---")

        # Step 5: Collect response (text from thinking/transcript, skip audio blobs)
        full_text = ""
        while True:
            try:
                resp = await asyncio.wait_for(ws.recv(), timeout=15)
                data = json.loads(resp)

                if "serverContent" in data:
                    sc = data["serverContent"]
                    if "modelTurn" in sc:
                        for part in sc["modelTurn"].get("parts", []):
                            if "text" in part:
                                full_text += part["text"]
                                print(f"  TEXT: {part['text']}")
                            elif "inlineData" in part:
                                pass  # audio blob, skip
                    if sc.get("turnComplete"):
                        print("---")
                        break
                # Also check for usageMetadata
                if "usageMetadata" in data:
                    um = data["usageMetadata"]
                    print(f"  TOKENS: prompt={um.get('promptTokenCount',0)} response={um.get('candidatesTokenCount',0)} total={um.get('totalTokenCount',0)}")
            except asyncio.TimeoutError:
                print("Timed out waiting for response")
                break

        # Step 6: Verdict
        print()
        text_lower = full_text.lower()
        if "red" in text_lower:
            print("RESULT: Model CAN see images (correctly identified red)")
        elif "no image" in text_lower or "cannot see" in text_lower or "don't see" in text_lower or "metadata" in text_lower:
            print("RESULT: Model CANNOT see images (only text/metadata)")
        else:
            print(f"RESULT: Unclear — model said: {full_text[:200]}")

        # Step 7: Also test — can it see images in tool_response?
        print("\n=== Test 2: Image in tool_response ===")

        # Simulate a function call by asking the model to call a tool
        # Actually, let's just send another image with turn_complete:true
        frame_msg2 = {
            "client_content": {
                "turns": [{
                    "role": "user",
                    "parts": [
                        {"text": "I am sending you a blue square image now. What color is it?"},
                        {
                            "inlineData": {
                                "mimeType": "image/png",
                                "data": img_b64  # still red, to see if model hallucinates "blue"
                            }
                        },
                    ]
                }],
                "turn_complete": True
            }
        }
        await ws.send(json.dumps(frame_msg2))
        print("Sent image WITH turn_complete:true + misleading text ('blue square')")
        print("If model says 'red' = it sees the image. If 'blue' = reading text only.")
        print("---")

        full_text2 = ""
        while True:
            try:
                resp = await asyncio.wait_for(ws.recv(), timeout=15)
                data = json.loads(resp)
                if "serverContent" in data:
                    sc = data["serverContent"]
                    if "modelTurn" in sc:
                        for part in sc["modelTurn"].get("parts", []):
                            if "text" in part:
                                full_text2 += part["text"]
                                print(f"  TEXT: {part['text']}")
                    if sc.get("turnComplete"):
                        print("---")
                        break
                if "usageMetadata" in data:
                    um = data["usageMetadata"]
                    print(f"  TOKENS: prompt={um.get('promptTokenCount',0)} response={um.get('candidatesTokenCount',0)} total={um.get('totalTokenCount',0)}")
            except asyncio.TimeoutError:
                print("Timed out")
                break

        print()
        t2 = full_text2.lower()
        if "red" in t2 and "blue" not in t2:
            print("RESULT: Model sees ACTUAL image content (said red, not blue)")
        elif "blue" in t2:
            print("RESULT: Model reads TEXT only, not image data (said blue)")
        else:
            print(f"RESULT: Unclear — {full_text2[:200]}")


if __name__ == "__main__":
    asyncio.run(test_gemini_live_image())
