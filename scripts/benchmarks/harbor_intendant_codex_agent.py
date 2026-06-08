import json
import shlex
from pathlib import Path

from harbor.agents.installed.base import with_prompt_template
from harbor.agents.installed.codex import Codex
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.models.trial.paths import EnvironmentPaths


class IntendantCodex(Codex):
    """Harbor agent that solves Terminal-Bench through Intendant-managed Codex.

    This intentionally differs from Harbor's stock Codex agent: the task command
    launches `intendant --agent codex --web ...`, and Intendant then launches the
    prebuilt Codex app-server with its MCP integration enabled.
    """

    @staticmethod
    def name() -> str:
        return "intendant-codex"

    def __init__(
        self,
        *args,
        binary_path: str,
        intendant_path: str,
        reasoning_effort: str = "xhigh",
        web_port: int = 8765,
        **kwargs,
    ):
        super().__init__(*args, **kwargs)
        self._binary_path = Path(binary_path).expanduser()
        self._intendant_path = Path(intendant_path).expanduser()
        self._intendant_reasoning_effort = reasoning_effort
        self._web_port = int(web_port)
        if not self._binary_path.is_file():
            raise ValueError(f"binary_path does not exist: {self._binary_path}")
        if not self._intendant_path.is_file():
            raise ValueError(f"intendant_path does not exist: {self._intendant_path}")

    def get_version_command(self) -> str | None:
        return "codex --version && intendant --help | head -1"

    async def install(self, environment: BaseEnvironment) -> None:
        await self.exec_as_root(
            environment,
            command=(
                "if ldd --version 2>&1 | grep -qi musl || [ -f /etc/alpine-release ]; then"
                "  echo 'IntendantCodex requires a glibc Linux task image' >&2; exit 1;"
                " elif command -v apt-get &>/dev/null; then"
                "  apt-get update &&"
                "  apt-get install -y --no-install-recommends "
                "curl ripgrep ca-certificates libzstd1 zlib1g "
                "libpipewire-0.3-0 libxcb1 libxcb-shm0 libxcb-randr0 &&"
                "  (apt-get install -y --no-install-recommends libssl3 ||"
                "   apt-get install -y --no-install-recommends libssl3t64) &&"
                "  (apt-get install -y --no-install-recommends libvpx7 ||"
                "   (curl -fsSL -o /tmp/libvpx7.deb "
                "http://archive.ubuntu.com/ubuntu/pool/main/libv/libvpx/"
                "libvpx7_1.11.0-2ubuntu2.5_amd64.deb &&"
                "    apt-get install -y --no-install-recommends /tmp/libvpx7.deb));"
                " elif command -v yum &>/dev/null; then"
                "  yum install -y curl ripgrep ca-certificates;"
                " else"
                "  echo 'IntendantCodex requires apt-get or a compatible glibc image' >&2; exit 1;"
                " fi"
            ),
            env={"DEBIAN_FRONTEND": "noninteractive"},
        )

        remote_codex = "/tmp/patched-codex"
        remote_intendant = "/tmp/intendant"
        await environment.upload_file(self._binary_path, remote_codex)
        await environment.upload_file(self._intendant_path, remote_intendant)
        await self.exec_as_root(
            environment,
            command=(
                f"install -m 0755 {remote_codex} /usr/local/bin/codex && "
                f"install -m 0755 {remote_intendant} /usr/local/bin/intendant && "
                "codex --version && intendant --help | head -1"
            ),
        )

    def _intendant_toml(self, model: str) -> str:
        lines = [
            "[agent]",
            'default_backend = "codex"',
            "",
            "[agent.codex]",
            'command = "/usr/local/bin/codex"',
            f"model = {json.dumps(model)}",
            'approval_policy = "never"',
            'sandbox = "danger-full-access"',
            "network_access = true",
            "web_search = false",
        ]
        if self._intendant_reasoning_effort.strip():
            lines.append(
                "reasoning_effort = "
                + json.dumps(self._intendant_reasoning_effort.strip())
            )
        return "\n".join(lines) + "\n"

    @staticmethod
    def _quote_heredoc(text: str) -> str:
        return "cat > intendant.toml <<'TOML'\n" + text + "TOML\n"

    @with_prompt_template
    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        if not self.model_name:
            raise ValueError("Model name is required")

        model = self.model_name.split("/")[-1]
        auth_json_path = self._resolve_auth_json_path()

        remote_codex_home = self._REMOTE_CODEX_HOME.as_posix()
        remote_secrets_dir = self._REMOTE_CODEX_SECRETS_DIR.as_posix()
        remote_auth_path = (self._REMOTE_CODEX_SECRETS_DIR / "auth.json").as_posix()
        agent_dir = EnvironmentPaths.agent_dir.as_posix()
        intendant_log_dir = (EnvironmentPaths.agent_dir / "intendant").as_posix()

        env: dict[str, str] = {
            "CODEX_HOME": remote_codex_home,
            "NO_COLOR": "1",
            "TERM": "dumb",
        }

        await self.exec_as_agent(
            environment,
            command=(
                f'mkdir -p "$CODEX_HOME" {shlex.quote(remote_secrets_dir)} '
                f"{shlex.quote(agent_dir)} {shlex.quote(intendant_log_dir)}"
            ),
            env=env,
        )

        if auth_json_path:
            self.logger.debug("Codex auth: using auth.json from %s", auth_json_path)
            await environment.upload_file(auth_json_path, remote_auth_path)
            if environment.default_user is not None:
                await self.exec_as_root(
                    environment,
                    command=f"chown {environment.default_user} {remote_auth_path}",
                )
            setup_command = (
                f'ln -sf {shlex.quote(remote_auth_path)} "$CODEX_HOME/auth.json"\n'
            )
        else:
            self.logger.debug("Codex auth: using OPENAI_API_KEY")
            env["OPENAI_API_KEY"] = self._get_env("OPENAI_API_KEY") or ""
            setup_command = (
                f"cat >{shlex.quote(remote_auth_path)} <<EOF\n"
                '{\n  "OPENAI_API_KEY": "${OPENAI_API_KEY}"\n}\nEOF\n'
                f"ln -sf {shlex.quote(remote_auth_path)} "
                '"$CODEX_HOME/auth.json"\n'
            )

        if openai_base_url := self._get_env("OPENAI_BASE_URL"):
            env["OPENAI_BASE_URL"] = openai_base_url
            setup_command += (
                '\ncat >>"$CODEX_HOME/config.toml" <<TOML\n'
                'openai_base_url = "${OPENAI_BASE_URL}"\n'
                "TOML\n"
            )

        skills_command = self._build_register_skills_command()
        if skills_command:
            setup_command += f"\n{skills_command}"

        mcp_command = self._build_register_mcp_servers_command()
        if mcp_command:
            setup_command += f"\n{mcp_command}"

        setup_command += "\n" + self._quote_heredoc(self._intendant_toml(model))

        await self.exec_as_agent(environment, command=setup_command, env=env)

        output_path = (EnvironmentPaths.agent_dir / self._OUTPUT_FILENAME).as_posix()
        task_path = (EnvironmentPaths.agent_dir / "intendant-task.txt").as_posix()
        escaped_instruction = shlex.quote(instruction)
        try:
            await self.exec_as_agent(
                environment,
                command=(
                    "set -euo pipefail\n"
                    f"printf '%s' {escaped_instruction} > {shlex.quote(task_path)}\n"
                    f": > {shlex.quote(output_path)}\n"
                    "/usr/local/bin/intendant "
                    f"--web {self._web_port} "
                    "--no-tui "
                    "--no-presence "
                    "--agent codex "
                    f"--log-file {shlex.quote(intendant_log_dir)} "
                    f"--task-file {shlex.quote(task_path)} "
                    f"> {shlex.quote(output_path)} 2>&1 </dev/null &\n"
                    "intendant_pid=$!\n"
                    f"tail -n +1 -F {shlex.quote(output_path)} &\n"
                    "tail_pid=$!\n"
                    "completed=0\n"
                    "for _ in $(seq 1 3600); do\n"
                    '  if grep -R \'"type":"task_complete"\' "$CODEX_HOME/sessions" >/dev/null 2>&1; then\n'
                    "    completed=1\n"
                    "    break\n"
                    "  fi\n"
                    '  if ! kill -0 "$intendant_pid" 2>/dev/null; then\n'
                    "    break\n"
                    "  fi\n"
                    "  sleep 1\n"
                    "done\n"
                    'kill "$tail_pid" 2>/dev/null || true\n'
                    'wait "$tail_pid" 2>/dev/null || true\n'
                    'if [ "$completed" = 1 ]; then\n'
                    '  kill "$intendant_pid" 2>/dev/null || true\n'
                    "  sleep 1\n"
                    '  kill -9 "$intendant_pid" 2>/dev/null || true\n'
                    '  wait "$intendant_pid" 2>/dev/null || true\n'
                    "  exit 0\n"
                    "fi\n"
                    'if kill -0 "$intendant_pid" 2>/dev/null; then\n'
                    '  echo "Timed out waiting for Codex task_complete" >&2\n'
                    '  kill "$intendant_pid" 2>/dev/null || true\n'
                    "  sleep 1\n"
                    '  kill -9 "$intendant_pid" 2>/dev/null || true\n'
                    '  wait "$intendant_pid" 2>/dev/null || true\n'
                    "  exit 124\n"
                    "fi\n"
                    'wait "$intendant_pid"\n'
                ),
                env=env,
            )
        finally:
            try:
                await self.exec_as_agent(
                    environment,
                    command=(
                        f"mkdir -p {shlex.quote(agent_dir)}\n"
                        'if [ -d "$CODEX_HOME/sessions" ]; then\n'
                        f"  rm -rf {shlex.quote((EnvironmentPaths.agent_dir / 'sessions').as_posix())}\n"
                        f'  cp -R "$CODEX_HOME/sessions" {shlex.quote((EnvironmentPaths.agent_dir / "sessions").as_posix())}\n'
                        "fi\n"
                        f"cp -f intendant.toml {shlex.quote(agent_dir)}/intendant.toml 2>/dev/null || true\n"
                    ),
                    env=env,
                )
            except Exception:
                pass
            if auth_json_path:
                try:
                    await environment.download_file(remote_auth_path, auth_json_path)
                    auth_json_path.chmod(0o600)
                except Exception as exc:
                    self.logger.warning(
                        "Failed to persist refreshed Codex auth.json: %s", exc
                    )
            try:
                await self.exec_as_agent(
                    environment,
                    command=f'rm -rf {shlex.quote(remote_secrets_dir)} "$CODEX_HOME"',
                    env=env,
                )
            except Exception:
                pass
