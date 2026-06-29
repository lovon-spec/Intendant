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

async function waitBounded(promise, timeoutMs) {
  let timer = null;
  try {
    return await Promise.race([
      promise,
      new Promise(resolve => {
        timer = setTimeout(() => resolve(undefined), timeoutMs);
        if (typeof timer.unref === 'function') timer.unref();
      }),
    ]);
  } finally {
    if (timer) clearTimeout(timer);
  }
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

async function dashboardAccessUi(page) {
  return page.evaluate(() => {
    const normalizedText = el => String(el?.textContent || '').replace(/\s+/g, ' ').trim();
    const read = selector => {
      const el = document.querySelector(selector);
      return {
        text: normalizedText(el),
        className: String(el?.className || ''),
        title: String(el?.title || ''),
      };
    };
    return {
      status: read('#sb-dashboard-transport'),
      statusLabel: normalizedText(document.querySelector('#sb-dashboard-transport-label')),
      diagnosticsLegend: normalizedText(document.querySelector('#connect-health-panel legend')),
      files: read('#files-target-summary'),
      shell: read('#shell-target-summary'),
    };
  });
}

async function typeText(page, text) {
  if (page.keyboard?.type) {
    await page.keyboard.type(text);
    return;
  }
  await page.connection.send('Input.insertText', { text }, page.sessionId);
}

async function pressKey(page, key) {
  if (page.keyboard?.press) {
    await page.keyboard.press(key);
    return;
  }
  const keyMap = {
    Enter: { key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13, nativeVirtualKeyCode: 13 },
  };
  const event = keyMap[key] || { key, code: key, windowsVirtualKeyCode: 0, nativeVirtualKeyCode: 0 };
  await page.connection.send('Input.dispatchKeyEvent', { type: 'keyDown', ...event }, page.sessionId);
  await page.connection.send('Input.dispatchKeyEvent', { type: 'keyUp', ...event }, page.sessionId);
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
    const genericDownloadText = [
      'connect generic download fixture',
      'range one',
      'range two',
      'range three',
      'done',
    ].join('\n');
    const genericDownloadPath = path.join(tmp, 'connect-generic-download.txt');
    fs.writeFileSync(genericDownloadPath, genericDownloadText);

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
      cwd: tmp,
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

    await goto(page, `${connectOrigin}/app?connect=1&daemon_id=${encodeURIComponent(options.daemonId)}#terminal/shell`, {
      timeout: START_TIMEOUT_MS,
    });
    let connected;
    try {
      connected = await waitFor(async () => {
        const status = await page.evaluate(() => window.intendantDashboardControl?.status?.() || null);
        if (
          status?.connected &&
          status?.verifiedBinding?.ok &&
          status?.signalingMode === 'connect-rendezvous' &&
          status?.terminalFramesAvailable === true &&
          status?.tuiFramesAvailable === false
        ) {
          return status;
        }
        return null;
      }, CONNECT_TIMEOUT_MS, 'hosted dashboard Connect tunnel capabilities');
    } catch (err) {
      const status = await page.evaluate(() => window.intendantDashboardControl?.status?.() || null).catch(e => ({ error: e.message }));
      throw new Error(`${err.message}; last dashboard status: ${JSON.stringify(status)}`);
    }
    assert.strictEqual(connected.claimedDaemonPublicKey, registered.daemon_public_key);
    assert.strictEqual(connected.verifiedBinding.daemonPublicKey, registered.daemon_public_key);
    assert(connected.sessionGrantSha256, 'Connect dashboard did not bind a session grant');
    assert.strictEqual(connected.terminalFramesAvailable, true, `Connect tunnel did not advertise terminal frames: ${JSON.stringify(connected)}`);
    assert.strictEqual(connected.tuiFramesAvailable, false, `--no-tui daemon unexpectedly advertised TUI frames: ${JSON.stringify(connected)}`);
    assert.strictEqual(connected.byteStreamsAvailable, true, `Connect tunnel did not advertise byte streams: ${JSON.stringify(connected)}`);
    assert.strictEqual(connected.apiFsReadAvailable, true, `Connect tunnel did not advertise filesystem reads: ${JSON.stringify(connected)}`);
    assert.strictEqual(connected.apiTransferJobsAvailable, true, `Connect tunnel did not advertise transfer jobs: ${JSON.stringify(connected)}`);
    assert.strictEqual(connected.apiTransferJobCreateAvailable, true, `Connect tunnel did not advertise transfer job creation: ${JSON.stringify(connected)}`);
    assert.strictEqual(connected.apiTransferDownloadReadAvailable, true, `Connect tunnel did not advertise transfer downloads: ${JSON.stringify(connected)}`);
    assert.strictEqual(connected.apiTransferUploadChunkAvailable, true, `Connect tunnel did not advertise transfer upload chunks: ${JSON.stringify(connected)}`);
    assert.strictEqual(connected.apiTransferUploadCommitAvailable, true, `Connect tunnel did not advertise transfer upload commit: ${JSON.stringify(connected)}`);

    async function waitForDashboardReconnect(label = 'hosted dashboard reconnect') {
      return waitFor(async () => {
        const status = await page.evaluate(() => window.intendantDashboardControl?.status?.() || null);
        if (status?.connected && status?.verifiedBinding?.ok) return status;
        return null;
      }, CONNECT_TIMEOUT_MS, label);
    }

    async function reloadHostedFilesPage(label) {
      const url = `${connectOrigin}/app?connect=1&daemon_id=${encodeURIComponent(options.daemonId)}#files`;
      await page.evaluate(`window.history.replaceState(null, '', ${JSON.stringify(url)})`);
      if (typeof page.reload === 'function') {
        await page.reload({ timeout: START_TIMEOUT_MS }).catch(() => {});
      } else if (page.connection && page.sessionId) {
        await page.connection.send('Page.reload', { ignoreCache: true }, page.sessionId, START_TIMEOUT_MS).catch(() => {});
      } else {
        await page.evaluate('window.location.reload()').catch(() => {});
      }
      await new Promise(resolve => setTimeout(resolve, 500));
      await waitForDashboardReconnect(label);
      await click(page, '.tab-btn[data-tab="files"]');
    }

    await click(page, '.tab-btn[data-tab="access"]');
    await click(page, '#access-subtabs .subtab-btn[data-access-tab="diagnostics"]');
    const healthPanel = await page.evaluate(() => window.intendantDashboardControl._debugProbeConnectHealthPanel());
    assert.strictEqual(healthPanel.state.connectMode, true, `Connect health panel did not detect hosted Connect mode: ${JSON.stringify(healthPanel)}`);
    assert.strictEqual(healthPanel.state.verifiedBindingOk, true, `Connect health panel did not show a verified binding: ${JSON.stringify(healthPanel)}`);
    assert(healthPanel.summaryText.includes('Hosted Connect'), `Connect health summary missing hosted mode: ${JSON.stringify(healthPanel)}`);
    assert.strictEqual(healthPanel.result.ok, true, `Connect health self-test failed: ${JSON.stringify(healthPanel.result)}`);
    assert.strictEqual(healthPanel.result.failed, 0, `Connect health self-test had failures: ${JSON.stringify(healthPanel.result)}`);
    assert.strictEqual(healthPanel.result.skipped, 0, `Connect health self-test skipped probes: ${JSON.stringify(healthPanel.result)}`);
    assert(healthPanel.resultText.includes('pass'), `Connect health self-test did not render pass rows: ${JSON.stringify(healthPanel)}`);

    await click(page, '.tab-btn[data-tab="files"]');
    const filesAccessUi = await dashboardAccessUi(page);
    assert(
      ['Ready', 'Relay'].includes(filesAccessUi.statusLabel),
      `dashboard access status should be user-facing: ${JSON.stringify(filesAccessUi)}`
    );
    assert.strictEqual(filesAccessUi.diagnosticsLegend, 'Connection Diagnostics', `Access Diagnostics should own transport details: ${JSON.stringify(filesAccessUi)}`);
    assert(filesAccessUi.files.text.includes('This daemon'), `Files target summary did not identify the local daemon: ${JSON.stringify(filesAccessUi)}`);
    assert(filesAccessUi.files.text.includes('Hosted Connect'), `Files target summary did not name Hosted Connect mode: ${JSON.stringify(filesAccessUi)}`);
    assert(filesAccessUi.files.text.includes('full dashboard access'), `Files target summary did not summarize access: ${JSON.stringify(filesAccessUi)}`);
    assert(filesAccessUi.files.text.includes('Files'), `Files target summary did not include the Files capability: ${JSON.stringify(filesAccessUi)}`);
    assert(!/\b(DataChannel|dashboard-control|tunnel|mTLS|ICE)\b/i.test(`${filesAccessUi.statusLabel} ${filesAccessUi.files.text}`), `Files target summary leaked protocol wording: ${JSON.stringify(filesAccessUi)}`);
    const filesDownloadPanel = await page.evaluate(`window.intendantDashboardFiles._debugProbeDownloadPath(${JSON.stringify(genericDownloadPath)}, { chunkBytes: 11 })`);
    assert.strictEqual(filesDownloadPanel.path, genericDownloadPath, `Files tab did not keep selected path: ${JSON.stringify(filesDownloadPanel)}`);
    assert(filesDownloadPanel.rangeCount >= 2, `Files tab download did not use multiple ranges: ${JSON.stringify(filesDownloadPanel)}`);
    assert.strictEqual(filesDownloadPanel.text, genericDownloadText, `Files tab download returned wrong bytes: ${JSON.stringify(filesDownloadPanel)}`);
    assert.strictEqual(filesDownloadPanel.size, Buffer.byteLength(genericDownloadText), `Files tab download returned wrong size: ${JSON.stringify(filesDownloadPanel)}`);
    assert(filesDownloadPanel.statusText.includes('Downloaded'), `Files tab did not render completion status: ${JSON.stringify(filesDownloadPanel)}`);
    assert.strictEqual(filesDownloadPanel.progressWidth, '100%', `Files tab did not render complete progress: ${JSON.stringify(filesDownloadPanel)}`);

    const filesInterruptedDownload = await page.evaluate(`window.intendantDashboardFiles._debugProbeInterruptedDownload(${JSON.stringify(genericDownloadPath)}, { chunkBytes: 11 })`);
    assert.strictEqual(filesInterruptedDownload.failedStatus, 'failed', `Files tab interrupted download did not fail first: ${JSON.stringify(filesInterruptedDownload)}`);
    assert(filesInterruptedDownload.failedLoaded > 0, `Files tab interrupted download did not retain partial bytes: ${JSON.stringify(filesInterruptedDownload)}`);
    assert(filesInterruptedDownload.failedRangeCount >= 1, `Files tab interrupted download did not record ranges before failure: ${JSON.stringify(filesInterruptedDownload)}`);
    assert.strictEqual(filesInterruptedDownload.finalStatus, 'completed', `Files tab interrupted download did not complete after resume: ${JSON.stringify(filesInterruptedDownload)}`);
    assert(filesInterruptedDownload.finalRangeCount >= 2, `Files tab resumed download did not use multiple ranges: ${JSON.stringify(filesInterruptedDownload)}`);
    assert.strictEqual(filesInterruptedDownload.text, genericDownloadText, `Files tab resumed download returned wrong bytes: ${JSON.stringify(filesInterruptedDownload)}`);
    assert.strictEqual(filesInterruptedDownload.size, Buffer.byteLength(genericDownloadText), `Files tab resumed download returned wrong size: ${JSON.stringify(filesInterruptedDownload)}`);

    const reloadDownloadStart = await page.evaluate(`window.intendantDashboardFiles._debugStartInterruptedDownload(${JSON.stringify(genericDownloadPath)}, { chunkBytes: 11 })`);
    assert.strictEqual(reloadDownloadStart.status, 'failed', `Reload download setup did not fail first: ${JSON.stringify(reloadDownloadStart)}`);
    assert(reloadDownloadStart.loaded > 0, `Reload download setup did not retain partial bytes: ${JSON.stringify(reloadDownloadStart)}`);
    assert(reloadDownloadStart.snapshot?.resumeToken, `Reload download setup did not persist resume token: ${JSON.stringify(reloadDownloadStart)}`);
    await reloadHostedFilesPage('hosted dashboard reconnect after download reload');
    const reloadDownloadRestored = await page.evaluate(`window.intendantDashboardFiles._debugTransferSnapshot().find(item => item.id === ${JSON.stringify(reloadDownloadStart.transferId)}) || null`);
    assert(reloadDownloadRestored, `Reload download transfer was not restored: ${JSON.stringify(reloadDownloadStart)}`);
    assert.strictEqual(reloadDownloadRestored.resumeToken, reloadDownloadStart.snapshot.resumeToken, `Reload download resume token changed: ${JSON.stringify(reloadDownloadRestored)}`);
    assert(reloadDownloadRestored.loaded > 0, `Reload download restored without partial progress: ${JSON.stringify(reloadDownloadRestored)}`);
    const reloadDownloadResumed = await page.evaluate(`window.intendantDashboardFiles._debugResumeTransfer(${JSON.stringify(reloadDownloadStart.transferId)})`);
    assert.strictEqual(reloadDownloadResumed.status, 'completed', `Reload download did not complete after resume: ${JSON.stringify(reloadDownloadResumed)}`);
    assert.strictEqual(reloadDownloadResumed.rawText, genericDownloadText, `Reload download resume returned wrong bytes: ${JSON.stringify(reloadDownloadResumed)}`);

    await click(page, '.tab-btn[data-tab="terminal"]');
    await click(page, '#tab-terminal .subtab-btn[data-term-tab="tui"]');
    await page.waitForFunction(() => {
      const el = document.getElementById('terminal-tui-unavailable');
      return Boolean(
        el &&
        !el.classList.contains('hidden') &&
        el.textContent.includes('TUI unavailable for this daemon')
      );
    }, {
      timeout: START_TIMEOUT_MS,
    });

    const shellToken = `connect_shell_${Date.now()}`;
    await click(page, '#tab-terminal .subtab-btn[data-term-tab="shell"]');
    const shellAccessUi = await dashboardAccessUi(page);
    assert(shellAccessUi.shell.text.includes('This daemon'), `Shell target summary did not identify the local daemon: ${JSON.stringify(shellAccessUi)}`);
    assert(shellAccessUi.shell.text.includes('Hosted Connect'), `Shell target summary did not name Hosted Connect mode: ${JSON.stringify(shellAccessUi)}`);
    assert(shellAccessUi.shell.text.includes('Shell'), `Shell target summary did not include the Shell capability: ${JSON.stringify(shellAccessUi)}`);
    assert(!/\b(DataChannel|dashboard-control|tunnel|mTLS|ICE)\b/i.test(shellAccessUi.shell.text), `Shell target summary leaked protocol wording: ${JSON.stringify(shellAccessUi)}`);
    await page.waitForFunction(() => Boolean(document.querySelector('#term-pane-shell.active #shell-container .xterm')), {
      timeout: START_TIMEOUT_MS,
    });
    await click(page, '#shell-container');
    await typeText(page, `echo ${shellToken}`);
    await pressKey(page, 'Enter');
    await page.waitForFunction(`(() => {
      const el = document.getElementById('shell-container');
      return Boolean(el && el.textContent && el.textContent.includes(${JSON.stringify(shellToken)}));
    })()`, {
      timeout: START_TIMEOUT_MS,
    });

    const probes = await page.evaluate(async () => {
      const control = window.intendantDashboardControl;
      const names = [
        '_debugProbeControlNoReplay',
        '_debugProbeControlUnavailableConnectNoLegacy',
        '_debugProbeMediaConnectNoLegacy',
        '_debugProbePeerMutationConnectNoHttp',
        '_debugProbeDiagnosticsConnectNoHttp',
        '_debugProbeDisplaySignalConnectNoLegacy',
        '_debugProbeDisplayAuthorityConnectNoLegacy',
        '_debugProbeTuiConnectNoLegacy',
        '_debugProbeShellQueuesUntilOpened',
        '_debugProbeTerminalOutputBypassesDedupe',
        '_debugProbeEventDedupeAllowlist',
        '_debugProbeDuplicateShellInputSends',
        '_debugProbePresenceMediaConnectNoLegacy',
        '_debugProbePresenceServerSenderConnectNoLegacy',
        '_debugProbeTunneledPresenceServerCallback',
      ];
      const out = {};
      for (const name of names) {
        out[name] = await control[name]();
      }
      return out;
    });
    for (const [name, probe] of Object.entries(probes)) {
      assert.strictEqual(probe.skipped, false, `${name} skipped: ${JSON.stringify(probe)}`);
    }
    assert.strictEqual(probes._debugProbeControlNoReplay.wsReplayCount, 0, `control RPC failure replayed over WS: ${JSON.stringify(probes)}`);
    assert(probes._debugProbeControlNoReplay.rpcAttempts >= 1, `control probe did not attempt RPC: ${JSON.stringify(probes)}`);
    assert(probes._debugProbeControlNoReplay.rpcFailureWarnings >= 1, `control probe did not warn on RPC failure: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeControlUnavailableConnectNoLegacy.wsReplayCount, 0, `unavailable control path replayed over WS: ${JSON.stringify(probes)}`);
    assert(probes._debugProbeControlUnavailableConnectNoLegacy.unavailableWarnings >= 3, `unavailable control path did not surface all warnings: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeMediaConnectNoLegacy.threw, true, `media unavailable path did not throw: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeMediaConnectNoLegacy.wsReplayCount, 0, `media unavailable path replayed over WS: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbePeerMutationConnectNoHttp.threw, true, `peer mutation RPC failure did not throw: ${JSON.stringify(probes)}`);
    assert(probes._debugProbePeerMutationConnectNoHttp.rpcAttempts >= 1, `peer mutation probe did not attempt RPC: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbePeerMutationConnectNoHttp.httpFallbackCount, 0, `peer mutation used HTTP fallback: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeDiagnosticsConnectNoHttp.threw, true, `diagnostics unavailable path did not throw: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeDiagnosticsConnectNoHttp.httpFallbackCount, 0, `diagnostics unavailable path used HTTP fallback: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeDisplaySignalConnectNoLegacy.wsReplayCount, 0, `display signaling used WS fallback: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeDisplaySignalConnectNoLegacy.httpFallbackCount, 0, `display signaling used HTTP fallback: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeDisplayAuthorityConnectNoLegacy.requestResult, false, `display authority request unexpectedly succeeded without tunnel support: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeDisplayAuthorityConnectNoLegacy.releaseResult, false, `display authority release unexpectedly succeeded without tunnel support: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeDisplayAuthorityConnectNoLegacy.requestReplayCount, 0, `display authority request used legacy path: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeDisplayAuthorityConnectNoLegacy.releaseReplayCount, 0, `display authority release used legacy path: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeTuiConnectNoLegacy.keyReplayCount, 0, `TUI key used legacy path: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeTuiConnectNoLegacy.resizeReplayCount, 0, `TUI resize used legacy path: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeTuiConnectNoLegacy.wsReplayCount, 0, `TUI subscription used WS fallback: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeTuiConnectNoLegacy.subscriptionSent, false, `TUI subscription unexpectedly sent over WS: ${JSON.stringify(probes)}`);
    assert.deepStrictEqual(probes._debugProbeShellQueuesUntilOpened.framesBeforeAck, ['terminal_open'], `Shell input was sent before terminal_opened: ${JSON.stringify(probes)}`);
    assert.deepStrictEqual(probes._debugProbeShellQueuesUntilOpened.framesAfterAck, ['terminal_open', 'terminal_resize', 'terminal_input'], `Shell queued input did not flush after terminal_opened: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeShellQueuesUntilOpened.queuedBeforeAck, 'queued-before-open', `Shell probe did not queue input before ACK: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeShellQueuesUntilOpened.queuedAfterAck, '', `Shell queued input was not cleared after ACK: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeShellQueuesUntilOpened.ackedBeforeAck, false, `Shell unexpectedly marked ACKed before terminal_opened: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeShellQueuesUntilOpened.ackedAfterAck, true, `Shell did not mark ACKed after terminal_opened: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeTerminalOutputBypassesDedupe.writtenBytes, 2, `duplicate Shell output was deduped: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeTerminalOutputBypassesDedupe.recentKeyCount, 0, `terminal output polluted event dedupe state: ${JSON.stringify(probes)}`);
    assert.deepStrictEqual(
      probes._debugProbeEventDedupeAllowlist.status,
      { first: false, second: true, recentKeyCount: 1 },
      `dedupable status event was not deduped: ${JSON.stringify(probes)}`
    );
    assert.deepStrictEqual(
      probes._debugProbeEventDedupeAllowlist.sessionIdentity,
      { first: false, second: true, recentKeyCount: 1 },
      `dedupable session identity event was not deduped: ${JSON.stringify(probes)}`
    );
    for (const name of ['terminalOutput', 'displayIce', 'modelDelta', 'logEntry', 'peerEventForwarded', 'futureEvent']) {
      assert.deepStrictEqual(
        probes._debugProbeEventDedupeAllowlist[name],
        { first: false, second: false, recentKeyCount: 0 },
        `${name} was incorrectly deduped: ${JSON.stringify(probes)}`
      );
    }
    assert.deepStrictEqual(probes._debugProbeDuplicateShellInputSends.inputFrames, ['eA==', 'eA=='], `duplicate Shell input was not sent twice: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeDuplicateShellInputSends.queuedAfterSend, '', `duplicate Shell input was unexpectedly queued: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbePresenceMediaConnectNoLegacy.presenceFrameCount, 2, `presence frames did not use tunnel: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbePresenceMediaConnectNoLegacy.uploadCount, 1, `presence video did not use upload tunnel: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbePresenceMediaConnectNoLegacy.legacyCount, 0, `presence media used legacy path: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbePresenceServerSenderConnectNoLegacy.presenceFrameCount, 2, `presence server sender did not use frame tunnel: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbePresenceServerSenderConnectNoLegacy.actionRpcCount, 1, `presence server sender did not use action RPC: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeTunneledPresenceServerCallback.handled, true, `tunneled presence callback was not handled: ${JSON.stringify(probes)}`);
    assert.strictEqual(probes._debugProbeTunneledPresenceServerCallback.diagnosticCount, 1, `tunneled presence callback did not emit diagnostic: ${JSON.stringify(probes)}`);

    const fsDownloadProbe = await page.evaluate(`window.intendantDashboardControl._debugProbeResumableFsDownload(${JSON.stringify(genericDownloadPath)}, { chunkBytes: 11 })`);
    assert.strictEqual(fsDownloadProbe.skipped, false, `resumable filesystem download probe skipped: ${JSON.stringify(fsDownloadProbe)}`);
    assert.strictEqual(fsDownloadProbe.httpFallbackCount, 0, `resumable filesystem download used HTTP fallback: ${JSON.stringify(fsDownloadProbe)}`);
    assert(fsDownloadProbe.rangeCount >= 2, `resumable filesystem download did not use multiple ranges: ${JSON.stringify(fsDownloadProbe)}`);
    assert.strictEqual(fsDownloadProbe.text, genericDownloadText, `resumable filesystem download returned wrong bytes: ${JSON.stringify(fsDownloadProbe)}`);
    assert.strictEqual(fsDownloadProbe.size, Buffer.byteLength(genericDownloadText), `resumable filesystem download returned wrong size: ${JSON.stringify(fsDownloadProbe)}`);

    const uploadRawText = 'connect upload raw byte-stream fixture';
    const uploadRawProbe = await page.evaluate(`(async () => {
      const previousFetch = window.fetch;
      let httpFallbackCount = 0;
      window.fetch = function(input, init) {
        const url = typeof input === 'string' ? input : (input && input.url || '');
        if (String(url).includes('/api/session/current/uploads')) httpFallbackCount += 1;
        return previousFetch.call(this, input, init);
      };
      try {
        const ctl = window.intendantDashboardControl;
        const text = ${JSON.stringify(uploadRawText)};
        const bytes = new TextEncoder().encode(text);
        const upload = await ctl.uploadBytes('api_session_current_upload', {
          destination: 'task',
          name: 'connect-upload-raw.txt',
          mime: 'text/plain',
        }, bytes, { timeoutMs: 120000 });
        const raw = await ctl.requestBytes('api_session_current_upload_raw', {
          id: upload.id,
          offset: 8,
          length: 10,
        }, { timeoutMs: 120000 });
        return {
          httpFallbackCount,
          uploadId: upload.id || '',
          uploadName: upload.name || '',
          rawText: new TextDecoder().decode(raw.bytes),
          rawSize: raw.size,
          rawTotalSize: raw.total_size,
          rawRangeStart: raw.range_start,
          rawRangeEnd: raw.range_end,
        };
      } finally {
        window.fetch = previousFetch;
      }
    })()`);
    assert(uploadRawProbe.uploadId, `upload did not return an id: ${JSON.stringify(uploadRawProbe)}`);
    assert.strictEqual(uploadRawProbe.uploadName, 'connect-upload-raw.txt', `upload returned wrong descriptor: ${JSON.stringify(uploadRawProbe)}`);
    assert.strictEqual(uploadRawProbe.httpFallbackCount, 0, `upload/raw path used HTTP fallback: ${JSON.stringify(uploadRawProbe)}`);
    assert.strictEqual(uploadRawProbe.rawText, uploadRawText.slice(8, 18), `upload raw read returned wrong range: ${JSON.stringify(uploadRawProbe)}`);
    assert.strictEqual(uploadRawProbe.rawSize, 10, `upload raw read returned wrong size: ${JSON.stringify(uploadRawProbe)}`);
    assert.strictEqual(uploadRawProbe.rawTotalSize, Buffer.byteLength(uploadRawText), `upload raw read returned wrong total size: ${JSON.stringify(uploadRawProbe)}`);
    assert.strictEqual(uploadRawProbe.rawRangeStart, 8, `upload raw read returned wrong start: ${JSON.stringify(uploadRawProbe)}`);
    assert.strictEqual(uploadRawProbe.rawRangeEnd, 18, `upload raw read returned wrong end: ${JSON.stringify(uploadRawProbe)}`);

    const filesUploadProbe = await page.evaluate(`window.intendantDashboardFiles._debugProbeUploadText(${JSON.stringify(uploadRawText)}, { name: 'connect-files-upload.txt', chunkBytes: 9 })`);
    assert(filesUploadProbe.uploadId, `Files tab upload did not return an id: ${JSON.stringify(filesUploadProbe)}`);
    assert.strictEqual(filesUploadProbe.uploadName, 'connect-files-upload.txt', `Files tab upload returned wrong descriptor: ${JSON.stringify(filesUploadProbe)}`);
    assert.strictEqual(filesUploadProbe.transferStatus, 'completed', `Files tab upload transfer did not complete: ${JSON.stringify(filesUploadProbe)}`);
    assert.strictEqual(filesUploadProbe.httpFallbackCount, 0, `Files tab upload/raw path used HTTP fallback: ${JSON.stringify(filesUploadProbe)}`);
    assert.strictEqual(filesUploadProbe.rawText, uploadRawText, `Files tab upload raw read returned wrong bytes: ${JSON.stringify(filesUploadProbe)}`);
    assert(filesUploadProbe.rawRangeCount >= 2, `Files tab upload raw read did not use multiple ranges: ${JSON.stringify(filesUploadProbe)}`);
    assert(filesUploadProbe.statusText.includes('Uploaded'), `Files tab upload did not render completion status: ${JSON.stringify(filesUploadProbe)}`);

    const stagedArtifactProbe = await page.evaluate(`window.intendantDashboardFiles._debugProbeArtifactDownload({
      type: 'staged_upload',
      id: ${JSON.stringify(filesUploadProbe.uploadId)}
    }, {
      sourceLabel: 'Staged upload: connect-files-upload.txt',
      filename: 'connect-files-upload.txt',
      contentType: 'text/plain',
      chunkBytes: 9
    })`);
    assert.strictEqual(stagedArtifactProbe.text, uploadRawText, `Staged upload artifact transfer returned wrong bytes: ${JSON.stringify(stagedArtifactProbe)}`);
    assert(stagedArtifactProbe.rangeCount >= 2, `Staged upload artifact transfer did not use multiple ranges: ${JSON.stringify(stagedArtifactProbe)}`);
    assert(stagedArtifactProbe.transfer?.serverJobId, `Staged upload artifact transfer did not keep server job id: ${JSON.stringify(stagedArtifactProbe)}`);
    assert(stagedArtifactProbe.transfer?.resumeToken, `Staged upload artifact transfer did not keep resume token: ${JSON.stringify(stagedArtifactProbe)}`);
    assert.strictEqual(stagedArtifactProbe.transfer?.sourceKind, 'staged_upload', `Staged upload artifact transfer kept wrong source kind: ${JSON.stringify(stagedArtifactProbe)}`);
    assert.strictEqual(stagedArtifactProbe.transfer?.sourceLabel, 'Staged upload: connect-files-upload.txt', `Staged upload artifact transfer kept wrong source label: ${JSON.stringify(stagedArtifactProbe)}`);
    assert.strictEqual(stagedArtifactProbe.transfer?.artifact?.type, 'staged_upload', `Staged upload artifact transfer did not persist artifact descriptor: ${JSON.stringify(stagedArtifactProbe)}`);
    assert(stagedArtifactProbe.statusText.includes('Downloaded'), `Staged upload artifact transfer did not render completion status: ${JSON.stringify(stagedArtifactProbe)}`);

    const filesystemUploadText = 'connect filesystem upload fixture';
    const filesystemUploadPath = path.join(tmp, 'connect-filesystem-upload.txt');
    const filesystemUploadProbe = await page.evaluate(`window.intendantDashboardFiles._debugProbeFilesystemUploadText(${JSON.stringify(filesystemUploadText)}, { destination: ${JSON.stringify(filesystemUploadPath)}, name: 'connect-filesystem-upload.txt', chunkBytes: 9 })`);
    assert.strictEqual(filesystemUploadProbe.rawText, filesystemUploadText, `Files tab filesystem upload readback mismatch: ${JSON.stringify(filesystemUploadProbe)}`);
    assert.strictEqual(filesystemUploadProbe.transferStatus, 'completed', `Files tab filesystem upload did not complete: ${JSON.stringify(filesystemUploadProbe)}`);
    assert(filesystemUploadProbe.serverJobId, `Files tab filesystem upload did not keep server job id: ${JSON.stringify(filesystemUploadProbe)}`);
    assert(filesystemUploadProbe.resumeToken, `Files tab filesystem upload did not keep resume token: ${JSON.stringify(filesystemUploadProbe)}`);
    assert.strictEqual(fs.readFileSync(filesystemUploadPath, 'utf8'), filesystemUploadText, 'filesystem upload did not commit to daemon path');

    const reloadUploadText = 'connect filesystem upload reload resume fixture';
    const reloadUploadPath = path.join(tmp, 'connect-filesystem-upload-reload.txt');
    const reloadUploadStart = await page.evaluate(`window.intendantDashboardFiles._debugStartInterruptedFilesystemUploadText(${JSON.stringify(reloadUploadText)}, { destination: ${JSON.stringify(reloadUploadPath)}, name: 'connect-filesystem-upload-reload.txt', chunkBytes: 9, failAfterChunks: 1 })`);
    assert.strictEqual(reloadUploadStart.status, 'failed', `Reload upload setup did not fail first: ${JSON.stringify(reloadUploadStart)}`);
    assert(reloadUploadStart.loaded > 0, `Reload upload setup did not retain partial bytes: ${JSON.stringify(reloadUploadStart)}`);
    assert(reloadUploadStart.snapshot?.resumeToken, `Reload upload setup did not persist resume token: ${JSON.stringify(reloadUploadStart)}`);
    assert.strictEqual(fs.existsSync(reloadUploadPath), false, 'interrupted upload committed before resume');
    await reloadHostedFilesPage('hosted dashboard reconnect after upload reload');
    const reloadUploadRestored = await page.evaluate(`window.intendantDashboardFiles._debugTransferSnapshot().find(item => item.id === ${JSON.stringify(reloadUploadStart.transferId)}) || null`);
    assert(reloadUploadRestored, `Reload upload transfer was not restored: ${JSON.stringify(reloadUploadStart)}`);
    assert.strictEqual(reloadUploadRestored.resumeToken, reloadUploadStart.snapshot.resumeToken, `Reload upload resume token changed: ${JSON.stringify(reloadUploadRestored)}`);
    assert(reloadUploadRestored.uploadBlobStored, `Reload upload did not preserve local upload blob: ${JSON.stringify(reloadUploadRestored)}`);
    const reloadUploadResumed = await page.evaluate(`window.intendantDashboardFiles._debugResumeTransfer(${JSON.stringify(reloadUploadStart.transferId)})`);
    assert.strictEqual(reloadUploadResumed.status, 'completed', `Reload upload did not complete after resume: ${JSON.stringify(reloadUploadResumed)}`);
    assert.strictEqual(reloadUploadResumed.rawText, reloadUploadText, `Reload upload resume returned wrong bytes: ${JSON.stringify(reloadUploadResumed)}`);
    assert.strictEqual(fs.readFileSync(reloadUploadPath, 'utf8'), reloadUploadText, 'reload upload did not commit to daemon path');

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
    if (browser) await waitBounded(browser.close().catch(() => {}), 5000);
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

main()
  .then(() => process.exit(0))
  .catch(err => {
    console.error(err && err.stack || err);
    process.exit(1);
  });
