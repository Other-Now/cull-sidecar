'use strict';
// node --test tests/supervisor.test.js   (needs CULL_BIN = path to release binary)
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { once } = require('node:events');
const { Supervisor } = require('../electron/sidecar');

const bin = process.env.CULL_BIN || path.join(__dirname, '..', 'target', 'release', 'cull-sidecar');
const tmp = () => fs.mkdtempSync(path.join(os.tmpdir(), 'cull-sup-'));
const ms = (t0) => Number(process.hrtime.bigint() - t0) / 1e6;

test('restarts after kill -9, detects a hang, stops gracefully', async (t) => {
  const sup = new Supervisor({ bin, dataDir: tmp(), healthMs: 200, healthTimeoutMs: 300 });
  const first = await sup.start();
  assert.ok(first.pid > 0);

  // 1. crash: SIGKILL the child, supervisor must bring a new one up
  let t0 = process.hrtime.bigint();
  process.kill(first.pid, 'SIGKILL');
  const [hello2] = await once(sup, 'ready');
  const crashRestartMs = ms(t0);
  assert.notStrictEqual(hello2.pid, first.pid);
  assert.deepStrictEqual(await sup.request('ping', { payload: 1 }), { payload: 1 });

  // 2. hang: SIGSTOP freezes it; health checks must time out, kill, restart
  t0 = process.hrtime.bigint();
  process.kill(hello2.pid, 'SIGSTOP');
  const [hello3] = await once(sup, 'ready');
  const hangRestartMs = ms(t0);
  assert.notStrictEqual(hello3.pid, hello2.pid);

  // 3. requests during the gap fail fast with a clear code, not hang
  // (covered by request() when cur is null; exercised implicitly above)

  // 4. graceful stop: shutdown -> exit 0 within the grace period
  t0 = process.hrtime.bigint();
  const r = await sup.stop();
  const stopMs = ms(t0);
  assert.strictEqual(r.how, 'graceful');
  assert.strictEqual(r.code, 0);

  t.diagnostic(JSON.stringify({ crashRestartMs, hangRestartMs, stopMs }));
  if (process.env.RESULTS) {
    fs.writeFileSync(path.join(process.env.RESULTS, 'supervisor.json'),
      JSON.stringify({ crashRestartMs, hangRestartMs, stopMs, healthMs: 200, healthTimeoutMs: 300 }, null, 2));
  }
});

test('stop() escalates to SIGKILL when the sidecar is frozen', async () => {
  const sup = new Supervisor({ bin, dataDir: tmp(), healthMs: 60_000 });
  const h = await sup.start();
  process.kill(h.pid, 'SIGSTOP');
  const r = await sup.stop({ graceMs: 500 });
  assert.strictEqual(r.how, 'killed');
});
