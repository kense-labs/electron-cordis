/**
 * Parallel Windows x64 packaging config for Inkstone on Electron-Cordis.
 *
 * Keeps native Electron 43.2.0 Windows builds 100% untouched:
 * - Isolated appId (`cn.internai.inkstone.cordis`) & productName for side-by-side install
 * - Isolated output directory (`dist/cordis-win`) and update channel (`cordis-canary`)
 * - Removes legacy Go `runtime/orbit-*` resources
 * - Previews the resources excluded by `ELECTRON_CORDIS_SLIM_PACK=1` in a stock config dry run
 *
 * Usage (from ../web on Windows x64):
 *   ELECTRON_CORDIS_DIST=C:\path\to\prebuilt-electron-cordis-win32-x64 \
 *     pnpm exec electron-builder --win --x64 \
 *     --config ../electron-cordis/script/inkstone-cordis-win-config.mjs --publish never
 */
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { parse } from 'yaml';
import { validateInkstoneElectronDist } from './inkstone-electron-dist.mjs';
import { includeInkstoneResource, inkstoneWebRoot } from './inkstone-package-resources.mjs';

const WEB_ROOT = inkstoneWebRoot();
const IDENTITY = {
  appId: 'cn.internai.inkstone.cordis',
  executableName: 'inkstone-cordis',
  packageName: 'inkstone-desktop-cordis',
};
const CANDIDATE_MODE = 'direct-windows-verification';

async function loadWindowsCandidate(stock) {
  const handoff = pathToFileURL(resolve(WEB_ROOT, 'scripts/build-windows-update-handoff.mjs'));
  const { windowsHandoffBuildIdentity } = await import(handoff.href);
  try {
    windowsHandoffBuildIdentity(IDENTITY, stock);
  } catch (error) {
    throw new Error(`Inkstone Windows update handoff does not accept ${IDENTITY.appId}: ${error.message}`, { cause: error });
  }

  const candidateConfig = pathToFileURL(resolve(WEB_ROOT, 'scripts/direct-windows-candidate-config.mjs'));
  const { default: createCandidate } = await import(candidateConfig.href);
  const candidate = await createCandidate();
  if (candidate.extraMetadata?.inkstoneRuntime !== CANDIDATE_MODE) {
    throw new Error('Cordis requires the independent Windows verification mode');
  }
  // Inkstone binds its signed handoff and installer registration to this identity.
  if (
    candidate.appId !== IDENTITY.appId ||
    candidate.win?.executableName !== IDENTITY.executableName ||
    candidate.extraMetadata?.name !== IDENTITY.packageName
  ) {
    throw new Error(`Inkstone's independent Windows config must select ${IDENTITY.appId} before packaging`);
  }
  return candidate;
}

export default async function inkstoneCordisWinConfig() {
  if (process.platform !== 'win32' || process.arch !== 'x64') {
    throw new Error('inkstone-cordis-win-config requires a Windows x64 host');
  }
  if (process.env.INKSTONE_CORE_RUNTIME_BUILD_PROFILE === 'release') {
    throw new Error('Parallel cordis canary config must never run under release profile');
  }

  const electronDist = process.env.ELECTRON_CORDIS_DIST?.trim();
  const dryRun = process.env.ELECTRON_CORDIS_ALLOW_STOCK_DRYRUN === '1';
  if (!electronDist && !dryRun) {
    throw new Error(
      'ELECTRON_CORDIS_DIST is required (or set ELECTRON_CORDIS_ALLOW_STOCK_DRYRUN=1 for config dry-run)'
    );
  }
  const slimPack = process.env.ELECTRON_CORDIS_SLIM_PACK === '1';
  if (slimPack && electronDist) {
    throw new Error(
      'Slim Inkstone packaging requires runtime support for loading Science Pack, Lark CLI and SSH on demand. ' +
      'Use the stock config dry run to preview resource exclusions.'
    );
  }
  const stock = parse(
    await readFile(resolve(WEB_ROOT, 'electron-builder.yml'), 'utf8')
  );
  const validatedDist = electronDist && await validateInkstoneElectronDist(
    WEB_ROOT, electronDist, 'win32', 'x64', stock.electronVersion
  );
  const base = electronDist && !dryRun ? await loadWindowsCandidate(stock) : stock;

  const filterExtra = (list = []) => list.filter(entry => includeInkstoneResource(entry, 'win32', slimPack));

  return {
    ...base,
    appId: IDENTITY.appId,
    productName: '书生·端砚 Cordis',
    ...(validatedDist ? { electronDist: validatedDist } : {}),
    directories: {
      ...base.directories,
      output: 'dist/cordis-win',
    },
    publish: {
      ...base.publish,
      channel: 'cordis-canary',
    },
    extraMetadata: {
      ...base.extraMetadata,
      name: IDENTITY.packageName,
      inkstoneRuntime: slimPack ? 'electron-cordis-win-slim' : CANDIDATE_MODE,
      electronCordisBase: true,
    },
    extraResources: filterExtra(base.extraResources),
    win: {
      ...base.win,
      executableName: IDENTITY.executableName,
      extraResources: filterExtra(base.win?.extraResources),
      target: process.env.ELECTRON_CORDIS_NSIS === '1'
        ? [{ target: 'nsis', arch: ['x64'] }]
        : [{ target: 'dir', arch: ['x64'] }],
    },
  };
}
