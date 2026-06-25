#!/usr/bin/env node
'use strict';

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn } = require('child_process');
const { httpStatus, launchBrowser } = require('./lib/browser-automation.cjs');

const DEFAULT_DAEMON_PORT = 8877;
const START_TIMEOUT_MS = 30000;
const CONNECT_TIMEOUT_MS = 30000;
const FRAME_FIXTURE_PNG_BASE64 = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII=';

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

function createRecordingFixture(label) {
  const streamName = `dashboard_control_${label}_${process.pid}_${Date.now()}`;
  const dir = path.join(os.homedir(), '.intendant', 'recordings', streamName);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'segments.csv'), 'seg_00000.mp4,0,1.25\n');
  fs.writeFileSync(path.join(dir, 'seg_00000.mp4'), 'recording segment e2e local');
  return { streamName, dir };
}

function createHlsRecordingFixture(label) {
  const streamName = `dashboard_control_hls_${label}_${process.pid}_${Date.now()}`;
  const dir = path.join(os.homedir(), '.intendant', 'recordings', streamName);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'segments.csv'), 'seg_00000.ts,0,1.25\n');
  fs.writeFileSync(path.join(dir, 'seg_00000.ts'), 'recording hls transport stream e2e local');
  return { streamName, dir };
}

function removeRecordingFixture(fixture) {
  if (!fixture?.dir) return;
  fs.rmSync(fixture.dir, { recursive: true, force: true });
}

function createSessionFrameFixture(label) {
  const sessionId = `dashboard-control-frame-${label}-${process.pid}-${Date.now()}`;
  const filename = 'ann-dashboard-frame.png';
  const dir = path.join(os.homedir(), '.intendant', 'logs', sessionId);
  const framesDir = path.join(dir, 'frames');
  fs.mkdirSync(framesDir, { recursive: true });
  fs.writeFileSync(path.join(framesDir, filename), Buffer.from(FRAME_FIXTURE_PNG_BASE64, 'base64'));
  return { sessionId, filename, dir };
}

function removeSessionFrameFixture(fixture) {
  if (!fixture?.dir) return;
  fs.rmSync(fixture.dir, { recursive: true, force: true });
}

function createFilesystemFixture(label) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `intendant-dashboard-control-fs-${label}-`));
  const filePath = path.join(dir, 'filesystem-read.txt');
  const text = `dashboard filesystem read e2e ${label}`;
  fs.writeFileSync(filePath, text);
  return { dir, filePath, text };
}

