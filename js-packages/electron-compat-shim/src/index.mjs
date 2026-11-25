/**
 * @dsh/electron-compat
 *
 * Thin fail-closed JSON-RPC 2.0 client & module interception shim that bridges legacy
 * `require('electron')` Class-A OS API calls (Batch 1–3: clipboard, safeStorage, dialog,
 * shell, trayMenu, notification, osIntegration, desktopCapture, updater, pluginInstaller)
 * to the local `electron-cordis-daemon` microkernel while passing Class-B Chromium surface
 * APIs to Electron-Lite and failing closed on stripped Class-C APIs.
 */
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';

export const STRIPPED_CLASS_C_EXPORTS = Object.freeze([
  'BrowserView',
  'TouchBar',
  'inAppPurchase',
  'pushNotifications',
  'contentTracing',
  'netLog',
  'ShareMenu',
  'sharedTexture',
]);

export class CordisCompatError extends Error {
  constructor(code, message) {
    super(message);
    this.name = 'CordisCompatError';
    this.code = code;
  }
}

export class CordisDaemonClient {
  #proc;
  #nextId = 1;
  #pending = new Map();
  #closed = false;

  constructor({ binaryPath, loadPlanPath, extraArgs = [], env = {} }) {
    if (!binaryPath) {
      throw new CordisCompatError(-32000, 'binaryPath is required for CordisDaemonClient');
    }
    const args = [];
    if (loadPlanPath) args.push('--load-plan', loadPlanPath);
    if (Array.isArray(extraArgs) && extraArgs.length > 0) {
      args.push(...extraArgs);
    }
    // Security invariant: fixed binary, argv array, shell: false, explicit env allowlist.
    this.#proc = spawn(binaryPath, args, {
      stdio: ['pipe', 'pipe', 'pipe'],
      shell: false,
      env: { PATH: process.env.PATH ?? '', ...env },
    });

    const rl = createInterface({ input: this.#proc.stdout });
    rl.on('line', (line) => {
      const trimmed = line.trim();
      if (!trimmed) return;
      let msg;
      try {
        msg = JSON.parse(trimmed);
      } catch {
        return;
      }
      const pending = this.#pending.get(msg.id);
      if (!pending) return;
      this.#pending.delete(msg.id);
      if (msg.error) {
        pending.reject(new CordisCompatError(msg.error.code, msg.error.message));
      } else {
        pending.resolve(msg.result);
      }
    });

    const rejectAll = (reason) => {
      if (this.#closed) return;
      this.#closed = true;
      for (const p of this.#pending.values()) {
        p.reject(new CordisCompatError(-32099, reason));
      }
      this.#pending.clear();
    };

    this.#proc.on('exit', (code, signal) =>
      rejectAll(`cordis-daemon exited (code=${code}, signal=${signal})`)
    );
    this.#proc.on('error', (err) => rejectAll(`cordis-daemon error: ${err.message}`));
  }

  request(method, params = {}) {
    if (this.#closed) {
      return Promise.reject(new CordisCompatError(-32099, 'cordis-daemon client is closed'));
    }
    const id = this.#nextId++;
    const payload = JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n';
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      this.#proc.stdin.write(payload, 'utf8', (err) => {
        if (err) {
          this.#pending.delete(id);
          reject(new CordisCompatError(-32098, err.message));
        }
      });
    });
  }

  health() {
    return this.request('cordis.health');
  }

  notifyUiReady() {
    return this.request('cordis.ui_ready');
  }

  swapAuthGeneration() {
    return this.request('cordis.auth.swap');
  }

  callCapability(capability, method, args = {}) {
    return this.request('cordis.compat.call', { capability, method, args });
  }

  async shutdown() {
    if (this.#closed) return;
    try {
      await this.request('cordis.shutdown');
    } finally {
      this.#proc.stdin.end();
    }
  }
}

/**
 * Builds Electron-compatible Class-A facade objects (Batch 1–3) backed by a live `CordisDaemonClient`.
 */
