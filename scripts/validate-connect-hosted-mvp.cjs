#!/usr/bin/env node
'use strict';

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn } = require('child_process');
const { launchBrowser } = require('./lib/browser-automation.cjs');

const DEFAULT_CONNECT_PORT = 9886;
const DEFAULT_DAEMON_PORT = 8886;
const DEFAULT_DAEMON_ID = 'connect-hosted-mvp-daemon';
const DEFAULT_CONNECT_TOKEN = 'connect-hosted-mvp-token';
const START_TIMEOUT_MS = 45000;
const CONNECT_TIMEOUT_MS = 45000;

function parseArgs(argv) {
  const repoRoot = path.resolve(__dirname, '..');
  const out = {
    repoRoot,
    connectBinary: path.join(repoRoot, 'target', 'debug', 'intendant-connect'),
    daemonBinary: path.join(repoRoot, 'target', 'debug', 'intendant'),
    connectPort: DEFAULT_CONNECT_PORT,
    daemonPort: DEFAULT_DAEMON_PORT,
    daemonId: DEFAULT_DAEMON_ID,
    connectToken: DEFAULT_CONNECT_TOKEN,
  };
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--connect-binary') out.connectBinary = path.resolve(argv[++i]);
    else if (arg === '--daemon-binary') out.daemonBinary = path.resolve(argv[++i]);
    else if (arg === '--connect-port') out.connectPort = Number(argv[++i]);
    else if (arg === '--daemon-port') out.daemonPort = Number(argv[++i]);
    else if (arg === '--daemon-id') out.daemonId = String(argv[++i] || '').trim();
    else if (arg === '--connect-token') out.connectToken = String(argv[++i] || '').trim();
    else if (arg === '--help' || arg === '-h') {
      console.log(`Usage:
  node scripts/validate-connect-hosted-mvp.cjs [options]

Options:
  --connect-binary <path>    intendant-connect binary. Default target/debug/intendant-connect.
  --daemon-binary <path>     intendant daemon binary. Default target/debug/intendant.
  --connect-port <port>      Local hosted Connect port. Default ${DEFAULT_CONNECT_PORT}.
  --daemon-port <port>       Fresh daemon web port. Default ${DEFAULT_DAEMON_PORT}.
  --daemon-id <id>           Connect daemon id. Default ${DEFAULT_DAEMON_ID}.
  --connect-token <token>    Bearer token for daemon endpoints. Default ${DEFAULT_CONNECT_TOKEN}.
`);
      process.exit(0);
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  assert(Number.isInteger(out.connectPort) && out.connectPort > 0, 'invalid connect port');
  assert(Number.isInteger(out.daemonPort) && out.daemonPort > 0, 'invalid daemon port');
  assert(out.daemonId, 'daemon id is required');
  assert(out.connectToken, 'connect token is required');
  return out;
}

async function fetchJson(url, options = {}) {
  const resp = await fetch(url, options);
  const body = await resp.json().catch(() => ({}));
  if (!resp.ok || body.ok === false) {
    throw new Error(`${url} returned ${resp.status}: ${body.error || JSON.stringify(body)}`);
  }
  return body;
}

async function httpStatus(url, options = {}) {
  const resp = await fetch(url, options).catch(err => ({ status: 0, error: err }));
  return resp.status || 0;
}

async function waitFor(fn, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;
  while (Date.now() < deadline) {
    try {
      const value = await fn();
      if (value) return value;
    } catch (err) {
      lastError = err;
    }
    await new Promise(resolve => setTimeout(resolve, 150));
  }
  throw new Error(`timed out waiting for ${label}${lastError ? `: ${lastError.message}` : ''}`);
}

async function addVirtualAuthenticator(browser, page) {
  if (browser.kind === 'playwright' && page.context) {
    const client = await page.context().newCDPSession(page);
    await client.send('WebAuthn.enable');
    await client.send('WebAuthn.addVirtualAuthenticator', {
      options: {
        protocol: 'ctap2',
        transport: 'internal',
        hasResidentKey: true,
        hasUserVerification: true,
        isUserVerified: true,
        automaticPresenceSimulation: true,
      },
    });
    return;
  }
  if (page.connection && page.sessionId) {
    await page.connection.send('WebAuthn.enable', {}, page.sessionId);
    await page.connection.send('WebAuthn.addVirtualAuthenticator', {
      options: {
        protocol: 'ctap2',
        transport: 'internal',
        hasResidentKey: true,
        hasUserVerification: true,
        isUserVerified: true,
        automaticPresenceSimulation: true,
      },
    }, page.sessionId);
    return;
  }
  throw new Error('browser driver does not expose CDP WebAuthn controls');
}

async function click(page, selector) {
  if (typeof page.locator === 'function') {
    await page.locator(selector).click();
    return;
  }
  const point = await page.evaluate(`(() => {
    const sel = ${JSON.stringify(selector)};
    const el = document.querySelector(sel);
    if (!el) throw new Error('missing selector ' + sel);
    const r = el.getBoundingClientRect();
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  })()`);
  await page.connection.send('Input.dispatchMouseEvent', {
    type: 'mousePressed',
    x: point.x,
    y: point.y,
    button: 'left',
    clickCount: 1,
  }, page.sessionId);
  await page.connection.send('Input.dispatchMouseEvent', {
    type: 'mouseReleased',
    x: point.x,
    y: point.y,
    button: 'left',
    clickCount: 1,
  }, page.sessionId);
}

async function goto(page, url, opts = {}) {
  const response = await page.goto(url, opts);
  if (response && response.status && response.status() >= 400) {
    throw new Error(`${url} returned ${response.status()}`);
  }
  return response;
}

async function main() {
  const options = parseArgs(process.argv);
  for (const binary of [options.connectBinary, options.daemonBinary]) {
    if (!fs.existsSync(binary)) {
      throw new Error(`missing binary ${binary}; run cargo build --bin intendant-connect --bin intendant`);
    }
  }

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'intendant-connect-hosted-mvp-'));
  const connectOrigin = `http://localhost:${options.connectPort}`;
  const connectApi = `http://127.0.0.1:${options.connectPort}`;
  const connectLogs = [];
  const daemonLogs = [];
  const children = [];
  let browser = null;

  function spawnLogged(command, args, spawnOptions, logs) {
    const child = spawn(command, args, spawnOptions);
    children.push(child);
    child.stdout?.on('data', chunk => logs.push(String(chunk)));
    child.stderr?.on('data', chunk => logs.push(String(chunk)));
    child.once('error', err => logs.push(String(err && err.message || err)));
    return child;
  }

  try {
    spawnLogged(options.connectBinary, [
      '--listen', `127.0.0.1:${options.connectPort}`,
      '--origin', connectOrigin,
      '--rp-id', 'localhost',
      '--static-root', path.join(options.repoRoot, 'static'),
      '--data-file', path.join(tmp, 'connect-state.json'),
      '--daemon-token', options.connectToken,
    ], {
      cwd: options.repoRoot,
      stdio: ['ignore', 'pipe', 'pipe'],
    }, connectLogs);

    await waitFor(async () => {
      const status = await httpStatus(`${connectApi}/healthz`);
      return status === 200;
    }, START_TIMEOUT_MS, 'intendant-connect health');

    spawnLogged(options.daemonBinary, ['--no-tui', '--web', String(options.daemonPort)], {
      cwd: options.repoRoot,
      env: {
        ...process.env,
        INTENDANT_CONNECT_RENDEZVOUS_URL: connectApi,
        INTENDANT_CONNECT_DAEMON_ID: options.daemonId,
        INTENDANT_CONNECT_TOKEN: options.connectToken,
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    }, daemonLogs);

    await waitFor(
      () => daemonLogs.join('').includes(`Web TUI: https://0.0.0.0:${options.daemonPort}`),
      START_TIMEOUT_MS,
      'daemon web startup'
    );
    const unauthorized = await httpStatus(`${connectApi}/api/daemon/next?daemon_id=${encodeURIComponent(options.daemonId)}&timeout_ms=1`);
    assert.strictEqual(unauthorized, 401, 'daemon endpoint did not require bearer auth');

    const registered = await waitFor(async () => {
      const status = await fetchJson(`${connectApi}/api/status?daemon_id=${encodeURIComponent(options.daemonId)}`);
      return status.registered && status.daemon_public_key ? status : null;
    }, START_TIMEOUT_MS, 'daemon registration');
    assert.strictEqual(registered.claimed, false, 'daemon should start unclaimed');

    const claimCode = await waitFor(() => {
      const logs = `${connectLogs.join('')}\n${daemonLogs.join('')}`;
      const urlMatch = logs.match(/claim_code=([^\s"'<>]+)/);
      if (urlMatch) return decodeURIComponent(urlMatch[1]);
      const codeMatch = logs.match(/claim this daemon with code ([^\s"'<>]+)/);
      return codeMatch && codeMatch[1];
    }, START_TIMEOUT_MS, 'claim phrase log');

    browser = await launchBrowser({ ignoreHTTPSErrors: true });
    const page = await browser.newPage();
    await addVirtualAuthenticator(browser, page);
    await goto(page, `${connectOrigin}/connect?claim_code=${encodeURIComponent(claimCode)}`, { timeout: START_TIMEOUT_MS });

    await page.evaluate(() => {
      document.getElementById('account').value = `hosted-e2e-${Date.now()}`;
      document.getElementById('display').value = 'Hosted E2E';
    });
    await click(page, '#register');
    await page.waitForFunction(() => !document.getElementById('manage').classList.contains('hidden'), {
      timeout: START_TIMEOUT_MS,
    });

    await click(page, '#claim');
    await page.waitForFunction(() => document.getElementById('claim-status').textContent.includes('Claimed'), {
      timeout: START_TIMEOUT_MS,
    });

    const daemons = await page.evaluate(async () => fetch('/api/daemons').then(r => r.json()));
    assert.strictEqual(daemons.daemons.length, 1, `expected one claimed daemon: ${JSON.stringify(daemons)}`);
    assert.strictEqual(daemons.daemons[0].daemon_id, options.daemonId);
    const labelResult = await page.evaluate(`(async () => {
      const daemonId = ${JSON.stringify(options.daemonId)};
      const me = await fetch('/api/me').then(r => r.json());
      const resp = await fetch('/api/daemons/' + encodeURIComponent(daemonId) + '/label', {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          'x-intendant-csrf': me.csrf_token || '',
        },
        body: JSON.stringify({ label: 'Hosted E2E Daemon' }),
      });
      return resp.json();
    })()`);
    assert.strictEqual(labelResult.ok, true, `label update failed: ${JSON.stringify(labelResult)}`);
    assert.strictEqual(labelResult.daemon.label, 'Hosted E2E Daemon');

    await goto(page, `${connectOrigin}/app?connect=1&daemon_id=${encodeURIComponent(options.daemonId)}`, {
      timeout: START_TIMEOUT_MS,
    });
    let connected;
    try {
      connected = await waitFor(async () => {
        const status = await page.evaluate(() => window.intendantDashboardControl?.status?.() || null);
        if (status?.connected && status?.verifiedBinding?.ok && status?.signalingMode === 'connect-rendezvous') {
          return status;
        }
        return null;
      }, CONNECT_TIMEOUT_MS, 'hosted dashboard Connect tunnel');
    } catch (err) {
      const status = await page.evaluate(() => window.intendantDashboardControl?.status?.() || null).catch(e => ({ error: e.message }));
      throw new Error(`${err.message}; last dashboard status: ${JSON.stringify(status)}`);
    }
    assert.strictEqual(connected.claimedDaemonPublicKey, registered.daemon_public_key);
    assert.strictEqual(connected.verifiedBinding.daemonPublicKey, registered.daemon_public_key);
    assert(connected.sessionGrantSha256, 'Connect dashboard did not bind a session grant');

    const revoked = await page.evaluate(`(async () => {
      const daemonId = ${JSON.stringify(options.daemonId)};
      const me = await fetch('/api/me').then(r => r.json());
      const resp = await fetch('/api/daemons/' + encodeURIComponent(daemonId) + '/revoke', {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          'x-intendant-csrf': me.csrf_token || '',
        },
        body: '{}',
      });
      return resp.json();
    })()`);
    assert.strictEqual(revoked.ok, true, `revoke failed: ${JSON.stringify(revoked)}`);
    assert(revoked.closed_sessions >= 1, `revoke did not close the active dashboard session: ${JSON.stringify(revoked)}`);
    await waitFor(async () => {
      const status = await page.evaluate(() => window.intendantDashboardControl?.status?.() || null);
      if (!status) return null;
      if (!status.connected || status.pcState === 'closed' || status.channelState === 'closed') return status;
      return null;
    }, START_TIMEOUT_MS, 'active dashboard session close after revoke');
    const afterRevoke = await page.evaluate(async () => fetch('/api/daemons').then(r => r.json()));
    assert.deepStrictEqual(afterRevoke.daemons, [], 'daemon remained visible after revoke');
    const audit = await page.evaluate(async () => fetch('/api/audit').then(r => r.json()));
    const eventNames = new Set((audit.events || []).map(event => event.event));
    for (const name of ['passkey_registered', 'daemon_claimed', 'daemon_label_updated', 'dashboard_grant_started', 'dashboard_grant_answered', 'daemon_revoked']) {
      assert(eventNames.has(name), `missing audit event ${name}: ${JSON.stringify(audit)}`);
    }

    console.log(JSON.stringify({
      ok: true,
      daemon_id: options.daemonId,
      daemon_public_key: registered.daemon_public_key,
      dashboard_session_id: connected.sessionId,
      audit_events: Array.from(eventNames).sort(),
    }, null, 2));
  } finally {
    if (browser) await browser.close().catch(() => {});
    for (const child of children.reverse()) {
      if (child.exitCode === null && !child.killed) child.kill('SIGTERM');
    }
    await new Promise(resolve => setTimeout(resolve, 500));
    for (const child of children.reverse()) {
      if (child.exitCode === null && !child.killed) child.kill('SIGKILL');
    }
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

main().catch(err => {
  console.error(err && err.stack || err);
  process.exit(1);
});
