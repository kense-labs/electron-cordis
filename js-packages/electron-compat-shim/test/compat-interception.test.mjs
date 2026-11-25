import test from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  CordisCompatError,
  CordisDaemonClient,
  STRIPPED_CLASS_C_EXPORTS,
  createPatchedElectronModule,
} from '../src/index.mjs';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const ELECTRON_CORDIS_ROOT = path.resolve(__dirname, '../../..');
const DAEMON_BIN = path.join(
  ELECTRON_CORDIS_ROOT,
  'cordis',
  'target',
  'debug',
  process.platform === 'win32' ? 'electron-cordis-daemon.exe' : 'electron-cordis-daemon'
);

function generateDesktopLoadPlan() {
  const outDir = mkdtempSync(path.join(tmpdir(), 'cordis-compat-'));
  execFileSync(
    'python3',
    [
      path.join(ELECTRON_CORDIS_ROOT, 'script/gen-cordis-build.py'),
      path.join(ELECTRON_CORDIS_ROOT, 'profiles/inkstone-desktop.cordis.json'),
      '--out-dir',
      outDir,
    ],
    { stdio: 'pipe' }
  );
  return { outDir, loadPlanPath: path.join(outDir, 'load-plan.json') };
}

function generateEd25519Pair() {
  const { publicKey, privateKey } = crypto.generateKeyPairSync('ed25519');
  // Raw 32-byte Ed25519 public key from SPKI DER (last 32 bytes).
  const spki = publicKey.export({ format: 'der', type: 'spki' });
  const pubHex = spki.subarray(spki.length - 32).toString('hex');
  return { pubHex, privateKey };
}

function sha256Hex(str) {
  return crypto.createHash('sha256').update(str, 'utf8').digest('hex');
}