function removeFilesystemFixture(fixture) {
  if (!fixture?.dir) return;
  fs.rmSync(fixture.dir, { recursive: true, force: true });
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
  const recordingFixture = createRecordingFixture('local');
  const hlsRecordingFixture = createHlsRecordingFixture('local');
  const sessionFrameFixture = createSessionFrameFixture('local');
  const filesystemFixture = createFilesystemFixture('local');
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
    await page.evaluate(`window.__intendantRecordingStreamName = ${JSON.stringify(recordingFixture.streamName)}`);
    await page.evaluate(`window.__intendantHlsRecordingStreamName = ${JSON.stringify(hlsRecordingFixture.streamName)}`);
    await page.evaluate(`window.__intendantSessionFrameFixture = ${JSON.stringify({
      sessionId: sessionFrameFixture.sessionId,
      filename: sessionFrameFixture.filename,
    })}`);
    await page.evaluate(`window.__intendantFilesystemFixture = ${JSON.stringify({
      filePath: filesystemFixture.filePath,
      text: filesystemFixture.text,
    })}`);
    const result = await page.evaluate(async () => {
      const ctl = window.intendantDashboardControl;
      const recordingStreamName = window.__intendantRecordingStreamName;
      const hlsRecordingStreamName = window.__intendantHlsRecordingStreamName;
      const sessionFrameFixture = window.__intendantSessionFrameFixture;
      const filesystemFixture = window.__intendantFilesystemFixture;
      const labeled = async (label, promise) => {
        try {
          return await promise;
        } catch (err) {
          throw new Error(`${label}: ${err?.message || err}`);
        }
      };
      const sessionReport = async () => {
        const report = await labeled('api_session_report current', (
          ctl.requestBytes ? ctl.requestBytes('api_session_report', {
            session_id: 'current',
          }, { timeoutMs: 120000 }) : ctl.request('api_session_report', {
            session_id: 'current',
          }, { timeoutMs: 120000 })
        ));
        if (report?.bytes instanceof Uint8Array) {
          const { bytes, ...rest } = report;
          return { ...rest, byteLength: bytes.byteLength };
        }
        return {
          ...(report || {}),
          byteLength: report?.data_base64 ? atob(String(report.data_base64)).length : 0,
        };
      };
      const upload = async () => {
        const bytes = new TextEncoder().encode('dashboard upload e2e local');
        return labeled('api_session_current_upload', ctl.uploadBytes('api_session_current_upload', {
          destination: 'task',
          name: 'dashboard-upload-local.txt',
          mime: 'text/plain',
        }, bytes, { timeoutMs: 60000 }));
      };
      const uploadRaw = async uploadResult => {
        const raw = await labeled('api_session_current_upload_raw', ctl.requestBytes('api_session_current_upload_raw', {
          id: uploadResult.id,
          offset: 10,
          length: 6,
        }, { timeoutMs: 60000 }));
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
      const imagePreview = async () => {
        const input = document.getElementById('upload-file-input');
        if (!input) throw new Error('upload file input is not available on the dashboard page');
        const png = Uint8Array.from(
          atob('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII='),
          ch => ch.charCodeAt(0)
        );
        const before = ctl.status().completedByteStreams || 0;
        const file = new File([png], 'dashboard-preview-local.png', { type: 'image/png' });
        const transfer = new DataTransfer();
        transfer.items.add(file);
        input.files = transfer.files;
        input.dispatchEvent(new Event('change', { bubbles: true }));
        const deadline = performance.now() + 60000;
        let img = null;
        while (performance.now() < deadline) {
          img = document.querySelector('.pending-attachment-chip img.chip-thumb');
          if (img && String(img.src || '').startsWith('blob:')) break;
          await new Promise(resolve => setTimeout(resolve, 50));
        }
        const after = ctl.status().completedByteStreams || 0;
        const previewUrl = String(img?.src || '');
        const result = {
          ok: previewUrl.startsWith('blob:'),
          previewScheme: previewUrl.split(':', 1)[0],
          byteStreamDelta: after - before,
        };
        document.querySelector('.pending-attachment-chip .chip-remove')?.click();
        return result;
      };
      const recordingAsset = async () => {
        const raw = await labeled('api_recording_asset segment', ctl.requestBytes('api_recording_asset', {
          stream_name: recordingStreamName,
          asset: 'seg_00000.mp4',
          offset: 10,
          length: 7,
        }, { timeoutMs: 60000 }));
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
      const sessionFrameAsset = async () => {
        const raw = await labeled('api_session_frame_asset image', ctl.requestBytes('api_session_frame_asset', {
          session_id: sessionFrameFixture.sessionId,
          filename: sessionFrameFixture.filename,
          offset: 0,
          length: 8,
        }, { timeoutMs: 60000 }));
        if (raw?.bytes instanceof Uint8Array) {
          const { bytes, ...rest } = raw;
          return {
            ...rest,
            byteLength: bytes.byteLength,
            firstBytes: Array.from(bytes),
          };
        }
        const text = raw?.data_base64 ? atob(String(raw.data_base64)) : '';
        return {
          ...(raw || {}),
          byteLength: text.length,
          firstBytes: Array.from(text, ch => ch.charCodeAt(0)),
        };
      };
      const sessionFramePreview = async () => {
        if (typeof window.intendantSessionFrameRenderer?.render !== 'function') {
          throw new Error('renderSessionFrames is not available on the dashboard page');
        }
        const before = ctl.status().completedByteStreams || 0;
        window.intendantSessionFrameRenderer.render(
          sessionFrameFixture.sessionId,
          [sessionFrameFixture.filename]
        );
        const deadline = performance.now() + 60000;
        let img = null;
        while (performance.now() < deadline) {
          img = document.querySelector('#session-detail-frames .sd-frame-thumb img');
          if (img && String(img.src || '').startsWith('blob:')) break;
          await new Promise(resolve => setTimeout(resolve, 50));
        }
        const after = ctl.status().completedByteStreams || 0;
        const previewUrl = String(img?.src || '');
        window.intendantSessionFrameRenderer?.revokeObjectUrls?.();
        const framesEl = document.getElementById('session-detail-frames');
        if (framesEl) framesEl.innerHTML = '';
        return {
          ok: previewUrl.startsWith('blob:'),
          previewScheme: previewUrl.split(':', 1)[0],
          byteStreamDelta: after - before,
        };
      };
      const recordingFallbackPlayback = async () => {
        if (typeof RecordingPlayer !== 'function') {
          throw new Error('RecordingPlayer is not available on the dashboard page');
        }
        const wrap = document.createElement('div');
        wrap.style.display = 'none';
        const video = document.createElement('video');
        const timeline = document.createElement('div');
        const cursor = document.createElement('div');
        const progress = document.createElement('div');
        const timeLabel = document.createElement('span');
        const playBtn = document.createElement('button');
        wrap.append(video, timeline, cursor, progress, timeLabel, playBtn);
        document.body.appendChild(wrap);
        const player = new RecordingPlayer(video, timeline, cursor, progress, timeLabel, playBtn, '/recordings');
        try {
          player.streamName = recordingStreamName;
          player.segments = [{ filename: 'seg_00000.mp4', start_secs: 0, end_secs: 1 }];
          player.totalDuration = 1;
          const before = ctl.status().completedByteStreams || 0;
          await player._loadSegment(0, 0);
          const after = ctl.status().completedByteStreams || 0;
          return {
            srcScheme: String(video.src || '').split(':', 1)[0],
            objectUrl: Boolean(player._segmentObjectUrl),
            byteStreamDelta: after - before,
          };
        } finally {
          player.destroy();
          wrap.remove();
        }
      };
      const recordingHlsBlobPlaylist = async () => {
        if (typeof RecordingPlayer !== 'function') {
          throw new Error('RecordingPlayer is not available on the dashboard page');
        }
        const wrap = document.createElement('div');
        wrap.style.display = 'none';
        const video = document.createElement('video');
        const timeline = document.createElement('div');
        const cursor = document.createElement('div');
        const progress = document.createElement('div');
        const timeLabel = document.createElement('span');
        const playBtn = document.createElement('button');
        wrap.append(video, timeline, cursor, progress, timeLabel, playBtn);
        document.body.appendChild(wrap);
        const player = new RecordingPlayer(video, timeline, cursor, progress, timeLabel, playBtn, '/recordings');
        try {
          if (typeof player._loadHlsBlobPlaylist !== 'function') {
            throw new Error('RecordingPlayer HLS blob loader is not available');
          }
          player.streamName = hlsRecordingStreamName;
          const before = ctl.status().completedByteStreams || 0;
          const ok = await labeled('recording HLS blob playlist', player._loadHlsBlobPlaylist(`/recordings/${hlsRecordingStreamName}/playlist.m3u8`));
          const after = ctl.status().completedByteStreams || 0;
          return {
            ok,
            srcScheme: String(video.src || '').split(':', 1)[0],
            objectUrlCount: Array.isArray(player._hlsObjectUrls) ? player._hlsObjectUrls.length : 0,
            byteStreamDelta: after - before,
          };
        } finally {
          player.destroy();
          wrap.remove();
        }
      };
      const filesystemRead = async () => {
        const raw = await labeled('api_fs_read fixture', ctl.requestBytes('api_fs_read', {
          path: filesystemFixture.filePath,
          offset: 10,
          length: 10,
        }, { timeoutMs: 60000 }));
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
      const diagnosticsVisualFreshness = async () => {
        const sessionId = `validator-local-vf-${Date.now()}-${Math.random().toString(36).slice(2)}`;
        const body = '{"t":"session_start"}\n{"t":"summary","transitions":1}\n';
        const response = await labeled('api_diagnostics_visual_freshness', ctl.request('api_diagnostics_visual_freshness', {
          session_id: sessionId,
          body,
        }, { timeoutMs: 60000 }));
        return {
          ...response,
          sessionId,
          bodyLength: body.length,
        };
      };
      const peerPairing = async () => ({
        requests: await labeled('api_peer_pairing_requests', ctl.request('api_peer_pairing_requests', {}, { timeoutMs: 60000 })),
        identities: await labeled('api_peer_pairing_identities', ctl.request('api_peer_pairing_identities', {}, { timeoutMs: 60000 })),
        missingDecision: await labeled('api_peer_pairing_request_decision missing request', ctl.request('api_peer_pairing_request_decision', {
          request_id: `missing-request-${Date.now()}`,
          op: 'approve',
        }, { timeoutMs: 60000 })),
        missingRevokeIdentity: await labeled('api_peer_pairing_identity_revoke missing identity', ctl.request('api_peer_pairing_identity_revoke', {}, { timeoutMs: 60000 })),
      });
      const terminal = async () => {
        const terminalId = `dashboard-terminal-local-${Date.now()}`;
        const token = 'dashboard_terminal_e2e_local';
        const frames = [];
        const handler = event => frames.push(event.detail || {});
        const waitFor = (predicate, label) => new Promise((resolve, reject) => {
          const started = Date.now();
          const tick = () => {
            const found = frames.find(predicate);
            if (found) {
              resolve(found);
              return;
            }
            if (Date.now() - started > 60000) {
              reject(new Error(`terminal ${label} timed out`));
              return;
            }
            setTimeout(tick, 25);
          };
          tick();
        });
        window.addEventListener('intendant-dashboard-terminal-frame', handler);
        try {
          ctl.terminalFrame({
            t: 'terminal_open',
            host_id: 'local',
            terminal_id: terminalId,
            cols: 80,
            rows: 24,
          });
          await waitFor(frame => frame.t === 'terminal_opened' && frame.terminal_id === terminalId, 'open');
          ctl.terminalFrame({
            t: 'terminal_input',
            host_id: 'local',
            terminal_id: terminalId,
            data: btoa(`printf '${token}\\n'\\r`),
          });
          const output = await waitFor(frame => {
            if (frame.t !== 'terminal_output' || frame.terminal_id !== terminalId) return false;
            return atob(String(frame.data || '')).includes(token);
          }, 'output');
          ctl.terminalFrame({
            t: 'terminal_close',
            host_id: 'local',
            terminal_id: terminalId,
          });
          return {
            opened: true,
            sawToken: true,
            terminalId,
            outputBytes: atob(String(output.data || '')).length,
          };
        } finally {
          window.removeEventListener('intendant-dashboard-terminal-frame', handler);
        }
      };
      const tui = async () => {
        const connectionId = `dashboard-tui-local-${Date.now()}`;
        const frames = [];
        const handler = event => frames.push(event.detail || {});
        const waitFor = (predicate, label) => new Promise((resolve, reject) => {
          const started = Date.now();
          const tick = () => {
            const found = frames.find(predicate);
            if (found) {
              resolve(found);
              return;
            }
            if (Date.now() - started > 60000) {
              reject(new Error(`tui ${label} timed out`));
              return;
            }
            setTimeout(tick, 25);
          };
          tick();
        });
        window.addEventListener('intendant-dashboard-tui-frame', handler);
        try {
          ctl.tuiFrame({
            t: 'tui_subscribe',
            connection_id: connectionId,
            cols: 80,
            rows: 24,
          });
          const frame = await waitFor(item => (
            item.t === 'tui_term' &&
            item.connection_id === connectionId &&
            Boolean(item.base64 || item.d)
          ), 'term frame');
          ctl.tuiFrame({
            t: 'tui_key',
            connection_id: connectionId,
            key: 'Tab',
            ctrl: false,
            alt: false,
            shift: false,
          });
          ctl.tuiFrame({
            t: 'tui_unsubscribe',
            connection_id: connectionId,
          });
          ctl.tuiFrame({
            t: 'tui_close',
            connection_id: connectionId,
          });
          const data = String(frame.base64 || frame.d || '');
          return {
            subscribed: true,
            connectionId,
            frameBytes: atob(data).length,
          };
        } finally {
          window.removeEventListener('intendant-dashboard-tui-frame', handler);
        }
      };
      const status = await ctl.request('status', {}, { timeoutMs: 60000 });
      const uploaded = await upload();
      return {
        status,
        agentCard: await ctl.agentCard({ timeoutMs: 60000 }),
        cachedBootstrapEvents: await ctl.cachedBootstrapEvents({ timeoutMs: 60000 }),
        browserWorkspaceSnapshot: await ctl.browserWorkspaceSnapshot({ timeoutMs: 60000 }),
        stateSnapshot: await ctl.stateSnapshot({ timeoutMs: 60000 }),
        displayBootstrap: await ctl.displayBootstrap({ timeoutMs: 60000 }),
        displayAuthoritySnapshot: await ctl.displayAuthoritySnapshot({ timeoutMs: 60000 }),
        sessionLogReplay: await ctl.sessionLogReplay({ timeoutMs: 60000 }),
        externalSessionActivityReplay: await ctl.externalSessionActivityReplay({ timeoutMs: 60000 }),
        dashboardBootstrap: await ctl.dashboardBootstrap({ timeoutMs: 60000 }),
        sessions: await ctl.request('api_sessions', { limit: 2 }, { timeoutMs: 60000 }),
        sessionReport: await sessionReport(),
        upload: uploaded,
        uploads: await ctl.request('api_session_current_uploads', {}, { timeoutMs: 60000 }),
        uploadRaw: await uploadRaw(uploaded),
        imagePreview: await imagePreview(),
        recordingAsset: await recordingAsset(),
        sessionFrameAsset: await sessionFrameAsset(),
        sessionFramePreview: await sessionFramePreview(),
        recordingFallbackPlayback: await recordingFallbackPlayback(),
        recordingHlsBlobPlaylist: await recordingHlsBlobPlaylist(),
        filesystemRead: await filesystemRead(),
        diagnosticsVisualFreshness: await diagnosticsVisualFreshness(),
        terminal: await terminal(),
        tui: status.tui_frames_available ? await tui() : { skipped: true, subscribed: false, frameBytes: 0 },
        rejectedControlMsg: await labeled('api_control_msg rejected create_session', ctl.request('api_control_msg', {
          message: { action: 'create_session', task: 'noop' },
        }, { timeoutMs: 60000 })),
        sessionControlMsg: await labeled('api_session_control_msg interrupt', ctl.request('api_session_control_msg', {
          message: { action: 'interrupt' },
        }, { timeoutMs: 60000 })),
        rejectedSessionControlMsg: await labeled('api_session_control_msg rejected set_codex_sandbox', ctl.request('api_session_control_msg', {
          message: { action: 'set_codex_sandbox', mode: 'workspace-write' },
        }, { timeoutMs: 60000 })),
        dashboardActionMsg: await labeled('api_dashboard_action_msg close_browser_workspace', ctl.request('api_dashboard_action_msg', {
          message: { action: 'close_browser_workspace', workspace_id: `validator-workspace-${Date.now()}` },
        }, { timeoutMs: 60000 })),
        diagnosticsMarkerActionMsg: await labeled('api_dashboard_action_msg diagnostics visual marker', ctl.request('api_dashboard_action_msg', {
          message: { action: 'set_diagnostics_visual_marker', display_id: 0, enabled: false },
        }, { timeoutMs: 60000 })),
        peerPairing: await peerPairing(),
        peerWebRtcSignal: await labeled('api_peer_webrtc_signal missing peer', ctl.request('api_peer_webrtc_signal', {
          peer_id: 'missing-peer',
          display_id: 0,
          session_id: `validator-peer-display-${Date.now()}`,
          signal: { kind: 'close' },
        }, { timeoutMs: 60000 })),
        rejectedDashboardActionMsg: await labeled('api_dashboard_action_msg rejected set_codex_sandbox', ctl.request('api_dashboard_action_msg', {
          message: { action: 'set_codex_sandbox', mode: 'workspace-write' },
        }, { timeoutMs: 60000 })),
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
    assert.strictEqual(result.stateSnapshot?.t, 'state_snapshot', 'state snapshot RPC did not return the event shape');
    assert.strictEqual(
      result.stateSnapshot.connection_id,
      result.status.session_id,
      'state snapshot connection id did not match control session id'
    );
    assert(result.stateSnapshot.state && typeof result.stateSnapshot.state === 'object', 'state snapshot did not return state');
    assert(Array.isArray(result.displayBootstrap?.frames), 'display bootstrap did not return frames');
    assert.strictEqual(
      result.displayBootstrap.frame_count,
      result.displayBootstrap.frames.length,
      'display bootstrap frame count did not match'
    );
    assert(
      !result.displayBootstrap.omitted?.includes('display_input_authority_state'),
      'display bootstrap still marked authority state as omitted'
    );
    assert.strictEqual(
      result.status.api_display_input_authority_available,
      true,
      'dashboard control status did not advertise display input authority'
    );
    assert.strictEqual(
      result.displayAuthoritySnapshot?.available,
      true,
      'display authority snapshot did not report availability'
    );
    assert(Array.isArray(result.displayAuthoritySnapshot?.frames), 'display authority snapshot did not return frames');
    assert.strictEqual(
      result.displayAuthoritySnapshot.frame_count,
      result.displayAuthoritySnapshot.frames.length,
      'display authority snapshot frame count did not match'
    );
    assert.strictEqual(result.sessionLogReplay?.t, 'log_replay', 'session log replay RPC did not return the event shape');
    assert(Array.isArray(result.sessionLogReplay.entries), 'session log replay did not return entries');
    assert(Array.isArray(result.externalSessionActivityReplay?.frames), 'external session activity replay did not return frames');
    assert.strictEqual(
      result.externalSessionActivityReplay.frame_count,
      result.externalSessionActivityReplay.frames.length,
      'external session activity replay frame count did not match'
    );
    assert(Array.isArray(result.dashboardBootstrap?.frames), 'dashboard bootstrap did not return frames');
    assert.strictEqual(
      result.dashboardBootstrap.frame_count,
      result.dashboardBootstrap.frames.length,
      'dashboard bootstrap frame count did not match'
    );
    assert.strictEqual(result.dashboardBootstrap.frames[0]?.t, 'state_snapshot', 'dashboard bootstrap did not start with state snapshot');
    assert(
      !result.dashboardBootstrap.omitted?.includes('display_ready'),
      'dashboard bootstrap still marked display_ready as omitted'
    );
    assert(
      !result.dashboardBootstrap.omitted?.includes('display_input_authority_state'),
      'dashboard bootstrap still marked authority state as omitted'
    );
    assert(
      !result.dashboardBootstrap.omitted?.includes('external_session_activity_replay'),
      'dashboard bootstrap still marked external session activity replay as omitted'
    );
    assert(Array.isArray(result.sessions), 'api_sessions did not return an array');
    assert.strictEqual(result.finalStatus.signalingMode, 'local-http');
    assert.strictEqual(result.finalStatus.apiAgentCardAvailable, true);
    assert.strictEqual(result.finalStatus.apiCachedBootstrapEventsAvailable, true);
    assert.strictEqual(result.finalStatus.apiBrowserWorkspaceSnapshotAvailable, true);
    assert.strictEqual(result.finalStatus.apiStateSnapshotAvailable, true);
    assert.strictEqual(result.finalStatus.apiDisplayBootstrapAvailable, true);
    assert.strictEqual(result.finalStatus.apiDisplayInputAuthorityAvailable, true);
    assert.strictEqual(result.finalStatus.apiSessionLogReplayAvailable, true);
    assert.strictEqual(result.finalStatus.apiExternalSessionActivityReplayAvailable, true);
    assert.strictEqual(result.finalStatus.apiDashboardBootstrapAvailable, true);
    assert.strictEqual(result.finalStatus.apiControlMsgAvailable, true);
    assert.strictEqual(result.finalStatus.apiSessionControlMsgAvailable, true);
    assert.strictEqual(result.finalStatus.apiDashboardActionMsgAvailable, true);
    assert.strictEqual(result.finalStatus.apiDiagnosticsVisualFreshnessAvailable, true);
    assert.strictEqual(result.finalStatus.apiPeerMutationsAvailable, true);
    assert.strictEqual(result.finalStatus.apiPeerPairingAvailable, true);
    assert.strictEqual(result.finalStatus.apiPeerWebRtcSignalAvailable, true);
    assert.strictEqual(result.finalStatus.apiCoordinatorAvailable, result.status.api_coordinator_available);
    assert.strictEqual(result.finalStatus.apiSessionDetailAvailable, true);
    assert.strictEqual(result.finalStatus.apiSessionReportAvailable, true);
    assert.strictEqual(result.finalStatus.apiSessionDeleteAvailable, result.status.api_session_delete_available);
    assert.strictEqual(result.finalStatus.apiSessionCurrentAgentOutputAvailable, result.status.api_session_current_agent_output_available);
    assert.strictEqual(result.finalStatus.apiSessionCurrentHistoryAvailable, result.status.api_session_current_history_available);
    assert.strictEqual(result.finalStatus.apiSessionCurrentRollbackAvailable, result.status.api_session_current_rollback_available);
    assert.strictEqual(result.finalStatus.apiSessionCurrentRedoAvailable, result.status.api_session_current_redo_available);
    assert.strictEqual(result.finalStatus.apiSessionCurrentPruneAvailable, result.status.api_session_current_prune_available);
    assert.strictEqual(result.finalStatus.apiSessionCurrentChangesAvailable, result.status.api_session_current_changes_available);
    assert.strictEqual(result.finalStatus.apiSessionContextSnapshotAvailable, result.status.api_session_context_snapshot_available);
    assert.strictEqual(result.finalStatus.byteStreamsAvailable, true);
    assert.strictEqual(result.finalStatus.uploadFramesAvailable, true);
    assert.strictEqual(result.finalStatus.terminalFramesAvailable, true);
    assert.strictEqual(result.finalStatus.tuiFramesAvailable, result.status.tui_frames_available === true);
    assert.strictEqual(result.finalStatus.apiSessionCurrentUploadsAvailable, true);
    assert.strictEqual(result.finalStatus.apiSessionCurrentUploadAvailable, true);
    assert.strictEqual(result.finalStatus.apiSessionCurrentUploadRawAvailable, true);
    assert.strictEqual(result.finalStatus.apiSessionCurrentUploadDeleteAvailable, result.status.api_session_current_upload_delete_available);
    assert.strictEqual(result.finalStatus.apiRecordingAssetAvailable, true);
    assert.strictEqual(result.finalStatus.apiSessionFrameAssetAvailable, true);
    assert.strictEqual(result.finalStatus.apiFsStatAvailable, result.status.api_fs_stat_available);
    assert.strictEqual(result.finalStatus.apiFsListAvailable, result.status.api_fs_list_available);
    assert.strictEqual(result.finalStatus.apiFsMkdirAvailable, result.status.api_fs_mkdir_available);
    assert.strictEqual(result.finalStatus.apiFsReadAvailable, true);
    assert.strictEqual(result.finalStatus.apiSettingsAvailable, result.status.api_settings_available);
    assert.strictEqual(result.finalStatus.apiSettingsSaveAvailable, result.status.api_settings_save_available);
    assert.strictEqual(result.finalStatus.apiKeyStatusAvailable, result.status.api_key_status_available);
    assert.strictEqual(result.finalStatus.apiApiKeysSaveAvailable, result.status.api_api_keys_save_available);
    assert.strictEqual(result.finalStatus.apiVoiceSessionAvailable, result.status.api_voice_session_available);
    assert.strictEqual(result.finalStatus.apiProjectRootAvailable, result.status.api_project_root_available);
    assert.strictEqual(result.finalStatus.apiDisplaysAvailable, result.status.api_displays_available);
    assert.strictEqual(result.upload?._httpStatus, 200);
    assert.strictEqual(result.upload?._httpOk, true);
    assert.strictEqual(result.upload?.name, 'dashboard-upload-local.txt');
    assert.strictEqual(result.upload?.mime, 'text/plain');
    assert.strictEqual(result.upload?.size, 'dashboard upload e2e local'.length);
    assert(Array.isArray(result.uploads), 'api_session_current_uploads did not return an array');
    assert(
      result.uploads.some(upload => upload.id === result.upload.id),
      `api_session_current_uploads did not include the uploaded descriptor: ${JSON.stringify(result.uploads)}`
    );
    assert.strictEqual(result.uploadRaw?.ok, true);
    assert.strictEqual(result.uploadRaw?.byteLength, 6);
    assert.strictEqual(result.uploadRaw?.text, 'upload');
    assert.strictEqual(result.uploadRaw?.total_size, 'dashboard upload e2e local'.length);
    assert.strictEqual(result.uploadRaw?.range_start, 10);
    assert.strictEqual(result.uploadRaw?.range_end, 16);
    assert.strictEqual(result.uploadRaw?.resumable, true);
    assert.strictEqual(result.imagePreview?.ok, true);
    assert.strictEqual(result.imagePreview?.previewScheme, 'blob');
    assert(result.imagePreview?.byteStreamDelta >= 1, `image preview did not use a byte stream: ${JSON.stringify(result.imagePreview)}`);
    assert.strictEqual(result.recordingAsset?.ok, true);
    assert.strictEqual(result.recordingAsset?.byteLength, 7);
    assert.strictEqual(result.recordingAsset?.text, 'segment');
    assert.strictEqual(result.recordingAsset?.content_type, 'video/mp4');
    assert.strictEqual(result.recordingAsset?.range_start, 10);
    assert.strictEqual(result.recordingAsset?.range_end, 17);
    assert.strictEqual(result.recordingAsset?.resumable, true);
    assert.strictEqual(result.sessionFrameAsset?.ok, true);
    assert.strictEqual(result.sessionFrameAsset?.content_type, 'image/png');
    assert.strictEqual(result.sessionFrameAsset?.filename, sessionFrameFixture.filename);
    assert.strictEqual(result.sessionFrameAsset?.session_id, sessionFrameFixture.sessionId);
    assert.strictEqual(result.sessionFrameAsset?.byteLength, 8);
    assert.deepStrictEqual(result.sessionFrameAsset?.firstBytes, [137, 80, 78, 71, 13, 10, 26, 10]);
    assert.strictEqual(result.sessionFrameAsset?.range_start, 0);
    assert.strictEqual(result.sessionFrameAsset?.range_end, 8);
    assert.strictEqual(result.sessionFrameAsset?.resumable, true);
    assert.strictEqual(result.sessionFramePreview?.ok, true);
    assert.strictEqual(result.sessionFramePreview?.previewScheme, 'blob');
    assert(result.sessionFramePreview?.byteStreamDelta >= 1, `session frame preview did not use a byte stream: ${JSON.stringify(result.sessionFramePreview)}`);
    assert.strictEqual(result.recordingFallbackPlayback?.srcScheme, 'blob');
    assert.strictEqual(result.recordingFallbackPlayback?.objectUrl, true);
    assert(result.recordingFallbackPlayback?.byteStreamDelta >= 1, `recording fallback playback did not use a byte stream: ${JSON.stringify(result.recordingFallbackPlayback)}`);
    assert.strictEqual(result.recordingHlsBlobPlaylist?.ok, true);
    assert.strictEqual(result.recordingHlsBlobPlaylist?.srcScheme, 'blob');
    assert(result.recordingHlsBlobPlaylist?.objectUrlCount >= 2, `HLS blob playlist did not create playlist and segment URLs: ${JSON.stringify(result.recordingHlsBlobPlaylist)}`);
    assert(result.recordingHlsBlobPlaylist?.byteStreamDelta >= 2, `HLS blob playlist did not use byte streams: ${JSON.stringify(result.recordingHlsBlobPlaylist)}`);
    assert.strictEqual(result.filesystemRead?.ok, true);
    assert.strictEqual(result.filesystemRead?.byteLength, 10);
    assert.strictEqual(result.filesystemRead?.text, 'filesystem');
    assert.strictEqual(result.filesystemRead?.content_type, 'text/plain; charset=utf-8');
    assert.strictEqual(result.filesystemRead?.range_start, 10);
    assert.strictEqual(result.filesystemRead?.range_end, 20);
    assert.strictEqual(result.filesystemRead?.total_size, filesystemFixture.text.length);
    assert.strictEqual(result.filesystemRead?.resumable, true);
    assert.strictEqual(result.status.api_diagnostics_visual_freshness_available, true);
    assert.strictEqual(result.diagnosticsVisualFreshness?.ok, true);
    assert.strictEqual(result.diagnosticsVisualFreshness?._httpStatus, 200);
    assert.strictEqual(result.diagnosticsVisualFreshness?.written, result.diagnosticsVisualFreshness?.bodyLength);
    fs.rmSync(path.join(
      os.homedir(),
      '.intendant',
      'diagnostics',
      'visual-freshness',
      `${result.diagnosticsVisualFreshness.sessionId}.ndjson`
    ), { force: true });
    assert.strictEqual(result.terminal?.opened, true);
    assert.strictEqual(result.terminal?.sawToken, true);
    if (result.status.tui_frames_available) {
      assert.strictEqual(result.tui?.subscribed, true);
      assert(Number(result.tui?.frameBytes || 0) > 0, 'TUI frame did not contain bytes');
    } else {
      assert.strictEqual(result.tui?.skipped, true);
    }
    if (result.sessionReport?.ok === true) {
      assert.strictEqual(result.sessionReport.content_type, 'application/zip');
      assert(String(result.sessionReport.filename || '').endsWith('.zip'), 'session report filename was not a zip');
      assert(Number(result.sessionReport.size || 0) > 0, 'session report had no bytes');
      assert(Number(result.sessionReport.byteLength || 0) > 0, 'session report had no byte-stream body');
      assert.strictEqual(result.sessionReport.byteLength, result.sessionReport.size);
    } else {
      assert.strictEqual(result.sessionReport?._httpStatus, 404);
      assert.strictEqual(result.sessionReport?._httpOk, false);
    }
    assert.strictEqual(result.rejectedControlMsg?._httpStatus, 400);
    assert.strictEqual(result.rejectedControlMsg?._httpOk, false);
    assert(
      String(result.rejectedControlMsg?.error || '').includes('not available over dashboard WebRTC'),
      `unexpected control-message rejection: ${JSON.stringify(result.rejectedControlMsg)}`
    );
    assert.strictEqual(result.sessionControlMsg?.ok, true);
    assert.strictEqual(result.sessionControlMsg?.action, 'interrupt');
    assert.strictEqual(result.rejectedSessionControlMsg?._httpStatus, 400);
    assert.strictEqual(result.rejectedSessionControlMsg?._httpOk, false);
    assert(
      String(result.rejectedSessionControlMsg?.error || '').includes('not available over dashboard session WebRTC'),
      `unexpected session-control rejection: ${JSON.stringify(result.rejectedSessionControlMsg)}`
    );
    assert.strictEqual(result.dashboardActionMsg?.ok, true);
    assert.strictEqual(result.dashboardActionMsg?.action, 'close_browser_workspace');
    assert.strictEqual(result.diagnosticsMarkerActionMsg?.ok, true);
    assert.strictEqual(result.diagnosticsMarkerActionMsg?.action, 'set_diagnostics_visual_marker');
    assert.strictEqual(result.diagnosticsMarkerActionMsg?.display_id, 0);
    assert.strictEqual(typeof result.diagnosticsMarkerActionMsg?.registry_available, 'boolean');
    assert.strictEqual(typeof result.diagnosticsMarkerActionMsg?.active_display_updated, 'boolean');
    assert(Array.isArray(result.peerPairing?.requests?.requests), 'peer pairing requests RPC did not return an array');
    assert(Array.isArray(result.peerPairing?.identities?.identities), 'peer pairing identities RPC did not return an array');
    assert.strictEqual(result.peerPairing?.missingDecision?._httpStatus, 400);
    assert.strictEqual(result.peerPairing?.missingDecision?._httpOk, false);
    assert.strictEqual(result.peerPairing?.missingRevokeIdentity?._httpStatus, 400);
    assert.strictEqual(result.peerPairing?.missingRevokeIdentity?._httpOk, false);
    assert.strictEqual(result.peerWebRtcSignal?._httpStatus, 404);
    assert.strictEqual(result.peerWebRtcSignal?._httpOk, false);
    assert.strictEqual(result.peerWebRtcSignal?.error, 'peer not found');
    assert.strictEqual(result.rejectedDashboardActionMsg?._httpStatus, 400);
    assert.strictEqual(result.rejectedDashboardActionMsg?._httpOk, false);
    assert(
      String(result.rejectedDashboardActionMsg?.error || '').includes('not available over dashboard action WebRTC'),
      `unexpected dashboard-action rejection: ${JSON.stringify(result.rejectedDashboardActionMsg)}`
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
        stateSnapshotConnectionId: result.stateSnapshot.connection_id,
        displayBootstrapFrameCount: result.displayBootstrap.frame_count,
        displayAuthoritySnapshotFrameCount: result.displayAuthoritySnapshot.frame_count,
        sessionLogReplayEntryCount: result.sessionLogReplay.entries.length,
        externalSessionActivityReplayFrameCount: result.externalSessionActivityReplay.frame_count,
        dashboardBootstrapFrameCount: result.dashboardBootstrap.frame_count,
        sessionCount: result.sessions.length,
        apiAgentCardAvailable: result.finalStatus.apiAgentCardAvailable,
        apiCachedBootstrapEventsAvailable: result.finalStatus.apiCachedBootstrapEventsAvailable,
        apiBrowserWorkspaceSnapshotAvailable: result.finalStatus.apiBrowserWorkspaceSnapshotAvailable,
        apiStateSnapshotAvailable: result.finalStatus.apiStateSnapshotAvailable,
        apiDisplayBootstrapAvailable: result.finalStatus.apiDisplayBootstrapAvailable,
        apiDisplayInputAuthorityAvailable: result.finalStatus.apiDisplayInputAuthorityAvailable,
        apiSessionLogReplayAvailable: result.finalStatus.apiSessionLogReplayAvailable,
        apiExternalSessionActivityReplayAvailable: result.finalStatus.apiExternalSessionActivityReplayAvailable,
        apiDashboardBootstrapAvailable: result.finalStatus.apiDashboardBootstrapAvailable,
        apiControlMsgAvailable: result.finalStatus.apiControlMsgAvailable,
        apiSessionControlMsgAvailable: result.finalStatus.apiSessionControlMsgAvailable,
        apiDashboardActionMsgAvailable: result.finalStatus.apiDashboardActionMsgAvailable,
        apiDiagnosticsVisualFreshnessAvailable: result.finalStatus.apiDiagnosticsVisualFreshnessAvailable,
        apiPeerMutationsAvailable: result.finalStatus.apiPeerMutationsAvailable,
        apiPeerPairingAvailable: result.finalStatus.apiPeerPairingAvailable,
        apiPeerWebRtcSignalAvailable: result.finalStatus.apiPeerWebRtcSignalAvailable,
        apiCoordinatorAvailable: result.finalStatus.apiCoordinatorAvailable,
        apiSessionDetailAvailable: result.finalStatus.apiSessionDetailAvailable,
        apiSessionReportAvailable: result.finalStatus.apiSessionReportAvailable,
        apiSessionDeleteAvailable: result.finalStatus.apiSessionDeleteAvailable,
        apiSessionContextSnapshotAvailable: result.finalStatus.apiSessionContextSnapshotAvailable,
        byteStreamsAvailable: result.finalStatus.byteStreamsAvailable,
        uploadFramesAvailable: result.finalStatus.uploadFramesAvailable,
        terminalFramesAvailable: result.finalStatus.terminalFramesAvailable,
        tuiFramesAvailable: result.finalStatus.tuiFramesAvailable,
        apiSessionCurrentUploadsAvailable: result.finalStatus.apiSessionCurrentUploadsAvailable,
        apiSessionCurrentUploadAvailable: result.finalStatus.apiSessionCurrentUploadAvailable,
        apiSessionCurrentUploadRawAvailable: result.finalStatus.apiSessionCurrentUploadRawAvailable,
        apiSessionCurrentUploadDeleteAvailable: result.finalStatus.apiSessionCurrentUploadDeleteAvailable,
        apiRecordingAssetAvailable: result.finalStatus.apiRecordingAssetAvailable,
        apiSessionFrameAssetAvailable: result.finalStatus.apiSessionFrameAssetAvailable,
        apiFsStatAvailable: result.finalStatus.apiFsStatAvailable,
        apiFsListAvailable: result.finalStatus.apiFsListAvailable,
        apiFsMkdirAvailable: result.finalStatus.apiFsMkdirAvailable,
        apiFsReadAvailable: result.finalStatus.apiFsReadAvailable,
        apiSettingsSaveAvailable: result.finalStatus.apiSettingsSaveAvailable,
        apiApiKeysSaveAvailable: result.finalStatus.apiApiKeysSaveAvailable,
        apiVoiceSessionAvailable: result.finalStatus.apiVoiceSessionAvailable,
        uploadStatus: result.upload._httpStatus,
        uploadListCount: result.uploads.length,
        uploadSize: result.upload.size,
        uploadRawBytes: result.uploadRaw.byteLength,
        uploadRawText: result.uploadRaw.text,
        imagePreviewScheme: result.imagePreview.previewScheme,
        imagePreviewByteStreamDelta: result.imagePreview.byteStreamDelta,
        recordingAssetBytes: result.recordingAsset.byteLength,
        recordingAssetText: result.recordingAsset.text,
        sessionFrameAssetBytes: result.sessionFrameAsset.byteLength,
        sessionFramePreviewScheme: result.sessionFramePreview.previewScheme,
        sessionFramePreviewByteStreamDelta: result.sessionFramePreview.byteStreamDelta,
        recordingFallbackSrcScheme: result.recordingFallbackPlayback.srcScheme,
        recordingFallbackByteStreamDelta: result.recordingFallbackPlayback.byteStreamDelta,
        recordingHlsSrcScheme: result.recordingHlsBlobPlaylist.srcScheme,
        recordingHlsObjectUrlCount: result.recordingHlsBlobPlaylist.objectUrlCount,
        recordingHlsByteStreamDelta: result.recordingHlsBlobPlaylist.byteStreamDelta,
        filesystemReadBytes: result.filesystemRead.byteLength,
        filesystemReadText: result.filesystemRead.text,
        diagnosticsVisualFreshnessWritten: result.diagnosticsVisualFreshness.written,
        terminalOutputBytes: result.terminal.outputBytes,
        tuiFrameBytes: result.tui.frameBytes,
        sessionReportStatus: result.sessionReport._httpStatus || 200,
        sessionReportSize: result.sessionReport.byteLength || result.sessionReport.size || 0,
        rejectedControlStatus: result.rejectedControlMsg._httpStatus,
        sessionControlAction: result.sessionControlMsg.action,
        rejectedSessionControlStatus: result.rejectedSessionControlMsg._httpStatus,
        dashboardActionAction: result.dashboardActionMsg.action,
        diagnosticsMarkerRegistryAvailable: result.diagnosticsMarkerActionMsg.registry_available,
        diagnosticsMarkerActiveDisplayUpdated: result.diagnosticsMarkerActionMsg.active_display_updated,
        peerPairingRequestCount: result.peerPairing.requests.requests.length,
        peerPairingIdentityCount: result.peerPairing.identities.identities.length,
        peerPairingMissingDecisionStatus: result.peerPairing.missingDecision._httpStatus,
        peerPairingMissingRevokeStatus: result.peerPairing.missingRevokeIdentity._httpStatus,
        peerWebRtcSignalStatus: result.peerWebRtcSignal._httpStatus,
        rejectedDashboardActionStatus: result.rejectedDashboardActionMsg._httpStatus,
        signalingMode: result.finalStatus.signalingMode,
        pendingRequests: result.finalStatus.pendingRequests,
      },
    }, null, 2));

  } finally {
    if (browser) await browser.close().catch(() => {});
    if (!daemon.killed) daemon.kill('SIGINT');
    await Promise.race([daemonExit, wait(5000)]);
    if (daemon.exitCode === null) daemon.kill('SIGKILL');
    removeRecordingFixture(recordingFixture);
    removeRecordingFixture(hlsRecordingFixture);
    removeSessionFrameFixture(sessionFrameFixture);
    removeFilesystemFixture(filesystemFixture);
    if (daemonLogs.length && daemon.exitCode && daemon.exitCode !== 0 && daemon.exitCode !== 130) {
      console.error(daemonLogs.join('').slice(-4000));
    }
  }
}

main()
  .then(() => process.exit(0))
  .catch(err => {
    console.error(err);
    process.exit(1);
  });
