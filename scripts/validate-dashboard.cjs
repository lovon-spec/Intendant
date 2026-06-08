#!/usr/bin/env node
'use strict';

const assert = require('assert');
const crypto = require('crypto');
const fs = require('fs');
const http = require('http');
const https = require('https');
const net = require('net');
const os = require('os');
const path = require('path');
const tls = require('tls');
const { EventEmitter } = require('events');
const vm = require('vm');
const { spawn, spawnSync } = require('child_process');

const DEFAULT_TIMEOUT_MS = 15000;
const DEFAULT_CDP_TIMEOUT_MS = 10000;
const DEFAULT_DASHBOARD_TIMEOUT_MS = 15000;
const DEFAULT_LOG_LINES = 8;
const LOG_BUFFER_LIMIT = 80;
const LOG_TEXT_LIMIT = 260;
const RESULT_REASON_LIMIT = 520;
const RESULT_LOG_LIMIT = 320;
const DIAGNOSTIC_TEXT_LIMIT = 260;
const DIAGNOSTIC_BODY_LIMIT = 360;
const DIAGNOSTIC_SELECTOR_LIMIT = 220;
const DIAGNOSTIC_LIST_LIMIT = 8;
const DIAGNOSTIC_SELECTOR_MATCH_LIMIT = 8;
const DIAGNOSTIC_SOURCE_SELECTOR_LIMIT = 8;
const FORMATTED_DIAGNOSTIC_LINE_LIMIT = 520;
const FORMATTED_STATION_STATUS_LINE_LIMIT = 2000;
const STATION_WARNING_LIMIT = 6;
const PROTECTED_DASHBOARD_PORT = 8765;
const STALE_BINARY_MTIME_SLOP_MS = 1000;
const DASHBOARD_BINARY_INPUT_FILES = ['Cargo.toml', 'Cargo.lock'];
const DASHBOARD_BINARY_INPUT_DIRS = ['src', 'crates', 'static'];

const BROWSER_EXECUTABLE_ENVS = [
  'INTENDANT_BROWSER_WORKSPACE_EXECUTABLE',
  'INTENDANT_BROWSER_EXECUTABLE',
  'CHROME_PATH',
  'CHROME_BIN',
];
const GRAPHICAL_SESSION_ENV_KEYS = [
  'DISPLAY',
  'WAYLAND_DISPLAY',
  'XDG_RUNTIME_DIR',
  'XDG_SESSION_TYPE',
  'DBUS_SESSION_BUS_ADDRESS',
  'XAUTHORITY',
  'XDG_CURRENT_DESKTOP',
  'DESKTOP_SESSION',
];

function printUsage() {
  console.log(`Usage:
  scripts/validate-dashboard.cjs --port <port> [checks]
  scripts/validate-dashboard.cjs --url http://127.0.0.1:<port>/app [checks]

Checks:
  --selector CSS              Wait until document.querySelector(CSS) exists
  --wait-for-selector CSS     Alias for --selector
  --wait-for-function JS      Wait until a JS expression/function returns truthy
  --station-probe NAME        Named Station probe: status, canvas, rendered, dock, dock-hidden, dock-controls, webgpu

Options:
  --host HOST                 Host used with --port (default: 127.0.0.1)
  --path PATH                 Path used with --port (default: /)
  --timeout MS               Page/check timeout (default: ${DEFAULT_TIMEOUT_MS})
  --cdp-timeout MS           Chromium CDP readiness timeout (default: ${DEFAULT_CDP_TIMEOUT_MS})
  --browser PATH             Chromium/Chrome executable
  --headed                   Run without --headless=new
  --enable-gpu               Omit the default --disable-gpu Chromium flag (implied by --station-probe webgpu)
  --browser-arg ARG          Extra Chromium arg; repeatable
  --sandbox                  Omit default --no-sandbox
  --log-lines N              Bounded browser/page log lines on failure (default: ${DEFAULT_LOG_LINES})
  --diagnostics              On failure, include compact generic DOM/page state
  --launch-dashboard         Launch a temporary Intendant dashboard and stop it afterward
  --dashboard-binary PATH    Intendant binary for --launch-dashboard (default: $INTENDANT or a fresh target/{release,debug}/intendant)
  --dashboard-arg ARG        Extra arg appended to the launched dashboard command; repeatable
  --dashboard-timeout MS     Temporary dashboard readiness timeout (default: ${DEFAULT_DASHBOARD_TIMEOUT_MS})
  --check-static-scripts     Parse inline classic/module scripts in static/app.html without executing them
  --app-html PATH            HTML file for --check-static-scripts (default: static/app.html)
  --json                     Print one compact JSON result
  --self-test                Run parser/formatter self-tests; does not launch a browser

If --url/--port are omitted, the script derives the dashboard port from
INTENDANT_MCP_URL when available. It never defaults to port 8765.
--check-static-scripts may run by itself without --url/--port.

On Linux, --headed browser runs launched from SSH import graphical session
variables from systemd --user when DISPLAY/WAYLAND_DISPLAY are absent.

With --launch-dashboard, HTTP targets add --no-tls and stale worktree target
binaries are rejected before launch. Rebuild or use $INTENDANT/--dashboard-binary.`);
}

function parseArgs(argv, env = process.env) {
  const opts = {
    host: '127.0.0.1',
    path: '/',
    selectors: [],
    functions: [],
    stationProbes: [],
    timeoutMs: DEFAULT_TIMEOUT_MS,
    cdpTimeoutMs: DEFAULT_CDP_TIMEOUT_MS,
    logLines: DEFAULT_LOG_LINES,
    diagnostics: false,
    launchDashboard: false,
    dashboardArgs: [],
    dashboardTimeoutMs: DEFAULT_DASHBOARD_TIMEOUT_MS,
    headless: true,
    enableGpu: false,
    browserArgs: [],
    noSandbox: true,
    json: false,
    selfTest: false,
    checkStaticScripts: false,
    appHtmlPath: path.join('static', 'app.html'),
    explicitDashboardTarget: false,
  };

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const readValue = () => {
      i += 1;
      if (i >= argv.length) {
        throw new Error(`${arg} requires a value`);
      }
      return argv[i];
    };
    const readNumber = (name) => {
      const value = Number(readValue());
      if (!Number.isFinite(value) || value <= 0) {
        throw new Error(`${name} must be a positive number`);
      }
      return Math.floor(value);
    };

    if (arg === '-h' || arg === '--help') {
      opts.help = true;
    } else if (arg === '--self-test') {
      opts.selfTest = true;
    } else if (arg === '--url') {
      opts.url = readValue();
      opts.explicitDashboardTarget = true;
    } else if (arg.startsWith('--url=')) {
      opts.url = arg.slice('--url='.length);
      opts.explicitDashboardTarget = true;
    } else if (arg === '--port') {
      opts.port = readNumber('--port');
      opts.explicitDashboardTarget = true;
    } else if (arg.startsWith('--port=')) {
      opts.port = parsePositiveInt(arg.slice('--port='.length), '--port');
      opts.explicitDashboardTarget = true;
    } else if (arg === '--host') {
      opts.host = readValue();
    } else if (arg.startsWith('--host=')) {
      opts.host = arg.slice('--host='.length);
    } else if (arg === '--path') {
      opts.path = readValue();
    } else if (arg.startsWith('--path=')) {
      opts.path = arg.slice('--path='.length);
    } else if (arg === '--selector' || arg === '--wait-for-selector') {
      opts.selectors.push(readValue());
    } else if (arg.startsWith('--selector=')) {
      opts.selectors.push(arg.slice('--selector='.length));
    } else if (arg.startsWith('--wait-for-selector=')) {
      opts.selectors.push(arg.slice('--wait-for-selector='.length));
    } else if (arg === '--wait-for-function') {
      opts.functions.push(readValue());
    } else if (arg.startsWith('--wait-for-function=')) {
      opts.functions.push(arg.slice('--wait-for-function='.length));
    } else if (arg === '--station-probe') {
      opts.stationProbes.push(normalizeStationProbeName(readValue()));
    } else if (arg.startsWith('--station-probe=')) {
      opts.stationProbes.push(normalizeStationProbeName(arg.slice('--station-probe='.length)));
    } else if (arg === '--timeout') {
      opts.timeoutMs = readNumber('--timeout');
    } else if (arg.startsWith('--timeout=')) {
      opts.timeoutMs = parsePositiveInt(arg.slice('--timeout='.length), '--timeout');
    } else if (arg === '--cdp-timeout') {
      opts.cdpTimeoutMs = readNumber('--cdp-timeout');
    } else if (arg.startsWith('--cdp-timeout=')) {
      opts.cdpTimeoutMs = parsePositiveInt(arg.slice('--cdp-timeout='.length), '--cdp-timeout');
    } else if (arg === '--browser') {
      opts.browser = readValue();
    } else if (arg.startsWith('--browser=')) {
      opts.browser = arg.slice('--browser='.length);
    } else if (arg === '--headed') {
      opts.headless = false;
    } else if (arg === '--enable-gpu') {
      opts.enableGpu = true;
    } else if (arg === '--browser-arg') {
      opts.browserArgs.push(readValue());
    } else if (arg.startsWith('--browser-arg=')) {
      opts.browserArgs.push(arg.slice('--browser-arg='.length));
    } else if (arg === '--sandbox') {
      opts.noSandbox = false;
    } else if (arg === '--log-lines') {
      opts.logLines = readNumber('--log-lines');
    } else if (arg.startsWith('--log-lines=')) {
      opts.logLines = parsePositiveInt(arg.slice('--log-lines='.length), '--log-lines');
    } else if (arg === '--diagnostics') {
      opts.diagnostics = true;
    } else if (arg === '--launch-dashboard') {
      opts.launchDashboard = true;
    } else if (arg === '--dashboard-binary') {
      opts.dashboardBinary = readValue();
    } else if (arg.startsWith('--dashboard-binary=')) {
      opts.dashboardBinary = arg.slice('--dashboard-binary='.length);
    } else if (arg === '--dashboard-arg') {
      opts.dashboardArgs.push(readValue());
    } else if (arg.startsWith('--dashboard-arg=')) {
      opts.dashboardArgs.push(arg.slice('--dashboard-arg='.length));
    } else if (arg === '--dashboard-timeout') {
      opts.dashboardTimeoutMs = readNumber('--dashboard-timeout');
    } else if (arg.startsWith('--dashboard-timeout=')) {
      opts.dashboardTimeoutMs = parsePositiveInt(arg.slice('--dashboard-timeout='.length), '--dashboard-timeout');
    } else if (arg === '--check-static-scripts') {
      opts.checkStaticScripts = true;
    } else if (arg === '--app-html') {
      opts.appHtmlPath = readValue();
    } else if (arg.startsWith('--app-html=')) {
      opts.appHtmlPath = arg.slice('--app-html='.length);
    } else if (arg === '--json') {
      opts.json = true;
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }

  opts.url = resolveDashboardUrl(opts, env);
  if (opts.launchDashboard) {
    validateDashboardLaunchOptions(opts);
  }
  return opts;
}

function parsePositiveInt(raw, name) {
  const value = Number(raw);
  if (!Number.isFinite(value) || value <= 0) {
    throw new Error(`${name} must be a positive number`);
  }
  return Math.floor(value);
}

function normalizeStationProbeName(raw) {
  const value = String(raw || '').trim().toLowerCase().replace(/_/g, '-');
  const aliases = new Map([
    ['ready', 'status'],
    ['surface', 'rendered'],
    ['rendered-surface', 'rendered'],
    ['canvas-rendered', 'rendered'],
    ['dock-nav', 'dock-controls'],
    ['controls-dock', 'dock-controls'],
    ['hidden-dock', 'dock-hidden'],
    ['gpu', 'webgpu'],
  ]);
  const normalized = aliases.get(value) || value;
  const allowed = new Set(['status', 'canvas', 'rendered', 'dock', 'dock-hidden', 'dock-controls', 'webgpu']);
  if (!allowed.has(normalized)) {
    throw new Error(`unknown Station probe '${raw}'; expected one of ${Array.from(allowed).join(', ')}`);
  }
  return normalized;
}

function resolveDashboardUrl(opts, env) {
  if (opts.url) {
    return normalizeDashboardUrl(opts.url);
  }
  if (opts.port) {
    return normalizeDashboardUrl(`http://${opts.host}:${opts.port}${normalizePath(opts.path)}`);
  }
  const fromMcp = dashboardUrlFromMcpUrl(env.INTENDANT_MCP_URL);
  if (fromMcp) {
    return fromMcp;
  }
  return undefined;
}

function validateDashboardLaunchOptions(opts) {
  const port = dashboardLaunchPort(opts);
  if (port === PROTECTED_DASHBOARD_PORT) {
    throw new Error(
      `--launch-dashboard refuses protected port ${PROTECTED_DASHBOARD_PORT}; choose a throwaway port`,
    );
  }
  if (!opts.url) {
    throw new Error('--launch-dashboard requires --port or --url');
  }
  const url = new URL(opts.url);
  if (!isLoopbackHost(url.hostname)) {
    throw new Error('--launch-dashboard only supports loopback dashboard URLs');
  }
  return port;
}

function dashboardLaunchPort(opts) {
  if (opts.port) {
    return opts.port;
  }
  if (opts.url) {
    const url = new URL(opts.url);
    if (url.port) {
      return parsePositiveInt(url.port, 'dashboard URL port');
    }
  }
  throw new Error('--launch-dashboard requires --port or a --url with an explicit port');
}

function isLoopbackHost(hostname) {
  const host = String(hostname || '').replace(/^\[|\]$/g, '').toLowerCase();
  if (host === 'localhost' || host === '::1' || host === '0:0:0:0:0:0:0:1') {
    return true;
  }
  return /^127(?:\.\d{1,3}){3}$/.test(host);
}

function dashboardUrlFromMcpUrl(raw) {
  if (!raw || !raw.trim()) {
    return undefined;
  }
  try {
    const url = new URL(raw);
    url.pathname = '/';
    url.search = '';
    url.hash = '';
    return url.toString();
  } catch (_) {
    return undefined;
  }
}

function normalizePath(raw) {
  if (!raw || raw === '/') {
    return '/';
  }
  return raw.startsWith('/') ? raw : `/${raw}`;
}

function normalizeDashboardUrl(raw) {
  try {
    const url = new URL(raw);
    if (url.protocol !== 'http:' && url.protocol !== 'https:') {
      throw new Error('dashboard URL must use http or https');
    }
    return url.toString();
  } catch (error) {
    throw new Error(`invalid dashboard URL '${raw}': ${error.message}`);
  }
}

