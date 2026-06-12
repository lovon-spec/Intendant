"""LongCLI-Bench (terminal-bench fork) managed lane: Intendant-supervised Codex.

Port of scripts/benchmarks/harbor_intendant_codex_agent.py (2026-06-11 host
revision) to the terminal-bench installed-agents API that LongCLI-Bench
vendors. The task command launches

    intendant --no-tls --bind <BIND> --web <PORT> --no-tui --no-presence \
        --agent codex --log-file /agent-logs/intendant --task-file <task>

inside the task container with prebuilt binaries uploaded from the host
(`codex_binary_path` = the codex-minimal-lineage fork, `intendant_binary_path`
= the bench intendant build), a per-project intendant.toml with
`managed_context = "managed"`, and ChatGPT-token auth handled exactly like the
vanilla lane (upload + refresh persist-back via CODEX_AUTH_JSON_PATH).

Density-first overhaul note: this lane runs at the FULL model window — no
`model_context_window` cap is written (unlike the constrained-window harbor
lanes this descends from).

Durable artifacts, by construction (no copy-out steps, timeout-safe):
  - $CODEX_HOME = /agent-logs/codex-home  -> codex rollouts (sessions/),
    incl. fission branch sessions, plus the live auth.json
  - --log-file /agent-logs/intendant      -> intendant session log dir:
    context_rewinds/ (rewind records + *-source-rollout.jsonl full-history
    archives) and fission_ledger.json live inside it (context_rewind.rs
    records_dir, fission_ledger.rs ledger_path)
/agent-logs is the trial's host agent-logs directory (bind mount).

Task completion: polls codex rollouts for a `task_complete` event in a
*parent* rollout — files carrying a `<fission_charter>` developer message are
fission branches and are excluded (deviation from the harbor agent's blanket
grep, which a completed branch could false-trigger).

Recall probes (scripts/benchmarks/probes/): when `probes_dir` is set and
contains `<task-id>.json`, the agent keeps intendant alive after
task_complete, binds the gateway on 0.0.0.0 (reachable from the docker host
via the container IP), and shells out to inject_probes.py to drive
post-completion follow-up turns through the gateway WebSocket
(ControlMsg::FollowUp). Probe answers land in /agent-logs/probe_answers.json.

Usage (from the LongCLI checkout, with this directory on PYTHONPATH):

    CODEX_AUTH_JSON_PATH=/path/to/auth.json \
    tb run --dataset-path tasks_long_cli \
        --agent-import-path tb_intendant_codex_agent:IntendantCodex \
        --model gpt-5.5 \
        --agent-kwarg codex_binary_path=/path/to/codex-fork \
        --agent-kwarg intendant_binary_path=/path/to/intendant \
        --task-id <task> --n-concurrent 1 ...
"""

import json
import os
import shlex
import subprocess
import sys
import tempfile
from pathlib import Path

from terminal_bench.agents.installed_agents.abstract_installed_agent import (
    AbstractInstalledAgent,
)
from terminal_bench.terminal.models import TerminalCommand
from terminal_bench.terminal.tmux_session import TmuxSession
from terminal_bench.utils.logger import logger

AGENT_LOGS = "/agent-logs"
CODEX_HOME = f"{AGENT_LOGS}/codex-home"
AUTH_JSON = f"{CODEX_HOME}/auth.json"
INTENDANT_LOG_DIR = f"{AGENT_LOGS}/intendant"
TASK_FILE = f"{AGENT_LOGS}/task.txt"
CONSOLE_LOG = f"{AGENT_LOGS}/intendant-console.log"
PID_FILE = f"{AGENT_LOGS}/intendant.pid"


