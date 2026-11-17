'use strict';
// Kill the sidecar with SIGKILL mid-edit, N times. Every edit the sidecar
// acknowledged before dying must be in the catalog after restart.
//
//   node tests/crash.js <bin> <dataDir> [--kills 10] [--unsafe]
//
// An edit that was sent but not yet acknowledged at kill time may land either
// way; the check accepts its old or new value. Concurrent edits never target
// the same photo, so "last acknowledged value" is well defined.

const fs = require('node:fs');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const { startSidecar } = require('../electron/sidecar');

const [bin, dataDir] = process.argv.slice(2);
const kills = Number(process.argv[process.argv.indexOf('--kills') + 1]) || 10;
const unsafe = process.argv.includes('--unsafe');
const PHOTOS = 2000, INFLIGHT = 8;

let seed = 12345;
const rnd = (n) => { seed = (seed * 1103515245 + 12345) & 0x7fffffff; return seed % n; };

async function readAll() {
  const { child, conn } = await startSidecar({ bin, dataDir });
  const r = await conn.request('list', { limit: 100000 });
  child.stdin.end();
  await new Promise((res) => child.once('exit', res));
  return new Map(r.photos.map((p) => [p.id, p.rating]));
}

(async () => {
  fs.rmSync(dataDir, { recursive: true, force: true });
  execFileSync(bin, ['seed', '--data', dataDir, '--rows', String(PHOTOS)], { stdio: 'ignore' });
  const acked = await readAll(); // id -> rating the client believes is stored
  const rounds = [];
  let totalLost = 0, totalAcked = 0;

  for (let k = 0; k < kills; k++) {
    const { child, conn } = await startSidecar({ bin, dataDir, args: unsafe ? ['--unsafe-ack-before-commit'] : [] });
    const inflight = new Map(); // id -> value sent, not yet acked
    let ackedThisRound = 0, dead = false;
    conn.on('closed', () => { dead = true; });

    const worker = async () => {
      while (!dead) {
        let id;
        do { id = 1 + rnd(PHOTOS); } while (inflight.has(id));
        const v = rnd(6);
        inflight.set(id, v);
        try {
          await conn.request('set_rating', { id, rating: v }, { timeoutMs: 0 });
          acked.set(id, v);
          ackedThisRound++;
          inflight.delete(id);
        } catch { return; } // connection died: leave it in inflight
      }
    };
    const workers = Array.from({ length: INFLIGHT }, worker);
    const killAfter = 300 + rnd(1200);
    await new Promise((r) => setTimeout(r, killAfter));
    child.kill('SIGKILL');
    await Promise.all(workers);
    await new Promise((r) => (child.exitCode !== null || child.signalCode ? r() : child.once('exit', r)));

    const stored = await readAll();
    let lost = 0, inflightLanded = 0;
    for (const [id, v] of acked) {
      const s = stored.get(id);
      if (s === v) continue;
      if (inflight.has(id) && inflight.get(id) === s) { inflightLanded++; acked.set(id, s); continue; }
      lost++;
      acked.set(id, s); // resync so the next round measures only its own losses
    }
    totalLost += lost;
    totalAcked += ackedThisRound;
    rounds.push({ round: k + 1, killAfterMs: killAfter, ackedThisRound, inflightAtKill: inflight.size, inflightLanded, lostAcked: lost });
    console.error(JSON.stringify(rounds[rounds.length - 1]));
  }
  const out = { mode: unsafe ? 'ack-before-commit (unsafe control)' : 'ack-after-commit', kills, totalAcked, totalLost, rounds };
  console.log(JSON.stringify(out, null, 2));
  process.exit(!unsafe && totalLost > 0 ? 1 : 0);
})();
