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

function removeRecordingFixture(fixture) {
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
    const result = await page.evaluate(async () => {
      const ctl = window.intendantDashboardControl;
      const recordingStreamName = window.__intendantRecordingStreamName;
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
        uploadRaw: await uploadRaw(uploaded),
        recordingAsset: await recordingAsset(),
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
    assert.strictEqual(result.finalStatus.apiSessionReportAvailable, true);
    assert.strictEqual(result.finalStatus.byteStreamsAvailable, true);
    assert.strictEqual(result.finalStatus.uploadFramesAvailable, true);
    assert.strictEqual(result.finalStatus.terminalFramesAvailable, true);
    assert.strictEqual(result.finalStatus.tuiFramesAvailable, result.status.tui_frames_available === true);
    assert.strictEqual(result.finalStatus.apiSessionCurrentUploadAvailable, true);
    assert.strictEqual(result.finalStatus.apiSessionCurrentUploadRawAvailable, true);
    assert.strictEqual(result.finalStatus.apiRecordingAssetAvailable, true);
    assert.strictEqual(result.upload?._httpStatus, 200);
    assert.strictEqual(result.upload?._httpOk, true);
    assert.strictEqual(result.upload?.name, 'dashboard-upload-local.txt');
    assert.strictEqual(result.upload?.mime, 'text/plain');
    assert.strictEqual(result.upload?.size, 'dashboard upload e2e local'.length);
    assert.strictEqual(result.uploadRaw?.ok, true);
    assert.strictEqual(result.uploadRaw?.byteLength, 6);
    assert.strictEqual(result.uploadRaw?.text, 'upload');
    assert.strictEqual(result.uploadRaw?.total_size, 'dashboard upload e2e local'.length);
    assert.strictEqual(result.uploadRaw?.range_start, 10);
    assert.strictEqual(result.uploadRaw?.range_end, 16);
    assert.strictEqual(result.uploadRaw?.resumable, true);
    assert.strictEqual(result.recordingAsset?.ok, true);
    assert.strictEqual(result.recordingAsset?.byteLength, 7);
    assert.strictEqual(result.recordingAsset?.text, 'segment');
    assert.strictEqual(result.recordingAsset?.content_type, 'video/mp4');
    assert.strictEqual(result.recordingAsset?.range_start, 10);
    assert.strictEqual(result.recordingAsset?.range_end, 17);
    assert.strictEqual(result.recordingAsset?.resumable, true);
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
        apiSessionReportAvailable: result.finalStatus.apiSessionReportAvailable,
        byteStreamsAvailable: result.finalStatus.byteStreamsAvailable,
        uploadFramesAvailable: result.finalStatus.uploadFramesAvailable,
        terminalFramesAvailable: result.finalStatus.terminalFramesAvailable,
        tuiFramesAvailable: result.finalStatus.tuiFramesAvailable,
        apiSessionCurrentUploadAvailable: result.finalStatus.apiSessionCurrentUploadAvailable,
        apiSessionCurrentUploadRawAvailable: result.finalStatus.apiSessionCurrentUploadRawAvailable,
        apiRecordingAssetAvailable: result.finalStatus.apiRecordingAssetAvailable,
        uploadStatus: result.upload._httpStatus,
        uploadSize: result.upload.size,
        uploadRawBytes: result.uploadRaw.byteLength,
        uploadRawText: result.uploadRaw.text,
        recordingAssetBytes: result.recordingAsset.byteLength,
        recordingAssetText: result.recordingAsset.text,
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
