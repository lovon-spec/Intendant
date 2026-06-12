"""LongCLI-Bench (terminal-bench fork) vanilla lane: stock Codex + auth persistence.

Port of scripts/benchmarks/harbor_persistent_codex_agent.py to the
terminal-bench installed-agents API that LongCLI-Bench vendors (the `tb` CLI;
see fetch-longcli.sh). The run path stays as close to LongCLI's stock
`codex` agent as possible — same npm install flow, same
`codex exec --sandbox danger-full-access ...` invocation — with two deltas:

1. **ChatGPT-token auth with refresh persistence.** The stock tb agent only
   supports `OPENAI_API_KEY` written into a throwaway auth.json. With
   ChatGPT-token auth, Codex may rotate the refresh token mid-task; dropping
   the refreshed file can make later tasks reuse an already-consumed refresh
   token (the May-2026 harbor lanes hit exactly this). When
   `CODEX_AUTH_JSON_PATH` is set in the host environment, this agent uploads
   that auth.json into the task container before the run and persists the
   (possibly refreshed) copy back to the same host path afterwards. Run with
   `--n-concurrent 1`: concurrent trials sharing one auth file would race the
   refresh chain.

2. **Durable trajectory archive.** `CODEX_HOME` is pointed at
   `/agent-logs/codex-home`, which terminal-bench bind-mounts to the trial's
   host `agent-logs/` directory. Codex rollouts (`sessions/`) and the live
   auth.json therefore land on the host *as they are written* — they survive
   agent timeouts and container teardown with no copy-out step. The archived
   `sessions/` directory is the ground-truth input for the recall-probe
   tooling (scripts/benchmarks/probes/).

The npm Codex version is pinned to 0.133.0 by default (`--agent-kwarg
version=...` to override) — the same wire-protocol revision the managed lane's
fork is based on.

Usage (from the LongCLI checkout, with this directory on PYTHONPATH):

    CODEX_AUTH_JSON_PATH=/path/to/auth.json \
    tb run --dataset-path tasks_long_cli \
        --agent-import-path tb_persistent_codex_agent:PersistentAuthCodex \
        --model gpt-5.5 --task-id <task> --n-concurrent 1 ...
"""

import json
import os
from pathlib import Path

from terminal_bench.agents.installed_agents.codex.codex_agent import CodexAgent
from terminal_bench.terminal.models import TerminalCommand
from terminal_bench.terminal.tmux_session import TmuxSession
from terminal_bench.utils.logger import logger

# Container paths. /agent-logs is bind-mounted to the trial's host
# agent-logs dir by terminal-bench's docker-compose template.
AGENT_LOGS = "/agent-logs"
CODEX_HOME = f"{AGENT_LOGS}/codex-home"
AUTH_JSON = f"{CODEX_HOME}/auth.json"

DEFAULT_CODEX_VERSION = "0.133.0"


