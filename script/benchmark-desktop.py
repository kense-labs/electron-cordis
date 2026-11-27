#!/usr/bin/env python3
"""Compare startup and process-tree memory for two macOS Electron ZIPs."""

import argparse
from contextlib import suppress
import ctypes
import errno
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import signal
from statistics import median
import subprocess
import sys
from tempfile import TemporaryDirectory
import time
from zipfile import BadZipFile, ZipFile


EXECUTABLE = Path('Electron.app/Contents/MacOS/Electron')
FIXTURE = Path(__file__).resolve().parent / 'desktop-parity/benchmark.mjs'
SAMPLE_INTERVAL = 0.05
SETTLE_SECONDS = 0.5


class RusageInfoV0(ctypes.Structure):
    # RUSAGE_INFO_V0 from the macOS SDK's sys/resource.h.
    _fields_ = [('uuid', ctypes.c_uint8 * 16)] + [
        (name, ctypes.c_uint64) for name in (
            'user_time', 'system_time', 'pkg_idle_wkups', 'interrupt_wkups', 'pageins',
            'wired_size', 'resident_size', 'phys_footprint', 'proc_start_abstime', 'proc_exit_abstime',
        )
    ]


def process_sampler():
    pid_rusage = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True).proc_pid_rusage
    pid_rusage.argtypes = (ctypes.c_int, ctypes.c_int, ctypes.POINTER(RusageInfoV0))
    pid_rusage.restype = ctypes.c_int

    def sample(root_pid):
        output = subprocess.check_output(
            ['/bin/ps', '-axo', 'pid=,ppid=,rss=,time='], text=True, timeout=10,
        )
        rows = [line.split() for line in output.splitlines() if line.strip()]
        tree = {root_pid}
        while True:
            children = {int(pid) for pid, parent, *_ in rows if int(parent) in tree} - tree
            if not children:
                break
            tree.update(children)
        usage = {}
        for pid, _, rss, cpu in rows:
            pid = int(pid)
            if pid not in tree:
                continue
            info = RusageInfoV0()
            if pid_rusage(pid, 0, ctypes.byref(info)) != 0:
                error = ctypes.get_errno()
                if error == errno.ESRCH:
                    continue
                raise OSError(error, f'Could not sample Electron process {pid}: {os.strerror(error)}')
            seconds = sum(float(part) * 60 ** position
                          for position, part in enumerate(reversed(cpu.split(':'))))
            usage[(pid, info.proc_start_abstime)] = (int(rss), info.phys_footprint, seconds)
        return usage

    return sample


def measure_launch(binary, directory, version, arch, sample_processes):
    directory.mkdir()
    profile = directory / 'profile'
    profile.mkdir()
    ready = directory / 'ready.json'
    env = {name: value for name, value in os.environ.items()
           if name not in ('ELECTRON_RUN_AS_NODE', 'NODE_OPTIONS')}
    log = directory / 'runtime.log'
    with log.open('wb') as output:
        started = time.monotonic()
        job = subprocess.Popen(
            [binary, FIXTURE, profile, ready], env=env, stdin=subprocess.PIPE,
            stdout=output, stderr=output, start_new_session=True,
        )
        peak_rss = 0
        peak_footprint = 0
        cpu_by_process = {}

        def sample_usage():
            nonlocal peak_rss, peak_footprint
            usage = sample_processes(job.pid)
            rss = sum(rss for rss, _, _ in usage.values())
            footprint = sum(footprint for _, footprint, _ in usage.values())
            peak_rss = max(peak_rss, rss)
            peak_footprint = max(peak_footprint, footprint)
            cpu_by_process.update({process: cpu for process, (_, _, cpu) in usage.items()})
            return usage, rss, footprint

        try:
            next_sample = started
            while not ready.is_file():
                now = time.monotonic()
                if job.poll() is not None or now - started > 45:
                    raise RuntimeError('Electron did not paint the benchmark window')
                if now >= next_sample:
                    sample_usage()
                    next_sample = now + SAMPLE_INTERVAL
                time.sleep(0.005)
            elapsed_ms = (time.monotonic() - started) * 1_000
            result = json.loads(ready.read_text())
            if result != {'pid': job.pid, 'version': version, 'arch': arch, 'page': 'cordis-benchmark'}:
                raise RuntimeError(f'Unexpected benchmark renderer output: {result}')

            deadline = time.monotonic() + SETTLE_SECONDS
            while (remaining := deadline - time.monotonic()) > 0:
                sample_usage()
                time.sleep(min(SAMPLE_INTERVAL, remaining))
            usage, rss, footprint = sample_usage()
            if len(usage) < 2 or not any(pid == job.pid for pid, _ in usage):
                raise RuntimeError('Expected a running Electron browser and renderer process')
            job.communicate(b'quit\n', timeout=15)
            if job.returncode:
                raise RuntimeError(f'Benchmark Electron exited with status {job.returncode}')
            return {
                'startupMs': round(elapsed_ms, 2), 'processes': len(usage), 'rssKiB': rss,
                'sampledPeakRssKiB': peak_rss, 'footprintBytes': footprint,
                'sampledPeakFootprintBytes': peak_footprint,
                'sampledCpuMs': round(sum(cpu_by_process.values()) * 1000, 2),
            }
        except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
            tail = log.read_text(errors='replace')[-2000:]
            raise RuntimeError(f'Benchmark failed for {binary}: {error}\n{tail}') from error
        finally:
            if job.poll() is None:
                with suppress(ProcessLookupError):
                    os.killpg(job.pid, signal.SIGTERM)
                try:
                    job.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    with suppress(ProcessLookupError):
                        os.killpg(job.pid, signal.SIGKILL)
                    job.wait(timeout=5)
            job.stdin.close()


