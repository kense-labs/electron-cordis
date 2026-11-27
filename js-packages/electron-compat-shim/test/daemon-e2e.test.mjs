import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  CordisCompatError,
  CordisDaemonClient,
  createElectronCompatFacades,
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

function generateLoadPlan(profileRelativePath) {
  const outDir = mkdtempSync(path.join(tmpdir(), 'cordis-e2e-'));
  execFileSync(
    'python3',
    [
      path.join(ELECTRON_CORDIS_ROOT, 'script/gen-cordis-build.py'),
      path.join(ELECTRON_CORDIS_ROOT, profileRelativePath),
      '--out-dir',
      outDir,
    ],
    { stdio: 'pipe' }
  );
  return { outDir, loadPlanPath: path.join(outDir, 'load-plan.json') };
}

test('e2e: standalone daemon refuses a load plan that requires embedded isolation', () => {
  const { outDir, loadPlanPath } = generateLoadPlan('profiles/inkstone-headless.cordis.json');
  try {
    const plan = JSON.parse(readFileSync(loadPlanPath, 'utf8'));
    for (const [entry, diagnostic] of [
      [
        { id: '@dsh/krun-runtime', runtime: 'rust', isolation: 'in-process', realm: 'core' },
        /requires an embedded KRun runtime/,
      ],
      [
        { id: '@dsh/runtime-node-host', runtime: 'rust', isolation: 'microvm', realm: 'core' },
        /requires a trusted local process/,
      ],
    ]) {
      writeFileSync(loadPlanPath, JSON.stringify({ ...plan, entries: [...plan.entries, entry] }));
      const result = spawnSync(DAEMON_BIN, ['--load-plan', loadPlanPath], {
        encoding: 'utf8',
        timeout: 5000,
      });
      assert.equal(result.status, 1, result.stderr);
      assert.match(result.stderr, diagnostic);
    }
  } finally {
    rmSync(outDir, { recursive: true, force: true });
  }
});

test('socket clients share a daemon and disconnect independently', { skip: process.platform === 'win32' }, async () => {
  const socketDir = mkdtempSync('/tmp/cordis-e2e-');
  const socketPath = path.join(socketDir, 'cordis.sock');
  const owner = new CordisDaemonClient({ binaryPath: DAEMON_BIN, extraArgs: ['--socket', socketPath] });
  let ui;
  try {
    assert.equal((await owner.health()).ui_ready, false);
    ui = new CordisDaemonClient({ socketPath });
    assert.equal((await ui.notifyUiReady()).ui_ready, true);
    await ui.shutdown();
    assert.equal((await owner.health()).ui_ready, true);
  } finally {
    await ui?.shutdown();
    await owner.shutdown();
    rmSync(socketDir, { recursive: true, force: true });
  }
});

