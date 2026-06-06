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
const { spawn } = require('child_process');

const DEFAULT_TIMEOUT_MS = 15000;
const DEFAULT_CDP_TIMEOUT_MS = 10000;
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
const FORMATTED_DIAGNOSTIC_LINE_LIMIT = 520;

const BROWSER_EXECUTABLE_ENVS = [
  'INTENDANT_BROWSER_WORKSPACE_EXECUTABLE',
  'INTENDANT_BROWSER_EXECUTABLE',
  'CHROME_PATH',
  'CHROME_BIN',
];

function printUsage() {
  console.log(`Usage:
  scripts/validate-dashboard.cjs --port <port> [checks]
  scripts/validate-dashboard.cjs --url http://127.0.0.1:<port>/app [checks]

Checks:
  --selector CSS              Wait until document.querySelector(CSS) exists
  --wait-for-selector CSS     Alias for --selector
  --wait-for-function JS      Wait until a JS expression/function returns truthy

Options:
  --host HOST                 Host used with --port (default: 127.0.0.1)
  --path PATH                 Path used with --port (default: /)
  --timeout MS               Page/check timeout (default: ${DEFAULT_TIMEOUT_MS})
  --cdp-timeout MS           Chromium CDP readiness timeout (default: ${DEFAULT_CDP_TIMEOUT_MS})
  --browser PATH             Chromium/Chrome executable
  --headed                   Run without --headless=new
  --sandbox                  Omit default --no-sandbox
  --log-lines N              Bounded browser/page log lines on failure (default: ${DEFAULT_LOG_LINES})
  --diagnostics              On failure, include compact generic DOM/page state
  --json                     Print one compact JSON result
  --self-test                Run parser/formatter self-tests; does not launch a browser

If --url/--port are omitted, the script derives the dashboard port from
INTENDANT_MCP_URL when available. It never defaults to port 8765.`);
}

function parseArgs(argv, env = process.env) {
  const opts = {
    host: '127.0.0.1',
    path: '/',
    selectors: [],
    functions: [],
    timeoutMs: DEFAULT_TIMEOUT_MS,
    cdpTimeoutMs: DEFAULT_CDP_TIMEOUT_MS,
    logLines: DEFAULT_LOG_LINES,
    diagnostics: false,
    headless: true,
    noSandbox: true,
    json: false,
    selfTest: false,
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
    } else if (arg.startsWith('--url=')) {
      opts.url = arg.slice('--url='.length);
    } else if (arg === '--port') {
      opts.port = readNumber('--port');
    } else if (arg.startsWith('--port=')) {
      opts.port = parsePositiveInt(arg.slice('--port='.length), '--port');
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
    } else if (arg === '--sandbox') {
      opts.noSandbox = false;
    } else if (arg === '--log-lines') {
      opts.logLines = readNumber('--log-lines');
    } else if (arg.startsWith('--log-lines=')) {
      opts.logLines = parsePositiveInt(arg.slice('--log-lines='.length), '--log-lines');
    } else if (arg === '--diagnostics') {
      opts.diagnostics = true;
    } else if (arg === '--json') {
      opts.json = true;
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }

  opts.url = resolveDashboardUrl(opts, env);
  return opts;
}

