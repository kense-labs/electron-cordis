const { contextBridge, webUtils } = require('electron');

const dropped = new Promise((resolve) => {
  window.addEventListener('dragover', (event) => event.preventDefault());
  window.addEventListener('drop', (event) => {
    event.preventDefault();
    const file = event.dataTransfer.files[0];
    resolve({
      name: file?.name,
      path: file && webUtils.getPathForFile(file),
      sandboxed: process.sandboxed,
      contextIsolated: process.contextIsolated
    });
  }, { once: true });
});

contextBridge.exposeInMainWorld('fileDropProbe', { result: () => dropped });
