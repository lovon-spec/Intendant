#!/usr/bin/env node
'use strict';

const assert = require('assert');
const crypto = require('crypto');
const fs = require('fs');
const http = require('http');
const os = require('os');
const path = require('path');
const { spawn } = require('child_process');
const { httpStatus, launchBrowser } = require('./lib/browser-automation.cjs');

const DEFAULT_DAEMON_PORT = 8876;
const DEFAULT_RENDEZVOUS_PORT = 9876;
const DEFAULT_DAEMON_ID = 'connect-e2e-daemon';
const START_TIMEOUT_MS = 30000;
const CONNECT_TIMEOUT_MS = 30000;
const FRAME_FIXTURE_PNG_BASE64 = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII=';

function parseArgs(argv) {
  const repoRoot = path.resolve(__dirname, '..');
  const out = {
    repoRoot,
    dashboardBinary: path.join(repoRoot, 'target', 'release', 'intendant'),
    daemonPort: DEFAULT_DAEMON_PORT,
    rendezvousPort: DEFAULT_RENDEZVOUS_PORT,
    daemonId: DEFAULT_DAEMON_ID,
  };
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--dashboard-binary') {
      out.dashboardBinary = path.resolve(argv[++i]);
    } else if (arg === '--daemon-port') {
      out.daemonPort = Number(argv[++i]);
    } else if (arg === '--rendezvous-port') {
      out.rendezvousPort = Number(argv[++i]);
    } else if (arg === '--daemon-id') {
      out.daemonId = String(argv[++i] || '').trim();
    } else if (arg === '--help' || arg === '-h') {
      console.log(`Usage:
  node scripts/validate-connect-rendezvous.cjs [options]

Options:
  --dashboard-binary <path>   Intendant binary to launch.
  --daemon-port <port>        Fresh daemon web port. Default ${DEFAULT_DAEMON_PORT}.
  --rendezvous-port <port>    Local public-origin emulator port. Default ${DEFAULT_RENDEZVOUS_PORT}.
  --daemon-id <id>            Rendezvous daemon id. Default ${DEFAULT_DAEMON_ID}.
`);
      process.exit(0);
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  assert(Number.isInteger(out.daemonPort) && out.daemonPort > 0, 'invalid daemon port');
  assert(Number.isInteger(out.rendezvousPort) && out.rendezvousPort > 0, 'invalid rendezvous port');
  assert(out.daemonId, 'daemon id is required');
  return out;
}

function sendJson(res, status, body) {
  const text = JSON.stringify(body);
  res.writeHead(status, {
    'content-type': 'application/json',
    'content-length': Buffer.byteLength(text),
    'cache-control': 'no-store',
  });
  res.end(text);
}

function sendText(res, status, text, contentType = 'text/plain; charset=utf-8') {
  res.writeHead(status, {
    'content-type': contentType,
    'content-length': Buffer.byteLength(text),
    'cache-control': 'no-store',
  });
  res.end(text);
}

async function readJson(req) {
  const chunks = [];
  let total = 0;
  for await (const chunk of req) {
    total += chunk.length;
    if (total > 2 * 1024 * 1024) throw new Error('request body too large');
    chunks.push(chunk);
  }
  if (chunks.length === 0) return {};
  return JSON.parse(Buffer.concat(chunks).toString('utf8'));
}

function createRendezvousServer() {
  const daemons = new Map();
  const events = new Map();
  const pollers = new Map();
  const pendingOffers = new Map();
  const server = http.createServer(async (req, res) => {
    try {
      const url = new URL(req.url, 'http://127.0.0.1');
      if (req.method === 'GET' && (url.pathname === '/' || url.pathname === '/connect')) {
        return sendText(res, 200, publicBootstrapHtml(), 'text/html; charset=utf-8');
      }
      if (req.method === 'GET' && url.pathname === '/api/status') {
        const daemonId = url.searchParams.get('daemon_id') || '';
        return sendJson(res, 200, {
          ok: true,
          daemon_id: daemonId,
          registered: daemons.has(daemonId),
          queued: (events.get(daemonId) || []).length,
          pending_offers: Array.from(pendingOffers.values()).filter(p => p.daemonId === daemonId).length,
        });
      }
      if (req.method === 'POST' && url.pathname === '/api/daemon/register') {
        const body = await readJson(req);
        const daemonId = String(body.daemon_id || '').trim();
        if (!daemonId) return sendJson(res, 400, { error: 'missing daemon_id' });
        daemons.set(daemonId, {
          daemonId,
          daemonPublicKey: String(body.daemon_public_key || ''),
          registeredAt: Date.now(),
        });
        return sendJson(res, 200, { ok: true });
      }
      if (req.method === 'GET' && url.pathname === '/api/daemon/next') {
        const daemonId = String(url.searchParams.get('daemon_id') || '').trim();
        if (!daemonId) return sendJson(res, 400, { error: 'missing daemon_id' });
        const queue = events.get(daemonId) || [];
        if (queue.length > 0) return sendJson(res, 200, queue.shift());
        const timeoutMs = Math.min(Number(url.searchParams.get('timeout_ms') || 15000), 30000);
        let settled = false;
        const timer = setTimeout(() => {
          if (settled) return;
          settled = true;
          clearPoller(daemonId, res);
          res.writeHead(204);
          res.end();
        }, timeoutMs);
        const waiter = { res, timer };
        if (!pollers.has(daemonId)) pollers.set(daemonId, []);
        pollers.get(daemonId).push(waiter);
        req.on('close', () => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          clearPoller(daemonId, res);
        });
        return;
      }
      if (req.method === 'POST' && url.pathname === '/api/daemon/answer') {
        const body = await readJson(req);
        const offer = pendingOffers.get(String(body.request_id || ''));
        if (!offer) return sendJson(res, 404, { error: 'offer not found' });
        pendingOffers.delete(offer.id);
        clearTimeout(offer.timer);
        sendJson(offer.res, 200, {
          ok: true,
          session_id: body.session_id,
          sdp: body.sdp,
          binding: body.binding,
        });
        return sendJson(res, 200, { ok: true });
      }
      if (req.method === 'POST' && url.pathname === '/api/daemon/error') {
        const body = await readJson(req);
        const offer = pendingOffers.get(String(body.request_id || ''));
        if (offer) {
          pendingOffers.delete(offer.id);
          clearTimeout(offer.timer);
          sendJson(offer.res, 502, { error: String(body.error || 'daemon error') });
        }
        return sendJson(res, 200, { ok: true });
      }
      if (req.method === 'POST' && url.pathname === '/api/daemon/ack') {
        await readJson(req);
        return sendJson(res, 200, { ok: true });
      }
      if (req.method === 'POST' && url.pathname === '/api/browser/offer') {
        const body = await readJson(req);
        const daemonId = String(body.daemon_id || '').trim();
        const sdp = String(body.sdp || '');
        if (!daemonId || !sdp.trim()) return sendJson(res, 400, { error: 'missing daemon_id or sdp' });
        if (!daemons.has(daemonId)) return sendJson(res, 404, { error: 'daemon not registered' });
        const id = crypto.randomUUID();
        const timer = setTimeout(() => {
          if (!pendingOffers.has(id)) return;
          pendingOffers.delete(id);
          sendJson(res, 504, { error: 'timed out waiting for daemon answer' });
        }, CONNECT_TIMEOUT_MS);
        pendingOffers.set(id, { id, daemonId, res, timer });
        res.on('close', () => {
          clearTimeout(timer);
          pendingOffers.delete(id);
        });
        enqueueEvent(daemonId, { id, kind: 'offer', sdp });
        return;
      }
      if (req.method === 'POST' && url.pathname === '/api/browser/ice') {
        const body = await readJson(req);
        const daemonId = String(body.daemon_id || '').trim();
        const sessionId = String(body.session_id || '').trim();
        if (!daemonId || !sessionId) return sendJson(res, 400, { error: 'missing daemon_id or session_id' });
        enqueueEvent(daemonId, {
          id: crypto.randomUUID(),
          kind: 'ice',
          session_id: sessionId,
          candidate: body.candidate || {},
        });
        return sendJson(res, 200, { ok: true });
      }
      if (req.method === 'POST' && url.pathname === '/api/browser/close') {
        const body = await readJson(req);
        const daemonId = String(body.daemon_id || '').trim();
        const sessionId = String(body.session_id || '').trim();
        if (daemonId && sessionId) {
          enqueueEvent(daemonId, {
            id: crypto.randomUUID(),
            kind: 'close',
            session_id: sessionId,
          });
        }
        return sendJson(res, 200, { ok: true });
      }
      return sendJson(res, 404, { error: 'not found' });
    } catch (err) {
      return sendJson(res, 500, { error: err && err.message || String(err) });
    }
  });

  function clearPoller(daemonId, res) {
    const list = pollers.get(daemonId) || [];
    const next = list.filter(p => p.res !== res);
    if (next.length > 0) pollers.set(daemonId, next);
    else pollers.delete(daemonId);
  }

  function enqueueEvent(daemonId, event) {
    const list = pollers.get(daemonId) || [];
    if (list.length > 0) {
      const waiter = list.shift();
      if (list.length > 0) pollers.set(daemonId, list);
      else pollers.delete(daemonId);
      clearTimeout(waiter.timer);
      return sendJson(waiter.res, 200, event);
    }
    if (!events.has(daemonId)) events.set(daemonId, []);
    events.get(daemonId).push(event);
  }

  return server;
}