def benchmark_electron_variants(variants, *, version, rounds=3, directory=None):
    if sys.platform != 'darwin':
        raise RuntimeError('Desktop performance comparison requires macOS')
    if not variants or rounds < 1:
        raise ValueError('Provide at least one Electron and one measurement round')
    arch = {'arm64': 'arm64', 'x86_64': 'x64'}[platform.machine()]
    names = list(variants)
    results = {name: {'binary': str(binary), 'samples': []} for name, binary in variants.items()}
    sample_processes = process_sampler()
    with TemporaryDirectory(prefix='cordis-startup-', dir=directory) as folder:
        scratch = Path(folder)
        for index in range(rounds):
            offset = index % len(names)
            for name in names[offset:] + names[:offset]:
                sample = measure_launch(variants[name], scratch / f'{name}-{index}', version, arch, sample_processes)
                results[name]['samples'].append(sample)
    for result in results.values():
        samples = result['samples']
        result['median'] = {key: median(sample[key] for sample in samples) for key in samples[0]}
        result['startupP95Ms'] = sorted(sample['startupMs'] for sample in samples)[math.ceil(0.95 * rounds) - 1]
    return {
        'version': version, 'architecture': arch, 'rounds': rounds,
        'launch': 'new Electron process and user profile, using the existing OS file cache',
        'startup': 'process spawn to a loaded page and the first ready-to-show event',
        'memory': 'process-tree RSS and macOS physical footprint sampled about every 50 ms through 500 ms after readiness',
        'cpu': 'sum of the latest sampled cumulative CPU time for each observed process, in milliseconds',
        'variants': results,
    }


def benchmark_archives(archives, *, rounds=20, expected_digests=None, directory=None):
    if sys.platform != 'darwin':
        raise RuntimeError('Desktop performance comparison requires macOS')
    paths = {name: Path(archive).resolve(strict=True) for name, archive in archives.items()}
    if paths['upstream'].samefile(paths['candidate']):
        raise ValueError('Provide two distinct Electron ZIP files')
    variants, packages = {}, {}
    with TemporaryDirectory(prefix='cordis-release-benchmark-', dir=directory) as folder:
        scratch = Path(folder)
        for name, archive in paths.items():
            snapshot = scratch / f'{name}.zip'
            clone = subprocess.run(['/bin/cp', '-c', archive, snapshot], stderr=subprocess.DEVNULL)
            if clone.returncode != 0:
                shutil.copyfile(archive, snapshot)
            digest = hashlib.sha256()
            with snapshot.open('rb') as stream:
                for block in iter(lambda: stream.read(1024 * 1024), b''):
                    digest.update(block)
            checksum = digest.hexdigest()
            if expected_digests is not None and checksum != expected_digests[name]:
                raise RuntimeError(f'{name} ZIP differs from the package verified for compatibility: {archive}')
            with ZipFile(snapshot) as package:
                version = package.read('version').decode().strip()
            if not version:
                raise RuntimeError(f'Electron version is missing from {archive}')
            packages[name] = {'archive': str(archive), 'archiveSha256': checksum,
                              'archiveBytes': snapshot.stat().st_size, 'version': version}
            extracted = scratch / name
            subprocess.run(['/usr/bin/ditto', '-x', '-k', snapshot, extracted], check=True, timeout=90)
            variants[name] = extracted / EXECUTABLE
        if packages['upstream']['version'] != packages['candidate']['version']:
            raise RuntimeError('Electron versions differ between the two ZIPs')
        report = benchmark_electron_variants(
            variants, version=packages['upstream']['version'], rounds=rounds, directory=scratch,
        )
        for name, local in variants.items():
            output = subprocess.check_output(['/usr/bin/dwarfdump', '--uuid', local], text=True, timeout=15)
            uuids = {arch.strip('()'): uuid for _, uuid, arch, _ in
                     (line.split(maxsplit=3) for line in output.splitlines() if line.startswith('UUID: '))}
            if not uuids:
                raise RuntimeError(f'Could not read a Mach-O UUID for {name}')
            artifact = {**packages[name], 'binary': EXECUTABLE.as_posix(), 'binaryUuids': uuids}
            report['variants'][name].update(artifact)
        return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('upstream', type=Path)
    parser.add_argument('candidate', type=Path)
    parser.add_argument('--rounds', type=int, default=20)
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error('--rounds must be positive')
    cwd = Path(os.environ.get('CORDIS_PARITY_CWD', os.getcwd()))
    try:
        report = benchmark_archives({'upstream': cwd / args.upstream, 'candidate': cwd / args.candidate},
                                    rounds=args.rounds)
    except (OSError, ValueError, KeyError, RuntimeError, BadZipFile, subprocess.SubprocessError) as error:
        parser.exit(1, f'{error}\n')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
