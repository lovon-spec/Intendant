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
const DEFAULT_CDP_COMMAND_TIMEOUT_MS = 8000;
const DEFAULT_DASHBOARD_TIMEOUT_MS = 15000;
const SCREENSHOT_LIVENESS_TIMEOUT_MS = 1500;
const FAILURE_SCREENSHOT_TIMEOUT_MS = 3000;
const FAILURE_SCREENSHOT_NAVIGATION_TIMEOUT_MS = 5000;
const FAILURE_SCREENSHOT_SETTLE_MS = 300;
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
const DEFAULT_STATION_MIN_FPS = 24;
const DEFAULT_STATION_MAX_FRAME_GAP_MS = 40;
const STATION_SMOOTH_SAMPLE_MS = 2000;
const STATION_SMOOTH_HARD_GAP_LIMIT_MS = 250;
const STATION_SMOOTH_EVALUATE_TIMEOUT_BUFFER_MS = 4000;
const STATION_ACTIVATE_SETTLE_BUDGET_MS = 1000;
const STATION_PERF_EVAL_SETTLE_MS = 1500;
const STATION_PERF_EVAL_DEFAULT_TARGETS = ['system:activity', 'system:controls', 'system:view'];
const STATION_DEBUG_JSON_REQUIRED_KEYS = ['fps', 'renderer', 'hosts'];
const STATION_DEBUG_JSON_LIST_LIMIT = 16;
const PROTECTED_DASHBOARD_PORT = 8765;
const STALE_BINARY_MTIME_SLOP_MS = 1000;
const DASHBOARD_BINARY_INPUT_FILES = ['Cargo.toml', 'Cargo.lock'];
const DASHBOARD_BINARY_INPUT_DIRS = ['src', 'crates', 'static'];
const STATIC_IDENTITY_ASSETS = [
  { urlPath: '/app', file: path.join('static', 'app.html'), kind: 'text', normalize: normalizeAppHtmlForIdentity },
  { urlPath: '/wasm-web/presence_web.js', file: path.join('static', 'wasm-web', 'presence_web.js'), kind: 'text' },
  { urlPath: '/wasm-web/presence_web_bg.wasm', file: path.join('static', 'wasm-web', 'presence_web_bg.wasm'), kind: 'binary' },
  { urlPath: '/wasm-station/station_web.js', file: path.join('static', 'wasm-station', 'station_web.js'), kind: 'text' },
  { urlPath: '/wasm-station/station_web_bg.wasm', file: path.join('static', 'wasm-station', 'station_web_bg.wasm'), kind: 'binary' },
  { urlPath: '/three.module.min.js', file: path.join('static', 'three.module.min.js'), kind: 'text' },
  { urlPath: '/audio-processor.js', file: path.join('static', 'audio-processor.js'), kind: 'text' },
  { urlPath: '/icon-128.png', file: path.join('static', 'icon-128.png'), kind: 'binary' },
];
const APP_HTML_CACHEBUSTED_ASSET_PATHS = [
  '/wasm-web/presence_web.js',
  '/wasm-web/presence_web_bg.wasm',
  '/wasm-station/station_web.js',
  '/wasm-station/station_web_bg.wasm',
  '/three.module.min.js',
  '/icon-128.png',
];

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
  --station-probe NAME        Named Station probe: status, canvas, rendered, dock-hidden, webgpu,
                              fps, smooth, debug-json. Repeatable; comma lists are accepted
                              (e.g. --station-probe rendered,webgpu,fps,smooth).
                              dock-hidden passes when the legacy #station-dock is absent or hidden.
                              fps reads the renderer's trailing-2s presented-frame rate from debug_state.
                              smooth samples page frame pacing via requestAnimationFrame for ~${STATION_SMOOTH_SAMPLE_MS}ms
                              and fails when the p95 frame gap exceeds --station-max-frame-gap or any
                              gap exceeds ${STATION_SMOOTH_HARD_GAP_LIMIT_MS}ms; reports fps/p50/p95/worst. Catches main-thread
                              stalls the Station fps figure can miss.
                              debug-json passes when station.debug_json() parses and contains
                              fps/renderer/hosts keys; on builds without the export it soft-passes
                              as unsupported unless --require-debug-json is set.
  --station-min-fps N         Minimum fps for the fps probe and --station-perf-eval (default: ${DEFAULT_STATION_MIN_FPS})
  --station-max-frame-gap MS  p95 frame-gap budget for the smooth probe and --station-perf-eval
                              (default: ${DEFAULT_STATION_MAX_FRAME_GAP_MS})
  --require-debug-json        Fail the debug-json probe when station.debug_json() is absent instead
                              of soft-passing
  --station-activate NAME     Activate a Station target programmatically via station.activate(NAME)
                              and verify via debug_json selectedId/status text; repeatable. Faster and
                              more robust than the click-based interaction probe; requires a build
                              that exports station.activate
  --station-perf-eval         Scripted perf sequence: settle, idle smooth sample, activate three
                              targets (or each --station-activate NAME) timing each, smooth sample
                              again; emits one JSON report and fails on threshold violations

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
  --screenshot PATH          Capture a PNG screenshot after validation/probes
  --station-interaction-probe
                             Click/activate rendered Station hotspots and report latency
  --station-hotspot-probe    Alias for --station-interaction-probe
  --keep-artifacts           Preserve the Chromium profile/temp artifact directory
  --keep-browser             Leave Chromium running for manual follow-up; implies --keep-artifacts
  --launch-dashboard         Launch a temporary Intendant dashboard and stop it afterward
  --hold-dashboard           With --launch-dashboard, keep the temporary dashboard alive until interrupted
  --dashboard-binary PATH    Intendant binary for --launch-dashboard (default: $INTENDANT or a fresh target/{release,debug}/intendant)
  --dashboard-arg ARG        Extra arg appended to the launched dashboard command; repeatable
  --dashboard-timeout MS     Temporary dashboard readiness timeout (default: ${DEFAULT_DASHBOARD_TIMEOUT_MS})
  --require-current-static   Fail unless served embedded dashboard static assets match this worktree
  --require-station-state    Fail if Station sessions/events/managed/peers are all empty
  --require-ai-provider-session
                             Fail unless Station exposes a real session with non-placeholder provider/model
  --require-external-agent BACKEND
                             Fail unless that real Station session is backed by external BACKEND (e.g. codex)
  --require-managed-context-state
                             Fail unless Station exposes active managed/context session state
  --allow-empty-station-state
                             Explicitly allow empty Station state with --require-station-state
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
binaries are rejected before launch. Rebuild or use $INTENDANT/--dashboard-binary.
Use --hold-dashboard for separate headed CU/browser E2E: keep this command
foregrounded, run the CU steps against the printed URL, then interrupt it for
helper-owned cleanup.`);
}

function parseArgs(argv, env = process.env) {
  const opts = {
    host: '127.0.0.1',
    path: '/',
    selectors: [],
    functions: [],
    stationProbes: [],
    stationMinFps: DEFAULT_STATION_MIN_FPS,
    stationMaxFrameGapMs: DEFAULT_STATION_MAX_FRAME_GAP_MS,
    requireDebugJson: false,
    stationActivateTargets: [],
    stationPerfEval: false,
    timeoutMs: DEFAULT_TIMEOUT_MS,
    cdpTimeoutMs: DEFAULT_CDP_TIMEOUT_MS,
    logLines: DEFAULT_LOG_LINES,
    diagnostics: false,
    screenshotPath: undefined,
    stationInteractionProbe: false,
    keepArtifacts: false,
    keepBrowser: false,
    launchDashboard: false,
    holdDashboard: false,
    dashboardArgs: [],
    dashboardTimeoutMs: DEFAULT_DASHBOARD_TIMEOUT_MS,
    headless: true,
    enableGpu: false,
    browserArgs: [],
    noSandbox: true,
    json: false,
    selfTest: false,
    requireCurrentStatic: false,
    requireStationState: false,
    requireAiProviderSession: false,
    requireManagedContextState: false,
    allowEmptyStationState: false,
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
    } else if (arg === '--station-min-fps') {
      opts.stationMinFps = parsePositiveInt(readValue(), '--station-min-fps');
    } else if (arg.startsWith('--station-min-fps=')) {
      opts.stationMinFps = parsePositiveInt(arg.slice('--station-min-fps='.length), '--station-min-fps');
    } else if (arg === '--station-max-frame-gap') {
      opts.stationMaxFrameGapMs = parsePositiveInt(readValue(), '--station-max-frame-gap');
    } else if (arg.startsWith('--station-max-frame-gap=')) {
      opts.stationMaxFrameGapMs = parsePositiveInt(arg.slice('--station-max-frame-gap='.length), '--station-max-frame-gap');
    } else if (arg === '--require-debug-json') {
      opts.requireDebugJson = true;
    } else if (arg === '--station-activate') {
      opts.stationActivateTargets.push(normalizeStationActivateTarget(readValue()));
    } else if (arg.startsWith('--station-activate=')) {
      opts.stationActivateTargets.push(normalizeStationActivateTarget(arg.slice('--station-activate='.length)));
    } else if (arg === '--station-perf-eval') {
      opts.stationPerfEval = true;
    } else if (arg === '--station-probe') {
      opts.stationProbes.push(...parseStationProbeArgument(readValue()));
    } else if (arg.startsWith('--station-probe=')) {
      opts.stationProbes.push(...parseStationProbeArgument(arg.slice('--station-probe='.length)));
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
    } else if (arg === '--screenshot') {
      opts.screenshotPath = readValue();
    } else if (arg.startsWith('--screenshot=')) {
      opts.screenshotPath = arg.slice('--screenshot='.length);
    } else if (arg === '--station-interaction-probe' || arg === '--station-hotspot-probe') {
      opts.stationInteractionProbe = true;
    } else if (arg === '--keep-artifacts') {
      opts.keepArtifacts = true;
    } else if (arg === '--keep-browser') {
      opts.keepBrowser = true;
      opts.keepArtifacts = true;
    } else if (arg === '--launch-dashboard') {
      opts.launchDashboard = true;
    } else if (arg === '--hold-dashboard') {
      opts.launchDashboard = true;
      opts.holdDashboard = true;
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
    } else if (arg === '--require-current-static') {
      opts.requireCurrentStatic = true;
    } else if (arg === '--require-station-state') {
      opts.requireStationState = true;
    } else if (arg === '--require-ai-provider-session') {
      opts.requireAiProviderSession = true;
    } else if (arg === '--require-external-agent') {
      opts.requireExternalAgent = normalizeExternalAgentRequirement(readValue());
    } else if (arg.startsWith('--require-external-agent=')) {
      opts.requireExternalAgent = normalizeExternalAgentRequirement(arg.slice('--require-external-agent='.length));
    } else if (arg === '--require-managed-context-state') {
      opts.requireManagedContextState = true;
    } else if (arg === '--allow-empty-station-state') {
      opts.allowEmptyStationState = true;
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
  if (opts.screenshotPath) {
    opts.screenshotPath = path.resolve(opts.screenshotPath);
  }
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
    ['hidden-dock', 'dock-hidden'],
    ['gpu', 'webgpu'],
    ['framerate', 'fps'],
    ['frame-pacing', 'smooth'],
    ['pacing', 'smooth'],
    ['debugjson', 'debug-json'],
  ]);
  const normalized = aliases.get(value) || value;
  const allowed = new Set(['status', 'canvas', 'rendered', 'dock-hidden', 'webgpu', 'fps', 'smooth', 'debug-json']);
  if (!allowed.has(normalized)) {
    throw new Error(`unknown Station probe '${raw}'; expected one of ${Array.from(allowed).join(', ')}`);
  }
  return normalized;
}

function parseStationProbeArgument(raw) {
  const names = String(raw || '')
    .split(',')
    .map((name) => name.trim())
    .filter(Boolean)
    .map(normalizeStationProbeName);
  if (!names.length) {
    throw new Error('--station-probe requires at least one probe name');
  }
  return names;
}

function normalizeStationActivateTarget(raw) {
  const value = String(raw || '').trim();
  if (!value) {
    throw new Error('--station-activate requires a target name');
  }
  return value;
}

function normalizeExternalAgentRequirement(raw) {
  const value = String(raw || '').trim().toLowerCase().replace(/[_\s]+/g, '-');
  if (!value) {
    throw new Error('--require-external-agent requires a backend name');
  }
  return value;
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
  let launchEnv;
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
    launchEnv = resolveLaunchEnvironment(opts);
    if (opts.launchDashboard) {
      dashboard = await TemporaryDashboard.launch(opts, launchEnv);
    }
    const staticIdentity = opts.requireCurrentStatic
      ? await validateCurrentStaticIdentity(opts.url, process.cwd())
      : undefined;
    if (opts.holdDashboard) {
      printDashboardHoldResult(opts, {
        status: 'pass',
        url: opts.url,
        readyUrl: dashboard.readyUrl,
        ms: Date.now() - started,
        pid: dashboard.child && dashboard.child.pid,
        command: [dashboard.executable, ...dashboard.args],
        staticIdentity,
      });
      const status = await dashboard.waitForExit();
      throw new Error(`temporary dashboard exited while holding (${status})`);
    }
    harness = await BrowserHarness.launch(opts, launchEnv);
    let validation = await harness.validate(opts);
    const artifacts = {};
    const harnessRetries = {};
    if (opts.screenshotPath) {
      const screenshot = await captureValidationScreenshotWithRetry(
        opts,
        launchEnv,
        harness,
        (nextHarness) => {
          harness = nextHarness;
        },
      );
      artifacts.screenshot = screenshot.path;
      if (screenshot.validation) {
        validation = screenshot.validation;
      }
      if (screenshot.retried) {
        harnessRetries.screenshotCapture = 1;
      }
    }
    if (opts.keepArtifacts || opts.keepBrowser) {
      artifacts.browserUserDataDir = harness.userDataDir;
    }
    const result = {
      status: 'pass',
      url: opts.url,
      ms: Date.now() - started,
      browser: harness.browserExecutable,
      websocket: harness.websocketKind,
      artifacts,
      selectors: opts.selectors.length,
      functions: opts.functions.length,
      stationProbes: opts.stationProbes.length,
      stationProbeReports: validation.stationProbeReports,
      stationInteraction: validation.stationInteraction,
      stationInteractions: validation.stationInteraction ? validation.stationInteraction.count : 0,
      stationActivation: validation.stationActivation,
      stationActivations: validation.stationActivation ? validation.stationActivation.count : 0,
      stationPerfEval: validation.stationPerfEval,
      stationState: validation.stationState,
      harnessRetries: Object.keys(harnessRetries).length ? harnessRetries : undefined,
      staticIdentity,
      staticScripts: staticScriptResult,
    };
    printResult(opts, result);
  } catch (error) {
    const harnessFallbacks = harness
      ? await captureStationRuntimeTimeoutEvidence(
          opts,
          launchEnv,
          harness,
          error,
          (nextHarness) => {
            harness = nextHarness;
          },
        )
      : undefined;
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
      artifacts: failureArtifacts(opts, harness),
      logs: collectFailureLogs(opts.logLines, dashboard, harness),
      stationPerfEval: error && error.stationPerfEval,
      diagnostics,
      diagnosticsAuto: Boolean(diagnostics && !opts.diagnostics),
      harnessFallbacks,
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
      && !opts.holdDashboard
      && opts.selectors.length === 0
      && opts.functions.length === 0
      && opts.stationProbes.length === 0
      && !opts.stationInteractionProbe
      && opts.stationActivateTargets.length === 0
      && !opts.stationPerfEval
      && !opts.requireStationState
      && !opts.requireAiProviderSession
      && !opts.requireExternalAgent
      && !opts.requireManagedContextState
      && !opts.screenshotPath
  );
}

function failureArtifacts(opts, harness) {
  const artifacts = {};
  if (opts.screenshotPath && fs.existsSync(opts.screenshotPath)) {
    artifacts.screenshot = opts.screenshotPath;
  }
  if (harness && (opts.keepArtifacts || opts.keepBrowser)) {
    artifacts.browserUserDataDir = harness.userDataDir;
  }
  return artifacts;
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

  async waitForExit() {
    await waitForProcessExit(this.child);
    return childExitStatus(this.child) || 'exited';
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
      `PASS dashboard-validation url=${quote(displayResult.url)} selectors=${displayResult.selectors} functions=${displayResult.functions} stationProbes=${displayResult.stationProbes || 0} stationInteractions=${displayResult.stationInteractions || 0} stationActivations=${displayResult.stationActivations || 0} ms=${displayResult.ms} websocket=${displayResult.websocket || 'unknown'}`,
    );
    for (const line of formatArtifactLines(displayResult.artifacts)) {
      console.log(`  ${line}`);
    }
    for (const line of formatStationProbeReportLines(displayResult.stationProbeReports)) {
      console.log(`  ${line}`);
    }
    if (displayResult.stationInteraction) {
      console.log(
        `  station interaction count=${displayResult.stationInteraction.count || 0} warmupLatencyMs=${displayResult.stationInteraction.warmupLatencyMs || 0} subsequentLatencyMs=${displayResult.stationInteraction.subsequentLatencyMs || 0} names=${quote((displayResult.stationInteraction.names || []).join(','))}`,
      );
    }
    if (displayResult.stationActivation) {
      console.log(formatStationActivationPass(displayResult.stationActivation));
    }
    if (displayResult.stationPerfEval) {
      console.log(formatStationPerfEvalLine(displayResult.stationPerfEval));
    }
    if (displayResult.stationState) {
      console.log(formatStationStatePass(displayResult.stationState));
    }
    if (displayResult.staticIdentity) {
      console.log(formatStaticIdentityPass(displayResult.staticIdentity));
    }
    if (displayResult.staticScripts) {
      console.log(formatStaticScriptPass(displayResult.staticScripts));
    }
    return;
  }
  console.error(
    `FAIL dashboard-validation url=${quote(displayResult.url)} kind=${quote(displayResult.failureKind || 'unknown')} reason=${quote(displayResult.reason)} ms=${displayResult.ms}`,
  );
  if (displayResult.stationPerfEval) {
    console.error(formatStationPerfEvalLine(displayResult.stationPerfEval));
  }
  for (const line of displayResult.logs || []) {
    console.error(`  ${line}`);
  }
  for (const line of formatArtifactLines(displayResult.artifacts)) {
    console.error(`  ${line}`);
  }
  for (const line of formatHarnessFallbackLines(displayResult.harnessFallbacks)) {
    console.error(`  ${line}`);
  }
  for (const line of formatDiagnostics(displayResult.diagnostics)) {
    console.error(`  ${line}`);
  }
  if (displayResult.next) {
    console.error(`  next=${quote(displayResult.next)}`);
  }
}

function printDashboardHoldResult(opts, result) {
  if (opts.json) {
    console.log(JSON.stringify({
      status: result.status,
      mode: 'dashboard-hold',
      url: result.url,
      readyUrl: result.readyUrl,
      ms: result.ms,
      pid: result.pid,
      command: result.command,
      staticIdentity: result.staticIdentity,
    }));
    return;
  }
  console.log(
    `PASS dashboard-hold url=${quote(result.url)} readyUrl=${quote(result.readyUrl)} pid=${result.pid || 'unknown'} ms=${result.ms}`,
  );
  console.log('Hold this command open while running CU/browser E2E; interrupt it to stop the temporary dashboard.');
}

function formatArtifactLines(artifacts) {
  if (!artifacts || typeof artifacts !== 'object') {
    return [];
  }
  const lines = [];
  if (artifacts.screenshot) {
    lines.push(`artifact screenshot=${quote(artifacts.screenshot)}`);
  }
  if (artifacts.browserUserDataDir) {
    lines.push(`artifact browserUserDataDir=${quote(artifacts.browserUserDataDir)}`);
  }
  return lines;
}

function formatHarnessFallbackLines(fallbacks) {
  if (!fallbacks || typeof fallbacks !== 'object') {
    return [];
  }
  const lines = [];
  if (fallbacks.stationRuntimeEvaluateTimeout) {
    const screenshot = fallbacks.screenshot || {};
    const reason = screenshot.reason ? ` reason=${quote(screenshot.reason)}` : '';
    lines.push(
      `harness fallback stationRuntimeEvaluateTimeout=true screenshot=${quote(screenshot.status || 'unknown')} mode=${quote(screenshot.mode || 'unknown')}${reason}`,
    );
  }
  return lines;
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

function formatStaticIdentityPass(result) {
  return `  static identity assets=${result.assets} hash=${quote(result.hash)}`;
}

function formatStationStatePass(result) {
  const counts = result.counts || {};
  const live = result.liveAgentSession && result.liveAgentSession.ok
    ? ` liveSession=${quote(result.liveAgentSession.sessionId || '')} provider=${quote(result.liveAgentSession.provider || '')} model=${quote(result.liveAgentSession.model || '')}`
    : '';
  const external = result.externalAgentSession && result.externalAgentSession.ok
    ? ` externalAgent=${quote(result.externalAgentSession.agent || result.externalAgentSession.required || '')} externalEvidence=${quote(result.externalAgentSession.evidence || '')}`
    : '';
  const managedContext = result.managedContextState && result.managedContextState.ok
    ? ` managedContext=${quote(result.managedContextState.sessionId || result.managedSessionId || 'active')}`
    : '';
  return `  station state sessions=${counts.sessions || 0} events=${counts.events || 0} managed=${counts.managed || 0} peers=${counts.peers || 0}${live}${external}${managedContext}`;
}

function formatStationProbeReportLines(reports) {
  if (!reports || typeof reports !== 'object') {
    return [];
  }
  const lines = [];
  if (reports.fps) {
    lines.push(`station probe=fps fps=${Number(reports.fps.fps) || 0} minFps=${Number(reports.fps.minFps) || 0}`);
  }
  if (reports.smooth) {
    const smooth = reports.smooth;
    lines.push(
      `station probe=smooth fps=${Number(smooth.fps) || 0} p50=${Number(smooth.p50) || 0} p95=${Number(smooth.p95) || 0} worst=${Number(smooth.worst) || 0} frames=${Number(smooth.frames) || 0} sampleMs=${Number(smooth.sampleMs) || 0}`,
    );
  }
  if (reports['debug-json']) {
    const report = reports['debug-json'];
    if (!report.supported) {
      lines.push(`station probe=debug-json supported=false reason=${quote(report.reason || 'not supported by this build')}`);
    } else {
      const data = report.data || {};
      lines.push(
        `station probe=debug-json supported=true fps=${Number(data.fps) || 0} renderer=${quote(data.renderer || '')} gpu=${data.gpu === undefined ? 'unknown' : Boolean(data.gpu)} hosts=${Number(data.hosts) || 0} agents=${Number(data.agents) || 0} events=${Number(data.events) || 0} displays=${Number(data.displays) || 0} selectedId=${quote(data.selectedId || '')} layout=${quote(data.layout || '')} hitZones=${Number(data.hitZones) || 0} systemTargets=${quote((data.systemTargets || []).join(','))}`,
      );
    }
  }
  return lines.map((line) => truncateMiddle(line, FORMATTED_DIAGNOSTIC_LINE_LIMIT));
}

function formatStationActivationPass(activation) {
  return `  station activate count=${activation.count || 0} warmupLatencyMs=${activation.warmupLatencyMs || 0} subsequentLatencyMs=${activation.subsequentLatencyMs || 0} names=${quote((activation.names || []).join(','))}`;
}

function formatStationPerfEvalLine(report) {
  let json;
  try {
    json = JSON.stringify(compactStationPerfEval(report));
  } catch (_) {
    json = String(report);
  }
  return `  station perf-eval ${truncateMiddle(json, FORMATTED_STATION_STATUS_LINE_LIMIT)}`;
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

async function validateCurrentStaticIdentity(targetUrl, cwd = process.cwd()) {
  const assets = [];
  for (const asset of STATIC_IDENTITY_ASSETS) {
    const localPath = path.join(cwd, asset.file);
    const local = fs.readFileSync(localPath);
    const expected = normalizeStaticIdentityBytes(local, asset);
    const servedUrl = dashboardAssetUrl(targetUrl, asset.urlPath);
    const served = await httpBuffer(servedUrl);
    const actual = normalizeStaticIdentityBytes(served, asset);
    const expectedHash = sha256Hex(expected);
    const actualHash = sha256Hex(actual);
    if (expectedHash !== actualHash) {
      throw new Error(
        [
          `stale dashboard static asset ${asset.urlPath}`,
          `served hash ${actualHash} does not match ${path.relative(cwd, localPath) || localPath} hash ${expectedHash}`,
          'the target is likely an already-running stale controller; rebuild/restart the controller for this worktree or omit --require-current-static only for non-build validation',
        ].join('; '),
      );
    }
    assets.push({ path: asset.urlPath, hash: actualHash });
  }
  return {
    status: 'pass',
    assets: assets.length,
    hash: sha256Hex(Buffer.from(assets.map((item) => `${item.path}:${item.hash}`).join('\n'))),
    items: assets,
  };
}

function normalizeStaticIdentityBytes(bytes, asset) {
  if (asset.kind === 'text') {
    const text = Buffer.isBuffer(bytes) ? bytes.toString('utf8') : String(bytes || '');
    const normalized = asset.normalize ? asset.normalize(text) : text;
    return Buffer.from(normalized, 'utf8');
  }
  return Buffer.isBuffer(bytes) ? bytes : Buffer.from(bytes);
}

function normalizeAppHtmlForIdentity(text) {
  let normalized = String(text || '');
  for (const assetPath of APP_HTML_CACHEBUSTED_ASSET_PATHS) {
    const pattern = new RegExp(`(${escapeRegExp(assetPath)})\\?[^"'\\s<>\`)]*`, 'g');
    normalized = normalized.replace(pattern, '$1');
  }
  return normalized;
}