async function main() {
  let opts;
  try {
    opts = parseArgs(process.argv.slice(2));
  } catch (error) {
    console.error(`FAIL dashboard-validation reason=${quote(error.message)}`);
    console.error('Run scripts/validate-dashboard.cjs --help for usage.');
    process.exitCode = 2;
    return;
  }

  if (opts.help) {
    printUsage();
    return;
  }

  if (opts.selfTest) {
    await runSelfTest();
    return;
  }

  let staticScriptResult;
  if (opts.checkStaticScripts) {
    staticScriptResult = validateInlineScriptsInHtmlFile(opts.appHtmlPath);
    if (staticScriptsOnly(opts)) {
      printStaticScriptResult(opts, staticScriptResult);
      return;
    }
  }

  if (!opts.url) {
    console.error('FAIL dashboard-validation reason="missing --url/--port and INTENDANT_MCP_URL"');
    console.error('Run scripts/validate-dashboard.cjs --help for usage.');
    process.exitCode = 2;
    return;
  }

  const started = Date.now();
  let dashboard;
  let harness;
  const closeOwnedProcesses = async () => {
    const closeTasks = [];
    if (harness) {
      closeTasks.push(harness.close());
    }
    if (dashboard) {
      closeTasks.push(dashboard.close());
    }
    await Promise.allSettled(closeTasks);
  };
  const removeSignalCleanup = installSignalCleanup(async () => {
    await closeOwnedProcesses();
  });
  try {
    const launchEnv = resolveLaunchEnvironment(opts);
    if (opts.launchDashboard) {
      dashboard = await TemporaryDashboard.launch(opts, launchEnv);
    }
    harness = await BrowserHarness.launch(opts, launchEnv);
    await harness.validate(opts);
    const result = {
      status: 'pass',
      url: opts.url,
      ms: Date.now() - started,
      browser: harness.browserExecutable,
      websocket: harness.websocketKind,
      selectors: opts.selectors.length,
      functions: opts.functions.length,
      stationProbes: opts.stationProbes.length,
      staticScripts: staticScriptResult,
    };
    printResult(opts, result);
  } catch (error) {
    const diagnostics = shouldCollectFailureDiagnostics(opts, error) && harness
      ? await harness.failureDiagnostics(opts).catch((diagError) => ({
          error: diagError.message || String(diagError),
        }))
      : undefined;
    const result = {
      status: 'fail',
      url: opts.url,
      ms: Date.now() - started,
      reason: error.message || String(error),
      failureKind: validationFailureKind(error.message || String(error)),
      browser: harness && harness.browserExecutable,
      websocket: harness && harness.websocketKind,
      logs: collectFailureLogs(opts.logLines, dashboard, harness),
      diagnostics,
      diagnosticsAuto: Boolean(diagnostics && !opts.diagnostics),
    };
    printResult(opts, result);
    process.exitCode = 1;
  } finally {
    await closeOwnedProcesses();
    removeSignalCleanup();
  }
}

function staticScriptsOnly(opts) {
  return Boolean(
    opts.checkStaticScripts
      && !opts.explicitDashboardTarget
      && !opts.launchDashboard
      && opts.selectors.length === 0
      && opts.functions.length === 0
      && opts.stationProbes.length === 0,
  );
}

class TemporaryDashboard {
  static async launch(opts, launchEnv = { env: process.env }) {
    const port = validateDashboardLaunchOptions(opts);
    await assertDashboardPortAvailable(port, opts.url);
    const executable = resolveDashboardBinary(opts.dashboardBinary);
    const args = dashboardLaunchArgs(port, opts.dashboardArgs, new URL(opts.url).protocol);
    assertDashboardBinarySupportsLaunchArgs(executable, args);
    const logs = new BoundedLog(LOG_BUFFER_LIMIT);
    const child = spawn(executable, args, {
      cwd: process.cwd(),
      detached: process.platform !== 'win32',
      env: launchEnv.env || process.env,
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    recordChildOutput(child.stdout, logs, 'dashboard.stdout');
    recordChildOutput(child.stderr, logs, 'dashboard.stderr');

    const dashboard = new TemporaryDashboard(
      executable,
      args,
      child,
      logs,
      port,
      dashboardReadyUrl(opts),
    );
    try {
      await dashboard.waitForReady(opts.dashboardTimeoutMs);
      return dashboard;
    } catch (error) {
      await dashboard.close();
      throw new Error(`${error.message || String(error)}${formatLogSuffix(logs, 4)}`);
    }
  }

  constructor(executable, args, child, logs, port, readyUrl) {
    this.executable = executable;
    this.args = args;
    this.child = child;
    this.logs = logs;
    this.port = port;
    this.readyUrl = readyUrl;
    this.closed = false;
  }

  async waitForReady(timeoutMs) {
    await waitUntil(
      async () => {
        const status = childExitStatus(this.child);
        if (status) {
          throw new Error(`temporary dashboard exited before readiness (${status})`);
        }
        return httpReady(this.readyUrl);
      },
      timeoutMs,
      `temporary dashboard was not ready at ${this.readyUrl} within ${timeoutMs}ms`,
    );
  }

  failureExcerpt(lineCount) {
    return this.logs.excerpt(lineCount);
  }

  async close() {
    if (this.closed) {
      return;
    }
    this.closed = true;
    if (!this.child || this.child.exitCode !== null || this.child.signalCode !== null) {
      return;
    }
    terminateChildProcess(this.child, 'SIGTERM');
    try {
      await waitForExit(this.child, 1500);
    } catch (_) {
      terminateChildProcess(this.child, 'SIGKILL');
      await waitForExit(this.child, 800).catch(() => {});
    }
  }
}

function printResult(opts, result) {
  const displayResult = compactResultForOutput(opts, result);
  if (opts.json) {
    console.log(JSON.stringify(displayResult));
    return;
  }
  if (displayResult.status === 'pass') {
    console.log(
      `PASS dashboard-validation url=${quote(displayResult.url)} selectors=${displayResult.selectors} functions=${displayResult.functions} stationProbes=${displayResult.stationProbes || 0} ms=${displayResult.ms} websocket=${displayResult.websocket || 'unknown'}`,
    );
    if (displayResult.staticScripts) {
      console.log(formatStaticScriptPass(displayResult.staticScripts));
    }
    return;
  }
  console.error(
    `FAIL dashboard-validation url=${quote(displayResult.url)} kind=${quote(displayResult.failureKind || 'unknown')} reason=${quote(displayResult.reason)} ms=${displayResult.ms}`,
  );
  for (const line of displayResult.logs || []) {
    console.error(`  ${line}`);
  }
  for (const line of formatDiagnostics(displayResult.diagnostics)) {
    console.error(`  ${line}`);
  }
  if (displayResult.next) {
    console.error(`  next=${quote(displayResult.next)}`);
  }
}

function printStaticScriptResult(opts, result) {
  if (opts.json) {
    console.log(JSON.stringify({ status: 'pass', staticScripts: result }));
    return;
  }
  console.log(formatStaticScriptPass(result));
}

function formatStaticScriptPass(result) {
  return `PASS dashboard-static-scripts file=${quote(result.file)} scripts=${result.scripts} classic=${result.classic} modules=${result.modules}`;
}

function collectFailureLogs(lineCount, dashboard, harness) {
  if (!lineCount || lineCount <= 0) {
    return [];
  }
  const dashboardBudget = dashboard && harness ? Math.max(1, Math.floor(lineCount / 2)) : lineCount;
  const dashboardLogs = dashboard ? dashboard.failureExcerpt(dashboardBudget) : [];
  const browserBudget = Math.max(0, lineCount - dashboardLogs.length);
  const browserLogs = harness ? harness.failureExcerpt(browserBudget) : [];
  return [...dashboardLogs, ...browserLogs].slice(-lineCount);
}

function dashboardLaunchArgs(port, extraArgs = [], protocol = 'http:') {
  const args = ['--web', String(port), '--no-tui'];
  if (protocol === 'http:' && !dashboardArgsSelectTlsMode(extraArgs)) {
    args.push('--no-tls');
  }
  args.push(...extraArgs);
  return args;
}

function dashboardReadyUrl(opts) {
  const url = new URL(opts.url);
  url.pathname = '/';
  url.search = '';
  url.hash = '';
  return url.toString();
}

function dashboardArgsSelectTlsMode(args) {
  return args.some((arg) => {
    const flag = String(arg).split('=')[0];
    return ['--no-tls', '--tls', '--mtls', '--tls-cert', '--tls-key', '--mtls-ca'].includes(flag);
  });
}

function resolveDashboardBinary(explicit, env = process.env, cwd = process.cwd()) {
  const exeName = process.platform === 'win32' ? 'intendant.exe' : 'intendant';
  const candidates = [];
  if (explicit) {
    candidates.push({ path: explicit, source: '--dashboard-binary', strictFreshness: true });
  }
  if (env.INTENDANT) {
    candidates.push({ path: env.INTENDANT, source: 'INTENDANT', strictFreshness: true });
  }
  candidates.push({
    path: path.join(cwd, 'target', 'release', exeName),
    source: 'target/release',
    strictFreshness: false,
  });
  candidates.push({
    path: path.join(cwd, 'target', 'debug', exeName),
    source: 'target/debug',
    strictFreshness: false,
  });
  candidates.push(...whichCandidates([exeName, 'intendant']).map((candidate) => ({
    path: candidate,
    source: 'PATH',
    strictFreshness: false,
  })));
  const stale = [];
  for (const candidate of candidates) {
    if (!candidate.path || !isExecutableFile(candidate.path)) {
      continue;
    }
    const staleReason = worktreeTargetFreshnessIssue(candidate.path, cwd);
    if (staleReason) {
      stale.push(staleReason);
      if (candidate.strictFreshness) {
        throw new Error(formatStaleDashboardBinaryMessage(staleReason));
      }
      continue;
    }
    return candidate.path;
  }
  if (stale.length) {
    throw new Error(formatStaleDashboardBinaryMessage(stale[0]));
  }
  throw new Error(
    'no intendant binary found for --launch-dashboard; run `cargo build --release` or pass --dashboard-binary',
  );
}

function worktreeTargetFreshnessIssue(candidate, cwd = process.cwd()) {
  let binaryPath;
  let targetRoot;
  try {
    binaryPath = fs.realpathSync(candidate);
    targetRoot = fs.realpathSync(path.join(cwd, 'target'));
  } catch (_) {
    return undefined;
  }
  if (!pathInside(binaryPath, targetRoot)) {
    return undefined;
  }
  const newestInput = newestDashboardBinaryInput(cwd);
  if (!newestInput) {
    return undefined;
  }
  const binaryStat = fs.statSync(binaryPath);
  if (binaryStat.mtimeMs + STALE_BINARY_MTIME_SLOP_MS >= newestInput.mtimeMs) {
    return undefined;
  }
  return {
    binary: binaryPath,
    binaryMtimeMs: binaryStat.mtimeMs,
    input: newestInput.path,
    inputMtimeMs: newestInput.mtimeMs,
  };
}

function pathInside(child, parent) {
  const relative = path.relative(parent, child);
  return relative === '' || (relative && !relative.startsWith('..') && !path.isAbsolute(relative));
}

function newestDashboardBinaryInput(cwd) {
  let newest;
  const record = (filePath, stat) => {
    if (!newest || stat.mtimeMs > newest.mtimeMs) {
      newest = { path: filePath, mtimeMs: stat.mtimeMs };
    }
  };
  for (const rel of DASHBOARD_BINARY_INPUT_FILES) {
    const full = path.join(cwd, rel);
    try {
      const stat = fs.statSync(full);
      if (stat.isFile()) {
        record(full, stat);
      }
    } catch (_) {}
  }
  for (const rel of DASHBOARD_BINARY_INPUT_DIRS) {
    scanNewestInput(path.join(cwd, rel), record);
  }
  return newest;
}

function scanNewestInput(root, record) {
  let entries;
  try {
    entries = fs.readdirSync(root, { withFileTypes: true });
  } catch (_) {
    return;
  }
  for (const entry of entries) {
    const full = path.join(root, entry.name);
    if (entry.isDirectory()) {
      if (entry.name !== 'target' && entry.name !== 'node_modules') {
        scanNewestInput(full, record);
      }
    } else if (entry.isFile()) {
      try {
        record(full, fs.statSync(full));
      } catch (_) {}
    }
  }
}

function formatStaleDashboardBinaryMessage(issue) {
  return [
    `refusing stale dashboard binary ${issue.binary}`,
    `it is older than ${path.relative(process.cwd(), issue.input) || issue.input}`,
    'run `cargo build --release`, set INTENDANT to the current controller binary, or pass --dashboard-binary <current-intendant>',
  ].join('; ');
}

function assertDashboardBinarySupportsLaunchArgs(executable, args) {
  const requiredFlags = requiredDashboardHelpFlags(args);
  if (!requiredFlags.length) {
    return;
  }
  const result = spawnSync(executable, ['--help'], {
    encoding: 'utf8',
    maxBuffer: 1024 * 1024,
    timeout: 5000,
  });
  if (result.error) {
    throw new Error(`could not verify dashboard binary ${executable}: ${result.error.message}`);
  }
  const output = `${result.stdout || ''}\n${result.stderr || ''}`;
  const missing = requiredFlags.filter((flag) => !output.includes(flag));
  if (missing.length) {
    throw new Error(
      `dashboard binary ${executable} does not advertise ${missing.join(', ')}; it is likely stale for scripts/validate-dashboard.cjs. Run \`cargo build --release\`, set INTENDANT to the current controller binary, or pass --dashboard-binary <current-intendant>`,
    );
  }
}

function requiredDashboardHelpFlags(args) {
  const flags = new Set(['--web', '--no-tui']);
  if (args.includes('--no-tls')) {
    flags.add('--no-tls');
  }
  return Array.from(flags);
}

async function assertDashboardPortAvailable(port, targetUrl) {
  const hosts = new Set(process.platform === 'win32' ? ['127.0.0.1'] : ['127.0.0.1', '::1']);
  if (targetUrl) {
    hosts.add(new URL(targetUrl).hostname.replace(/^\[|\]$/g, ''));
  }
  const occupied = [];
  for (const host of hosts) {
    if (await canConnect(host, port, 250)) {
      occupied.push(host);
    }
  }
  if (occupied.length) {
    throw new Error(
      `temporary dashboard port ${port} is already accepting connections on ${occupied.join(', ')}; choose another port`,
    );
  }
}

function canConnect(host, port, timeoutMs) {
  return new Promise((resolve) => {
    const socket = net.connect({ host, port });
    let settled = false;
    const finish = (connected) => {
      if (settled) {
        return;
      }
      settled = true;
      socket.destroy();
      resolve(connected);
    };
    socket.once('connect', () => finish(true));
    socket.once('error', () => finish(false));
    socket.setTimeout(timeoutMs, () => finish(false));
  });
}

function httpReady(url) {
  return new Promise((resolve) => {
    const parsed = new URL(url);
    const client = parsed.protocol === 'https:' ? https : http;
    const req = client.get(
      {
        hostname: parsed.hostname.replace(/^\[|\]$/g, ''),
        port: parsed.port,
        path: `${parsed.pathname || '/'}${parsed.search || ''}`,
        protocol: parsed.protocol,
        rejectUnauthorized: false,
      },
      (res) => {
        res.resume();
        resolve(Number(res.statusCode) >= 200 && Number(res.statusCode) < 500);
      },
    );
    req.on('error', () => resolve(false));
    req.setTimeout(1000, () => {
      req.destroy();
      resolve(false);
    });
  });
}

function validateInlineScriptsInHtmlFile(filePath) {
  const html = fs.readFileSync(filePath, 'utf8');
  const scripts = extractInlineJavaScript(html);
  const counts = { classic: 0, modules: 0 };
  for (const script of scripts) {
    if (script.goal === 'module') {
      counts.modules += 1;
      checkModuleSyntax(script.source, scriptLabel(filePath, script));
    } else {
      counts.classic += 1;
      checkClassicScriptSyntax(script.source, scriptLabel(filePath, script));
    }
  }
  return {
    file: filePath,
    scripts: scripts.length,
    classic: counts.classic,
    modules: counts.modules,
  };
}

function extractInlineJavaScript(html) {
  const scripts = [];
  const scriptTag = /<script\b([^>]*)>([\s\S]*?)<\/script>/gi;
  let match;
  while ((match = scriptTag.exec(html)) !== null) {
    const attrs = parseHtmlAttributes(match[1] || '');
    if (attrs.has('src')) {
      continue;
    }
    const goal = scriptGoal(attrs.get('type'));
    if (!goal) {
      continue;
    }
    scripts.push({
      index: scripts.length + 1,
      line: lineNumberAt(html, match.index),
      goal,
      source: match[2] || '',
    });
  }
  return scripts;
}

function parseHtmlAttributes(raw) {
  const attrs = new Map();
  const attrPattern = /([^\s"'<>/=]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+)))?/g;
  let match;
  while ((match = attrPattern.exec(raw)) !== null) {
    attrs.set(String(match[1]).toLowerCase(), match[2] ?? match[3] ?? match[4] ?? '');
  }
  return attrs;
}

function scriptGoal(rawType) {
  const type = String(rawType || '').trim().toLowerCase();
  if (!type || isClassicScriptType(type)) {
    return 'classic';
  }
  if (type === 'module') {
    return 'module';
  }
  return undefined;
}

function isClassicScriptType(type) {
  return [
    'text/javascript',
    'application/javascript',
    'application/ecmascript',
    'text/ecmascript',
    'text/jscript',
  ].includes(type);
}

function lineNumberAt(text, offset) {
  const prefix = text.slice(0, offset);
  const newlines = prefix.match(/\n/g);
  return newlines ? newlines.length + 1 : 1;
}

function scriptLabel(filePath, script) {
  return `${filePath}:script#${script.index}:${script.goal}:line${script.line}`;
}

function checkClassicScriptSyntax(source, filename) {
  try {
    new vm.Script(source, { filename });
  } catch (error) {
    throw new Error(`classic inline script syntax check failed in ${filename}: ${error.message}`);
  }
}

function checkModuleSyntax(source, filename) {
  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), 'validate-dashboard-module-'));
  const tempFile = path.join(tempDir, 'inline-script.mjs');
  const tempFileAliases = [tempFile];
  let result;
  try {
    fs.writeFileSync(tempFile, source, 'utf8');
    tempFileAliases.push(fs.realpathSync(tempFile));
    result = spawnSync(process.execPath, ['--check', tempFile], {
      encoding: 'utf8',
      maxBuffer: 4 * 1024 * 1024,
    });
  } finally {
    fs.rmSync(tempDir, { recursive: true, force: true });
  }
  if (result.error) {
    throw new Error(`module inline script syntax check failed in ${filename}: ${result.error.message}`);
  }
  if (result.status !== 0) {
    const details = nodeSyntaxCheckDetails(result, tempFileAliases);
    throw new Error(`module inline script syntax check failed in ${filename}: ${details}`);
  }
}

function nodeSyntaxCheckDetails(result, tempFileAliases) {
  const fallback = result.signal
    ? `node terminated by ${result.signal}`
    : `node exited ${result.status}`;
  const lines = String(result.stderr || result.stdout || fallback)
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean);
  const locationLine = lines
    .map((line) => replaceFirstPathAlias(line, tempFileAliases, 'inline module'))
    .find((line) => line !== undefined);
  const parserLine = lines.find((line) => /^[A-Za-z]+Error:/.test(line));
  if (locationLine && parserLine) {
    return `${locationLine}: ${parserLine}`;
  }
  return lines[0] || fallback;
}

