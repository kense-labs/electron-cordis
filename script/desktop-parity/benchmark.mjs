import { app, BrowserWindow } from 'electron/main';
import { once } from 'node:events';
import { renameSync, writeFileSync } from 'node:fs';

const [, , profile, ready] = process.argv;
app.setPath('userData', profile);
app.whenReady().then(async () => {
  const window = new BrowserWindow({ width: 640, height: 480, show: false });
  const painted = once(window, 'ready-to-show');
  await window.loadURL('data:text/html,<title>cordis-benchmark</title><main>Cordis</main>');
  await painted;
  process.stdin.once('data', () => app.quit());
  process.stdin.resume();
  writeFileSync(ready + '.tmp', JSON.stringify({
    pid: process.pid,
    version: process.versions.electron,
    arch: process.arch,
    page: window.webContents.getTitle(),
  }));
  renameSync(ready + '.tmp', ready);
}).catch(error => {
  console.error(error);
  app.exit(1);
});
