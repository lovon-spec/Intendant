#!/usr/bin/env node
'use strict';

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { httpJson, httpStatus, launchBrowser } = require('./lib/browser-automation.cjs');

const DEFAULT_ORIGIN = 'https://127.0.0.1:8766';
const CONNECT_TIMEOUT_MS = 30000;

function usage() {
  console.log(`Usage:
  node scripts/validate-connect-bootstrap.cjs [--origin <https-origin>]

Environment:
  INTENDANT_CONNECT_ORIGIN   Origin to test. Defaults to ${DEFAULT_ORIGIN}.
  PLAYWRIGHT_NODE_PATH       Optional node_modules directory containing playwright.
  CHROME_PATH/CHROME_BIN     Optional Chromium executable for the CDP fallback.
`);
}

function parseArgs(argv) {
  let origin = process.env.INTENDANT_CONNECT_ORIGIN || DEFAULT_ORIGIN;
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--help' || arg === '-h') {
      usage();
      process.exit(0);
    }
    if (arg === '--origin') {
      origin = argv[i + 1];
      i += 1;
      continue;
    }
    throw new Error(`unknown argument: ${arg}`);
  }
  if (!origin || !/^https:\/\//.test(origin)) {
    throw new Error(`origin must be an https origin, got: ${origin || '<empty>'}`);
  }
  return { origin: origin.replace(/\/+$/, '') };
}

async function waitForConnect(page) {
  const deadline = Date.now() + CONNECT_TIMEOUT_MS;
  let last = null;
  while (Date.now() < deadline) {
    try {
      last = await page.evaluate(() => {
        if (!window.intendantConnectDashboard) return null;
        return window.intendantConnectDashboard.status();
      });
    } catch (err) {
      if (!String(err && err.message || err).includes('Execution context was destroyed')) {
        throw err;
      }
      await page.waitForLoadState('domcontentloaded').catch(() => {});
      last = null;
    }
    if (
      last &&
      last.connected &&
      last.channelState === 'open' &&
      last.verifiedBinding &&
      last.verifiedBinding.ok
    ) {
      return last;
    }
    await page.waitForTimeout(250);
  }
  throw new Error(`connect bootstrap did not connect: ${JSON.stringify(last)}`);
}