function replaceFirstPathAlias(line, aliases, replacement) {
  for (const alias of aliases) {
    if (alias && line.includes(alias)) {
      return line.replace(alias, replacement);
    }
  }
  return undefined;
}

function recordChildOutput(stream, logs, kind) {
  if (!stream) {
    return;
  }
  stream.on('data', (chunk) => {
    for (const line of String(chunk).split(/\r?\n/)) {
      if (line.trim()) {
        logs.push(kind, line);
      }
    }
  });
}

function terminateChildProcess(child, signal) {
  if (!child || child.exitCode !== null || child.signalCode !== null) {
    return;
  }
  try {
    if (process.platform !== 'win32' && child.pid) {
      process.kill(-child.pid, signal);
    } else {
      child.kill(signal);
    }
  } catch (error) {
    if (!error || error.code !== 'ESRCH') {
      throw error;
    }
  }
}

function childExitStatus(child) {
  if (!child) {
    return 'missing child process';
  }
  if (child.exitCode !== null) {
    return `exit ${child.exitCode}`;
  }
  if (child.signalCode !== null) {
    return `signal ${child.signalCode}`;
  }
  return '';
}

function formatLogSuffix(logs, lineCount) {
  const lines = logs.excerpt(lineCount);
  return lines.length ? `; ${lines.join('; ')}` : '';
}

function installSignalCleanup(cleanup) {
  let cleaning = false;
  const handlers = new Map();
  for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
    const handler = () => {
      if (cleaning) {
        return;
      }
      cleaning = true;
      cleanup()
        .catch(() => {})
        .finally(() => {
          process.exit(signal === 'SIGINT' ? 130 : 143);
        });
    };
    handlers.set(signal, handler);
    process.once(signal, handler);
  }
  return () => {
    for (const [signal, handler] of handlers) {
      process.removeListener(signal, handler);
    }
  };
}

class BrowserHarness {
  static async launch(opts, launchEnv = { env: process.env, notes: [] }) {
    const executable = resolveBrowserExecutable(opts.browser);
    const userDataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'intendant-dashboard-validate-'));
    const stderr = new BoundedLog(LOG_BUFFER_LIMIT);
    const args = browserArgs(userDataDir, opts);
    for (const note of launchEnv.notes || []) {
      stderr.push('browser.env', note);
    }
    const child = spawn(executable, args, {
      stdio: ['ignore', 'ignore', 'pipe'],
      env: launchEnv.env || process.env,
    });
    child.stderr.on('data', (chunk) => {
      for (const line of String(chunk).split(/\r?\n/)) {
        if (line.trim()) {
          stderr.push('browser.stderr', line);
        }
      }
    });

    const harness = new BrowserHarness(executable, userDataDir, child, stderr);
    harness.activePort = await waitForDevToolsPort(userDataDir, child, stderr, opts.cdpTimeoutMs);
    const version = await httpJson(`http://127.0.0.1:${harness.activePort.port}/json/version`);
    const wsUrl = version.webSocketDebuggerUrl || `ws://127.0.0.1:${harness.activePort.port}${harness.activePort.path}`;
    const socket = await openWebSocket(wsUrl, opts.cdpTimeoutMs);
    harness.websocketKind = socket.kind;
    harness.cdp = new CdpConnection(socket);
    await harness.preparePage();
    return harness;
  }

  constructor(browserExecutable, userDataDir, child, stderr) {
    this.browserExecutable = browserExecutable;
    this.userDataDir = userDataDir;
    this.child = child;
    this.stderr = stderr;
    this.pageLogs = new BoundedLog(LOG_BUFFER_LIMIT);
    this.websocketKind = 'unknown';
    this.closed = false;
  }

  async preparePage() {
    const target = await this.cdp.send('Target.createTarget', { url: 'about:blank' });
    const attached = await this.cdp.send('Target.attachToTarget', {
      targetId: target.targetId,
      flatten: true,
    });
    this.sessionId = attached.sessionId;
    this.cdp.on('event', (message) => this.recordPageEvent(message));
    await this.cdp.send('Page.enable', {}, this.sessionId);
    await this.cdp.send('Runtime.enable', {}, this.sessionId);
    await this.cdp.send('Network.enable', {}, this.sessionId).catch(() => {});
    await this.cdp.send('Log.enable', {}, this.sessionId).catch(() => {});
  }

  async validate(opts) {
    let loaded = false;
    const onEvent = (message) => {
      if (message.sessionId === this.sessionId && message.method === 'Page.loadEventFired') {
        loaded = true;
      }
    };
    this.cdp.on('event', onEvent);
    try {
      const nav = await this.cdp.send('Page.navigate', { url: opts.url }, this.sessionId);
      if (nav.errorText) {
        throw new Error(`navigation failed: ${nav.errorText}`);
      }
      await waitUntil(
        async () => loaded || (await this.documentReadyState()) !== 'loading',
        opts.timeoutMs,
        `page did not become ready at ${opts.url}`,
      );
      for (const selector of opts.selectors) {
        await this.waitForSelector(selector, opts.timeoutMs);
      }
      for (const source of opts.functions) {
        await this.waitForFunction(source, opts.timeoutMs);
      }
      if (opts.stationProbes.length > 0) {
        await this.prepareStationSurface(opts.timeoutMs);
      }
      for (const probe of opts.stationProbes) {
        await this.waitForStationProbe(probe, opts.timeoutMs);
      }
    } finally {
      this.cdp.off('event', onEvent);
    }
  }

  async documentReadyState() {
    const result = await this.evaluate('document.readyState');
    return String(result || '');
  }

  async waitForSelector(selector, timeoutMs) {
    const expression = `Boolean(document.querySelector(${JSON.stringify(selector)}))`;
    await waitUntil(
      async () => Boolean(await this.evaluate(expression)),
      timeoutMs,
      `selector not found: ${selector}`,
    );
  }

  async waitForFunction(source, timeoutMs) {
    let lastError = '';
    let lastValue = '';
    const expression = waitFunctionExpression(source);
    await waitUntil(
      async () => {
        try {
          const value = await this.evaluate(expression);
          lastValue = summarizeWaitValue(value);
          return Boolean(value);
        } catch (error) {
          lastError = error.message || String(error);
          return false;
        }
      },
      timeoutMs,
      () => {
        const suffix = waitFailureSuffix(lastError, lastValue);
        return `wait-for-function did not become truthy${suffix}`;
      },
    );
  }

  async waitForStationProbe(probe, timeoutMs) {
    let lastError = '';
    let lastValue = '';
    const expression = stationProbeExpression(probe);
    await waitUntil(
      async () => {
        try {
          const value = await this.evaluate(expression);
          lastValue = summarizeWaitValue(value);
          return Boolean(value && value.ok);
        } catch (error) {
          lastError = error.message || String(error);
          return false;
        }
      },
      timeoutMs,
      () => `station probe ${probe} did not pass${waitFailureSuffix(lastError, lastValue)}`,
    );
  }

  async prepareStationSurface(timeoutMs) {
    const expression = `Promise.resolve((() => {
      const button = document.querySelector('[data-tab="station"]');
      if (typeof switchTab === 'function') {
        switchTab('station');
      } else if (button) {
        button.click();
      }
      const pane = document.getElementById('tab-station');
      const canvas = document.getElementById('station-hud-canvas');
      const rect = canvas ? canvas.getBoundingClientRect() : { width: 0, height: 0 };
      return Boolean(pane && pane.classList.contains('active') && canvas && rect.width > 0 && rect.height > 0);
    })())`;
    await waitUntil(
      async () => Boolean(await this.evaluate(expression)),
      timeoutMs,
      'Station tab did not expose a measurable rendered surface',
    );
  }

  async evaluate(expression) {
    const result = await this.cdp.send(
      'Runtime.evaluate',
      {
        expression,
        awaitPromise: true,
        returnByValue: true,
      },
      this.sessionId,
    );
    if (result.exceptionDetails) {
      throw new Error(exceptionText(result.exceptionDetails));
    }
    return result.result && Object.prototype.hasOwnProperty.call(result.result, 'value')
      ? result.result.value
      : undefined;
  }

  recordPageEvent(message) {
    if (message.sessionId !== this.sessionId || !message.method) {
      return;
    }
    const params = message.params || {};
    if (message.method === 'Runtime.consoleAPICalled') {
      const args = (params.args || []).map(remoteObjectText).filter(Boolean).join(' ');
      this.pageLogs.push(`console.${params.type || 'log'}`, args);
    } else if (message.method === 'Runtime.exceptionThrown') {
      this.pageLogs.push('exception', exceptionText(params.exceptionDetails || {}));
    } else if (message.method === 'Log.entryAdded') {
      const entry = params.entry || {};
      this.pageLogs.push(`log.${entry.level || 'entry'}`, entry.text || '');
    } else if (message.method === 'Network.loadingFailed') {
      this.pageLogs.push('network.failed', `${params.errorText || 'failed'} ${params.url || ''}`);
    } else if (message.method === 'Network.responseReceived') {
      const response = params.response || {};
      if (Number(response.status) >= 400) {
        this.pageLogs.push('network.status', `${response.status} ${response.url || ''}`);
      }
    }
  }

  failureExcerpt(lineCount) {
    return [
      ...this.pageLogs.excerpt(lineCount),
      ...this.stderr.excerpt(Math.max(0, lineCount - this.pageLogs.size())),
    ];
  }

  async failureDiagnostics(opts) {
    const diagnostics = await this.evaluate(`(${pageDiagnosticsSource()})(${JSON.stringify(failureDiagnosticSelectors(opts))})`);
    if (isStationFocusedCheck(opts)) {
      diagnostics.station = await this.evaluate(`(${stationDiagnosticsSource()})()`);
      diagnostics.station.warnings = stationConsoleWarnings(this.pageLogs.excerpt(LOG_BUFFER_LIMIT));
    }
    return diagnostics;
  }

  async close() {
    if (this.closed) {
      return;
    }
    this.closed = true;
    if (this.cdp) {
      await Promise.race([this.cdp.send('Browser.close'), delay(1000)]).catch(() => {});
      this.cdp.close();
    }
    if (this.child && !this.child.killed) {
      await waitForExit(this.child, 800).catch(() => {
        this.child.kill('SIGKILL');
      });
    }
    fs.rmSync(this.userDataDir, { recursive: true, force: true });
  }
}

