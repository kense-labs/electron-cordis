import { deepStrictEqual } from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { createHash } from 'node:crypto';
import { constants, createReadStream } from 'node:fs';
import { access, copyFile, mkdtemp, realpath, readFile, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { promisify } from 'node:util';

const run = promisify(execFile);
const invocationDir = process.env.CORDIS_PARITY_CWD || process.cwd();
const probe = join(import.meta.dirname, 'desktop-parity', 'main.mjs');
const archiveExecutable = {
  darwin: 'Electron.app/Contents/MacOS/Electron',
  linux: 'electron',
  win32: 'electron.exe'
}[process.platform];

async function inspectInput(name, filename, archiveMode) {
  const path = await realpath(resolve(invocationDir, filename));
  const info = await stat(path);
  const expected = archiveMode ? 'an Electron distribution ZIP' : 'an Electron executable';
  if (!info.isFile()) throw new Error(`${name} must be ${expected}: ${path}`);
  await access(path, archiveMode ? constants.R_OK : constants.X_OK);
  return { path, device: info.dev, inode: info.ino };
}

async function probeArchive(name, archive, workspace, addon) {
  const staging = await mkdtemp(join(workspace, `${name}-archive-`));
  try {
    const snapshot = join(staging, 'distribution.zip');
    await copyFile(archive, snapshot, constants.COPYFILE_FICLONE);
    const hash = createHash('sha256');
    for await (const data of createReadStream(snapshot)) hash.update(data);
    const extracted = join(staging, 'distribution');
    if (process.platform === 'darwin') {
      await run('/usr/bin/ditto', ['-x', '-k', snapshot, extracted], { timeout: 90000 });
    } else {
      const { extract } = await import('@electron-internal/extract-zip');
      await extract(snapshot, { dir: extracted });
    }
    const binary = join(extracted, archiveExecutable);
    await access(binary, constants.X_OK);
    return {
      report: await probeBinary(name, binary, workspace, addon),
      artifact: { archive, sha256: hash.digest('hex'), executable: archiveExecutable }
    };
  } finally {
    await rm(staging, { recursive: true, force: true, maxRetries: 5 });
  }
}

async function probeBinary(name, binary, workspace, addon) {
  const reportPath = join(workspace, `${name}.json`);
  const userData = join(workspace, `${name}-user-data`);
  const env = { ...process.env };
  for (const name of Object.keys(env)) {
    const key = process.platform === 'win32' ? name.toUpperCase() : name;
    if (key === 'ELECTRON_RUN_AS_NODE' || key === 'NODE_OPTIONS') delete env[name];
  }
  const args = [probe, userData, reportPath];
  if (addon) args.push(addon);
  try {
    await run(binary, args, { env, timeout: 45000, windowsHide: true });
    return JSON.parse(await readFile(reportPath, 'utf8'));
  } catch (error) {
    const status = error.signal ? ` (signal ${error.signal})`
      : error.code !== undefined ? ` (code ${error.code})` : '';
    throw new Error(`${name} probe failed for ${binary}${status}: ${error.stderr?.trim() || error.message}`);
  }
}

async function main() {
  const args = process.argv.slice(2);
  const archiveMode = args[0] === '--archives';
  if (archiveMode) args.shift();
  if (args.length < 2 || args.length > 3) {
    throw new Error('Usage: just verify-desktop <upstream-electron> <candidate-electron> [addon.node]\n' +
      '       just verify-desktop --archives <upstream.zip> <candidate.zip> [addon.node]');
  }
  if (archiveMode && !archiveExecutable) {
    throw new Error('Archive verification requires macOS, Linux, or Windows.');
  }
  const [reference, actual, addonPath] = args;
  const baseline = await inspectInput('Upstream', reference, archiveMode);
  const candidate = await inspectInput('Candidate', actual, archiveMode);
  const sameFile = baseline.inode !== 0 && baseline.device === candidate.device && baseline.inode === candidate.inode;
  if (baseline.path === candidate.path || sameFile) {
    throw new Error(`Provide distinct upstream and candidate ${archiveMode ? 'archives' : 'executables'}.`);
  }

  const addon = addonPath && await realpath(resolve(invocationDir, addonPath));
  if (addon && (!addon.endsWith('.node') || !(await stat(addon)).isFile())) {
    throw new Error(`Expected a native .node addon: ${addon}`);
  }
  const workspace = await mkdtemp(join(tmpdir(), 'cordis-desktop-parity-'));
  try {
    const inspect = async (name, path) => archiveMode
      ? probeArchive(name, path, workspace, addon)
      : { report: await probeBinary(name, path, workspace, addon), artifact: path };
    const upstream = await inspect('upstream', baseline.path);
    const candidateResult = await inspect('candidate', candidate.path);
    deepStrictEqual(candidateResult.report.versions, upstream.report.versions, 'Electron/Chromium/Node/V8/ABI baseline differs');
    deepStrictEqual(candidateResult.report, upstream.report, 'Desktop API or renderer behavior differs');
    console.log(JSON.stringify({
      upstream: upstream.artifact,
      candidate: candidateResult.artifact,
      addon: addon || null,
      report: candidateResult.report
    }, null, 2));
  } finally {
    await rm(workspace, { recursive: true, force: true, maxRetries: 5 });
  }
}

try {
  await main();
} catch (error) {
  console.error(error);
  process.exitCode = 1;
}
