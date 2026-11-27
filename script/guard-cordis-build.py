#!/usr/bin/env python3
"""Run a macOS Chromium build subject to an available-memory threshold."""

import argparse
from contextlib import ExitStack, nullcontext
import fcntl
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time


BUILD_SLOT_DIRECTORY = Path(__file__).resolve().parents[2] / 'out'
BUILD_SLOT_COUNT = 2
BUILD_START_INTERVAL = 25
SISO_GO_SOFT_MEMORY_LIMIT = '900MiB'
TEMPORARY_FAILURE = 75


def log(message):
    print(time.strftime('%F %T'), message, flush=True)


def memory():
    level = subprocess.check_output(
        ['/usr/sbin/sysctl', '-n', 'kern.memorystatus_vm_pressure_level'], text=True, timeout=4,
    ).strip()
    report = subprocess.check_output(['/usr/bin/memory_pressure', '-Q'], text=True, timeout=4)
    free_percentage = re.search(r'memory free percentage:\s*(\d+)%', report)
    if not free_percentage:
        raise RuntimeError('Could not read available memory from memory_pressure.')
    return level, int(free_percentage[1])


def needs_build_admission(command):
    program = Path(command[0]).name
    return (program == 'siso' and command[1:2] == ['ninja']) or (
        program == 'just' and len(command) > 1
        and (command[1].startswith('build-electron') or command[1] == 'verify-desktop-specs')
    )


def try_lock(lifetime, path):
    lock = path.open('a')
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        lock.close()
        return None
    except BaseException:
        lock.close()
        raise
    return lifetime.enter_context(lock).fileno()


def group_exists(job):
    job.poll()
    try:
        os.killpg(job.pid, 0)
    except ProcessLookupError:
        return False
    return True


def stop_build(job, *, interrupt_timeout=30, terminate_timeout=10):
    for sig, grace in ((signal.SIGINT, interrupt_timeout), (signal.SIGTERM, terminate_timeout), (signal.SIGKILL, 5)):
        try:
            os.killpg(job.pid, sig)
        except ProcessLookupError:
            break
        deadline = time.monotonic() + grace
        while time.monotonic() < deadline and group_exists(job):
            time.sleep(0.1)
        if not group_exists(job):
            break
    job.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--reserve', type=int, default=35)
    parser.add_argument('--start-at', type=int)
    parser.add_argument('--log', type=Path, required=True)
    parser.add_argument('--output-lock', type=Path)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    if not command:
        parser.error('supply a build command')
    start_at = min(100, args.reserve + 10) if args.start_at is None else args.start_at
    if not 0 <= args.reserve <= start_at <= 100:
        parser.error('memory percentages must satisfy 0 <= reserve <= start-at <= 100')
    if sys.platform != 'darwin':
        parser.error('the memory monitor requires macOS')

    requested_signal = None

    def request_stop(signum, _frame):
        nonlocal requested_signal
        requested_signal = signum

    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, request_stop)

    admits_build = args.output_lock is not None or needs_build_admission(command)
    with ExitStack() as lifetime:
        descriptors = []
        if args.output_lock is not None:
            args.output_lock.parent.mkdir(parents=True, exist_ok=True)
            target_lock = try_lock(lifetime, args.output_lock)
            if target_lock is None:
                log(f'Build deferred while its output directory is in use ({args.output_lock.name}).')
                return TEMPORARY_FAILURE
            descriptors.append(target_lock)

        admission = nullcontext()
        if admits_build:
            BUILD_SLOT_DIRECTORY.mkdir(parents=True, exist_ok=True)
            for index in range(BUILD_SLOT_COUNT):
                slot = try_lock(lifetime, BUILD_SLOT_DIRECTORY / f'.cordis-build-slot-{index}.lock')
                if slot is not None:
                    descriptors.append(slot)
                    break
            else:
                log('Build deferred while all heavy build slots are occupied.')
                return TEMPORARY_FAILURE
            admission = (BUILD_SLOT_DIRECTORY / '.cordis-build-start.lock').open('a+')

        job = None
        try:
            with admission as record:
                if record is not None:
                    fcntl.flock(record, fcntl.LOCK_EX)
                    record.seek(0)
                    try:
                        elapsed = time.monotonic() - float(record.read())
                    except ValueError:
                        elapsed = BUILD_START_INTERVAL
                    if 0 <= elapsed < BUILD_START_INTERVAL:
                        log('Build start deferred while a previously launched build is loading.')
                        return TEMPORARY_FAILURE

                level, available = memory()
                if level != '1' or available < start_at:
                    log(f'Preflight deferred: pressure={level}, available={available}%')
                    return TEMPORARY_FAILURE
                if requested_signal is not None:
                    return TEMPORARY_FAILURE
                log(f'Preflight passed: pressure={level}, available={available}%')
                env = dict(os.environ)
                if admits_build and not env.get('GOMEMLIMIT'):
                    env['GOMEMLIMIT'] = SISO_GO_SOFT_MEMORY_LIMIT
                args.log.parent.mkdir(parents=True, exist_ok=True)
                with args.log.open('ab', buffering=0) as output:
                    job = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT,
                                           start_new_session=True, env=env, pass_fds=descriptors)
                last = time.monotonic()
                if record is not None:
                    record.seek(0)
                    record.truncate()
                    record.write(f'{last}\n')

            log(f'Build started: PID {job.pid}. Build output is in {args.log}')
            while job.poll() is None:
                if requested_signal is not None:
                    raise InterruptedError(f'Monitor received signal {requested_signal}')
                level, available = memory()
                if level != '1' or available < args.reserve:
                    raise InterruptedError(f'Memory guard stopped build: pressure={level}, available={available}%')
                if time.monotonic() - last >= 60:
                    log(f'pressure={level}, available={available}%')
                    last = time.monotonic()
                try:
                    job.wait(timeout=4)
                except subprocess.TimeoutExpired:
                    continue
            log(f'Build exited {job.returncode}')
            return job.returncode if job.returncode >= 0 else 128 - job.returncode
        except (OSError, RuntimeError, subprocess.SubprocessError) as error:
            if job is None and not isinstance(error, (RuntimeError, subprocess.SubprocessError)):
                raise
            log(str(error))
            if job is not None:
                stop_build(job)
            return TEMPORARY_FAILURE


if __name__ == '__main__':
    sys.exit(main())