function resolveLaunchEnvironment(
  opts,
  baseEnv = process.env,
  loadGraphicalEnv = loadSystemdUserEnvironment,
  platform = process.platform,
) {
  const env = { ...baseEnv };
  const notes = [];
  if (platform !== 'linux' || opts.headless) {
    return { env, notes };
  }

  const existingDisplayEnv = hasGraphicalDisplayEnv(env);
  const needsSupportEnv =
    (env.WAYLAND_DISPLAY && !env.XDG_RUNTIME_DIR)
    || (env.DISPLAY && !env.XAUTHORITY && !env.DBUS_SESSION_BUS_ADDRESS);
  if (!existingDisplayEnv || needsSupportEnv) {
    const graphicalEnv = loadGraphicalEnv(env);
    const imported = importMissingGraphicalSessionEnv(env, graphicalEnv);
    if (imported.length) {
      notes.push(`imported Linux graphical session env from systemd user manager: ${formatGraphicalEnvSummary(env, imported)}`);
    }
  }

  if (!hasGraphicalDisplayEnv(env)) {
    throw new Error(
      'headed browser validation requires DISPLAY or WAYLAND_DISPLAY, but neither was set and systemd --user did not expose a graphical session; run from the graphical/RDP session or export DISPLAY/WAYLAND_DISPLAY plus XDG_RUNTIME_DIR, DBUS_SESSION_BUS_ADDRESS, and XAUTHORITY from `systemctl --user show-environment`',
    );
  }

  return { env, notes };
}

function hasGraphicalDisplayEnv(env) {
  return Boolean((env.DISPLAY && String(env.DISPLAY).trim()) || (env.WAYLAND_DISPLAY && String(env.WAYLAND_DISPLAY).trim()));
}

function importMissingGraphicalSessionEnv(targetEnv, sourceEnv) {
  const imported = [];
  for (const key of GRAPHICAL_SESSION_ENV_KEYS) {
    if (!targetEnv[key] && sourceEnv && sourceEnv[key]) {
      targetEnv[key] = sourceEnv[key];
      imported.push(key);
    }
  }
  return imported;
}

function formatGraphicalEnvSummary(env, keys) {
  return keys
    .filter((key) => env[key])
    .map((key) => `${key}=${truncateMiddle(env[key], 80)}`)
    .join(' ');
}

function loadSystemdUserEnvironment(baseEnv = process.env) {
  const env = { ...baseEnv };
  if (!env.XDG_RUNTIME_DIR && typeof process.getuid === 'function') {
    env.XDG_RUNTIME_DIR = `/run/user/${process.getuid()}`;
  }
  if (!env.DBUS_SESSION_BUS_ADDRESS && env.XDG_RUNTIME_DIR) {
    env.DBUS_SESSION_BUS_ADDRESS = `unix:path=${env.XDG_RUNTIME_DIR}/bus`;
  }
  const result = spawnSync('systemctl', ['--user', 'show-environment'], {
    env,
    encoding: 'utf8',
    maxBuffer: 1024 * 1024,
  });
  if (result.error || result.status !== 0) {
    return {};
  }
  return parseSystemdUserEnvironment(result.stdout);
}

function parseSystemdUserEnvironment(output) {
  const parsed = {};
  for (const line of String(output || '').split(/\r?\n/)) {
    const index = line.indexOf('=');
    if (index <= 0) {
      continue;
    }
    const key = line.slice(0, index);
    if (GRAPHICAL_SESSION_ENV_KEYS.includes(key)) {
      parsed[key] = line.slice(index + 1);
    }
  }
  return parsed;
}

function browserArgs(userDataDir, opts) {
  const needsGpu = browserValidationNeedsGpu(opts);
  const args = [
    '--remote-debugging-port=0',
    `--user-data-dir=${userDataDir}`,
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-background-networking',
    '--disable-dev-shm-usage',
    '--disable-extensions',
    '--disable-popup-blocking',
    '--window-size=1440,1000',
  ];
  if (!needsGpu) {
    args.push('--disable-gpu');
  }
  if (opts.headless) {
    args.push('--headless=new');
  }
  if (opts.noSandbox) {
    args.push('--no-sandbox');
  }
  if (needsGpu && (opts.stationProbes || []).includes('webgpu')) {
    args.push('--enable-unsafe-webgpu');
  }
  args.push(...opts.browserArgs);
  return args;
}

function browserValidationNeedsGpu(opts) {
  return Boolean(opts.enableGpu || (opts.stationProbes || []).includes('webgpu'));
}

function resolveBrowserExecutable(explicit) {
  const candidates = [];
  if (explicit) {
    candidates.push(explicit);
  }
  for (const envName of BROWSER_EXECUTABLE_ENVS) {
    if (process.env[envName]) {
      candidates.push(process.env[envName]);
    }
  }
  candidates.push(...managedBrowserCandidates());
  candidates.push(...systemBrowserCandidates());
  for (const candidate of candidates) {
    if (candidate && isExecutableFile(candidate)) {
      return candidate;
    }
  }
  throw new Error(
    'no Chromium executable found; run `intendant setup browsers`, pass --browser, or set INTENDANT_BROWSER_WORKSPACE_EXECUTABLE',
  );
}

function managedBrowserCandidates() {
  const roots = [];
  const home = os.homedir();
  const cacheRoot = process.env.XDG_CACHE_HOME || (home ? path.join(home, '.cache') : '');
  const dataRoot = process.env.XDG_DATA_HOME || (home ? path.join(home, '.local', 'share') : '');
  if (process.platform === 'darwin' && home) {
    roots.push(path.join(home, 'Library', 'Caches', 'ms-playwright'));
    roots.push(path.join(home, 'Library', 'Caches', 'puppeteer'));
    roots.push(path.join(home, 'Library', 'Caches', 'chrome-for-testing'));
    roots.push(path.join(home, 'Library', 'Caches', 'intendant', 'browser-workspaces'));
    roots.push(path.join(home, 'Library', 'Application Support', 'intendant', 'browser-workspaces'));
  }
  if (cacheRoot) {
    roots.push(path.join(cacheRoot, 'ms-playwright'));
    roots.push(path.join(cacheRoot, 'puppeteer'));
    roots.push(path.join(cacheRoot, 'chrome-for-testing'));
    roots.push(path.join(cacheRoot, 'intendant', 'browser-workspaces'));
  }
  if (dataRoot) {
    roots.push(path.join(dataRoot, 'intendant', 'browser-workspaces'));
    roots.push(path.join(dataRoot, 'intendant', 'browsers'));
  }

  const names =
    process.platform === 'win32'
      ? ['chrome.exe', 'msedge.exe', 'chromium.exe']
      : process.platform === 'darwin'
        ? ['Google Chrome for Testing', 'Chromium', 'chrome']
        : ['chrome', 'chromium', 'chromium-browser', 'google-chrome'];
  const found = [];
  for (const root of roots) {
    found.push(...findExecutablesUnder(root, names, 8));
  }
  return found;
}

function systemBrowserCandidates() {
  if (process.platform === 'darwin') {
    return [
      '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
      '/Applications/Chromium.app/Contents/MacOS/Chromium',
      '/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge',
      '/Applications/Brave Browser.app/Contents/MacOS/Brave Browser',
    ];
  }
  if (process.platform === 'win32') {
    const roots = [
      process.env.PROGRAMFILES,
      process.env['PROGRAMFILES(X86)'],
      process.env.LOCALAPPDATA,
    ].filter(Boolean);
    const rels = [
      ['Google', 'Chrome', 'Application', 'chrome.exe'],
      ['Microsoft', 'Edge', 'Application', 'msedge.exe'],
      ['Chromium', 'Application', 'chrome.exe'],
    ];
    return roots.flatMap((root) => rels.map((rel) => path.join(root, ...rel)));
  }
  return whichCandidates(['google-chrome', 'chrome', 'chromium', 'chromium-browser', 'msedge', 'brave-browser']);
}

function whichCandidates(names) {
  const pathEnv = process.env.PATH || '';
  const dirs = pathEnv.split(path.delimiter).filter(Boolean);
  const candidates = [];
  for (const dir of dirs) {
    for (const name of names) {
      candidates.push(path.join(dir, name));
    }
  }
  return candidates;
}

function findExecutablesUnder(root, names, maxDepth) {
  if (!root || maxDepth < 0 || !fs.existsSync(root)) {
    return [];
  }
  let entries;
  try {
    entries = fs.readdirSync(root, { withFileTypes: true });
  } catch (_) {
    return [];
  }
  entries.sort((a, b) => a.name.localeCompare(b.name));
  const found = [];
  for (const entry of entries) {
    const full = path.join(root, entry.name);
    if (entry.isFile() && names.includes(entry.name) && isExecutableFile(full)) {
      found.push(full);
    } else if (entry.isDirectory() && maxDepth > 0) {
      found.push(...findExecutablesUnder(full, names, maxDepth - 1));
    }
  }
  return found;
}

function isExecutableFile(candidate) {
  try {
    const stat = fs.statSync(candidate);
    if (!stat.isFile()) {
      return false;
    }
    if (process.platform === 'win32') {
      return true;
    }
    return Boolean(stat.mode & 0o111);
  } catch (_) {
    return false;
  }
}

async function waitForDevToolsPort(userDataDir, child, stderr, timeoutMs) {
  const activePortPath = path.join(userDataDir, 'DevToolsActivePort');
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      throw new Error(chromiumCdpReadinessFailure(stderr, 'Chromium exited before CDP was ready'));
    }
    if (fs.existsSync(activePortPath)) {
      const lines = fs.readFileSync(activePortPath, 'utf8').trim().split(/\r?\n/);
      const port = Number(lines[0]);
      if (Number.isFinite(port) && port > 0) {
        return { port, path: lines[1] || '/devtools/browser' };
      }
    }
    await delay(80);
  }
  throw new Error(`CDP was not ready within ${timeoutMs}ms${formatStderrSuffix(stderr)}`);
}

function chromiumCdpReadinessFailure(stderr, prefix) {
  const displayHint = chromiumDisplayStartupHint(stderr);
  if (displayHint) {
    return `${prefix}; ${displayHint}${formatStderrSuffix(stderr)}`;
  }
  return `${prefix}${formatStderrSuffix(stderr)}`;
}

function chromiumDisplayStartupHint(stderr) {
  const text = stderr.excerpt(LOG_BUFFER_LIMIT).join('\n');
  if (!/Missing X server or \$DISPLAY|platform failed to initialize|ozone_platform_x11/i.test(text)) {
    return '';
  }
  return 'headed Linux Chromium could not reach the graphical display. For SSH validation, run with a live GNOME/RDP session or export DISPLAY/WAYLAND_DISPLAY plus XDG_RUNTIME_DIR, DBUS_SESSION_BUS_ADDRESS, and XAUTHORITY from `systemctl --user show-environment`';
}

function formatStderrSuffix(stderr) {
  const lines = stderr.excerpt(2);
  return lines.length ? `; ${lines.join('; ')}` : '';
}

function httpJson(url) {
  return new Promise((resolve, reject) => {
    const client = url.startsWith('https:') ? https : http;
    const req = client.get(url, (res) => {
      let body = '';
      res.setEncoding('utf8');
      res.on('data', (chunk) => {
        body += chunk;
      });
      res.on('end', () => {
        if (res.statusCode < 200 || res.statusCode >= 300) {
          reject(new Error(`GET ${url} returned ${res.statusCode}`));
          return;
        }
        try {
          resolve(JSON.parse(body));
        } catch (error) {
          reject(new Error(`GET ${url} returned invalid JSON: ${error.message}`));
        }
      });
    });
    req.on('error', reject);
    req.setTimeout(5000, () => {
      req.destroy(new Error(`GET ${url} timed out`));
    });
  });
}

class CdpConnection extends EventEmitter {
  constructor(socket) {
    super();
    this.socket = socket;
    this.nextId = 1;
    this.pending = new Map();
    socket.on('message', (raw) => this.handleMessage(raw));
    socket.on('close', () => this.rejectAll(new Error('CDP WebSocket closed')));
    socket.on('error', (error) => this.rejectAll(error));
  }

  send(method, params = {}, sessionId) {
    const id = this.nextId;
    this.nextId += 1;
    const payload = { id, method, params };
    if (sessionId) {
      payload.sessionId = sessionId;
    }
    this.socket.send(JSON.stringify(payload));
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      setTimeout(() => {
        if (this.pending.delete(id)) {
          reject(new Error(`CDP ${method} timed out`));
        }
      }, 8000).unref();
    });
  }

  handleMessage(raw) {
    let message;
    try {
      message = JSON.parse(String(raw));
    } catch (_) {
      return;
    }
    if (message.id && this.pending.has(message.id)) {
      const pending = this.pending.get(message.id);
      this.pending.delete(message.id);
      if (message.error) {
        pending.reject(new Error(message.error.message || JSON.stringify(message.error)));
      } else {
        pending.resolve(message.result || {});
      }
      return;
    }
    this.emit('event', message);
  }

  rejectAll(error) {
    for (const pending of this.pending.values()) {
      pending.reject(error);
    }
    this.pending.clear();
  }

  close() {
    this.socket.close();
  }
}

