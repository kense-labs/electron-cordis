import { app, BrowserWindow, ipcMain } from 'electron/main';

import { once } from 'node:events';
import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

import { CordisDaemonClient } from '../../js-packages/electron-compat-shim/src/index.mjs';

app.setName('Electron Cordis Dev');
app.on('window-all-closed', () => app.quit());

async function start() {
  if (process.env.ELECTRON_CORDIS_DEV_STATE) {
    const userData = join(process.env.ELECTRON_CORDIS_DEV_STATE, 'user-data');
    mkdirSync(userData, { recursive: true });
    app.setPath('userData', userData);
  }

  const client = new CordisDaemonClient({ socketPath: process.env.ELECTRON_CORDIS_SOCKET });
  app.once('will-quit', () => client.shutdown());
  const health = await client.health();
  await app.whenReady();

  const index = join(import.meta.dirname, 'index.html');
  const trustedUrl = pathToFileURL(index).href;
  const window = new BrowserWindow({
    width: 620,
    height: 450,
    show: false,
    webPreferences: {
      preload: join(import.meta.dirname, 'preload.cjs'),
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false
    }
  });
  window.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  window.webContents.on('will-navigate', (event) => event.preventDefault());

  const ready = once(window, 'ready-to-show').then(() => client.notifyUiReady());
  ipcMain.handle('cordis-dev:health', async (event) => {
    if (event.sender !== window.webContents || event.senderFrame?.url !== trustedUrl) {
      throw new Error('Unexpected Cordis dev window');
    }
    await ready;
    const { profile, phase, ui_ready: uiReady } = await client.health();
    return { profile, phase, uiReady, electron: process.versions.electron };
  });

  await Promise.all([window.loadFile(index), ready]);
  window.show();
  console.log(`[cordis-dev] Window ready (${health.profile}, Electron ${process.versions.electron})`);
}

start().catch((error) => {
  console.error(`[cordis-dev] ${error.message}`);
  app.exit(1);
});
