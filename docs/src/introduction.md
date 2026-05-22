# Introduction

Intendant is an autonomous AI agent operating environment written in Rust. It gives AI agents a full desktop to work in — shell access, file editing, a graphical display they can see and control, voice interaction, and the ability to make phone calls — all wrapped in a layered human oversight system.

## Design Philosophy

Intendant is built around several core ideas:

**Security through process isolation.** Two separate binaries form a trust boundary. The runtime that executes arbitrary commands runs under Landlock filesystem restrictions and never holds API keys. The controller that manages model conversations never executes user-requested shell commands directly.

**Provider agnosticism.** OpenAI, Anthropic, and Gemini are all first-class providers with native tool calling, streaming, prompt caching, and computer use support. The system is not locked to any single vendor.

**Frontend parity.** The TUI, web dashboard, MCP server, and control socket all produce the same `UserAction` variants via a compile-time enum contract. Adding a capability to one forces handling in all — it is impossible for interfaces to diverge.

**Presence as a separate AI.** Rather than a chat wrapper, the presence layer is an independent model with its own conversation, tools, and state awareness. It mediates between the user and the working agent, translating intent into tasks and narrating progress back.

**Layered human oversight.** A three-layer autonomy system (global level, per-category rules, per-action approval) ensures the user maintains control at whatever granularity they prefer — from approving every command to fully autonomous operation.

## Architecture at a Glance

```
                          ┌──────────────────────────────────────────┐
                          │           intendant (controller)         │
                          │                                          │
  Web Dashboard ◄─────────┤  presence ─── agent loop ───┐           │
  TUI / MCP     ◄─────────┤     │            │          │           │
  Voice         ◄─────────┤     │      ┌─────┴──────┐   │           │
                          │     │      │ sub-agents  │   │           │
                          │     │      └────────────┘   │           │
                          └─────┼────────────────────────┼───────────┘
                                │                        │
                    ┌───────────┤                        │
                    │           │                        │
                    v           v                        v
              Voice APIs   Model APIs              intendant-runtime
           (Gemini Live,  (OpenAI/Anthropic/       (sandboxed command
            OAI Realtime)  Gemini + streaming)      execution, Landlock)
```

## Key Capabilities

- **Multi-provider LLM integration** with native tool calling, streaming, prompt caching, and computer use across OpenAI, Anthropic, and Gemini
- **WebRTC display pipeline** with hardware H264 encoding, multi-monitor support, bidirectional clipboard sync, and remote input injection
- **Computer use** via a provider-agnostic abstraction supporting X11, Wayland, macOS, and Windows backends
- **Live voice interaction** via Gemini Live and OpenAI Realtime, with a WASM-powered browser client
- **Phone calls** via SIP (pjsua) with structured data extraction from voice conversations
- **Multi-agent orchestration** with sub-agent spawning, git worktree isolation, and knowledge routing
- **Web dashboard** with activity log, token usage tracking, embedded terminal, WebRTC display viewers, session browser, and recording replay
- **MCP server and client** for integration with Claude Code and other MCP-compatible tools
- **Filesystem sandboxing** via Landlock (Linux) with configurable write paths
- **Session persistence** with structured JSONL logging and resume capability
- **Skills system** for named instruction sets with YAML frontmatter and autonomy overrides

## What's Next

Intendant is evolving toward a persistent daemon architecture:

- **Persistent presence** — voice model stays connected between tasks
- **Background agents** — monitoring, scheduled, and triggered tasks
- **Cross-session knowledge** — accumulated learning across conversations