async function main() {
  const { origin } = parseArgs(process.argv);
  const browser = await launchBrowser({ headless: true, ignoreHTTPSErrors: true });
  const filesystemFixture = (() => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'intendant-connect-bootstrap-fs-'));
    const filePath = path.join(dir, 'filesystem-read.txt');
    const text = 'connect bootstrap filesystem read e2e';
    fs.writeFileSync(filePath, text);
    return { dir, filePath, text };
  })();

  try {
    const certlessConfigStatus = await httpStatus(`${origin}/config`, { ignoreHTTPSErrors: true });
    assert.strictEqual(
      certlessConfigStatus,
      401,
      `/config without client cert returned ${certlessConfigStatus}`
    );

    const statusBody = await httpJson(`${origin}/connect/status`, { ignoreHTTPSErrors: true });
    assert.strictEqual(
      statusBody.transport,
      'webrtc-dashboard-control',
      'connect status did not advertise dashboard control'
    );
    assert.strictEqual(
      statusBody.mtls_required_for_dashboard,
      true,
      'connect status did not report dashboard mTLS requirement'
    );

    const page = await browser.newPage();
    page.on('console', msg => console.log(`[browser:${msg.type()}] ${msg.text()}`));
    let response;
    try {
      response = await page.goto(`${origin}/connect/bootstrap`, {
        waitUntil: 'domcontentloaded',
        timeout: CONNECT_TIMEOUT_MS,
      });
    } catch (err) {
      if (browser.kind === 'cdp') {
        throw new Error(
          `CDP browser fallback could not load ${origin}/connect/bootstrap: ${err.message}. ` +
            'Install Playwright, set PLAYWRIGHT_NODE_PATH, or use a Chrome profile that trusts this daemon HTTPS origin.'
        );
      }
      throw err;
    }
    assert(response, '/connect/bootstrap produced no response');
    assert.strictEqual(response.status(), 200, `/connect/bootstrap returned ${response.status()}`);
    await page.waitForFunction(() => Boolean(window.intendantConnectDashboard));
    const connected = await waitForConnect(page);
    await page.evaluate(`window.__intendantFilesystemFixture = ${JSON.stringify({
      filePath: filesystemFixture.filePath,
      text: filesystemFixture.text,
    })}`);

    const result = await page.evaluate(async () => {
      const ctl = window.intendantConnectDashboard;
      const filesystemFixture = window.__intendantFilesystemFixture;
      const beforeChunks = ctl.status().completedChunkedResponses || 0;
      const largeSessions = await ctl.request('api_sessions', { limit: 'all' }, { timeoutMs: 60000 });
      const largeSessionsJson = JSON.stringify(largeSessions);
      const bytesToTextResult = raw => {
        if (raw?.bytes instanceof Uint8Array) {
          const { bytes, ...rest } = raw;
          return {
            ...rest,
            byteLength: bytes.byteLength,
            text: new TextDecoder().decode(bytes),
          };
        }
        return {
          ...(raw || {}),
          byteLength: raw?.data_base64 ? atob(String(raw.data_base64)).length : 0,
          text: raw?.data_base64 ? atob(String(raw.data_base64)) : '',
        };
      };
      const mediaTransfer = async () => {
        const annotation = await ctl.uploadBytes('api_media_annotation_submit', {
          frame_id: 'e2e-local-bootstrap-ann-1',
          stream: 'annotation',
          note: 'local connect media protocol e2e',
          inject: false,
        }, new TextEncoder().encode('jpeg annotation local bootstrap'), { timeoutMs: 60000 });
        const start = await ctl.request('api_media_clip_start', {
          clip_id: 'e2e-local-bootstrap-clip-1',
          stream: 'recording',
          fps: 2,
          total_frames: 1,
          inject: false,
        }, { timeoutMs: 60000 });
        const frame = await ctl.uploadBytes('api_media_clip_frame', {
          clip_id: 'e2e-local-bootstrap-clip-1',
          frame_id: 'e2e-local-bootstrap-clip-1-f000',
          frame_index: 0,
        }, new TextEncoder().encode('jpeg clip frame local bootstrap'), { timeoutMs: 60000 });
        const end = await ctl.request('api_media_clip_end', {
          clip_id: 'e2e-local-bootstrap-clip-1',
          frames_sent: 1,
        }, { timeoutMs: 60000 });
        return { annotation, start, frame, end };
      };
      const filesystemRead = async () => bytesToTextResult(await ctl.requestBytes('api_fs_read', {
        path: filesystemFixture.filePath,
        offset: 8,
        length: 10,
      }, { timeoutMs: 60000 }));
      return {
        status: await ctl.request('status'),
        config: await ctl.request('config'),
        sessions: await ctl.request('api_sessions', { limit: 2 }),
        filesystemRead: await filesystemRead(),
        mediaTransfer: await mediaTransfer(),
        largeSessions: {
          ok: Array.isArray(largeSessions),
          length: Array.isArray(largeSessions) ? largeSessions.length : null,
          jsonBytes: new TextEncoder().encode(largeSessionsJson).length,
          completedChunkedResponsesBefore: beforeChunks,
        },
        appError: await ctl.request('api_peer_eligible', { capabilities: [] }),
        finalStatus: ctl.status(),
      };
    });

    assert(result.status && result.status.session_id, 'status RPC did not return a session id');
    assert.strictEqual(result.status.byte_streams_available, true, 'status did not advertise byte streams');
    assert.strictEqual(result.status.upload_frames_available, true, 'status did not advertise upload frames');
    assert.strictEqual(result.status.api_fs_read_available, true, 'status did not advertise api_fs_read');
    assert.strictEqual(result.status.api_media_editor_available, true, 'status did not advertise media editor protocol');
    assert(result.config && typeof result.config === 'object', 'config RPC did not return an object');
    assert(Array.isArray(result.sessions), 'api_sessions did not return an array');
    assert.strictEqual(result.filesystemRead?.ok, true);
    assert.strictEqual(result.filesystemRead?.byteLength, 10);
    assert.strictEqual(result.filesystemRead?.text, 'bootstrap ');
    assert.strictEqual(result.filesystemRead?.range_start, 8);
    assert.strictEqual(result.filesystemRead?.range_end, 18);
    assert.strictEqual(result.filesystemRead?.total_size, filesystemFixture.text.length);
    assert.strictEqual(result.filesystemRead?.resumable, true);
    assert.strictEqual(result.mediaTransfer?.annotation?._httpStatus, 200);
    assert.strictEqual(result.mediaTransfer?.annotation?._httpOk, true);
    assert.strictEqual(result.mediaTransfer?.annotation?.t, 'annotation_saved');
    assert.strictEqual(result.mediaTransfer?.annotation?.frame_id, 'e2e-local-bootstrap-ann-1');
    assert.strictEqual(result.mediaTransfer?.start?._httpStatus, 200);
    assert.strictEqual(result.mediaTransfer?.start?.t, 'media_clip_started');
    assert.strictEqual(result.mediaTransfer?.frame?._httpStatus, 200);
    assert.strictEqual(result.mediaTransfer?.frame?.t, 'media_clip_frame_saved');
    assert.strictEqual(result.mediaTransfer?.frame?.frames_received, 1);
    assert.strictEqual(result.mediaTransfer?.end?._httpStatus, 200);
    assert.strictEqual(result.mediaTransfer?.end?.t, 'clip_saved');
    assert.strictEqual(result.mediaTransfer?.end?.frames_registered, 1);
    assert(
      result.appError && result.appError._httpStatus === 400,
      'application error metadata was not preserved'
    );
    assert(result.largeSessions.ok, 'large api_sessions did not return an array');
    assert(
      result.largeSessions.jsonBytes > 65536,
      `large api_sessions did not cross chunk threshold: ${result.largeSessions.jsonBytes}`
    );
    assert(
      result.finalStatus.completedChunkedResponses > result.largeSessions.completedChunkedResponsesBefore,
      'chunked response counter did not advance'
    );
    assert.strictEqual(
      result.finalStatus.pendingChunkedResponses,
      0,
      'chunked response map was not drained'
    );
    assert.strictEqual(result.finalStatus.pendingRequests, 0, 'request map was not drained');

    console.log(JSON.stringify({
      origin,
      certlessConfigStatus,
      connectStatus: statusBody,
      connected,
      rpc: {
        controlSessionId: result.status.session_id,
        sessionCount: result.sessions.length,
        filesystemReadBytes: result.filesystemRead.byteLength,
        largeSessionCount: result.largeSessions.length,
        largeSessionBytes: result.largeSessions.jsonBytes,
        completedByteStreams: result.finalStatus.completedByteStreams,
        completedChunkedResponses: result.finalStatus.completedChunkedResponses,
        appErrorStatus: result.appError._httpStatus,
        pendingRequests: result.finalStatus.pendingRequests,
        pendingChunkedResponses: result.finalStatus.pendingChunkedResponses,
        pendingByteStreams: result.finalStatus.pendingByteStreams,
      },
    }, null, 2));

    await page.evaluate(() => window.intendantConnectDashboard.close());
  } finally {
    fs.rmSync(filesystemFixture.dir, { recursive: true, force: true });
    await browser.close();
  }
}

main().catch(err => {
  console.error(err);
  process.exit(1);
});
