import io
import subprocess
import sys
from base64 import b64encode
from contextlib import redirect_stdout
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import MagicMock, patch

import fabfile
from benchmark.local import LocalBench
from benchmark.utils import BenchError


class FabricLocalTests(unittest.TestCase):
    @patch('fabfile.LocalBench')
    def test_no_argument_task_preserves_existing_defaults_and_reports_window(self, bench):
        with redirect_stdout(io.StringIO()):
            fabfile.local.body(None)
        params, nodes = bench.call_args.args
        self.assertEqual(params, dict(faults=0, nodes=4, workers=1, rate=50000,
                                      tx_size=512, duration=20))
        self.assertEqual(nodes['header_size'], 1000)
        self.assertEqual(nodes['max_header_delay'], 200)
        bench.return_value.run.assert_called_once_with(False)
        bench.return_value.run.return_value.print_comparison.assert_called_once_with(
            20, 'logs/comparison.json')

    def test_actual_fabric_task_failure_exits_nonzero(self):
        # Run the real Fabric dispatcher with only the workload mocked out.
        # This catches errors swallowed by a task body despite cleanup succeeding.
        for task, target in [('local', 'benchmark.local.LocalBench'),
                             ('remote', 'benchmark.remote.Bench')]:
            script = f'''
from unittest.mock import patch
from benchmark.utils import BenchError
from fabric.main import program
with patch('{target}.__init__', return_value=None), patch('{target}.run') as run:
    run.side_effect = BenchError('fixture process failure', RuntimeError('exit 3'))
    program.run(argv=['fab', '{task}'])
'''
            with self.subTest(task=task):
                result = subprocess.run(
                    [sys.executable, '-c', script], text=True, capture_output=True,
                    cwd=Path(__file__).resolve().parents[1],
                )
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn('fixture process failure', result.stdout + result.stderr)

    @patch('benchmark.local.sleep')
    @patch('benchmark.local.subprocess.run')
    @patch('benchmark.local.Key.from_file')
    def test_runtime_failure_reaps_local_processes(self, key, run, sleep):
        params = dict(faults=0, nodes=4, workers=1, rate=50000, tx_size=512, duration=20)
        nodes = dict(header_size=1000, max_header_delay=200, gc_depth=50,
                     sync_retry_delay=10000, sync_retry_nodes=3, batch_size=500000,
                     max_batch_delay=200)
        key.side_effect = [SimpleNamespace(name=b64encode(bytes([i]) * 32).decode())
                           for i in range(4)]
        bench = LocalBench(params, nodes)
        bench.processes = MagicMock()
        bench.processes.wait.side_effect = RuntimeError('child exited')
        # Configuration writes are a fixture too; preserve the filesystem.
        with patch('benchmark.local.LocalCommittee.print'), patch.object(
                bench.node_parameters, 'print'), redirect_stdout(io.StringIO()):
            with self.assertRaises(BenchError):
                bench.run()
        bench.processes.wait.assert_called_once_with(20)
        self.assertEqual(bench.processes.stop.call_count, 2)


if __name__ == '__main__':
    unittest.main()
