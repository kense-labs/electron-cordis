const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('cordisDev', {
  health: () => ipcRenderer.invoke('cordis-dev:health')
});
