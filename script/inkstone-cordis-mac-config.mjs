/**
 * Parallel macOS packaging config for Inkstone on Electron-Cordis.
 *
 * Keeps native Electron 43.2.0 Inkstone builds 100% untouched:
 * - Isolated appId (`cn.internai.inkstone.cordis`) & productName for side-by-side install
 * - Isolated output directory (`dist/cordis-macos`) and update channel (`cordis-canary`)
 * - Removes legacy Go `runtime/orbit-*` resources
 * - Supports `ELECTRON_CORDIS_SLIM_PACK=1` to exclude `on-demand` packs (`science-pack`, `feishu-cli`)
 *
 * Usage (from ../web):
 *   ELECTRON_CORDIS_DIST=/path/to/prebuilt-electron-cordis-darwin-arm64 \
 *     pnpm exec electron-builder --mac dir --arm64 \
 *     --config ../electron-cordis/script/inkstone-cordis-mac-config.mjs --publish never
 */
import { existsSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { createRequire } from 'node:module';

const WEB_ROOT = resolve(import.meta.dirname, '../../web');
const requireFromWeb = createRequire(resolve(WEB_ROOT, 'package.json'));
const YAML = requireFromWeb('yamljs');

export default async function inkstoneCordisMacConfig() {
  if (process.platform !== 'darwin') {
    throw new Error('inkstone-cordis-mac-config requires a macOS host');
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
      // Never bundle legacy Go orbit-* runtime
      if (entry.to === 'runtime') return false;
      // Slim installer offloads heavy domain packs to @dsh/plugin-installer (on-demand)
      if (slimPack && ['science-pack', 'science-mcp', 'feishu-cli'].includes(entry.to)) {
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
      output: 'dist/cordis-macos',
    },
    publish: {
      ...base.publish,
      channel: 'cordis-canary',
    },
    extraMetadata: {
      ...base.extraMetadata,
      inkstoneRuntime: slimPack ? 'electron-cordis-mac-slim' : 'electron-cordis-mac-canary',
      electronCordisBase: true,
    },
    extraResources: filterExtra(base.extraResources),
    mac: {
      ...base.mac,
      identity: process.env.ELECTRON_CORDIS_SIGN_IDENTITY || '-',
      extraResources: filterExtra(base.mac?.extraResources),
      target: process.env.ELECTRON_CORDIS_DMG === '1' ? ['dmg'] : ['dir'],
    },
  };
}
