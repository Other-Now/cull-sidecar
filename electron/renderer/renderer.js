'use strict';
const $ = (id) => document.getElementById(id);
let photos = [], sel = 0, job = null;

async function refresh() {
  const r = await window.cull.call('list', { limit: 2000, picks_only: $('picks').checked });
  photos = r.photos;
  $('status').textContent = `${r.total} photos`;
  const grid = $('grid');
  grid.replaceChildren(...photos.map((p, i) => {
    const d = document.createElement('div');
    d.className = 'cell' + (i === sel ? ' sel' : '') + (p.decision === 2 ? ' reject' : '');
    const img = document.createElement('img');
    img.loading = 'lazy';
    if (p.thumb) img.src = 'file://' + p.thumb;
    const m = document.createElement('div');
    m.className = 'meta';
    m.innerHTML = `<span>${'★'.repeat(p.rating)}</span><span class="${p.pick ? 'pick' : ''}">${p.group ? (p.pick ? 'pick' : 'g' + p.group) : ''}</span><span>${Math.round(p.sharpness ?? 0)}</span>`;
    d.append(img, m);
    d.onclick = () => { sel = i; refresh(); };
    return d;
  }));
}

window.cull.onEvent((m) => {
  if (m.event === 'progress') { $('prog').max = m.total || 1; $('prog').value = m.done; }
  if (m.event === 'import_done' || m.event === 'import_failed') { job = null; $('cancel').disabled = true; refresh(); }
  if (m.event === 'sidecar_restarted') { $('status').textContent = 'backend restarted'; refresh(); }
});

$('import').onclick = async () => {
  const dir = await window.cull.pickFolder();
  if (!dir) return;
  job = (await window.cull.call('import', { dir })).job;
  $('cancel').disabled = false;
};
$('cancel').onclick = () => job && window.cull.call('cancel', { job });
$('group').onclick = async () => { const g = await window.cull.call('group', {}); $('status').textContent = `${g.groups} bursts`; refresh(); };
$('picks').onchange = refresh;

document.addEventListener('keydown', async (e) => {
  const p = photos[sel];
  if (e.key === 'ArrowRight') sel = Math.min(sel + 1, photos.length - 1);
  else if (e.key === 'ArrowLeft') sel = Math.max(sel - 1, 0);
  else if (!p) return;
  else if (e.key >= '0' && e.key <= '5') await window.cull.call('set_rating', { id: p.id, rating: Number(e.key) });
  else if (e.key === 'k') await window.cull.call('set_decision', { id: p.id, decision: 'keep' });
  else if (e.key === 'x') await window.cull.call('set_decision', { id: p.id, decision: 'reject' });
  else return;
  refresh();
});

refresh();
