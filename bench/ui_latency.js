'use strict';
// How long does the UI wait for answers while an import saturates the CPU?
// Probes every 10 ms: list(50) [read connection] and set_rating [writer,
// contends with import batch commits]. Idle phase first, then during import.
//   node bench/ui_latency.js <bin> <scratchDir> <photoDir> <threads>
const fs = require('node:fs');
const path = require('node:path');
const { startSidecar } = require('../electron/sidecar');

const [bin, scratch, photoDir, threads] = process.argv.slice(2);
const now = () => process.hrtime.bigint();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const pct = (a, p) => a[Math.min(a.length - 1, Math.floor(a.length * p))];
const summ = (a) => {
  const s = [...a].sort((x, y) => x - y);
  return { n: s.length, p50_ms: +pct(s, 0.5).toFixed(2), p99_ms: +pct(s, 0.99).toFixed(2), max_ms: +s[s.length - 1].toFixed(2) };
};

async function probe(conn, ids, lists, edits) {
  let t0 = now();
  await conn.request('list', { offset: 0, limit: 50 });
  lists.push(Number(now() - t0) / 1e6);
  if (ids.length) {
    t0 = now();
    await conn.request('set_rating', { id: ids[edits.length % ids.length], rating: edits.length % 6 });
    edits.push(Number(now() - t0) / 1e6);
  }
}

(async () => {
  const dataDir = path.join(scratch, `ui-${threads}`);
  fs.rmSync(dataDir, { recursive: true, force: true });
  fs.mkdirSync(dataDir, { recursive: true });
  require('node:child_process').execFileSync(bin, ['seed', '--data', dataDir, '--rows', '500']);
  const { child, conn } = await startSidecar({ bin, dataDir, args: ['--threads', threads] });
  const ids = (await conn.request('list', { limit: 500 })).photos.map((p) => p.id);

  const idle = { list: [], edit: [] };
  for (let i = 0; i < 300; i++) { await probe(conn, ids, idle.list, idle.edit); await sleep(10); }

  const busy = { list: [], edit: [] };
  let done = null;
  conn.on('event', (m) => { if (m.event === 'import_done') done = m.stats; });
  await conn.request('import', { dir: photoDir });
  while (!done) { await probe(conn, ids, busy.list, busy.edit); await sleep(10); }

  const res = {
    threads: Number(threads),
    cpus: require('node:os').cpus().length,
    import: { imported: done.imported, secs: +done.secs.toFixed(2), imgs_per_sec: +(done.imported / done.secs).toFixed(1) },
    idle: { list: summ(idle.list), set_rating: summ(idle.edit) },
    during_import: { list: summ(busy.list), set_rating: summ(busy.edit) },
  };
  await conn.request('shutdown', {});
  await new Promise((r) => child.once('exit', r));
  console.log(JSON.stringify(res, null, 2));
})();