test('patched electron module intercepts Batch 1-3 Class-A APIs (including cross-language Ed25519), passes through Class-B, and fails closed on Class-C', async () => {
  assert.ok(existsSync(DAEMON_BIN), `missing daemon binary: ${DAEMON_BIN}`);
  const { outDir, loadPlanPath } = generateDesktopLoadPlan();
  const client = new CordisDaemonClient({ binaryPath: DAEMON_BIN, loadPlanPath });

  const stubBrowserWindow = class StubBrowserWindow {
    static kind = 'electron-lite-web-surface';
  };
  const electron = createPatchedElectronModule(client, {
    BrowserWindow: stubBrowserWindow,
    ipcMain: { handle: () => 'ipc-ok' },
  });

  try {
    // 1. Class-B Chromium surface passes through untouched
    assert.equal(electron.BrowserWindow.kind, 'electron-lite-web-surface');
    assert.equal(electron.ipcMain.handle(), 'ipc-ok');

    // 2. Class-C stripped APIs fail closed immediately
    for (const strippedName of STRIPPED_CLASS_C_EXPORTS) {
      assert.throws(
        () => electron[strippedName],
        (err) => err instanceof CordisCompatError && err.message.includes('CLASS_C_MODULE_STRIPPED')
      );
    }

    // 3. Batch 1 multi-format clipboard (HTML + text) never leaks body in availableFormats
    await electron.clipboard.writeText('plain-secret-999');
    await electron.clipboard.writeHTML('<b>html-secret-999</b>');
    const fmts = await electron.clipboard.availableFormats();
    assert.deepEqual(fmts, ['text/html', 'text/plain']);
    assert.ok(!JSON.stringify(fmts).includes('secret-999'));
    assert.equal(await electron.clipboard.readHTML(), '<b>html-secret-999</b>');

    // 4. Batch 2: Bilingual Tray Menu + Startup Intent FIFO flushed on ui_ready + Notification
    const zhMenu = await electron.trayMenu.getMenu();
    assert.equal(zhMenu.items[0].label, '打开主窗口');
    const enMenu = await electron.trayMenu.setLocale('en-US');
    assert.equal(enMenu.items[0].label, 'Open Main Window');

    await electron.osIntegration.enqueueStartupIntent('deep-link', 'inkstone-cordis://project/42');
    await electron.osIntegration.enqueueStartupIntent('open-file', '/workspace/demo.pdf');
    const uiReady = await client.notifyUiReady();
    assert.equal(uiReady.startup_intents.length, 2);
    assert.equal(uiReady.startup_intents[0].kind, 'deep-link');
    assert.equal(uiReady.startup_intents[1].kind, 'open-file');

    const notif = await electron.notification.show({ title: 'Cordis', body: 'Ready' });
    assert.equal(notif.id, 1);

    // 5. Batch 3: Real cross-language Node Ed25519 signing -> Rust ed25519-dalek verification!
    const { pubHex, privateKey } = generateEd25519Pair();

    // 5a. Dual-channel Updater (cordis-canary + base=electron-cordis MAIN_COMPATIBILITY_HASH)
    const chInfo = await electron.updater.getChannelInfo();
    assert.equal(chInfo.channel, 'cordis-canary');
    assert.ok(chInfo.main_compatibility_hash.includes('base=electron-cordis'));
    await electron.updater.setTrustedPubkey(pubHex);

    const bundleText = 'export const rendererVersion = "0.3.0-cordis";';
    const bundleSha = sha256Hex(bundleText);
    const canonicalUpdate = `renderer-update-v1|0.3.0-cordis.1|cordis-canary|${chInfo.main_compatibility_hash}|${bundleSha}`;
    const updateSigHex = crypto
      .sign(null, Buffer.from(canonicalUpdate, 'utf8'), privateKey)
      .toString('hex');

    const verified = await electron.updater.verifyRendererUpdate(
      {
        version: '0.3.0-cordis.1',
        channel: 'cordis-canary',
        main_compatibility_hash: chInfo.main_compatibility_hash,
        bundle_sha256: bundleSha,
        ed25519_signature_hex: updateSigHex,
      },
      bundleText
    );
    assert.equal(verified.verified, true);

    // Cross-base native Electron renderer bundle MUST be rejected
    await assert.rejects(
      () =>
        electron.updater.verifyRendererUpdate(
          {
            version: '0.3.0',
            channel: 'cordis-canary',
            main_compatibility_hash: 'base=electron-native;abi=electron43;v1',
            bundle_sha256: bundleSha,
            ed25519_signature_hex: updateSigHex,
          },
          bundleText
        ),
      (err) => err instanceof CordisCompatError && err.message.includes('COMPATIBILITY_MISMATCH')
    );

    // 5b. Signed on-demand Plugin Installer (Ed25519 + SHA-256 + atomic activate & rollback)
    await electron.pluginInstaller.setTrustedPubkey(pubHex);
    const makeSignedPkg = (version, body) => {
      const fSha = sha256Hex(body);
      const canonical = `plugin-pkg-v1|@dsh/plugin-feishu|${version}|index.mjs=${fSha}`;
      const sigHex = crypto
        .sign(null, Buffer.from(canonical, 'utf8'), privateKey)
        .toString('hex');
      return {
        plugin_id: '@dsh/plugin-feishu',
        version,
        files: [{ relative_path: 'index.mjs', sha256: fSha, content_utf8: body }],
        ed25519_signature_hex: sigHex,
      };
    };

    const inst1 = await electron.pluginInstaller.install(makeSignedPkg('1.0.0', 'export const v=1;'));
    assert.equal(inst1.active_version, '1.0.0');
    const inst2 = await electron.pluginInstaller.install(makeSignedPkg('1.1.0', 'export const v=2;'));
    assert.equal(inst2.active_version, '1.1.0');
    const rolled = await electron.pluginInstaller.rollback('@dsh/plugin-feishu');
    assert.equal(rolled.active_version, '1.0.0');

    // 5c. Desktop capture permission gate
    await assert.rejects(
      () => electron.desktopCapturer.getSources({ types: ['screen'] }),
      (err) => err instanceof CordisCompatError && err.message.includes('CAPTURE_PERMISSION_DENIED')
    );
    await electron.desktopCapturer.setPermission(true);
    const caps = await electron.desktopCapturer.getSources({ types: ['screen'] });
    assert.equal(caps.sources.length, 1);

    await client.shutdown();
  } finally {
    rmSync(outDir, { recursive: true, force: true });
  }
});
