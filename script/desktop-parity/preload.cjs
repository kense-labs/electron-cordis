const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('desktopParity', {
  defaults: {
    sandboxed: process.sandboxed,
    contextIsolated: process.contextIsolated,
    exports: Object.keys(require('electron')).sort()
  },
  echo: value => ipcRenderer.invoke('cordis:desktop-parity', value)
});
