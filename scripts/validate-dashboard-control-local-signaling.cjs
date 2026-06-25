#!/usr/bin/env node
'use strict';

const assert = require('assert');
const path = require('path');
const { spawn } = require('child_process');
const { httpStatus, launchBrowser } = require('./lib/browser-automation.cjs');

const DEFAULT_DAEMON_PORT = 8877;
const START_TIMEOUT_MS = 30000;
const CONNECT_TIMEOUT_MS = 30000;

function parseArgs(argv) {
  const repoRoot = path.resolve(__dirname, '..');
  const out = {
    repoRoot,
    dashboardBinary: path.join(repoRoot, 'target', 'release', 'intendant'),
    daemonPort: DEFAULT_DAEMON_PORT,
  };
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--dashboard-binary') {
      out.dashboardBinary = path.resolve(argv[++i]);
    } else if (arg === '--daemon-port') {
      out.daemonPort = Number(argv[++i]);
    } else if (arg === '--help' || arg === '-h') {
      console.log(`Usage:
  node scripts/validate-dashboard-control-local-signaling.cjs [options]

Options:
  --dashboard-binary <path>   Intendant binary to launch.
  --daemon-port <port>        Fresh daemon web port. Default ${DEFAULT_DAEMON_PORT}.
`);
      process.exit(0);
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  assert(Number.isInteger(out.daemonPort) && out.daemonPort > 0, 'invalid daemon port');
  return out;
}

function wait(ms) {
  return new Promise(resolve => setTimeout(resolve, ms));
}

async function waitFor(predicate, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  let last;
  while (Date.now() < deadline) {
    last = await predicate();
    if (last) return last;
    await wait(200);
  }
  throw new Error(`timed out waiting for ${label}`);
}

async function waitForDashboardControl(page) {
  let last = null;
  const deadline = Date.now() + CONNECT_TIMEOUT_MS;
  while (Date.now() < deadline) {
    last = await page.evaluate(() => {
      if (!window.intendantDashboardControl) return null;
      return window.intendantDashboardControl.status();
    }).catch(() => null);
    if (
      last &&
      last.connected &&
      last.channelState === 'open' &&
      last.signalingMode === 'local-http' &&
      last.verifiedBinding &&
      last.verifiedBinding.ok
    ) {
      return last;
    }
    await page.waitForTimeout(250);
  }
  throw new Error(`dashboard control did not connect with local signaling: ${JSON.stringify(last)}`);
}