async function openWebSocket(wsUrl, timeoutMs) {
  const globalWebSocket = globalThis.WebSocket;
  if (typeof globalWebSocket === 'function') {
    try {
      return await openLibraryWebSocket(globalWebSocket, wsUrl, timeoutMs, 'global');
    } catch (_) {
      // Fall through; older global implementations can be incomplete.
    }
  }
  try {
    // Optional dependency fallback when available in the caller's environment.
    // eslint-disable-next-line global-require, import/no-extraneous-dependencies
    const Ws = require('ws');
    return await openLibraryWebSocket(Ws, wsUrl, timeoutMs, 'ws');
  } catch (_) {
    return openMinimalWebSocket(wsUrl, timeoutMs);
  }
}

function openLibraryWebSocket(Ws, wsUrl, timeoutMs, kind) {
  return new Promise((resolve, reject) => {
    const ws = new Ws(wsUrl);
    const timer = setTimeout(() => {
      ws.close();
      reject(new Error(`CDP WebSocket did not open within ${timeoutMs}ms`));
    }, timeoutMs);
    ws.addEventListener
      ? ws.addEventListener('open', onOpen)
      : ws.on('open', onOpen);
    ws.addEventListener
      ? ws.addEventListener('error', onError)
      : ws.on('error', onError);

    function onOpen() {
      clearTimeout(timer);
      resolve(new LibraryWebSocketAdapter(ws, kind));
    }
    function onError(error) {
      clearTimeout(timer);
      reject(error.error || error);
    }
  });
}

class LibraryWebSocketAdapter extends EventEmitter {
  constructor(ws, kind) {
    super();
    this.ws = ws;
    this.kind = kind;
    const onMessage = (eventOrData) => {
      const data = eventOrData && typeof eventOrData === 'object' && 'data' in eventOrData
        ? eventOrData.data
        : eventOrData;
      if (Buffer.isBuffer(data)) {
        this.emit('message', data.toString('utf8'));
      } else if (data instanceof ArrayBuffer) {
        this.emit('message', Buffer.from(data).toString('utf8'));
      } else if (ArrayBuffer.isView(data)) {
        this.emit('message', Buffer.from(data.buffer, data.byteOffset, data.byteLength).toString('utf8'));
      } else if (data && typeof data.arrayBuffer === 'function') {
        data
          .arrayBuffer()
          .then((buffer) => this.emit('message', Buffer.from(buffer).toString('utf8')))
          .catch((error) => this.emit('error', error));
      } else {
        this.emit('message', String(data));
      }
    };
    const onClose = () => this.emit('close');
    const onError = (error) => this.emit('error', error.error || error);
    if (ws.addEventListener) {
      ws.addEventListener('message', onMessage);
      ws.addEventListener('close', onClose);
      ws.addEventListener('error', onError);
    } else {
      ws.on('message', onMessage);
      ws.on('close', onClose);
      ws.on('error', onError);
    }
  }

  send(text) {
    this.ws.send(text);
  }

  close() {
    this.ws.close();
  }
}

function openMinimalWebSocket(wsUrl, timeoutMs) {
  const url = new URL(wsUrl);
  const isTls = url.protocol === 'wss:';
  const port = Number(url.port) || (isTls ? 443 : 80);
  const key = crypto.randomBytes(16).toString('base64');
  const pathAndQuery = `${url.pathname || '/'}${url.search || ''}`;
  const socket = isTls
    ? tls.connect({ host: url.hostname, port, servername: url.hostname })
    : net.connect({ host: url.hostname, port });
  const adapter = new MinimalWebSocketAdapter(socket, key);

  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error(`CDP WebSocket did not open within ${timeoutMs}ms`));
    }, timeoutMs);
    socket.once(isTls ? 'secureConnect' : 'connect', () => {
      socket.write(
        [
          `GET ${pathAndQuery} HTTP/1.1`,
          `Host: ${url.host}`,
          'Upgrade: websocket',
          'Connection: Upgrade',
          `Sec-WebSocket-Key: ${key}`,
          'Sec-WebSocket-Version: 13',
          '',
          '',
        ].join('\r\n'),
      );
    });
    adapter.once('open', () => {
      clearTimeout(timer);
      resolve(adapter);
    });
    adapter.once('error', (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });
}

class MinimalWebSocketAdapter extends EventEmitter {
  constructor(socket, key) {
    super();
    this.kind = 'minimal';
    this.socket = socket;
    this.key = key;
    this.buffer = Buffer.alloc(0);
    this.opened = false;
    socket.on('data', (chunk) => this.handleData(chunk));
    socket.on('close', () => this.emit('close'));
    socket.on('error', (error) => this.emit('error', error));
  }

  handleData(chunk) {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    if (!this.opened) {
      const marker = this.buffer.indexOf('\r\n\r\n');
      if (marker === -1) {
        return;
      }
      const headers = this.buffer.slice(0, marker).toString('utf8');
      this.buffer = this.buffer.slice(marker + 4);
      if (!headers.startsWith('HTTP/1.1 101') && !headers.startsWith('HTTP/1.0 101')) {
        this.emit('error', new Error(`WebSocket handshake failed: ${headers.split(/\r?\n/)[0]}`));
        return;
      }
      this.opened = true;
      this.emit('open');
    }
    this.readFrames();
  }

  readFrames() {
    while (this.buffer.length >= 2) {
      const first = this.buffer[0];
      const second = this.buffer[1];
      const opcode = first & 0x0f;
      let offset = 2;
      let length = second & 0x7f;
      if (length === 126) {
        if (this.buffer.length < offset + 2) return;
        length = this.buffer.readUInt16BE(offset);
        offset += 2;
      } else if (length === 127) {
        if (this.buffer.length < offset + 8) return;
        const high = this.buffer.readUInt32BE(offset);
        const low = this.buffer.readUInt32BE(offset + 4);
        length = high * 2 ** 32 + low;
        offset += 8;
      }
      const masked = Boolean(second & 0x80);
      let mask;
      if (masked) {
        if (this.buffer.length < offset + 4) return;
        mask = this.buffer.slice(offset, offset + 4);
        offset += 4;
      }
      if (this.buffer.length < offset + length) {
        return;
      }
      let payload = this.buffer.slice(offset, offset + length);
      this.buffer = this.buffer.slice(offset + length);
      if (masked) {
        payload = unmask(payload, mask);
      }
      if (opcode === 0x1) {
        this.emit('message', payload.toString('utf8'));
      } else if (opcode === 0x8) {
        this.close();
      } else if (opcode === 0x9) {
        this.writeFrame(0xA, payload);
      }
    }
  }

  send(text) {
    this.writeFrame(0x1, Buffer.from(text, 'utf8'));
  }

  writeFrame(opcode, payload) {
    const mask = crypto.randomBytes(4);
    let header;
    if (payload.length < 126) {
      header = Buffer.alloc(2);
      header[1] = 0x80 | payload.length;
    } else if (payload.length < 65536) {
      header = Buffer.alloc(4);
      header[1] = 0x80 | 126;
      header.writeUInt16BE(payload.length, 2);
    } else {
      header = Buffer.alloc(10);
      header[1] = 0x80 | 127;
      header.writeUInt32BE(0, 2);
      header.writeUInt32BE(payload.length, 6);
    }
    header[0] = 0x80 | opcode;
    this.socket.write(Buffer.concat([header, mask, unmask(payload, mask)]));
  }

  close() {
    if (!this.socket.destroyed) {
      this.socket.end();
    }
  }
}

function unmask(payload, mask) {
  const out = Buffer.alloc(payload.length);
  for (let i = 0; i < payload.length; i += 1) {
    out[i] = payload[i] ^ mask[i % 4];
  }
  return out;
}

class BoundedLog {
  constructor(limit) {
    this.limit = limit;
    this.lines = [];
  }

  push(kind, text) {
    const compact = String(text || '').replace(/\s+/g, ' ').trim();
    if (!compact) {
      return;
    }
    this.lines.push(`[${kind}] ${truncate(compact, LOG_TEXT_LIMIT)}`);
    if (this.lines.length > this.limit) {
      this.lines.splice(0, this.lines.length - this.limit);
    }
  }

  excerpt(count) {
    return this.lines.slice(Math.max(0, this.lines.length - count));
  }

  size() {
    return this.lines.length;
  }
}

function waitFunctionExpression(source) {
  const trimmed = source.trim();
  return `Promise.resolve((() => {
    const candidate = (${trimmed});
    return typeof candidate === 'function' ? candidate() : candidate;
  })())`;
}

function stationProbeExpression(probe) {
  const normalized = normalizeStationProbeName(probe);
  return `Promise.resolve((() => {
    const stationDiagnostics = (${stationDiagnosticsSource()})();
    return (${stationProbeSource()})(${JSON.stringify(normalized)}, stationDiagnostics);
  })())`;
}

function summarizeWaitValue(value) {
  if (value === undefined) {
    return 'undefined';
  }
  let text;
  try {
    text = JSON.stringify(value);
  } catch (_) {
    text = String(value);
  }
  if (text === undefined) {
    text = String(value);
  }
  return truncateMiddle(text, 360);
}

function waitFailureSuffix(lastError, lastValue) {
  const details = [];
  if (lastValue) {
    details.push(`last value: ${lastValue}`);
  }
  if (lastError) {
    details.push(`last error: ${truncateMiddle(lastError, 260)}`);
  }
  return details.length ? ` (${details.join('; ')})` : '';
}

async function waitUntil(predicate, timeoutMs, failureMessage) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) {
      return;
    }
    await delay(100);
  }
  throw new Error(typeof failureMessage === 'function' ? failureMessage() : failureMessage);
}

function remoteObjectText(obj) {
  if (!obj) {
    return '';
  }
  if (Object.prototype.hasOwnProperty.call(obj, 'value')) {
    return String(obj.value);
  }
  return obj.description || obj.unserializableValue || obj.type || '';
}

function exceptionText(details) {
  if (!details) {
    return 'JavaScript exception';
  }
  const exception = details.exception || {};
  return (
    exception.description ||
    exception.value ||
    details.text ||
    `${details.url || ''}:${details.lineNumber || 0}:${details.columnNumber || 0}`
  );
}

function pageDiagnosticsSource() {
  return function collectPageDiagnostics(selectors) {
    const compact = (value, limit = 180) => {
      const text = String(value || '').replace(/\s+/g, ' ').trim();
      return text.length <= limit ? text : `${text.slice(0, limit - 1)}...`;
    };
    const describeElement = (el) => {
      if (!el) {
        return '';
      }
      const tag = (el.tagName || '').toLowerCase();
      const id = el.id ? `#${el.id}` : '';
      const classes = el.classList && el.classList.length
        ? `.${Array.from(el.classList).slice(0, 3).join('.')}`
        : '';
      const text = compact(
        el.getAttribute('aria-label') ||
          el.getAttribute('title') ||
          el.placeholder ||
          el.innerText ||
          el.textContent ||
          '',
        80,
      );
      const state = [];
      if (el.hidden) state.push('hidden');
      if (el.getAttribute('aria-hidden')) state.push(`aria-hidden=${el.getAttribute('aria-hidden')}`);
      if (el.getAttribute('aria-expanded')) state.push(`aria-expanded=${el.getAttribute('aria-expanded')}`);
      if (el.disabled) state.push('disabled');
      const data = Object.entries(el.dataset || {})
        .slice(0, 3)
        .map(([key, value]) => `${key}=${compact(value, 40)}`);
      const stateText = state.length ? ` [${state.join(' ')}]` : '';
      const dataText = data.length ? ` {${data.join(' ')}}` : '';
      return compact(`${tag}${id}${classes}${stateText}${dataText}${text ? ` "${text}"` : ''}`, 140);
    };
    const describeMany = (query, limit) => Array.from(document.querySelectorAll(query))
      .slice(0, limit)
      .map(describeElement)
      .filter(Boolean);
    return {
      location: window.location.href,
      title: compact(document.title, 120),
      readyState: document.readyState,
      activeElement: describeElement(document.activeElement),
      bodyText: compact(document.body ? document.body.innerText || document.body.textContent : '', 360),
      headings: describeMany('h1,h2,h3,[role="heading"]', 8),
      controls: describeMany('button,a,[role="button"],input,select,textarea', 12),
      selectorMatches: selectors.map((selector) => {
        try {
          const matches = Array.from(document.querySelectorAll(selector));
          return {
            selector,
            count: matches.length,
            first: describeElement(matches[0]),
          };
        } catch (error) {
          return {
            selector,
            error: error.message || String(error),
          };
        }
      }),
    };
  }.toString();
}

function stationDiagnosticsSource() {
  return function collectStationDiagnostics() {
    const status = document.getElementById('station-status');
    const canvas = document.getElementById('station-hud-canvas');
    const result = {
      statusFound: Boolean(status),
      statusText: status ? String(status.textContent || '') : '',
      canvasFound: Boolean(canvas),
    };
    if (!canvas) {
      return result;
    }
    const rect = canvas.getBoundingClientRect();
    result.canvas = {
      attrWidth: Number(canvas.width) || 0,
      attrHeight: Number(canvas.height) || 0,
      clientWidth: Number(canvas.clientWidth) || 0,
      clientHeight: Number(canvas.clientHeight) || 0,
      rectWidth: Math.round(Number(rect.width) || 0),
      rectHeight: Math.round(Number(rect.height) || 0),
      devicePixelRatio: Number(window.devicePixelRatio) || 1,
    };
    result.pixels = {
      sampleWidth: 0,
      sampleHeight: 0,
      litCount: 0,
      total: 0,
      samples: [],
    };
    const width = Number(canvas.width) || Math.round(Number(rect.width) || 0);
    const height = Number(canvas.height) || Math.round(Number(rect.height) || 0);
    if (width <= 0 || height <= 0) {
      result.pixels.error = 'canvas has no readable pixel area';
      return result;
    }
    try {
      const sampleWidth = Math.max(1, Math.min(12, width));
      const sampleHeight = Math.max(1, Math.min(12, height));
      const scratch = document.createElement('canvas');
      scratch.width = sampleWidth;
      scratch.height = sampleHeight;
      const ctx = scratch.getContext('2d');
      if (!ctx) {
        result.pixels.error = '2d sample context unavailable';
        return result;
      }
      ctx.drawImage(canvas, 0, 0, width, height, 0, 0, sampleWidth, sampleHeight);
      const data = ctx.getImageData(0, 0, sampleWidth, sampleHeight).data;
      let litCount = 0;
      const samples = [];
      for (let y = 0; y < sampleHeight; y += 1) {
        for (let x = 0; x < sampleWidth; x += 1) {
          const offset = (y * sampleWidth + x) * 4;
          const rgba = [data[offset], data[offset + 1], data[offset + 2], data[offset + 3]];
          if (rgba[3] > 0) {
            litCount += 1;
            if (samples.length < 4) {
              samples.push({ x, y, rgba });
            }
          }
        }
      }
      result.pixels = {
        sampleWidth,
        sampleHeight,
        litCount,
        total: sampleWidth * sampleHeight,
        samples,
      };
    } catch (error) {
      result.pixels.error = error && error.message ? error.message : String(error);
    }
    return result;
  }.toString();
}

