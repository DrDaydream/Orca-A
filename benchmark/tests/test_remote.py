"""Exercise the Ubuntu remote runner without AWS credentials or network calls."""
from collections import OrderedDict
from base64 import b64encode
from datetime import datetime, timezone
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import MagicMock, patch

from benchmark.config import Committee
from benchmark.remote import Bench, ExecutionError
from benchmark.utils import BenchError


class RemoteBenchTests(unittest.TestCase):
    def setUp(self):
        # Do not construct InstanceManager: its constructor creates AWS clients.
        self.bench = Bench.__new__(Bench)
        self.bench.connect = {}
        self.bench.settings = SimpleNamespace(
            repo_name='Orca-A', repo_url='https://example.invalid/Orca-A.git',
            branch='orca-a-grbc-echo-ready', base_port=3000,
        )
        self.bench.manager = MagicMock()
        self.hosts = ['192.0.2.1', '192.0.2.2', '192.0.2.3', '192.0.2.4']
        self.params = dict(faults=0, nodes=[4], workers=1, collocate=True,
                           rate=[50000], tx_size=512, duration=20, runs=1)
        self.node_params = dict(header_size=1000, max_header_delay=200,
                                gc_depth=50, sync_retry_delay=10000,
                                sync_retry_nodes=3, batch_size=500000,
                                max_batch_delay=200)

    @patch('benchmark.remote.subprocess.run')
    @patch('benchmark.remote.Group')
    def test_install_prepares_controller_and_selected_ubuntu_hosts(self, group, run):
        with patch.dict('benchmark.remote.environ', {'TOKIO_WORKER_THREADS': '1'}), \
             patch.object(self.bench, '_controller_revision', return_value='a' * 40), \
             patch.object(self.bench, '_update'), patch.object(self.bench, '_build_local'):
            self.bench.install(hosts=self.hosts)
            from benchmark.remote import environ
            self.assertNotIn('TOKIO_WORKER_THREADS', environ)
        setup = run.call_args.args[0]
        self.assertEqual(setup[:2], ['bash', '-c'])
        self.assertIn('clang-14 libclang-14-dev', setup[2])
        self.assertIn('cloud-init status --wait', setup[2])
        self.assertIn('chronyc waitsync', setup[2])
        self.assertIn('1.85.0', setup[2])
        group.assert_called_once_with(*self.hosts, user='ubuntu', connect_kwargs={})
        command = group.return_value.run.call_args.args[0]
        self.assertIn('git clone --branch orca-a-grbc-echo-ready', command)
        self.assertNotIn('venv', command)

    @patch('benchmark.remote.Group')
    def test_update_builds_exact_controller_commit_without_forcing_remote_edits(self, group):
        revision = '1234567890abcdef' * 2 + '12345678'
        self.bench.revision = revision
        with patch.object(self.bench, '_controller_revision', return_value=revision):
            self.bench._update(self.hosts, collocate=True)
        command = group.return_value.__enter__.return_value.run.call_args.args[0]
        self.assertIn(f'git checkout --detach {revision}', command)
        self.assertIn(f'test "$(git rev-parse HEAD)" = {revision}', command)
        self.assertIn('git diff --quiet && git diff --cached --quiet', command)
        self.assertIn('cargo build --locked --target-dir ../target', command)
        self.assertIn('TOKIO_WORKER_THREADS', command)
        self.assertNotIn('checkout -f', command)
        self.assertNotIn('git pull', command)

    @patch('benchmark.remote.subprocess.check_output', return_value='a' * 40 + '\n')
    @patch('benchmark.remote.subprocess.run')
    def test_controller_native_code_check_runs_at_repository_root(self, run, output):
        run.return_value.returncode = 0
        with patch('benchmark.remote.PathMaker.node_crate_path', return_value='/repo/node'):
            self.assertEqual(self.bench._controller_revision(), 'a' * 40)
        command = run.call_args.args[0]
        self.assertEqual(Path(run.call_args.kwargs['cwd']).as_posix(), '/repo/node/..')
        self.assertIn('Cargo.lock', command)
        self.assertIn('*.rs', command)
        # Experiment parameters are allowed to differ from the deployed commit.
        self.assertNotIn('benchmark', command)
        self.assertNotIn('fabfile.py', command)

    @patch('benchmark.remote.subprocess.run')
    def test_dirty_native_controller_rejected_before_remote_preparation(self, run):
        run.return_value.returncode = 1
        self.bench._select_hosts = MagicMock(return_value=self.hosts)
        self.bench.install = MagicMock()
        with self.assertRaises(BenchError):
            self.bench.run(self.params, self.node_params)
        self.bench.install.assert_not_called()

    def fixture_run(self):
        committee = Committee(OrderedDict(
            (f'key-{i}', [host, host]) for i, host in enumerate(self.hosts)
        ), 3000)
        self.bench._select_hosts = MagicMock(return_value=self.hosts)
        self.bench._controller_revision = MagicMock(return_value='a' * 40)
        self.bench.install = MagicMock()
        self.bench._update = MagicMock()
        self.bench._check_binaries = MagicMock()
        self.bench._config = MagicMock(return_value=committee)
        self.bench._release_config_storage = MagicMock()
        self.bench._run_single = MagicMock()
        self.bench._logs = MagicMock(return_value=MagicMock())
        self.bench.kill = MagicMock()
        return self.bench._logs.return_value

    def test_remote_fixture_reuses_build_and_writes_fixed_window_comparison(self):
        logger = self.fixture_run()
        self.bench.run(self.params, self.node_params)
        self.bench.install.assert_not_called()
        self.bench._update.assert_not_called()
        self.bench._check_binaries.assert_called_once_with(self.hosts)
        self.bench._run_single.assert_called_once()
        duration, filename = logger.print_comparison.call_args.args
        self.assertEqual(logger.print_comparison.call_args.kwargs, {'verbose': False})
        self.assertEqual(duration, 20)
        self.assertTrue(filename.endswith('-run-1.comparison.json'))
        committee = self.bench._run_single.call_args.args[1]
        primary = committee.json['authorities']['key-0']['primary']
        self.assertEqual(set(primary), {'primary_to_primary', 'worker_to_primary'})

    def test_failed_run_propagates_without_global_tmux_cleanup(self):
        self.fixture_run()
        self.bench._run_single.side_effect = ExecutionError('fixture failure')
        # ExecutionError comes from Fabric checks and must reject the run.
        with self.assertRaises(BenchError):
            self.bench.run(self.params, self.node_params)
        self.bench.kill.assert_not_called()

    @patch('benchmark.remote.Connection')
    def test_remote_window_waits_from_delayed_client_start(self, connection):
        clock = [1000.0]
        stamp = datetime.fromtimestamp(1000.2, timezone.utc).isoformat().replace('+00:00', 'Z')
        connection.return_value.__enter__.return_value.run.side_effect = [
            SimpleNamespace(stdout=''),
            SimpleNamespace(stdout=f'{stamp} INFO client: Start sending transactions\n'),
        ]
        def advance(seconds):
            clock[0] += seconds
        with patch('benchmark.remote.monotonic', side_effect=lambda: clock[0]), \
                patch('benchmark.remote.time', side_effect=lambda: clock[0]), \
                patch('benchmark.remote.sleep', side_effect=advance):
            self.bench._wait_for_clients([('192.0.2.1', 'logs/client-0-0.log')], 0.3)
        self.assertGreaterEqual(clock[0], 1000.5)
        self.assertEqual(connection.return_value.__enter__.return_value.run.call_count, 2)
        connection.return_value.__exit__.assert_called()

    @patch('benchmark.remote.Connection')
    def test_missing_start_rejects_remote_run_at_bounded_timeout(self, connection):
        connection.return_value.__enter__.return_value.run.return_value.stdout = 'INFO waiting for nodes\n'
        with patch('benchmark.remote.monotonic', side_effect=[0, 121]), \
                self.assertRaisesRegex(ExecutionError, '120 seconds'):
            self.bench._wait_for_clients([('192.0.2.1', 'logs/client-0-0.log')], 20)

    @patch('benchmark.remote.Connection')
    def test_remote_startup_skew_cannot_shorten_a_clients_window(self, connection):
        def started(timestamp):
            stamp = datetime.fromtimestamp(timestamp, timezone.utc).isoformat().replace('+00:00', 'Z')
            return SimpleNamespace(stdout=f'{stamp} INFO client: Start sending transactions\n')
        connection.return_value.__enter__.return_value.run.side_effect = [started(1000), started(1020)]
        clients = [('192.0.2.1', 'logs/client-0-0.log'), ('192.0.2.2', 'logs/client-1-0.log')]
        with self.assertRaisesRegex(ExecutionError, 'startup skew'):
            self.bench._wait_for_clients(clients, 20)

    def test_run_single_waits_for_every_launched_client_before_stopping(self):
        names = [b64encode(bytes([i]) * 32).decode() for i in range(4)]
        committee = Committee(OrderedDict(
            (name, [host, host]) for name, host in zip(names, self.hosts)
        ), 3000)
        from benchmark.config import BenchParameters, NodeParameters
        self.bench.kill = MagicMock()
        self.bench._background_run = MagicMock()
        self.bench._wait_for_clients = MagicMock()
        self.bench._run_single(50000, committee, BenchParameters(self.params),
                               NodeParameters(self.node_params))
        clients, duration = self.bench._wait_for_clients.call_args.args
        self.assertEqual(duration, 20)
        self.assertEqual(set(clients), {
            (host, str(Path('logs') / f'client-{i}-0.log'))
            for i, host in enumerate(self.hosts)
        })
        self.assertEqual(self.bench.kill.call_count, 2)


if __name__ == '__main__':
    unittest.main()
