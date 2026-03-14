# Configuration

## CLI Flags

| Flag | Description |
|------|-------------|
| `--provider <name>` | Force provider (`openai`, `anthropic`, or `gemini`) |
| `--model <name>` | Override model name |
| `--verbose` / `-v` | Show debug-level log entries in TUI |
| `--no-tui` | Disable TUI, use plain text output |
| `--autonomy <level>` | Set autonomy level (`low`, `medium`, `high`, `full`) |
| `--log-file <dir>` | Override session log directory |
| `--mcp` | Run as MCP server on stdio (replaces TUI) |
| `--control-socket` | Enable Unix control socket at `/tmp/intendant-<pid>.sock` |
| `--json` | JSONL structured output to stdout (implies `--no-tui`) |
| `--sandbox` | Enable Landlock filesystem sandboxing (Linux kernel 5.13+) |
| `--direct` | Force single-agent direct mode (skip orchestrator even for complex tasks) |
| `--no-presence` | Disable the presence layer (direct agent interaction) |
| `--continue` / `-c` | Resume most recent session for this project |
| `--resume <id>` / `-r <id>` | Resume specific session by ID or prefix |
| `--web [PORT]` | Start web gateway for remote TUI + optional voice/text interaction (default port 8765) |

The TUI launches only when both stdin and stdout are terminals. When piping input/output or in sub-agent mode, `intendant` falls back to headless mode.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `OPENAI_API_KEY` / `OPENAI` | — | OpenAI API key |
| `ANTHROPIC_API_KEY` / `ANTHROPIC` | — | Anthropic API key |
| `GEMINI_API_KEY` | — | Google AI (Gemini) API key |
| `PROVIDER` | auto-detect | `"openai"`, `"anthropic"`, or `"gemini"` (used when multiple keys are set) |
| `MODEL_NAME` | per-provider default | Model to use (e.g. `gpt-5.2-codex`, `claude-sonnet-4-5-20250929`, `gemini-2.5-pro`) |
| `USE_NATIVE_TOOLS` | `true` | Enable native API tool calling; `false` falls back to text-based JSON extraction |
| `MODEL_CONTEXT_WINDOW` | per-model default | Context window size in tokens |
| `MAX_OUTPUT_TOKENS` | per-model default | Max output tokens per API call (sent to API) |
| `STRUCTURED_OUTPUT` | `true` for gpt-5+/o3/o4 | Enable JSON object mode for deterministic parsing |
| `REASONING_EFFORT` | — | Reasoning effort for GPT-5/o3/o4 models (`low`, `medium`, `high`) |
| `REASONING_SUMMARY` | — | Reasoning summary mode (`auto`, `concise`, `detailed`) |
| `PRESENCE_PROVIDER` | — | Override provider for the presence layer (fallback: `PROVIDER`) |
| `PRESENCE_MODEL` | — | Override model for the presence layer |
| `INTENDANT_LOG_DIR` | auto | Session log directory (set automatically by caller for the runtime) |

### Sub-Agent Environment Variables

These are set automatically when spawning sub-agents (see [Multi-Agent Orchestration](./multi-agent.md)):

| Variable | Description |
|----------|-------------|
| `INTENDANT_ROLE` | Sub-agent role (`orchestrator`, `research`, `implementation`, `testing`) |
| `INTENDANT_ID` | Unique sub-agent identifier |
| `INTENDANT_TASK` | Task description for the sub-agent |
| `INTENDANT_RESULT_FILE` | Path for sub-agent to write final results |
| `INTENDANT_PROGRESS_FILE` | Path for sub-agent to write periodic progress |
| `INTENDANT_PARENT_KNOWLEDGE` | Path to parent's knowledge store for inheritance |
| `INTENDANT_INHERIT_MEMORY` | `1` to inherit project memory |
| `INTENDANT_SANDBOX_WRITE_PATHS` | Landlock write paths (set by caller when sandboxing) |
| `INTENDANT_MCP_RELOAD` | `1` when process was exec'd for MCP hot-reload |

The agent runner hard timeout is 120s default, automatically extended to 600s when `askHuman` is present in the command batch.

## Project Configuration

Create `intendant.toml` in the project root:

```toml
[memory]
enabled = true  # default: true

[model]
context_window = 200000       # override per-model default
max_output_tokens = 8192      # override per-model default

[orchestrator]
max_parallel_agents = 4       # max concurrent sub-agents
sub_agent_dir = ".intendant/subagents"  # where sub-agent workspaces are created

[approval]
file_read = "auto"            # auto-approve file reads
file_write = "ask"            # ask before file writes (default)
file_delete = "ask"           # ask before file deletes (default)
command_exec = "auto"         # auto-approve command execution
network = "auto"              # auto-approve network requests
destructive = "ask"           # ask before destructive commands (default)

[presence]
enabled = true                # enable the conversational presence layer (default: true)
provider = "gemini"           # provider for the presence model (optional, falls back to PROVIDER)
model = "gemini-2.5-flash"    # model for the presence layer (optional)
audio_model = "gemini-2.5-flash-live"  # model for browser-side live presence (optional)
context_window = 32768        # context window for the presence conversation (default: 32768)

[sandbox]
enabled = false               # enable Landlock filesystem sandboxing (default: false)
extra_write_paths = ["/var/log"]  # additional writable paths beyond project root, /tmp, log dir

# External MCP servers to connect to as a client
[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[[mcp_servers]]
name = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[mcp_servers.env]
GITHUB_TOKEN = "ghp_..."
```

When sandboxing is enabled (via `--sandbox` or `[sandbox].enabled = true`), runtime command execution is restricted to read-only filesystem access plus writes to project root, `/tmp`, session log directory, `~/.intendant`, and `extra_write_paths`. On kernels without Landlock support, sandboxing is silently skipped.

## INTENDANT.md Project Instructions

Place an `INTENDANT.md` file in your project root or at `~/.config/intendant/INTENDANT.md` for global instructions. These are injected into the conversation at session start, before knowledge/memory. Both files are loaded if present (global first, project-local second).

## System Prompts

System prompts are compiled into the binary at build time, so `intendant` works from any directory without needing the source tree. Two base prompt variants exist:

- **`SysPrompt.md`** — Full prompt with JSON schema and per-function documentation (used with text-based JSON extraction)
- **`SysPrompt_tools.md`** — Condensed prompt for native tool calling mode (function docs live in API tool definitions, reducing system prompt tokens)

The active variant is selected automatically based on whether the provider has native tool calling enabled.

Prompts are resolved using a 3-layer cascade (highest priority first):

1. **Project root** — `<git-root>/SysPrompt.md` or `SysPrompt_tools.md` (per-project customization)
2. **Global config** — `~/.config/intendant/SysPrompt.md` or `SysPrompt_tools.md` (user-wide customization)
3. **Compiled-in default** — always available, zero-config

Role-specific prompts (`SysPrompt_orchestrator.md`, `SysPrompt_research.md`, `SysPrompt_implementation.md`) follow the same cascade and are appended to the base prompt. The presence layer uses its own standalone prompt (`SysPrompt_presence.md`).

To customize prompts for a specific project, place your modified `.md` files in the project's git root. For user-wide customization, place them in `~/.config/intendant/`.