function stationProbeSource() {
  return function collectStationProbe(probe, diagnostics) {
    const statusText = String(diagnostics && diagnostics.statusText ? diagnostics.statusText : '');
    const canvas = (diagnostics && diagnostics.canvas) || {};
    const pixels = (diagnostics && diagnostics.pixels) || {};
    const dock = document.getElementById('station-dock');
    const controlsNav = document.querySelector('#station-dock-nav [data-station-dock-nav="system:controls"]');
    const getGlobalLabel = (name) => {
      try {
        const fn = globalThis[name];
        return typeof fn === 'function' ? String(fn() || '') : '';
      } catch (_) {
        return '';
      }
    };
    const getDebugState = () => {
      try {
        return typeof station !== 'undefined' && station && station.debug_state
          ? String(station.debug_state() || '')
          : '';
      } catch (_) {
        return '';
      }
    };
    const renderer = getGlobalLabel('stationRendererLabel')
      || (/renderer=([^ ]+)/i.exec(statusText) || [])[1]
      || '';
    const webgpu = getGlobalLabel('stationWebgpuStatusLabel')
      || (/webgpu=([^ ]+)/i.exec(statusText) || [])[1]
      || '';
    const debugState = getDebugState();
    const canvasArea = Math.max(
      Number(canvas.attrWidth) * Number(canvas.attrHeight),
      Number(canvas.clientWidth) * Number(canvas.clientHeight),
      Number(canvas.rectWidth) * Number(canvas.rectHeight),
    );
    const base = {
      ok: false,
      probe,
      failureKind: 'probe',
      reason: '',
      statusText,
      renderer,
      webgpu,
      debugState,
      canvas: {
        found: Boolean(diagnostics && diagnostics.canvasFound),
        attr: `${Number(canvas.attrWidth) || 0}x${Number(canvas.attrHeight) || 0}`,
        client: `${Number(canvas.clientWidth) || 0}x${Number(canvas.clientHeight) || 0}`,
        rect: `${Number(canvas.rectWidth) || 0}x${Number(canvas.rectHeight) || 0}`,
      },
      pixels: {
        lit: Number(pixels.litCount) || 0,
        total: Number(pixels.total) || 0,
        error: pixels.error ? String(pixels.error) : '',
      },
      dock: {
        found: Boolean(dock),
        hidden: Boolean(dock && dock.classList && dock.classList.contains('hidden')),
        kind: dock && dock.dataset ? String(dock.dataset.kind || '') : '',
        controlsNavFound: Boolean(controlsNav),
      },
    };
    const pass = () => ({ ...base, ok: true, reason: 'passed' });
    const fail = (reason, failureKind = 'probe') => ({ ...base, reason, failureKind });

    if (probe === 'status') {
      if (base.statusText.trim() && !/\b(initializing|loading|failed)\b/i.test(base.statusText)) {
        return pass();
      }
      return fail(base.statusText ? 'Station status is not ready' : 'Station status text is missing');
    }
    if (probe === 'canvas') {
      if (base.canvas.found && canvasArea > 0) {
        return pass();
      }
      return fail(base.canvas.found ? 'Station canvas has no measured area' : 'Station canvas is missing');
    }
    if (probe === 'rendered') {
      if (base.canvas.found && (Number(pixels.litCount) || 0) > 0) {
        return pass();
      }
      return fail(
        base.canvas.found ? 'Station rendered surface has no lit canvas pixels' : 'Station canvas is missing',
        'renderer',
      );
    }
    if (probe === 'dock') {
      if (base.dock.found) {
        return pass();
      }
      return fail('Station dock is missing');
    }
    if (probe === 'dock-hidden') {
      if (base.dock.found && base.dock.hidden) {
        return pass();
      }
      return fail(base.dock.found ? 'Station dock is visible' : 'Station dock is missing');
    }
    if (probe === 'dock-controls') {
      if (base.dock.found && base.dock.controlsNavFound) {
        return pass();
      }
      return fail(base.dock.found ? 'Station dock controls nav is missing' : 'Station dock is missing');
    }
    if (probe === 'webgpu') {
      const active = /^webgpu$/i.test(base.renderer)
        || /\bactive\b/i.test(base.webgpu)
        || /\bgpu=true\b/i.test(`${base.debugState} ${base.statusText}`);
      if (active) {
        return pass();
      }
      const unavailable = /\b(unavailable|fallback|failed|gpu=false)\b/i.test(
        `${base.webgpu} ${base.renderer} ${base.debugState} ${base.statusText}`,
      );
      return fail(
        unavailable ? 'Station WebGPU renderer is unavailable or in fallback' : 'Station WebGPU renderer is not active yet',
        'renderer',
      );
    }
    return fail(`Unknown Station probe: ${probe}`);
  }.toString();
}

function stationConsoleWarnings(lines) {
  if (!Array.isArray(lines)) {
    return [];
  }
  return lines
    .filter((line) => /\[(console\.(warn|warning)|log\.warning|console\.error|exception)\]/i.test(line))
    .filter((line) => /\b(station|webgpu|canvas|fallback|wasm)\b/i.test(line))
    .slice(-STATION_WARNING_LIMIT);
}

function compactResultForOutput(opts, result) {
  const compact = { ...result };
  if (compact.reason) {
    compact.reason = truncateMiddle(compact.reason, RESULT_REASON_LIMIT);
  }
  if (compact.failureKind) {
    compact.failureKind = truncateMiddle(compact.failureKind, DIAGNOSTIC_TEXT_LIMIT);
  }
  if (Array.isArray(compact.logs)) {
    compact.logs = compact.logs.map((line) => truncateMiddle(line, RESULT_LOG_LIMIT));
  }
  if (compact.diagnostics) {
    compact.diagnostics = compactDiagnostics(compact.diagnostics, {
      suppressControls: isStationFocusedCheck(opts) && !opts.diagnostics,
    });
  }
  if (compact.status === 'fail') {
    compact.next = validationFailureNextStep(compact);
  }
  return compact;
}

function validationFailureNextStep(result) {
  if (/headed Linux Chromium could not reach the graphical display|Missing X server or \$DISPLAY|ozone_platform_x11/i.test(result.reason || '')) {
    return 'fix the remote graphical session environment first: on SSH hosts, prepend ~/.cargo/bin to PATH and run from a live GNOME/RDP session or import DISPLAY/WAYLAND_DISPLAY, XDG_RUNTIME_DIR, DBUS_SESSION_BUS_ADDRESS, and XAUTHORITY from systemctl --user show-environment';
  }
  if (result.failureKind === 'renderer') {
    return 'treat as renderer validation failure; use the Station diagnostics here instead of repeating broad DOM/source dumps';
  }
  if (result.failureKind === 'probe' || result.failureKind === 'assertion') {
    return 'treat as probe/assertion failure; adjust the targeted condition or report partial validation; avoid further broad selector/source dumps and do not retry the same brittle wait';
  }
  if (result.diagnostics) {
    return 'fix from these targeted facts or report partial validation; avoid further broad selector/source dumps';
  }
  return 'retry at most once with --diagnostics --json and a targeted selector/function';
}

function validationFailureKind(reason) {
  const text = String(reason || '');
  if (/^station probe .* did not pass/.test(text)) {
    return text.includes('"failureKind":"renderer"') ? 'renderer' : 'probe';
  }
  if (/^wait-for-function did not become truthy/.test(text)) {
    return 'assertion';
  }
  if (/^selector not found: /.test(text)) {
    return 'selector';
  }
  if (/navigation failed|did not become ready|temporary dashboard|Chromium|CDP|WebSocket|browser/i.test(text)) {
    return 'harness';
  }
  return 'unknown';
}

function compactDiagnostics(diagnostics, options = {}) {
  if (!diagnostics || typeof diagnostics !== 'object') {
    return diagnostics;
  }
  if (diagnostics.error) {
    return { error: truncateMiddle(diagnostics.error, RESULT_REASON_LIMIT) };
  }

  const compact = {
    readyState: truncateMiddle(diagnostics.readyState || '', DIAGNOSTIC_TEXT_LIMIT),
    title: truncateMiddle(diagnostics.title || '', DIAGNOSTIC_TEXT_LIMIT),
    location: truncateMiddle(diagnostics.location || '', DIAGNOSTIC_TEXT_LIMIT),
  };
  if (diagnostics.activeElement) {
    compact.activeElement = truncateMiddle(diagnostics.activeElement, DIAGNOSTIC_TEXT_LIMIT);
  }
  if (diagnostics.bodyText) {
    compact.bodyText = truncateMiddle(diagnostics.bodyText, DIAGNOSTIC_BODY_LIMIT);
  }
  if (diagnostics.station) {
    compact.station = compactStationDiagnostics(diagnostics.station);
  }

  const headings = compactStringArray(diagnostics.headings, DIAGNOSTIC_LIST_LIMIT, DIAGNOSTIC_TEXT_LIMIT);
  if (headings.values.length) {
    compact.headings = headings.values;
  }
  if (headings.omitted) {
    compact.headingsOmitted = headings.omitted;
  }

  if (!options.suppressControls) {
    const controls = compactStringArray(diagnostics.controls, DIAGNOSTIC_LIST_LIMIT, DIAGNOSTIC_TEXT_LIMIT);
    if (controls.values.length) {
      compact.controls = controls.values;
    }
    if (controls.omitted) {
      compact.controlsOmitted = controls.omitted;
    }
  }

  if (Array.isArray(diagnostics.selectorMatches)) {
    const matches = diagnostics.selectorMatches.slice(0, DIAGNOSTIC_SELECTOR_MATCH_LIMIT);
    compact.selectorMatches = matches.map((match) => {
      const item = {
        selector: truncateMiddle(match.selector || '', DIAGNOSTIC_SELECTOR_LIMIT),
      };
      if (match.error) {
        item.error = truncateMiddle(match.error, RESULT_REASON_LIMIT);
      } else {
        item.count = Number(match.count) || 0;
        if (match.first) {
          item.first = truncateMiddle(match.first, DIAGNOSTIC_TEXT_LIMIT);
        }
      }
      return item;
    });
    if (diagnostics.selectorMatches.length > matches.length) {
      compact.selectorMatchesOmitted = diagnostics.selectorMatches.length - matches.length;
    }
  }

  return compact;
}

function compactStationDiagnostics(station) {
  if (!station || typeof station !== 'object') {
    return station;
  }
  const compact = {
    statusText: String(station.statusText ?? ''),
    statusFound: Boolean(station.statusFound),
    canvasFound: Boolean(station.canvasFound),
  };
  if (station.canvas) {
    compact.canvas = {
      attrWidth: Number(station.canvas.attrWidth) || 0,
      attrHeight: Number(station.canvas.attrHeight) || 0,
      clientWidth: Number(station.canvas.clientWidth) || 0,
      clientHeight: Number(station.canvas.clientHeight) || 0,
      rectWidth: Number(station.canvas.rectWidth) || 0,
      rectHeight: Number(station.canvas.rectHeight) || 0,
      devicePixelRatio: Number(station.canvas.devicePixelRatio) || 0,
    };
  }
  if (station.pixels) {
    compact.pixels = {
      sampleWidth: Number(station.pixels.sampleWidth) || 0,
      sampleHeight: Number(station.pixels.sampleHeight) || 0,
      litCount: Number(station.pixels.litCount) || 0,
      total: Number(station.pixels.total) || 0,
      samples: Array.isArray(station.pixels.samples)
        ? station.pixels.samples.slice(0, 4).map((sample) => ({
            x: Number(sample.x) || 0,
            y: Number(sample.y) || 0,
            rgba: Array.isArray(sample.rgba) ? sample.rgba.slice(0, 4).map((n) => Number(n) || 0) : [],
          }))
        : [],
    };
    if (station.pixels.error) {
      compact.pixels.error = truncateMiddle(station.pixels.error, DIAGNOSTIC_TEXT_LIMIT);
    }
  }
  const warnings = compactStringArray(station.warnings, STATION_WARNING_LIMIT, DIAGNOSTIC_TEXT_LIMIT);
  if (warnings.values.length) {
    compact.warnings = warnings.values;
  }
  if (warnings.omitted) {
    compact.warningsOmitted = warnings.omitted;
  }
  return compact;
}

function compactStringArray(values, limit, textLimit) {
  if (!Array.isArray(values)) {
    return { values: [], omitted: 0 };
  }
  const kept = values.slice(0, limit).map((value) => truncateMiddle(value, textLimit));
  return {
    values: kept,
    omitted: Math.max(0, values.length - kept.length),
  };
}

function formatDiagnostics(diagnostics) {
  if (!diagnostics) {
    return [];
  }
  if (diagnostics.error) {
    return [`diagnostics error=${quote(diagnostics.error)}`];
  }
  const lines = [
    `diagnostics readyState=${quote(diagnostics.readyState || '')} title=${quote(diagnostics.title || '')} location=${quote(diagnostics.location || '')}`,
  ];
  if (diagnostics.station) {
    lines.push(...formatStationDiagnostics(diagnostics.station));
  }
  if (diagnostics.activeElement) {
    lines.push(`diagnostics active=${quote(diagnostics.activeElement)}`);
  }
  if (diagnostics.bodyText) {
    lines.push(`diagnostics body=${quote(diagnostics.bodyText)}`);
  }
  for (const selector of diagnostics.selectorMatches || []) {
    if (selector.error) {
      lines.push(`diagnostics selector=${quote(selector.selector)} error=${quote(selector.error)}`);
    } else {
      lines.push(
        `diagnostics selector=${quote(selector.selector)} count=${selector.count || 0} first=${quote(selector.first || '')}`,
      );
    }
  }
  if (diagnostics.headings && diagnostics.headings.length) {
    lines.push(`diagnostics headings=${quote(diagnostics.headings.join(' | '))}`);
  }
  if (diagnostics.controls && diagnostics.controls.length) {
    lines.push(`diagnostics controls=${quote(diagnostics.controls.join(' | '))}`);
  }
  if (diagnostics.headingsOmitted) {
    lines.push(`diagnostics headingsOmitted=${diagnostics.headingsOmitted}`);
  }
  if (diagnostics.controlsOmitted) {
    lines.push(`diagnostics controlsOmitted=${diagnostics.controlsOmitted}`);
  }
  if (diagnostics.selectorMatchesOmitted) {
    lines.push(`diagnostics selectorMatchesOmitted=${diagnostics.selectorMatchesOmitted}`);
  }
  return lines.map((line) => truncateMiddle(
    line,
    line.startsWith('station statusFound=')
      ? FORMATTED_STATION_STATUS_LINE_LIMIT
      : FORMATTED_DIAGNOSTIC_LINE_LIMIT,
  ));
}

