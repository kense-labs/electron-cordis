/**
 * Parallel Windows x64 packaging config for Inkstone on Electron-Cordis.
 *
 * Keeps native Electron 43.2.0 Windows builds 100% untouched:
 * - Isolated appId (`cn.internai.inkstone.cordis`) & productName for side-by-side install
 * - Isolated output directory (`dist/cordis-win`) and update channel (`cordis-canary`)
 * - Removes legacy Go `runtime/orbit-*` resources
 * - Supports `ELECTRON_CORDIS_SLIM_PACK=1` to exclude `on-demand` packs (`science-pack`, `feishu-cli`, `ssh-native`)
 *
 * Usage (from ../web on Windows x64):
 *   ELECTRON_CORDIS_DIST=C:\path\to\prebuilt-electron-cordis-win32-x64 \
 *     pnpm exec electron-builder --win dir --x64 \
 *     --config ../electron-cordis/script/inkstone-cordis-win-config.mjs --publish never
 */
import { existsSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { createRequire } from 'node:module';

const WEB_ROOT = resolve(import.meta.dirname, '../../web');
const requireFromWeb = createRequire(resolve(WEB_ROOT, 'package.json'));
const YAML = requireFromWeb('yamljs');

export default async function inkstoneCordisWinConfig() {
  if (process.platform !== 'win32' || process.arch !== 'x64') {
    throw new Error('inkstone-cordis-win-config requires a Windows x64 host');
  }
  if (process.env.INKSTONE_CORE_RUNTIME_BUILD_PROFILE === 'release') {
    throw new Error('Parallel cordis canary config must never run under release profile');
  }

  const electronDist = process.env.ELECTRON_CORDIS_DIST?.trim();
  if (!electronDist && process.env.ELECTRON_CORDIS_ALLOW_STOCK_DRYRUN !== '1') {
    throw new Error(
      'ELECTRON_CORDIS_DIST is required (or set ELECTRON_CORDIS_ALLOW_STOCK_DRYRUN=1 for config dry-run)'
    );
  }
  if (electronDist && !existsSync(electronDist)) {
    throw new Error(`ELECTRON_CORDIS_DIST does not exist: ${electronDist}`);
  }

  const slimPack = process.env.ELECTRON_CORDIS_SLIM_PACK === '1';
  const base = YAML.parse(
    await readFile(resolve(WEB_ROOT, 'electron-builder.yml'), 'utf8')
  );

  const filterExtra = (list = []) =>
    list.filter((entry) => {
      if (entry.to === 'runtime') return false;
      if (slimPack && ['science-pack', 'science-mcp', 'feishu-cli', 'ssh-native'].includes(entry.to)) {
        return false;
      }
      return true;
    });

  return {
    ...base,
    appId: 'cn.internai.inkstone.cordis',
    productName: '书生·端砚 Cordis',
    ...(electronDist ? { electronDist } : {}),
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
      inkstoneRuntime: slimPack ? 'electron-cordis-win-slim' : 'electron-cordis-win-canary',
      electronCordisBase: true,
    },
    extraResources: filterExtra(base.extraResources),
    win: {
      ...base.win,
      executableName: 'inkstone-cordis',
      extraResources: filterExtra(base.win?.extraResources),
      target: process.env.ELECTRON_CORDIS_NSIS === '1'
        ? [{ target: 'nsis', arch: ['x64'] }]
        : [{ target: 'dir', arch: ['x64'] }],
    },
  };
}
