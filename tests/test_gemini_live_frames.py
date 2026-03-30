#!/usr/bin/env python3
"""
Realistic test: stream real session frames to Gemini Live at 1fps,
then ask the model what it sees. Simulates our actual harness behavior.

Uses frames from a real session directory.

Usage:
    source .env && python3 tests/test_gemini_live_frames.py [session_dir]
"""

import asyncio
import base64
import json
import os
import sys
import glob
import time


async def collect_response(ws, timeout=15):
    """Collect text + token info from model response until turnComplete."""
    full_text = ""
    tokens = {}
    while True:
        try:
            resp = await asyncio.wait_for(ws.recv(), timeout=timeout)
            data = json.loads(resp)
            if "serverContent" in data:
                sc = data["serverContent"]
                if "modelTurn" in sc:
                    for part in sc["modelTurn"].get("parts", []):
                        if "text" in part:
                            full_text += part["text"]
                if sc.get("turnComplete"):
                    break
            if "usageMetadata" in data:
                tokens = data["usageMetadata"]
        except asyncio.TimeoutError:
            print("  [timeout]")
            break
    return full_text, tokens


async def test_streaming_frames():
    try:
        import websockets
    except ImportError:
        os.system(f"{sys.executable} -m pip install websockets -q")
        import websockets

    api_key = os.environ.get("GEMINI_API_KEY")
    if not api_key:
        print("ERROR: GEMINI_API_KEY not set")
        sys.exit(1)

    # Find frames directory
    if len(sys.argv) > 1:
        frames_dir = os.path.join(sys.argv[1], "frames")
    else:
        # Use most recent session with frames
        log_dir = os.path.expanduser("~/.intendant/logs")
        sessions = sorted(glob.glob(f"{log_dir}/*/frames"), key=os.path.getmtime, reverse=True)
        for s in sessions:
            if len(os.listdir(s)) > 10:
                frames_dir = s
                break
        else:
            print("No session with frames found")
            sys.exit(1)

    frame_files = sorted(
        [f for f in os.listdir(frames_dir) if f.startswith("display_0-") and f.endswith(".jpg")],
        key=lambda f: int(f.split("-f")[1].split(".")[0])
    )
    print(f"Session frames: {frames_dir}")
    print(f"Available frames: {len(frame_files)}")
    if not frame_files:
        print("No display frames found")
        sys.exit(1)

    model = "gemini-2.5-flash-native-audio-preview-12-2025"
    url = f"wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent?key={api_key}"

    print(f"Model: {model}")
    print(f"Connecting...")

    async with websockets.connect(url) as ws:
        # Setup
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
                },
                "system_instruction": {
                    "parts": [{"text": "You are a helpful assistant. When asked what you see, describe the visual content of images you receive."}]
                }
            }
        }
        await ws.send(json.dumps(setup))
        resp = await ws.recv()
        print("Setup complete")

        # Grounding message (same as our harness)
        grounding = {
            "client_content": {
                "turns": [{"role": "user", "parts": [{"text": "[System: Ready. Waiting for user.]"}]}],
                "turn_complete": True
            }
        }
        await ws.send(json.dumps(grounding))
        text, tokens = await collect_response(ws)
        print(f"Greeting: {text[:100]}...")
        print(f"Tokens: {tokens.get('totalTokenCount', '?')}")

        # === Test 1: Send N frames at 1fps, then ask ===
        num_frames = min(10, len(frame_files))
        print(f"\n=== Streaming {num_frames} frames at 1fps ===")

        for i in range(num_frames):
            frame_path = os.path.join(frames_dir, frame_files[i])
            with open(frame_path, "rb") as f:
                img_data = f.read()
            img_b64 = base64.b64encode(img_data).decode()
            frame_id = frame_files[i].replace(".jpg", "")

            frame_msg = {
                "client_content": {
                    "turns": [{
                        "role": "user",
                        "parts": [
                            {"inlineData": {"mimeType": "image/jpeg", "data": img_b64}},
                            {"text": f"[frame:{frame_id}]"}
                        ]
                    }],
                    "turn_complete": False
                }
            }
            await ws.send(json.dumps(frame_msg))
            size_kb = len(img_data) / 1024
            print(f"  Sent {frame_id} ({size_kb:.0f}KB)")

            # Drain any server messages (audio, thinking) without blocking
            try:
                while True:
                    resp = await asyncio.wait_for(ws.recv(), timeout=0.1)
                    data = json.loads(resp)
                    if "serverContent" in data:
                        sc = data["serverContent"]
                        if "modelTurn" in sc:
                            for part in sc["modelTurn"].get("parts", []):
                                if "text" in part:
                                    print(f"    [model thinking]: {part['text'][:80]}")
            except asyncio.TimeoutError:
                pass

            if i < num_frames - 1:
                await asyncio.sleep(1.0)

        # Ask what it sees
        print("\n=== Asking: 'What do you see?' ===")
        ask = {
            "client_content": {
                "turns": [{"role": "user", "parts": [
                    {"text": "Describe what you see in the frames I've been sending. Be specific about window titles, text, colors, and UI elements."}
                ]}],
                "turn_complete": True
            }
        }
        await ws.send(json.dumps(ask))
        text1, tokens1 = await collect_response(ws, timeout=20)
        print(f"Response: {text1[:500]}")
        print(f"Tokens: {tokens1.get('totalTokenCount', '?')}")

        # === Test 2: Send 20 more frames, ask again ===
        more_frames = min(20, len(frame_files) - num_frames)
        if more_frames > 0:
            print(f"\n=== Streaming {more_frames} more frames ===")
            for i in range(num_frames, num_frames + more_frames):
                frame_path = os.path.join(frames_dir, frame_files[i])
                with open(frame_path, "rb") as f:
                    img_data = f.read()
                img_b64 = base64.b64encode(img_data).decode()
                frame_id = frame_files[i].replace(".jpg", "")

                frame_msg = {
                    "client_content": {
                        "turns": [{
                            "role": "user",
                            "parts": [
                                {"inlineData": {"mimeType": "image/jpeg", "data": img_b64}},
                                {"text": f"[frame:{frame_id}]"}
                            ]
                        }],
                        "turn_complete": False
                    }
                }
                await ws.send(json.dumps(frame_msg))
                if i % 5 == 0:
                    size_kb = len(img_data) / 1024
                    print(f"  Sent {frame_id} ({size_kb:.0f}KB)")
                await asyncio.sleep(1.0)

            print("\n=== Asking again: 'What do you see now?' ===")
            ask2 = {
                "client_content": {
                    "turns": [{"role": "user", "parts": [
                        {"text": "What do you see now? Has anything changed?"}
                    ]}],
                    "turn_complete": True
                }
            }
            await ws.send(json.dumps(ask2))
            text2, tokens2 = await collect_response(ws, timeout=20)
            print(f"Response: {text2[:500]}")
            print(f"Tokens: {tokens2.get('totalTokenCount', '?')}")

        # === Test 3: Rapid burst — 5 frames with no delay ===
        burst_start = num_frames + more_frames
        burst_count = min(5, len(frame_files) - burst_start)
        if burst_count > 0:
            print(f"\n=== Burst: {burst_count} frames with no delay ===")
            for i in range(burst_start, burst_start + burst_count):
                frame_path = os.path.join(frames_dir, frame_files[i])
                with open(frame_path, "rb") as f:
                    img_data = f.read()
                img_b64 = base64.b64encode(img_data).decode()
                frame_id = frame_files[i].replace(".jpg", "")

                frame_msg = {
                    "client_content": {
                        "turns": [{
                            "role": "user",
                            "parts": [
                                {"inlineData": {"mimeType": "image/jpeg", "data": img_b64}},
                                {"text": f"[frame:{frame_id}]"}
                            ]
                        }],
                        "turn_complete": False
                    }
                }
                await ws.send(json.dumps(frame_msg))

            print("  Sent all burst frames")

            ask3 = {
                "client_content": {
                    "turns": [{"role": "user", "parts": [
                        {"text": "What do you see in the latest frames?"}
                    ]}],
                    "turn_complete": True
                }
            }
            await ws.send(json.dumps(ask3))
            text3, tokens3 = await collect_response(ws, timeout=20)
            print(f"Response: {text3[:500]}")
            print(f"Tokens: {tokens3.get('totalTokenCount', '?')}")

        print("\n=== Summary ===")
        print("If the model described actual screen content (Intendant window, dock, etc.), frames ARE processed.")
        print("If it only mentioned metadata/frame IDs, frames are NOT processed.")


if __name__ == "__main__":
    asyncio.run(test_streaming_frames())