function formatStationDiagnostics(station) {
  if (!station || typeof station !== 'object') {
    return [];
  }
  const lines = [
    `station statusFound=${Boolean(station.statusFound)} statusText=${quote(station.statusText ?? '')}`,
  ];
  if (station.canvas) {
    lines.push(
      `station canvasFound=${Boolean(station.canvasFound)} attr=${Number(station.canvas.attrWidth) || 0}x${Number(station.canvas.attrHeight) || 0} client=${Number(station.canvas.clientWidth) || 0}x${Number(station.canvas.clientHeight) || 0} rect=${Number(station.canvas.rectWidth) || 0}x${Number(station.canvas.rectHeight) || 0} dpr=${Number(station.canvas.devicePixelRatio) || 0}`,
    );
  } else {
    lines.push(`station canvasFound=${Boolean(station.canvasFound)}`);
  }
  if (station.pixels) {
    const sampleText = Array.isArray(station.pixels.samples)
      ? station.pixels.samples
          .slice(0, 4)
          .map((sample) => `${Number(sample.x) || 0},${Number(sample.y) || 0}:${(sample.rgba || []).join('/')}`)
          .join(' ')
      : '';
    const errorText = station.pixels.error ? ` error=${quote(station.pixels.error)}` : '';
    lines.push(
      `station pixels lit=${Number(station.pixels.litCount) || 0}/${Number(station.pixels.total) || 0} sample=${Number(station.pixels.sampleWidth) || 0}x${Number(station.pixels.sampleHeight) || 0} rgba=${quote(sampleText)}${errorText}`,
    );
  }
  for (const warning of station.warnings || []) {
    lines.push(`station warning=${quote(warning)}`);
  }
  if (station.warningsOmitted) {
    lines.push(`station warningsOmitted=${station.warningsOmitted}`);
  }
  return lines;
}

function shouldCollectFailureDiagnostics(opts, error) {
  if (opts.diagnostics) {
    return true;
  }
  const reason = error && (error.message || String(error));
  return isWaitFailureReason(reason);
}

function isWaitFailureReason(reason) {
  return /^selector not found: /.test(String(reason || ''))
    || /^wait-for-function did not become truthy/.test(String(reason || ''))
    || /^station probe .* did not pass/.test(String(reason || ''));
}

function failureDiagnosticSelectors(opts) {
  const selectors = [];
  for (const selector of opts.selectors || []) {
    addDiagnosticSelector(selectors, selector);
  }
  for (const selector of diagnosticSelectorsFromWaitFunctions(opts.functions || [])) {
    addDiagnosticSelector(selectors, selector);
  }
  if ((opts.stationProbes || []).length) {
    for (const selector of ['#station-status', '#station-hud-canvas', '#station-dock', '#station-dock-nav [data-station-dock-nav="system:controls"]']) {
      addDiagnosticSelector(selectors, selector);
    }
  }
  return selectors.slice(0, DIAGNOSTIC_SOURCE_SELECTOR_LIMIT);
}

function isStationFocusedCheck(opts) {
  const haystack = [
    ...(opts.selectors || []),
    ...(opts.functions || []),
    ...(opts.stationProbes || []).map((probe) => `station:${probe}`),
  ].join('\n').toLowerCase();
  return haystack.includes('station-status')
    || haystack.includes('station-hud-canvas')
    || haystack.includes('station-dock')
    || haystack.includes('station');
}

function addDiagnosticSelector(selectors, selector) {
  const compact = String(selector || '').trim();
  if (compact && !selectors.includes(compact)) {
    selectors.push(compact);
  }
}

function diagnosticSelectorsFromWaitFunctions(functions) {
  const selectors = [];
  for (const source of functions) {
    for (const call of extractDomLookupLiteralArgs(source)) {
      if (call.name === 'getElementById') {
        addDiagnosticSelector(selectors, idDiagnosticSelector(call.value));
      } else if (call.name === 'querySelector' || call.name === 'querySelectorAll') {
        addDiagnosticSelector(selectors, call.value);
      }
      if (selectors.length >= DIAGNOSTIC_SOURCE_SELECTOR_LIMIT) {
        return selectors;
      }
    }
  }
  return selectors;
}

function extractDomLookupLiteralArgs(source) {
  const text = String(source || '');
  const names = ['getElementById', 'querySelector', 'querySelectorAll'];
  const calls = [];
  for (const name of names) {
    let searchFrom = 0;
    while (searchFrom < text.length) {
      const index = text.indexOf(name, searchFrom);
      if (index === -1) {
        break;
      }
      searchFrom = index + name.length;
      if (isIdentifierChar(text[index - 1]) || isIdentifierChar(text[index + name.length])) {
        continue;
      }
      let pos = index + name.length;
      while (/\s/.test(text[pos] || '')) pos += 1;
      if (text[pos] !== '(') {
        continue;
      }
      pos += 1;
      while (/\s/.test(text[pos] || '')) pos += 1;
      const quoteChar = text[pos];
      if (quoteChar !== '\'' && quoteChar !== '"' && quoteChar !== '`') {
        continue;
      }
      const parsed = readJsStringLiteral(text, pos);
      if (!parsed) {
        continue;
      }
      if (quoteChar === '`' && parsed.raw.includes('${')) {
        continue;
      }
      calls.push({ name, value: parsed.value, pos: index });
      searchFrom = parsed.end;
    }
  }
  calls.sort((a, b) => a.pos - b.pos);
  return calls;
}

function readJsStringLiteral(text, start) {
  const quoteChar = text[start];
  let value = '';
  let raw = '';
  for (let i = start + 1; i < text.length; i += 1) {
    const ch = text[i];
    if (ch === quoteChar) {
      return { value, raw, end: i + 1 };
    }
    if (ch === '\\') {
      const next = text[i + 1];
      if (next === undefined) {
        return undefined;
      }
      raw += ch + next;
      value += next;
      i += 1;
    } else {
      raw += ch;
      value += ch;
    }
  }
  return undefined;
}

function isIdentifierChar(ch) {
  return Boolean(ch && /[A-Za-z0-9_$]/.test(ch));
}

function idDiagnosticSelector(id) {
  const text = String(id || '');
  return /^[A-Za-z_][A-Za-z0-9_-]*$/.test(text)
    ? `#${text}`
    : `[id=${quoteCssString(text)}]`;
}

function quoteCssString(value) {
  return `"${String(value).replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`;
}

function quote(value) {
  return JSON.stringify(String(value));
}

function truncate(value, limit) {
  const text = String(value);
  return text.length <= limit ? text : `${text.slice(0, limit - 1)}...`;
}

function truncateMiddle(value, limit) {
  const text = String(value || '');
  if (text.length <= limit) {
    return text;
  }
  let marker = ' ...[truncated]... ';
  let available = limit - marker.length;
  if (available <= 0) {
    return text.slice(0, limit);
  }
  let head = Math.ceil(available * 0.6);
  let tail = available - head;
  let omitted = text.length - head - tail;
  marker = ` ...[${omitted} chars omitted]... `;
  available = limit - marker.length;
  if (available <= 0) {
    return text.slice(0, limit);
  }
  head = Math.ceil(available * 0.6);
  tail = available - head;
  omitted = text.length - head - tail;
  marker = ` ...[${omitted} chars omitted]... `;
  return `${text.slice(0, head)}${marker}${text.slice(text.length - tail)}`;
}

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function waitForExit(child, timeoutMs) {
  if (child.exitCode !== null) {
    return Promise.resolve();
  }
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('process exit timeout')), timeoutMs);
    child.once('exit', () => {
      clearTimeout(timer);
      resolve();
    });
  });
}

