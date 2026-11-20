#!/usr/bin/env python3
"""List, or gracefully stop, the current user's v3.4/v3.5/v3.6 matrix controller.

--stop sends SIGTERM once to the verified run_matrix.py process, waits for its
exit, then invokes the installed cleanup task for that process's result folder.
It never signals a Screen server or a process group and never uses SIGKILL.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import time

RUNNER_HASHES = {
    'b5d4990188b561e3b134837ed3e99b87982e84f7168ab8322b82fb5d4c4b7e4d',
    'fa35d4762c6d9873656f819abb2f0bf7bdf6c268f8e92d42e51aed4a71c2c884',
    'd3131c1fc4f7cf16b2efc7bd007f7de91b9ea54995c0c79736b45652995689be',
}
LIFECYCLE_HASH = '1b5664d2ef967d3db898ab5281962f54f92a46e8055be3ef2b0d6c2a518035ee'


def option(argv, name):
    for index, value in enumerate(argv):
        if value == name:
            return argv[index + 1]
        if value.startswith(name + '='):
            return value.split('=', 1)[1]
    raise ValueError(f'Runner does not provide {name}; no signal sent')


def inspect(pid, tools):
    process = Path('/proc') / str(pid)
    try:
        if process.stat().st_uid != os.getuid():
            return None
        argv = [s.decode() for s in (process / 'cmdline').read_bytes().split(b'\0') if s]
        if str(tools / 'run_matrix.py') not in argv or '--cleanup-only' in argv or '--dry-run' in argv:
            return None
        cwd = (process / 'cwd').resolve(strict=True)
        output = (cwd / option(argv, '--output')).resolve()
        root = (cwd / option(argv, '--root')).resolve()
        return dict(pid=pid, output=str(output), root=str(root), argv=argv)
    except (OSError, IndexError, UnicodeError):
        return None


def file_hash(path):
    return hashlib.sha256(path.read_text(encoding='utf-8').encode('utf-8')).hexdigest()


def stop(record, tools, timeout=900):
    if file_hash(tools / 'run_matrix.py') not in RUNNER_HASHES or file_hash(tools / 'lifecycle.py') != LIFECYCLE_HASH:
        raise ValueError('Installed controller is not a released v3.4/v3.5/v3.6 version; no signal sent')
    descriptor = os.pidfd_open(record['pid'])
    deadline, last_update = time.monotonic() + timeout, 0
    try:
        if inspect(record['pid'], tools) != record:
            raise ValueError('Runner changed during verification; no signal sent')
        sent = False
        while not select.select([descriptor], [], [], 1)[0]:
            if time.monotonic() >= deadline:
                raise ValueError('Runner has not exited after the timeout; inspect its terminal/cleanup logs before proceeding')
            destroying = False
            for path in Path(record['output']).glob('lifecycle/*/state.json'):
                state = json.loads(path.read_text(encoding='utf-8-sig'))
                destroying |= state.get('status') == 'destroying'
            if not sent and not destroying:
                print(f'STOPPING PID {record["pid"]}: SIGTERM once; waiting for archive and cleanup.', flush=True)
                try:
                    signal.pidfd_send_signal(descriptor, signal.SIGTERM)
                except ProcessLookupError:
                    break
                sent = True
            if time.monotonic() - last_update >= 15:
                print('Waiting for instance cleanup and controller exit...', flush=True)
                last_update = time.monotonic()
    finally:
        os.close(descriptor)
    command = ['fab', '-r', str(tools), 'cleanup', '--root', record['root'], '--output', record['output']]
    print('Controller exited. Checking recorded pools with fab cleanup.', flush=True)
    subprocess.run(command, check=True)
    print('STOPPED: controller exited; cleanup succeeded. Completed results are retained.', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--tools', type=Path, default=Path('/home/ubuntu/experiments/figure-tools'))
    parser.add_argument('--pid', type=int, help='Select one run_matrix.py PID if several are listed')
    parser.add_argument('--stop', action='store_true', help='Without this flag, only list matching main processes')
    args = parser.parse_args()
    if sys.platform != 'linux' or not hasattr(signal, 'pidfd_send_signal'):
        parser.error('Run with Python 3.9+ on Ubuntu as the experiment user')
    tools = args.tools.resolve()
    candidates = [args.pid] if args.pid else [int(p.name) for p in Path('/proc').iterdir() if p.name.isdigit()]
    records = [record for pid in candidates if (record := inspect(pid, tools)) is not None]
    for record in records:
        print(f'MAIN PID: {record["pid"]}\nOUTPUT: {record["output"]}\nCOMMAND: {" ".join(record["argv"])}', flush=True)
    if not records:
        print('No live run_matrix.py found; no signal sent.')
        return
    if args.stop:
        if len(records) != 1:
            parser.error('Multiple controllers found; choose the intended --pid. No signal sent.')
        stop(records[0], tools)


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print(f'Stop failed: {error}', file=sys.stderr)
        raise SystemExit(1)
