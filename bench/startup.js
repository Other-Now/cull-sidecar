'use strict';
// Cold-ish start: spawn -> hello reply, and spawn -> first page of 50, for an
// empty catalog and a 100k-photo catalog.  node bench/startup.js <bin> <scratch>
const fs = require('node:fs');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const { startSidecar } = require('../electron/sidecar');

const [bin, scratch] = process.argv.slice(2);
const med = (a) => [...a].sort((x, y) => x - y)[Math.floor(a.length / 2)];

async function measure(dataDir, runs = 21) {
  const hello = [], page = [];
  for (let i = 0; i < runs; i++) {
    const s = await startSidecar({ bin, dataDir });
    hello.push(Number(process.hrtime.bigint() - s.spawnedAt) / 1e6);
    await s.conn.request('list', { offset: 0, limit: 50 });
    page.push(Number(process.hrtime.bigint() - s.spawnedAt) / 1e6);
    await s.conn.request('shutdown', {});
    await new Promise((r) => s.child.once('exit', r));
  }
  return { runs, hello_ms_median: +med(hello).toFixed(2), first_page_ms_median: +med(page).toFixed(2) };
}

(async () => {
  const out = {};
  for (const rows of [0, 100000]) {
    const d = path.join(scratch, `startup-${rows}`);
    fs.rmSync(d, { recursive: true, force: true });
    if (rows) execFileSync(bin, ['seed', '--data', d, '--rows', String(rows)]);
    const db = path.join(d, 'catalog.db');
    out[rows] = { ...(await measure(d)), db_mb: fs.existsSync(db) ? +(fs.statSync(db).size / 1e6).toFixed(1) : 0 };
    console.error(rows, JSON.stringify(out[rows]));
  }
  console.log(JSON.stringify(out, null, 2));
})();
