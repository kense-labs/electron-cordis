import { lstat, readFile, readdir } from 'node:fs/promises';
import { isAbsolute, join, relative, resolve } from 'node:path';
import { parseArgs } from 'node:util';
import { parse } from 'yaml';
import { validateInkstoneElectronDist } from './inkstone-electron-dist.mjs';
import { includeInkstoneResource, inkstoneWebRoot } from './inkstone-package-resources.mjs';

const platforms = { darwin: { config: 'mac', arch: 'arm64' }, win32: { config: 'win', arch: 'x64' } };
const invocationDir = process.env.CORDIS_PARITY_CWD || process.cwd();

function isLiteralPath(pattern) {
  return typeof pattern === 'string' && pattern.length > 0 && !isAbsolute(pattern) &&
    !pattern.split(/[/\\]/u).includes('..') && !Array.from('*?!{}[]').some(character => pattern.includes(character));
}

async function measure(path, source, totals, visited) {
  if (visited.has(path)) return;
  visited.add(path);
  let info;
  try {
    info = await lstat(path);
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
    totals.missing.push(relative(source, path) || '.');
    return;
  }
  if (info.isDirectory()) {
    for (const name of await readdir(path)) {
      await measure(join(path, name), source, totals, visited);
    }
  } else if (info.isFile()) {
    totals.regularFiles += 1;
    totals.regularFileBytes += info.size;
  } else if (info.isSymbolicLink()) {
    totals.symlinks += 1;
  }
}

async function describeExcluded(root, entry) {
  const { from, to, filter } = entry;
  const settings = { from, to };
  const filters = Array.isArray(filter) ? filter : filter ? [filter] : [];
  if (typeof from !== 'string' || !filters.every(isLiteralPath)) {
    return { ...settings, status: 'requires-builder-file-selection' };
  }
  const source = resolve(root, from);
  const totals = { regularFiles: 0, regularFileBytes: 0, symlinks: 0, missing: [] };
  const visited = new Set();
  const targets = filters.length ? filters.map(path => resolve(source, path)) : [source];
  for (const path of targets) {
    await measure(path, source, totals, visited);
  }
  return { ...settings, ...totals, status: totals.missing.length ? 'missing-source-files' : 'measured' };
}

async function main() {
  const { values } = parseArgs({
    options: {
      'web-root': { type: 'string' },
      platform: { type: 'string' },
      distribution: { type: 'string' },
    },
  });
  const platform = values.platform || process.platform;
  if (!Object.hasOwn(platforms, platform)) throw new Error('Set --platform to darwin or win32');
  const { config, arch } = platforms[platform];
  const webRoot = values['web-root'] ? resolve(invocationDir, values['web-root']) : inkstoneWebRoot();
  const base = parse(await readFile(join(webRoot, 'electron-builder.yml'), 'utf8'));
  const pkg = JSON.parse(await readFile(join(webRoot, 'package.json'), 'utf8'));
  const distribution = values.distribution && await validateInkstoneElectronDist(
    webRoot, resolve(invocationDir, values.distribution), platform, arch, base.electronVersion
  );
  const resources = [...(base.extraResources ?? []), ...(base[config]?.extraResources ?? [])];
  const standard = resources.filter(entry => includeInkstoneResource(entry, platform, false));
  const slim = new Set(resources.filter(entry => includeInkstoneResource(entry, platform, true)));
  const excluded = [];
  for (const entry of standard) {
    if (!slim.has(entry)) excluded.push(await describeExcluded(webRoot, entry));
  }
  console.log(JSON.stringify({
    webRoot, platform, architecture: arch, electronVersion: pkg.devDependencies?.electron,
    distribution: distribution || null,
    measurement: 'Logical bytes of regular files in stock-config resource sources',
    retainedResources: standard.filter(entry => slim.has(entry)).map(entry => entry.to).filter(Boolean),
    excluded,
    measuredRegularFileBytes: excluded.reduce((sum, entry) => sum + (entry.regularFileBytes ?? 0), 0),
    complete: excluded.every(entry => entry.status === 'measured'),
  }, null, 2));
}

try {
  await main();
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