function parsePositiveInt(raw, name) {
  const value = Number(raw);
  if (!Number.isFinite(value) || value <= 0) {
    throw new Error(`${name} must be a positive number`);
  }
  return Math.floor(value);
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
    runSelfTest();
    return;
  }

  if (!opts.url) {
    console.error('FAIL dashboard-validation reason="missing --url/--port and INTENDANT_MCP_URL"');
    console.error('Run scripts/validate-dashboard.cjs --help for usage.');
    process.exitCode = 2;
    return;
  }

  const started = Date.now();
  let harness;
  try {
    harness = await BrowserHarness.launch(opts);
    await harness.validate(opts);
    const result = {
      status: 'pass',
      url: opts.url,
      ms: Date.now() - started,
      browser: harness.browserExecutable,
      websocket: harness.websocketKind,
      selectors: opts.selectors.length,
      functions: opts.functions.length,
    };
    printResult(opts, result);
  } catch (error) {
    const diagnostics = opts.diagnostics && harness
      ? await harness.failureDiagnostics(opts).catch((diagError) => ({
          error: diagError.message || String(diagError),
        }))
      : undefined;
    const result = {
      status: 'fail',
      url: opts.url,
      ms: Date.now() - started,
      reason: error.message || String(error),
      browser: harness && harness.browserExecutable,
      websocket: harness && harness.websocketKind,
      logs: harness ? harness.failureExcerpt(opts.logLines) : [],
      diagnostics,
    };
    printResult(opts, result);
    process.exitCode = 1;
  } finally {
    if (harness) {
      await harness.close();
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
      `PASS dashboard-validation url=${quote(displayResult.url)} selectors=${displayResult.selectors} functions=${displayResult.functions} ms=${displayResult.ms} websocket=${displayResult.websocket || 'unknown'}`,
    );
    return;
  }
  console.error(
    `FAIL dashboard-validation url=${quote(displayResult.url)} reason=${quote(displayResult.reason)} ms=${displayResult.ms}`,
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

class BrowserHarness {
  static async launch(opts) {
    const executable = resolveBrowserExecutable(opts.browser);
    const userDataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'intendant-dashboard-validate-'));
    const stderr = new BoundedLog(LOG_BUFFER_LIMIT);
    const args = browserArgs(userDataDir, opts);
    const child = spawn(executable, args, {
      stdio: ['ignore', 'ignore', 'pipe'],
      env: process.env,
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
    const expression = waitFunctionExpression(source);
    await waitUntil(
      async () => {
        try {
          return Boolean(await this.evaluate(expression));
        } catch (error) {
          lastError = error.message || String(error);
          return false;
        }
      },
      timeoutMs,
      () => {
        const suffix = lastError ? ` (last error: ${lastError})` : '';
        return `wait-for-function did not become truthy${suffix}`;
      },
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
    return this.evaluate(`(${pageDiagnosticsSource()})(${JSON.stringify(opts.selectors || [])})`);
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

function browserArgs(userDataDir, opts) {
  const args = [
    '--remote-debugging-port=0',
    `--user-data-dir=${userDataDir}`,
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-background-networking',
    '--disable-dev-shm-usage',
    '--disable-extensions',
    '--disable-gpu',
    '--disable-popup-blocking',
    '--window-size=1440,1000',
  ];
  if (opts.headless) {
    args.push('--headless=new');
  }
  if (opts.noSandbox) {
    args.push('--no-sandbox');
  }
  return args;
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
      throw new Error(`Chromium exited before CDP was ready${formatStderrSuffix(stderr)}`);
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
      return compact(`${tag}${id}${classes}${text ? ` "${text}"` : ''}`, 120);
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

function compactResultForOutput(opts, result) {
  const compact = { ...result };
  if (compact.reason) {
    compact.reason = truncateMiddle(compact.reason, RESULT_REASON_LIMIT);
  }
  if (Array.isArray(compact.logs)) {
    compact.logs = compact.logs.map((line) => truncateMiddle(line, RESULT_LOG_LIMIT));
  }
  if (compact.diagnostics) {
    compact.diagnostics = compactDiagnostics(compact.diagnostics);
  }
  if (compact.status === 'fail') {
    compact.next = validationFailureNextStep(opts);
  }
  return compact;
}

function validationFailureNextStep(opts) {
  if (opts.diagnostics) {
    return 'fix from these targeted facts or report partial validation; avoid further broad selector/source dumps';
  }
  return 'retry at most once with --diagnostics --json and a targeted selector/function';
}

function compactDiagnostics(diagnostics) {
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

  const headings = compactStringArray(diagnostics.headings, DIAGNOSTIC_LIST_LIMIT, DIAGNOSTIC_TEXT_LIMIT);
  if (headings.values.length) {
    compact.headings = headings.values;
  }
  if (headings.omitted) {
    compact.headingsOmitted = headings.omitted;
  }

  const controls = compactStringArray(diagnostics.controls, DIAGNOSTIC_LIST_LIMIT, DIAGNOSTIC_TEXT_LIMIT);
  if (controls.values.length) {
    compact.controls = controls.values;
  }
  if (controls.omitted) {
    compact.controlsOmitted = controls.omitted;
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
  return lines.map((line) => truncateMiddle(line, FORMATTED_DIAGNOSTIC_LINE_LIMIT));
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

function runSelfTest() {
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
      '--timeout=2500',
      '--log-lines',
      '3',
      '--diagnostics',
    ],
    {},
  );
  assert.strictEqual(parsed.url, 'http://127.0.0.1:1234/app');
  assert.deepStrictEqual(parsed.selectors, ['#root']);
  assert.deepStrictEqual(parsed.functions, ['() => true']);
  assert.strictEqual(parsed.timeoutMs, 2500);
  assert.strictEqual(parsed.logLines, 3);
  assert.strictEqual(parsed.diagnostics, true);
  assert.strictEqual(
    dashboardUrlFromMcpUrl('http://localhost:7777/mcp?managed_context=managed'),
    'http://localhost:7777/',
  );
  assert.ok(waitFunctionExpression('document.body').includes('typeof candidate'));
  assert.ok(pageDiagnosticsSource().includes('selectorMatches'));
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
  const log = new BoundedLog(2);
  log.push('a', 'first');
  log.push('b', 'second');
  log.push('c', 'third');
  assert.deepStrictEqual(log.excerpt(3), ['[b] second', '[c] third']);
  console.log('PASS dashboard-validation-self-test');
}

if (require.main === module) {
  main().catch((error) => {
    console.error(`FAIL dashboard-validation reason=${quote(error.message || String(error))}`);
    process.exitCode = 1;
  });
}