class IntendantCodex(AbstractInstalledAgent):
    """Terminal-bench agent that solves tasks through Intendant-managed Codex."""

    @staticmethod
    def name() -> str:
        return "intendant-codex"

    def __init__(
        self,
        *args,
        codex_binary_path: str,
        intendant_binary_path: str,
        model_name: str | None = None,
        reasoning_effort: str = "xhigh",
        web_port: int = 8901,
        command_timeout_sec: float = 7000.0,
        probes_dir: str | None = None,
        **kwargs,
    ):
        super().__init__(**kwargs)
        if not model_name:
            raise ValueError("Model name is required (tb run --model ...)")
        self._model = model_name.split("/")[-1]
        self._codex_binary_path = Path(codex_binary_path).expanduser()
        self._intendant_binary_path = Path(intendant_binary_path).expanduser()
        for path in (self._codex_binary_path, self._intendant_binary_path):
            if not path.is_file():
                raise ValueError(f"binary path does not exist: {path}")
        self._reasoning_effort = str(reasoning_effort).strip()
        self._web_port = int(web_port)
        # Budget for the in-container launch+poll command. Bounded (instead of
        # the stock agents' unbounded block) so the auth persist-back and
        # intendant shutdown below still run on timeout; keep under the
        # task's max_agent_timeout_sec (LongCLI: 7200s).
        self._command_timeout_sec = float(command_timeout_sec)
        self._probes_dir = Path(probes_dir).expanduser() if probes_dir else None
        if self._probes_dir is not None and not self._probes_dir.is_dir():
            raise ValueError(f"probes_dir does not exist: {self._probes_dir}")
        self._auth_json_path = self._resolve_auth_json_path()
        self._probes_enabled = False
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

    def get_version_command(self) -> str | None:
        return "codex --version && intendant --help | head -1"

    @property
    def _env(self) -> dict[str, str]:
        env = {
            "CODEX_HOME": CODEX_HOME,
            "NO_COLOR": "1",
            "TERM": "dumb",
        }
        if self._auth_json_path is None and os.environ.get("OPENAI_API_KEY"):
            env["OPENAI_API_KEY"] = os.environ["OPENAI_API_KEY"]
        return env

    @property
    def _install_agent_script_path(self) -> Path:
        return self._get_templated_script_path("tb-intendant-setup.sh.j2")

    # -- container provisioning ----------------------------------------------

    def _upload_binaries(self, session: TmuxSession) -> None:
        session.copy_to_container(
            self._codex_binary_path,
            container_dir="/installed-agent",
            container_filename="codex-upload",
        )
        session.copy_to_container(
            self._intendant_binary_path,
            container_dir="/installed-agent",
            container_filename="intendant-upload",
        )
        result = session.container.exec_run(
            [
                "sh",
                "-c",
                # rm first: the c-env base image ships /usr/local/bin/codex as
                # a symlink to an npm-global codex 0.98.0 — replace it.
                "rm -f /usr/local/bin/codex /usr/local/bin/intendant && "
                "install -m 0755 /installed-agent/codex-upload /usr/local/bin/codex && "
                "install -m 0755 /installed-agent/intendant-upload /usr/local/bin/intendant && "
                "codex --version && intendant --help | head -1",
            ]
        )
        if result.exit_code != 0:
            raise RuntimeError(
                "Binary upload failed: "
                f"{result.output.decode(errors='replace')[-2000:]}"
            )

    def _intendant_toml(self) -> str:
        # Keys verified against project.rs `CodexConfig`. `managed_context`
        # defaults to "vanilla", so it MUST be pinned here or this lane
        # silently loses the managed protocol. Managed sessions spawn
        # `managed_command` (effective_command()); `command` stays pinned as
        # the fallback path. Both point at the uploaded fork binary.
        # Full-window lane: no model_context_window is written anywhere.
        lines = [
            "[agent]",
            'default_backend = "codex"',
            "",
            "[agent.codex]",
            'command = "/usr/local/bin/codex"',
            'managed_command = "/usr/local/bin/codex"',
            'managed_context = "managed"',
            'context_archive = "exact"',
            f"model = {json.dumps(self._model)}",
            'approval_policy = "never"',
            'sandbox = "danger-full-access"',
            "network_access = true",
            "web_search = false",
        ]
        if self._reasoning_effort:
            lines.append(f"reasoning_effort = {json.dumps(self._reasoning_effort)}")
        return "\n".join(lines) + "\n"

    def _place_intendant_toml(self, session: TmuxSession) -> None:
        with tempfile.NamedTemporaryFile(
            mode="w", suffix=".toml", delete=False
        ) as handle:
            handle.write(self._intendant_toml())
            tmp = Path(handle.name)
        try:
            # /app is the task workdir for every LongCLI image; intendant
            # reads intendant.toml from its launch cwd (project config).
            session.copy_to_container(
                tmp, container_dir="/app", container_filename="intendant.toml"
            )
        finally:
            tmp.unlink(missing_ok=True)

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
        assert self._auth_json_path is not None
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
            json.loads(payload)
        except (OSError, ValueError) as exc:
            self._logger.warning("Refreshed auth.json unreadable/invalid: %s", exc)
            return
        tmp = self._auth_json_path.with_suffix(".tmp")
        tmp.write_bytes(payload)
        tmp.chmod(0o600)
        os.replace(tmp, self._auth_json_path)
        self._logger.debug("Persisted refreshed auth.json to %s", self._auth_json_path)

    # -- probes ----------------------------------------------------------------

    def _probes_file_for(self, logging_dir: Path | None) -> Path | None:
        """Match a probes JSON to this trial by task-id path component.

        Trial agent-logs paths look like .../<run-id>/<task-id>/<trial>/agent-logs;
        probe files are <probes_dir>/<task-id>.json (authored post-pilot).
        """
        if self._probes_dir is None or logging_dir is None:
            return None
        for part in reversed(logging_dir.resolve().parts):
            candidate = self._probes_dir / f"{part.split('.')[0]}.json"
            if candidate.is_file():
                return candidate
        return None

    def _container_ip(self, session: TmuxSession) -> str | None:
        session.container.reload()
        networks = (
            session.container.attrs.get("NetworkSettings", {}).get("Networks", {})
        )
        for net in networks.values():
            ip = net.get("IPAddress")
            if ip:
                return ip
        return None

    def _run_probes(
        self, session: TmuxSession, probes_file: Path, logging_dir: Path
    ) -> None:
        """Drive post-completion follow-up probes through the live gateway."""
        ip = self._container_ip(session)
        if ip is None:
            self._logger.warning("No container IP; skipping probes")
            return
        # Repo layout: ../probes/inject_probes.py; host deploys copy the
        # probes scripts flat next to this file (see RUN-COMMANDS.md).
        here = Path(__file__).resolve().parent
        injector = here / "inject_probes.py"
        if not injector.is_file():
            injector = here.parent / "probes" / "inject_probes.py"
        if not injector.is_file():
            self._logger.warning("inject_probes.py not found; skipping probes")
            return
        cmd = [
            sys.executable,
            str(injector),
            "managed",
            "--gateway",
            f"ws://{ip}:{self._web_port}/ws",
            "--codex-home",
            str(logging_dir / "codex-home"),
            "--console-log",
            str(logging_dir / "intendant-console.log"),
            "--probes",
            str(probes_file),
            "--out",
            str(logging_dir / "probe_answers.json"),
        ]
        self._logger.info("Injecting probes: %s", shlex.join(cmd))
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=1800)
        if result.returncode != 0:
            self._logger.warning(
                "Probe injection failed (%s): %s", result.returncode, result.stderr[-2000:]
            )

    # -- run -------------------------------------------------------------------

    def _launch_command(self, bind_addr: str) -> str:
        poll_sec = max(300, int(self._command_timeout_sec) - 120)
        return "\n".join(
            [
                "set -uo pipefail",
                f"mkdir -p {CODEX_HOME} {INTENDANT_LOG_DIR}",
                f": > {CONSOLE_LOG}",
                "cd /app",
                "/usr/local/bin/intendant "
                f"--no-tls --bind {bind_addr} --web {self._web_port} "
                "--no-tui --no-presence "
                "--agent codex "
                f"--log-file {INTENDANT_LOG_DIR} "
                f"--task-file {TASK_FILE} "
                f"> {CONSOLE_LOG} 2>&1 </dev/null &",
                "intendant_pid=$!",
                f'echo "$intendant_pid" > {PID_FILE}',
                "completed=0",
                f"for _ in $(seq 1 {poll_sec}); do",
                # task_complete in a PARENT rollout only: fission branch
                # rollouts always carry the <fission_charter> developer
                # message — a completed branch must not end the task.
                f'  for f in $(grep -Rl \'"type":"task_complete"\' "{CODEX_HOME}/sessions" 2>/dev/null); do',
                "    if ! grep -q '<fission_charter>' \"$f\"; then",
                "      completed=1",
                "      break",
                "    fi",
                "  done",
                '  [ "$completed" = 1 ] && break',
                '  if ! kill -0 "$intendant_pid" 2>/dev/null; then',
                '    echo "intendant exited before task_complete" >&2',
                "    break",
                "  fi",
                "  sleep 1",
                "done",
                'if [ "$completed" = 1 ]; then',
                '  echo "task_complete observed (parent rollout)"',
                "  exit 0",
                "fi",
                'if kill -0 "$intendant_pid" 2>/dev/null; then',
                '  echo "Timed out waiting for Codex task_complete" >&2',
                "  exit 124",
                "fi",
                "exit 1",
            ]
        )

    def _shutdown_intendant(self, session: TmuxSession) -> None:
        session.container.exec_run(
            [
                "sh",
                "-c",
                f'if [ -f {PID_FILE} ]; then pid=$(cat {PID_FILE}); '
                'kill "$pid" 2>/dev/null; sleep 1; '
                'kill -9 "$pid" 2>/dev/null; fi; true',
            ]
        )

    def _run_agent_commands(self, instruction: str) -> list[TerminalCommand]:
        # The instruction itself travels via --task-file (written host-side
        # into the agent-logs mount by perform_task), so no shell-quoting of
        # model-facing text happens here.
        del instruction
        bind_addr = "0.0.0.0" if self._probes_enabled else "127.0.0.1"
        script = self._launch_command(bind_addr)
        return [
            TerminalCommand(
                command=f"bash -c {shlex.quote(script)}",
                min_timeout_sec=0.0,
                max_timeout_sec=self._command_timeout_sec,
                block=True,
                append_enter=True,
            )
        ]

    def perform_task(
        self,
        instruction: str,
        session: TmuxSession,
        logging_dir: Path | None = None,
    ):
        probes_file = self._probes_file_for(logging_dir)
        self._probes_enabled = probes_file is not None

        if logging_dir is not None:
            # /agent-logs is this directory inside the container: the task
            # file is in place before the launch command runs.
            (logging_dir / "task.txt").write_text(
                self._render_instruction(instruction)
            )
        else:
            raise ValueError("IntendantCodex requires a logging_dir (agent-logs mount)")

        if self._auth_json_path is not None:
            self._place_auth_json(session)
        self._upload_binaries(session)
        self._place_intendant_toml(session)

        try:
            result = super().perform_task(
                instruction, session, logging_dir=logging_dir
            )
            if probes_file is not None:
                try:
                    self._run_probes(session, probes_file, logging_dir)
                except Exception as exc:  # noqa: BLE001 - probes must not fail the trial
                    self._logger.warning("Probe run failed: %s", exc)
            return result
        finally:
            try:
                self._shutdown_intendant(session)
            except Exception as exc:  # noqa: BLE001
                self._logger.warning("intendant shutdown failed: %s", exc)
            if self._auth_json_path is not None:
                try:
                    self._persist_auth_json(session, logging_dir)
                except Exception as exc:  # noqa: BLE001 - never mask the run result
                    self._logger.warning("Auth persist-back failed: %s", exc)