class PersistentAuthCodex(CodexAgent):
    """Stock-flow Codex agent with ChatGPT auth.json upload + refresh persistence."""

    @staticmethod
    def name() -> str:
        return "persistent-auth-codex"

    def __init__(self, *args, command_timeout_sec: float | None = None, **kwargs):
        super().__init__(*args, **kwargs)
        # Stock CodexAgent defaults version to "latest"; pin unless overridden.
        if kwargs.get("version") is None:
            self._version = DEFAULT_CODEX_VERSION
        # Bound the in-container codex command so the post-run auth
        # persistence in perform_task() still executes on slow tasks. The
        # stock agent blocks unboundedly and relies on the harness-level
        # asyncio timeout, which abandons the worker thread and skips any
        # cleanup. Keep this comfortably below the task's
        # max_agent_timeout_sec (LongCLI tasks: 7200s).
        self._command_timeout_sec = (
            float(command_timeout_sec) if command_timeout_sec is not None else None
        )
        self._auth_json_path = self._resolve_auth_json_path()
        self._logger = logger.getChild(__name__)

    @staticmethod
    def _resolve_auth_json_path() -> Path | None:
        raw = os.environ.get("CODEX_AUTH_JSON_PATH")
        if not raw:
            return None
        path = Path(raw).expanduser()
        if not path.is_file():
            raise ValueError(f"CODEX_AUTH_JSON_PATH does not exist: {path}")
        return path

    @property
    def _env(self) -> dict[str, str]:
        env = {} if self._auth_json_path else dict(super()._env)
        env["CODEX_HOME"] = CODEX_HOME
        env["NO_COLOR"] = "1"
        return env

    @property
    def _install_agent_script_path(self) -> Path:
        # Own template: identical npm flow to the stock agent, but auth.json
        # is only synthesized from OPENAI_API_KEY when no real auth.json is
        # uploaded, and it is written into $CODEX_HOME (the /agent-logs
        # mount), not $HOME/.codex.
        return self._get_templated_script_path("tb-codex-setup.sh.j2")

    def _run_agent_commands(self, instruction: str) -> list[TerminalCommand]:
        commands = super()._run_agent_commands(instruction)
        if self._command_timeout_sec is not None:
            commands = [
                command.model_copy(update={"max_timeout_sec": self._command_timeout_sec})
                for command in commands
            ]
        return commands

    # -- auth upload / persist-back -----------------------------------------

    def _place_auth_json(self, session: TmuxSession) -> None:
        assert self._auth_json_path is not None
        session.container.exec_run(["mkdir", "-p", CODEX_HOME])
        session.copy_to_container(
            self._auth_json_path,
            container_dir=CODEX_HOME,
            container_filename="auth.json",
        )
        session.container.exec_run(["chmod", "600", AUTH_JSON])

    def _persist_auth_json(self, session: TmuxSession, logging_dir: Path | None) -> None:
        """Copy the (possibly refreshed) container auth.json back to the host path."""
        assert self._auth_json_path is not None
        # Codex rewrites auth.json as container-root with mode 0600; the
        # /agent-logs bind mount preserves that on the host, so chown it to
        # the harness uid before reading it host-side.
        result = session.container.exec_run(
            ["chown", f"{os.getuid()}:{os.getgid()}", AUTH_JSON]
        )
        if result.exit_code != 0:
            self._logger.warning(
                "Could not chown refreshed auth.json (exit %s); auth may not persist",
                result.exit_code,
            )
            return
        if logging_dir is None:
            return
        host_auth = logging_dir / "codex-home" / "auth.json"
        if not host_auth.is_file():
            self._logger.warning("No refreshed auth.json at %s", host_auth)
            return
        try:
            payload = host_auth.read_bytes()
            json.loads(payload)  # refuse to clobber the host copy with garbage
        except (OSError, ValueError) as exc:
            self._logger.warning("Refreshed auth.json unreadable/invalid: %s", exc)
            return
        tmp = self._auth_json_path.with_suffix(".tmp")
        tmp.write_bytes(payload)
        tmp.chmod(0o600)
        os.replace(tmp, self._auth_json_path)
        self._logger.debug("Persisted refreshed auth.json to %s", self._auth_json_path)

    # -- main entry ----------------------------------------------------------

    def perform_task(
        self,
        instruction: str,
        session: TmuxSession,
        logging_dir: Path | None = None,
    ):
        if self._auth_json_path is not None:
            self._place_auth_json(session)
        try:
            return super().perform_task(instruction, session, logging_dir=logging_dir)
        finally:
            # CODEX_HOME lives on the /agent-logs mount, so sessions/ are
            # already archived host-side; only the auth file needs syncing.
            if self._auth_json_path is not None:
                try:
                    self._persist_auth_json(session, logging_dir)
                except Exception as exc:  # noqa: BLE001 - never mask the run result
                    self._logger.warning("Auth persist-back failed: %s", exc)