export function createElectronCompatFacades(client) {
  return {
    clipboard: {
      availableFormats: () => client.callCapability('clipboard', 'availableFormats'),
      readText: () => client.callCapability('clipboard', 'readText'),
      writeText: (text) => client.callCapability('clipboard', 'writeText', { text }),
      readHTML: () => client.callCapability('clipboard', 'readHTML'),
      writeHTML: (markup) => client.callCapability('clipboard', 'writeHTML', { markup }),
      readImageBase64: () => client.callCapability('clipboard', 'readImageBase64'),
      writeImageBase64: (pngBase64) =>
        client.callCapability('clipboard', 'writeImageBase64', { pngBase64 }),
      clear: () => client.callCapability('clipboard', 'clear'),
    },
    safeStorage: {
      isEncryptionAvailable: () => client.callCapability('safeStorage', 'isEncryptionAvailable'),
      encryptString: (plainText) =>
        client.callCapability('safeStorage', 'encryptString', { plainText }),
      decryptString: (cipherBase64) =>
        client.callCapability('safeStorage', 'decryptString', { cipherBase64 }),
    },
    dialog: {
      showOpenDialog: (options) => client.callCapability('dialog', 'showOpenDialog', { options }),
      showSaveDialog: (options) => client.callCapability('dialog', 'showSaveDialog', { options }),
      showMessageBox: (options) => client.callCapability('dialog', 'showMessageBox', { options }),
    },
    shell: {
      openExternal: (url) => client.callCapability('shell', 'openExternal', { url }),
      showItemInFolder: (fullPath) =>
        client.callCapability('shell', 'showItemInFolder', { fullPath }),
      execArgv: (argv, opts = {}) =>
        client.callCapability('shell', 'execArgv', { argv, ...opts }),
    },
    trayMenu: {
      getMenu: () => client.callCapability('trayMenu', 'getMenu'),
      setLocale: (locale) => client.callCapability('trayMenu', 'setLocale', { locale }),
      setMenu: (items) => client.callCapability('trayMenu', 'setMenu', { items }),
      setTooltip: (tooltip) => client.callCapability('trayMenu', 'setTooltip', { tooltip }),
    },
    notification: {
      isSupported: () => client.callCapability('notification', 'isSupported'),
      show: (opts) => client.callCapability('notification', 'show', opts),
      listSent: () => client.callCapability('notification', 'listSent'),
      setPermission: (permitted) =>
        client.callCapability('notification', 'setPermission', { permitted }),
    },
    osIntegration: {
      getLocale: () => client.callCapability('osIntegration', 'getLocale'),
      setLocale: (locale) => client.callCapability('osIntegration', 'setLocale', { locale }),
      enqueueStartupIntent: (kind, payload) =>
        client.callCapability('osIntegration', 'enqueueStartupIntent', { kind, payload }),
      drainStartupIntents: () => client.callCapability('osIntegration', 'drainStartupIntents'),
      getProtocolScheme: () => client.callCapability('osIntegration', 'getProtocolScheme'),
      setLoginItemSettings: (openAtLogin) =>
        client.callCapability('osIntegration', 'setLoginItemSettings', { openAtLogin }),
    },
    desktopCapturer: {
      getSources: (opts) => client.callCapability('desktopCapture', 'getSources', opts),
      setPermission: (granted) =>
        client.callCapability('desktopCapture', 'setPermission', { granted }),
    },
    updater: {
      getChannelInfo: () => client.callCapability('updater', 'getChannelInfo'),
      setTrustedPubkey: (pubkeyHex) =>
        client.callCapability('updater', 'setTrustedPubkey', { pubkeyHex }),
      verifyRendererUpdate: (manifest, bundleText) =>
        client.callCapability('updater', 'verifyRendererUpdate', { manifest, bundleText }),
    },
    pluginInstaller: {
      setTrustedPubkey: (pubkeyHex) =>
        client.callCapability('pluginInstaller', 'setTrustedPubkey', { pubkeyHex }),
      install: (signedPkg) => client.callCapability('pluginInstaller', 'install', signedPkg),
      rollback: (plugin_id) => client.callCapability('pluginInstaller', 'rollback', { plugin_id }),
      listInstalled: () => client.callCapability('pluginInstaller', 'listInstalled'),
    },
  };
}

/**
 * Drop-in `require('electron')` proxy for Electron-Cordis:
 * - Class-A OS APIs -> routed to Rust DSH plugins in `electron-cordis-daemon`
 * - Class-B Chromium surface APIs (`BrowserWindow`, `ipcMain`, `protocol`, `session`, `app`, ...)
 *   -> passed through untouched to `electronLiteSurface`
 * - Class-C stripped APIs (`BrowserView`, `TouchBar`, `inAppPurchase`, ...) -> FailClosed error
 */
export function createPatchedElectronModule(client, electronLiteSurface = {}) {
  const facades = createElectronCompatFacades(client);
  const stripped = new Set(STRIPPED_CLASS_C_EXPORTS);

  return new Proxy(electronLiteSurface, {
    get(target, prop, receiver) {
      if (typeof prop === 'string') {
        if (stripped.has(prop)) {
          throw new CordisCompatError(
            -32005,
            `CLASS_C_MODULE_STRIPPED (fail-closed): \`electron.${prop}\` is removed in Electron-Cordis`
          );
        }
        if (Object.prototype.hasOwnProperty.call(facades, prop)) {
          return facades[prop];
        }
      }
      return Reflect.get(target, prop, receiver);
    },
  });
}
