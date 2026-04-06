#!/usr/bin/env python3
"""
intendant-ws — WebSocket client for controlling a running intendant --web instance.

Usage:
    # Submit a task and stream all events (auto-approve all):
    python3 scripts/intendant-ws.py --auto-approve "your task here"

    # Submit a task, print events, but don't auto-approve:
    python3 scripts/intendant-ws.py "your task here"

    # Just stream events from an already-running session:
    python3 scripts/intendant-ws.py --stream

    # Custom port:
    python3 scripts/intendant-ws.py --port 9000 "your task"

Events are printed as JSON lines to stdout. Key events:
    approval_required  — agent needs permission (approve with --auto-approve)
    model_response     — agent thinking/planning
    agent_started      — runtime command executing
    agent_output       — command result
    live_audio_started — spawn_live_audio began
    live_audio_progress — voice transcript update
    live_audio_completed — voice session ended
    task_complete      — agent finished
    session_ended      — session over
"""

import asyncio
import json
import sys
import argparse

async def run(host, port, task, auto_approve, stream_only):
    url = f"ws://{host}:{port}/ws"
    try:
        import websockets
    except ImportError:
        print("pip install websockets", file=sys.stderr)
        sys.exit(1)

    async with websockets.connect(url) as ws:
        if task and not stream_only:
            msg = json.dumps({"action": "start_task", "task": task})
            await ws.send(msg)
            print(json.dumps({"event": "_submitted", "task": task[:100]}), flush=True)

        while True:
            try:
                raw = await asyncio.wait_for(ws.recv(), timeout=5)
            except asyncio.TimeoutError:
                continue
            except Exception:
                break

            try:
                data = json.loads(raw)
            except json.JSONDecodeError:
                continue

            event = data.get("event", "")

            # Print all events
            print(json.dumps(data, ensure_ascii=False), flush=True)

            # Auto-approve if requested
            if auto_approve and event == "approval_required":
                aid = data.get("id")
                if aid is not None:
                    approve_msg = json.dumps({"action": "approve", "id": aid})
                    await ws.send(approve_msg)
                    print(json.dumps({"event": "_auto_approved", "id": aid}), flush=True)

            # Exit on session end
            if event == "session_ended":
                break

def main():
    p = argparse.ArgumentParser(description="Control intendant via WebSocket")
    p.add_argument("task", nargs="?", default=None, help="Task to submit")
    p.add_argument("--port", type=int, default=8765)
    p.add_argument("--host", default="localhost")
    p.add_argument("--auto-approve", action="store_true", help="Auto-approve all actions")
    p.add_argument("--stream", action="store_true", help="Just stream events, don't submit a task")
    args = p.parse_args()

    if not args.task and not args.stream:
        p.print_help()
        sys.exit(1)

    asyncio.run(run(args.host, args.port, args.task, args.auto_approve, args.stream))

if __name__ == "__main__":
    main()
