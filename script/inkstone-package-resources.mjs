import { existsSync } from 'node:fs';
import { resolve } from 'node:path';

const defaultWebRoot = existsSync(resolve(import.meta.dirname, '../../chrome/VERSION'))
  ? '../../../../inkstone/web'
  : '../../web';

export function inkstoneWebRoot() {
  return resolve(process.env.INKSTONE_WEB_ROOT || resolve(import.meta.dirname, defaultWebRoot));
}

const onDemand = {
  darwin: new Set(['science-pack', 'feishu-cli']),
  win32: new Set(['science-pack', 'feishu-cli', 'ssh-native']),
};

export function includeInkstoneResource(entry, platform, slimPack) {
  return entry.to !== 'runtime' && !(slimPack && onDemand[platform].has(entry.to));
}