function escapeRegExp(value) {
  return String(value).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function dashboardAssetUrl(targetUrl, urlPath) {
  const url = new URL(targetUrl);
  url.pathname = urlPath;
  url.search = '';
  url.hash = '';
  return url.toString();
}

function sha256Hex(bytes) {
  return crypto.createHash('sha256').update(bytes).digest('hex');
}

function httpBuffer(url) {
  return new Promise((resolve, reject) => {
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
        const chunks = [];
        res.on('data', (chunk) => chunks.push(Buffer.from(chunk)));
        res.on('end', () => {
          if (Number(res.statusCode) < 200 || Number(res.statusCode) >= 300) {
            reject(new Error(`GET ${url} returned ${res.statusCode}`));
            return;
          }
          resolve(Buffer.concat(chunks));
        });
      },
    );
    req.on('error', reject);
    req.setTimeout(5000, () => {
      req.destroy(new Error(`GET ${url} timed out`));
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

async function captureValidationScreenshotWithRetry(opts, launchEnv, harness, replaceHarness) {
  try {
    return {
      path: await harness.captureScreenshot(opts.screenshotPath),
      retried: false,
    };
  } catch (error) {
    if (!isCdpScreenshotTimeout(error)) {
      throw error;
    }
    const liveness = await harness.pageLiveness(SCREENSHOT_LIVENESS_TIMEOUT_MS);
    if (!liveness.alive) {
      await harness.close({ forceBrowser: true }).catch(() => {});
      throw screenshotTimeoutError(error, { retried: false, liveness });
    }
    harness.pageLogs.push(
      'harness',
      `Page.captureScreenshot timed out; page alive (${formatPageLiveness(liveness)}); retrying once in a fresh browser`,
    );
    await harness.close({ forceBrowser: true });

    const retryHarness = await BrowserHarness.launch(opts, launchEnv);
    replaceHarness(retryHarness);
    const validation = await retryHarness.validate(opts);
    try {
      return {
        path: await retryHarness.captureScreenshot(opts.screenshotPath),
        retried: true,
        validation,
      };
    } catch (retryError) {
      if (isCdpScreenshotTimeout(retryError)) {
        await retryHarness.close({ forceBrowser: true }).catch(() => {});
        throw screenshotTimeoutError(retryError, { retried: true, liveness });
      }
      throw retryError;
    }
  }
}

async function captureStationRuntimeTimeoutEvidence(
  opts,
  launchEnv,
  harness,
  error,
  replaceHarness,
  launchHarness = BrowserHarness.launch,
) {
  if (!shouldCaptureStationRuntimeTimeoutScreenshot(opts, error)) {
    return undefined;
  }
  removeExistingScreenshotArtifact(opts.screenshotPath);

  const fallback = {
    stationRuntimeEvaluateTimeout: true,
    screenshot: {
      status: 'not-captured',
      mode: 'same-browser',
    },
  };

  const first = await tryCaptureFailureScreenshot(
    harness,
    opts.screenshotPath,
    FAILURE_SCREENSHOT_TIMEOUT_MS,
    'same-browser',
  );
  if (first.ok) {
    fallback.screenshot = {
      status: 'captured',
      mode: 'same-browser',
    };
    return fallback;
  }

  fallback.screenshot.reason = first.reason;
  fallback.screenshot.status = 'failed';
  if (!launchEnv) {
    return fallback;
  }

  await harness.close({ forceBrowser: true }).catch(() => {});
  let retryHarness;
  try {
    retryHarness = await launchHarness(opts, launchEnv);
    replaceHarness(retryHarness);
    await retryHarness.navigateForScreenshot(opts, FAILURE_SCREENSHOT_NAVIGATION_TIMEOUT_MS);
    const retry = await tryCaptureFailureScreenshot(
      retryHarness,
      opts.screenshotPath,
      FAILURE_SCREENSHOT_TIMEOUT_MS,
      'fresh-browser',
    );
    fallback.screenshot.mode = 'fresh-browser';
    if (retry.ok) {
      fallback.screenshot.status = 'captured';
      delete fallback.screenshot.reason;
    } else {
      fallback.screenshot.reason = retry.reason;
    }
  } catch (retryError) {
    fallback.screenshot.mode = retryHarness ? 'fresh-browser' : 'fresh-browser-launch';
    fallback.screenshot.reason = retryError.message || String(retryError);
    if (retryHarness) {
      replaceHarness(retryHarness);
    }
  }
  return fallback;
}

function removeExistingScreenshotArtifact(filePath) {
  try {
    fs.rmSync(filePath, { force: true });
  } catch (_) {
    // A stale screenshot is less useful than an explicit capture failure.
  }
}

async function tryCaptureFailureScreenshot(harness, filePath, timeoutMs, mode) {
  try {
    await harness.captureScreenshot(filePath, timeoutMs);
    if (harness.pageLogs) {
      harness.pageLogs.push('harness', `captured failure screenshot via ${mode}`);
    }
    return { ok: true };
  } catch (error) {
    const reason = error.message || String(error);
    if (harness.pageLogs) {
      harness.pageLogs.push('harness', `failure screenshot via ${mode} failed: ${truncateMiddle(reason, LOG_TEXT_LIMIT)}`);
    }
    return {
      ok: false,
      reason,
    };
  }
}

function shouldCaptureStationRuntimeTimeoutScreenshot(opts, error) {
  return Boolean(
    opts
      && opts.screenshotPath
      && isStationFocusedCheck(opts)
      && isCdpRuntimeEvaluateTimeout(error),
  );
}

function isCdpRuntimeEvaluateTimeout(errorOrReason) {
  const text = errorOrReason && errorOrReason.message
    ? errorOrReason.message
    : String(errorOrReason || '');
  return /\bCDP Runtime\.evaluate timed out\b/.test(text);
}

function isCdpScreenshotTimeout(errorOrReason) {
  const text = errorOrReason && errorOrReason.message
    ? errorOrReason.message
    : String(errorOrReason || '');
  return /^CDP Page\.captureScreenshot timed out$/.test(text)
    || /^screenshot capture timed out\b/.test(text);
}

function screenshotTimeoutError(cause, details = {}) {
  const causeText = cause && cause.message ? cause.message : String(cause || 'unknown timeout');
  const retryText = details.retried ? ' after one retry' : '';
  const aliveText = details.liveness && details.liveness.alive
    ? `; page alive (${formatPageLiveness(details.liveness)})`
    : `; page liveness check failed${details.liveness && details.liveness.reason ? ` (${details.liveness.reason})` : ''}`;
  return new Error(`screenshot capture timed out${retryText}${aliveText}: ${causeText}`);
}

function formatPageLiveness(liveness) {
  if (!liveness || typeof liveness !== 'object') {
    return 'unknown';
  }
  const parts = [];
  if (liveness.readyState) {
    parts.push(`readyState=${liveness.readyState}`);
  }
  if (liveness.href) {
    parts.push(`url=${truncateMiddle(liveness.href, 160)}`);
  }
  if (liveness.reason) {
    parts.push(`reason=${truncateMiddle(liveness.reason, 160)}`);
  }
  return parts.join(' ') || 'unknown';
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

    const harness = new BrowserHarness(executable, userDataDir, child, stderr, {
      keepArtifacts: opts.keepArtifacts,
      keepBrowser: opts.keepBrowser,
    });
    try {
      harness.activePort = await waitForDevToolsPort(userDataDir, child, stderr, opts.cdpTimeoutMs);
      const version = await httpJson(`http://127.0.0.1:${harness.activePort.port}/json/version`);
      const wsUrl = version.webSocketDebuggerUrl || `ws://127.0.0.1:${harness.activePort.port}${harness.activePort.path}`;
      const socket = await openWebSocket(wsUrl, opts.cdpTimeoutMs);
      harness.websocketKind = socket.kind;
      harness.cdp = new CdpConnection(socket);
      await harness.preparePage();
      return harness;
    } catch (error) {
      await harness.close({ forceBrowser: true }).catch(() => {});
      throw error;
    }
  }

  constructor(browserExecutable, userDataDir, child, stderr, lifecycle = {}) {
    this.browserExecutable = browserExecutable;
    this.userDataDir = userDataDir;
    this.child = child;
    this.stderr = stderr;
    this.keepArtifacts = Boolean(lifecycle.keepArtifacts);
    this.keepBrowser = Boolean(lifecycle.keepBrowser);
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
    const validation = {};
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
      if (stationSurfaceRequired(opts)) {
        await this.prepareStationSurface(opts.timeoutMs);
      }
      for (const probe of opts.stationProbes) {
        const report = await this.runStationProbe(probe, opts);
        if (report) {
          validation.stationProbeReports = validation.stationProbeReports || {};
          validation.stationProbeReports[probe] = report;
        }
      }
      if (opts.stationInteractionProbe) {
        validation.stationInteraction = await this.runStationInteractionProbe(opts.timeoutMs);
      }
      if (opts.stationActivateTargets.length > 0) {
        validation.stationActivation = await this.runStationActivateProbe(opts);
      }
      if (opts.stationPerfEval) {
        validation.stationPerfEval = await this.runStationPerfEval(opts);
      }
      if (opts.requireStationState || opts.requireAiProviderSession || opts.requireExternalAgent || opts.requireManagedContextState) {
        validation.stationState = await this.requireStationState(opts);
      }
      return validation;
    } finally {
      this.cdp.off('event', onEvent);
    }
  }

  async navigateForScreenshot(opts, timeoutMs = FAILURE_SCREENSHOT_NAVIGATION_TIMEOUT_MS) {
    let loaded = false;
    const onEvent = (message) => {
      if (message.sessionId === this.sessionId && message.method === 'Page.loadEventFired') {
        loaded = true;
      }
    };
    this.cdp.on('event', onEvent);
    try {
      const nav = await this.cdp.send('Page.navigate', { url: opts.url }, this.sessionId, timeoutMs);
      if (nav.errorText) {
        throw new Error(`navigation failed before screenshot fallback: ${nav.errorText}`);
      }
      await waitUntil(
        () => loaded,
        timeoutMs,
        `page load event did not fire before screenshot fallback at ${opts.url}`,
      ).catch((error) => {
        this.pageLogs.push('harness', truncateMiddle(error.message || String(error), LOG_TEXT_LIMIT));
      });
      await delay(FAILURE_SCREENSHOT_SETTLE_MS);
    } finally {
      this.cdp.off('event', onEvent);
    }
  }

  async documentReadyState() {
    const result = await this.evaluate('document.readyState');
    return String(result || '');
  }

  async pageLiveness(timeoutMs = SCREENSHOT_LIVENESS_TIMEOUT_MS) {
    try {
      const result = await this.evaluate(
        `(() => ({
          readyState: document.readyState,
          href: window.location.href,
          hasBody: Boolean(document.body),
        }))()`,
        timeoutMs,
      );
      const readyState = String(result && result.readyState ? result.readyState : '');
      return {
        alive: Boolean(result && result.hasBody && readyState && readyState !== 'loading'),
        readyState,
        href: result && result.href ? String(result.href) : '',
      };
    } catch (error) {
      return {
        alive: false,
        reason: error.message || String(error),
      };
    }
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

  async runStationProbe(probe, opts) {
    if (probe === 'smooth') {
      return this.runStationSmoothProbe(opts);
    }
    if (probe === 'debug-json') {
      return this.waitForStationDebugJsonProbe(opts);
    }
    const value = await this.waitForStationProbe(probe, opts);
    if (probe === 'fps' && value && value.fps !== undefined) {
      return { fps: Number(value.fps) || 0, minFps: Number(value.minFps) || 0 };
    }
    return undefined;
  }

  async waitForStationProbe(probe, opts) {
    let lastError = '';
    let lastValue = '';
    let passed;
    const expression = stationProbeExpression(probe, { minFps: opts.stationMinFps });
    await waitUntil(
      async () => {
        try {
          const value = await this.evaluate(expression);
          lastValue = summarizeWaitValue(value);
          if (value && value.ok) {
            passed = value;
            return true;
          }
          return false;
        } catch (error) {
          lastError = error.message || String(error);
          return false;
        }
      },
      opts.timeoutMs,
      () => `station probe ${probe} did not pass${waitFailureSuffix(lastError, lastValue)}`,
    );
    return passed;
  }

  async runStationSmoothProbe(opts) {
    const value = await this.sampleStationSmooth(opts);
    if (!value || !value.ok) {
      throw new Error(`station probe smooth did not pass${waitFailureSuffix('', summarizeWaitValue(value))}`);
    }
    return compactStationSmoothReport(value);
  }

  async sampleStationSmooth(opts) {
    const options = stationSmoothOptions(opts);
    return this.evaluate(
      stationSmoothProbeExpression(options),
      options.sampleMs + options.stallTimeoutMs + STATION_SMOOTH_EVALUATE_TIMEOUT_BUFFER_MS,
    );
  }

  async waitForStationDebugJsonProbe(opts) {
    let lastError = '';
    let lastValue = '';
    let passed;
    const expression = stationDebugJsonProbeExpression({ require: Boolean(opts.requireDebugJson) });
    await waitUntil(
      async () => {
        try {
          const value = await this.evaluate(expression);
          lastValue = summarizeWaitValue(value);
          if (value && value.ok) {
            passed = value;
            return true;
          }
          return false;
        } catch (error) {
          lastError = error.message || String(error);
          return false;
        }
      },
      opts.timeoutMs,
      () => `station probe debug-json did not pass${waitFailureSuffix(lastError, lastValue)}`,
    );
    return compactStationDebugJsonReport(passed);
  }

  async stationActivateSupported() {
    const value = await this.evaluate(
      "Boolean(typeof station !== 'undefined' && station && typeof station.activate === 'function')",
    );
    return Boolean(value);
  }

  async activateStationTargetViaWasm(target) {
    return this.evaluate(
      `Promise.resolve((${stationActivateWasmSource()})(${JSON.stringify(target)}, ${STATION_ACTIVATE_SETTLE_BUDGET_MS}))`,
      STATION_ACTIVATE_SETTLE_BUDGET_MS + DEFAULT_CDP_COMMAND_TIMEOUT_MS,
    );
  }

  async activateStationTargetAuto(target) {
    if (await this.stationActivateSupported()) {
      return this.activateStationTargetViaWasm(target);
    }
    return this.activateStationHotspot(target);
  }

  async runStationActivateProbe(opts) {
    const activations = [];
    for (const target of opts.stationActivateTargets) {
      const started = Date.now();
      const result = await this.activateStationTargetViaWasm(target);
      if (!result || !result.ok) {
        throw new Error(`station activate probe ${target} did not pass (last value: ${summarizeWaitValue(result)})`);
      }
      activations.push({
        name: target,
        input: result.input || 'wasm-activate',
        verifiedVia: result.verifiedVia || '',
        selectedId: result.selectedId || '',
        latencyMs: Date.now() - started,
        settleMs: Number(result.settleMs) || 0,
      });
    }
    const latencies = activations.map((item) => item.latencyMs);
    const subsequent = latencies.slice(1);
    return {
      status: 'pass',
      count: activations.length,
      names: activations.map((item) => item.name),
      warmupLatencyMs: latencies[0] || 0,
      subsequentLatencyMs: subsequent.length
        ? Math.round(subsequent.reduce((sum, value) => sum + value, 0) / subsequent.length)
        : 0,
      activations,
    };
  }

  async stationDebugJsonSnapshot() {
    try {
      const value = await this.evaluate(stationDebugJsonProbeExpression({ require: false }));
      return value && value.ok && value.supported ? value.data : undefined;
    } catch (_) {
      return undefined;
    }
  }

  async runStationPerfEval(opts) {
    const thresholds = stationPerfEvalThresholds(opts);
    await delay(STATION_PERF_EVAL_SETTLE_MS);
    const idle = await this.sampleStationSmooth(opts);
    const targets = opts.stationActivateTargets.length
      ? opts.stationActivateTargets
      : STATION_PERF_EVAL_DEFAULT_TARGETS;
    const activations = [];
    for (const target of targets) {
      const started = Date.now();
      let result;
      try {
        result = await this.activateStationTargetAuto(target);
      } catch (error) {
        result = { ok: false, reason: error.message || String(error) };
      }
      activations.push({
        target,
        ok: Boolean(result && result.ok),
        input: (result && result.input) || '',
        latencyMs: Date.now() - started,
        reason: result && !result.ok
          ? truncateMiddle((result && result.reason) || 'activation failed', DIAGNOSTIC_TEXT_LIMIT)
          : undefined,
      });
    }
    const active = await this.sampleStationSmooth(opts);
    const snapshot = await this.stationDebugJsonSnapshot();
    const report = buildStationPerfEvalReport({
      idle,
      active,
      activations,
      thresholds,
      displays: snapshot ? Number(snapshot.displays) || 0 : undefined,
    });
    if (report.verdict !== 'pass') {
      const error = new Error(`station perf eval did not pass (${report.failures.join('; ')})`);
      error.stationPerfEval = report;
      throw error;
    }
    return report;
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

  async evaluate(expression, timeoutMs = DEFAULT_CDP_COMMAND_TIMEOUT_MS) {
    const result = await this.cdp.send(
      'Runtime.evaluate',
      {
        expression,
        awaitPromise: true,
        returnByValue: true,
      },
      this.sessionId,
      timeoutMs,
    );
    if (result.exceptionDetails) {
      throw new Error(exceptionText(result.exceptionDetails));
    }
    return result.result && Object.prototype.hasOwnProperty.call(result.result, 'value')
      ? result.result.value
      : undefined;
  }

  async captureScreenshot(filePath, timeoutMs = DEFAULT_CDP_COMMAND_TIMEOUT_MS) {
    const result = await this.cdp.send(
      'Page.captureScreenshot',
      {
        format: 'png',
        captureBeyondViewport: false,
        fromSurface: true,
      },
      this.sessionId,
      timeoutMs,
    );
    if (!result.data) {
      throw new Error('screenshot capture returned no image data');
    }
    fs.mkdirSync(path.dirname(filePath), { recursive: true });
    const tempPath = `${filePath}.tmp-${process.pid}`;
    fs.writeFileSync(tempPath, Buffer.from(result.data, 'base64'));
    fs.renameSync(tempPath, filePath);
    return filePath;
  }

  async runStationInteractionProbe(timeoutMs) {
    const targets = ['system:activity', 'system:controls', 'system:view'];
    const interactions = [];
    for (const target of targets) {
      const point = await this.stationHotspotPoint(target);
      if (!point.ok) {
        throw new Error(`station interaction probe impossible: ${point.reason || `could not resolve ${target}`} (last value: ${summarizeWaitValue(point)})`);
      }
      const started = Date.now();
      const activated = await this.activateStationHotspot(target);
      if (!activated.ok) {
        throw new Error(`station interaction probe ${target} did not pass (last value: ${summarizeWaitValue(activated)})`);
      }
      interactions.push({
        name: target,
        control: point.control || target,
        input: activated.input || 'activation-event',
        x: Math.round(point.x),
        y: Math.round(point.y),
        latencyMs: Date.now() - started,
        state: activated.state,
      });
    }
    const latencies = interactions.map((item) => item.latencyMs);
    const subsequent = latencies.slice(1);
    return {
      status: 'pass',
      count: interactions.length,
      names: interactions.map((item) => item.name),
      warmupLatencyMs: latencies[0] || 0,
      subsequentLatencyMs: subsequent.length
        ? Math.round(subsequent.reduce((sum, value) => sum + value, 0) / subsequent.length)
        : 0,
      interactions,
    };
  }

  async stationHotspotPoint(target) {
    return this.evaluate(`(${stationHotspotPointSource()})(${JSON.stringify(target)})`);
  }

  async activateStationHotspot(target) {
    return this.evaluate(`(${stationHotspotActivateSource()})(${JSON.stringify(target)})`);
  }

  async requireStationState(opts) {
    const state = await this.evaluate(`(${stationStateSummarySource()})(${JSON.stringify(opts.requireExternalAgent || '')})`);
    if (!state || !state.ok) {
      throw new Error(`station state check failed: ${state && state.reason ? state.reason : 'could not inspect Station state'} (last value: ${summarizeWaitValue(state)})`);
    }
    if (!state.nonEmpty && !opts.allowEmptyStationState) {
      throw new Error(`station state check failed: Station sessions/events/managed/peers are all empty (last value: ${summarizeWaitValue(state)})`);
    }
    if (opts.requireAiProviderSession && !(state.liveAgentSession && state.liveAgentSession.ok)) {
      const reason = state.liveAgentSession && state.liveAgentSession.reason
        ? state.liveAgentSession.reason
        : 'Station did not expose a real provider/model/session';
      throw new Error(`ai provider session check failed: ${reason} (last value: ${summarizeWaitValue(state)})`);
    }
    if (opts.requireExternalAgent && !(state.externalAgentSession && state.externalAgentSession.ok)) {
      const reason = state.externalAgentSession && state.externalAgentSession.reason
        ? state.externalAgentSession.reason
        : `Station did not expose an external ${opts.requireExternalAgent} session`;
      throw new Error(`external agent session check failed: ${reason} (last value: ${summarizeWaitValue(state)})`);
    }
    if (opts.requireManagedContextState && !(state.managedContextState && state.managedContextState.ok)) {
      const reason = state.managedContextState && state.managedContextState.reason
        ? state.managedContextState.reason
        : 'Station did not expose active managed/context session state';
      throw new Error(`managed context state check failed: ${reason} (last value: ${summarizeWaitValue(state)})`);
    }
    return state;
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

  async close(options = {}) {
    if (this.closed) {
      return;
    }
    this.closed = true;
    const keepBrowser = this.keepBrowser && !options.forceBrowser;
    if (this.cdp && !keepBrowser) {
      await Promise.race([this.cdp.send('Browser.close'), delay(1000)]).catch(() => {});
      this.cdp.close();
    } else if (this.cdp) {
      this.cdp.close();
    }
    if (!keepBrowser && this.child && !this.child.killed) {
      await waitForExit(this.child, 800).catch(() => {
        this.child.kill('SIGKILL');
      });
    }
    if (!this.keepArtifacts) {
      fs.rmSync(this.userDataDir, { recursive: true, force: true });
    }
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

  send(method, params = {}, sessionId, timeoutMs = DEFAULT_CDP_COMMAND_TIMEOUT_MS) {
    const id = this.nextId;
    this.nextId += 1;
    const payload = { id, method, params };
    if (sessionId) {
      payload.sessionId = sessionId;
    }
    this.socket.send(JSON.stringify(payload));
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        if (this.pending.delete(id)) {
          reject(new Error(`CDP ${method} timed out`));
        }
      }, timeoutMs);
      if (typeof timer.unref === 'function') {
        timer.unref();
      }
      this.pending.set(id, { resolve, reject, timer });
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
      clearTimeout(pending.timer);
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
      clearTimeout(pending.timer);
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

function stationProbeExpression(probe, options = {}) {
  const normalized = normalizeStationProbeName(probe);
  return `Promise.resolve((() => {
    const stationDiagnostics = (${stationDiagnosticsSource()})();
    return (${stationProbeSource()})(${JSON.stringify(normalized)}, stationDiagnostics, ${JSON.stringify(options)});
  })())`;
}

function stationSurfaceRequired(opts) {
  return Boolean(
    (opts.stationProbes || []).length > 0
      || opts.stationInteractionProbe
      || (opts.stationActivateTargets || []).length > 0
      || opts.stationPerfEval
      || opts.requireStationState
      || opts.requireAiProviderSession
      || opts.requireExternalAgent
      || opts.requireManagedContextState,
  );
}

function stationSmoothOptions(opts = {}) {
  const sampleMs = STATION_SMOOTH_SAMPLE_MS;
  return {
    sampleMs,
    maxFrameGapMs: Number(opts.stationMaxFrameGapMs) || DEFAULT_STATION_MAX_FRAME_GAP_MS,
    hardGapLimitMs: STATION_SMOOTH_HARD_GAP_LIMIT_MS,
    stallTimeoutMs: sampleMs * 2 + 1000,
  };
}

function stationSmoothProbeExpression(options = {}) {
  return `Promise.resolve((${stationSmoothProbeSource()})(${JSON.stringify(options)}))`;
}

function stationSmoothProbeSource() {
  return function collectStationSmoothSample(options) {
    const sampleMs = Math.max(250, Number(options && options.sampleMs) || 2000);
    const maxFrameGapMs = Number(options && options.maxFrameGapMs) || 40;
    const hardGapLimitMs = Number(options && options.hardGapLimitMs) || 250;
    const stallTimeoutMs = Math.max(50, Number(options && options.stallTimeoutMs) || sampleMs * 2 + 1000);
    return new Promise((resolve) => {
      const deltas = [];
      let last = -1;
      let endAt = 0;
      let settled = false;
      const finish = (result) => {
        if (settled) {
          return;
        }
        settled = true;
        resolve(result);
      };
      const quantile = (sorted, q) => {
        if (!sorted.length) {
          return 0;
        }
        const index = Math.min(sorted.length - 1, Math.max(0, Math.ceil(q * sorted.length) - 1));
        return sorted[index];
      };
      const round1 = (value) => Math.round(value * 10) / 10;
      const summarize = () => {
        const sorted = deltas.slice().sort((a, b) => a - b);
        const total = deltas.reduce((sum, value) => sum + value, 0);
        return {
          probe: 'smooth',
          frames: deltas.length,
          sampleMs: Math.round(total),
          fps: total > 0 ? Math.round((deltas.length * 1000) / total) : 0,
          p50: round1(quantile(sorted, 0.5)),
          p95: round1(quantile(sorted, 0.95)),
          worst: round1(sorted.length ? sorted[sorted.length - 1] : 0),
          maxFrameGapMs,
          hardGapLimitMs,
        };
      };
      const verdict = () => {
        const stats = summarize();
        if (!stats.frames) {
          return { ...stats, ok: false, stalled: true, failureKind: 'renderer', reason: 'no requestAnimationFrame frames were observed' };
        }
        if (stats.worst > hardGapLimitMs) {
          return { ...stats, ok: false, failureKind: 'renderer', reason: `worst frame gap ${stats.worst}ms exceeds the ${hardGapLimitMs}ms stall limit` };
        }
        if (stats.p95 > maxFrameGapMs) {
          return { ...stats, ok: false, failureKind: 'renderer', reason: `p95 frame gap ${stats.p95}ms exceeds the ${maxFrameGapMs}ms budget` };
        }
        return { ...stats, ok: true, reason: 'passed' };
      };
      const guard = setTimeout(() => {
        const stats = summarize();
        finish({
          ...stats,
          ok: false,
          stalled: true,
          failureKind: 'renderer',
          reason: `requestAnimationFrame stalled: ${stats.frames} frame(s) in ${stallTimeoutMs}ms`,
        });
      }, stallTimeoutMs);
      const step = (now) => {
        if (settled) {
          return;
        }
        if (last < 0) {
          last = now;
          endAt = now + sampleMs;
          requestAnimationFrame(step);
          return;
        }
        deltas.push(now - last);
        last = now;
        if (now < endAt) {
          requestAnimationFrame(step);
          return;
        }
        clearTimeout(guard);
        finish(verdict());
      };
      requestAnimationFrame(step);
    });
  }.toString();
}

function compactStationSmoothReport(sample) {
  if (!sample || typeof sample !== 'object') {
    return sample;
  }
  return {
    ok: Boolean(sample.ok),
    frames: Number(sample.frames) || 0,
    sampleMs: Number(sample.sampleMs) || 0,
    fps: Number(sample.fps) || 0,
    p50: Number(sample.p50) || 0,
    p95: Number(sample.p95) || 0,
    worst: Number(sample.worst) || 0,
    maxFrameGapMs: Number(sample.maxFrameGapMs) || 0,
    hardGapLimitMs: Number(sample.hardGapLimitMs) || 0,
  };
}

function stationDebugJsonProbeExpression(options = {}) {
  return `Promise.resolve((${stationDebugJsonProbeSource()})(${JSON.stringify({
    requiredKeys: STATION_DEBUG_JSON_REQUIRED_KEYS,
    listLimit: STATION_DEBUG_JSON_LIST_LIMIT,
    ...options,
  })}))`;
}

function stationDebugJsonProbeSource() {
  return function collectStationDebugJson(options) {
    const requireExport = Boolean(options && options.require);
    const listLimit = Number(options && options.listLimit) || 16;
    const requiredKeys = Array.isArray(options && options.requiredKeys) && options.requiredKeys.length
      ? options.requiredKeys.map(String)
      : ['fps', 'renderer', 'hosts'];
    const fail = (reason) => ({ ok: false, probe: 'debug-json', supported: true, failureKind: 'probe', reason });
    const handle = (typeof station !== 'undefined' && station)
      || (typeof window !== 'undefined' && window.station)
      || null;
    if (!handle) {
      return { ok: false, probe: 'debug-json', supported: false, failureKind: 'probe', reason: 'Station WASM handle is unavailable' };
    }
    if (typeof handle.debug_json !== 'function') {
      if (requireExport) {
        return { ok: false, probe: 'debug-json', supported: false, failureKind: 'probe', reason: 'station.debug_json() is not supported by this build' };
      }
      return { ok: true, probe: 'debug-json', supported: false, reason: 'station.debug_json() is not supported by this build' };
    }
    let raw;
    try {
      raw = handle.debug_json();
    } catch (error) {
      return fail(`station.debug_json() threw: ${error && error.message ? error.message : error}`);
    }
    let data = raw;
    if (typeof raw === 'string') {
      try {
        data = JSON.parse(raw);
      } catch (error) {
        return fail(`station.debug_json() did not parse as JSON: ${error && error.message ? error.message : error}`);
      }
    }
    if (!data || typeof data !== 'object' || Array.isArray(data)) {
      return fail('station.debug_json() returned a non-object payload');
    }
    const missing = requiredKeys.filter((key) => !(key in data));
    if (missing.length) {
      return fail(`station.debug_json() is missing required key(s): ${missing.join(', ')}`);
    }
    const countOf = (value) => {
      if (Array.isArray(value)) {
        return value.length;
      }
      if (typeof value === 'number') {
        return value;
      }
      if (value && typeof value === 'object') {
        return Object.keys(value).length;
      }
      return 0;
    };
    const text = (value, limit) => {
      const out = String(value === undefined || value === null ? '' : value);
      return out.length <= limit ? out : `${out.slice(0, limit - 3)}...`;
    };
    const names = (value) => {
      if (!Array.isArray(value)) {
        return [];
      }
      return value
        .slice(0, listLimit)
        .map((item) => {
          if (typeof item === 'string') {
            return text(item, 48);
          }
          if (item && typeof item === 'object') {
            return text(item.name || item.id || '', 48);
          }
          return text(item, 48);
        })
        .filter(Boolean);
    };
    return {
      ok: true,
      probe: 'debug-json',
      supported: true,
      reason: 'passed',
      data: {
        fps: Number(data.fps) || 0,
        renderer: text(data.renderer, 48),
        gpu: data.gpu === undefined ? undefined : Boolean(data.gpu),
        hosts: countOf(data.hosts),
        agents: countOf(data.agents),
        events: countOf(data.events),
        displays: countOf(data.displays),
        selectedId: text(data.selectedId, 80),
        layout: text(data.layout, 48),
        mood: text(data.mood, 48),
        motion: text(data.motion, 48),
        hitZones: countOf(data.hitZones),
        hitZoneNames: names(data.hitZones),
        systemTargets: names(data.systemTargets),
        systemTargetCount: countOf(data.systemTargets),
      },
    };
  }.toString();
}

function compactStationDebugJsonReport(value) {
  if (!value || typeof value !== 'object') {
    return value;
  }
  const compact = {
    ok: Boolean(value.ok),
    supported: Boolean(value.supported),
    reason: truncateMiddle(value.reason || '', DIAGNOSTIC_TEXT_LIMIT),
  };
  if (value.data && typeof value.data === 'object') {
    const data = value.data;
    compact.data = {
      fps: Number(data.fps) || 0,
      renderer: truncateMiddle(data.renderer || '', 48),
      gpu: data.gpu === undefined ? undefined : Boolean(data.gpu),
      hosts: Number(data.hosts) || 0,
      agents: Number(data.agents) || 0,
      events: Number(data.events) || 0,
      displays: Number(data.displays) || 0,
      selectedId: truncateMiddle(data.selectedId || '', 80),
      layout: truncateMiddle(data.layout || '', 48),
      mood: truncateMiddle(data.mood || '', 48),
      motion: truncateMiddle(data.motion || '', 48),
      hitZones: Number(data.hitZones) || 0,
      hitZoneNames: compactStringArray(data.hitZoneNames, STATION_DEBUG_JSON_LIST_LIMIT, 48).values,
      systemTargets: compactStringArray(data.systemTargets, STATION_DEBUG_JSON_LIST_LIMIT, 48).values,
      systemTargetCount: Number(data.systemTargetCount) || 0,
    };
  }
  return compact;
}

function stationActivateWasmSource() {
  return function activateStationTargetViaWasm(target, settleBudgetMs) {
    const budget = Math.max(50, Number(settleBudgetMs) || 1000);
    const handleFor = () => (typeof station !== 'undefined' && station)
      || (typeof window !== 'undefined' && window.station)
      || null;
    const readState = () => {
      const state = { selectedId: '', statusText: '' };
      const handle = handleFor();
      try {
        if (handle && typeof handle.debug_json === 'function') {
          let data = handle.debug_json();
          if (typeof data === 'string') {
            data = JSON.parse(data);
          }
          if (data && typeof data === 'object') {
            state.selectedId = String(data.selectedId === undefined || data.selectedId === null ? '' : data.selectedId);
          }
        }
      } catch (_) {}
      const status = document.getElementById('station-status');
      state.statusText = String((status && status.textContent) || '');
      return state;
    };
    const targetText = String(target || '');
    const kind = targetText.includes(':') ? targetText.slice(targetText.lastIndexOf(':') + 1) : targetText;
    const verify = (state) => {
      const norm = (value) => String(value || '').trim().toLowerCase();
      const selected = norm(state.selectedId);
      if (selected && (selected === norm(targetText) || selected === norm(kind))) {
        return 'selectedId';
      }
      if (kind) {
        const kindPattern = new RegExp(`\\b${kind.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}\\b`, 'i');
        if (kindPattern.test(state.statusText) && /\b(rendered|opening|station)\b/i.test(state.statusText)) {
          return 'status-text';
        }
      }
      return '';
    };
    const fail = (reason, extra = {}) => ({
      ok: false,
      failureKind: 'interaction',
      input: 'wasm-activate',
      target: targetText,
      reason,
      ...extra,
    });
    const pane = document.getElementById('tab-station');
    if (!pane || !pane.classList.contains('active')) {
      return fail('Station tab is not active');
    }
    const handle = handleFor();
    if (!handle || typeof handle.activate !== 'function') {
      return fail('station.activate() is not supported by this build');
    }
    let accepted;
    try {
      accepted = handle.activate(targetText);
    } catch (error) {
      return fail(`station.activate(${targetText}) threw: ${error && error.message ? error.message : error}`);
    }
    if (accepted === false) {
      return fail(`station.activate(${targetText}) returned false (unknown target?)`);
    }
    const startedAt = Date.now();
    const finish = (state, verifiedVia, settleMs) => ({
      ok: Boolean(verifiedVia),
      input: 'wasm-activate',
      target: targetText,
      accepted: accepted !== false,
      verifiedVia: verifiedVia || '',
      selectedId: state.selectedId,
      statusText: state.statusText,
      settleMs: Number(settleMs) || 0,
      failureKind: verifiedVia ? '' : 'interaction',
      reason: verifiedVia
        ? 'passed'
        : `Station did not reflect activation of ${targetText} in debug_json selectedId or status text`,
    });
    const first = readState();
    const firstVia = verify(first);
    if (firstVia) {
      return finish(first, firstVia, 0);
    }
    // Selection lands on the next WASM render/state tick; poll briefly. This
    // is an upper bound only and resolves as soon as the state reflects it.
    return new Promise((resolve) => {
      const deadlineAt = startedAt + budget;
      const recheck = () => {
        const state = readState();
        const via = verify(state);
        if (via || Date.now() >= deadlineAt) {
          resolve(finish(state, via, Date.now() - startedAt));
          return;
        }
        setTimeout(recheck, 50);
      };
      setTimeout(recheck, 16);
    });
  }.toString();
}

function stationPerfEvalThresholds(opts = {}) {
  return {
    minFps: Number(opts.stationMinFps) || DEFAULT_STATION_MIN_FPS,
    maxFrameGapMs: Number(opts.stationMaxFrameGapMs) || DEFAULT_STATION_MAX_FRAME_GAP_MS,
    hardGapLimitMs: STATION_SMOOTH_HARD_GAP_LIMIT_MS,
  };
}

function buildStationPerfEvalReport({ idle, active, activations, thresholds, displays }) {
  const failures = [];
  const checkSample = (label, value) => {
    if (!value || typeof value !== 'object') {
      failures.push(`${label} smooth sample is missing`);
      return { fps: 0, p50: 0, p95: 0, worst: 0, frames: 0 };
    }
    const stats = {
      fps: Number(value.fps) || 0,
      p50: Number(value.p50) || 0,
      p95: Number(value.p95) || 0,
      worst: Number(value.worst) || 0,
      frames: Number(value.frames) || 0,
    };
    if (value.stalled) {
      failures.push(`${label} sample stalled: ${value.reason || 'requestAnimationFrame stalled'}`);
      return stats;
    }
    if (stats.fps < thresholds.minFps) {
      failures.push(`${label} fps ${stats.fps} below minimum ${thresholds.minFps}`);
    }
    if (stats.p95 > thresholds.maxFrameGapMs) {
      failures.push(`${label} p95 frame gap ${stats.p95}ms exceeds ${thresholds.maxFrameGapMs}ms`);
    }
    if (stats.worst > thresholds.hardGapLimitMs) {
      failures.push(`${label} worst frame gap ${stats.worst}ms exceeds ${thresholds.hardGapLimitMs}ms`);
    }
    return stats;
  };
  const idleStats = checkSample('idle', idle);
  const activeStats = checkSample('active', active);
  const list = Array.isArray(activations) ? activations : [];
  for (const activation of list) {
    if (!activation || !activation.ok) {
      const target = activation && activation.target ? activation.target : 'unknown';
      const reason = activation && activation.reason ? `: ${activation.reason}` : '';
      failures.push(`activation ${target} failed${reason}`);
    }
  }
  const report = {
    fpsIdle: idleStats.fps,
    fpsAfterInteraction: activeStats.fps,
    p50Idle: idleStats.p50,
    p95Idle: idleStats.p95,
    worstIdle: idleStats.worst,
    p50Active: activeStats.p50,
    p95Active: activeStats.p95,
    worstActive: activeStats.worst,
    framesIdle: idleStats.frames,
    framesActive: activeStats.frames,
    interactionTargets: list.map((item) => String((item && item.target) || '')),
    interactionLatencies: list.map((item) => Number(item && item.latencyMs) || 0),
    interactionInput: list.length ? String((list[0] && list[0].input) || '') : '',
    thresholds: {
      minFps: Number(thresholds && thresholds.minFps) || 0,
      maxFrameGapMs: Number(thresholds && thresholds.maxFrameGapMs) || 0,
      hardGapLimitMs: Number(thresholds && thresholds.hardGapLimitMs) || 0,
    },
    failures,
    verdict: failures.length ? 'fail' : 'pass',
  };
  if (displays !== undefined) {
    report.displays = Number(displays) || 0;
  }
  return report;
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
  return function collectStationProbe(probe, diagnostics, probeOptions) {
    const statusText = String(diagnostics && diagnostics.statusText ? diagnostics.statusText : '');
    const canvas = (diagnostics && diagnostics.canvas) || {};
    const pixels = (diagnostics && diagnostics.pixels) || {};
    const dock = document.getElementById('station-dock');
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
        // Station is dock-less; an absent #station-dock is the expected state.
        found: Boolean(dock),
        hidden: Boolean(dock && dock.classList && dock.classList.contains('hidden')),
      },
    };
    const pass = (extra = {}) => ({ ...base, ...extra, ok: true, reason: 'passed' });
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
    if (probe === 'dock-hidden') {
      // Name kept for compatibility: it now passes when the legacy dock is
      // absent (the expected dock-less state) or present-but-hidden, and only
      // fails when a visible dock is still in the DOM.
      if (!base.dock.found || base.dock.hidden) {
        return pass();
      }
      return fail('Station dock is visible');
    }
    if (probe === 'fps') {
      // Render-health eval: the WASM reports presented frames/sec over a
      // trailing 2s window in debug_state ("fps=NN"). Catches the failure
      // mode where state probes pass while the page is stalling (slow
      // snapshot builds, GPU-process death, parked render loop).
      const m = /\bfps=(\d+)\b/.exec(`${base.debugState} ${base.statusText}`);
      if (!m) {
        return fail('Station debug state reports no fps figure', 'renderer');
      }
      const fps = Number(m[1]);
      const minFps = Number(probeOptions && probeOptions.minFps) || 24;
      if (fps >= minFps) {
        return pass({ fps, minFps });
      }
      return fail(`Station presenting at ${fps}fps (minimum ${minFps})`, 'renderer');
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

function stationHotspotPointSource() {
  return function collectStationHotspotPoint(target) {
    const fail = (reason, extra = {}) => ({
      ok: false,
      failureKind: 'interaction',
      reason,
      target,
      ...extra,
    });
    const pane = document.getElementById('tab-station');
    const hud = document.getElementById('station-hud-canvas');
    if (!pane || !pane.classList.contains('active')) {
      return fail('Station tab is not active');
    }
    if (!hud) {
      return fail('Station HUD canvas is missing');
    }
    const rect = hud.getBoundingClientRect();
    if (!rect || rect.width <= 0 || rect.height <= 0) {
      return fail('Station HUD canvas has no measurable viewport area', {
        rect: rect ? {
          left: Math.round(rect.left),
          top: Math.round(rect.top),
          width: Math.round(rect.width),
          height: Math.round(rect.height),
        } : null,
      });
    }

    let box = null;
    try {
      if (typeof stationHotspotBoxes === 'function') {
        box = stationHotspotBoxes(rect.width || 1, rect.height || 1)
          .find((candidate) => candidate && candidate.name === target);
      }
    } catch (error) {
      return fail(`Station hotspot geometry failed: ${error && error.message ? error.message : error}`);
    }
    if (!box) {
      const button = document.querySelector(`[data-station-hotspot="${String(target).replace(/"/g, '\\"')}"]`);
      if (button) {
        const buttonRect = button.getBoundingClientRect();
        if (buttonRect.width > 0 && buttonRect.height > 0) {
          box = {
            name: target,
            x: buttonRect.left - rect.left,
            y: buttonRect.top - rect.top,
            w: buttonRect.width,
            h: buttonRect.height,
          };
        }
      }
    }
    if (!box || box.w <= 0 || box.h <= 0) {
      return fail(`Station hotspot ${target} is not measurable`);
    }

    const x = rect.left + box.x + box.w / 2;
    const y = rect.top + box.y + box.h / 2;
    const inViewport = x >= 0
      && y >= 0
      && x <= (window.innerWidth || document.documentElement.clientWidth || 0)
      && y <= (window.innerHeight || document.documentElement.clientHeight || 0);
    if (!inViewport) {
      return fail(`Station hotspot ${target} is outside the viewport`, {
        x: Math.round(x),
        y: Math.round(y),
        viewport: {
          width: window.innerWidth || document.documentElement.clientWidth || 0,
          height: window.innerHeight || document.documentElement.clientHeight || 0,
        },
      });
    }
    const hit = document.elementFromPoint(x, y);
    const control = hit && hit.closest
      ? (
          hit.closest('[data-station-hotspot]')?.getAttribute('data-station-hotspot')
          || hit.closest('#station-hud-canvas')?.id
          || hit.id
          || hit.tagName
        )
      : '';
    return {
      ok: true,
      target,
      input: 'mouse',
      x,
      y,
      control,
      rect: {
        left: Math.round(rect.left),
        top: Math.round(rect.top),
        width: Math.round(rect.width),
        height: Math.round(rect.height),
      },
      box: {
        x: Math.round(box.x),
        y: Math.round(box.y),
        width: Math.round(box.w),
        height: Math.round(box.h),
      },
    };
  }.toString();
}

function stationHotspotActivateSource() {
  return function activateStationHotspot(target) {
    const stateFor = function collectStationInteractionStateInline(targetText, settleMs) {
      const kind = targetText.includes(':') ? targetText.slice(targetText.lastIndexOf(':') + 1) : targetText;
      const status = document.getElementById('station-status');
      const activeTab = document.getElementById('tab-station')?.classList.contains('active') ? 'station' : '';
      const statusText = String(status?.textContent || '');
      const statusMatches = kind
        ? new RegExp(`\\b${kind.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}\\b`, 'i').test(statusText)
        : false;
      // Hotspot activation drives WASM-side selection; the contract is the
      // `Opening <kind>` pattern on #station-status, not a dock opening.
      const activationMatches = statusMatches && /\b(rendered|opening|station)\b/i.test(statusText);
      const ok = activeTab === 'station' && activationMatches;
      return {
        ok,
        failureKind: ok ? '' : 'interaction',
        target: targetText,
        activeTab,
        statusText,
        statusMatches,
        settleMs: Number(settleMs) || 0,
        reason: ok ? 'passed' : `Station did not activate ${targetText}`,
      };
    };
    const fail = (reason, extra = {}) => ({
      ok: false,
      failureKind: 'interaction',
      reason,
      target,
      state: stateFor(String(target || ''), 0),
      ...extra,
    });
    const pane = document.getElementById('tab-station');
    if (!pane || !pane.classList.contains('active')) {
      return fail('Station tab is not active');
    }
    const selector = `[data-station-hotspot="${String(target).replace(/"/g, '\\"')}"]`;
    const button = document.querySelector(selector);
    if (!button) {
      return fail(`Station hotspot ${target} is missing`, { selector });
    }
    if (button.disabled) {
      return fail(`Station hotspot ${target} is disabled`, { selector });
    }
    button.focus();
    const event = new MouseEvent('click', {
      bubbles: true,
      cancelable: true,
      detail: 0,
      view: window,
    });
    const accepted = button.dispatchEvent(event);
    const startedAt = Date.now();
    const finish = (state) => ({
      ok: Boolean(state.ok),
      target,
      selector,
      input: 'activation-event',
      accepted,
      defaultPrevented: event.defaultPrevented,
      state,
      reason: state.ok ? 'passed' : state.reason,
    });
    const first = stateFor(String(target || ''), 0);
    if (first.ok) {
      return finish(first);
    }
    // The renderer repaints on requestAnimationFrame with render-on-change, so
    // the status text may land a tick after the click. Poll briefly; this is an
    // upper bound only and resolves as soon as the status reflects activation.
    return new Promise((resolve) => {
      const deadlineAt = startedAt + 1000;
      const recheck = () => {
        const state = stateFor(String(target || ''), Date.now() - startedAt);
        if (state.ok || Date.now() >= deadlineAt) {
          resolve(finish(state));
          return;
        }
        setTimeout(recheck, 50);
      };
      setTimeout(recheck, 16);
    });
  }.toString();
}

function stationStateSummarySource() {
  return function collectStationStateSummary(requiredExternalAgentRaw) {
    const countArray = (value) => Array.isArray(value) ? value.length : 0;
    const countMap = (value) => {
      if (!value) return 0;
      if (value instanceof Map || value instanceof Set) return value.size;
      if (Array.isArray(value)) return value.length;
      if (typeof value === 'object') return Object.keys(value).length;
      return 0;
    };
    const countManaged = (managed) => {
      if (!managed || typeof managed !== 'object') return 0;
      let count = 0;
      if (managed.live) count += 1;
      if (managed.sessionId) count += 1;
      count += Number(managed.records) || 0;
      count += Number(managed.anchors) || 0;
      count += Number(managed.lineageGroups) || 0;
      count += Number(managed.fissionGroups) || 0;
      count += countArray(managed.recentRecords);
      count += countArray(managed.recentAnchors);
      count += countArray(managed.recentBranches);
      return count;
    };
    const managedCountDetails = (managed) => {
      if (!managed || typeof managed !== 'object') {
        return { records: 0, anchors: 0, lineageGroups: 0, fissionGroups: 0, recentRecords: 0, recentAnchors: 0, recentBranches: 0 };
      }
      return {
        records: Number(managed.records) || 0,
        anchors: Number(managed.anchors) || 0,
        lineageGroups: Number(managed.lineageGroups) || 0,
        fissionGroups: Number(managed.fissionGroups) || 0,
        recentRecords: countArray(managed.recentRecords),
        recentAnchors: countArray(managed.recentAnchors),
        recentBranches: countArray(managed.recentBranches),
      };
    };
    const asText = (value) => String(value ?? '').trim();
    const compactUniqueText = (values) => {
      const out = [];
      for (const value of values || []) {
        const text = asText(value);
        if (text && !out.includes(text)) out.push(text);
      }
      return out;
    };
    const normalizeExternalAgentId = (value) => asText(value).toLowerCase().replace(/[_\s]+/g, '-');
    const placeholderPattern = /^(?:-|--|n\/a|na|null|undefined|unknown|none|no[-_\s]?provider|no[-_\s]?model|placeholder|dummy|default|auto|browser|current|global|managed-context|done-placeholder|claude-code-session)$/i;
    const usableText = (value) => {
      const text = asText(value);
      return text && !placeholderPattern.test(text) ? text : '';
    };
    const sessionIdText = (value) => {
      const text = usableText(value);
      if (!text || /^(?:primary-agent|local|session|current-daemon-session)$/i.test(text)) return '';
      return text;
    };
    const sourceValuesFor = (item, fallback = {}) => compactUniqueText([
      item.backend_source,
      item.backendSource,
      item.source,
      item.source_label,
      item.sourceLabel,
      item.backend_source_label,
      item.backendSourceLabel,
      item.agent,
      item.agent_name,
      item.agentName,
      item.isCodex ? 'codex' : '',
      fallback.backendSource,
      fallback.source,
      fallback.sourceLabel,
    ]);
    const candidateFrom = (raw, fallback = {}) => {
      const item = raw && typeof raw === 'object' ? raw : {};
      const backendSessionId = sessionIdText(item.backend_session_id || item.backendSessionId || item.backendId || fallback.backendSessionId);
      const intendantSessionId = sessionIdText(item.intendant_session_id || item.intendantSessionId || item.intendantId || fallback.intendantSessionId);
      const sessionId = sessionIdText(
        item.session_id || item.sessionId || item.resume_id || item.resumeId ||
        backendSessionId || item.id || fallback.sessionId
      );
      const provider = usableText(item.provider || item.provider_name || item.providerName || fallback.provider);
      const model = usableText(item.model || item.model_name || item.modelName || fallback.model);
      const sourceCandidates = sourceValuesFor(item, fallback);
      return {
        sessionId,
        backendSessionId,
        intendantSessionId,
        provider,
        model,
        source: usableText(sourceCandidates[0] || ''),
        sourceCandidates,
        status: asText(item.status || item.phase || fallback.status),
        evidence: fallback.evidence || '',
      };
    };
    const collectMapCandidates = (out, value, evidence) => {
      if (!value || typeof value.forEach !== 'function') return;
      value.forEach((item, key) => out.push(candidateFrom(item, { sessionId: key, evidence })));
    };
    const collectArrayCandidates = (out, value, evidence) => {
      if (!Array.isArray(value)) return;
      for (const item of value) out.push(candidateFrom(item, { evidence }));
    };
    const stationSnapshotBuilder = () => {
      if (typeof buildStationSnapshot === 'function') {
        return () => buildStationSnapshot();
      }
      const root = typeof window !== 'undefined' ? window : (typeof globalThis !== 'undefined' ? globalThis : null);
      if (root && typeof root.buildStationSnapshot === 'function') {
        return () => root.buildStationSnapshot();
      }
      const probe = root && root.stationProbe;
      if (probe && typeof probe.snapshot === 'function') {
        return () => probe.snapshot();
      }
      return null;
    };
    const collectSessionCandidates = (snapshot) => {
      const candidates = [];
      try { collectMapCandidates(candidates, typeof sessionMetadataById !== 'undefined' ? sessionMetadataById : null, 'sessionMetadataById'); } catch (_) {}
      try { collectMapCandidates(candidates, typeof sessionWindows !== 'undefined' ? sessionWindows : null, 'sessionWindows'); } catch (_) {}
      try { collectArrayCandidates(candidates, typeof _cachedSessions !== 'undefined' ? _cachedSessions : null, '_cachedSessions'); } catch (_) {}
      try {
        const cache = typeof sessionsListCache !== 'undefined' ? sessionsListCache : null;
        if (cache && typeof cache.forEach === 'function') {
          cache.forEach((value) => collectArrayCandidates(candidates, value, 'sessionsListCache'));
        }
      } catch (_) {}
      collectArrayCandidates(candidates, snapshot.sessions && snapshot.sessions.recent, 'snapshot.sessions.recent');
      collectArrayCandidates(candidates, snapshot.sessions && snapshot.sessions.filteredSessions, 'snapshot.sessions.filteredSessions');
      collectArrayCandidates(candidates, snapshot.sessions && snapshot.sessions.externalTargets, 'snapshot.sessions.externalTargets');
      if (snapshot.context) candidates.push(candidateFrom(snapshot.context, { evidence: 'snapshot.context' }));
      if (snapshot.managed) candidates.push(candidateFrom(snapshot.managed, { evidence: 'snapshot.managed' }));
      if (snapshot.controls) {
        candidates.push(candidateFrom(snapshot.controls, {
          source: snapshot.controls.backend || snapshot.controls.agent || '',
          evidence: 'snapshot.controls',
        }));
      }

      const agents = Array.isArray(snapshot.agents) ? snapshot.agents : [];
      const agentWithModel = agents.find(agent => usableText(agent && agent.provider) && usableText(agent && agent.model));
      const snapshotSession = (snapshot.sessions && (
        (Array.isArray(snapshot.sessions.recent) && snapshot.sessions.recent[0]) ||
        (Array.isArray(snapshot.sessions.externalTargets) && snapshot.sessions.externalTargets[0]) ||
        null
      )) || {};
      if (agentWithModel) {
        candidates.push(candidateFrom(snapshotSession, {
          sessionId: snapshot.managed && snapshot.managed.sessionId || snapshotSession.id || snapshotSession.sessionId || snapshot.sessions && snapshot.sessions.latestSessionId || '',
          provider: agentWithModel.provider,
          model: agentWithModel.model,
          source: snapshot.sessions && snapshot.sessions.latestSource || snapshot.managed && (snapshot.managed.backendSource || snapshot.managed.backend_source) || snapshot.context && (snapshot.context.backendSource || snapshot.context.backend_source) || agentWithModel.role || '',
          status: agentWithModel.status || agentWithModel.phase || '',
          evidence: 'snapshot.agents+sessions',
        }));
      }
      try {
        const statusProvider = document.getElementById('sb-provider') && document.getElementById('sb-provider').textContent;
        const statusModel = document.getElementById('sb-model') && document.getElementById('sb-model').textContent;
        const currentId = typeof daemonSessionFullId !== 'undefined' && daemonSessionFullId
          || typeof currentSessionFullId !== 'undefined' && currentSessionFullId
          || typeof foregroundSessionFullId !== 'undefined' && foregroundSessionFullId
          || '';
        candidates.push(candidateFrom({}, {
          sessionId: currentId,
          provider: statusProvider,
          model: statusModel,
          source: 'status-bar',
          status: typeof currentPhase !== 'undefined' ? currentPhase : '',
          evidence: 'status-bar',
        }));
      } catch (_) {}
      return candidates;
    };

    const liveAgentSession = (candidates) => {
      const winner = candidates.find(candidate => candidate.sessionId && candidate.provider && candidate.model);
      if (winner) {
        return {
          ok: true,
          sessionId: winner.sessionId,
          provider: winner.provider,
          model: winner.model,
          source: winner.source,
          status: winner.status,
          evidence: winner.evidence,
          candidates: candidates.length,
        };
      }
      const withSession = candidates.filter(candidate => candidate.sessionId).length;
      const withProvider = candidates.filter(candidate => candidate.provider).length;
      const withModel = candidates.filter(candidate => candidate.model).length;
      return {
        ok: false,
        reason: `found ${withSession} session id candidate(s), ${withProvider} provider candidate(s), ${withModel} model candidate(s), but no single non-placeholder provider/model/session`,
        candidates: candidates.length,
      };
    };
    const candidateExternalSource = (candidate, requiredAgent) => {
      for (const source of candidate.sourceCandidates || [candidate.source]) {
        if (normalizeExternalAgentId(source) === requiredAgent) return source;
      }
      return '';
    };
    const externalAgentSession = (candidates, requiredAgent) => {
      if (!requiredAgent) return undefined;
      const sourceMatches = candidates
        .map(candidate => ({ candidate, source: candidateExternalSource(candidate, requiredAgent) }))
        .filter(match => match.source);
      const winner = sourceMatches
        .map(match => ({ ...match.candidate, matchedSource: match.source }))
        .find(candidate => candidate.sessionId && candidate.provider && candidate.model);
      if (winner) {
        return {
          ok: true,
          required: requiredAgent,
          agent: normalizeExternalAgentId(winner.matchedSource),
          sessionId: winner.sessionId,
          backendSessionId: winner.backendSessionId,
          intendantSessionId: winner.intendantSessionId,
          provider: winner.provider,
          model: winner.model,
          source: winner.matchedSource,
          status: winner.status,
          evidence: winner.evidence,
          candidates: candidates.length,
          matches: sourceMatches.length,
        };
      }
      const realSessions = candidates.filter(candidate => candidate.sessionId && candidate.provider && candidate.model).length;
      const sourceSummary = compactUniqueText(candidates.flatMap(candidate => candidate.sourceCandidates || [candidate.source])).slice(0, 8).join(', ');
      return {
        ok: false,
        required: requiredAgent,
        reason: `required external agent ${requiredAgent}, found ${sourceMatches.length} matching source candidate(s) and ${realSessions} real provider/model/session candidate(s), but no single real session tied to that external backend${sourceSummary ? ` (sources: ${sourceSummary})` : ''}`,
        candidates: candidates.length,
        matches: sourceMatches.length,
      };
    };
    const managedContextState = (snapshot, managedCount) => {
      const managed = snapshot.managed && typeof snapshot.managed === 'object' ? snapshot.managed : {};
      const context = snapshot.context && typeof snapshot.context === 'object' ? snapshot.context : {};
      const controls = snapshot.controls && typeof snapshot.controls === 'object' ? snapshot.controls : {};
      const sessionId = sessionIdText(
        managed.sessionId || managed.session_id || context.sessionId || context.session_id ||
        managed.intendantSessionId || managed.intendant_session_id || context.intendantSessionId || context.intendant_session_id ||
        controls.sessionLiveId || controls.session_live_id || controls.sessionId || controls.session_id
      );
      const managedStatus = asText(managed.status || managed.phase || managed.contextStatus || managed.context_status);
      const contextStatus = asText(context.status || context.phase || context.contextStatus || context.context_status || context.exactStatus || context.exact_status);
      const controlsStatus = asText(controls.sessionLivePhase || controls.session_live_phase || controls.sessionStatus || controls.session_status);
      const status = managedStatus || contextStatus || controlsStatus;
      const mode = asText(
        managed.mode || managed.managedMode || managed.managed_mode || managed.configuredMode || managed.configured_mode ||
        context.managedMode || context.managed_mode ||
        controls.sessionManagedContext || controls.session_managed_context
      );
      const replayMode = asText(
        managed.replayMode || managed.replay_mode || context.replayMode || context.replay_mode
      );
      const active = Boolean(
        managed.active || context.active || controls.sessionActive || controls.session_active
      );
      const live = Boolean(managed.live || context.live || active);
      const counts = managedCountDetails(managed);
      const normalizedStatus = asText(status).toLowerCase().replace(/[_\s]+/g, '-');
      const normalizedMode = asText(mode).toLowerCase().replace(/[_\s]+/g, '-');
      const normalizedReplayMode = asText(replayMode).toLowerCase().replace(/[_\s]+/g, '-');
      const terminalStatus = !normalizedStatus || /^(?:-|--|n\/a|na|null|undefined|unknown|none|stale|historical|history|archived|replay|snapshot|stopped|inactive|ended|complete|completed|done|idle|detached|unavailable|empty|missing|closed|finished|orphaned)$/.test(normalizedStatus);
      const staleStatus = /\b(?:stale|historical|archived|replay|snapshot|stopped|inactive|ended|complete|completed|done|idle|detached|unavailable|empty|missing|closed|finished|orphaned)\b/i.test(status);
      const liveReplay = !normalizedReplayMode || normalizedReplayMode === 'live';
      const managedMode = normalizedMode === 'managed';
      const ok = Boolean(sessionId && managedMode && live && liveReplay && !terminalStatus && !staleStatus);
      return {
        ok,
        live,
        active,
        sessionId,
        status,
        mode,
        replayMode,
        counts,
        reason: ok
          ? 'passed'
          : `no active live managed/context state (live=${live}, active=${active}, sessionId=${sessionId || 'none'}, mode=${mode || 'none'}, status=${status || 'none'}, replayMode=${replayMode || 'none'}, managedCount=${managedCount})`,
      };
    };
    try {
      const buildSnapshot = stationSnapshotBuilder();
      const snapshot = buildSnapshot ? buildSnapshot() : null;
      if (!snapshot || typeof snapshot !== 'object') {
        return { ok: false, reason: 'Station snapshot hook is unavailable (checked buildStationSnapshot and window.stationProbe.snapshot)' };
      }
      const sessions = Number(snapshot.sessions && snapshot.sessions.total) || 0;
      const events = Math.max(
        countArray(snapshot.events),
        Number(snapshot.activity && snapshot.activity.retainedCount) || 0,
        Number(snapshot.activity && snapshot.activity.shownCount) || 0,
      );
      const managed = countManaged(snapshot.managed);
      const peers = Math.max(
        countArray(snapshot.hosts) > 0 ? countArray(snapshot.hosts) - 1 : 0,
        countArray(snapshot.agents) > 0 ? snapshot.agents.filter(agent => !/^primary-agent$/.test(String(agent && agent.id || ''))).length : 0,
        countMap(typeof daemons !== 'undefined' ? daemons : null),
      );
      const counts = { sessions, events, managed, peers };
      const candidates = collectSessionCandidates(snapshot);
      const requiredExternalAgent = normalizeExternalAgentId(requiredExternalAgentRaw);
      const managedState = managedContextState(snapshot, managed);
      return {
        ok: true,
        nonEmpty: sessions > 0 || events > 0 || managed > 0 || peers > 0,
        counts,
        latestSession: snapshot.sessions && snapshot.sessions.latestTask || '',
        managedSessionId: snapshot.managed && snapshot.managed.sessionId || '',
        liveAgentSession: liveAgentSession(candidates),
        externalAgentSession: externalAgentSession(candidates, requiredExternalAgent),
        managedContextState: managedState,
      };
    } catch (error) {
      return {
        ok: false,
        reason: error && error.message ? error.message : String(error),
      };
    }
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
  if (compact.stationInteraction) {
    compact.stationInteraction = compactStationInteraction(compact.stationInteraction);
  }
  if (compact.stationProbeReports) {
    compact.stationProbeReports = compactStationProbeReports(compact.stationProbeReports);
  }
  if (compact.stationActivation) {
    compact.stationActivation = compactStationActivation(compact.stationActivation);
  }
  if (compact.stationPerfEval) {
    compact.stationPerfEval = compactStationPerfEval(compact.stationPerfEval);
  }
  if (compact.stationState) {
    compact.stationState = compactStationState(compact.stationState);
  }
  if (compact.diagnostics) {
    compact.diagnostics = compactDiagnostics(compact.diagnostics, {
      suppressControls: isStationFocusedCheck(opts) && !opts.diagnostics,
    });
  }
  if (compact.harnessFallbacks) {
    compact.harnessFallbacks = compactHarnessFallbacks(compact.harnessFallbacks);
  }
  if (compact.status === 'fail') {
    compact.next = validationFailureNextStep(compact);
  }
  return compact;
}

function compactHarnessFallbacks(fallbacks) {
  if (!fallbacks || typeof fallbacks !== 'object') {
    return fallbacks;
  }
  const compact = {
    stationRuntimeEvaluateTimeout: Boolean(fallbacks.stationRuntimeEvaluateTimeout),
  };
  if (fallbacks.screenshot && typeof fallbacks.screenshot === 'object') {
    compact.screenshot = {
      status: String(fallbacks.screenshot.status || ''),
      mode: String(fallbacks.screenshot.mode || ''),
    };
    if (fallbacks.screenshot.reason) {
      compact.screenshot.reason = truncateMiddle(fallbacks.screenshot.reason, RESULT_REASON_LIMIT);
    }
  }
  return compact;
}

function compactStationState(state) {
  if (!state || typeof state !== 'object') {
    return state;
  }
  const counts = state.counts || {};
  return {
    ok: Boolean(state.ok),
    nonEmpty: Boolean(state.nonEmpty),
    counts: {
      sessions: Number(counts.sessions) || 0,
      events: Number(counts.events) || 0,
      managed: Number(counts.managed) || 0,
      peers: Number(counts.peers) || 0,
    },
    latestSession: truncateMiddle(state.latestSession || '', DIAGNOSTIC_TEXT_LIMIT),
    managedSessionId: truncateMiddle(state.managedSessionId || '', DIAGNOSTIC_TEXT_LIMIT),
    liveAgentSession: compactLiveAgentSession(state.liveAgentSession),
    externalAgentSession: compactExternalAgentSession(state.externalAgentSession),
    managedContextState: compactManagedContextState(state.managedContextState),
  };
}

function compactManagedContextState(state) {
  if (!state || typeof state !== 'object') {
    return state;
  }
  const counts = state.counts || {};
  return {
    ok: Boolean(state.ok),
    live: Boolean(state.live),
    active: Boolean(state.active),
    sessionId: truncateMiddle(state.sessionId || '', DIAGNOSTIC_TEXT_LIMIT),
    status: truncateMiddle(state.status || '', DIAGNOSTIC_TEXT_LIMIT),
    mode: truncateMiddle(state.mode || '', DIAGNOSTIC_TEXT_LIMIT),
    replayMode: truncateMiddle(state.replayMode || '', DIAGNOSTIC_TEXT_LIMIT),
    counts: {
      records: Number(counts.records) || 0,
      anchors: Number(counts.anchors) || 0,
      lineageGroups: Number(counts.lineageGroups) || 0,
      fissionGroups: Number(counts.fissionGroups) || 0,
      recentRecords: Number(counts.recentRecords) || 0,
      recentAnchors: Number(counts.recentAnchors) || 0,
      recentBranches: Number(counts.recentBranches) || 0,
    },
    reason: truncateMiddle(state.reason || '', DIAGNOSTIC_TEXT_LIMIT),
  };
}

function compactLiveAgentSession(session) {
  if (!session || typeof session !== 'object') {
    return session;
  }
  return {
    ok: Boolean(session.ok),
    sessionId: truncateMiddle(session.sessionId || '', DIAGNOSTIC_TEXT_LIMIT),
    provider: truncateMiddle(session.provider || '', DIAGNOSTIC_TEXT_LIMIT),
    model: truncateMiddle(session.model || '', DIAGNOSTIC_TEXT_LIMIT),
    source: truncateMiddle(session.source || '', DIAGNOSTIC_TEXT_LIMIT),
    status: truncateMiddle(session.status || '', DIAGNOSTIC_TEXT_LIMIT),
    evidence: truncateMiddle(session.evidence || '', DIAGNOSTIC_TEXT_LIMIT),
    reason: truncateMiddle(session.reason || '', DIAGNOSTIC_TEXT_LIMIT),
    candidates: Number(session.candidates) || 0,
  };
}

function compactExternalAgentSession(session) {
  if (!session || typeof session !== 'object') {
    return session;
  }
  return {
    ok: Boolean(session.ok),
    required: truncateMiddle(session.required || '', DIAGNOSTIC_TEXT_LIMIT),
    agent: truncateMiddle(session.agent || '', DIAGNOSTIC_TEXT_LIMIT),
    sessionId: truncateMiddle(session.sessionId || '', DIAGNOSTIC_TEXT_LIMIT),
    backendSessionId: truncateMiddle(session.backendSessionId || '', DIAGNOSTIC_TEXT_LIMIT),
    intendantSessionId: truncateMiddle(session.intendantSessionId || '', DIAGNOSTIC_TEXT_LIMIT),
    provider: truncateMiddle(session.provider || '', DIAGNOSTIC_TEXT_LIMIT),
    model: truncateMiddle(session.model || '', DIAGNOSTIC_TEXT_LIMIT),
    source: truncateMiddle(session.source || '', DIAGNOSTIC_TEXT_LIMIT),
    status: truncateMiddle(session.status || '', DIAGNOSTIC_TEXT_LIMIT),
    evidence: truncateMiddle(session.evidence || '', DIAGNOSTIC_TEXT_LIMIT),
    reason: truncateMiddle(session.reason || '', DIAGNOSTIC_TEXT_LIMIT),
    candidates: Number(session.candidates) || 0,
    matches: Number(session.matches) || 0,
  };
}

function compactStationInteraction(interaction) {
  if (!interaction || typeof interaction !== 'object') {
    return interaction;
  }
  const compact = {
    status: interaction.status || '',
    count: Number(interaction.count) || 0,
    names: Array.isArray(interaction.names) ? interaction.names.slice(0, 8).map(String) : [],
    warmupLatencyMs: Number(interaction.warmupLatencyMs) || 0,
    subsequentLatencyMs: Number(interaction.subsequentLatencyMs) || 0,
  };
  if (Array.isArray(interaction.interactions)) {
    compact.interactions = interaction.interactions.slice(0, 8).map((item) => ({
      name: String(item.name || ''),
      control: truncateMiddle(item.control || '', DIAGNOSTIC_TEXT_LIMIT),
      input: String(item.input || ''),
      x: Number(item.x) || 0,
      y: Number(item.y) || 0,
      latencyMs: Number(item.latencyMs) || 0,
      state: compactStationInteractionState(item.state),
    }));
    if (interaction.interactions.length > compact.interactions.length) {
      compact.interactionsOmitted = interaction.interactions.length - compact.interactions.length;
    }
  }
  return compact;
}

function compactStationInteractionState(state) {
  if (!state || typeof state !== 'object') {
    return state;
  }
  return {
    ok: Boolean(state.ok),
    target: String(state.target || ''),
    activeTab: String(state.activeTab || ''),
    statusText: truncateMiddle(state.statusText || '', DIAGNOSTIC_TEXT_LIMIT),
    statusMatches: Boolean(state.statusMatches),
    settleMs: Number(state.settleMs) || 0,
    reason: truncateMiddle(state.reason || '', DIAGNOSTIC_TEXT_LIMIT),
  };
}

function compactStationProbeReports(reports) {
  if (!reports || typeof reports !== 'object') {
    return reports;
  }
  const compact = {};
  if (reports.fps) {
    compact.fps = {
      fps: Number(reports.fps.fps) || 0,
      minFps: Number(reports.fps.minFps) || 0,
    };
  }
  if (reports.smooth) {
    compact.smooth = compactStationSmoothReport(reports.smooth);
  }
  if (reports['debug-json']) {
    compact['debug-json'] = compactStationDebugJsonReport(reports['debug-json']);
  }
  return compact;
}

function compactStationActivation(activation) {
  if (!activation || typeof activation !== 'object') {
    return activation;
  }
  const compact = {
    status: activation.status || '',
    count: Number(activation.count) || 0,
    names: Array.isArray(activation.names) ? activation.names.slice(0, 8).map(String) : [],
    warmupLatencyMs: Number(activation.warmupLatencyMs) || 0,
    subsequentLatencyMs: Number(activation.subsequentLatencyMs) || 0,
  };
  if (Array.isArray(activation.activations)) {
    compact.activations = activation.activations.slice(0, 8).map((item) => ({
      name: String(item.name || ''),
      input: String(item.input || ''),
      verifiedVia: String(item.verifiedVia || ''),
      selectedId: truncateMiddle(item.selectedId || '', DIAGNOSTIC_TEXT_LIMIT),
      latencyMs: Number(item.latencyMs) || 0,
      settleMs: Number(item.settleMs) || 0,
    }));
    if (activation.activations.length > compact.activations.length) {
      compact.activationsOmitted = activation.activations.length - compact.activations.length;
    }
  }
  return compact;
}

function compactStationPerfEval(report) {
  if (!report || typeof report !== 'object') {
    return report;
  }
  const compact = {
    fpsIdle: Number(report.fpsIdle) || 0,
    fpsAfterInteraction: Number(report.fpsAfterInteraction) || 0,
    p50Idle: Number(report.p50Idle) || 0,
    p95Idle: Number(report.p95Idle) || 0,
    worstIdle: Number(report.worstIdle) || 0,
    p50Active: Number(report.p50Active) || 0,
    p95Active: Number(report.p95Active) || 0,
    worstActive: Number(report.worstActive) || 0,
    framesIdle: Number(report.framesIdle) || 0,
    framesActive: Number(report.framesActive) || 0,
    interactionTargets: Array.isArray(report.interactionTargets)
      ? report.interactionTargets.slice(0, 8).map(String)
      : [],
    interactionLatencies: Array.isArray(report.interactionLatencies)
      ? report.interactionLatencies.slice(0, 8).map((value) => Number(value) || 0)
      : [],
    interactionInput: String(report.interactionInput || ''),
    thresholds: {
      minFps: Number(report.thresholds && report.thresholds.minFps) || 0,
      maxFrameGapMs: Number(report.thresholds && report.thresholds.maxFrameGapMs) || 0,
      hardGapLimitMs: Number(report.thresholds && report.thresholds.hardGapLimitMs) || 0,
    },
    failures: compactStringArray(report.failures, DIAGNOSTIC_LIST_LIMIT, DIAGNOSTIC_TEXT_LIMIT).values,
    verdict: String(report.verdict || ''),
  };
  if (report.displays !== undefined) {
    compact.displays = Number(report.displays) || 0;
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
  if (result.failureKind === 'interaction') {
    return 'treat as headed interaction validation failure; use the reported hotspot state/latency facts instead of scraping DevToolsActivePort or switching to desktop portal screenshots';
  }
  if (result.failureKind === 'perf-eval') {
    return 'treat as a Station performance regression: compare the emitted perf-eval JSON report against a pre-change baseline run, and bisect renderer/display changes instead of loosening --station-min-fps/--station-max-frame-gap';
  }
  if (result.failureKind === 'station-state') {
    return 'target a populated managed Station controller, or rerun with --allow-empty-station-state only for renderer smoke tests';
  }
  if (result.failureKind === 'ai-provider-session') {
    return 'target a real managed Station session with populated provider/model metadata; do not count no-provider or placeholder sessions as product E2E success';
  }
  if (result.failureKind === 'external-agent-session') {
    return 'target a live Station session backed by the requested external agent; do not count native/default provider sessions as external-agent QA success';
  }
  if (result.failureKind === 'managed-context-state') {
    return 'target a Station session with active managed/context state; empty or stopped managed context does not satisfy product E2E';
  }
  if (result.failureKind === 'stale-static') {
    return 'rebuild and restart the dashboard controller from this worktree, then rerun with --require-current-static';
  }
  if (result.failureKind === 'harness' && isCdpRuntimeEvaluateTimeout(result.reason || '')) {
    return 'treat as browser harness Runtime.evaluate timeout; use the screenshot artifact/fallback facts as product evidence and inspect browser/GPU stability before rerunning';
  }
  if (result.failureKind === 'harness' && isCdpScreenshotTimeout(result.reason || '')) {
    return 'treat as browser harness screenshot failure after the built-in single retry; keep Station validator failures separate and inspect browser/GPU stability';
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
  if (/^station interaction probe/.test(text) || /station interaction probe impossible/.test(text)) {
    return 'interaction';
  }
  if (/^station activate probe/.test(text)) {
    return 'interaction';
  }
  if (/^station perf eval did not pass/.test(text)) {
    return 'perf-eval';
  }
  if (/^station state check failed/.test(text)) {
    return 'station-state';
  }
  if (/^ai provider session check failed/.test(text)) {
    return 'ai-provider-session';
  }
  if (/^external agent session check failed/.test(text)) {
    return 'external-agent-session';
  }
  if (/^managed context state check failed/.test(text)) {
    return 'managed-context-state';
  }
  if (/^stale dashboard static asset/.test(text)) {
    return 'stale-static';
  }
  if (isCdpScreenshotTimeout(text)) {
    return 'harness';
  }
  if (/^screenshot capture/.test(text)) {
    return 'artifact';
  }
  if (/^station probe .* did not pass/.test(text)) {
    if (isCdpRuntimeEvaluateTimeout(text)) {
      return 'harness';
    }
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
  if (isCdpRuntimeEvaluateTimeout(reason)) {
    return false;
  }
  return isWaitFailureReason(reason);
}

function isWaitFailureReason(reason) {
  return /^selector not found: /.test(String(reason || ''))
    || /^wait-for-function did not become truthy/.test(String(reason || ''))
    || /^station probe .* did not pass/.test(String(reason || ''))
    || /^station state check failed/.test(String(reason || ''))
    || /^external agent session check failed/.test(String(reason || ''))
    || /^station interaction probe/.test(String(reason || ''))
    || /^station activate probe/.test(String(reason || ''))
    || /^station perf eval did not pass/.test(String(reason || ''));
}

function failureDiagnosticSelectors(opts) {
  const selectors = [];
  for (const selector of opts.selectors || []) {
    addDiagnosticSelector(selectors, selector);
  }
  for (const selector of diagnosticSelectorsFromWaitFunctions(opts.functions || [])) {
    addDiagnosticSelector(selectors, selector);
  }
  if (stationSurfaceRequired(opts)) {
    for (const selector of ['#station-status', '#station-hud-canvas', '[data-station-hotspot]']) {
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
    opts.stationInteractionProbe ? 'station:interaction' : '',
    (opts.stationActivateTargets || []).length ? 'station:activate' : '',
    opts.stationPerfEval ? 'station:perf-eval' : '',
    opts.requireStationState ? 'station:state' : '',
    opts.requireAiProviderSession ? 'station:ai-provider-session' : '',
    opts.requireExternalAgent ? `station:external-agent:${opts.requireExternalAgent}` : '',
    opts.requireManagedContextState ? 'station:managed-context-state' : '',
  ].join('\n').toLowerCase();
  // Any mention of "station" (selectors, wait functions, or the synthetic
  // station:* tokens above) marks the run as Station-focused; the previous
  // station-status/station-hud-canvas/station-dock checks were subsumed by it.
  return haystack.includes('station');
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

function waitForProcessExit(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) {
    return Promise.resolve();
  }
  return new Promise((resolve) => {
    child.once('exit', resolve);
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
      '--screenshot',
      '/tmp/station-shot.png',
      '--station-hotspot-probe',
      '--keep-browser',
      '--enable-gpu',
      '--browser-arg=--ozone-platform=x11',
      '--require-current-static',
      '--require-station-state',
      '--require-ai-provider-session',
      '--require-external-agent',
      'codex',
      '--require-managed-context-state',
      '--allow-empty-station-state',
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
  assert.strictEqual(parsed.screenshotPath, path.resolve('/tmp/station-shot.png'));
  assert.strictEqual(parsed.stationInteractionProbe, true);
  assert.strictEqual(parsed.keepArtifacts, true);
  assert.strictEqual(parsed.keepBrowser, true);
  assert.strictEqual(parsed.enableGpu, true);
  assert.strictEqual(parsed.requireCurrentStatic, true);
  assert.strictEqual(parsed.requireStationState, true);
  assert.strictEqual(parsed.requireAiProviderSession, true);
  assert.strictEqual(parsed.requireExternalAgent, 'codex');
  assert.strictEqual(parsed.requireManagedContextState, true);
  assert.strictEqual(parsed.allowEmptyStationState, true);
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
  assert.throws(
    () => parseArgs(['--station-probe', 'dock'], {}),
    /unknown Station probe/,
  );
  assert.throws(
    () => parseArgs(['--station-probe', 'dock-controls'], {}),
    /unknown Station probe/,
  );
  assert.throws(
    () => parseArgs(['--station-probe', 'dock-nav'], {}),
    /unknown Station probe/,
  );
  assert.deepStrictEqual(parseArgs(['--station-probe', 'hidden-dock'], {}).stationProbes, ['dock-hidden']);
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
  const holdParsed = parseArgs(['--hold-dashboard', '--port', '8894', '--json'], {});
  assert.strictEqual(holdParsed.launchDashboard, true);
  assert.strictEqual(holdParsed.holdDashboard, true);
  assert.strictEqual(dashboardLaunchPort(holdParsed), 8894);
  assert.strictEqual(holdParsed.url, 'http://127.0.0.1:8894/');
  assert.strictEqual(staticScriptsOnly(holdParsed), false);
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
  assert.strictEqual(
    validationFailureKind('station probe rendered did not pass (last error: CDP Runtime.evaluate timed out)'),
    'harness',
  );
  assert.strictEqual(
    validationFailureKind('station interaction probe system:view did not pass (last value: false)'),
    'interaction',
  );
  assert.strictEqual(isCdpScreenshotTimeout(new Error('CDP Page.captureScreenshot timed out')), true);
  assert.strictEqual(isCdpScreenshotTimeout('screenshot capture returned no image data'), false);
  assert.strictEqual(
    validationFailureKind('CDP Page.captureScreenshot timed out'),
    'harness',
  );
  assert.strictEqual(isCdpRuntimeEvaluateTimeout(new Error('CDP Runtime.evaluate timed out')), true);
  assert.strictEqual(
    shouldCaptureStationRuntimeTimeoutScreenshot(
      { screenshotPath: '/tmp/station-timeout.png', stationProbes: ['rendered'], functions: [], selectors: [] },
      new Error('CDP Runtime.evaluate timed out'),
    ),
    true,
  );
  assert.strictEqual(
    shouldCaptureStationRuntimeTimeoutScreenshot(
      { stationProbes: ['rendered'], functions: [], selectors: [] },
      new Error('CDP Runtime.evaluate timed out'),
    ),
    false,
  );
  const screenshotTimeout = screenshotTimeoutError(new Error('CDP Page.captureScreenshot timed out'), {
    retried: true,
    liveness: {
      alive: true,
      readyState: 'complete',
      href: 'http://127.0.0.1:8956/#station',
    },
  });
  assert.ok(screenshotTimeout.message.includes('after one retry'));
  assert.strictEqual(validationFailureKind(screenshotTimeout.message), 'harness');
  assert.ok(
    validationFailureNextStep({
      failureKind: 'harness',
      reason: screenshotTimeout.message,
    }).includes('built-in single retry'),
  );
  assert.ok(
    validationFailureNextStep({
      failureKind: 'harness',
      reason: 'CDP Runtime.evaluate timed out',
    }).includes('Runtime.evaluate timeout'),
  );
  assert.strictEqual(
    validationFailureKind('screenshot capture returned no image data'),
    'artifact',
  );
  assert.ok(
    validationFailureNextStep({
      failureKind: 'interaction',
      reason: 'station interaction probe system:view did not pass',
    }).includes('DevToolsActivePort'),
  );
  assert.ok(pageDiagnosticsSource().includes('selectorMatches'));
  assert.ok(pageDiagnosticsSource().includes('dataset'));
  assert.ok(stationDiagnosticsSource().includes('station-hud-canvas'));
  assert.ok(stationStateSummarySource().includes('buildStationSnapshot'));
  assert.ok(stationHotspotPointSource().includes('stationHotspotBoxes'));
  assert.ok(stationHotspotActivateSource().includes('MouseEvent'));
  const stationProbeDocumentFor = (dock) => ({
    getElementById: (id) => (id === 'station-dock' ? dock : null),
    querySelector: () => null,
  });
  // Dock-less Station is the expected state: no #station-dock in the DOM.
  const stationProbe = vm.runInNewContext(`(${stationProbeSource()})`, {
    document: stationProbeDocumentFor(null),
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
  const absentDockResult = stationProbe('dock-hidden', stationProbeDiagnostics);
  assert.strictEqual(absentDockResult.ok, true);
  assert.strictEqual(absentDockResult.dock.found, false);
  assert.strictEqual(stationProbe('rendered', stationProbeDiagnostics).ok, true);
  assert.strictEqual(stationProbe('webgpu', stationProbeDiagnostics).ok, true);
  const fpsDiag = { ...stationProbeDiagnostics, statusText: 'station hosts=1 renderer=WebGPU gpu=true fps=58' };
  const fpsPass = stationProbe('fps', fpsDiag, { minFps: 24 });
  assert.strictEqual(fpsPass.ok, true);
  assert.strictEqual(fpsPass.fps, 58);
  const fpsSlow = stationProbe('fps', { ...stationProbeDiagnostics, statusText: 'station fps=9' }, { minFps: 24 });
  assert.strictEqual(fpsSlow.ok, false);
  assert.ok(/9fps/.test(fpsSlow.reason));
  const fpsMissing = stationProbe('fps', stationProbeDiagnostics, { minFps: 24 });
  assert.strictEqual(fpsMissing.ok, false);
  assert.ok(/no fps figure/.test(fpsMissing.reason));
  assert.strictEqual(stationProbe('fps', fpsDiag, undefined).ok, true);
  assert.deepStrictEqual(parseArgs(['--station-probe', 'framerate'], {}).stationProbes, ['fps']);
  assert.strictEqual(parseArgs(['--station-min-fps', '30'], {}).stationMinFps, 30);
  assert.strictEqual(parseArgs([], {}).stationMinFps, DEFAULT_STATION_MIN_FPS);
  assert.strictEqual(parseArgs([], {}).stationMaxFrameGapMs, DEFAULT_STATION_MAX_FRAME_GAP_MS);
  assert.strictEqual(parseArgs(['--station-max-frame-gap', '25'], {}).stationMaxFrameGapMs, 25);
  assert.strictEqual(parseArgs(['--station-max-frame-gap=60'], {}).stationMaxFrameGapMs, 60);
  assert.strictEqual(parseArgs(['--require-debug-json'], {}).requireDebugJson, true);
  assert.strictEqual(parseArgs([], {}).requireDebugJson, false);
  assert.strictEqual(parseArgs(['--station-perf-eval'], {}).stationPerfEval, true);
  assert.deepStrictEqual(
    parseArgs(['--station-probe', 'rendered,webgpu,fps,smooth'], {}).stationProbes,
    ['rendered', 'webgpu', 'fps', 'smooth'],
  );
  assert.deepStrictEqual(parseArgs(['--station-probe', 'frame-pacing'], {}).stationProbes, ['smooth']);
  assert.deepStrictEqual(parseArgs(['--station-probe', 'debug_json,smooth,'], {}).stationProbes, ['debug-json', 'smooth']);
  assert.throws(() => parseArgs(['--station-probe', ','], {}), /requires at least one probe name/);
  assert.throws(() => parseArgs(['--station-probe', 'rendered,everything'], {}), /unknown Station probe/);
  assert.deepStrictEqual(
    parseArgs(['--station-activate', 'system:view', '--station-activate=system:context'], {}).stationActivateTargets,
    ['system:view', 'system:context'],
  );
  assert.throws(() => parseArgs(['--station-activate', '  '], {}), /requires a target name/);
  assert.strictEqual(stationSurfaceRequired(parseArgs(['--station-perf-eval', '--port', '7777'], {})), true);
  assert.strictEqual(stationSurfaceRequired(parseArgs(['--station-activate', 'system:view', '--port', '7777'], {})), true);
  assert.strictEqual(stationSurfaceRequired(parseArgs(['--port', '7777'], {})), false);
  assert.strictEqual(
    staticScriptsOnly(parseArgs(['--check-static-scripts', '--station-perf-eval'], {
      INTENDANT_MCP_URL: 'http://127.0.0.1:7777/mcp',
    })),
    false,
  );
  assert.strictEqual(
    staticScriptsOnly(parseArgs(['--check-static-scripts', '--station-activate', 'system:view'], {
      INTENDANT_MCP_URL: 'http://127.0.0.1:7777/mcp',
    })),
    false,
  );

  // smooth probe: scripted requestAnimationFrame timestamps drive the in-page sampler.
  const smoothOptionsDefaults = stationSmoothOptions(parseArgs([], {}));
  assert.strictEqual(smoothOptionsDefaults.sampleMs, STATION_SMOOTH_SAMPLE_MS);
  assert.strictEqual(smoothOptionsDefaults.maxFrameGapMs, DEFAULT_STATION_MAX_FRAME_GAP_MS);
  assert.strictEqual(smoothOptionsDefaults.hardGapLimitMs, STATION_SMOOTH_HARD_GAP_LIMIT_MS);
  assert.strictEqual(stationSmoothOptions(parseArgs(['--station-max-frame-gap', '25'], {})).maxFrameGapMs, 25);
  assert.ok(stationSmoothProbeExpression({ sampleMs: 100 }).includes('requestAnimationFrame'));
  const smoothSandboxFor = (timestamps) => {
    let index = 0;
    return {
      requestAnimationFrame: (callback) => {
        if (index < timestamps.length) {
          const ts = timestamps[index];
          index += 1;
          setImmediate(() => callback(ts));
        }
      },
      setTimeout,
      clearTimeout,
    };
  };
  const smoothStampsFrom = (deltas) => {
    const stamps = [0];
    let acc = 0;
    for (const delta of deltas) {
      acc += delta;
      stamps.push(acc);
    }
    return stamps;
  };
  const runSmoothSample = async (deltas) => {
    const stamps = smoothStampsFrom(deltas);
    const total = stamps[stamps.length - 1];
    return vm.runInNewContext(`(${stationSmoothProbeSource()})`, smoothSandboxFor(stamps))({
      sampleMs: total,
      maxFrameGapMs: 40,
      hardGapLimitMs: 250,
      stallTimeoutMs: 5000,
    });
  };
  const smoothPass = await runSmoothSample(Array(20).fill(16));
  assert.strictEqual(smoothPass.ok, true);
  assert.strictEqual(smoothPass.frames, 20);
  assert.strictEqual(smoothPass.sampleMs, 320);
  assert.strictEqual(smoothPass.fps, 63);
  assert.strictEqual(smoothPass.p50, 16);
  assert.strictEqual(smoothPass.p95, 16);
  assert.strictEqual(smoothPass.worst, 16);
  assert.deepStrictEqual(compactStationSmoothReport(smoothPass), {
    ok: true,
    frames: 20,
    sampleMs: 320,
    fps: 63,
    p50: 16,
    p95: 16,
    worst: 16,
    maxFrameGapMs: 40,
    hardGapLimitMs: 250,
  });
  const smoothP95Fail = await runSmoothSample([...Array(18).fill(16), 60, 60]);
  assert.strictEqual(smoothP95Fail.ok, false);
  assert.strictEqual(smoothP95Fail.failureKind, 'renderer');
  assert.strictEqual(smoothP95Fail.p95, 60);
  assert.ok(/p95 frame gap 60ms exceeds the 40ms budget/.test(smoothP95Fail.reason));
  const smoothHardFail = await runSmoothSample([...Array(10).fill(16), 300]);
  assert.strictEqual(smoothHardFail.ok, false);
  assert.strictEqual(smoothHardFail.worst, 300);
  assert.ok(/worst frame gap 300ms exceeds the 250ms stall limit/.test(smoothHardFail.reason));
  const smoothStalled = await vm.runInNewContext(`(${stationSmoothProbeSource()})`, {
    requestAnimationFrame: () => {},
    setTimeout,
    clearTimeout,
  })({ sampleMs: 300, stallTimeoutMs: 20 });
  assert.strictEqual(smoothStalled.ok, false);
  assert.strictEqual(smoothStalled.stalled, true);
  assert.strictEqual(smoothStalled.frames, 0);
  assert.ok(/requestAnimationFrame stalled/.test(smoothStalled.reason));
  assert.strictEqual(
    validationFailureKind('station probe smooth did not pass (last value: {"failureKind":"renderer"})'),
    'renderer',
  );

  // debug-json probe: station.debug_json() is live-only, so fixtures stub the
  // station handle directly (statusText/debug_state fixtures stay fps-token only).
  assert.ok(stationDebugJsonProbeExpression().includes('collectStationDebugJson'));
  assert.ok(stationDebugJsonProbeExpression({ require: true }).includes('"require":true'));
  const debugJsonProbeIn = (sandbox, options = {}) => vm.runInNewContext(
    `(${stationDebugJsonProbeSource()})`,
    sandbox,
  )({ requiredKeys: STATION_DEBUG_JSON_REQUIRED_KEYS, listLimit: STATION_DEBUG_JSON_LIST_LIMIT, ...options });
  const debugJsonFixture = {
    fps: 58,
    renderer: 'webgpu',
    gpu: true,
    hosts: [{ id: 'local' }],
    agents: [{ id: 'primary-agent' }, { id: 'peer-1' }],
    events: 5,
    displays: [{ id: 'd0' }],
    selectedId: 'system:view',
    layout: 'orbital',
    mood: 'calm',
    motion: 'normal',
    hitZones: [
      { name: 'system:activity', x: 1, y: 2, w: 3, h: 4 },
      { name: 'system:controls', x: 5, y: 6, w: 7, h: 8 },
    ],
    systemTargets: ['system:activity', 'system:controls', 'system:view'],
  };
  const debugJsonPass = debugJsonProbeIn({ station: { debug_json: () => debugJsonFixture } });
  assert.strictEqual(debugJsonPass.ok, true);
  assert.strictEqual(debugJsonPass.supported, true);
  assert.strictEqual(debugJsonPass.data.fps, 58);
  assert.strictEqual(debugJsonPass.data.renderer, 'webgpu');
  assert.strictEqual(debugJsonPass.data.gpu, true);
  assert.strictEqual(debugJsonPass.data.hosts, 1);
  assert.strictEqual(debugJsonPass.data.agents, 2);
  assert.strictEqual(debugJsonPass.data.events, 5);
  assert.strictEqual(debugJsonPass.data.displays, 1);
  assert.strictEqual(debugJsonPass.data.selectedId, 'system:view');
  assert.strictEqual(debugJsonPass.data.layout, 'orbital');
  assert.strictEqual(debugJsonPass.data.hitZones, 2);
  assert.deepStrictEqual(debugJsonPass.data.hitZoneNames, ['system:activity', 'system:controls']);
  assert.deepStrictEqual(debugJsonPass.data.systemTargets, ['system:activity', 'system:controls', 'system:view']);
  assert.strictEqual(debugJsonPass.data.systemTargetCount, 3);
  const debugJsonStringPass = debugJsonProbeIn({
    station: { debug_json: () => JSON.stringify(debugJsonFixture) },
  });
  assert.strictEqual(debugJsonStringPass.ok, true);
  assert.strictEqual(debugJsonStringPass.data.renderer, 'webgpu');
  const debugJsonMissingKeys = debugJsonProbeIn({ station: { debug_json: () => ({ fps: 30 }) } });
  assert.strictEqual(debugJsonMissingKeys.ok, false);
  assert.ok(/missing required key\(s\): renderer, hosts/.test(debugJsonMissingKeys.reason));
  const debugJsonBroken = debugJsonProbeIn({ station: { debug_json: () => '{nope' } });
  assert.strictEqual(debugJsonBroken.ok, false);
  assert.ok(/did not parse as JSON/.test(debugJsonBroken.reason));
  const debugJsonThrew = debugJsonProbeIn({
    station: { debug_json: () => { throw new Error('wasm panic'); } },
  });
  assert.strictEqual(debugJsonThrew.ok, false);
  assert.ok(/threw: .*wasm panic/.test(debugJsonThrew.reason));
  const debugJsonUnsupported = debugJsonProbeIn({ station: { debug_state: () => 'fps=58' } });
  assert.strictEqual(debugJsonUnsupported.ok, true);
  assert.strictEqual(debugJsonUnsupported.supported, false);
  assert.ok(/not supported by this build/.test(debugJsonUnsupported.reason));
  const debugJsonRequired = debugJsonProbeIn({ station: { debug_state: () => 'fps=58' } }, { require: true });
  assert.strictEqual(debugJsonRequired.ok, false);
  assert.strictEqual(debugJsonRequired.supported, false);
  assert.ok(/not supported by this build/.test(debugJsonRequired.reason));
  const debugJsonNoHandle = debugJsonProbeIn({ window: {} });
  assert.strictEqual(debugJsonNoHandle.ok, false);
  assert.ok(/Station WASM handle is unavailable/.test(debugJsonNoHandle.reason));
  const compactDebugJson = compactStationDebugJsonReport(debugJsonPass);
  assert.strictEqual(compactDebugJson.ok, true);
  assert.strictEqual(compactDebugJson.data.renderer, 'webgpu');
  assert.deepStrictEqual(compactDebugJson.data.systemTargets, ['system:activity', 'system:controls', 'system:view']);
  assert.strictEqual(
    validationFailureKind('station probe debug-json did not pass (last value: {"supported":true})'),
    'probe',
  );

  // station.activate() probe: programmatic activation verified via
  // debug_json selectedId or the status text.
  assert.ok(stationActivateWasmSource().includes('station.activate'));
  const wasmActivateDocFor = (statusElement) => ({
    getElementById: (id) => {
      if (id === 'tab-station') {
        return { classList: { contains: (name) => name === 'active' } };
      }
      if (id === 'station-status') {
        return statusElement;
      }
      return null;
    },
  });
  const runWasmActivate = (sandbox, target, budgetMs = 400) => vm.runInNewContext(
    `(${stationActivateWasmSource()})`,
    { Date, setTimeout, RegExp, String, Number, Math, JSON, ...sandbox },
  )(target, budgetMs);
  const activateImmediate = await runWasmActivate({
    document: wasmActivateDocFor({ textContent: 'station idle' }),
    station: {
      activate: (name) => name === 'system:view',
      debug_json: () => ({ fps: 60, renderer: 'webgpu', hosts: 1, selectedId: 'system:view' }),
    },
  }, 'system:view');
  assert.strictEqual(activateImmediate.ok, true);
  assert.strictEqual(activateImmediate.input, 'wasm-activate');
  assert.strictEqual(activateImmediate.verifiedVia, 'selectedId');
  assert.strictEqual(activateImmediate.settleMs, 0);
  const deferredWasmStatus = { textContent: 'station idle' };
  const activateDeferredWasm = await runWasmActivate({
    document: wasmActivateDocFor(deferredWasmStatus),
    station: {
      activate: () => {
        setTimeout(() => {
          deferredWasmStatus.textContent = 'Opening context';
        }, 20);
        return true;
      },
    },
  }, 'system:context');
  assert.strictEqual(activateDeferredWasm.ok, true);
  assert.strictEqual(activateDeferredWasm.verifiedVia, 'status-text');
  assert.ok(activateDeferredWasm.settleMs > 0);
  const activateRejected = await runWasmActivate({
    document: wasmActivateDocFor({ textContent: '' }),
    station: { activate: () => false },
  }, 'system:peers');
  assert.strictEqual(activateRejected.ok, false);
  assert.ok(/returned false/.test(activateRejected.reason));
  const activateUnsupported = await runWasmActivate({
    document: wasmActivateDocFor({ textContent: '' }),
    station: { debug_state: () => 'fps=58' },
  }, 'system:view');
  assert.strictEqual(activateUnsupported.ok, false);
  assert.ok(/station\.activate\(\) is not supported by this build/.test(activateUnsupported.reason));
  const activateUnverified = await runWasmActivate({
    document: wasmActivateDocFor({ textContent: 'station idle' }),
    station: { activate: () => true },
  }, 'system:view', 60);
  assert.strictEqual(activateUnverified.ok, false);
  assert.ok(/did not reflect activation/.test(activateUnverified.reason));
  assert.strictEqual(
    validationFailureKind('station activate probe system:view did not pass (last value: false)'),
    'interaction',
  );
  assert.strictEqual(isWaitFailureReason('station activate probe system:view did not pass'), true);

  // perf eval: pure report builder drives the verdict.
  const perfThresholds = stationPerfEvalThresholds(parseArgs([], {}));
  assert.deepStrictEqual(perfThresholds, {
    minFps: DEFAULT_STATION_MIN_FPS,
    maxFrameGapMs: DEFAULT_STATION_MAX_FRAME_GAP_MS,
    hardGapLimitMs: STATION_SMOOTH_HARD_GAP_LIMIT_MS,
  });
  const perfPass = buildStationPerfEvalReport({
    idle: { ok: true, fps: 60, p50: 16, p95: 17, worst: 30, frames: 120 },
    active: { ok: true, fps: 58, p50: 16.5, p95: 21, worst: 38, frames: 116 },
    activations: [
      { target: 'system:activity', ok: true, input: 'wasm-activate', latencyMs: 38 },
      { target: 'system:controls', ok: true, input: 'wasm-activate', latencyMs: 12 },
      { target: 'system:view', ok: true, input: 'wasm-activate', latencyMs: 11 },
    ],
    thresholds: perfThresholds,
    displays: 1,
  });
  assert.strictEqual(perfPass.verdict, 'pass');
  assert.strictEqual(perfPass.fpsIdle, 60);
  assert.strictEqual(perfPass.fpsAfterInteraction, 58);
  assert.strictEqual(perfPass.p95Idle, 17);
  assert.strictEqual(perfPass.p95Active, 21);
  assert.deepStrictEqual(perfPass.interactionTargets, ['system:activity', 'system:controls', 'system:view']);
  assert.deepStrictEqual(perfPass.interactionLatencies, [38, 12, 11]);
  assert.strictEqual(perfPass.interactionInput, 'wasm-activate');
  assert.strictEqual(perfPass.displays, 1);
  assert.deepStrictEqual(perfPass.failures, []);
  const perfFail = buildStationPerfEvalReport({
    idle: { ok: true, fps: 60, p50: 16, p95: 17, worst: 30, frames: 120 },
    active: { ok: false, fps: 19, p50: 30, p95: 87, worst: 260, frames: 40 },
    activations: [
      { target: 'system:activity', ok: true, input: 'wasm-activate', latencyMs: 41 },
      { target: 'system:controls', ok: false, input: 'wasm-activate', latencyMs: 1004, reason: 'Station did not reflect activation' },
    ],
    thresholds: perfThresholds,
  });
  assert.strictEqual(perfFail.verdict, 'fail');
  assert.ok(perfFail.failures.some((failure) => failure.includes('active fps 19 below minimum 24')));
  assert.ok(perfFail.failures.some((failure) => failure.includes('active p95 frame gap 87ms exceeds 40ms')));
  assert.ok(perfFail.failures.some((failure) => failure.includes('active worst frame gap 260ms exceeds 250ms')));
  assert.ok(perfFail.failures.some((failure) => failure.includes('activation system:controls failed: Station did not reflect activation')));
  assert.strictEqual('displays' in perfFail, false);
  const perfStalled = buildStationPerfEvalReport({
    idle: { ok: false, stalled: true, fps: 0, frames: 0, reason: 'requestAnimationFrame stalled: 0 frame(s) in 5000ms' },
    active: { ok: true, fps: 60, p50: 16, p95: 17, worst: 30, frames: 120 },
    activations: [],
    thresholds: perfThresholds,
  });
  assert.strictEqual(perfStalled.verdict, 'fail');
  assert.ok(perfStalled.failures.some((failure) => failure.includes('idle sample stalled')));
  assert.strictEqual(
    validationFailureKind('station perf eval did not pass (active fps 19 below minimum 24)'),
    'perf-eval',
  );
  assert.ok(validationFailureNextStep({ failureKind: 'perf-eval', reason: '' }).includes('perf-eval JSON report'));
  assert.strictEqual(isWaitFailureReason('station perf eval did not pass (idle sample stalled)'), true);

  // report formatting and output compaction for the new probes.
  const probeReportLines = formatStationProbeReportLines({
    fps: { fps: 58, minFps: 24 },
    smooth: { ok: true, fps: 60, p50: 16.7, p95: 18.2, worst: 33.4, frames: 119, sampleMs: 2001 },
    'debug-json': {
      ok: true,
      supported: true,
      data: {
        fps: 58,
        renderer: 'webgpu',
        gpu: true,
        hosts: 1,
        agents: 2,
        events: 5,
        displays: 1,
        selectedId: '',
        layout: 'orbital',
        hitZones: 12,
        systemTargets: ['system:activity', 'system:view'],
      },
    },
  });
  assert.strictEqual(probeReportLines.length, 3);
  assert.ok(probeReportLines.some((line) => line.includes('probe=fps fps=58 minFps=24')));
  assert.ok(probeReportLines.some((line) => line.includes('probe=smooth fps=60') && line.includes('p95=18.2') && line.includes('worst=33.4')));
  assert.ok(probeReportLines.some((line) => line.includes('probe=debug-json supported=true')
    && line.includes('renderer="webgpu"')
    && line.includes('systemTargets="system:activity,system:view"')));
  assert.ok(
    formatStationProbeReportLines({
      'debug-json': { ok: true, supported: false, reason: 'station.debug_json() is not supported by this build' },
    })[0].includes('supported=false'),
  );
  assert.deepStrictEqual(formatStationProbeReportLines(undefined), []);
  assert.ok(
    formatStationActivationPass({
      count: 2,
      warmupLatencyMs: 40,
      subsequentLatencyMs: 12,
      names: ['system:view', 'system:context'],
    }).includes('station activate count=2 warmupLatencyMs=40 subsequentLatencyMs=12'),
  );
  assert.ok(formatStationPerfEvalLine(perfPass).includes('"verdict":"pass"'));
  assert.ok(formatStationPerfEvalLine(perfFail).includes('"verdict":"fail"'));
  const compactedStationExtras = compactResultForOutput({}, {
    status: 'pass',
    stationProbeReports: {
      smooth: { ok: true, frames: 119, sampleMs: 2001, fps: 60, p50: 16.7, p95: 18.2, worst: 33.4, maxFrameGapMs: 40, hardGapLimitMs: 250 },
      'debug-json': {
        ok: true,
        supported: true,
        reason: 'passed',
        data: { fps: 58, renderer: 'webgpu', hosts: 1, systemTargets: ['system:view'], hitZoneNames: [] },
      },
    },
    stationActivation: {
      status: 'pass',
      count: 1,
      names: ['system:view'],
      warmupLatencyMs: 9,
      subsequentLatencyMs: 0,
      activations: [{
        name: 'system:view',
        input: 'wasm-activate',
        verifiedVia: 'selectedId',
        selectedId: 'system:view',
        latencyMs: 9,
        settleMs: 0,
      }],
    },
    stationPerfEval: perfPass,
  });
  assert.strictEqual(compactedStationExtras.stationProbeReports.smooth.p95, 18.2);
  assert.strictEqual(compactedStationExtras.stationProbeReports['debug-json'].data.renderer, 'webgpu');
  assert.strictEqual(compactedStationExtras.stationActivation.activations[0].verifiedVia, 'selectedId');
  assert.strictEqual(compactedStationExtras.stationPerfEval.verdict, 'pass');
  assert.deepStrictEqual(
    failureDiagnosticSelectors({ selectors: [], functions: [], stationProbes: [], stationPerfEval: true }),
    ['#station-status', '#station-hud-canvas', '[data-station-hotspot]'],
  );
  assert.strictEqual(
    isStationFocusedCheck({ selectors: [], functions: [], stationProbes: [], stationActivateTargets: ['system:view'] }),
    true,
  );
  assert.strictEqual(
    isStationFocusedCheck({ selectors: [], functions: [], stationProbes: [], stationPerfEval: true }),
    true,
  );
  const hiddenDockProbe = vm.runInNewContext(`(${stationProbeSource()})`, {
    document: stationProbeDocumentFor({ classList: { contains: (name) => name === 'hidden' } }),
  });
  assert.strictEqual(hiddenDockProbe('dock-hidden', stationProbeDiagnostics).ok, true);
  const visibleDockProbe = vm.runInNewContext(`(${stationProbeSource()})`, {
    document: stationProbeDocumentFor({ classList: { contains: () => false } }),
  });
  const visibleDockResult = visibleDockProbe('dock-hidden', stationProbeDiagnostics);
  assert.strictEqual(visibleDockResult.ok, false);
  assert.strictEqual(visibleDockResult.reason, 'Station dock is visible');
  const debugStateWebgpuProbe = vm.runInNewContext(`(${stationProbeSource()})`, {
    document: stationProbeDocumentFor(null),
    station: { debug_state: () => 'renderer=webgpu gpu=true' },
  });
  assert.strictEqual(
    debugStateWebgpuProbe('webgpu', { ...stationProbeDiagnostics, statusText: 'station ready' }).ok,
    true,
  );
  const canvasFallbackProbe = vm.runInNewContext(`(${stationProbeSource()})`, {
    document: stationProbeDocumentFor(null),
    station: { debug_state: () => 'renderer=canvas2d gpu=false' },
  });
  const canvasFallbackResult = canvasFallbackProbe('webgpu', { ...stationProbeDiagnostics, statusText: 'station ready' });
  assert.strictEqual(canvasFallbackResult.ok, false);
  assert.strictEqual(canvasFallbackResult.failureKind, 'renderer');
  assert.ok(canvasFallbackResult.reason.includes('unavailable or in fallback'));
  const unlitProbe = stationProbe('rendered', {
    ...stationProbeDiagnostics,
    pixels: { litCount: 0, total: 144 },
  });
  assert.strictEqual(unlitProbe.ok, false);
  assert.strictEqual(unlitProbe.failureKind, 'renderer');
  const hotspotPoint = vm.runInNewContext(`(${stationHotspotPointSource()})`, {
    window: { innerWidth: 1440, innerHeight: 1000 },
    document: {
      documentElement: { clientWidth: 1440, clientHeight: 1000 },
      getElementById: (id) => {
        if (id === 'tab-station') {
          return { classList: { contains: (name) => name === 'active' } };
        }
        if (id === 'station-hud-canvas') {
          return {
            getBoundingClientRect: () => ({ left: 10, top: 20, width: 640, height: 480 }),
            id,
          };
        }
        return null;
      },
      querySelector: () => null,
      elementFromPoint: () => ({ id: 'station-hud-canvas', closest: (selector) => (selector === '#station-hud-canvas' ? { id: 'station-hud-canvas' } : null) }),
    },
    stationHotspotBoxes: () => [{ name: 'system:view', x: 100, y: 120, w: 80, h: 40 }],
    Math,
  })('system:view');
  assert.strictEqual(hotspotPoint.ok, true);
  assert.strictEqual(Math.round(hotspotPoint.x), 150);
  class FakeMouseEvent {
    constructor(_type, init) {
      this.detail = init.detail;
      this.defaultPrevented = false;
    }
  }
  const activateDocumentFor = (statusElement, button, hotspot) => ({
    getElementById: (id) => {
      if (id === 'tab-station') return { classList: { contains: (name) => name === 'active' } };
      if (id === 'station-status') return statusElement;
      return null;
    },
    querySelector: (selector) => (selector === `[data-station-hotspot="${hotspot}"]` ? button : null),
  });
  let dispatchedActivationDetail = null;
  const syncStatus = { textContent: '' };
  const fakeHotspotButton = {
    disabled: false,
    focus() {},
    dispatchEvent(event) {
      dispatchedActivationDetail = event.detail;
      syncStatus.textContent = 'Opening view';
      return false;
    },
  };
  const hotspotActivation = await vm.runInNewContext(`(${stationHotspotActivateSource()})`, {
    document: activateDocumentFor(syncStatus, fakeHotspotButton, 'system:view'),
    window: {},
    MouseEvent: FakeMouseEvent,
    RegExp,
    String,
    Date,
    Number,
    setTimeout,
  })('system:view');
  assert.strictEqual(hotspotActivation.ok, true);
  assert.strictEqual(hotspotActivation.state.statusMatches, true);
  assert.strictEqual(hotspotActivation.state.settleMs, 0);
  assert.strictEqual(dispatchedActivationDetail, 0);
  // Verification without a dock: the status text lands a frame after the
  // click (requestAnimationFrame render-on-change), inside the settle window.
  const deferredStatus = { textContent: 'station idle' };
  const deferredHotspotButton = {
    disabled: false,
    focus() {},
    dispatchEvent() {
      setTimeout(() => {
        deferredStatus.textContent = 'Opening context';
      }, 20);
      return true;
    },
  };
  const deferredActivation = await vm.runInNewContext(`(${stationHotspotActivateSource()})`, {
    document: activateDocumentFor(deferredStatus, deferredHotspotButton, 'system:context'),
    window: {},
    MouseEvent: FakeMouseEvent,
    RegExp,
    String,
    Date,
    Number,
    setTimeout,
  })('system:context');
  assert.strictEqual(deferredActivation.ok, true);
  assert.strictEqual(deferredActivation.state.statusMatches, true);
  assert.ok(deferredActivation.state.settleMs > 0);
  const missingHotspotActivation = await vm.runInNewContext(`(${stationHotspotActivateSource()})`, {
    document: activateDocumentFor({ textContent: '' }, null, 'system:view'),
    window: {},
    MouseEvent: FakeMouseEvent,
    RegExp,
    String,
    Date,
    Number,
    setTimeout,
  })('system:peers');
  assert.strictEqual(missingHotspotActivation.ok, false);
  assert.ok(missingHotspotActivation.reason.includes('system:peers is missing'));
  const stationStateSummary = vm.runInNewContext(`(${stationStateSummarySource()})`, {
    buildStationSnapshot: () => ({
      hosts: [{ id: 'local' }],
      agents: [{ id: 'primary-agent' }],
      events: [],
      activity: { retainedCount: 0, shownCount: 0 },
      managed: { records: 0, anchors: 0 },
      sessions: { total: 0 },
    }),
    daemons: [],
    Map,
    Set,
    Array,
    Object,
    Number,
    String,
  })();
  assert.strictEqual(stationStateSummary.ok, true);
  assert.strictEqual(stationStateSummary.nonEmpty, false);
  assert.strictEqual(stationStateSummary.liveAgentSession.ok, false);
  assert.strictEqual(stationStateSummary.managedContextState.ok, false);
  assert.deepStrictEqual(JSON.parse(JSON.stringify(stationStateSummary.counts)), {
    sessions: 0,
    events: 0,
    managed: 0,
    peers: 0,
  });
  const stationProbeSnapshotSummary = vm.runInNewContext(`(${stationStateSummarySource()})`, {
    window: {
      stationProbe: {
        snapshot: () => ({
          hosts: [{ id: 'local' }],
          agents: [{ id: 'primary-agent' }],
          events: [{ id: 'probe-event' }],
          activity: { retainedCount: 1, shownCount: 1 },
          managed: { records: 0, anchors: 0 },
          sessions: { total: 0 },
        }),
      },
    },
    daemons: [],
    Map,
    Set,
    Array,
    Object,
    Number,
    String,
  })();
  assert.strictEqual(stationProbeSnapshotSummary.ok, true);
  assert.strictEqual(stationProbeSnapshotSummary.nonEmpty, true);
  assert.deepStrictEqual(JSON.parse(JSON.stringify(stationProbeSnapshotSummary.counts)), {
    sessions: 0,
    events: 1,
    managed: 0,
    peers: 0,
  });
  const populatedStationStateSummary = vm.runInNewContext(`(${stationStateSummarySource()})`, {
    buildStationSnapshot: () => ({
      hosts: [{ id: 'local' }, { id: 'peer-1' }],
      agents: [{ id: 'primary-agent' }, { id: 'peer-peer-1' }],
      events: [{ id: 'event-1' }],
      activity: { retainedCount: 1, shownCount: 1 },
      managed: { sessionId: 'abc', records: 2, anchors: 3 },
      sessions: { total: 4, latestTask: 'managed Station session' },
    }),
    daemons: [],
    Map,
    Set,
    Array,
    Object,
    Number,
    String,
  })();
  assert.strictEqual(populatedStationStateSummary.nonEmpty, true);
  assert.strictEqual(populatedStationStateSummary.liveAgentSession.ok, false);
  assert.strictEqual(populatedStationStateSummary.managedContextState.ok, false);
  assert.strictEqual(populatedStationStateSummary.managedContextState.sessionId, 'abc');
  assert.ok(populatedStationStateSummary.managedContextState.reason.includes('live=false'));
  assert.deepStrictEqual(JSON.parse(JSON.stringify(populatedStationStateSummary.counts)), {
    sessions: 4,
    events: 1,
    managed: 6,
    peers: 1,
  });
  const placeholderProviderStationStateSummary = vm.runInNewContext(`(${stationStateSummarySource()})`, {
    buildStationSnapshot: () => ({
      hosts: [{ id: 'local' }],
      agents: [{ id: 'primary-agent', provider: 'none', model: 'none', status: 'done' }],
      events: [{ id: 'event-1' }],
      activity: { retainedCount: 1, shownCount: 1 },
      managed: { sessionId: 'done-placeholder', records: 1, anchors: 0 },
      sessions: {
        total: 1,
        latestTask: 'done placeholder',
        recent: [{ id: 'done-placeholder', status: 'done', source: 'codex' }],
      },
    }),
    sessionMetadataById: new Map([
      ['done-placeholder', { provider: 'no-provider', model: 'none', status: 'done' }],
    ]),
    daemons: [],
    Map,
    Set,
    Array,
    Object,
    Number,
    String,
  })();
  assert.strictEqual(placeholderProviderStationStateSummary.nonEmpty, true);
  assert.strictEqual(placeholderProviderStationStateSummary.liveAgentSession.ok, false);
  assert.strictEqual(placeholderProviderStationStateSummary.managedContextState.ok, false);
  assert.ok(placeholderProviderStationStateSummary.liveAgentSession.reason.includes('no single non-placeholder'));
  const stoppedManagedStateSummary = vm.runInNewContext(`(${stationStateSummarySource()})`, {
    buildStationSnapshot: () => ({
      hosts: [{ id: 'local' }],
      agents: [{ id: 'primary-agent' }],
      events: [{ id: 'event-1' }],
      activity: { retainedCount: 1, shownCount: 1 },
      managed: { sessionId: 'stopped-managed', live: true, mode: 'managed', status: 'stopped', records: 2, anchors: 1 },
      controls: { sessionActive: true, sessionLiveId: 'stopped-managed', sessionManagedContext: 'managed' },
      sessions: { total: 1, recent: [{ id: 'stopped-managed', status: 'stopped', source: 'codex' }] },
    }),
    daemons: [],
    Map,
    Set,
    Array,
    Object,
    Number,
    String,
  })();
  assert.strictEqual(stoppedManagedStateSummary.nonEmpty, true);
  assert.strictEqual(stoppedManagedStateSummary.managedContextState.ok, false);
  assert.strictEqual(stoppedManagedStateSummary.managedContextState.live, true);
  assert.strictEqual(stoppedManagedStateSummary.managedContextState.status, 'stopped');
  const historicalManagedStateSummary = vm.runInNewContext(`(${stationStateSummarySource()})`, {
    buildStationSnapshot: () => ({
      hosts: [{ id: 'local' }],
      agents: [{ id: 'primary-agent' }],
      events: [{ id: 'event-1' }],
      activity: { retainedCount: 1, shownCount: 1 },
      managed: { sessionId: 'historical-managed', live: false, mode: 'managed', status: 'historical', records: 4, anchors: 2 },
      context: { replayMode: 'replay' },
      sessions: { total: 1, recent: [{ id: 'historical-managed', status: 'done', source: 'codex' }] },
    }),
    daemons: [],
    Map,
    Set,
    Array,
    Object,
    Number,
    String,
  })();
  assert.strictEqual(historicalManagedStateSummary.managedContextState.ok, false);
  assert.strictEqual(historicalManagedStateSummary.managedContextState.status, 'historical');
  assert.strictEqual(historicalManagedStateSummary.managedContextState.replayMode, 'replay');
  const unknownManagedStateSummary = vm.runInNewContext(`(${stationStateSummarySource()})`, {
    buildStationSnapshot: () => ({
      hosts: [{ id: 'local' }],
      agents: [{ id: 'primary-agent' }],
      events: [{ id: 'event-1' }],
      activity: { retainedCount: 1, shownCount: 1 },
      managed: { sessionId: 'unknown-managed', live: true, mode: 'managed', status: 'unknown', records: 1, anchors: 1 },
      controls: { sessionActive: true, sessionLiveId: 'unknown-managed', sessionManagedContext: 'managed' },
      sessions: { total: 1, recent: [{ id: 'unknown-managed', status: 'running', source: 'codex' }] },
    }),
    daemons: [],
    Map,
    Set,
    Array,
    Object,
    Number,
    String,
  })();
  assert.strictEqual(unknownManagedStateSummary.managedContextState.ok, false);
  assert.strictEqual(unknownManagedStateSummary.managedContextState.live, true);
  assert.strictEqual(unknownManagedStateSummary.managedContextState.status, 'unknown');
  const nativeProviderStationStateSummary = vm.runInNewContext(`(${stationStateSummarySource()})`, {
    buildStationSnapshot: () => ({
      hosts: [{ id: 'local' }],
      agents: [{ id: 'primary-agent', provider: 'google', model: 'gemini-2.5-pro', status: 'running' }],
      events: [{ id: 'event-1' }],
      activity: { retainedCount: 1, shownCount: 1 },
      managed: { sessionId: 'sess-native', records: 1, anchors: 0 },
      sessions: {
        total: 1,
        latestTask: 'native provider session',
        latestSource: 'intendant',
        recent: [{ id: 'sess-native', status: 'running', source: 'intendant' }],
      },
    }),
    sessionMetadataById: new Map([
      ['sess-native', { provider: 'google', model: 'gemini-2.5-pro', status: 'running', source: 'intendant' }],
    ]),
    daemons: [],
    Map,
    Set,
    Array,
    Object,
    Number,
    String,
  })('codex');
  assert.strictEqual(nativeProviderStationStateSummary.liveAgentSession.ok, true);
  assert.strictEqual(nativeProviderStationStateSummary.externalAgentSession.ok, false);
  assert.ok(nativeProviderStationStateSummary.externalAgentSession.reason.includes('required external agent codex'));
  const liveProviderStationStateSummary = vm.runInNewContext(`(${stationStateSummarySource()})`, {
    buildStationSnapshot: () => ({
      hosts: [{ id: 'local' }],
      agents: [{ id: 'primary-agent', provider: 'openai', model: 'gpt-5.2-codex', status: 'running' }],
      events: [{ id: 'event-1' }],
      activity: { retainedCount: 1, shownCount: 1 },
      managed: { sessionId: 'sess-live', live: true, mode: 'managed', status: 'watch', records: 1, anchors: 0 },
      context: { replayMode: 'live' },
      controls: { sessionActive: true, sessionLiveId: 'sess-live', sessionLivePhase: 'running', sessionManagedContext: 'managed' },
      sessions: {
        total: 1,
        latestTask: 'real managed Station session',
        latestSource: 'codex',
        recent: [{ id: 'sess-live', status: 'running', source: 'codex' }],
      },
    }),
    sessionMetadataById: new Map([
      ['sess-live', { provider: 'openai', model: 'gpt-5.2-codex', status: 'running', source: 'codex' }],
    ]),
    daemons: [],
    Map,
    Set,
    Array,
    Object,
    Number,
    String,
  })('codex');
  assert.strictEqual(liveProviderStationStateSummary.liveAgentSession.ok, true);
  assert.strictEqual(liveProviderStationStateSummary.liveAgentSession.sessionId, 'sess-live');
  assert.strictEqual(liveProviderStationStateSummary.liveAgentSession.provider, 'openai');
  assert.strictEqual(liveProviderStationStateSummary.liveAgentSession.model, 'gpt-5.2-codex');
  assert.strictEqual(liveProviderStationStateSummary.externalAgentSession.ok, true);
  assert.strictEqual(liveProviderStationStateSummary.externalAgentSession.sessionId, 'sess-live');
  assert.strictEqual(liveProviderStationStateSummary.externalAgentSession.agent, 'codex');
  assert.strictEqual(liveProviderStationStateSummary.managedContextState.ok, true);
  assert.strictEqual(liveProviderStationStateSummary.managedContextState.mode, 'managed');
  assert.strictEqual(liveProviderStationStateSummary.managedContextState.status, 'watch');
  const codexTargetStationStateSummary = vm.runInNewContext(`(${stationStateSummarySource()})`, {
    buildStationSnapshot: () => ({
      hosts: [{ id: 'local' }],
      agents: [{ id: 'primary-agent', provider: 'openai', model: 'gpt-5.2-codex', status: 'running' }],
      events: [{ id: 'event-1' }],
      activity: { retainedCount: 1, shownCount: 1 },
      managed: { sessionId: 'wrapper-live', records: 1, anchors: 0 },
      sessions: {
        total: 1,
        latestTask: 'codex target session',
        externalTargets: [{ id: 'codex-thread', sessionId: 'codex-thread', isCodex: true, status: 'running' }],
      },
    }),
    daemons: [],
    Map,
    Set,
    Array,
    Object,
    Number,
    String,
  })('codex');
  assert.strictEqual(codexTargetStationStateSummary.externalAgentSession.ok, true);
  assert.strictEqual(codexTargetStationStateSummary.externalAgentSession.sessionId, 'codex-thread');
  assert.strictEqual(codexTargetStationStateSummary.externalAgentSession.agent, 'codex');
  assert.strictEqual(
    validationFailureKind('station state check failed: Station sessions/events/managed/peers are all empty'),
    'station-state',
  );
  assert.strictEqual(
    validationFailureKind('ai provider session check failed: found 1 session id candidate(s), 0 provider candidate(s), 0 model candidate(s)'),
    'ai-provider-session',
  );
  assert.strictEqual(
    validationFailureKind('external agent session check failed: required external agent codex'),
    'external-agent-session',
  );
  assert.strictEqual(
    validationFailureKind('stale dashboard static asset /app; served hash abc does not match static/app.html hash def'),
    'stale-static',
  );
  assert.ok(validationFailureNextStep({ failureKind: 'station-state', reason: '' }).includes('--allow-empty-station-state'));
  assert.ok(validationFailureNextStep({ failureKind: 'ai-provider-session', reason: '' }).includes('provider/model'));
  assert.ok(validationFailureNextStep({ failureKind: 'external-agent-session', reason: '' }).includes('requested external agent'));
  assert.strictEqual(
    validationFailureKind('managed context state check failed: no active managed/context state'),
    'managed-context-state',
  );
  assert.ok(validationFailureNextStep({ failureKind: 'managed-context-state', reason: '' }).includes('active managed/context'));
  assert.ok(validationFailureNextStep({ failureKind: 'stale-static', reason: '' }).includes('--require-current-static'));
  assert.strictEqual(
    normalizeAppHtmlForIdentity('<script src="/wasm-station/station_web.js?v=abc123"></script>'),
    '<script src="/wasm-station/station_web.js"></script>',
  );
  assert.strictEqual(
    normalizeAppHtmlForIdentity('<script src="/wasm-station/station_web.js?cachebuster=abc%2F123&v=ignored"></script>'),
    '<script src="/wasm-station/station_web.js"></script>',
  );
  await withTempStaticIdentityTree(async ({ root, responses, localAppHtml, servedAppHtml }) => {
    assert.notStrictEqual(sha256Hex(Buffer.from(servedAppHtml)), sha256Hex(Buffer.from(localAppHtml)));
    assert.strictEqual(normalizeAppHtmlForIdentity(servedAppHtml), normalizeAppHtmlForIdentity(localAppHtml));
    await withStaticIdentityServer(responses, async (port) => {
      const identity = await validateCurrentStaticIdentity(`http://127.0.0.1:${port}/app?passive=1#station`, root);
      const appItem = identity.items.find((item) => item.path === '/app');
      assert.ok(appItem);
      assert.strictEqual(appItem.hash, sha256Hex(Buffer.from(normalizeAppHtmlForIdentity(localAppHtml))));
    });
  });
  assert.strictEqual(
    dashboardAssetUrl('http://127.0.0.1:8912/app?passive=1#station', '/wasm-station/station_web.js'),
    'http://127.0.0.1:8912/wasm-station/station_web.js',
  );
  assert.strictEqual(sha256Hex(Buffer.from('same')), sha256Hex(Buffer.from('same')));
  assert.deepStrictEqual(
    stationConsoleWarnings([
      '[console.log] Station ordinary log',
      '[console.warn] Station WebGPU unavailable; using canvas fallback',
      '[console.warning] Station canvas alpha sample failed',
      '[console.warn] unrelated warning',
      '[log.warning] canvas fallback path selected',
    ]),
    [
      '[console.warn] Station WebGPU unavailable; using canvas fallback',
      '[console.warning] Station canvas alpha sample failed',
      '[log.warning] canvas fallback path selected',
    ],
  );
  const fallbackDir = fs.mkdtempSync(path.join(os.tmpdir(), 'validate-dashboard-fallback-'));
  try {
    const sameBrowserShot = path.join(fallbackDir, 'same-browser.png');
    let sameBrowserTimeout = 0;
    const sameBrowserHarness = {
      pageLogs: new BoundedLog(LOG_BUFFER_LIMIT),
      captureScreenshot: async (filePath, timeoutMs) => {
        sameBrowserTimeout = timeoutMs;
        fs.writeFileSync(filePath, 'png');
        return filePath;
      },
    };
    const sameBrowserFallback = await captureStationRuntimeTimeoutEvidence(
      {
        url: 'http://127.0.0.1:8956/#station',
        screenshotPath: sameBrowserShot,
        stationProbes: ['rendered'],
        selectors: [],
        functions: [],
      },
      { env: {} },
      sameBrowserHarness,
      new Error('CDP Runtime.evaluate timed out'),
      () => {},
    );
    assert.strictEqual(sameBrowserFallback.screenshot.status, 'captured');
    assert.strictEqual(sameBrowserFallback.screenshot.mode, 'same-browser');
    assert.strictEqual(sameBrowserTimeout, FAILURE_SCREENSHOT_TIMEOUT_MS);
    assert.strictEqual(fs.existsSync(sameBrowserShot), true);

    const freshBrowserShot = path.join(fallbackDir, 'fresh-browser.png');
    let firstClosed = false;
    let replacedHarness;
    let navigationTimeout = 0;
    const firstHarness = {
      pageLogs: new BoundedLog(LOG_BUFFER_LIMIT),
      captureScreenshot: async () => {
        throw new Error('CDP Page.captureScreenshot timed out');
      },
      close: async () => {
        firstClosed = true;
      },
    };
    const retryHarness = {
      pageLogs: new BoundedLog(LOG_BUFFER_LIMIT),
      navigateForScreenshot: async (_opts, timeoutMs) => {
        navigationTimeout = timeoutMs;
      },
      captureScreenshot: async (filePath) => {
        fs.writeFileSync(filePath, 'png');
        return filePath;
      },
    };
    const freshBrowserFallback = await captureStationRuntimeTimeoutEvidence(
      {
        url: 'http://127.0.0.1:8956/#station',
        screenshotPath: freshBrowserShot,
        stationProbes: ['rendered'],
        selectors: [],
        functions: [],
      },
      { env: {} },
      firstHarness,
      new Error('CDP Runtime.evaluate timed out'),
      (nextHarness) => {
        replacedHarness = nextHarness;
      },
      async () => retryHarness,
    );
    assert.strictEqual(firstClosed, true);
    assert.strictEqual(replacedHarness, retryHarness);
    assert.strictEqual(navigationTimeout, FAILURE_SCREENSHOT_NAVIGATION_TIMEOUT_MS);
    assert.strictEqual(freshBrowserFallback.screenshot.status, 'captured');
    assert.strictEqual(freshBrowserFallback.screenshot.mode, 'fresh-browser');
    assert.strictEqual(fs.existsSync(freshBrowserShot), true);
    const staleShot = path.join(fallbackDir, 'stale.png');
    fs.writeFileSync(staleShot, 'old');
    const staleFallback = await captureStationRuntimeTimeoutEvidence(
      {
        url: 'http://127.0.0.1:8956/#station',
        screenshotPath: staleShot,
        stationProbes: ['rendered'],
        selectors: [],
        functions: [],
      },
      undefined,
      {
        pageLogs: new BoundedLog(LOG_BUFFER_LIMIT),
        captureScreenshot: async () => {
          throw new Error('CDP Page.captureScreenshot timed out');
        },
      },
      new Error('CDP Runtime.evaluate timed out'),
      () => {},
    );
    assert.strictEqual(staleFallback.screenshot.status, 'failed');
    assert.strictEqual(fs.existsSync(staleShot), false);
    assert.strictEqual(
      compactHarnessFallbacks({
        stationRuntimeEvaluateTimeout: true,
        screenshot: {
          status: 'failed',
          mode: 'fresh-browser',
          reason: 'x'.repeat(RESULT_REASON_LIMIT + 20),
        },
      }).screenshot.reason.length,
      RESULT_REASON_LIMIT,
    );
  } finally {
    fs.rmSync(fallbackDir, { recursive: true, force: true });
  }
  assert.strictEqual(shouldCollectFailureDiagnostics({ diagnostics: true }, new Error('boom')), true);
  assert.strictEqual(
    shouldCollectFailureDiagnostics({ diagnostics: false }, new Error('wait-for-function did not become truthy')),
    true,
  );
  assert.strictEqual(
    shouldCollectFailureDiagnostics({ diagnostics: false }, new Error('CDP Runtime.evaluate timed out')),
    false,
  );
  assert.strictEqual(
    shouldCollectFailureDiagnostics({ diagnostics: true }, new Error('CDP Runtime.evaluate timed out')),
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
        '() => document.getElementById("station-hud-canvas") && document.querySelector("[data-station-hotspot=\'system:controls\']")',
      ],
    }),
    ['#station-status', '#station-hud-canvas', '[data-station-hotspot=\'system:controls\']'],
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
    ['#station-status', '#station-hud-canvas', '[data-station-hotspot]'],
  );
  assert.deepStrictEqual(
    failureDiagnosticSelectors({
      selectors: [],
      functions: [],
      stationProbes: [],
      stationInteractionProbe: true,
    }),
    ['#station-status', '#station-hud-canvas', '[data-station-hotspot]'],
  );
  assert.strictEqual(isStationFocusedCheck({ selectors: [], functions: [], stationProbes: ['webgpu'] }), true);
  assert.strictEqual(isStationFocusedCheck({ selectors: [], functions: [], stationProbes: [], stationInteractionProbe: true }), true);
  assert.strictEqual(isStationFocusedCheck({ selectors: ['#station-status'], functions: [], stationProbes: [] }), true);
  assert.strictEqual(isStationFocusedCheck({ selectors: ['#root'], functions: ['() => document.title'], stationProbes: [] }), false);
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
        selectorMatches: [{ selector: '[data-station-hotspot]', count: 11, first: 'button[data-station-hotspot="layout:orbital"]' }],
      },
    },
  );
  assert.strictEqual(compactedAutoDiagnosticFailure.diagnosticsAuto, true);
  assert.strictEqual(compactedAutoDiagnosticFailure.failureKind, 'assertion');
  assert.ok(compactedAutoDiagnosticFailure.next.includes('avoid further broad selector/source dumps'));
  const compactedInteraction = compactResultForOutput(
    {},
    {
      status: 'pass',
      stationInteraction: {
        status: 'pass',
        count: 3,
        names: ['system:activity', 'system:controls', 'system:view'],
        warmupLatencyMs: 42,
        subsequentLatencyMs: 17,
        interactions: [{
          name: 'system:view',
          control: 'station-hud-canvas',
          input: 'mouse',
          x: 150,
          y: 160,
          latencyMs: 12,
          state: {
            ok: true,
            target: 'system:view',
            activeTab: 'station',
            statusText: `Opening view ${'detail '.repeat(200)}`,
            statusMatches: true,
            settleMs: 5,
            reason: 'passed',
          },
        }],
      },
    },
  );
  assert.strictEqual(compactedInteraction.stationInteraction.count, 3);
  assert.ok(compactedInteraction.stationInteraction.interactions[0].state.statusText.includes('chars omitted'));
  assert.strictEqual(compactedInteraction.stationInteraction.interactions[0].state.statusMatches, true);
  assert.strictEqual(compactedInteraction.stationInteraction.interactions[0].state.settleMs, 5);
  assert.strictEqual('dockMatches' in compactedInteraction.stationInteraction.interactions[0].state, false);
  assert.deepStrictEqual(
    formatArtifactLines({ screenshot: '/tmp/a.png', browserUserDataDir: '/tmp/profile' }),
    ['artifact screenshot="/tmp/a.png"', 'artifact browserUserDataDir="/tmp/profile"'],
  );
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
          warnings: ['[console.warn] Station WebGPU unavailable; using canvas fallback'],
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
  class FakeCdpSocket extends EventEmitter {
    constructor() {
      super();
      this.sent = [];
    }

    send(text) {
      this.sent.push(JSON.parse(text));
    }

    close() {
      this.emit('close');
    }
  }
  const timeoutSocket = new FakeCdpSocket();
  const timeoutCdp = new CdpConnection(timeoutSocket);
  const timeoutKeepalive = delay(20);
  await assert.rejects(
    () => timeoutCdp.send('Page.captureScreenshot', {}, undefined, 5),
    /CDP Page\.captureScreenshot timed out/,
  );
  await timeoutKeepalive;
  assert.strictEqual(timeoutCdp.pending.size, 0);
  const responseSocket = new FakeCdpSocket();
  const responseCdp = new CdpConnection(responseSocket);
  const responsePromise = responseCdp.send('Runtime.evaluate', {}, undefined, 100);
  responseCdp.handleMessage(JSON.stringify({
    id: responseSocket.sent[0].id,
    result: { ok: true },
  }));
  assert.deepStrictEqual(await responsePromise, { ok: true });
  assert.strictEqual(responseCdp.pending.size, 0);
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

async function withStaticIdentityServer(responses, callback) {
  const server = http.createServer((req, res) => {
    const pathname = new URL(req.url || '/', 'http://127.0.0.1').pathname;
    if (!responses.has(pathname)) {
      res.writeHead(404);
      res.end('not found');
      return;
    }
    res.writeHead(200);
    res.end(responses.get(pathname));
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

async function withTempStaticIdentityTree(callback) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'validate-dashboard-static-'));
  const localAppHtml = [
    '<link rel="icon" href="/icon-128.png">',
    '<script type="module" src="/wasm-web/presence_web.js"></script>',
    '<script type="module" src="/wasm-station/station_web.js"></script>',
    '<script>const presence = "/wasm-web/presence_web_bg.wasm";</script>',
    '<script>const station = "/wasm-station/station_web_bg.wasm";</script>',
    '<script type="module" src="/three.module.min.js"></script>',
  ].join('\n');
  const servedAppHtml = cachebustedAppHtml(localAppHtml);
  const responses = new Map();
  try {
    for (const asset of STATIC_IDENTITY_ASSETS) {
      const localPath = path.join(root, asset.file);
      fs.mkdirSync(path.dirname(localPath), { recursive: true });
      if (asset.urlPath === '/app') {
        fs.writeFileSync(localPath, localAppHtml);
        responses.set(asset.urlPath, Buffer.from(servedAppHtml));
      } else {
        const payload = Buffer.from(`fixture:${asset.urlPath}\n`);
        fs.writeFileSync(localPath, payload);
        responses.set(asset.urlPath, payload);
      }
    }
    await callback({ root, responses, localAppHtml, servedAppHtml });
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

function cachebustedAppHtml(html) {
  let output = String(html || '');
  for (const assetPath of APP_HTML_CACHEBUSTED_ASSET_PATHS) {
    output = output.replaceAll(assetPath, `${assetPath}?cachebuster=served%2Fidentity&v=ignored`);
  }
  return output;
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