async function runSelfTest() {
  const parsed = parseArgs(
    [
      '--port',
      '1234',
      '--path',
      'app',
      '--selector',
      '#root',
      '--wait-for-function',
      '() => true',
      '--station-probe',
      'rendered-surface',
      '--station-probe=dock-hidden',
      '--station-probe',
      'webgpu',
      '--timeout=2500',
      '--log-lines',
      '3',
      '--diagnostics',
      '--enable-gpu',
      '--browser-arg=--ozone-platform=x11',
    ],
    {},
  );
  assert.strictEqual(parsed.url, 'http://127.0.0.1:1234/app');
  assert.deepStrictEqual(parsed.selectors, ['#root']);
  assert.deepStrictEqual(parsed.functions, ['() => true']);
  assert.deepStrictEqual(parsed.stationProbes, ['rendered', 'dock-hidden', 'webgpu']);
  assert.strictEqual(parsed.timeoutMs, 2500);
  assert.strictEqual(parsed.logLines, 3);
  assert.strictEqual(parsed.diagnostics, true);
  assert.strictEqual(parsed.enableGpu, true);
  assert.deepStrictEqual(parsed.browserArgs, ['--ozone-platform=x11']);
  assert.ok(browserArgs('/tmp/profile', parseArgs([], {})).includes('--disable-gpu'));
  const gpuBrowserArgs = browserArgs('/tmp/profile', parsed);
  assert.ok(!gpuBrowserArgs.includes('--disable-gpu'));
  assert.ok(gpuBrowserArgs.includes('--ozone-platform=x11'));
  assert.ok(gpuBrowserArgs.includes('--enable-unsafe-webgpu'));
  const impliedGpuParsed = parseArgs([
    '--headed',
    '--station-probe',
    'rendered',
    '--station-probe',
    'webgpu',
  ], {});
  assert.strictEqual(impliedGpuParsed.enableGpu, false);
  const impliedGpuBrowserArgs = browserArgs('/tmp/profile', impliedGpuParsed);
  assert.ok(!impliedGpuBrowserArgs.includes('--disable-gpu'));
  assert.ok(impliedGpuBrowserArgs.includes('--enable-unsafe-webgpu'));
  const displayStartupLog = new BoundedLog(4);
  displayStartupLog.push('browser.stderr', '[123:123:0607/230000.000000:ERROR:ui/ozone/platform/x11/ozone_platform_x11.cc:257] Missing X server or $DISPLAY');
  displayStartupLog.push('browser.stderr', '[123:123:0607/230000.000001:ERROR:ui/aura/env.cc:246] The platform failed to initialize.  Exiting.');
  assert.ok(chromiumCdpReadinessFailure(displayStartupLog, 'Chromium exited before CDP was ready').includes('headed Linux Chromium could not reach the graphical display'));
  assert.ok(
    validationFailureNextStep({
      failureKind: 'harness',
      reason: chromiumCdpReadinessFailure(displayStartupLog, 'Chromium exited before CDP was ready'),
    }).includes('systemctl --user show-environment'),
  );
  const parsedSystemdEnv = parseSystemdUserEnvironment([
    'DISPLAY=:0',
    'WAYLAND_DISPLAY=wayland-0',
    'DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus',
    'IGNORED=value',
    'XAUTHORITY=/run/user/1000/.mutter-Xwaylandauth.TEIUP3',
    'DESKTOP_SESSION=gnome=wayland',
  ].join('\n'));
  assert.deepStrictEqual(parsedSystemdEnv, {
    DISPLAY: ':0',
    WAYLAND_DISPLAY: 'wayland-0',
    DBUS_SESSION_BUS_ADDRESS: 'unix:path=/run/user/1000/bus',
    XAUTHORITY: '/run/user/1000/.mutter-Xwaylandauth.TEIUP3',
    DESKTOP_SESSION: 'gnome=wayland',
  });
  const headedEnv = resolveLaunchEnvironment(
    parseArgs(['--headed'], {}),
    { PATH: '/usr/bin' },
    () => ({
      DISPLAY: ':0',
      WAYLAND_DISPLAY: 'wayland-0',
      XDG_RUNTIME_DIR: '/run/user/1000',
      DBUS_SESSION_BUS_ADDRESS: 'unix:path=/run/user/1000/bus',
      XAUTHORITY: '/run/user/1000/.mutter-Xwaylandauth.TEIUP3',
      XDG_SESSION_TYPE: 'wayland',
    }),
    'linux',
  );
  assert.strictEqual(headedEnv.env.DISPLAY, ':0');
  assert.strictEqual(headedEnv.env.WAYLAND_DISPLAY, 'wayland-0');
  assert.strictEqual(headedEnv.env.XDG_RUNTIME_DIR, '/run/user/1000');
  assert.ok(headedEnv.notes[0].includes('systemd user manager'));
  assert.throws(
    () => resolveLaunchEnvironment(parseArgs(['--headed'], {}), { PATH: '/usr/bin' }, () => ({}), 'linux'),
    /headed browser validation requires DISPLAY or WAYLAND_DISPLAY/,
  );
  assert.strictEqual(staticScriptsOnly(parseArgs(['--check-static-scripts'], {
    INTENDANT_MCP_URL: 'http://127.0.0.1:7777/mcp',
  })), true);
  assert.strictEqual(staticScriptsOnly(parseArgs([
    '--check-static-scripts',
    '--port',
    '7777',
  ], {})), false);
  assert.strictEqual(staticScriptsOnly(parseArgs([
    '--check-static-scripts',
    '--station-probe',
    'canvas',
  ], {})), false);
  assert.throws(
    () => parseArgs(['--station-probe', 'everything'], {}),
    /unknown Station probe/,
  );
  const launchParsed = parseArgs(
    [
      '--launch-dashboard',
      '--url',
      'http://localhost:8893/app',
      '--dashboard-arg',
      '--no-presence',
      '--dashboard-timeout=5000',
    ],
    {},
  );
  assert.strictEqual(launchParsed.launchDashboard, true);
  assert.strictEqual(dashboardLaunchPort(launchParsed), 8893);
  assert.deepStrictEqual(dashboardLaunchArgs(8893, launchParsed.dashboardArgs), [
    '--web',
    '8893',
    '--no-tui',
    '--no-tls',
    '--no-presence',
  ]);
  assert.deepStrictEqual(dashboardLaunchArgs(8893, [], 'https:'), [
    '--web',
    '8893',
    '--no-tui',
  ]);
  assert.deepStrictEqual(dashboardLaunchArgs(8893, ['--tls'], 'http:'), [
    '--web',
    '8893',
    '--no-tui',
    '--tls',
  ]);
  assert.strictEqual(dashboardReadyUrl(launchParsed), 'http://localhost:8893/');
  assert.deepStrictEqual(requiredDashboardHelpFlags(dashboardLaunchArgs(8893)), [
    '--web',
    '--no-tui',
    '--no-tls',
  ]);
  assert.throws(
    () => assertDashboardBinarySupportsLaunchArgs(process.execPath, dashboardLaunchArgs(8893)),
    /does not advertise .*--no-tls/,
  );
  await withTempDashboardBinaryTree(async ({ root, binary, touch }) => {
    touch('Cargo.toml', 1000);
    touch(path.join('src', 'main.rs'), 3000);
    touch(path.join('target', 'release', process.platform === 'win32' ? 'intendant.exe' : 'intendant'), 1500);
    assert.throws(
      () => resolveDashboardBinary(undefined, {}, root),
      /refusing stale dashboard binary.*cargo build --release/,
    );
    touch(path.join('target', 'release', process.platform === 'win32' ? 'intendant.exe' : 'intendant'), 5000);
    assert.strictEqual(resolveDashboardBinary(undefined, {}, root), binary);
    if (process.platform !== 'win32') {
      assertDashboardBinarySupportsLaunchArgs(binary, dashboardLaunchArgs(8893));
    }
  });
  assert.throws(
    () => parseArgs(['--launch-dashboard', '--port', String(PROTECTED_DASHBOARD_PORT)], {}),
    /refuses protected port/,
  );
  assert.throws(
    () => parseArgs(['--launch-dashboard', '--url', 'http://example.com:8893/'], {}),
    /loopback/,
  );
  await withLoopbackServer(async (port) => {
    await assert.rejects(
      () => assertDashboardPortAvailable(port, `http://127.0.0.1:${port}/`),
      /already accepting connections/,
    );
  });
  assert.strictEqual(
    dashboardUrlFromMcpUrl('http://localhost:7777/mcp?managed_context=managed'),
    'http://localhost:7777/',
  );
  const inlineScripts = extractInlineJavaScript(`
    <script>const classicOk = 1;</script>
    <script type="module">import missing from './missing.js'; const moduleOk = missing;</script>
    <script src="/external.js"></script>
    <script type="application/json">{"ignored": true}</script>
  `);
  assert.strictEqual(inlineScripts.length, 2);
  assert.strictEqual(inlineScripts[0].goal, 'classic');
  assert.strictEqual(inlineScripts[1].goal, 'module');
  checkClassicScriptSyntax(inlineScripts[0].source, 'self-test-classic');
  checkModuleSyntax(inlineScripts[1].source, 'self-test-module');
  checkModuleSyntax(`${'void 0;\n'.repeat(100000)}export const ok = true;`, 'self-test-large-module');
  assert.throws(
    () => checkClassicScriptSyntax(inlineScripts[1].source, 'self-test-classic-import'),
    /Cannot use import statement|Unexpected identifier|import declarations/i,
  );
  assert.throws(
    () => checkModuleSyntax('import x from "./missing.js"; const broken = ;', 'self-test-module-broken'),
    /module inline script syntax check failed.*(SyntaxError|Unexpected token)/,
  );
  assert.ok(waitFunctionExpression('document.body').includes('typeof candidate'));
  assert.ok(stationProbeExpression('rendered').includes('collectStationProbe'));
  assert.strictEqual(summarizeWaitValue(false), 'false');
  assert.ok(waitFailureSuffix('boom', 'false').includes('last value: false'));
  assert.strictEqual(
    validationFailureKind('wait-for-function did not become truthy (last value: false)'),
    'assertion',
  );
  assert.strictEqual(
    validationFailureKind('station probe rendered did not pass (last value: {"failureKind":"renderer"})'),
    'renderer',
  );
  assert.ok(pageDiagnosticsSource().includes('selectorMatches'));
  assert.ok(pageDiagnosticsSource().includes('dataset'));
  assert.ok(stationDiagnosticsSource().includes('station-hud-canvas'));
  const fakeDock = {
    classList: { contains: (name) => name === 'hidden' },
    dataset: { kind: 'controls' },
  };
  const fakeDocument = {
    getElementById: (id) => (id === 'station-dock' ? fakeDock : null),
    querySelector: (selector) => (
      selector === '#station-dock-nav [data-station-dock-nav="system:controls"]'
        ? { dataset: { stationDockNav: 'system:controls' } }
        : null
    ),
  };
  const stationProbe = vm.runInNewContext(`(${stationProbeSource()})`, {
    document: fakeDocument,
    stationRendererLabel: () => 'WebGPU',
    stationWebgpuStatusLabel: () => 'active',
  });
  const stationProbeDiagnostics = {
    statusFound: true,
    statusText: 'station hosts=1 agents=1 renderer=WebGPU webgpu=active',
    canvasFound: true,
    canvas: {
      attrWidth: 640,
      attrHeight: 360,
      clientWidth: 320,
      clientHeight: 180,
      rectWidth: 320,
      rectHeight: 180,
    },
    pixels: {
      litCount: 4,
      total: 144,
    },
  };
  assert.strictEqual(stationProbe('dock-hidden', stationProbeDiagnostics).ok, true);
  assert.strictEqual(stationProbe('rendered', stationProbeDiagnostics).ok, true);
  assert.strictEqual(stationProbe('webgpu', stationProbeDiagnostics).ok, true);
  const unlitProbe = stationProbe('rendered', {
    ...stationProbeDiagnostics,
    pixels: { litCount: 0, total: 144 },
  });
  assert.strictEqual(unlitProbe.ok, false);
  assert.strictEqual(unlitProbe.failureKind, 'renderer');
  assert.deepStrictEqual(
    stationConsoleWarnings([
      '[console.log] Station ordinary log',
      '[console.warn] Station WebGPU unavailable; using DOM fallback',
      '[console.warning] Station canvas alpha sample failed',
      '[console.warn] unrelated warning',
      '[log.warning] canvas fallback path selected',
    ]),
    [
      '[console.warn] Station WebGPU unavailable; using DOM fallback',
      '[console.warning] Station canvas alpha sample failed',
      '[log.warning] canvas fallback path selected',
    ],
  );
  assert.strictEqual(shouldCollectFailureDiagnostics({ diagnostics: true }, new Error('boom')), true);
  assert.strictEqual(
    shouldCollectFailureDiagnostics({ diagnostics: false }, new Error('wait-for-function did not become truthy')),
    true,
  );
  assert.strictEqual(
    shouldCollectFailureDiagnostics({ diagnostics: false }, new Error('navigation failed: nope')),
    false,
  );
  assert.deepStrictEqual(
    failureDiagnosticSelectors({
      selectors: ['#station-status'],
      functions: [
        '() => document.getElementById("station-dock")?.textContent.includes("Controls") && document.querySelector("[data-station-dock-nav=\'system:controls\']")',
      ],
    }),
    ['#station-status', '#station-dock', '[data-station-dock-nav=\'system:controls\']'],
  );
  assert.deepStrictEqual(
    failureDiagnosticSelectors({
      selectors: [],
      functions: ['() => document.getElementById("has:colon")'],
      stationProbes: [],
    }),
    ['[id="has:colon"]'],
  );
  assert.deepStrictEqual(
    failureDiagnosticSelectors({
      selectors: [],
      functions: [],
      stationProbes: ['rendered'],
    }),
    ['#station-status', '#station-hud-canvas', '#station-dock', '#station-dock-nav [data-station-dock-nav="system:controls"]'],
  );
  assert.deepStrictEqual(formatDiagnostics(undefined), []);
  assert.deepStrictEqual(formatDiagnostics({ error: 'boom' }), ['diagnostics error="boom"']);
  assert.ok(
    formatDiagnostics({
      readyState: 'complete',
      title: 'Dashboard',
      location: 'http://127.0.0.1:1234/',
      selectorMatches: [{ selector: '#root', count: 0, first: '' }],
    }).some((line) => line.includes('selector="#root" count=0')),
  );
  const compactedFailure = compactResultForOutput(
    { diagnostics: true },
    {
      status: 'fail',
      reason: `selector not found: ${'x'.repeat(1200)}`,
      diagnostics: {
        readyState: 'complete',
        title: 'Dashboard',
        location: 'http://127.0.0.1:1234/',
        bodyText: 'body '.repeat(300),
        controls: Array.from({ length: 12 }, (_, idx) => `button ${idx} ${'y'.repeat(120)}`),
        selectorMatches: Array.from({ length: 10 }, (_, idx) => ({
          selector: `.target-${idx}-${'z'.repeat(500)}`,
          count: 0,
          first: '',
        })),
      },
    },
  );
  assert.ok(compactedFailure.reason.includes('chars omitted'));
  assert.ok(compactedFailure.next.includes('avoid further broad selector/source dumps'));
  assert.ok(compactedFailure.diagnostics.bodyText.includes('chars omitted'));
  assert.strictEqual(compactedFailure.diagnostics.controls.length, DIAGNOSTIC_LIST_LIMIT);
  assert.strictEqual(compactedFailure.diagnostics.controlsOmitted, 4);
  assert.strictEqual(
    compactedFailure.diagnostics.selectorMatches.length,
    DIAGNOSTIC_SELECTOR_MATCH_LIMIT,
  );
  assert.strictEqual(compactedFailure.diagnostics.selectorMatchesOmitted, 2);
  assert.ok(compactedFailure.diagnostics.selectorMatches[0].selector.includes('chars omitted'));
  const compactedAutoDiagnosticFailure = compactResultForOutput(
    {},
    {
      status: 'fail',
      reason: 'wait-for-function did not become truthy',
      failureKind: 'assertion',
      diagnosticsAuto: true,
      diagnostics: {
        readyState: 'complete',
        title: 'Dashboard',
        location: 'http://127.0.0.1:1234/',
        selectorMatches: [{ selector: '#station-dock', count: 1, first: 'aside#station-dock.hidden {kind=controls}' }],
      },
    },
  );
  assert.strictEqual(compactedAutoDiagnosticFailure.diagnosticsAuto, true);
  assert.strictEqual(compactedAutoDiagnosticFailure.failureKind, 'assertion');
  assert.ok(compactedAutoDiagnosticFailure.next.includes('avoid further broad selector/source dumps'));
  const longStationStatus = `Station online ${'status-detail '.repeat(80)}ready`;
  const compactedStationFailure = compactResultForOutput(
    {
      diagnostics: false,
      selectors: ['#station-status'],
      functions: [],
    },
    {
      status: 'fail',
      reason: 'selector not found: #station-ready',
      diagnosticsAuto: true,
      diagnostics: {
        readyState: 'complete',
        title: 'Dashboard',
        location: 'http://127.0.0.1:1234/',
        controls: ['button#generic-control "Launch"', 'button#another-control "Stop"'],
        station: {
          statusFound: true,
          statusText: longStationStatus,
          canvasFound: true,
          canvas: {
            attrWidth: 640,
            attrHeight: 360,
            clientWidth: 320,
            clientHeight: 180,
            rectWidth: 320,
            rectHeight: 180,
            devicePixelRatio: 2,
          },
          pixels: {
            sampleWidth: 12,
            sampleHeight: 12,
            litCount: 7,
            total: 144,
            samples: [{ x: 1, y: 2, rgba: [3, 4, 5, 255] }],
          },
          warnings: ['[console.warn] Station WebGPU unavailable; using DOM fallback'],
        },
      },
    },
  );
  assert.strictEqual(compactedStationFailure.diagnostics.controls, undefined);
  assert.strictEqual(compactedStationFailure.diagnostics.station.statusText, longStationStatus);
  const stationLines = formatDiagnostics(compactedStationFailure.diagnostics);
  assert.ok(stationLines.some((line) => line.includes(`statusText=${quote(longStationStatus)}`)));
  assert.ok(stationLines.some((line) => line.includes('station canvasFound=true attr=640x360 client=320x180')));
  assert.ok(stationLines.some((line) => line.includes('station pixels lit=7/144')));
  assert.ok(stationLines.some((line) => line.includes('station warning=')));
  const explicitStationFailure = compactResultForOutput(
    {
      diagnostics: true,
      selectors: ['#station-status'],
      functions: [],
    },
    {
      status: 'fail',
      reason: 'selector not found: #station-ready',
      diagnostics: {
        readyState: 'complete',
        title: 'Dashboard',
        location: 'http://127.0.0.1:1234/',
        controls: ['button#generic-control "Launch"'],
        station: {
          statusFound: false,
          statusText: '',
          canvasFound: false,
        },
      },
    },
  );
  assert.deepStrictEqual(explicitStationFailure.diagnostics.controls, ['button#generic-control "Launch"']);
  const log = new BoundedLog(2);
  log.push('a', 'first');
  log.push('b', 'second');
  log.push('c', 'third');
  assert.deepStrictEqual(log.excerpt(3), ['[b] second', '[c] third']);
  console.log('PASS dashboard-validation-self-test');
}

async function withLoopbackServer(callback) {
  const server = net.createServer((socket) => {
    socket.on('error', () => {});
    socket.end('ok');
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  try {
    const address = server.address();
    await callback(address.port);
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
}

async function withTempDashboardBinaryTree(callback) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'validate-dashboard-binary-'));
  const exeName = process.platform === 'win32' ? 'intendant.exe' : 'intendant';
  const binary = path.join(root, 'target', 'release', exeName);
  const touch = (rel, seconds) => {
    const full = path.join(root, rel);
    fs.mkdirSync(path.dirname(full), { recursive: true });
    if (!fs.existsSync(full)) {
      fs.writeFileSync(full, rel === path.join('target', 'release', exeName)
        ? dashboardSelfTestBinarySource()
        : 'self-test\n');
    }
    if (rel === path.join('target', 'release', exeName) && process.platform !== 'win32') {
      fs.chmodSync(full, 0o755);
    }
    const date = new Date(seconds * 1000);
    fs.utimesSync(full, date, date);
  };
  try {
    await callback({ root, binary, touch });
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

function dashboardSelfTestBinarySource() {
  if (process.platform === 'win32') {
    return '@echo off\r\necho --web --no-tui --no-tls\r\n';
  }
  return '#!/bin/sh\necho "--web --no-tui --no-tls"\n';
}

if (require.main === module) {
  main().catch((error) => {
    console.error(`FAIL dashboard-validation reason=${quote(error.message || String(error))}`);
    process.exitCode = 1;
  });
}
