'use strict';
// IPC round trip per transport, and large-buffer delivery: base64-in-JSON vs
// the shared transfer region. Measured from Node, i.e. what Electron sees.
//   node bench/ipc.js <bin> <scratchDir>
const fs = require('node:fs');
const path = require('node:path');
const { startSidecar, ShmReader } = require('../electron/sidecar');

const [bin, scratch] = process.argv.slice(2);
const now = () => process.hrtime.bigint();
const pct = (a, p) => a[Math.min(a.length - 1, Math.floor(a.length * p))];
function summary(us) {
  const a = [...us].sort((x, y) => x - y);
  return { n: a.length, p50_us: +pct(a, 0.5).toFixed(1), p99_us: +pct(a, 0.99).toFixed(1), p999_us: +pct(a, 0.999).toFixed(1) };
}

async function pingBench(transport) {
  const dataDir = path.join(scratch, `ipc-${transport}`);
  fs.rmSync(dataDir, { recursive: true, force: true });
  const { child, conn } = await startSidecar({ bin, dataDir, transport, token: transport === 'stdio' ? undefined : 'bench-token' });
  for (let i = 0; i < 2000; i++) await conn.request('ping', { payload: i }, { timeoutMs: 0 });
  const us = [];
  for (let i = 0; i < 20000; i++) {
    const t0 = now();
    await conn.request('ping', { payload: i }, { timeoutMs: 0 });
    us.push(Number(now() - t0) / 1000);
  }
  return { child, conn, ping: summary(us) };
}

async function blobBench(conn, shmPath) {
  const shm = new ShmReader(shmPath);
  const out = [];
  for (const len of [64 << 10, 1 << 20, 8 << 20, 24 << 20]) {
    for (const via of ['base64', 'shm']) {
      const reps = len >= 8 << 20 ? 30 : 200;
      const ms = [];
      for (let i = 0; i < reps + 5; i++) {
        const t0 = now();
        const r = await conn.request('bench_blob', { len, via }, { timeoutMs: 0 });
        const buf = via === 'base64' ? Buffer.from(r.data, 'base64') : shm.read(r);
        if (buf.length !== len || buf[len - 1] !== (((len - 1) * 31 + 7) & 0xff)) throw new Error('corrupt blob');
        if (i >= 5) ms.push(Number(now() - t0) / 1e6);
      }
      ms.sort((a, b) => a - b);
      out.push({ bytes: len, via, p50_ms: +pct(ms, 0.5).toFixed(3), p99_ms: +pct(ms, 0.99).toFixed(3) });
    }
  }
  shm.close();
  return out;
}

(async () => {
  const res = { node: process.version, transports: {} };
  const order = (process.argv[4] || 'stdio,uds,tcp').split(',');
  for (const t of order) {
    const { child, conn, ping } = await pingBench(t);
    res.transports[t] = ping;
    if (t === 'stdio' && !process.env.NO_BLOBS) {
      const hello = await conn.request('hello', { protocol: 1 });
      res.blobs = await blobBench(conn, hello.shm);
    }
    await conn.request('shutdown', {});
    await new Promise((r) => child.once('exit', r));
    console.error(t, JSON.stringify(ping));
  }
  console.log(JSON.stringify(res, null, 2));
})();
