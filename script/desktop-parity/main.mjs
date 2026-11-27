import { deepStrictEqual, ok } from 'node:assert/strict';
import { fork } from 'node:child_process';
import { writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { join } from 'node:path';
import { app, BrowserWindow, ipcMain, nativeImage, protocol, session, utilityProcess } from 'electron/main';

const require = createRequire(import.meta.url);
const [, , userData, output, addon] = process.argv;
const scheme = 'cordis-parity';
app.setPath('userData', userData);
protocol.registerSchemesAsPrivileged([{ scheme, privileges: { standard: true, secure: true } }]);

function describeExports(module) {
  return Object.getOwnPropertyNames(module).sort().map(name => {
    const descriptor = Object.getOwnPropertyDescriptor(module, name);
    return {
      name,
      enumerable: descriptor.enumerable,
      configurable: descriptor.configurable,
      ...(Object.hasOwn(descriptor, 'value') ? {
        type: typeof descriptor.value,
        writable: descriptor.writable
      } : {
        getter: typeof descriptor.get === 'function',
        setter: typeof descriptor.set === 'function'
      })
    };
  });
}

const childPayload = { text: 'desktop child IPC', values: [1, true, null] };

function probeChild(child, send) {
  return new Promise((resolve, reject) => {
    let reply;
    child.once('error', reject);
    child.once('spawn', () => send(childPayload));
    child.once('message', value => {
      reply = value;
      send('exit');
    });
    child.once('exit', (code, signal) => {
      if (code === 0 && reply) {
        resolve(reply);
      } else {
        reject(new Error(`Child IPC probe exited with ${signal ?? code} before completing`));
      }
    });
  });
}

app.whenReady().then(async () => {
  const versions = {};
  for (const name of ['electron', 'chrome', 'node', 'v8', 'modules', 'napi']) {
    const version = process.versions[name];
    ok(version, `Electron did not report process.versions.${name}`);
    versions[name] = version;
  }
  const modules = {};
  for (const id of ['electron', 'electron/main', 'electron/common']) {
    modules[id] = { require: describeExports(require(id)), import: describeExports(await import(id)) };
  }

  const image = nativeImage.createFromBitmap(Buffer.from([0, 0, 255, 255]), { width: 1, height: 1 });
  const imageSize = image.getSize();
  deepStrictEqual(imageSize, { width: 1, height: 1 });

  const partition = session.fromPartition(scheme);
  partition.protocol.handle(scheme, () => new Response('<title>Electron desktop parity</title>', {
    headers: { 'content-type': 'text/html' }
  }));
  const sessionBehavior = {
    persistent: partition.isPersistent(),
    handlesProtocol: partition.protocol.isProtocolHandled(scheme),
    defaultSessionHandlesProtocol: session.defaultSession.protocol.isProtocolHandled(scheme)
  };
  deepStrictEqual(sessionBehavior, {
    persistent: false,
    handlesProtocol: true,
    defaultSessionHandlesProtocol: false
  });

  const channel = 'cordis:desktop-parity';
  const window = new BrowserWindow({
    show: false,
    webPreferences: { session: partition, preload: join(import.meta.dirname, 'preload.cjs') }
  });
  ipcMain.handle(channel, (event, value) => ({
    mainFrame: event.senderFrame === window.webContents.mainFrame,
    value
  }));
  await window.loadURL(`${scheme}://probe/`);
  deepStrictEqual(window.webContents.getTitle(), 'Electron desktop parity');
  const renderer = await window.webContents.executeJavaScript(`(async () => ({
    ...window.desktopParity.defaults,
    pageRequire: typeof window.require,
    pageProcess: typeof window.process,
    roundTrip: await window.desktopParity.echo({ text: 'IPC', values: [1, true, null] })
  }))()`);
  deepStrictEqual(renderer.roundTrip, { mainFrame: true, value: { text: 'IPC', values: [1, true, null] } });
  deepStrictEqual(
    [renderer.sandboxed, renderer.contextIsolated, renderer.pageRequire, renderer.pageProcess],
    [true, true, 'undefined', 'undefined']
  );

  const childEntry = join(import.meta.dirname, 'child.cjs');
  const node = fork(childEntry, [], { execArgv: [] });
  const nodeChild = await probeChild(node, message => node.send(message));
  const utility = utilityProcess.fork(childEntry);
  const utilityChild = await probeChild(utility, message => utility.postMessage(message));
  const expectedChild = { electron: versions.electron, modules: versions.modules, value: childPayload };
  deepStrictEqual(nodeChild, { ...expectedChild, type: null });
  deepStrictEqual(utilityChild, { ...expectedChild, type: 'utility' });

  let liteCapabilities;
  if (process.env.ELECTRON_CORDIS_PROBE_LITE === '1') {
    const { probeLiteCapabilities } = await import('./lite-capabilities.mjs');
    liteCapabilities = await probeLiteCapabilities(window, require('electron/main'));
  }

  ipcMain.removeHandler(channel);
  window.destroy();
  partition.protocol.unhandle(scheme);
  await writeFile(output, JSON.stringify({
    platform: process.platform,
    arch: process.arch,
    versions,
    modules,
    imageSize,
    sessionBehavior,
    renderer,
    nodeChild,
    utilityChild,
    addon: addon ? describeExports(require(addon)) : null,
    liteCapabilities
  }));
  app.exit(0);
}).catch(error => {
  console.error(error);
  app.exit(1);
});
