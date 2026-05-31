---
name: visual-collaboration
description: Use when the agent should cooperate with the user through Intendant's shared display surface: showing work, asking the user to watch a screen, focusing attention on a region, capturing a display frame, or requesting human input authority.
---

# Visual Collaboration

Use this skill when work benefits from a shared visual surface with the user: UI debugging, demos, app setup, auth handoff, remote desktop cooperation, visual inspection, or explaining what is happening on a display.

## Core Tools

- `show_shared_view`: opens the shared display surface and marks the relevant display as the shared view. For `user_session` / the primary display, this also requests display-stream activation.
- `focus_shared_view`: highlights a normalized region `{x, y, width, height}` on the shared display. Coordinates are fractions from 0.0 to 1.0.
- `capture_shared_view_frame`: captures the current display as an MCP image and foregrounds the same dashboard view.
- `request_shared_view_input`: asks the user to take input authority. The user must click the dashboard control; the tool does not grant control by itself.
- `hide_shared_view`: dismisses the banner and focus overlay when collaboration is done.

## Workflow

1. Call `show_shared_view` before work the user should watch or participate in. Prefer `display_target: "user_session"` when you mean the user's host screen.
2. Use `focus_shared_view` whenever you reference a specific UI area. Keep notes short and concrete.
3. Use `capture_shared_view_frame` when you need to reason about the current pixels.
4. Use `request_shared_view_input` only when the user needs to type, approve auth, choose from an account picker, or otherwise act directly.
5. Call `hide_shared_view` when the shared visual moment is over.

## Display Targets

Prefer `display_id` when known. Use `display_target` otherwise:

- `user_session` for the user's primary shared desktop.
- `display_99`, `99`, or legacy `:99` for a virtual display.
- Omit both only when auto-detection is acceptable.

The shared view is a dashboard coordination layer. For actual computer-use actions, continue using `take_screenshot` and `execute_cu_actions`; for archived stream frames, use `list_frames` and `read_frame`.
