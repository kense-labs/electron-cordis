import { execFileSync, spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { constants, existsSync } from 'node:fs';
import { access, mkdir, mkdtemp, readFile, realpath, rename, rm, stat } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { isAbsolute, join, resolve } from 'node:path';

const root = resolve(import.meta.dirname, '..');
const invocationDir = process.env.CORDIS_DEV_CWD || process.cwd();
const require = createRequire(import.meta.url);
const windows = process.platform === 'win32';
const executableSuffix = windows ? '.exe' : '';
const python = windows ? 'python' : 'python3';
const pythonOptions = windows ? ['-X', 'utf8'] : [];
const localOut = process.env.ELECTRON_CORDIS_CHECKOUT
  ? resolve(invocationDir, process.env.ELECTRON_CORDIS_CHECKOUT, 'src', 'out')
  : existsSync(join(root, '..', 'chrome', 'VERSION'))
    ? resolve(root, '..', 'out')
    : null;

async function isFileAvailable(path, mode) {
  try {
    await access(path, mode);
    const file = await stat(path);
    return file.isFile() && file.size > 0;
  } catch {
    return false;
  }
}

async function isLocalBuildAvailable(directory, binary) {
  const executables = [binary];
  const dataFiles = [];
  let resources = directory;
  if (process.platform === 'darwin') {
    const contents = join(directory, 'Electron.app', 'Contents');
    const frameworks = join(contents, 'Frameworks');
    const framework = join(frameworks, 'Electron Framework.framework');
    executables.push(
      join(framework, 'Electron Framework'),
      join(frameworks, 'Electron Helper (Renderer).app', 'Contents', 'MacOS', 'Electron Helper (Renderer)')
    );
    dataFiles.push(join(contents, 'Info.plist'));
    resources = join(framework, 'Resources');
  }
  if (windows) dataFiles.push(join(directory, 'ffmpeg.dll'));
  dataFiles.push(join(resources, 'icudtl.dat'), join(resources, 'resources.pak'));
  const present = await Promise.all([
    ...executables.map((path) => isFileAvailable(path, constants.X_OK)),
    ...dataFiles.map((path) => isFileAvailable(path, constants.R_OK))
  ]);
  return present.every(Boolean);
}

function loadInstaller() {
  try {
    return require('@electron/fiddle-core').Installer;
  } catch (error) {
    if (error.code !== 'MODULE_NOT_FOUND') throw error;
  }

  const { packageManager } = require('../package.json');
  const yarnVersion = packageManager.split('@')[1];
  execFileSync(process.execPath, [`.yarn/releases/yarn-${yarnVersion}.cjs`, 'install', '--immutable'], {
    cwd: root,
    stdio: 'inherit'
  });
  return require('@electron/fiddle-core').Installer;
}

async function resolveElectron() {
  if (process.env.ELECTRON_CORDIS_BINARY) {
    const binary = resolve(invocationDir, process.env.ELECTRON_CORDIS_BINARY);
    if (await isFileAvailable(binary, constants.X_OK)) return { binary, source: 'configured build' };
    throw new Error(`ELECTRON_CORDIS_BINARY is not executable: ${binary}`);
  }

  const outputDir = process.env.ELECTRON_OUT_DIR;
  const executable = process.platform === 'darwin'
    ? 'Electron.app/Contents/MacOS/Electron'
    : `electron${executableSuffix}`;
  if (outputDir && !localOut && !isAbsolute(outputDir)) {
    throw new Error('A relative ELECTRON_OUT_DIR requires the gclient checkout. Set ELECTRON_CORDIS_CHECKOUT to its root.');
  }
  const directories = outputDir ? [resolve(localOut ?? invocationDir, outputDir)] : [];
  if (!outputDir && localOut) {
    directories.push(...['Testing', 'Release', 'Default', 'Debug'].map((directory) => join(localOut, directory)));
  }
  for (const directory of directories) {
    const binary = join(directory, executable);
    if (await isLocalBuildAvailable(directory, binary)) return { binary, source: 'local build' };
  }
  if (outputDir) throw new Error(`No complete Electron build in ${directories[0]}`);

  const version = (await readFile(join(root, 'build', 'electron-cordis-upstream-version'), 'utf8')).trim();
  const cache = join(root, 'dist', 'cordis-dev');
  const platformVersion = `${version}-${process.platform}-${process.arch}`;
  const installDir = join(cache, platformVersion);
  const binary = join(installDir, executable);
  const source = `official Electron ${version}`;
  if (await isLocalBuildAvailable(installDir, binary)) return { binary, source };

  const Installer = loadInstaller();
  await mkdir(cache, { recursive: true });
  const staging = await mkdtemp(join(cache, 'install-'));
  try {
    const install = join(staging, 'electron');
    const installer = new Installer({ electronDownloads: join(staging, 'downloads'), electronInstall: install });
    console.log(`[cordis-dev] Installing ${source} for ${process.platform}-${process.arch}`);
    const stagedBinary = await installer.install(version);
    if (!(await isLocalBuildAvailable(install, stagedBinary))) {
      throw new Error(`Electron installation was incomplete: ${install}`);
    }
    try {
      await rename(install, installDir);
    } catch (error) {
      const occupied = ['EEXIST', 'ENOTEMPTY'].includes(error.code) ||
        (windows && ['EACCES', 'EPERM'].includes(error.code));
      if (!occupied || !(await isLocalBuildAvailable(installDir, binary))) throw error;
    }
    return { binary, source };
  } finally {
    await rm(staging, { recursive: true, force: true });
  }
}

function signalGroup(pid, signal) {
  try {
    process.kill(-pid, signal);
  } catch (error) {
    if (error.code !== 'ESRCH') throw error;
  }
}

async function supervise(supervisor, args, env) {
  const child = spawn(supervisor, args, {
    cwd: invocationDir,
    env,
    stdio: ['pipe', 'inherit', 'inherit'],
    detached: !windows,
    windowsHide: true
  });
  child.stdin?.on('error', (error) => {
    if (!['EPIPE', 'ERR_STREAM_DESTROYED'].includes(error.code)) {
      console.error(`[cordis-dev] Supervisor control pipe: ${error.message}`);
    }
  });
  let interrupted = false;
  let timeout;
  const stop = () => {
    if (interrupted || !child.pid) return;
    interrupted = true;
    child.stdin.end();
    timeout = setTimeout(() => {
      if (windows) child.kill('SIGKILL');
      else signalGroup(child.pid, 'SIGKILL');
    }, 20_000);
  };
  process.on('SIGINT', stop);
  process.on('SIGTERM', stop);
  try {
    const code = await new Promise((resolve, reject) => {
      child.once('error', reject);
      child.once('exit', resolve);
    });
    // A successful supervisor exit confirms that it has already stopped its children.
    if (!windows && child.pid && code !== 0) signalGroup(child.pid, 'SIGTERM');
    return interrupted ? 130 : (code ?? 1);
  } finally {
    clearTimeout(timeout);
    process.off('SIGINT', stop);
    process.off('SIGTERM', stop);
  }
}

async function main() {
  if (!['darwin', 'linux', 'win32'].includes(process.platform)) {
    throw new Error('Cordis desktop development requires macOS, Linux, or Windows.');
  }
  const [app = join(root, 'script', 'cordis-dev-app'), ...appArgs] = process.argv.slice(2);
  const appPath = resolve(invocationDir, app);
  if (!existsSync(appPath)) throw new Error(`Electron app entry point does not exist: ${appPath}`);

  const runtimeDir = await mkdtemp(join(windows ? tmpdir() : '/tmp', 'electron-cordis-dev.'));
  try {
    const profile = resolve(
      invocationDir,
      process.env.ELECTRON_CORDIS_PROFILE || join(root, 'profiles', 'inkstone-desktop.cordis.json')
    );
    execFileSync(python, [...pythonOptions, join(root, 'script', 'gen-cordis-build.py'), profile, '--out-dir', runtimeDir], {
      stdio: 'inherit'
    });
    const plan = JSON.parse(await readFile(join(runtimeDir, 'build-plan.json'), 'utf8'));
    if (!plan.build_chromium) throw new Error(`Cordis profile ${plan.profile} has no Electron web surface.`);

    const electron = await resolveElectron();
    const binDir = execFileSync(
      python,
      [...pythonOptions, join(root, 'script', 'build-cordis-runtime.py'), join(runtimeDir, 'build-plan.json')],
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'] }
    ).trim();
    const socket = windows ? `\\\\.\\pipe\\electron-cordis-${randomUUID()}` : join(runtimeDir, 'cordis.sock');
    const env = {
      ...process.env,
      NODE_ENV: 'development',
      ELECTRON_CORDIS_SOCKET: socket,
      ELECTRON_CORDIS_DEV_STATE: await realpath(runtimeDir)
    };
    for (const key of Object.keys(env)) {
      if ((windows ? key.toUpperCase() : key) === 'ELECTRON_RUN_AS_NODE') delete env[key];
    }
    const args = [
      '--stop-on-stdin-eof',
      '--cleanup-dev-session',
      '--daemon-bin',
      join(binDir, `electron-cordis-daemon${executableSuffix}`),
      '--load-plan',
      join(env.ELECTRON_CORDIS_DEV_STATE, 'load-plan.json'),
      '--daemon-arg',
      '--socket',
      '--daemon-arg',
      socket,
      '--ui-bin',
      electron.binary,
      '--max-ui-restarts',
      '0',
      ...[appPath, ...appArgs].flatMap((arg) => ['--ui-arg', arg])
    ];
    console.log(`[cordis-dev] Starting ${appPath} with ${electron.source}`);
    console.log(`[cordis-dev] Cordis socket: ${socket}`);
    return await supervise(join(binDir, `electron-cordis-supervisor${executableSuffix}`), args, env);
  } finally {
    await rm(runtimeDir, { recursive: true, force: true, maxRetries: 3 });
  }
}

try {
  process.exitCode = await main();
} catch (error) {
  console.error(`[cordis-dev] ${error.message}`);
  process.exitCode = 1;
}
