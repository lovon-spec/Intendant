# Introduction

Intendant is an autonomous AI agent operating environment written in Rust. It gives an AI agent a full desktop to work in — shell access, file editing, a graphical display it can see and control, voice interaction, and the ability to make phone calls — all wrapped in a layered human oversight system. Beyond running its own agent loop, Intendant also **supervises external coding agents** (Codex, Gemini CLI, Claude Code) as managed backends and **federates with peer machines** for multi-host display and task routing.

It runs on **macOS, Linux, and Windows**, is **provider-agnostic** (OpenAI, Anthropic, Gemini), and is designed so that every capability is reachable from any interface — CLI, TUI, web dashboard, MCP, or voice.

> **About this book.** These docs are verified against the source periodically, but Intendant moves fast and active areas — the dashboard, external-agent orchestration, federation — can drift ahead of the prose between verifications. **When the docs and the code disagree, the code is authoritative.** Every chapter cites real file and module paths so you can check; the explanations focus on the *shape and the why* of each subsystem, which changes more slowly than exact line numbers.

## Design Philosophy

Intendant is built around a few core ideas:

**Security through process isolation.** Two separate binaries form a trust boundary. The *runtime* that executes arbitrary commands runs under Landlock filesystem restrictions and never holds API keys. The *controller* that manages model conversations never executes user-requested shell commands directly. See [Architecture](./architecture.md).

**Provider agnosticism.** OpenAI, Anthropic, and Gemini are all first-class providers with native tool calling, streaming, prompt caching, and computer use. The system is not locked to any single vendor — and through [external-agent orchestration](./external-agent-orchestration.md) it can also drive whole third-party coding CLIs.

**A single-writer control plane.** Shared mutable state (autonomy level, the active agent backend, runtime config) has exactly one writer: the control plane. Frontends are *display-only* — they render state and emit intents, but never mutate state directly. See [Control Plane & Daemon](./control-plane-and-daemon.md).

**Compile-time frontend contracts.** Interfaces cannot silently diverge: the TUI and MCP server share the `UserAction`/`StateQuery` enums, while the web dashboard and control socket share the `ControlMsg` vocabulary — both enforced by Rust's exhaustive matching. See [Architecture](./architecture.md) and [TUI & Autonomy](./tui.md).

**Presence as a separate AI.** Rather than a chat wrapper, the presence layer is an independent (usually fast) model with its own conversation, tools, and state awareness. It mediates between the user and the working agent, turning intent into tasks and narrating progress back. See [Presence Layer](./presence.md).

**Layered human oversight.** A three-layer autonomy system — global level, per-category rules, and per-action approval — keeps the user in control at whatever granularity they prefer, from approving every command to fully autonomous operation. See [TUI & Autonomy](./tui.md).

## Architecture at a Glance

```
  ┌──────────────────────── intendant (controller) ─────────────────────────┐
  │                                                                          │
  │  Frontends ──intents──►  control plane (single writer) ──► EventBus      │
  │  (TUI · Web ·            session supervisor · task dispatch              │
  │   MCP · socket)               │                │                         │
  │      ▲                        │                │                         │
  │      │ render          ┌──────┴──────┐   ┌─────┴───────────────┐         │
  │   presence ◄───────────┤ native loop │   │ external-agent       │        │
  │   (mediator AI)        │ + sub-agents│   │ (Codex/Gemini/Claude)│        │
  │                        └──────┬──────┘   └─────┬───────────────┘         │
  └───────────────────────────────┼────────────────┼────────────────────────┘
              │                    │                │
              ▼                    ▼                ▼
        Voice / Model APIs   intendant-runtime   external CLI subprocess
        (live + streaming)   (sandboxed exec,    (wired to Intendant's
                              Landlock)            MCP server)

        ◄─── WebRTC display + peer federation ───►  browsers / peer daemons
```

See [Architecture](./architecture.md) for the full picture.

## Key Capabilities

- **Multi-provider LLM integration** — native tool calling, streaming, prompt caching, and computer use across OpenAI, Anthropic, and Gemini ([Runtime Protocol](./runtime-protocol.md), [Multi-Agent Orchestration](./multi-agent.md))
- **External-agent orchestration** — supervise Codex, Gemini CLI, or Claude Code as managed backends with steering, approvals, rollback, and cost accounting ([External-Agent Orchestration](./external-agent-orchestration.md))
- **WebRTC display pipeline** — a shared encoder pool (VP8 baseline + on-demand hardware H.264), tile-based dirty-region streaming, multi-monitor, and bidirectional clipboard ([Display Pipeline](./display-pipeline.md))
- **Peer federation** — Agent Cards, capability-based task routing, and cross-machine display sharing over mTLS ([Peer Federation](./peer-federation.md))
- **Computer use** — a provider-agnostic abstraction over X11, Wayland, macOS, and Windows backends ([Computer Use & Live Audio](./computer-use-and-audio.md))
- **Live voice & phone calls** — Gemini Live / OpenAI Realtime via a WASM browser client, and outbound SIP calls ([Presence Layer](./presence.md), [Computer Use & Live Audio](./computer-use-and-audio.md))
- **Persistent daemon** — long-lived session supervision, a multi-session dashboard, and content-addressed file snapshots with rewind ([Control Plane & Daemon](./control-plane-and-daemon.md), [Web Dashboard](./web-dashboard.md))
- **MCP server and client** — expose Intendant's control surface as MCP tools, and connect to external MCP servers ([MCP Server](./mcp-server.md))
- **Filesystem sandboxing** via Landlock (Linux), session persistence with structured JSONL logging and resume ([Session Logging](./session-logging.md)), and a skills system for named instruction sets

## Where to Go Next

- New here? Start with [Getting Started](./getting-started.md), then [Architecture](./architecture.md).
- Deploying or tuning? See [Configuration](./configuration.md) and [Windows Support](./windows-support.md).
- Building on a specific subsystem? Jump to its chapter via the sidebar.
