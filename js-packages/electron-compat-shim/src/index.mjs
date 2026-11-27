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
import { lstat, realpath } from 'node:fs/promises';
import { createConnection } from 'node:net';
import { isAbsolute } from 'node:path';
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
  #writer;
  #nextId = 1;
  #pending = new Map();
  #closed = false;

  constructor({ binaryPath, socketPath, loadPlanPath, extraArgs = [], env = {} }) {
    if (Boolean(binaryPath) === Boolean(socketPath)) {
      throw new CordisCompatError(-32000, 'Specify either binaryPath or socketPath for CordisDaemonClient');
    }
    let input;
    if (socketPath) {
      input = this.#writer = createConnection({ path: socketPath });
    } else {
      const args = [];
      if (loadPlanPath) args.push('--load-plan', loadPlanPath);
      if (Array.isArray(extraArgs) && extraArgs.length > 0) args.push(...extraArgs);
      // The child inherits only the environment explicitly provided here.
      this.#proc = spawn(binaryPath, args, {
        stdio: ['pipe', 'pipe', 'inherit'],
        shell: false,
        env: { PATH: process.env.PATH ?? '', ...env },
      });
      input = this.#proc.stdout;
      this.#writer = this.#proc.stdin;
    }

    const rl = createInterface({ input });
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

    rl.on('close', () => rejectAll('Cordis connection closed'));
    const connectionError = (err) => rejectAll(`Cordis connection error: ${err.message}`);
    rl.on('error', connectionError);
    this.#writer.on('error', connectionError);
    this.#proc?.on('exit', (code, signal) =>
      rejectAll(`cordis-daemon exited (code=${code}, signal=${signal})`)
    );
    this.#proc?.on('error', connectionError);
  }

  request(method, params = {}) {
    if (this.#closed) {
      return Promise.reject(new CordisCompatError(-32099, 'cordis-daemon client is closed'));
    }
    const id = this.#nextId++;
    const payload = JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n';
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      this.#writer.write(payload, 'utf8', (err) => {
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

  // A socket client closes its connection. The supervisor owns the shared daemon.
  async shutdown() {
    if (this.#closed) return;
    try {
      if (this.#proc) await this.request('cordis.shutdown');
    } finally {
      this.#writer.end();
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
      verifyArchiveManifest: (manifest) => client.callCapability('pluginInstaller', 'verifyArchiveManifest', manifest),
      installArchive: (manifest, archivePath) => client.callCapability('pluginInstaller', 'installArchive', { manifest, archivePath }),
      rollback: (plugin_id) => client.callCapability('pluginInstaller', 'rollback', { plugin_id }),
      listInstalled: () => client.callCapability('pluginInstaller', 'listInstalled'),
    },
  };
}

/** Resolve a signed archive when an application first uses the associated plugin. */
export function createOnDemandPluginResolver({ installer, pluginId, acquireArchive, acceptsVersion = () => true }) {
  let pending;

  async function resolveActive() {
    const { plugins } = await installer.listInstalled();
    let active = plugins.find((record) => record.plugin_id === pluginId);
    if (!active || !acceptsVersion(active.active_version)) {
      const { manifest, archivePath, release } = await acquireArchive();
      try {
        if (manifest.plugin_id !== pluginId || !acceptsVersion(manifest.version)) {
          throw new Error(`Unexpected archive identity for ${pluginId}`);
        }
        active = await installer.installArchive(manifest, archivePath);
        if (active.active_version !== manifest.version) {
          throw new Error(`Archive did not activate version ${manifest.version} of ${pluginId}`);
        }
      } finally {
        await release?.();
      }
    }
    if (active.plugin_id !== pluginId || !acceptsVersion(active.active_version)) {
      throw new Error(`Incompatible active version for ${pluginId}`);
    }
    if (!isAbsolute(active.active_path) || !(await lstat(active.active_path)).isDirectory()) {
      throw new Error(`Invalid installation directory for ${pluginId}`);
    }
    return { ...active, active_path: await realpath(active.active_path) };
  }

  return () => {
    pending ??= resolveActive().finally(() => { pending = undefined; });
    return pending;
  };
}

/** Resolve an on-demand plugin from host-configured HTTPS manifest and archive URLs. */
export function createRemotePluginResolver({
  installer, pluginId, manifestUrl, archiveUrl, acceptsVersion = () => true, signal, timeoutMs = 15 * 60_000
}) {
  return createOnDemandPluginResolver({
    installer, pluginId, acceptsVersion,
    acquireArchive: async () => {
      const { acquirePluginArchive } = await import('./plugin-archive-download.mjs');
      return acquirePluginArchive({ installer, pluginId, acceptsVersion, manifestUrl, archiveUrl, signal, timeoutMs });
    }
  });
}

/**
 * Augments an Electron module with Cordis facades. Native exports retain their
 * original implementations and signatures. Requests for stripped exports fail
 * closed when the underlying Electron build omits them.
 */
export function createPatchedElectronModule(client, electronLiteSurface = {}) {
  const facades = createElectronCompatFacades(client);
  const stripped = new Set(STRIPPED_CLASS_C_EXPORTS);

  return new Proxy(electronLiteSurface, {
    get(target, prop, receiver) {
      if (typeof prop === 'string' && !Reflect.has(target, prop)) {
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