async function main() {
  const options = parseArgs(process.argv);
  const origin = `http://127.0.0.1:${options.daemonPort}`;
  const daemonLogs = [];
  const daemon = spawn(options.dashboardBinary, [
    '--no-tui',
    '--no-tls',
    '--bind',
    '127.0.0.1',
    '--web',
    String(options.daemonPort),
  ], {
    cwd: options.repoRoot,
    env: process.env,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  const daemonExit = new Promise(resolve => daemon.once('exit', resolve));
  daemon.stdout.on('data', chunk => daemonLogs.push(chunk.toString()));
  daemon.stderr.on('data', chunk => daemonLogs.push(chunk.toString()));
  daemon.once('error', err => daemonLogs.push(String(err && err.message || err)));

  let browser;
  try {
    await waitFor(async () => {
      try {
        const status = await httpStatus(`${origin}/config`, { timeoutMs: 2000 });
        return status === 200 ? status : null;
      } catch (_) {
        return null;
      }
    }, START_TIMEOUT_MS, 'plaintext dashboard readiness');

    browser = await launchBrowser({ headless: true });
    const page = await browser.newPage();
    page.on('console', msg => console.log(`[browser:${msg.type()}] ${msg.text()}`));

    const response = await page.goto(`${origin}/`, {
      waitUntil: 'domcontentloaded',
      timeout: CONNECT_TIMEOUT_MS,
    });
    assert(response, 'dashboard produced no response');
    assert.strictEqual(response.status(), 200, `dashboard returned ${response.status()}`);
    await page.waitForFunction(() => Boolean(window.intendantDashboardControl));
    await page.evaluate(() => {
      localStorage.setItem('intendant_dashboard_transport', 'webrtc-control');
      location.reload();
    });
    await page.waitForFunction(() => Boolean(window.intendantDashboardControl));

    const connected = await waitForDashboardControl(page);
    const result = await page.evaluate(async () => {
      const ctl = window.intendantDashboardControl;
      return {
        status: await ctl.request('status', {}, { timeoutMs: 60000 }),
        agentCard: await ctl.agentCard({ timeoutMs: 60000 }),
        cachedBootstrapEvents: await ctl.cachedBootstrapEvents({ timeoutMs: 60000 }),
        browserWorkspaceSnapshot: await ctl.browserWorkspaceSnapshot({ timeoutMs: 60000 }),
        sessions: await ctl.request('api_sessions', { limit: 2 }, { timeoutMs: 60000 }),
        rejectedControlMsg: await ctl.request('api_control_msg', {
          message: { action: 'create_session', task: 'noop' },
        }, { timeoutMs: 60000 }),
        finalStatus: ctl.status(),
      };
    });

    assert(result.status && result.status.session_id, 'status RPC did not return a session id');
    assert(result.agentCard && result.agentCard.id, 'api_agent_card did not return an id');
    assert(result.agentCard.label, 'api_agent_card did not return a label');
    assert(Array.isArray(result.cachedBootstrapEvents?.events), 'cached bootstrap events RPC did not return events');
    assert.strictEqual(
      result.cachedBootstrapEvents.event_count,
      result.cachedBootstrapEvents.events.length,
      'cached bootstrap events count did not match events length'
    );
    assert.strictEqual(
      result.browserWorkspaceSnapshot?.t,
      'browser_workspace_snapshot',
      'browser workspace snapshot RPC did not return the event shape'
    );
    assert(Array.isArray(result.browserWorkspaceSnapshot.workspaces), 'browser workspace snapshot did not return workspaces');
    assert(Array.isArray(result.sessions), 'api_sessions did not return an array');
    assert.strictEqual(result.finalStatus.signalingMode, 'local-http');
    assert.strictEqual(result.finalStatus.apiAgentCardAvailable, true);
    assert.strictEqual(result.finalStatus.apiCachedBootstrapEventsAvailable, true);
    assert.strictEqual(result.finalStatus.apiBrowserWorkspaceSnapshotAvailable, true);
    assert.strictEqual(result.finalStatus.apiControlMsgAvailable, true);
    assert.strictEqual(result.rejectedControlMsg?._httpStatus, 400);
    assert.strictEqual(result.rejectedControlMsg?._httpOk, false);
    assert(
      String(result.rejectedControlMsg?.error || '').includes('not available over dashboard WebRTC'),
      `unexpected control-message rejection: ${JSON.stringify(result.rejectedControlMsg)}`
    );
    assert.strictEqual(result.finalStatus.pendingRequests, 0, 'request map was not drained');

    console.log(JSON.stringify({
      origin,
      connected,
      rpc: {
        controlSessionId: result.status.session_id,
        agentCardId: result.agentCard.id,
        cachedBootstrapEventCount: result.cachedBootstrapEvents.event_count,
        browserWorkspaceCount: result.browserWorkspaceSnapshot.workspaces.length,
        sessionCount: result.sessions.length,
        apiAgentCardAvailable: result.finalStatus.apiAgentCardAvailable,
        apiCachedBootstrapEventsAvailable: result.finalStatus.apiCachedBootstrapEventsAvailable,
        apiBrowserWorkspaceSnapshotAvailable: result.finalStatus.apiBrowserWorkspaceSnapshotAvailable,
        apiControlMsgAvailable: result.finalStatus.apiControlMsgAvailable,
        rejectedControlStatus: result.rejectedControlMsg._httpStatus,
        signalingMode: result.finalStatus.signalingMode,
        pendingRequests: result.finalStatus.pendingRequests,
      },
    }, null, 2));

    await page.evaluate(() => window.intendantDashboardControl.disable());
  } finally {
    if (browser) await browser.close().catch(() => {});
    if (!daemon.killed) daemon.kill('SIGINT');
    await Promise.race([daemonExit, wait(5000)]);
    if (daemon.exitCode === null) daemon.kill('SIGKILL');
    if (daemonLogs.length && daemon.exitCode && daemon.exitCode !== 0 && daemon.exitCode !== 130) {
      console.error(daemonLogs.join('').slice(-4000));
    }
  }
}

main().catch(err => {
  console.error(err);
  process.exit(1);
});
