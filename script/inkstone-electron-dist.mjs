import { execFile } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { promisify } from 'node:util';

const run = promisify(execFile);

export async function validateInkstoneElectronDist(webRoot, input, platform, arch, configuredVersion) {
  const pkg = JSON.parse(await readFile(join(webRoot, 'package.json'), 'utf8'));
  const version = pkg.devDependencies?.electron;
  if (typeof version !== 'string' || !/^\d+\.\d+\.\d+(?:-[\da-z.-]+)?(?:\+[\da-z.-]+)?$/i.test(version)) {
    throw new Error('Inkstone must pin an exact Electron version in devDependencies.electron');
  }
  if (configuredVersion && configuredVersion !== version) {
    throw new Error(`electron-builder specifies Electron ${configuredVersion}, but Inkstone pins ${version}`);
  }
  const validator = join(import.meta.dirname, 'validate-inkstone-electron-dist.py');
  const args = [validator, resolve(webRoot, input), version, platform, arch];
  const options = { env: { ...process.env, PYTHONIOENCODING: 'utf-8' } };
  const windows = process.platform === 'win32';
  const result = run(windows ? 'python' : 'python3', args, options).catch(error => {
    if (windows && error.code === 'ENOENT') return run('python3', args, options);
    throw error;
  });
  try {
    return (await result).stdout.trimEnd();
  } catch (error) {
    throw new Error(error.stderr?.trim() || `Could not validate ELECTRON_CORDIS_DIST: ${error.message}`, { cause: error });
  }
}