function publicBootstrapHtml() {
  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Intendant Connect Rendezvous E2E</title>
</head>
<body>
<pre id="status">starting</pre>
<script>
(() => {
  const statusEl = document.getElementById('status');
  const daemonId = new URLSearchParams(location.search).get('daemon_id') || '${DEFAULT_DAEMON_ID}';
  const MAX_CHUNKED_RESPONSE_BYTES = 128 * 1024 * 1024;
  const MAX_BYTE_STREAM_BYTES = 128 * 1024 * 1024;
  const UPLOAD_CHUNK_BYTES = 16 * 1024;
  const UPLOAD_BUFFER_HIGH_BYTES = 1024 * 1024;
  function paint(value) {
    statusEl.textContent = typeof value === 'string' ? value : JSON.stringify(value, null, 2);
  }
  function bytesToBase64Url(bytes) {
    let binary = '';
    for (const b of bytes) binary += String.fromCharCode(b);
    return btoa(binary).replace(/\\+/g, '-').replace(/\\//g, '_').replace(/=+$/g, '');
  }
  function base64UrlToBytes(value) {
    const normalized = String(value || '').replace(/-/g, '+').replace(/_/g, '/');
    const binary = atob(normalized.padEnd(Math.ceil(normalized.length / 4) * 4, '='));
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes;
  }
  function base64ToBytes(value) {
    const binary = atob(String(value || ''));
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes;
  }
  function bytesToBase64(bytes) {
    let binary = '';
    for (let i = 0; i < bytes.byteLength; i++) binary += String.fromCharCode(bytes[i]);
    return btoa(binary);
  }
  async function sha256B64u(text) {
    const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(String(text)));
    return bytesToBase64Url(new Uint8Array(digest));
  }
  function bindingPayload(binding) {
    return [
      binding.protocol || '',
      binding.session_id || '',
      binding.daemon_public_key || '',
      String(binding.created_unix_ms || ''),
      binding.offer_sha256 || '',
      binding.answer_sha256 || '',
    ].join('\\n');
  }
  async function verifyEd25519(publicKeyBytes, signatureBytes, payloadBytes) {
    let key;
    try {
      key = await crypto.subtle.importKey('raw', publicKeyBytes, { name: 'Ed25519' }, false, ['verify']);
    } catch (firstErr) {
      try {
        key = await crypto.subtle.importKey('raw', publicKeyBytes, 'Ed25519', false, ['verify']);
      } catch {
        throw firstErr;
      }
    }
    return crypto.subtle.verify({ name: 'Ed25519' }, key, signatureBytes, payloadBytes);
  }
  async function verifyBinding(binding, sessionId, offerSdp, answerSdp) {
    if (!binding || typeof binding !== 'object') return { ok: false, error: 'missing binding' };
    if (binding.protocol !== 'intendant-dashboard-control-v1') return { ok: false, error: 'unexpected protocol' };
    if (String(binding.session_id || '') !== String(sessionId || '')) return { ok: false, error: 'session mismatch' };
    if (binding.offer_sha256 !== await sha256B64u(offerSdp || '')) return { ok: false, error: 'offer hash mismatch' };
    if (binding.answer_sha256 !== await sha256B64u(answerSdp || '')) return { ok: false, error: 'answer hash mismatch' };
    const verified = await verifyEd25519(
      base64UrlToBytes(binding.daemon_public_key || ''),
      base64UrlToBytes(binding.signature || ''),
      new TextEncoder().encode(bindingPayload(binding))
    );
    if (!verified) return { ok: false, error: 'signature invalid' };
    return { ok: true, daemonPublicKey: binding.daemon_public_key, createdUnixMs: Number(binding.created_unix_ms || 0) };
  }
  function abortError(message = 'dashboard control request aborted') {
    try { return new DOMException(message, 'AbortError'); } catch {
      const err = new Error(message);
      err.name = 'AbortError';
      return err;
    }
  }
  const connect = {
    pc: null,
    channel: null,
    sessionId: '',
    verifiedBinding: null,
    pendingIce: [],
    pending: new Map(),
    chunkedResponses: new Map(),
    byteStreams: new Map(),
    completedChunkedResponses: 0,
    completedByteStreams: 0,
    lastStatus: null,
    seq: 0,
    async start() {
      this.pc = new RTCPeerConnection({});
      this.channel = this.pc.createDataChannel('intendant-dashboard-control', { ordered: true });
      this.channel.onopen = () => {
        this.sendFrame({ t: 'hello', id: this.nextId(), features: ['response_credit', 'byte_streams', 'terminal_frames', 'tui_frames'] });
        paint(this.status());
      };
      this.channel.onmessage = ev => this.handleMessage(ev.data);
      this.pc.onconnectionstatechange = () => paint(this.status());
      this.pc.onicecandidate = ev => {
        if (!ev.candidate) return;
        const candidate = ev.candidate.toJSON ? ev.candidate.toJSON() : ev.candidate;
        if (!this.sessionId) this.pendingIce.push(candidate);
        else this.sendIce(candidate).catch(err => console.warn('ice failed', err));
      };
      const offer = await this.pc.createOffer();
      await this.pc.setLocalDescription(offer);
      const offerSdp = offer.sdp || '';
      const answer = await fetch('/api/browser/offer', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ daemon_id: daemonId, sdp: offerSdp }),
      }).then(async resp => {
        const body = await resp.json().catch(() => ({}));
        if (!resp.ok) throw new Error(body.error || 'offer failed');
        return body;
      });
      this.sessionId = String(answer.session_id || '');
      const verified = await verifyBinding(answer.binding, this.sessionId, offerSdp, answer.sdp || '');
      if (!verified.ok) throw new Error('binding rejected: ' + (verified.error || 'unknown'));
      this.verifiedBinding = verified;
      await this.pc.setRemoteDescription({ type: 'answer', sdp: answer.sdp });
      for (const candidate of this.pendingIce.splice(0)) await this.sendIce(candidate);
      paint(this.status());
      return this.status();
    },
    async sendIce(candidate) {
      await fetch('/api/browser/ice', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ daemon_id: daemonId, session_id: this.sessionId, candidate }),
      });
    },
    handleMessage(data) {
      let msg;
      try { msg = JSON.parse(String(data)); } catch { return; }
      this.handleFrame(msg);
    },
    handleFrame(msg) {
      if (msg.t === 'hello_ack') {
        paint(this.status());
        return;
      }
      if (msg.t === 'terminal_output' || msg.t === 'terminal_exited' || msg.t === 'terminal_opened' || msg.t === 'terminal_error') {
        try {
          window.dispatchEvent(new CustomEvent('intendant-dashboard-terminal-frame', { detail: msg }));
        } catch (_) {}
        return;
      }
      if (msg.t === 'tui_term' || msg.t === 'tui_error') {
        try {
          window.dispatchEvent(new CustomEvent('intendant-dashboard-tui-frame', { detail: msg }));
        } catch (_) {}
        return;
      }
      if (msg.t === 'response_start') {
        this.handleResponseStart(msg);
        return;
      }
      if (msg.t === 'response_chunk') {
        this.handleResponseChunk(msg);
        return;
      }
      if (msg.t === 'response_end') {
        this.handleResponseEnd(msg);
        return;
      }
      if (msg.t === 'byte_stream_start') {
        this.handleByteStreamStart(msg);
        return;
      }
      if (msg.t === 'byte_stream_chunk') {
        this.handleByteStreamChunk(msg);
        return;
      }
      if (msg.t === 'byte_stream_end') {
        this.handleByteStreamEnd(msg);
        return;
      }
      if (msg.t === 'stream_start') {
        this.handleStreamStart(msg);
        return;
      }
      if (msg.t === 'stream_event') {
        this.handleStreamEvent(msg);
        return;
      }
      if (msg.t === 'stream_end') {
        this.handleStreamEnd(msg);
        return;
      }
      if (msg.t !== 'pong' && msg.t !== 'response') return;
      const pending = this.pending.get(msg.id);
      if (!pending) return;
      this.pending.delete(msg.id);
      if (msg.cancelled) pending.reject(abortError(msg.error || 'request cancelled'));
      else if (msg.t === 'response' && msg.ok === false) pending.reject(new Error(msg.error || 'request failed'));
      else pending.resolve(msg.t === 'pong' ? msg : msg.result);
    },
    handleResponseStart(msg) {
      const id = String(msg.id || '');
      const chunkKey = String(msg.chunk_id || id);
      if (!id || !chunkKey || !this.pending.has(id)) return;
      const totalBytes = Number(msg.total_bytes);
      const expectedChunks = Number(msg.chunks);
      if (
        msg.encoding !== 'base64-json-frame' ||
        !Number.isSafeInteger(totalBytes) ||
        totalBytes < 0 ||
        totalBytes > MAX_CHUNKED_RESPONSE_BYTES ||
        !Number.isSafeInteger(expectedChunks) ||
        expectedChunks < 0
      ) {
        this.rejectChunkedResponse(chunkKey, 'invalid dashboard control chunked response header');
        return;
      }
      this.chunkedResponses.set(chunkKey, {
        id,
        totalBytes,
        expectedChunks,
        receivedBytes: 0,
        chunks: new Map(),
        ended: false,
      });
      paint(this.status());
    },
    handleResponseChunk(msg) {
      const id = String(msg.id || '');
      const chunkKey = String(msg.chunk_id || id);
      const state = this.chunkedResponses.get(chunkKey);
      if (!state) return;
      const seq = Number(msg.seq);
      if (!Number.isSafeInteger(seq) || seq < 0 || seq >= state.expectedChunks) {
        this.rejectChunkedResponse(chunkKey, 'invalid dashboard control chunk sequence');
        return;
      }
      if (state.chunks.has(seq)) return;
      let bytes;
      try {
        bytes = base64ToBytes(msg.data);
      } catch {
        this.rejectChunkedResponse(chunkKey, 'invalid dashboard control chunk encoding');
        return;
      }
      state.chunks.set(seq, bytes);
      state.receivedBytes += bytes.byteLength;
      if (state.receivedBytes > state.totalBytes) {
        this.rejectChunkedResponse(chunkKey, 'dashboard control chunked response exceeded declared size');
        return;
      }
      const completed = this.maybeCompleteChunkedResponse(chunkKey);
      if (!completed && this.chunkedResponses.has(chunkKey)) {
        this.sendChunkCredit(id, 1, chunkKey === id ? null : chunkKey);
      }
      paint(this.status());
    },
    handleResponseEnd(msg) {
      const id = String(msg.id || '');
      const chunkKey = String(msg.chunk_id || id);
      const state = this.chunkedResponses.get(chunkKey);
      if (!state) return;
      const finalChunks = Number(msg.chunks);
      if (!Number.isSafeInteger(finalChunks) || finalChunks !== state.expectedChunks) {
        this.rejectChunkedResponse(chunkKey, 'invalid dashboard control chunked response footer');
        return;
      }
      state.ended = true;
      this.maybeCompleteChunkedResponse(chunkKey);
      paint(this.status());
    },
    maybeCompleteChunkedResponse(chunkKey) {
      const state = this.chunkedResponses.get(chunkKey);
      if (!state || !state.ended || state.chunks.size !== state.expectedChunks) return false;
      const merged = new Uint8Array(state.totalBytes);
      let offset = 0;
      for (let seq = 0; seq < state.expectedChunks; seq++) {
        const chunk = state.chunks.get(seq);
        if (!chunk) {
          this.rejectChunkedResponse(chunkKey, 'dashboard control chunked response missed a chunk');
          return false;
        }
        merged.set(chunk, offset);
        offset += chunk.byteLength;
      }
      if (offset !== state.totalBytes) {
        this.rejectChunkedResponse(chunkKey, 'dashboard control chunked response size mismatch');
        return false;
      }
      this.chunkedResponses.delete(chunkKey);
      let frame;
      try {
        frame = JSON.parse(new TextDecoder().decode(merged));
      } catch {
        this.rejectChunkedResponse(chunkKey, 'dashboard control chunked response was not valid JSON');
        return false;
      }
      if (!['response', 'stream_event'].includes(frame.t) || String(frame.id || '') !== state.id) {
        this.rejectChunkedResponse(chunkKey, 'dashboard control chunked response id mismatch');
        return false;
      }
      this.completedChunkedResponses += 1;
      this.handleFrame(frame);
      return true;
    },
    rejectChunkedResponse(chunkKey, message) {
      const state = this.chunkedResponses.get(chunkKey);
      const id = state?.id || chunkKey;
      this.chunkedResponses.delete(chunkKey);
      const pending = this.pending.get(id);
      if (pending) {
        this.pending.delete(id);
        pending.reject(new Error(message));
      }
      paint(this.status());
    },
    handleByteStreamStart(msg) {
      const id = String(msg.id || '');
      const streamId = String(msg.stream_id || id);
      if (!id || !streamId || !this.pending.has(id)) return;
      const totalBytes = Number(msg.total_bytes);
      const expectedChunks = Number(msg.chunks);
      if (
        msg.encoding !== 'base64' ||
        !Number.isSafeInteger(totalBytes) ||
        totalBytes < 0 ||
        totalBytes > MAX_BYTE_STREAM_BYTES ||
        !Number.isSafeInteger(expectedChunks) ||
        expectedChunks < 0
      ) {
        this.rejectByteStream(streamId, 'invalid dashboard control byte stream header', id);
        return;
      }
      this.byteStreams.set(streamId, {
        id,
        streamId,
        totalBytes,
        expectedChunks,
        receivedBytes: 0,
        chunks: new Map(),
        ended: false,
        result: null,
        contentType: String(msg.content_type || 'application/octet-stream'),
        filename: msg.filename ? String(msg.filename) : '',
      });
      paint(this.status());
    },
    handleByteStreamChunk(msg) {
      const id = String(msg.id || '');
      const streamId = String(msg.stream_id || id);
      const state = this.byteStreams.get(streamId);
      if (!state) return;
      const seq = Number(msg.seq);
      if (!Number.isSafeInteger(seq) || seq < 0 || seq >= state.expectedChunks) {
        this.rejectByteStream(streamId, 'invalid dashboard control byte stream chunk sequence');
        return;
      }
      if (state.chunks.has(seq)) return;
      let bytes;
      try {
        bytes = base64ToBytes(msg.data);
      } catch {
        this.rejectByteStream(streamId, 'invalid dashboard control byte stream encoding');
        return;
      }
      state.chunks.set(seq, bytes);
      state.receivedBytes += bytes.byteLength;
      if (state.receivedBytes > state.totalBytes) {
        this.rejectByteStream(streamId, 'dashboard control byte stream exceeded declared size');
        return;
      }
      const completed = this.maybeCompleteByteStream(streamId);
      if (!completed && this.byteStreams.has(streamId)) {
        this.sendChunkCredit(id, 1, streamId === id ? null : streamId);
      }
      paint(this.status());
    },
    handleByteStreamEnd(msg) {
      const id = String(msg.id || '');
      const streamId = String(msg.stream_id || id);
      const state = this.byteStreams.get(streamId);
      if (!state) return;
      if (msg.ok === false) {
        this.rejectByteStream(streamId, msg.error || 'dashboard control byte stream failed');
        return;
      }
      const finalChunks = Number(msg.chunks);
      if (!Number.isSafeInteger(finalChunks) || finalChunks !== state.expectedChunks) {
        this.rejectByteStream(streamId, 'invalid dashboard control byte stream footer');
        return;
      }
      state.ended = true;
      state.result = msg.result || null;
      this.maybeCompleteByteStream(streamId);
      paint(this.status());
    },
    maybeCompleteByteStream(streamId) {
      const state = this.byteStreams.get(streamId);
      if (!state || !state.ended || state.chunks.size !== state.expectedChunks) return false;
      const merged = new Uint8Array(state.totalBytes);
      let offset = 0;
      for (let seq = 0; seq < state.expectedChunks; seq++) {
        const chunk = state.chunks.get(seq);
        if (!chunk) {
          this.rejectByteStream(streamId, 'dashboard control byte stream missed a chunk');
          return false;
        }
        merged.set(chunk, offset);
        offset += chunk.byteLength;
      }
      if (offset !== state.totalBytes) {
        this.rejectByteStream(streamId, 'dashboard control byte stream size mismatch');
        return false;
      }
      this.byteStreams.delete(streamId);
      const pending = this.pending.get(state.id);
      if (!pending) return true;
      const result = state.result && typeof state.result === 'object' && !Array.isArray(state.result)
        ? { ...state.result }
        : {};
      result.ok = result.ok !== false;
      result.bytes = merged;
      result.size = state.totalBytes;
      result.content_type = result.content_type || state.contentType;
      result.filename = result.filename || state.filename;
      result.stream_id = state.streamId;
      this.completedByteStreams += 1;
      this.pending.delete(state.id);
      this.deleteChunkedResponsesForRequest(state.id);
      pending.resolve(result);
      paint(this.status());
      return true;
    },
    rejectByteStream(streamId, message, requestId = '') {
      const state = this.byteStreams.get(streamId);
      const id = state?.id || requestId || streamId;
      this.byteStreams.delete(streamId);
      const pending = this.pending.get(id);
      if (pending) {
        this.pending.delete(id);
        pending.reject(new Error(message));
      }
      paint(this.status());
    },
    handleStreamStart(msg) {
      const pending = this.pending.get(String(msg.id || ''));
      const stream = pending?.stream;
      if (!stream) return;
      stream.started = true;
      this.callStreamCallback(stream, 'start', msg);
    },
    handleStreamEvent(msg) {
      const pending = this.pending.get(String(msg.id || ''));
      const stream = pending?.stream;
      if (!stream) return;
      stream.eventCount += 1;
      this.callStreamCallback(stream, 'event', msg.event, msg);
    },
    handleStreamEnd(msg) {
      const id = String(msg.id || '');
      const pending = this.pending.get(id);
      const stream = pending?.stream;
      if (!pending || !stream) return;
      this.pending.delete(id);
      if (msg.ok === false) {
        pending.reject(new Error(msg.error || 'dashboard control stream failed'));
        return;
      }
      this.callStreamCallback(stream, 'end', msg.result || null, msg);
      pending.resolve(msg.result || null);
    },
    callStreamCallback(stream, name, ...args) {
      const callbacks = stream.callbacks;
      if (typeof callbacks === 'function' && name === 'event') {
        callbacks(...args);
      } else if (callbacks && typeof callbacks[name] === 'function') {
        callbacks[name](...args);
      }
    },
    request(method, params = {}, options = {}) {
      if (options.signal?.aborted) return Promise.reject(abortError());
      if (!this.canUseRpc()) return Promise.reject(new Error('dashboard control RPC is not connected'));
      const id = this.nextId();
      const promise = this.waitFor(id, options);
      this.sendFrame({ t: 'request', id, method, params });
      if (method === 'status') {
        return promise.then(status => {
          if (status && typeof status === 'object') this.lastStatus = status;
          return status;
        });
      }
      return promise;
    },
    requestBytes(method, params = {}, options = {}) {
      if (options.signal?.aborted) return Promise.reject(abortError());
      if (!this.canUseRpc()) return Promise.reject(new Error('dashboard control byte stream is not connected'));
      const id = this.nextId();
      const promise = this.waitFor(id, options);
      const pending = this.pending.get(id);
      if (pending) pending.expectBytes = true;
      this.sendFrame({ t: 'request', id, method, params, bytes: true });
      return promise;
    },
    async uploadBytes(method, params = {}, bytes, options = {}) {
      if (options.signal?.aborted) return Promise.reject(abortError());
      if (!this.canUseRpc()) return Promise.reject(new Error('dashboard control upload is not connected'));
      const data = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
      const id = this.nextId();
      const totalBytes = data.byteLength;
      const chunkSize = options.chunkBytes || UPLOAD_CHUNK_BYTES;
      const chunks = Math.ceil(totalBytes / chunkSize);
      const promise = this.waitFor(id, options);
      this.sendFrame({
        t: 'upload_start',
        id,
        method,
        params,
        encoding: 'base64',
        total_bytes: totalBytes,
        chunks,
      });
      try {
        for (let seq = 0, offset = 0; offset < totalBytes; seq++, offset += chunkSize) {
          if (options.signal?.aborted) throw abortError();
          if (!this.pending.has(id)) break;
          const chunk = data.subarray(offset, Math.min(offset + chunkSize, totalBytes));
          this.sendFrame({
            t: 'upload_chunk',
            id,
            seq,
            data: bytesToBase64(chunk),
          });
          await this.waitForBufferedAmountLow(options.signal);
        }
        if (this.pending.has(id)) this.sendFrame({ t: 'upload_end', id, chunks });
      } catch (err) {
        if (this.pending.has(id)) this.sendFrame({ t: 'cancel', id });
        throw err;
      }
      return promise;
    },
    async waitForBufferedAmountLow(signal = null) {
      while (
        this.channel &&
        this.channel.readyState === 'open' &&
        this.channel.bufferedAmount > UPLOAD_BUFFER_HIGH_BYTES
      ) {
        if (signal?.aborted) throw abortError();
        await new Promise(resolve => setTimeout(resolve, 10));
      }
    },
    terminalFrame(frame) {
      if (!this.canUseRpc()) return false;
      this.sendFrame(frame);
      return true;
    },
    tuiFrame(frame) {
      if (!this.canUseRpc()) return false;
      this.sendFrame(frame);
      return true;
    },
    stream(method, params = {}, options = {}, onEvent = {}) {
      if (options.signal?.aborted) return Promise.reject(abortError());
      if (!this.canUseRpc()) return Promise.reject(new Error('dashboard control stream is not connected'));
      const id = this.nextId();
      const promise = this.waitFor(id, options);
      const pending = this.pending.get(id);
      if (pending) {
        pending.stream = {
          callbacks: onEvent,
          eventCount: 0,
          started: false,
        };
      }
      this.sendFrame({ t: 'request', id, method, params, stream: true });
      return promise;
    },
    waitFor(id, options = {}) {
      return new Promise((resolve, reject) => {
        let settled = false;
        const signal = options.signal || null;
        const fail = (err, cancel = false) => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          if (signal && abortHandler) signal.removeEventListener('abort', abortHandler);
          this.pending.delete(id);
          this.deleteChunkedResponsesForRequest(id);
          this.deleteByteStreamsForRequest(id);
          if (cancel) this.sendFrame({ t: 'cancel', id });
          reject(err);
        };
        const timeoutMs = Number.isFinite(Number(options.timeoutMs)) ? Number(options.timeoutMs) : 10000;
        const timer = setTimeout(() => fail(new Error('request timed out'), true), timeoutMs);
        const abortHandler = signal ? () => fail(abortError(), true) : null;
        if (signal && abortHandler) signal.addEventListener('abort', abortHandler, { once: true });
        this.pending.set(id, {
          resolve: value => {
            if (settled) return;
            settled = true;
            clearTimeout(timer);
            if (signal && abortHandler) signal.removeEventListener('abort', abortHandler);
            this.deleteChunkedResponsesForRequest(id);
            this.deleteByteStreamsForRequest(id);
            resolve(value);
          },
          reject: err => fail(err),
        });
      });
    },
    deleteChunkedResponsesForRequest(id) {
      for (const [chunkKey, state] of this.chunkedResponses) {
        if (chunkKey === id || state?.id === id) {
          this.chunkedResponses.delete(chunkKey);
        }
      }
    },
    deleteByteStreamsForRequest(id) {
      for (const [streamId, state] of this.byteStreams) {
        if (streamId === id || state?.id === id) {
          this.byteStreams.delete(streamId);
        }
      }
    },
    canUseRpc() {
      return Boolean(this.verifiedBinding && this.pc?.connectionState === 'connected' && this.channel?.readyState === 'open');
    },
    sendFrame(frame) {
      if (this.channel?.readyState === 'open') this.channel.send(JSON.stringify(frame));
    },
    sendChunkCredit(id, chunks, chunkId = null) {
      const frame = { t: 'credit', id, chunks };
      if (chunkId) frame.chunk_id = chunkId;
      this.sendFrame(frame);
    },
    status() {
      return {
        daemonId,
        connected: this.pc?.connectionState === 'connected',
        pcState: this.pc?.connectionState || '',
        channelState: this.channel?.readyState || '',
        sessionId: this.sessionId,
        verifiedBinding: this.verifiedBinding,
        pendingRequests: this.pending.size,
        pendingChunkedResponses: this.chunkedResponses.size,
        pendingByteStreams: this.byteStreams.size,
        completedChunkedResponses: this.completedChunkedResponses,
        completedByteStreams: this.completedByteStreams,
        apiAgentCardAvailable: this.lastStatus?.api_agent_card_available ?? null,
        apiCachedBootstrapEventsAvailable: this.lastStatus?.api_cached_bootstrap_events_available ?? null,
        apiBrowserWorkspaceSnapshotAvailable: this.lastStatus?.api_browser_workspace_snapshot_available ?? null,
        apiStateSnapshotAvailable: this.lastStatus?.api_state_snapshot_available ?? null,
        apiDisplayBootstrapAvailable: this.lastStatus?.api_display_bootstrap_available ?? null,
        apiDisplayInputAuthorityAvailable: this.lastStatus?.api_display_input_authority_available ?? null,
        apiSessionLogReplayAvailable: this.lastStatus?.api_session_log_replay_available ?? null,
        apiExternalSessionActivityReplayAvailable: this.lastStatus?.api_external_session_activity_replay_available ?? null,
        apiDashboardBootstrapAvailable: this.lastStatus?.api_dashboard_bootstrap_available ?? null,
        apiPeersAvailable: this.lastStatus?.api_peers_available ?? null,
        apiSessionsAvailable: this.lastStatus?.api_sessions_available ?? null,
        apiSessionsStreamAvailable: this.lastStatus?.api_sessions_stream_available ?? null,
        byteStreamsAvailable: this.lastStatus?.byte_streams_available ?? null,
        uploadFramesAvailable: this.lastStatus?.upload_frames_available ?? null,
        terminalFramesAvailable: this.lastStatus?.terminal_frames_available ?? null,
        tuiFramesAvailable: this.lastStatus?.tui_frames_available ?? null,
        apiSessionDetailAvailable: this.lastStatus?.api_session_detail_available ?? null,
        apiSessionReportAvailable: this.lastStatus?.api_session_report_available ?? null,
        apiSessionDeleteAvailable: this.lastStatus?.api_session_delete_available ?? null,
        apiSessionCurrentAgentOutputAvailable: this.lastStatus?.api_session_current_agent_output_available ?? null,
        apiSessionCurrentHistoryAvailable: this.lastStatus?.api_session_current_history_available ?? null,
        apiSessionCurrentRollbackAvailable: this.lastStatus?.api_session_current_rollback_available ?? null,
        apiSessionCurrentRedoAvailable: this.lastStatus?.api_session_current_redo_available ?? null,
        apiSessionCurrentPruneAvailable: this.lastStatus?.api_session_current_prune_available ?? null,
        apiSessionCurrentChangesAvailable: this.lastStatus?.api_session_current_changes_available ?? null,
        apiSessionContextSnapshotAvailable: this.lastStatus?.api_session_context_snapshot_available ?? null,
        apiSessionCurrentUploadAvailable: this.lastStatus?.api_session_current_upload_available ?? null,
        apiSessionCurrentUploadsAvailable: this.lastStatus?.api_session_current_uploads_available ?? null,
        apiSessionCurrentUploadRawAvailable: this.lastStatus?.api_session_current_upload_raw_available ?? null,
        apiSessionCurrentUploadDeleteAvailable: this.lastStatus?.api_session_current_upload_delete_available ?? null,
        apiFsStatAvailable: this.lastStatus?.api_fs_stat_available ?? null,
        apiFsListAvailable: this.lastStatus?.api_fs_list_available ?? null,
        apiFsMkdirAvailable: this.lastStatus?.api_fs_mkdir_available ?? null,
        apiFsReadAvailable: this.lastStatus?.api_fs_read_available ?? null,
        apiSessionsSearchAvailable: this.lastStatus?.api_sessions_search_available ?? null,
        apiSettingsAvailable: this.lastStatus?.api_settings_available ?? null,
        apiSettingsSaveAvailable: this.lastStatus?.api_settings_save_available ?? null,
        apiControlMsgAvailable: this.lastStatus?.api_control_msg_available ?? null,
        apiSessionControlMsgAvailable: this.lastStatus?.api_session_control_msg_available ?? null,
        apiDashboardActionMsgAvailable: this.lastStatus?.api_dashboard_action_msg_available ?? null,
        apiDiagnosticsVisualFreshnessAvailable: this.lastStatus?.api_diagnostics_visual_freshness_available ?? null,
        apiKeyStatusAvailable: this.lastStatus?.api_key_status_available ?? null,
        apiApiKeysSaveAvailable: this.lastStatus?.api_api_keys_save_available ?? null,
        apiVoiceSessionAvailable: this.lastStatus?.api_voice_session_available ?? null,
        apiProjectRootAvailable: this.lastStatus?.api_project_root_available ?? null,
        apiDisplaysAvailable: this.lastStatus?.api_displays_available ?? null,
        apiRecordingsAvailable: this.lastStatus?.api_recordings_available ?? null,
        apiRecordingAssetAvailable: this.lastStatus?.api_recording_asset_available ?? null,
        apiSessionRecordingsAvailable: this.lastStatus?.api_session_recordings_available ?? null,
        apiSessionRecordingAssetAvailable: this.lastStatus?.api_session_recording_asset_available ?? null,
        apiSessionFrameAssetAvailable: this.lastStatus?.api_session_frame_asset_available ?? null,
        apiWorktreesAvailable: this.lastStatus?.api_worktrees_available ?? null,
        apiWorktreesScanAvailable: this.lastStatus?.api_worktrees_scan_available ?? null,
        apiWorktreesRemoveAvailable: this.lastStatus?.api_worktrees_remove_available ?? null,
        apiManagedContextAvailable: this.lastStatus?.api_managed_context_available ?? null,
        apiMcpToolCallAvailable: this.lastStatus?.api_mcp_tool_call_available ?? null,
        apiPeerMutationsAvailable: this.lastStatus?.api_peer_mutations_available ?? null,
        apiPeerPairingAvailable: this.lastStatus?.api_peer_pairing_available ?? null,
        apiPeerWebRtcSignalAvailable: this.lastStatus?.api_peer_webrtc_signal_available ?? null,
        apiCoordinatorAvailable: this.lastStatus?.api_coordinator_available ?? null,
      };
    },
    close() {
      if (this.sessionId) {
        fetch('/api/browser/close', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ daemon_id: daemonId, session_id: this.sessionId }),
        }).catch(() => {});
      }
      this.chunkedResponses.clear();
      this.byteStreams.clear();
      try { this.channel?.close(); } catch {}
      try { this.pc?.close(); } catch {}
    },
    nextId() {
      this.seq += 1;
      return 'public-connect-' + Date.now() + '-' + this.seq;
    },
  };
  window.intendantPublicConnectDashboard = connect;
  connect.start().catch(err => {
    console.error(err);
    paint(err?.message || String(err));
  });
})();
</script>
</body>
</html>`;
}

function wait(ms) {
  return new Promise(resolve => setTimeout(resolve, ms));
}

function createRecordingFixture(label) {
  const streamName = `dashboard_control_${label}_${process.pid}_${Date.now()}`;
  const dir = path.join(os.homedir(), '.intendant', 'recordings', streamName);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'segments.csv'), 'seg_00000.mp4,0,1.25\n');
  fs.writeFileSync(path.join(dir, 'seg_00000.mp4'), 'recording segment e2e rendezvous');
  return { streamName, dir };
}

function createHlsRecordingFixture(label) {
  const streamName = `dashboard_control_hls_${label}_${process.pid}_${Date.now()}`;
  const dir = path.join(os.homedir(), '.intendant', 'recordings', streamName);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'segments.csv'), 'seg_00000.ts,0,1.25\n');
  fs.writeFileSync(path.join(dir, 'seg_00000.ts'), 'recording hls transport stream e2e rendezvous');
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

async function waitForBrowserConnect(page, globalName = 'intendantPublicConnectDashboard') {
  let last = null;
  const deadline = Date.now() + CONNECT_TIMEOUT_MS;
  const globalNameJson = JSON.stringify(globalName);
  while (Date.now() < deadline) {
    last = await page.evaluate(`(() => {
      const dashboard = window[${globalNameJson}];
      if (!dashboard) return null;
      return dashboard.status();
    })()`).catch(() => null);
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
  throw new Error(`${globalName} did not connect: ${JSON.stringify(last)}`);
}

async function main() {
  const options = parseArgs(process.argv);
  const rendezvous = createRendezvousServer();
  await new Promise((resolve, reject) => {
    rendezvous.once('error', reject);
    rendezvous.listen(options.rendezvousPort, '127.0.0.1', resolve);
  });

  const daemonLogs = [];
  const recordingFixture = createRecordingFixture('rendezvous');
  const hlsRecordingFixture = createHlsRecordingFixture('rendezvous');
  const sessionFrameFixture = createSessionFrameFixture('rendezvous');
  const filesystemFixture = createFilesystemFixture('rendezvous');
  const daemon = spawn(options.dashboardBinary, ['--no-tui', '--web', String(options.daemonPort)], {
    cwd: options.repoRoot,
    env: {
      ...process.env,
      INTENDANT_CONNECT_RENDEZVOUS_URL: `http://127.0.0.1:${options.rendezvousPort}`,
      INTENDANT_CONNECT_DAEMON_ID: options.daemonId,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  const daemonExit = new Promise(resolve => daemon.once('exit', resolve));
  daemon.stdout.on('data', chunk => daemonLogs.push(chunk.toString()));
  daemon.stderr.on('data', chunk => daemonLogs.push(chunk.toString()));
  daemon.once('error', err => daemonLogs.push(String(err && err.message || err)));

  let browser;
  try {
    await waitFor(() => daemonLogs.join('').includes(`Web TUI: https://0.0.0.0:${options.daemonPort}`), START_TIMEOUT_MS, 'daemon web startup');
    await waitFor(async () => {
      const status = await fetchJson(`http://127.0.0.1:${options.rendezvousPort}/api/status?daemon_id=${encodeURIComponent(options.daemonId)}`);
      return status.registered ? status : null;
    }, START_TIMEOUT_MS, 'daemon rendezvous registration');

    const certlessConfigStatus = await httpStatus(`https://127.0.0.1:${options.daemonPort}/config`, {
      ignoreHTTPSErrors: true,
    });
    assert.strictEqual(certlessConfigStatus, 401, `/config without client cert returned ${certlessConfigStatus}`);

    browser = await launchBrowser({ headless: true, ignoreHTTPSErrors: true });
    const page = await browser.newPage();
    page.on('console', msg => console.log(`[browser:${msg.type()}] ${msg.text()}`));
    const publicOrigin = `http://127.0.0.1:${options.rendezvousPort}`;
    const response = await page.goto(`${publicOrigin}/connect?daemon_id=${encodeURIComponent(options.daemonId)}`, {
      waitUntil: 'domcontentloaded',
      timeout: CONNECT_TIMEOUT_MS,
    });
    assert(response, 'public bootstrap produced no response');
    assert.strictEqual(response.status(), 200, `public bootstrap returned ${response.status()}`);
    await page.waitForFunction(() => Boolean(window.intendantPublicConnectDashboard));
    const connected = await waitForBrowserConnect(page);

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
      const ctl = window.intendantPublicConnectDashboard;
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
      const beforeChunks = ctl.status().completedChunkedResponses || 0;
      const largeSessions = await ctl.request('api_sessions', { limit: 'all' }, { timeoutMs: 60000 });
      const largeSessionsJson = JSON.stringify(largeSessions);
      const streamEvents = [];
      const streamResult = await ctl.stream('api_sessions_stream', { limit: 3 }, { timeoutMs: 60000 }, {
        event(event) {
          streamEvents.push({
            type: event?.type || '',
            hasSession: Boolean(event?.session),
            sessionCount: Array.isArray(event?.sessions) ? event.sessions.length : null,
          });
        },
      });
      const largeStreamEvents = [];
      const beforeLargeStreamChunks = ctl.status().completedChunkedResponses || 0;
      const largeStreamResult = await ctl.stream('api_sessions_stream', { limit: 'all' }, { timeoutMs: 120000 }, {
        event(event) {
          if (event?.type === 'replace' && Array.isArray(event.sessions)) {
            largeStreamEvents.push({
              type: 'replace',
              sessionCount: event.sessions.length,
              jsonBytes: new TextEncoder().encode(JSON.stringify(event)).length,
            });
          } else {
            largeStreamEvents.push({
              type: event?.type || '',
              sessionCount: null,
              jsonBytes: null,
            });
          }
        },
      });
      const afterLargeStreamChunks = ctl.status().completedChunkedResponses || 0;
      const sessions = await ctl.request('api_sessions', { limit: 2 });
      const firstSessionId = Array.isArray(sessions)
        ? String(sessions[0]?.session_id || '').trim()
        : '';
      const sessionsById = firstSessionId
        ? await ctl.request('api_sessions', { ids: [firstSessionId] })
        : [];
      const sessionDelete = {
        invalidSession: await ctl.request('api_session_delete', { session_id: '../bad' }),
      };
      const sessionControl = {
        interrupt: await labeled('api_session_control_msg interrupt', ctl.request('api_session_control_msg', {
          message: { action: 'interrupt' },
        })),
        rejectedSettingsAction: await labeled('api_session_control_msg rejected set_codex_sandbox', ctl.request('api_session_control_msg', {
          message: { action: 'set_codex_sandbox', mode: 'workspace-write' },
        })),
      };
      const dashboardAction = {
        closeWorkspace: await labeled('api_dashboard_action_msg close_browser_workspace', ctl.request('api_dashboard_action_msg', {
          message: { action: 'close_browser_workspace', workspace_id: `validator-workspace-${Date.now()}` },
        })),
        diagnosticsVisualMarker: await labeled('api_dashboard_action_msg diagnostics visual marker', ctl.request('api_dashboard_action_msg', {
          message: { action: 'set_diagnostics_visual_marker', display_id: 0, enabled: false },
        })),
        rejectedSettingsAction: await labeled('api_dashboard_action_msg rejected set_codex_sandbox', ctl.request('api_dashboard_action_msg', {
          message: { action: 'set_codex_sandbox', mode: 'workspace-write' },
        })),
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
        const bytes = new TextEncoder().encode('dashboard upload e2e rendezvous');
        return labeled('api_session_current_upload', ctl.uploadBytes('api_session_current_upload', {
          destination: 'task',
          name: 'dashboard-upload-rendezvous.txt',
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
        if (!input) {
          return { skipped: true, reason: 'upload file input unavailable on rendezvous emulator' };
        }
        const png = Uint8Array.from(
          atob('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII='),
          ch => ch.charCodeAt(0)
        );
        const before = ctl.status().completedByteStreams || 0;
        const file = new File([png], 'dashboard-preview-rendezvous.png', { type: 'image/png' });
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
      const recordingHlsAssets = async () => {
        const playlistRaw = await labeled('api_recording_asset hls playlist', ctl.requestBytes('api_recording_asset', {
          stream_name: hlsRecordingStreamName,
          asset: 'playlist.m3u8',
        }, { timeoutMs: 60000 }));
        const segmentRaw = await labeled('api_recording_asset hls segment', ctl.requestBytes('api_recording_asset', {
          stream_name: hlsRecordingStreamName,
          asset: 'seg_00000.ts',
          offset: 10,
          length: 9,
        }, { timeoutMs: 60000 }));
        const bytesToText = raw => {
          if (raw?.bytes instanceof Uint8Array) return new TextDecoder().decode(raw.bytes);
          return raw?.data_base64 ? atob(String(raw.data_base64)) : '';
        };
        return {
          playlist: {
            ...Object.fromEntries(Object.entries(playlistRaw || {}).filter(([key]) => key !== 'bytes')),
            byteLength: playlistRaw?.bytes instanceof Uint8Array
              ? playlistRaw.bytes.byteLength
              : (playlistRaw?.data_base64 ? atob(String(playlistRaw.data_base64)).length : 0),
            text: bytesToText(playlistRaw),
          },
          segment: {
            ...Object.fromEntries(Object.entries(segmentRaw || {}).filter(([key]) => key !== 'bytes')),
            byteLength: segmentRaw?.bytes instanceof Uint8Array
              ? segmentRaw.bytes.byteLength
              : (segmentRaw?.data_base64 ? atob(String(segmentRaw.data_base64)).length : 0),
            text: bytesToText(segmentRaw),
          },
        };
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
      const peerPairing = async () => ({
        requests: await labeled('api_peer_pairing_requests', ctl.request('api_peer_pairing_requests', {}, { timeoutMs: 60000 })),
        identities: await labeled('api_peer_pairing_identities', ctl.request('api_peer_pairing_identities', {}, { timeoutMs: 60000 })),
        missingDecision: await labeled('api_peer_pairing_request_decision missing request', ctl.request('api_peer_pairing_request_decision', {
          request_id: `missing-request-${Date.now()}`,
          op: 'approve',
        }, { timeoutMs: 60000 })),
        missingRevokeIdentity: await labeled('api_peer_pairing_identity_revoke missing identity', ctl.request('api_peer_pairing_identity_revoke', {}, { timeoutMs: 60000 })),
      });
      const recordingFallbackPlayback = async () => {
        if (typeof RecordingPlayer !== 'function') {
          return { skipped: true, reason: 'RecordingPlayer unavailable on rendezvous emulator' };
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
      const diagnosticsVisualFreshness = async () => {
        const sessionId = `validator-rendezvous-vf-${Date.now()}-${Math.random().toString(36).slice(2)}`;
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
      const terminal = async () => {
        const terminalId = `dashboard-terminal-rendezvous-${Date.now()}`;
        const token = 'dashboard_terminal_e2e_rendezvous';
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
        const connectionId = `dashboard-tui-rendezvous-${Date.now()}`;
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
      const status = await ctl.request('status');
      const uploaded = await upload();
      return {
        status,
        config: await ctl.request('config'),
        agentCard: await ctl.request('api_agent_card'),
        cachedBootstrapEvents: await ctl.request('api_cached_bootstrap_events'),
        browserWorkspaceSnapshot: await ctl.request('api_browser_workspace_snapshot'),
        stateSnapshot: await ctl.request('api_state_snapshot'),
        displayBootstrap: await ctl.request('api_display_bootstrap'),
        displayAuthoritySnapshot: await ctl.request('api_display_input_authority_snapshot'),
        sessionLogReplay: await ctl.request('api_session_log_replay'),
        externalSessionActivityReplay: await ctl.request('api_external_session_activity_replay'),
        dashboardBootstrap: await ctl.request('api_dashboard_bootstrap'),
        sessions,
        sessionsById,
        sessionsByIdTarget: firstSessionId,
        sessionDelete,
        sessionControl,
        dashboardAction,
        peerWebRtcSignal: await labeled('api_peer_webrtc_signal missing peer', ctl.request('api_peer_webrtc_signal', {
          peer_id: 'missing-peer',
          display_id: 0,
          session_id: `validator-peer-display-${Date.now()}`,
          signal: { kind: 'close' },
        }, { timeoutMs: 60000 })),
        sessionReport: await sessionReport(),
        upload: uploaded,
        uploadList: await ctl.request('api_session_current_uploads', {}, { timeoutMs: 60000 }),
        uploadRaw: await uploadRaw(uploaded),
        imagePreview: await imagePreview(),
        recordingAsset: await recordingAsset(),
        recordingHlsAssets: await recordingHlsAssets(),
        sessionFrameAsset: await sessionFrameAsset(),
        filesystemRead: await filesystemRead(),
        recordingFallbackPlayback: await recordingFallbackPlayback(),
        diagnosticsVisualFreshness: await diagnosticsVisualFreshness(),
        peerPairing: await peerPairing(),
        terminal: await terminal(),
        tui: status.tui_frames_available ? await tui() : { skipped: true, subscribed: false, frameBytes: 0 },
        sessionsStream: {
          result: streamResult,
          eventTypes: streamEvents.map(event => event.type),
          eventCount: streamEvents.length,
          replaceCount: streamEvents.find(event => event.type === 'replace')?.sessionCount ?? null,
        },
        largeSessionsStream: {
          result: largeStreamResult,
          eventTypes: largeStreamEvents.map(event => event.type),
          eventCount: largeStreamEvents.length,
          replaceCount: largeStreamEvents.find(event => event.type === 'replace')?.sessionCount ?? null,
          replaceBytes: largeStreamEvents.find(event => event.type === 'replace')?.jsonBytes ?? null,
          completedChunkedResponsesBefore: beforeLargeStreamChunks,
          completedChunkedResponsesAfter: afterLargeStreamChunks,
        },
        largeSessions: {
          ok: Array.isArray(largeSessions),
          length: Array.isArray(largeSessions) ? largeSessions.length : null,
          jsonBytes: new TextEncoder().encode(largeSessionsJson).length,
          completedChunkedResponsesBefore: beforeChunks,
        },
        agentOutput: await ctl.request('api_session_current_agent_output', { ids: ['missing-output'] }),
        timeline: {
          history: await ctl.request('api_session_current_history'),
          rollbackValidation: await ctl.request('api_session_current_rollback', {
            round_id: 1,
            revert_files: false,
            revert_conversation: false,
          }),
        },
        changes: {
          list: await ctl.request('api_session_current_changes'),
          traversal: await ctl.request('api_session_current_changes', { path: '../Cargo.toml' }),
        },
        contextSnapshots: {
          missingSelector: await ctl.request('api_session_context_snapshot', { session_id: 'missing-context-session' }),
          invalidSession: await ctl.request('api_session_context_snapshot', {
            session_id: '../bad',
            file: 'snapshot.json',
          }),
        },
        uploads: {
          missingDeleteId: await ctl.request('api_session_current_upload_delete', {}),
        },
        mcp: {
          missingSession: await ctl.request('api_mcp_tool_call', {
            mcp_id: 99,
            name: 'get_status',
          }),
        },
        recordings: {
          live: await ctl.request('api_recordings'),
          invalidSession: await ctl.request('api_session_recordings', { session_id: '../bad' }),
        },
        worktrees: {
          cached: await ctl.request('api_worktrees'),
          scan: await ctl.request('api_worktrees_scan', {}, { timeoutMs: 120000 }),
          invalidRemove: await ctl.request('api_worktrees_remove', {}),
        },
        filesystem: {
          statHome: await ctl.request('api_fs_stat', { path: '~' }),
          listHome: await ctl.request('api_fs_list', { path: '~' }),
          badRelative: await ctl.request('api_fs_stat', { path: 'relative/path' }),
          mkdirHome: await ctl.request('api_fs_mkdir', { path: '~' }),
          mkdirBadRelative: await ctl.request('api_fs_mkdir', { path: 'relative/path' }),
        },
        appError: await ctl.request('api_peer_eligible', { capabilities: [] }),
        finalStatus: ctl.status(),
      };
    });
    assert(result.status && result.status.session_id, 'status RPC did not return a session id');
    assert.strictEqual(
      result.status.response_credit_enabled,
      true,
      'dashboard control did not negotiate response credit'
    );
    assert.strictEqual(
      result.status.api_sessions_stream_available,
      true,
      'dashboard control status did not advertise sessions streaming'
    );
    assert.strictEqual(
      result.status.api_session_control_msg_available,
      true,
      'dashboard control status did not advertise session control messages'
    );
    assert.strictEqual(
      result.status.api_session_report_available,
      true,
      'dashboard control status did not advertise session report downloads'
    );
    assert.strictEqual(
      result.status.byte_streams_available,
      true,
      'dashboard control status did not advertise byte streams'
    );
    assert.strictEqual(
      result.status.upload_frames_available,
      true,
      'dashboard control status did not advertise upload frames'
    );
    assert.strictEqual(
      result.status.terminal_frames_available,
      true,
      'dashboard control status did not advertise terminal frames'
    );
    assert.strictEqual(
      result.status.api_session_current_uploads_available,
      true,
      'dashboard control status did not advertise current upload lists'
    );
    assert.strictEqual(
      result.status.api_session_current_upload_available,
      true,
      'dashboard control status did not advertise current upload frames'
    );
    assert.strictEqual(
      result.status.api_session_current_upload_raw_available,
      true,
      'dashboard control status did not advertise upload raw byte streams'
    );
    assert.strictEqual(
      result.status.api_recording_asset_available,
      true,
      'dashboard control status did not advertise recording asset byte streams'
    );
    assert.strictEqual(
      result.status.api_session_frame_asset_available,
      true,
      'dashboard control status did not advertise session frame asset byte streams'
    );
    assert.strictEqual(
      result.status.api_dashboard_action_msg_available,
      true,
      'dashboard control status did not advertise dashboard action messages'
    );
    assert.strictEqual(
      result.status.api_diagnostics_visual_freshness_available,
      true,
      'dashboard control status did not advertise diagnostics visual freshness uploads'
    );
    assert.strictEqual(
      result.status.api_peer_mutations_available,
      true,
      'dashboard control status did not advertise peer mutations'
    );
    assert.strictEqual(
      result.status.api_peer_pairing_available,
      true,
      'dashboard control status did not advertise peer pairing'
    );
    assert.strictEqual(
      result.status.api_peer_webrtc_signal_available,
      true,
      'dashboard control status did not advertise peer WebRTC signaling'
    );
    assert.strictEqual(
      result.status.api_agent_card_available,
      true,
      'dashboard control status did not advertise agent card'
    );
    assert.strictEqual(
      result.status.api_cached_bootstrap_events_available,
      true,
      'dashboard control status did not advertise cached bootstrap events'
    );
    assert.strictEqual(
      result.status.api_browser_workspace_snapshot_available,
      true,
      'dashboard control status did not advertise browser workspace snapshots'
    );
    assert.strictEqual(
      result.status.api_state_snapshot_available,
      true,
      'dashboard control status did not advertise state snapshots'
    );
    assert.strictEqual(
      result.status.api_display_bootstrap_available,
      true,
      'dashboard control status did not advertise display bootstrap'
    );
    assert.strictEqual(
      result.status.api_display_input_authority_available,
      true,
      'dashboard control status did not advertise display input authority'
    );
    assert.strictEqual(
      result.status.api_session_log_replay_available,
      true,
      'dashboard control status did not advertise session log replay'
    );
    assert.strictEqual(
      result.status.api_external_session_activity_replay_available,
      true,
      'dashboard control status did not advertise external session activity replay'
    );
    assert.strictEqual(
      result.status.api_dashboard_bootstrap_available,
      true,
      'dashboard control status did not advertise dashboard bootstrap'
    );
    assert(result.config && typeof result.config === 'object', 'config RPC did not return an object');
    assert(result.agentCard && result.agentCard.id, 'api_agent_card did not return an id');
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
    assert(Array.isArray(result.sessionsById), 'api_sessions ids did not return an array');
    if (result.sessionsByIdTarget) {
      assert(
        result.sessionsById.some(session => session?.session_id === result.sessionsByIdTarget),
        'api_sessions ids did not return the requested session'
      );
    }
    assert(result.sessionsStream.eventTypes.includes('start'), 'api_sessions_stream missed start event');
    assert(result.sessionsStream.eventTypes.includes('replace'), 'api_sessions_stream missed replace event');
    assert(result.sessionsStream.eventTypes.includes('done'), 'api_sessions_stream missed done event');
    assert(
      result.sessionsStream.result && result.sessionsStream.result.events >= result.sessionsStream.eventCount,
      'api_sessions_stream did not report completed events'
    );
    assert(result.largeSessionsStream.eventTypes.includes('replace'), 'large api_sessions_stream missed replace event');
    assert(
      result.largeSessionsStream.replaceBytes > 65536,
      `large api_sessions_stream replace event did not cross chunk threshold: ${result.largeSessionsStream.replaceBytes}`
    );
    assert(
      result.largeSessionsStream.completedChunkedResponsesAfter > result.largeSessionsStream.completedChunkedResponsesBefore,
      'chunked stream event counter did not advance'
    );
    assert(
      result.largeSessionsStream.result && result.largeSessionsStream.result.events >= result.largeSessionsStream.eventCount,
      'large api_sessions_stream did not report completed events'
    );
    assert(result.largeSessions.ok, 'large api_sessions did not return an array');
    assert(
      result.largeSessions.jsonBytes > 65536,
      `large api_sessions did not cross chunk threshold: ${result.largeSessions.jsonBytes}`
    );
    assert.strictEqual(
      result.status.api_session_current_agent_output_available,
      true,
      'dashboard control status did not advertise current agent output'
    );
    assert.strictEqual(
      result.status.api_session_delete_available,
      true,
      'dashboard control status did not advertise session deletion'
    );
    assert(
      result.sessionDelete?.invalidSession?.ok === false &&
        result.sessionDelete.invalidSession.error === 'invalid session id',
      'session delete RPC did not preserve invalid session body'
    );
    assert.strictEqual(result.sessionControl?.interrupt?.ok, true);
    assert.strictEqual(result.sessionControl?.interrupt?.action, 'interrupt');
    assert.strictEqual(
      result.sessionControl?.rejectedSettingsAction?._httpStatus,
      400,
      'session control allowlist rejection did not preserve endpoint status'
    );
    assert(
      String(result.sessionControl?.rejectedSettingsAction?.error || '').includes('not available over dashboard session WebRTC'),
      `unexpected session-control rejection: ${JSON.stringify(result.sessionControl?.rejectedSettingsAction)}`
    );
    assert.strictEqual(result.dashboardAction?.closeWorkspace?.ok, true);
    assert.strictEqual(result.dashboardAction?.closeWorkspace?.action, 'close_browser_workspace');
    assert.strictEqual(result.dashboardAction?.diagnosticsVisualMarker?.ok, true);
    assert.strictEqual(
      result.dashboardAction?.diagnosticsVisualMarker?.action,
      'set_diagnostics_visual_marker'
    );
    assert.strictEqual(result.dashboardAction?.diagnosticsVisualMarker?.display_id, 0);
    assert.strictEqual(
      typeof result.dashboardAction?.diagnosticsVisualMarker?.registry_available,
      'boolean'
    );
    assert.strictEqual(
      typeof result.dashboardAction?.diagnosticsVisualMarker?.active_display_updated,
      'boolean'
    );
    assert.strictEqual(
      result.dashboardAction?.rejectedSettingsAction?._httpStatus,
      400,
      'dashboard action allowlist rejection did not preserve endpoint status'
    );
    assert(
      String(result.dashboardAction?.rejectedSettingsAction?.error || '').includes('not available over dashboard action WebRTC'),
      `unexpected dashboard-action rejection: ${JSON.stringify(result.dashboardAction?.rejectedSettingsAction)}`
    );
    assert.strictEqual(
      result.peerWebRtcSignal?._httpStatus,
      404,
      'peer WebRTC signaling did not preserve missing-peer status'
    );
    assert.strictEqual(result.peerWebRtcSignal?._httpOk, false);
    assert.strictEqual(result.peerWebRtcSignal?.error, 'peer not found');
    assert(Array.isArray(result.peerPairing?.requests?.requests), 'peer pairing requests RPC did not return an array');
    assert(Array.isArray(result.peerPairing?.identities?.identities), 'peer pairing identities RPC did not return an array');
    assert.strictEqual(result.peerPairing?.missingDecision?._httpStatus, 400);
    assert.strictEqual(result.peerPairing?.missingDecision?._httpOk, false);
    assert.strictEqual(result.peerPairing?.missingRevokeIdentity?._httpStatus, 400);
    assert.strictEqual(result.peerPairing?.missingRevokeIdentity?._httpOk, false);
    if (result.sessionReport?.ok === true) {
      assert.strictEqual(result.sessionReport.content_type, 'application/zip');
      assert(String(result.sessionReport.filename || '').endsWith('.zip'), 'session report filename was not a zip');
      assert(Number(result.sessionReport.size || 0) > 0, 'session report had no bytes');
      assert(Number(result.sessionReport.byteLength || 0) > 0, 'session report had no byte-stream body');
      assert.strictEqual(result.sessionReport.byteLength, result.sessionReport.size);
    } else {
      assert.strictEqual(
        result.sessionReport?._httpStatus,
        404,
        'idle current session report should preserve 404 status'
      );
      assert.strictEqual(result.sessionReport?._httpOk, false);
    }
    assert.strictEqual(result.upload?._httpStatus, 200);
    assert.strictEqual(result.upload?._httpOk, true);
    assert.strictEqual(result.upload?.name, 'dashboard-upload-rendezvous.txt');
    assert.strictEqual(result.upload?.mime, 'text/plain');
    assert.strictEqual(result.upload?.size, 'dashboard upload e2e rendezvous'.length);
    assert(Array.isArray(result.uploadList), 'api_session_current_uploads did not return an array');
    assert(
      result.uploadList.some(upload => upload.id === result.upload.id),
      `api_session_current_uploads did not include the uploaded descriptor: ${JSON.stringify(result.uploadList)}`
    );
    assert.strictEqual(result.uploadRaw?.ok, true);
    assert.strictEqual(result.uploadRaw?.byteLength, 6);
    assert.strictEqual(result.uploadRaw?.text, 'upload');
    assert.strictEqual(result.uploadRaw?.total_size, 'dashboard upload e2e rendezvous'.length);
    assert.strictEqual(result.uploadRaw?.range_start, 10);
    assert.strictEqual(result.uploadRaw?.range_end, 16);
    assert.strictEqual(result.uploadRaw?.resumable, true);
    if (result.imagePreview?.skipped) {
      assert.strictEqual(result.imagePreview.reason, 'upload file input unavailable on rendezvous emulator');
    } else {
      assert.strictEqual(result.imagePreview?.ok, true);
      assert.strictEqual(result.imagePreview?.previewScheme, 'blob');
      assert(result.imagePreview?.byteStreamDelta >= 1, `image preview did not use a byte stream: ${JSON.stringify(result.imagePreview)}`);
    }
    assert.strictEqual(result.recordingAsset?.ok, true);
    assert.strictEqual(result.recordingAsset?.byteLength, 7);
    assert.strictEqual(result.recordingAsset?.text, 'segment');
    assert.strictEqual(result.recordingAsset?.content_type, 'video/mp4');
    assert.strictEqual(result.recordingAsset?.range_start, 10);
    assert.strictEqual(result.recordingAsset?.range_end, 17);
    assert.strictEqual(result.recordingAsset?.resumable, true);
    assert.strictEqual(result.recordingHlsAssets?.playlist?.ok, true);
    assert.strictEqual(result.recordingHlsAssets?.playlist?.content_type, 'application/vnd.apple.mpegurl');
    assert(String(result.recordingHlsAssets?.playlist?.text || '').includes('seg_00000.ts'), 'HLS playlist did not include the TS segment');
    assert.strictEqual(result.recordingHlsAssets?.segment?.ok, true);
    assert.strictEqual(result.recordingHlsAssets?.segment?.byteLength, 9);
    assert.strictEqual(result.recordingHlsAssets?.segment?.text, 'hls trans');
    assert.strictEqual(result.recordingHlsAssets?.segment?.content_type, 'video/mp2t');
    assert.strictEqual(result.recordingHlsAssets?.segment?.range_start, 10);
    assert.strictEqual(result.recordingHlsAssets?.segment?.range_end, 19);
    assert.strictEqual(result.recordingHlsAssets?.segment?.resumable, true);
    assert.strictEqual(result.sessionFrameAsset?.ok, true);
    assert.strictEqual(result.sessionFrameAsset?.content_type, 'image/png');
    assert.strictEqual(result.sessionFrameAsset?.filename, sessionFrameFixture.filename);
    assert.strictEqual(result.sessionFrameAsset?.session_id, sessionFrameFixture.sessionId);
    assert.strictEqual(result.sessionFrameAsset?.byteLength, 8);
    assert.deepStrictEqual(result.sessionFrameAsset?.firstBytes, [137, 80, 78, 71, 13, 10, 26, 10]);
    assert.strictEqual(result.sessionFrameAsset?.range_start, 0);
    assert.strictEqual(result.sessionFrameAsset?.range_end, 8);
    assert.strictEqual(result.sessionFrameAsset?.resumable, true);
    if (result.recordingFallbackPlayback?.skipped) {
      assert.strictEqual(result.recordingFallbackPlayback.reason, 'RecordingPlayer unavailable on rendezvous emulator');
    } else {
      assert.strictEqual(result.recordingFallbackPlayback?.srcScheme, 'blob');
      assert.strictEqual(result.recordingFallbackPlayback?.objectUrl, true);
      assert(result.recordingFallbackPlayback?.byteStreamDelta >= 1, `recording fallback playback did not use a byte stream: ${JSON.stringify(result.recordingFallbackPlayback)}`);
    }
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
    assert.strictEqual(
      result.status.api_session_current_history_available,
      true,
      'dashboard control status did not advertise current session history'
    );
    assert.strictEqual(
      result.status.api_session_current_rollback_available,
      true,
      'dashboard control status did not advertise current session rollback'
    );
    assert.strictEqual(
      result.status.api_session_current_redo_available,
      true,
      'dashboard control status did not advertise current session redo'
    );
    assert.strictEqual(
      result.status.api_session_current_prune_available,
      true,
      'dashboard control status did not advertise current session prune'
    );
    assert(
      result.agentOutput && (
        result.agentOutput._httpStatus === 404 ||
        Array.isArray(result.agentOutput.missing)
      ),
      'agent output RPC did not preserve endpoint result metadata'
    );
    assert(
      result.timeline?.history && (
        result.timeline.history._httpStatus === 200 ||
        result.timeline.history._httpStatus === 503
      ),
      'timeline history RPC did not preserve endpoint status'
    );
    assert(
      result.timeline?.rollbackValidation && (
        result.timeline.rollbackValidation._httpStatus === 400 ||
        result.timeline.rollbackValidation._httpStatus === 503
      ),
      'timeline rollback validation RPC did not preserve endpoint status'
    );
    assert.strictEqual(
      result.status.api_session_current_changes_available,
      true,
      'dashboard control status did not advertise current session changes'
    );
    assert(
      result.changes?.list && (
        Array.isArray(result.changes.list) ||
        result.changes.list._httpStatus === 200 ||
        result.changes.list._httpStatus === 503
      ),
      'changes list RPC did not preserve endpoint status'
    );
    assert(
      result.changes?.traversal && (
        result.changes.traversal._httpStatus === 400 ||
        result.changes.traversal._httpStatus === 503
      ),
      'changes path validation RPC did not preserve endpoint status'
    );
    assert.strictEqual(
      result.status.api_session_context_snapshot_available,
      true,
      'dashboard control status did not advertise context snapshots'
    );
    assert(
      result.contextSnapshots?.missingSelector?._httpStatus === 400,
      'context snapshot RPC did not preserve missing selector status'
    );
    assert(
      result.contextSnapshots?.invalidSession?._httpStatus === 400,
      'context snapshot RPC did not preserve invalid session status'
    );
    assert.strictEqual(
      result.status.api_session_current_upload_delete_available,
      true,
      'dashboard control status did not advertise upload deletion'
    );
    assert.strictEqual(
      result.status.api_voice_session_available,
      true,
      'dashboard control status did not advertise voice session token minting'
    );
    assert(
      result.uploads?.missingDeleteId?._httpStatus === 400,
      'upload delete RPC did not preserve missing id status'
    );
    assert.strictEqual(
      result.status.api_mcp_tool_call_available,
      true,
      'dashboard control status did not advertise MCP tool calls'
    );
    assert(
      result.mcp?.missingSession?._httpStatus === 400 &&
        result.mcp.missingSession.error?.code === -32602,
      'MCP tool-call RPC did not preserve validation error metadata'
    );
    assert.strictEqual(
      result.status.api_recordings_available,
      true,
      'dashboard control status did not advertise recordings'
    );
    assert.strictEqual(
      result.status.api_session_recordings_available,
      true,
      'dashboard control status did not advertise session recordings'
    );
    assert(
      Array.isArray(result.recordings?.live),
      'recordings RPC did not return a stream array'
    );
    assert(
      result.recordings?.invalidSession?._httpStatus === 400,
      'session recordings RPC did not preserve invalid id status'
    );
    assert.strictEqual(
      result.status.api_worktrees_available,
      true,
      'dashboard control status did not advertise worktrees'
    );
    assert.strictEqual(
      result.status.api_worktrees_scan_available,
      true,
      'dashboard control status did not advertise worktree scan'
    );
    assert.strictEqual(
      result.status.api_worktrees_remove_available,
      true,
      'dashboard control status did not advertise worktree remove'
    );
    assert(
      result.worktrees?.cached && typeof result.worktrees.cached === 'object',
      'worktree cached RPC did not return an inventory object'
    );
    assert(
      result.worktrees?.scan && typeof result.worktrees.scan === 'object',
      'worktree scan RPC did not return an inventory object'
    );
    assert(
      result.worktrees?.invalidRemove?._httpStatus === 400,
      'worktree remove RPC did not preserve invalid request status'
    );
    assert.strictEqual(
      result.status.api_fs_stat_available,
      true,
      'dashboard control status did not advertise filesystem stat'
    );
    assert.strictEqual(
      result.status.api_fs_list_available,
      true,
      'dashboard control status did not advertise filesystem list'
    );
    assert.strictEqual(
      result.status.api_fs_mkdir_available,
      true,
      'dashboard control status did not advertise filesystem mkdir'
    );
    assert.strictEqual(
      result.status.api_fs_read_available,
      true,
      'dashboard control status did not advertise filesystem read'
    );
    assert(
      result.filesystem?.statHome &&
        result.filesystem.statHome._httpStatus === 200 &&
        result.filesystem.statHome.exists === true &&
        result.filesystem.statHome.is_dir === true,
      'filesystem stat RPC did not return home directory status'
    );
    assert(
      result.filesystem?.listHome &&
        result.filesystem.listHome._httpStatus === 200 &&
        Array.isArray(result.filesystem.listHome.entries),
      'filesystem list RPC did not return home directory entries'
    );
    assert(
      result.filesystem?.badRelative &&
        result.filesystem.badRelative._httpStatus === 400 &&
        result.filesystem.badRelative._httpOk === false,
      'filesystem stat RPC did not preserve bad path status'
    );
    assert(
      result.filesystem?.mkdirHome &&
        result.filesystem.mkdirHome._httpStatus === 200 &&
        result.filesystem.mkdirHome.ok === true,
      'filesystem mkdir RPC did not return existing-home status'
    );
    assert(
      result.filesystem?.mkdirBadRelative &&
        result.filesystem.mkdirBadRelative._httpStatus === 400 &&
        result.filesystem.mkdirBadRelative._httpOk === false,
      'filesystem mkdir RPC did not preserve bad path status'
    );
    assert.strictEqual(result.filesystemRead?.ok, true);
    assert.strictEqual(result.filesystemRead?.byteLength, 10);
    assert.strictEqual(result.filesystemRead?.text, 'filesystem');
    assert.strictEqual(result.filesystemRead?.content_type, 'text/plain; charset=utf-8');
    assert.strictEqual(result.filesystemRead?.range_start, 10);
    assert.strictEqual(result.filesystemRead?.range_end, 20);
    assert.strictEqual(result.filesystemRead?.total_size, filesystemFixture.text.length);
    assert.strictEqual(result.filesystemRead?.resumable, true);
    assert.strictEqual(result.finalStatus.apiSessionDetailAvailable, result.status.api_session_detail_available);
    assert.strictEqual(result.finalStatus.apiSessionReportAvailable, result.status.api_session_report_available);
    assert.strictEqual(result.finalStatus.apiSessionDeleteAvailable, result.status.api_session_delete_available);
    assert.strictEqual(result.finalStatus.apiSessionCurrentAgentOutputAvailable, result.status.api_session_current_agent_output_available);
    assert.strictEqual(result.finalStatus.apiSessionCurrentHistoryAvailable, result.status.api_session_current_history_available);
    assert.strictEqual(result.finalStatus.apiSessionCurrentRollbackAvailable, result.status.api_session_current_rollback_available);
    assert.strictEqual(result.finalStatus.apiSessionCurrentRedoAvailable, result.status.api_session_current_redo_available);
    assert.strictEqual(result.finalStatus.apiSessionCurrentPruneAvailable, result.status.api_session_current_prune_available);
    assert.strictEqual(result.finalStatus.apiSessionCurrentChangesAvailable, result.status.api_session_current_changes_available);
    assert.strictEqual(result.finalStatus.apiSessionContextSnapshotAvailable, result.status.api_session_context_snapshot_available);
    assert.strictEqual(result.finalStatus.apiSessionCurrentUploadsAvailable, result.status.api_session_current_uploads_available);
    assert.strictEqual(result.finalStatus.apiSessionCurrentUploadAvailable, result.status.api_session_current_upload_available);
    assert.strictEqual(result.finalStatus.apiSessionCurrentUploadRawAvailable, result.status.api_session_current_upload_raw_available);
    assert.strictEqual(result.finalStatus.apiSessionCurrentUploadDeleteAvailable, result.status.api_session_current_upload_delete_available);
    assert.strictEqual(result.finalStatus.apiFsStatAvailable, result.status.api_fs_stat_available);
    assert.strictEqual(result.finalStatus.apiFsListAvailable, result.status.api_fs_list_available);
    assert.strictEqual(result.finalStatus.apiFsMkdirAvailable, result.status.api_fs_mkdir_available);
    assert.strictEqual(result.finalStatus.apiFsReadAvailable, result.status.api_fs_read_available);
    assert.strictEqual(result.finalStatus.apiSettingsAvailable, result.status.api_settings_available);
    assert.strictEqual(result.finalStatus.apiSettingsSaveAvailable, result.status.api_settings_save_available);
    assert.strictEqual(result.finalStatus.apiKeyStatusAvailable, result.status.api_key_status_available);
    assert.strictEqual(result.finalStatus.apiApiKeysSaveAvailable, result.status.api_api_keys_save_available);
    assert.strictEqual(result.finalStatus.apiVoiceSessionAvailable, result.status.api_voice_session_available);
    assert.strictEqual(result.finalStatus.apiProjectRootAvailable, result.status.api_project_root_available);
    assert.strictEqual(result.finalStatus.apiDisplaysAvailable, result.status.api_displays_available);
    assert.strictEqual(result.finalStatus.apiCoordinatorAvailable, result.status.api_coordinator_available);
    assert(result.appError && result.appError._httpStatus === 400, 'application error metadata was not preserved');
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
      publicOrigin,
      daemonPort: options.daemonPort,
      daemonId: options.daemonId,
      certlessConfigStatus,
      connected,
      rpc: {
        controlSessionId: result.status.session_id,
        responseCredit: result.status.response_credit_enabled,
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
        sessionByIdCount: result.sessionsById.length,
        sessionDeleteInvalidOk: result.sessionDelete.invalidSession?.ok,
        sessionControlAction: result.sessionControl.interrupt?.action,
        rejectedSessionControlStatus: result.sessionControl.rejectedSettingsAction?._httpStatus,
        apiDashboardActionMsgAvailable: result.status.api_dashboard_action_msg_available,
        apiDiagnosticsVisualFreshnessAvailable: result.status.api_diagnostics_visual_freshness_available,
        apiPeerMutationsAvailable: result.status.api_peer_mutations_available,
        apiPeerPairingAvailable: result.status.api_peer_pairing_available,
        apiPeerWebRtcSignalAvailable: result.status.api_peer_webrtc_signal_available,
        dashboardActionAction: result.dashboardAction.closeWorkspace?.action,
        diagnosticsMarkerRegistryAvailable: result.dashboardAction.diagnosticsVisualMarker?.registry_available,
        diagnosticsMarkerActiveDisplayUpdated: result.dashboardAction.diagnosticsVisualMarker?.active_display_updated,
        rejectedDashboardActionStatus: result.dashboardAction.rejectedSettingsAction?._httpStatus,
        peerPairingRequestCount: result.peerPairing.requests.requests.length,
        peerPairingIdentityCount: result.peerPairing.identities.identities.length,
        peerPairingMissingDecisionStatus: result.peerPairing.missingDecision._httpStatus,
        peerPairingMissingRevokeStatus: result.peerPairing.missingRevokeIdentity._httpStatus,
        peerWebRtcSignalStatus: result.peerWebRtcSignal?._httpStatus,
        apiSessionReportAvailable: result.status.api_session_report_available,
        byteStreamsAvailable: result.status.byte_streams_available,
        uploadFramesAvailable: result.status.upload_frames_available,
        terminalFramesAvailable: result.status.terminal_frames_available,
        tuiFramesAvailable: result.status.tui_frames_available,
        apiSessionCurrentUploadsAvailable: result.status.api_session_current_uploads_available,
        apiSessionCurrentUploadAvailable: result.status.api_session_current_upload_available,
        apiSessionCurrentUploadRawAvailable: result.status.api_session_current_upload_raw_available,
        apiRecordingAssetAvailable: result.status.api_recording_asset_available,
        apiSessionFrameAssetAvailable: result.status.api_session_frame_asset_available,
        apiSessionDeleteDebugAvailable: result.finalStatus.apiSessionDeleteAvailable,
        apiSessionContextSnapshotDebugAvailable: result.finalStatus.apiSessionContextSnapshotAvailable,
        apiSessionCurrentUploadDeleteDebugAvailable: result.finalStatus.apiSessionCurrentUploadDeleteAvailable,
        apiFsStatDebugAvailable: result.finalStatus.apiFsStatAvailable,
        apiFsListDebugAvailable: result.finalStatus.apiFsListAvailable,
        apiFsMkdirDebugAvailable: result.finalStatus.apiFsMkdirAvailable,
        apiFsReadAvailable: result.status.api_fs_read_available,
        apiSettingsSaveDebugAvailable: result.finalStatus.apiSettingsSaveAvailable,
        apiApiKeysSaveDebugAvailable: result.finalStatus.apiApiKeysSaveAvailable,
        apiVoiceSessionDebugAvailable: result.finalStatus.apiVoiceSessionAvailable,
        apiCoordinatorDebugAvailable: result.finalStatus.apiCoordinatorAvailable,
        uploadStatus: result.upload._httpStatus,
        uploadListCount: result.uploadList.length,
        uploadSize: result.upload.size,
        uploadRawBytes: result.uploadRaw.byteLength,
        uploadRawText: result.uploadRaw.text,
        imagePreviewScheme: result.imagePreview.previewScheme || null,
        imagePreviewByteStreamDelta: result.imagePreview.byteStreamDelta || 0,
        imagePreviewSkipped: Boolean(result.imagePreview.skipped),
        recordingAssetBytes: result.recordingAsset.byteLength,
        recordingAssetText: result.recordingAsset.text,
        recordingHlsPlaylistBytes: result.recordingHlsAssets.playlist.byteLength,
        recordingHlsSegmentBytes: result.recordingHlsAssets.segment.byteLength,
        sessionFrameAssetBytes: result.sessionFrameAsset.byteLength,
        sessionFrameAssetSignature: result.sessionFrameAsset.firstBytes,
        filesystemReadBytes: result.filesystemRead.byteLength,
        filesystemReadText: result.filesystemRead.text,
        recordingFallbackSrcScheme: result.recordingFallbackPlayback.srcScheme || null,
        recordingFallbackByteStreamDelta: result.recordingFallbackPlayback.byteStreamDelta || 0,
        recordingFallbackSkipped: Boolean(result.recordingFallbackPlayback.skipped),
        diagnosticsVisualFreshnessWritten: result.diagnosticsVisualFreshness.written,
        terminalOutputBytes: result.terminal.outputBytes,
        tuiFrameBytes: result.tui.frameBytes,
        sessionReportStatus: result.sessionReport._httpStatus || 200,
        sessionReportSize: result.sessionReport.byteLength || result.sessionReport.size || 0,
        streamEventCount: result.sessionsStream.eventCount,
        streamReplaceCount: result.sessionsStream.replaceCount,
        largeStreamEventCount: result.largeSessionsStream.eventCount,
        largeStreamReplaceCount: result.largeSessionsStream.replaceCount,
        largeStreamReplaceBytes: result.largeSessionsStream.replaceBytes,
        largeSessionCount: result.largeSessions.length,
        largeSessionBytes: result.largeSessions.jsonBytes,
        completedChunkedResponses: result.finalStatus.completedChunkedResponses,
        agentOutputStatus: result.agentOutput?._httpStatus || 200,
        timelineStatuses: Object.fromEntries(Object.entries(result.timeline || {}).map(([key, value]) => [
          key,
          value?._httpStatus || 200,
        ])),
        changesStatuses: Object.fromEntries(Object.entries(result.changes || {}).map(([key, value]) => [
          key,
          value?._httpStatus || 200,
        ])),
        contextSnapshotStatuses: Object.fromEntries(Object.entries(result.contextSnapshots || {}).map(([key, value]) => [
          key,
          value?._httpStatus || 200,
        ])),
        uploadStatuses: Object.fromEntries(Object.entries(result.uploads || {}).map(([key, value]) => [
          key,
          value?._httpStatus || 200,
        ])),
        voiceSessionAvailable: result.status.api_voice_session_available,
        mcpStatuses: Object.fromEntries(Object.entries(result.mcp || {}).map(([key, value]) => [
          key,
          value?._httpStatus || 200,
        ])),
        recordingsStatuses: Object.fromEntries(Object.entries(result.recordings || {}).map(([key, value]) => [
          key,
          value?._httpStatus || 200,
        ])),
        worktreesStatuses: Object.fromEntries(Object.entries(result.worktrees || {}).map(([key, value]) => [
          key,
          value?._httpStatus || 200,
        ])),
        filesystemStatuses: Object.fromEntries(Object.entries(result.filesystem || {}).map(([key, value]) => [
          key,
          value?._httpStatus || 200,
        ])),
        appErrorStatus: result.appError._httpStatus,
        pendingRequests: result.finalStatus.pendingRequests,
        pendingChunkedResponses: result.finalStatus.pendingChunkedResponses,
      },
    }, null, 2));

  } finally {
    if (browser) await browser.close().catch(() => {});
    if (!daemon.killed) daemon.kill('SIGINT');
    await Promise.race([daemonExit, wait(5000)]);
    await new Promise(resolve => rendezvous.close(resolve));
    removeRecordingFixture(recordingFixture);
    removeRecordingFixture(hlsRecordingFixture);
    removeSessionFrameFixture(sessionFrameFixture);
    removeFilesystemFixture(filesystemFixture);
  }
}

async function fetchJson(url) {
  const resp = await fetch(url);
  if (!resp.ok) throw new Error(`${url} returned ${resp.status}`);
  return resp.json();
}

main()
  .then(() => process.exit(0))
  .catch(err => {
    console.error(err);
    process.exit(1);
  });
