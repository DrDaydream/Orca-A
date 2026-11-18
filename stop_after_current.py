#!/usr/bin/env python3
"""Stop a live released v2.1 campaign after its current case is archived.

Run separately on the Ubuntu controller. This does not edit running tools,
start experiments, or change results. The next case may start during polling;
that case can be retried with v3. The completed target case is preserved.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import sys
import time

V21_FILES = {
    'run_matrix.py': '374bf3e2feaa039a738a7e7d5945684b82b46da1bc49b61f415750cba7673cd0',
    'lifecycle.py': 'ff6c7e789bfbaa9f5b91a0efc974b8560023b632c6e761f1a57462af5428b678',
    'protocol_tasks.py': '1707147f9e6d77ee036a4df288f153fddf6efcf14d584a88478c3585700c590e',
}


def read(path):
    return json.loads(path.read_text(encoding='utf-8-sig'))


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def option(argv, name, default):
    for index, value in enumerate(argv):
        if value == name:
            return argv[index + 1]
        if value.startswith(name + '='):
            return value.split('=', 1)[1]
    return default


def inspect_process(pid, output):
    process = Path('/proc') / str(pid)
    try:
        if process.stat().st_uid != os.getuid():
            return None
        argv = [item.decode() for item in (process / 'cmdline').read_bytes().split(b'\0') if item]
        if '--cleanup-only' in argv or '--dry-run' in argv:
            return None
        scripts = [value for value in argv if Path(value).name == 'run_matrix.py']
        if len(scripts) != 1:
            return None
        cwd = (process / 'cwd').resolve(strict=True)
        actual_output = Path(option(argv, '--output', '/home/ubuntu/experiments/paper-results'))
        if not actual_output.is_absolute():
            actual_output = cwd / actual_output
        if actual_output.resolve() != output:
            return None
        script = Path(scripts[0])
        return (cwd / script).resolve()
    except (OSError, ValueError, IndexError, UnicodeError):
        return None


def find_runner(output, requested_pid):
    candidates = [requested_pid] if requested_pid else [int(p.name) for p in Path('/proc').iterdir() if p.name.isdigit()]
    matches = [(pid, script) for pid in candidates if (script := inspect_process(pid, output)) is not None]
    if len(matches) != 1:
        raise ValueError(f'Expected exactly one live run_matrix.py for {output}; found {len(matches)}. No signal sent.')
    pid, script = matches[0]
    fd = os.pidfd_open(pid)
    try:
        if inspect_process(pid, output) != script:
            raise ValueError('Runner changed while checking it. No signal sent.')
        for name, expected in V21_FILES.items():
            if sha(script.with_name(name)) != expected:
                raise ValueError(f'{name} is not the released v2.1 file. No signal sent.')
        return pid, fd
    except BaseException:
        os.close(fd)
        raise


def find_current(output):
    candidates = []
    for selected in output.glob('*/*/n*/r*/selected.json'):
        record = read(selected)
        if record.get('status') not in ('running', 'complete'):
            continue
        attempt = record.get('attempt', '')
        if not attempt.startswith('attempt-') or Path(attempt).name != attempt:
            continue
        archive = selected.parent / attempt
        if record['status'] == 'complete' and (archive / 'SHA256.json').is_file():
            continue
        metadata = archive / 'metadata.json'
        if not metadata.is_file():
            continue
        meta = read(metadata)
        case = record['case']
        state_path = output / 'lifecycle' / case['protocol'] / 'state.json'
        if not state_path.is_file():
            continue
        state = read(state_path)
        if (state.get('status') == 'running' and meta.get('testbed_scope') == state.get('scope')
                and meta.get('case') == case):
            candidates.append((selected, archive, case))
    if len(candidates) != 1:
        raise ValueError(f'Expected one active case; found {len(candidates)}. '
                         'If create/install or a case transition is in progress, retry when a RUN line appears. No signal sent.')
    return candidates[0]


def archive_complete(selected, archive, case):
    record = read(selected)
    if record.get('attempt') != archive.name or record.get('case') != case:
        raise ValueError('The selected attempt changed. No signal sent.')
    if record.get('status') in ('failed', 'interrupted'):
        return False
    manifest = archive / 'SHA256.json'
    if record.get('status') != 'complete' or not manifest.is_file():
        return False
    hashes = read(manifest)
    for name in ('measurement.json', 'metadata.json', 'summary.txt', 'console.log'):
        if name not in hashes or not (archive / name).is_file():
            raise ValueError(f'Incomplete archive: {name}. No signal sent.')
    for name in ('measurement.json', 'metadata.json', 'summary.txt'):
        if hashes[name] != sha(archive / name):
            raise ValueError(f'Archive hash mismatch: {name}. No signal sent.')
    for name in ('measurement.json', 'metadata.json'):
        saved = read(archive / name)
        if saved.get('status') != 'complete' or saved.get('case') != case:
            raise ValueError(f'Completed archive does not match case: {name}. No signal sent.')
    return True


def cleanup_finished(output):
    for path in output.glob('lifecycle/*/state.json'):
        record = read(path)
        if record.get('status') != 'terminated':
            return False
        if record.get('creation_started'):
            inventory = path.parent / record['scope'] / 'after-destroy.json'
            if not inventory.is_file():
                return False
            saved = read(inventory)
            if saved.get('scope') != record['scope'] or saved.get('instances') != [] or saved.get('security_groups') != []:
                return False
    return True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=Path('/home/ubuntu/experiments/paper-results-v2-fix1'))
    parser.add_argument('--pid', type=int, help='Optional run_matrix.py PID, never a Screen PID')
    args = parser.parse_args()
    if sys.platform != 'linux' or not hasattr(os, 'pidfd_open') or not hasattr(signal, 'pidfd_send_signal'):
        parser.error('Run with Python 3.9+ on the Ubuntu EC2 controller, as the experiment user.')
    output = args.output.resolve()
    pid, fd = find_runner(output, args.pid)
    try:
        selected, archive, case = find_current(output)
        key = selected.parent.relative_to(output).as_posix()
        print(f'WAITING: PID {pid}; current case {key}; archive {archive.name}', flush=True)
        print('Waiting for complete logs, summary and SHA256.json. No signal has been sent.', flush=True)
        sent = False
        complete = False
        while not select.select([fd], [], [], .2)[0]:
            if not complete:
                complete = archive_complete(selected, archive, case)
            if complete and not sent:
                states = [read(path).get('status') for path in output.glob('lifecycle/*/state.json')]
                # Do not intentionally interrupt an already-running destroy task.
                if 'destroying' in states:
                    continue
                print(f'ARCHIVED: {key}. Sending SIGTERM once to run_matrix.py PID {pid}; waiting for cleanup.', flush=True)
                try:
                    signal.pidfd_send_signal(fd, signal.SIGTERM)
                except ProcessLookupError:
                    break
                sent = True
        complete = archive_complete(selected, archive, case)
        if not complete:
            raise ValueError(f'Runner exited before {key} completed. Inspect its attempt and lifecycle logs before retrying.')
        if not cleanup_finished(output):
            raise ValueError('Runner exited, but cleanup is not confirmed. Run the OLD figure-tools cleanup '
                             f'with --output {output} before replacing tools.')
        print(f'STOPPED: {key} remains complete; runner exited; recorded instance cleanup is complete.', flush=True)
        print('You can now verify cleanup with the old tools, install v3, and resume with '
              '--upgrade-pool-policy --retry-failed using the same output directory.', flush=True)
    finally:
        os.close(fd)


if __name__ == '__main__':
    try:
        main()
    except KeyboardInterrupt:
        print('\nWatcher stopped. This does not undo a stop signal already sent.', file=sys.stderr)
        raise SystemExit(130)
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f'ERROR: {error}', file=sys.stderr)
        raise SystemExit(1)
