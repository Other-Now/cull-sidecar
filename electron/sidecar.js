'use strict';
// Client + supervisor for the Rust sidecar. Plain Node: used by the Electron
// main process and by the benchmarks/tests (which run without Electron).

const { spawn } = require('node:child_process');
const net = require('node:net');
const fs = require('node:fs');
const readline = require('node:readline');
const { EventEmitter } = require('node:events');

const PROTOCOL_VERSION = 1;

class Connection extends EventEmitter {
  constructor(readable, writable) {
    super();
    this.w = writable;
    this.nextId = 1;
    this.pending = new Map();
    this.closed = false;
    readline.createInterface({ input: readable, crlfDelay: Infinity }).on('line', (l) => this._line(l));
    readable.on('close', () => this._fail(new Error('connection closed')));
    readable.on('error', (e) => this._fail(e));
    writable.on('error', (e) => this._fail(e));
  }

  _line(line) {
    let m;
    try { m = JSON.parse(line); } catch { this.emit('garbage', line); return; }
    if (m.event) { this.emit('event', m); return; }
    const p = this.pending.get(m.id);
    if (!p) return;
    this.pending.delete(m.id);
    if (p.timer) clearTimeout(p.timer);
    if (m.error) {
      const e = new Error(m.error.message);
      e.code = m.error.code;
      p.reject(e);
    } else p.resolve(m.result);
  }

  request(method, params = {}, { timeoutMs = 30000 } = {}) {
    if (this.closed) return Promise.reject(new Error('connection closed'));
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = timeoutMs
        ? setTimeout(() => { this.pending.delete(id); reject(Object.assign(new Error(`${method} timed out`), { code: 'timeout' })); }, timeoutMs)
        : null;
      this.pending.set(id, { resolve, reject, timer });
      this.w.write(JSON.stringify({ id, method, params }) + '\n');
    });
  }

  _fail(err) {
    if (this.closed) return;
    this.closed = true;
    for (const p of this.pending.values()) { if (p.timer) clearTimeout(p.timer); p.reject(err); }
    this.pending.clear();
    this.emit('closed', err);
  }
}

/** Read a buffer the sidecar placed in its shared transfer region. */
class ShmReader {
  constructor(path) { this.fd = fs.openSync(path, 'r'); }
  read({ offset, len }) {
    const b = Buffer.allocUnsafe(len);
    fs.readSync(this.fd, b, 0, len, offset);
    return b;
  }
  close() { fs.closeSync(this.fd); }
}

function waitReady(child) {
  return new Promise((resolve, reject) => {
    const rl = readline.createInterface({ input: child.stdout });
    rl.once('line', (l) => {
      rl.close();
      try { resolve(JSON.parse(l)); } catch (e) { reject(e); }
    });
    child.once('exit', (c) => reject(new Error(`sidecar exited (${c}) before ready`)));
  });
}

/**
 * Spawn the sidecar and complete the hello handshake.
 * transport: 'stdio' | 'uds' | 'tcp'
 */
async function startSidecar({ bin, dataDir, transport = 'stdio', args = [], env = {}, token } = {}) {
  const argv = ['serve', '--data', dataDir, '--transport', transport, ...args];
  const child = spawn(bin, argv, {
    stdio: ['pipe', 'pipe', 'inherit'],
    env: { ...process.env, ...env, ...(token ? { CULL_TOKEN: token } : {}) },
  });
  const spawnedAt = process.hrtime.bigint();
  let conn;
  if (transport === 'stdio') {
    conn = new Connection(child.stdout, child.stdin);
  } else {
    const ready = await waitReady(child);
    const sock = transport === 'tcp'
      ? net.connect({ host: '127.0.0.1', port: Number(ready.addr.split(':')[1]) })
      : net.connect(ready.addr);
    sock.setNoDelay?.(true);
    await new Promise((res, rej) => { sock.once('connect', res); sock.once('error', rej); });
    conn = new Connection(sock, sock);
  }
  const hello = await conn.request('hello', { protocol: PROTOCOL_VERSION, token });
  return { child, conn, hello, spawnedAt };
}

/**
 * Keeps one sidecar alive for the app: health-checks it, restarts it with
 * backoff when it dies or hangs, and shuts it down cleanly on quit.
 */
class Supervisor extends EventEmitter {
  constructor(opts) {
    super();
    this.opts = { healthMs: 2000, healthTimeoutMs: 1500, maxBackoffMs: 5000, ...opts };
    this.cur = null;
    this.stopping = false;
    this.backoff = 200;
    this.restarts = 0;
  }

  async start() {
    const s = await startSidecar(this.opts);
    this.cur = s;
    this.backoff = 200;
    s.conn.on('event', (m) => this.emit('event', m));
    s.child.once('exit', (code, sig) => this._exited(s, code, sig));
    clearInterval(this.health);
    let misses = 0;
    this.health = setInterval(async () => {
      if (this.cur !== s) return;
      try {
        await s.conn.request('ping', {}, { timeoutMs: this.opts.healthTimeoutMs });
        misses = 0;
      } catch {
        if (++misses >= 2) { this.emit('unhealthy', s.hello.pid); s.child.kill('SIGKILL'); }
      }
    }, this.opts.healthMs);
    this.emit('ready', s.hello);
    return s.hello;
  }

  _exited(s, code, sig) {
    if (this.cur !== s) return;
    this.cur = null;
    this.emit('exit', { code, sig });
    this._scheduleRestart();
  }

  _scheduleRestart() {
    if (this.stopping) return;
    const wait = this.backoff;
    this.backoff = Math.min(this.backoff * 2, this.opts.maxBackoffMs);
    setTimeout(() => {
      if (this.stopping) return;
      this.restarts++;
      this.start().then(() => this.emit('restarted', this.restarts)).catch((e) => {
        this.emit('start-failed', e);
        this._scheduleRestart(); // keep trying, backing off
      });
    }, wait);
  }

  request(method, params, o) {
    if (!this.cur) return Promise.reject(Object.assign(new Error('sidecar not running'), { code: 'unavailable' }));
    return this.cur.conn.request(method, params, o);
  }

  /** Ask politely, then insist. Resolves with how the child exited. */
  async stop({ graceMs = 3000 } = {}) {
    this.stopping = true;
    clearInterval(this.health);
    const s = this.cur;
    if (!s) return { code: null, how: 'not-running' };
    const exited = new Promise((res) => s.child.once('exit', (code, sig) => res({ code, sig })));
    try { await s.conn.request('shutdown', {}, { timeoutMs: 1000 }); } catch { /* already dying */ }
    s.child.stdin.end();
    const r = await Promise.race([exited, new Promise((res) => setTimeout(() => res(null), graceMs))]);
    if (r) return { ...r, how: 'graceful' };
    s.child.kill('SIGKILL');
    return { ...(await exited), how: 'killed' };
  }
}

module.exports = { PROTOCOL_VERSION, Connection, ShmReader, startSidecar, Supervisor };