test('e2e: electron-compat-shim drives real electron-cordis-daemon across health -> ui_ready -> auth.swap -> compat caps -> shutdown', async () => {
  assert.ok(
    existsSync(DAEMON_BIN),
    `compiled daemon binary not found at ${DAEMON_BIN}; run cargo build -p electron-cordis-daemon first`
  );

  const { outDir, loadPlanPath } = generateLoadPlan('profiles/inkstone-desktop.cordis.json');
  const client = new CordisDaemonClient({ binaryPath: DAEMON_BIN, loadPlanPath });
  const compat = createElectronCompatFacades(client);

  try {
    // 1. Initial health (Pre-UI first-paint priority)
    const h0 = await client.health();
    assert.equal(h0.profile, 'inkstone-desktop');
    assert.equal(h0.phase, 'pre_ui');
    assert.equal(h0.ui_ready, false);
    assert.equal(h0.auth_generation, 0);
    assert.ok(h0.bundled_plugins > 0);
    assert.ok(h0.on_demand_plugins > 0);
    assert.deepEqual(h0.krun_backends, []);

    // 2. Two-phase boot convergence: ui_ready -> auth.swap
    const ui = await client.notifyUiReady();
    assert.equal(ui.phase, 'ui_ready');
    assert.equal(ui.ui_ready, true);

    const auth = await client.swapAuthGeneration();
    assert.equal(auth.phase, 'core_active');
    assert.equal(auth.auth_generation, 1);

    // 3. cap-clipboard: format query MUST NOT leak clipboard body
    const secretBody = 'INKSTONE_CLIPBOARD_SECRET_XYZ_42';
    await compat.clipboard.writeText(secretBody);
    const formats = await compat.clipboard.availableFormats();
    assert.deepEqual(formats, ['text/plain']);
    assert.ok(!JSON.stringify(formats).includes(secretBody));
    const readBack = await compat.clipboard.readText();
    assert.equal(readBack, secretBody);

    // 4. cap-safe-storage: HostRealm exclusive round-trip + non-host FailClosed denial
    const avail = await compat.safeStorage.isEncryptionAvailable();
    assert.equal(avail, true);
    const cipher = await compat.safeStorage.encryptString('inkstone-session-secret');
    assert.equal(typeof cipher, 'string');
    assert.notEqual(cipher, 'inkstone-session-secret');
    const decrypted = await compat.safeStorage.decryptString(cipher);
    assert.equal(decrypted, 'inkstone-session-secret');

    await assert.rejects(
      () =>
        client.request('cordis.compat.call', {
          capability: 'safeStorage',
          method: 'isEncryptionAvailable',
          realm: 'untrusted-plugin',
          args: {},
        }),
      (err) => err instanceof CordisCompatError && err.message.includes('SAFE_STORAGE_REALM_DENIED')
    );

    // 5. cap-dialog (non-blocking headless)
    const box = await compat.dialog.showMessageBox({
      title: 'Inkstone',
      message: 'Proceed?',
      buttons: ['Cancel', 'OK'],
      defaultId: 1,
    });
    assert.equal(box.response, 1);

    // 6. cap-os-shell: Plane A S1-S6 allowlist & zero shell concatenation
    const opened = await compat.shell.openExternal('https://internai.cn/inkstone');
    assert.equal(opened.opened, true);
    assert.ok(Array.isArray(opened.argv));
    assert.ok(!opened.argv.includes('sh') && !opened.argv.includes('-c'));

    await assert.rejects(
      () => compat.shell.openExternal('file:///etc/passwd'),
      (err) => err instanceof CordisCompatError && err.message.includes('SHELL_POLICY_DENIED')
    );
    await assert.rejects(
      () => compat.shell.openExternal('javascript:alert(1)'),
      (err) => err instanceof CordisCompatError && err.message.includes('SHELL_POLICY_DENIED')
    );
    await assert.rejects(
      () =>
        client.callCapability('shell', 'execArgv', {
          argv: ['sh', '-c', 'echo nope'],
        }),
      (err) => err instanceof CordisCompatError && err.message.includes('SHELL_POLICY_DENIED')
    );

    // 7. Clean reverse-order teardown
    await client.shutdown();
  } finally {
    rmSync(outDir, { recursive: true, force: true });
  }
});

test('e2e: headless profile load-plan fails closed on undeclared clipboard capability while keeping safeStorage', async () => {
  const { outDir, loadPlanPath } = generateLoadPlan('profiles/inkstone-headless.cordis.json');
  const client = new CordisDaemonClient({ binaryPath: DAEMON_BIN, loadPlanPath });
  const compat = createElectronCompatFacades(client);

  try {
    const h = await client.health();
    assert.equal(h.profile, 'inkstone-headless');

    // Clipboard is NOT in inkstone-headless.cordis.json -> must FailClosed
    await assert.rejects(
      () => compat.clipboard.readText(),
      (err) => err instanceof CordisCompatError && err.message.includes('CAPABILITY_NOT_LOADED')
    );

    // SafeStorage IS declared in inkstone-headless.cordis.json -> works in HostRealm
    assert.equal(await compat.safeStorage.isEncryptionAvailable(), true);
    await client.shutdown();
  } finally {
    rmSync(outDir, { recursive: true, force: true });
  }
});
