/**
 * Parallel macOS packaging config for Inkstone on Electron-Cordis.
 *
 * Keeps native Electron 43.2.0 Inkstone builds 100% untouched:
 * - Isolated appId (`cn.internai.inkstone.cordis`) & productName for side-by-side install
 * - Isolated output directory (`dist/cordis-macos`) and update channel (`cordis-canary`)
 * - Removes legacy Go `runtime/orbit-*` resources
 * - Previews the resources excluded by `ELECTRON_CORDIS_SLIM_PACK=1` in a stock config dry run
 *
 * Usage (from ../web):
 *   ELECTRON_CORDIS_DIST=/path/to/prebuilt-electron-cordis-darwin-arm64 \
 *     pnpm exec electron-builder --mac --arm64 \
 *     --config ../electron-cordis/script/inkstone-cordis-mac-config.mjs --publish never
 */
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { parse } from 'yaml';
import { validateInkstoneElectronDist } from './inkstone-electron-dist.mjs';
import { includeInkstoneResource, inkstoneWebRoot } from './inkstone-package-resources.mjs';

const WEB_ROOT = inkstoneWebRoot();

export default async function inkstoneCordisMacConfig() {
  if (process.platform !== 'darwin') {
    throw new Error('inkstone-cordis-mac-config requires a macOS host');
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
      'Slim Inkstone packaging requires runtime support for loading Science Pack and Lark CLI on demand. ' +
      'Use the stock config dry run to preview resource exclusions.'
    );
  }
  let base;
  if (electronDist && !dryRun) {
    const candidateConfig = pathToFileURL(resolve(WEB_ROOT, 'scripts/direct-mac-candidate-config.mjs'));
    const { default: createDirectCandidate } = await import(candidateConfig.href);
    base = await createDirectCandidate();
    if (base.extraMetadata?.inkstoneRuntime !== 'direct-mac-candidate') {
      throw new Error('Cordis requires the independent macOS candidate packaging mode');
    }
  } else {
    base = parse(await readFile(resolve(WEB_ROOT, 'electron-builder.yml'), 'utf8'));
  }
  const validatedDist = electronDist && await validateInkstoneElectronDist(
    WEB_ROOT, electronDist, 'darwin', 'arm64', base.electronVersion
  );
  const targets = process.env.ELECTRON_CORDIS_DMG === '1' ? ['dmg', 'zip'] : ['dir'];
  const compressDmg = base.afterAllArtifactBuild === 'scripts/compress-mac-dmg-ulmo.mjs';

  const filterExtra = (list = []) => list.filter(entry => includeInkstoneResource(entry, 'darwin', slimPack));

  return {
    ...base,
    appId: 'cn.internai.inkstone.cordis',
    productName: '书生·端砚 Cordis',
    ...(validatedDist ? { electronDist: validatedDist } : {}),
    directories: {
      ...base.directories,
      output: 'dist/cordis-macos',
    },
    publish: {
      ...base.publish,
      channel: 'cordis-canary',
    },
    extraMetadata: {
      ...base.extraMetadata,
      inkstoneRuntime: slimPack ? 'electron-cordis-mac-slim' : 'direct-mac-candidate',
      electronCordisBase: true,
    },
    extraResources: filterExtra(base.extraResources),
    // Inkstone can recompress the installer after electron-builder has hashed its update files.
    // The ZIP is the macOS update payload and keeps electron-builder's matching block map.
    dmg: { ...base.dmg, writeUpdateInfo: false },
    ...(compressDmg && {
      async afterAllArtifactBuild(result) {
        if (result.configuration.dmg?.sign === true) return [];
        const hook = pathToFileURL(resolve(WEB_ROOT, base.afterAllArtifactBuild));
        const { default: compress } = await import(hook.href);
        return compress(result);
      },
    }),
    mac: {
      ...base.mac,
      identity: process.env.ELECTRON_CORDIS_SIGN_IDENTITY || base.mac?.identity || '-',
      extraResources: filterExtra(base.mac?.extraResources),
      target: targets.map(target => ({ target, arch: ['arm64'] })),
    },
  };
}
