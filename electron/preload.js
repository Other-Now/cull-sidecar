'use strict';
const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('cull', {
  call: (method, params) => ipcRenderer.invoke('cull:call', method, params),
  pickFolder: () => ipcRenderer.invoke('cull:pickFolder'),
  onEvent: (fn) => ipcRenderer.on('cull:event', (_e, m) => fn(m)),
});
