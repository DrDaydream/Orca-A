from contextlib import redirect_stdout
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from benchmark.logs import LogParser, ParseError


def line(second, message):
    return f'[2026-11-16T01:00:{second:06.3f}Z INFO node] {message}\n'


def primary(final):
    configs = ['Header size set to 1000 B', 'Max header delay set to 200 ms',
               'Garbage collection depth set to 50', 'Sync retry delay set to 10000 ms',
               'Sync retry nodes set to 3', 'Batch size set to 500000 B',
               'Max batch delay set to 200 ms', 'Primary booted on 127.0.0.1:3000']
    return (''.join(line(0, value) for value in configs)
            + line(1, 'Created B1(author) -> batch=')
            + line(final, f'Committed B1(author) -> batch= @ 1794790802000 '
                          f'commit {1794790800000 + int(final * 1000)}'))


CLIENT = (line(0, 'Transactions size: 512 B') + line(0, 'Transactions rate: 50000 tx/s')
          + line(0, 'Start sending transactions') + line(0.5, 'Sending sample transaction 0'))
WORKER = (line(0, 'Worker booted on 127.0.0.1:3001')
          + line(0.5, 'Batch batch= contains 5120 B')
          + line(0.5, 'Batch batch= contains sample tx 0'))


class LogIntegrationTests(unittest.TestCase):
    def parser(self):
        # Exercise the real constructor and merge path without spawning a pool
        # per log type in this small fixture.
        with patch('benchmark.logs.Pool') as pool:
            pool.return_value.__enter__.return_value.map.side_effect = (
                lambda function, logs: list(map(function, logs))
            )
            return LogParser([CLIENT, CLIENT], [primary(5), primary(4)], [WORKER, WORKER])

    def test_orca_ready_and_final_timestamps_have_distinct_meanings(self):
        parser = self.parser()
        result = parser.comparison_metrics(20)
        self.assertEqual(result['final_tps'], 0.5)  # One batch despite two replicas.
        self.assertEqual(result['final_consensus_mean_ms'], 3000)
        self.assertEqual(result['final_e2e_mean_ms'], 3500)
        self.assertEqual(parser._consensus_latency(), 1)
        self.assertNotIn('ABA', parser.result())

    def test_comparison_json_uses_null_for_no_final_completions(self):
        parser = self.parser()
        parser.final_commits.clear()
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / 'comparison.json'
            with redirect_stdout(io.StringIO()) as screen:
                parser.print_comparison(20, str(output))
            metrics = json.loads(output.read_text())
            self.assertIsNone(metrics['final_e2e_mean_ms'])
            self.assertEqual(metrics['sample_unfinished'], 2)
            self.assertIn('N/A', screen.getvalue())

    def test_log_identity_mismatch_is_rejected_before_pairing_samples(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'client-0-0.log').write_text(CLIENT)
            (root / 'worker-1-0.log').write_text(WORKER)
            (root / 'primary-0.log').write_text(primary(4))
            with self.assertRaisesRegex(ParseError, 'identities'):
                LogParser.process(directory)


if __name__ == '__main__':
    unittest.main()
