#!/usr/bin/env python3
"""Reuse verified Orca-A A0/A3/A5 v3.4 archives in a v3.5 campaign.

Copies evidence, checks parameters and recomputes the existing latency metric.
The source directory, old A1, campaign identity and lifecycle state stay separate.
Requires all 72 compatible cases, so the following all-protocol matrix runs only
Orca-A A1 before moving on to the other protocols. No AWS calls are made here.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import math
from pathlib import Path, PurePosixPath
import shutil
import sys
import tempfile
import types

MODES = ('A0', 'A3', 'A5')
ADDED_FIELDS = {'offered_rate_tps', 'byzantine_ids', 'fault_model', 'aba_scheduling'}
SOURCE_COMMITS = {
    '57a6ccc0b4a0ce21baa59081192bc8bb5f0208ba',
    'c617ac3cfeb69a0dd32ca096ca53c8d62cbb6a83',
}
PARSER_SHA256 = 'b99a6c24b92400aae07f4a2602953a8f339c8d0419cf993eef3c33eb96d58cbb'
NODE_PARAMETERS = dict(header_size=20, max_header_delay=200, gc_depth=50,
                       sync_retry_delay=10000, sync_retry_nodes=3,
                       batch_size=500000, max_batch_delay=200)
DEFAULT_SOURCE = '/home/ubuntu/experiments/paper-results-v3.4-60s-h20-a20-50'
DEFAULT_OUTPUT = '/home/ubuntu/experiments/paper-results-v3.5-60s-h20-crash-a1'


def read(path):
    return json.loads(path.read_text(encoding='utf-8-sig'))


def save(path, data):
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(json.dumps(data, ensure_ascii=False, indent=2, allow_nan=False) + '\n',
                         encoding='utf-8', newline='\n')
    temporary.replace(path)


def sha(path):
    with path.open('rb') as handle:
        return hashlib.file_digest(handle, 'sha256').hexdigest()


def regular_files(directory):
    if directory.is_symlink() or not directory.is_dir():
        raise ValueError(f'Expected an ordinary archive directory: {directory}')
    result = {}
    for path in sorted(directory.rglob('*')):
        if path.is_symlink():
            raise ValueError(f'Archive contains a symbolic link: {path}')
        if path.is_file():
            result[path.relative_to(directory).as_posix()] = path
    return result


def verify_archive(directory):
    files = regular_files(directory)
    if 'SHA256.json' not in files:
        raise ValueError(f'Missing SHA256.json: {directory}')
    manifest = read(files.pop('SHA256.json'))
    if not isinstance(manifest, dict):
        raise ValueError(f'Invalid SHA256 manifest: {directory}')
    for name in manifest:
        path = PurePosixPath(name)
        if path.is_absolute() or '..' in path.parts or '\\' in name or ':' in name:
            raise ValueError(f'Invalid SHA256 path: {name}')
    if set(manifest) != set(files):
        raise ValueError(f'SHA256 file inventory differs: {directory}')
    for name, path in files.items():
        if sha(path) != manifest[name]:
            raise ValueError(f'SHA256 mismatch: {path}')
    return manifest


def selected_archive(root, case, matrix):
    directory = root / matrix.case_key(case)
    selected = directory / 'selected.json'
    if not selected.is_file():
        raise ValueError(f'Missing completed case: {selected}')
    for path in [selected, *selected.parents]:
        if path == root.parent:
            break
        if path.is_symlink():
            raise ValueError(f'Symbolic link in selected path: {path}')
    meta = read(selected)
    name = meta.get('attempt', '')
    if not name.startswith('attempt-') or '/' in name or '\\' in name or ':' in name:
        raise ValueError(f'Invalid selected attempt: {selected}')
    archive = directory / name
    verify_archive(archive)
    if meta.get('status') != 'complete' or meta != read(archive / 'metadata.json'):
        raise ValueError(f'Incomplete or inconsistent selected metadata: {selected}')
    measurement = read(archive / 'measurement.json')
    if measurement.get('status') != 'complete' or measurement.get('case') != meta.get('case'):
        raise ValueError(f'Inconsistent measurement metadata: {archive}')
    return archive, meta, measurement


def verified_parser(path):
    source = path.read_text(encoding='utf-8')
    if hashlib.sha256(source.encode('utf-8')).hexdigest() != PARSER_SHA256:
        raise ValueError(f'Unrecognized v3.4 measurement parser: {path}')
    # Compile the reviewed source directly; never load an archived bytecode cache.
    module = types.ModuleType('verified_v34_measurements')
    exec(compile(source, str(path), 'exec'), module.__dict__)
    return module


def validate_source(archive, meta, measurement, case, matrix):
    previous = {key: value for key, value in case.items() if key not in ADDED_FIELDS}
    if meta['case'] != previous:
        raise ValueError(f'v3.4 experiment parameters differ: {archive}')
    if (meta.get('controller_commit') not in SOURCE_COMMITS
            or measurement.get('controller_commit') != meta['controller_commit']):
        raise ValueError(f'Unrecognized Orca-A source revision: {archive}')
    if meta.get('returncode') != 0:
        raise ValueError(f'Original remote command did not succeed: {archive}')
    for key in ('started_utc', 'finished_utc'):
        if datetime.fromisoformat(meta[key]).tzinfo is None:
            raise ValueError(f'Missing timezone for {key}: {archive}')
    snapshots = list((archive / 'native-results').glob('*.parameters.json'))
    if len(snapshots) != 1:
        raise ValueError(f'Missing or ambiguous parameter snapshot: {archive}')
    snapshot = read(snapshots[0])
    if (snapshot.get('protocol') != 'orca-a' or snapshot.get('nodes') != case['nodes']
            or snapshot.get('rate') != case['input_rate_tps'] or snapshot.get('run') != 1
            or snapshot.get('node_parameters') != NODE_PARAMETERS):
        raise ValueError(f'Original node parameters differ: {snapshots[0]}')
    bench = snapshot['benchmark_parameters']
    expected = dict(nodes=[case['nodes']], rate=[case['input_rate_tps']], faults=0,
                    workers=1, tx_size=512, duration=60, runs=1, collocate=True)
    if any(bench.get(key) != value for key, value in expected.items()):
        raise ValueError(f'Original workload parameters differ: {snapshots[0]}')
    authorities = snapshot.get('launch_authorities', [])
    if len(authorities) != case['nodes'] or len(set(authorities)) != case['nodes']:
        raise ValueError(f'Incomplete launch authority mapping: {snapshots[0]}')
    adversary = snapshot.get('adversary', {})
    expected = dict(mode=matrix.NATIVE_MODES[case['mode']],
                    byzantine_authorities=authorities[:case['byzantine']], seed=7,
                    start_round=1, rounds=0, delay_ms=200,
                    max_pending_messages=4096, max_pending_bytes=67108864)
    if any(adversary.get(key) != value for key, value in expected.items()):
        raise ValueError(f'Original adversary parameters differ: {snapshots[0]}')
    verified_parser(archive / 'measurement_parser.py')


def equal_metric(left, right):
    if isinstance(left, float) and isinstance(right, (float, int)):
        return math.isfinite(left) and math.isclose(left, right, rel_tol=1e-12, abs_tol=1e-9)
    if isinstance(left, dict) and isinstance(right, dict):
        return set(left) == set(right) and all(equal_metric(left[k], right[k]) for k in left)
    if isinstance(left, list) and isinstance(right, list):
        return len(left) == len(right) and all(equal_metric(a, b) for a, b in zip(left, right))
    return left == right


def import_results(source, output, matrix, *, plan=None, dry_run=False):
    source, output = Path(source).resolve(), Path(output).resolve()
    if source == output or source in output.parents or output in source.parents:
        raise ValueError('Source and destination must be separate, non-nested directories')
    permitted = {matrix.case_key(c): c for c in matrix.cases('Orca-A') if c['mode'] in MODES}
    if len(permitted) != 72 or any(c['duration_s'] != 60 or c['header_size_bytes'] != 20
                                  or c['fault_model'] == 'crash-stop' for c in permitted.values()):
        raise ValueError('Expected the released v3.5 matrix')
    plan = list(permitted.values()) if plan is None else list(plan)
    if not plan or any(permitted.get(matrix.case_key(c)) != c for c in plan):
        raise ValueError('Reuse is restricted to Orca-A A0/A3/A5; changed A1 must be rerun')
    pending, kept = [], []
    for case in plan:
        key = matrix.case_key(case)
        previous_selection = None
        selected = output / key / 'selected.json'
        if selected.exists():
            saved = read(selected)
            if saved.get('case') != case:
                raise ValueError(f'Destination configuration differs: {selected}')
            if saved.get('status') == 'complete':
                archive, meta, result = selected_archive(output, case, matrix)
                kept.append(dict(case=key, archive=str(archive), reused_from=result.get('reused_from')))
                continue
            name = saved.get('attempt', '')
            if (saved.get('status') not in ('failed', 'interrupted')
                    or not name.startswith('attempt-') or '/' in name or '\\' in name or ':' in name):
                raise ValueError(f'Destination attempt is still running or incomplete: {selected}')
            failed_archive = selected.parent / name
            verify_archive(failed_archive)
            if read(failed_archive / 'metadata.json') != saved:
                raise ValueError(f'Destination failure metadata differs: {selected}')
            previous_selection = selected.read_bytes()
        archive, meta, result = selected_archive(source, case, matrix)
        validate_source(archive, meta, result, case, matrix)
        pending.append((case, archive, meta, result, previous_selection))
        print(f'VERIFIED {key}', flush=True)
    report = dict(source=str(source), output=str(output), protocol='Orca-A', modes=list(MODES),
                  eligible=len(plan), imported=0, kept=len(kept), records=kept, dry_run=dry_run)
    if dry_run:
        report['ready_to_import'] = len(pending)
        print(f'CHECK ONLY: {len(pending)} ready, {len(kept)} already complete; no files written.')
        return report
    output.mkdir(parents=True, exist_ok=True)
    # Validate and recalculate every copied case before publishing any selections.
    # TemporaryDirectory confines cleanup to this newly created staging folder.
    with tempfile.TemporaryDirectory(prefix='.reuse-v34-', dir=output) as temporary:
        staged = []
        for index, (case, archive, meta, result, previous_selection) in enumerate(pending):
            target = Path(temporary) / str(index)
            shutil.copytree(archive, target)
            verify_archive(target)
            original = target / 'original-v3.4'
            original.mkdir()
            for name in ('metadata.json', 'measurement.json', 'measurement_stats.json', 'summary.txt', 'SHA256.json'):
                shutil.copy2(target / name, original / name)
            shutil.copy2(archive.parent / 'selected.json', original / 'selected.json')
            if previous_selection is not None:
                (target / 'previous-destination-selected.json').write_bytes(previous_selection)
            loader = matrix.load_module
            try:
                matrix.load_module = verified_parser
                recalculated = matrix.legacy_measurement(target, case)
            finally:
                matrix.load_module = loader
            for key, value in recalculated.items():
                if key == 'crashed_primary_ids' and key not in result:
                    continue
                if key not in result or not equal_metric(value, result[key]):
                    raise ValueError(f'Original measurement differs from raw logs ({key}): {archive}')
            digest = sha(original / 'SHA256.json')
            attempt = 'attempt-reuse-v34-' + digest[:20]
            destination = output / matrix.case_key(case) / attempt
            if destination.exists():
                raise ValueError(f'Unselected destination already exists; inspect before retry: {destination}')
            provenance = dict(release='v3.4', archive=str(archive), sha256_manifest=digest,
                              controller_commit=meta['controller_commit'], finished_utc=meta['finished_utc'],
                              imported_utc=datetime.now(timezone.utc).isoformat(),
                              importer_sha256=sha(Path(__file__)),
                              validation='Full archive hashes, workload/adversary snapshot and raw-log latency recalculation')
            new_meta = dict(meta, case=case, attempt=attempt, reused_from=provenance)
            new_result = dict(result, case=case, archive=str(destination), reused_from=provenance,
                              crashed_primary_ids=[])
            save(target / 'metadata.json', new_meta)
            save(target / 'measurement.json', new_result)
            save(target / 'provenance.json', provenance)
            original_summary = (original / 'summary.txt').read_text(encoding='utf-8-sig')
            (target / 'summary.txt').write_text(
                f'Reused v3.4 evidence; no new benchmark run\nSource archive: {archive}\n'
                f'Original finish: {meta["finished_utc"]}\nController commit: {meta["controller_commit"]}\n'
                f'Current archive: {destination}\nLatency revalidated from original logs.\n\n'
                f'Original v3.4 summary follows:\n{original_summary}', encoding='utf-8', newline='\n')
            manifest = {name: sha(path) for name, path in regular_files(target).items() if name != 'SHA256.json'}
            save(target / 'SHA256.json', manifest)
            staged.append((target, destination, new_meta, provenance))
        for target, destination, meta, provenance in staged:
            destination.parent.mkdir(parents=True, exist_ok=True)
            target.rename(destination)
            save(destination.parent / 'selected.json', meta)
            report['records'].append(dict(case=matrix.case_key(meta['case']), archive=str(destination),
                                          reused_from=provenance))
            report['imported'] += 1
    save(output / 'reuse-v3.4-orca-a.json', report)
    print(f'REUSE COMPLETE: {report["imported"]} imported; {report["kept"]} already complete. '
          'Orca-A A1 remains a separate v3.5 run.', flush=True)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, default=Path(DEFAULT_SOURCE))
    parser.add_argument('--output', type=Path, default=Path(DEFAULT_OUTPUT))
    parser.add_argument('--tools', type=Path, default=Path('/home/ubuntu/experiments/figure-tools'))
    parser.add_argument('--dry-run', action='store_true')
    args = parser.parse_args()
    if sys.platform != 'linux':
        parser.error('Run on the Ubuntu EC2 controller')
    import fcntl
    try:
        with Path('/tmp/five-protocol-figure-matrix.lock').open('a') as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                parser.error('A matrix/update/import is running; finish its cleanup before importing')
            spec = importlib.util.spec_from_file_location('reuse_matrix', args.tools / 'run_matrix.py')
            matrix = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(matrix)
            import_results(args.source, args.output, matrix, dry_run=args.dry_run)
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.exit(1, f'Reuse stopped: {error}\nSource archives were not changed.\n')


if __name__ == '__main__':
    main()
