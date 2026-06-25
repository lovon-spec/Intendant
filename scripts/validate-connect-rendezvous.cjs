#!/usr/bin/env node
'use strict';

const assert = require('assert');
const crypto = require('crypto');
const http = require('http');
const path = require('path');
const { spawn } = require('child_process');
const { httpStatus, launchBrowser } = require('./lib/browser-automation.cjs');

const DEFAULT_DAEMON_PORT = 8876;
const DEFAULT_RENDEZVOUS_PORT = 9876;
const DEFAULT_DAEMON_ID = 'connect-e2e-daemon';
const START_TIMEOUT_MS = 30000;
const CONNECT_TIMEOUT_MS = 30000;

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
    completedChunkedResponses: 0,
    seq: 0,
    async start() {
      this.pc = new RTCPeerConnection({});
      this.channel = this.pc.createDataChannel('intendant-dashboard-control', { ordered: true });
      this.channel.onopen = () => {
        this.sendFrame({ t: 'hello', id: this.nextId(), features: ['response_credit'] });
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
      return promise;
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
        completedChunkedResponses: this.completedChunkedResponses,
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

async function waitForBrowserConnect(page) {
  let last = null;
  const deadline = Date.now() + CONNECT_TIMEOUT_MS;
  while (Date.now() < deadline) {
    last = await page.evaluate(() => {
      if (!window.intendantPublicConnectDashboard) return null;
      return window.intendantPublicConnectDashboard.status();
    }).catch(() => null);
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
  throw new Error(`public Connect dashboard did not connect: ${JSON.stringify(last)}`);
}

async function main() {
  const options = parseArgs(process.argv);
  const rendezvous = createRendezvousServer();
  await new Promise((resolve, reject) => {
    rendezvous.once('error', reject);
    rendezvous.listen(options.rendezvousPort, '127.0.0.1', resolve);
  });

  const daemonLogs = [];
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

    browser = await launchBrowser({ headless: true });
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

    const result = await page.evaluate(async () => {
      const ctl = window.intendantPublicConnectDashboard;
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
      return {
        status: await ctl.request('status'),
        config: await ctl.request('config'),
        sessions,
        sessionsById,
        sessionsByIdTarget: firstSessionId,
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
    assert(result.config && typeof result.config === 'object', 'config RPC did not return an object');
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
        sessionCount: result.sessions.length,
        sessionByIdCount: result.sessionsById.length,
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
        appErrorStatus: result.appError._httpStatus,
        pendingRequests: result.finalStatus.pendingRequests,
        pendingChunkedResponses: result.finalStatus.pendingChunkedResponses,
      },
    }, null, 2));

    await page.evaluate(() => window.intendantPublicConnectDashboard.close());
  } finally {
    if (browser) await browser.close().catch(() => {});
    if (!daemon.killed) daemon.kill('SIGINT');
    await Promise.race([daemonExit, wait(5000)]);
    await new Promise(resolve => rendezvous.close(resolve));
  }
}

async function fetchJson(url) {
  const resp = await fetch(url);
  if (!resp.ok) throw new Error(`${url} returned ${resp.status}`);
  return resp.json();
}

main().catch(err => {
  console.error(err);
  process.exit(1);
});
