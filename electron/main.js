'use strict';
// Minimal Electron shell around the Rust sidecar. The renderer never talks to
// the sidecar directly: it calls an allowlisted set of methods through the
// main process (contextIsolation + sandbox), and receives events.
//
// CULL_SMOKE=<photo dir> runs headless: import, group, list, quit, and print a
// JSON summary (used to check the app end to end without clicking).

const { app, BrowserWindow, ipcMain, dialog } = require('electron');
const path = require('node:path');
const { Supervisor } = require('./sidecar');

const ALLOWED = new Set(['import', 'cancel', 'list', 'set_rating', 'set_decision', 'group', 'stats']);
const bin = process.env.CULL_SIDECAR_BIN || path.join(__dirname, '..', 'target', 'release', 'cull-sidecar');
let sup, win;

async function boot() {
  sup = new Supervisor({ bin, dataDir: process.env.CULL_DATA || path.join(app.getPath('userData'), 'catalog') });
  sup.on('event', (m) => win?.webContents.send('cull:event', m));
  sup.on('restarted', (n) => win?.webContents.send('cull:event', { event: 'sidecar_restarted', n }));
  sup.on('exit', (e) => console.error('[main] sidecar exited', e));
  await sup.start();

  ipcMain.handle('cull:call', (_e, method, params) => {
    if (!ALLOWED.has(method)) throw new Error(`method '${method}' not allowed from renderer`);
    return sup.request(method, params ?? {});
  });
  ipcMain.handle('cull:pickFolder', async () => {
    const r = await dialog.showOpenDialog(win, { properties: ['openDirectory'] });
    return r.canceled ? null : r.filePaths[0];
  });

  if (process.env.CULL_SMOKE && !process.env.CULL_SCREENSHOT) return smoke(process.env.CULL_SMOKE);

  win = new BrowserWindow({
    width: 1200, height: 800,
    webPreferences: { preload: path.join(__dirname, 'preload.js'), contextIsolation: true, sandbox: true, nodeIntegration: false },
  });
  await win.loadFile(path.join(__dirname, 'renderer', 'index.html'));
  if (process.env.CULL_SMOKE) return smoke(process.env.CULL_SMOKE);
}

async function smoke(dir) {
  const t0 = Date.now();
  const done = new Promise((res) => sup.on('event', (m) => m.event === 'import_done' && res(m.stats)));
  await sup.request('import', { dir });
  const stats = await done;
  const groups = await sup.request('group', {});
  const page = await sup.request('list', { limit: 3, picks_only: true });
  if (win && process.env.CULL_SCREENSHOT) {
    win.webContents.send('cull:event', { event: 'import_done' }); // renderer refreshes
    await new Promise((r) => setTimeout(r, 2000));
    require('node:fs').writeFileSync(process.env.CULL_SCREENSHOT, (await win.webContents.capturePage()).toPNG());
  }
  const stop = await sup.stop();
  console.log(JSON.stringify({ electron: process.versions.electron, imported: stats.imported, groups: groups.groups,
    picks_total: page.total, first: page.photos[0]?.path, sidecar_exit: stop, ms: Date.now() - t0 }));
  app.exit(stop.code === 0 ? 0 : 1);
}

app.whenReady().then(boot).catch((e) => { console.error(e); app.exit(1); });

// Quit: stop the sidecar first so it can checkpoint the WAL, then really quit.
let quitting = false;
app.on('before-quit', (e) => {
  if (quitting || !sup) return;
  e.preventDefault();
  quitting = true;
  sup.stop().finally(() => app.quit());
});
app.on('window-all-closed', () => app.quit());
