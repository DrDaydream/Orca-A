# Copyright(C) Facebook, Inc. and its affiliates.
from collections import OrderedDict, deque
from concurrent.futures import ThreadPoolExecutor, as_completed
from fabric import Connection, ThreadingGroup as Group
from fabric.exceptions import GroupException
from paramiko import RSAKey
from paramiko.ssh_exception import PasswordRequiredException, SSHException
from os.path import basename, splitext
from time import sleep
from math import ceil
from copy import deepcopy
from threading import Event, Lock, Thread
import shlex
import subprocess
import sys

from benchmark.config import Committee, Key, NodeParameters, BenchParameters, ConfigError
from benchmark.utils import BenchError, Print, PathMaker, progress_bar
from benchmark.commands import CommandMaker
from benchmark.adversary_schedule import build_client_schedules, client_silence_slot_ms
from benchmark.logs import LogParser, ParseError
from benchmark.instance import InstanceManager


class FabricError(Exception):
    ''' Wrapper for Fabric exception with a meaningfull error message. '''

    def __init__(self, error):
        assert isinstance(error, GroupException)
        message = list(error.result.values())[-1]
        super().__init__(message)


class ExecutionError(Exception):
    pass


_INSTALL_OUTPUT_LOCK = Lock()


class _InstallOutput:
    ''' Prefix streamed remote output so concurrent hosts remain identifiable. '''

    def __init__(self, host, step, stream, output_tail, stream_name):
        self.host = host
        self.step = step
        self.stream = stream
        self.output_tail = output_tail
        self.stream_name = stream_name
        self.buffer = ''

    def _write_line(self, line):
        with _INSTALL_OUTPUT_LOCK:
            if line:
                self.output_tail.append(f'{self.stream_name}: {line}')
            self.stream.write(
                f'[INSTALL][{self.host}][{self.step}][output] {line}\n'
            )
            self.stream.flush()

    def write(self, data):
        if not data:
            return 0
        self.buffer += data
        while '\n' in self.buffer:
            line, self.buffer = self.buffer.split('\n', 1)
            self._write_line(line.rstrip('\r'))
        return len(data)

    def flush(self):
        with _INSTALL_OUTPUT_LOCK:
            self.stream.flush()

    def finish(self):
        if self.buffer:
            self._write_line(self.buffer.rstrip('\r'))
            self.buffer = ''
        self.flush()


class Bench:
    def __init__(self, ctx):
        self.manager = InstanceManager.make()
        self.settings = self.manager.settings
        try:
            ctx.connect_kwargs.pkey = RSAKey.from_private_key_file(
                self.manager.settings.key_path
            )
            self.connect = ctx.connect_kwargs
        except (IOError, PasswordRequiredException, SSHException) as e:
            raise BenchError('Failed to load SSH key', e)

    def _check_stderr(self, output):
        if isinstance(output, dict):
            for x in output.values():
                if x.stderr:
                    raise ExecutionError(x.stderr)
        else:
            if output.stderr:
                raise ExecutionError(output.stderr)

    @staticmethod
    def _install_status(host, step, status, detail=''):
        detail = f' {detail}' if detail else ''
        with _INSTALL_OUTPUT_LOCK:
            print(
                f'[INSTALL][{host}][{step}] {status}{detail}',
                flush=True
            )

    def _install_steps(self):
        apt = (
            'sudo -n timeout --signal=TERM --kill-after=30s 30m '
            'env DEBIAN_FRONTEND=noninteractive '
            'NEEDRESTART_MODE=a APT_LISTCHANGES_FRONTEND=none '
            'apt-get -o DPkg::Lock::Timeout=900 '
            '-o Acquire::Retries=5 '
            '-o Acquire::ForceIPv4=true '
            '-o Acquire::http::Timeout=30 '
            '-o Acquire::https::Timeout=30 '
            '-o Dpkg::Use-Pty=0 '
        )
        repo_url = shlex.quote(self.settings.repo_url)
        repo_name = shlex.quote(self.settings.repo_name)
        repo_branch = shlex.quote(self.settings.branch)

        return [
            (
                'cloud-init',
                'if command -v cloud-init >/dev/null 2>&1; then '
                'sudo -n timeout --signal=TERM --kill-after=30s 15m '
                'cloud-init status --wait & cloud_pid=$!; elapsed=0; '
                'while kill -0 "$cloud_pid" 2>/dev/null; do '
                'echo "cloud-init still running; waited ${elapsed}s"; '
                'sleep 10; elapsed=$((elapsed + 10)); done; '
                'wait "$cloud_pid"; '
                'else echo "cloud-init is not installed; skipping"; fi'
            ),
            (
                'apt-lock',
                'locks="/var/lib/dpkg/lock-frontend /var/lib/dpkg/lock '
                '/var/cache/apt/archives/lock /var/lib/apt/lists/lock"; '
                'elapsed=0; '
                'while sudo -n fuser $locks >/dev/null 2>&1; do '
                'if [ "$elapsed" -ge 900 ]; then '
                'echo "timed out waiting for apt/dpkg locks" >&2; '
                'sudo -n fuser -v $locks >&2; exit 1; fi; '
                'echo "apt/dpkg lock busy; waited ${elapsed}s"; '
                'sudo -n fuser -v $locks 2>&1; '
                'sleep 10; elapsed=$((elapsed + 10)); '
                'done'
            ),
            (
                'dpkg-configure',
                'sudo -n env DEBIAN_FRONTEND=noninteractive '
                'NEEDRESTART_MODE=a APT_LISTCHANGES_FRONTEND=none '
                'timeout --signal=TERM --kill-after=30s 15m '
                'dpkg --configure -a'
            ),
            ('apt-update', f'{apt}update'),
            (
                'base-packages',
                f'{apt}-y install build-essential cmake curl git '
                'software-properties-common'
            ),
            (
                'universe-repository',
                'sudo -n env DEBIAN_FRONTEND=noninteractive '
                'timeout --signal=TERM --kill-after=30s 5m '
                'add-apt-repository -y -n universe'
            ),
            ('apt-update-universe', f'{apt}update'),
            (
                'clang-packages',
                f'{apt}-y install clang-14 llvm-14 llvm-14-dev '
                'libclang-14-dev'
            ),
            (
                'rustup-install',
                'if [ -x "$HOME/.cargo/bin/rustup" ]; then '
                'echo "rustup is already installed"; '
                'else installer=$(mktemp) && '
                'trap \'rm -f "$installer"\' EXIT && '
                'curl --proto "=https" --tlsv1.2 -fsS '
                '--retry 5 --retry-delay 2 --retry-all-errors '
                '--connect-timeout 30 --max-time 300 '
                '-o "$installer" https://sh.rustup.rs && '
                'timeout --signal=TERM --kill-after=30s 15m '
                'sh "$installer" -y; fi'
            ),
            (
                'rust-stable',
                'for attempt in 1 2 3; do '
                'timeout --signal=TERM --kill-after=30s 15m '
                '"$HOME/.cargo/bin/rustup" default stable && exit 0; '
                'rc=$?; echo "rustup attempt ${attempt}/3 failed '
                '(exit=$rc)" >&2; sleep $((attempt * 5)); '
                'done; exit "$rc"'
            ),
            (
                'clang-alternatives',
                'sudo -n update-alternatives --install /usr/bin/clang clang '
                '/usr/bin/clang-14 140 && '
                'sudo -n update-alternatives --install /usr/bin/clang++ clang++ '
                '/usr/bin/clang++-14 140 && '
                'sudo -n update-alternatives --set clang /usr/bin/clang-14 && '
                'sudo -n update-alternatives --set clang++ /usr/bin/clang++-14'
            ),
            (
                'compiler-environment',
                'if ! grep -q "LIBCLANG_PATH=/usr/lib/llvm-14/lib" '
                '"$HOME/.cargo/env"; then '
                "printf '\\nexport PATH=/usr/lib/llvm-14/bin:$PATH\\n"
                'export CC=/usr/bin/clang-14\\n'
                'export CXX=/usr/bin/clang++-14\\n'
                'export CLANG_PATH=/usr/bin/clang-14\\n'
                'export LIBCLANG_PATH=/usr/lib/llvm-14/lib\\n'
                "export CXXFLAGS=\"-include cstdint\"\\n' "
                '>> "$HOME/.cargo/env"; fi'
            ),
            (
                'repository',
                f'if [ -d {repo_name}/.git ]; then '
                'for attempt in 1 2 3; do '
                f'timeout 10m git -C {repo_name} '
                '-c http.lowSpeedLimit=1024 -c http.lowSpeedTime=60 '
                f'fetch origin {repo_branch} && '
                f'git -C {repo_name} checkout {repo_branch} && '
                f'timeout 10m git -C {repo_name} '
                '-c http.lowSpeedLimit=1024 -c http.lowSpeedTime=60 '
                f'pull --ff-only origin {repo_branch} && exit 0; '
                'rc=$?; echo "git pull attempt ${attempt}/3 failed '
                '(exit=$rc)" >&2; sleep $((attempt * 5)); done; '
                'exit "$rc"; '
                f'elif [ -e {repo_name} ]; then '
                f'echo "repository path already exists but is not a git repo: '
                f'{repo_name}" >&2; exit 1; '
                'else for attempt in 1 2 3; do '
                'timeout 10m git -c http.lowSpeedLimit=1024 '
                '-c http.lowSpeedTime=60 '
                f'clone --branch {repo_branch} {repo_url} {repo_name} '
                '&& exit 0; '
                'rc=$?; echo "git clone attempt ${attempt}/3 failed '
                '(exit=$rc)" >&2; sleep $((attempt * 5)); done; '
                'exit "$rc"; fi'
            )
        ]

    def _install_host(self, host, steps):
        connection = None
        step = 'connect'
        output_tail = deque(maxlen=12)
        try:
            self._install_status(host, step, 'START', 'attempts=5')
            for attempt in range(1, 6):
                connection = Connection(
                    host,
                    user='ubuntu',
                    connect_kwargs=self.connect,
                    connect_timeout=30
                )
                try:
                    connection.open()
                    self._install_status(
                        host, step, 'OK', f'attempt={attempt}/5'
                    )
                    break
                except Exception as e:
                    try:
                        connection.close()
                    except Exception:
                        pass
                    connection = None
                    if attempt == 5:
                        raise
                    delay = attempt * 5
                    detail = str(e).strip().replace('\n', ' | ')
                    self._install_status(
                        host, step, 'RETRY',
                        f'attempt={attempt}/5 wait={delay}s: {detail}'
                    )
                    sleep(delay)

            for step, command in steps:
                self._install_status(host, step, 'START')
                output_tail = deque(maxlen=12)
                stdout = _InstallOutput(
                    host, step, sys.stdout, output_tail, 'stdout'
                )
                stderr = _InstallOutput(
                    host, step, sys.stderr, output_tail, 'stderr'
                )
                heartbeat_stop = Event()

                def heartbeat():
                    elapsed = 0
                    while not heartbeat_stop.wait(30):
                        elapsed += 30
                        self._install_status(
                            host, step, 'RUNNING', f'elapsed={elapsed}s'
                        )

                heartbeat_thread = Thread(target=heartbeat, daemon=True)
                heartbeat_thread.start()
                try:
                    result = connection.run(
                        command,
                        warn=True,
                        hide=False,
                        pty=False,
                        in_stream=False,
                        out_stream=stdout,
                        err_stream=stderr
                    )
                finally:
                    heartbeat_stop.set()
                    heartbeat_thread.join()
                    stdout.finish()
                    stderr.finish()

                if result.failed:
                    detail = ' | '.join(output_tail) or 'no output'
                    self._install_status(
                        host, step, 'ERROR',
                        f'exit={result.exited}: {detail}'
                    )
                    return step, f'exit={result.exited}: {detail}'
                self._install_status(host, step, 'OK')

            self._install_status(host, 'complete', 'OK')
            return None
        except Exception as e:
            detail = str(e).strip().replace('\n', ' | ') or type(e).__name__
            if output_tail:
                detail += f'; last output: {" | ".join(output_tail)}'
            self._install_status(
                host, step, 'ERROR', f'{type(e).__name__}: {detail}'
            )
            return step, f'{type(e).__name__}: {detail}'
        finally:
            if connection is not None:
                try:
                    connection.close()
                except Exception:
                    pass

    def install(self):
        hosts = self.manager.hosts(flat=True)
        if not hosts:
            raise BenchError(
                'Failed to install repo on testbed',
                ExecutionError('No available hosts')
            )

        Print.info(
            f'[INSTALL] Installing dependencies and cloning the repo on '
            f'{len(hosts)} nodes...'
        )
        steps = self._install_steps()
        failures = {}
        completed = 0
        with ThreadPoolExecutor(max_workers=len(hosts)) as executor:
            futures = {
                executor.submit(self._install_host, host, steps): host
                for host in hosts
            }
            for future in as_completed(futures):
                host = futures[future]
                try:
                    failure = future.result()
                except Exception as e:
                    failure = ('internal', f'{type(e).__name__}: {e}')
                    self._install_status(
                        host, failure[0], 'ERROR', failure[1]
                    )
                if failure:
                    failures[host] = failure
                completed += 1
                with _INSTALL_OUTPUT_LOCK:
                    print(
                        f'[INSTALL] PROGRESS completed={completed}/{len(hosts)} '
                        f'ok={completed - len(failures)} '
                        f'failed={len(failures)}',
                        flush=True
                    )

        succeeded = len(hosts) - len(failures)
        Print.info(
            f'[INSTALL] SUMMARY total={len(hosts)} '
            f'ok={succeeded} failed={len(failures)}'
        )
        if failures:
            details = []
            for host in hosts:
                if host in failures:
                    step, reason = failures[host]
                    line = f'{host}: step={step}; {reason}'
                    details.append(line)
                    Print.info(f'[INSTALL][{host}][summary] FAILED {line}')
            raise BenchError(
                'Failed to install repo on testbed',
                ExecutionError('\n'.join(details))
            )

        Print.heading(f'Initialized testbed of {len(hosts)} nodes')

    def kill(self, hosts=[], delete_logs=False):
        assert isinstance(hosts, list)
        assert isinstance(delete_logs, bool)
        hosts = hosts if hosts else self.manager.hosts(flat=True)
        delete_logs = CommandMaker.clean_logs() if delete_logs else 'true'
        cmd = [delete_logs, f'({CommandMaker.kill()} || true)']
        try:
            g = Group(*hosts, user='ubuntu', connect_kwargs=self.connect)
            g.run(' && '.join(cmd), hide=True)
        except GroupException as e:
            raise BenchError('Failed to kill nodes', FabricError(e))

    def _select_hosts(self, bench_parameters):
        # Collocate the primary and its workers on the same machine.
        if bench_parameters.collocate:
            nodes = max(bench_parameters.nodes)

            # Ensure there are enough hosts.
            hosts = self.manager.hosts()
            if sum(len(x) for x in hosts.values()) < nodes:
                return []

            # Select the hosts in different data centers.
            ordered = zip(*hosts.values())
            ordered = [x for y in ordered for x in y]
            return ordered[:nodes]

        # Spawn the primary and each worker on a different machine. Each
        # authority runs in a single data center.
        else:
            primaries = max(bench_parameters.nodes)

            # Ensure there are enough hosts.
            hosts = self.manager.hosts()
            if len(hosts.keys()) < primaries:
                return []
            for ips in hosts.values():
                if len(ips) < bench_parameters.workers + 1:
                    return []

            # Ensure the primary and its workers are in the same region.
            selected = []
            for region in list(hosts.keys())[:primaries]:
                ips = list(hosts[region])[:bench_parameters.workers + 1]
                selected.append(ips)
            return selected

    def _background_run(self, host, command, log_file):
        name = splitext(basename(log_file))[0]
        cmd = f'tmux new -d -s "{name}" "{command} |& tee {log_file}"'
        c = Connection(host, user='ubuntu', connect_kwargs=self.connect)
        output = c.run(cmd, hide=True)
        self._check_stderr(output)

    def _update(self, hosts, collocate):
        if collocate:
            ips = list(set(hosts))
        else:
            ips = list(set([x for y in hosts for x in y]))

        Print.info(
            f'Updating {len(ips)} machines (branch "{self.settings.branch}")...'
        )
        cmd = [
            f'(cd {self.settings.repo_name} && git fetch -f)',
            f'(cd {self.settings.repo_name} && git checkout -f {self.settings.branch})',
            f'(cd {self.settings.repo_name} && git pull -f)',
            'source $HOME/.cargo/env',
            f'(cd {self.settings.repo_name}/node && {CommandMaker.compile()})',
            CommandMaker.alias_binaries(
                f'./{self.settings.repo_name}/target/release/'
            )
        ]
        g = Group(*ips, user='ubuntu', connect_kwargs=self.connect)
        g.run(' && '.join(cmd), hide=True)

    def _config(self, hosts, node_parameters, bench_parameters):
        Print.info('Generating configuration files...')

        # Cleanup all local configuration files.
        cmd = CommandMaker.cleanup()
        subprocess.run([cmd], shell=True, stderr=subprocess.DEVNULL)

        # Recompile the latest code.
        cmd = CommandMaker.compile().split()
        subprocess.run(cmd, check=True, cwd=PathMaker.node_crate_path())

        # Create alias for the client and nodes binary.
        cmd = CommandMaker.alias_binaries(PathMaker.binary_path())
        subprocess.run([cmd], shell=True)

        # Generate configuration files.
        keys = []
        key_files = [PathMaker.key_file(i) for i in range(len(hosts))]
        for filename in key_files:
            cmd = CommandMaker.generate_key(filename).split()
            subprocess.run(cmd, check=True)
            keys += [Key.from_file(filename)]

        names = [x.name for x in keys]

        if bench_parameters.collocate:
            workers = bench_parameters.workers
            addresses = OrderedDict(
                (x, [y] * (workers + 1)) for x, y in zip(names, hosts)
            )
        else:
            addresses = OrderedDict(
                (x, y) for x, y in zip(names, hosts)
            )
        committee = Committee(addresses, self.settings.base_port)
        committee.print(PathMaker.committee_file())

        node_parameters.print(PathMaker.parameters_file())

        # Cleanup all nodes and upload configuration files.
        progress = progress_bar(names, prefix='Uploading config files:')
        for i, name in enumerate(progress):
            for ip in committee.ips(name):
                c = Connection(ip, user='ubuntu', connect_kwargs=self.connect)
                c.run(f'{CommandMaker.cleanup()} || true', hide=True)
                c.put(PathMaker.committee_file(), '.')
                c.put(PathMaker.key_file(i), '.')
                c.put(PathMaker.parameters_file(), '.')

        return committee

    def _run_single(self, rate, committee, bench_parameters, node_parameters, debug=False):
        faults = bench_parameters.faults

        # Kill any potentially unfinished run and delete logs.
        hosts = committee.ips()
        self.kill(hosts=hosts, delete_logs=True)

        # Run clients for every worker; dynamically silent workers apply backpressure.
        Print.info('Booting clients...')
        workers_addresses = committee.workers_addresses(0)
        rate_share = ceil(rate / committee.workers())
        names = list(committee.json['authorities'])
        silence_slot_ms = client_silence_slot_ms(
            node_parameters.json['max_header_delay']
        )
        silence_schedules = build_client_schedules(
            names, faults, bench_parameters.duration, silence_slot_ms
        )
        for i, addresses in enumerate(workers_addresses):
            for (id, address) in addresses:
                host = Committee.ip(address)
                cmd = CommandMaker.run_client(
                    address,
                    bench_parameters.tx_size,
                    rate_share,
                    [x for y in workers_addresses for _, x in y],
                    silence_schedules[names[i]],
                    silence_slot_ms,
                )
                log_file = PathMaker.client_log_file(i, id)
                self._background_run(host, cmd, log_file)

        # Run every primary; adversarial authorities are selected per round.
        Print.info('Booting primaries...')
        for i, address in enumerate(committee.primary_addresses(0)):
            host = Committee.ip(address)
            cmd = CommandMaker.run_primary(
                PathMaker.key_file(i),
                PathMaker.committee_file(),
                PathMaker.db_path(i),
                PathMaker.parameters_file(),
                debug=debug,
                faults=faults
            )
            log_file = PathMaker.primary_log_file(i)
            self._background_run(host, cmd, log_file)

        # Run every worker; batch production is paused dynamically.
        Print.info('Booting workers...')
        for i, addresses in enumerate(workers_addresses):
            for (id, address) in addresses:
                host = Committee.ip(address)
                cmd = CommandMaker.run_worker(
                    PathMaker.key_file(i),
                    PathMaker.committee_file(),
                    PathMaker.db_path(i, id),
                    PathMaker.parameters_file(),
                    id,  # The worker's id.
                    debug=debug
                )
                log_file = PathMaker.worker_log_file(i, id)
                self._background_run(host, cmd, log_file)

        # Wait for all transactions to be processed.
        duration = bench_parameters.duration
        for _ in progress_bar(range(20), prefix=f'Running benchmark ({duration} sec):'):
            sleep(ceil(duration / 20))
        self.kill(hosts=hosts, delete_logs=False)

    def _logs(self, committee, faults):
        # Delete local logs (if any).
        cmd = CommandMaker.clean_logs()
        subprocess.run([cmd], shell=True, stderr=subprocess.DEVNULL)

        # Download log files.
        workers_addresses = committee.workers_addresses(0)
        progress = progress_bar(workers_addresses, prefix='Downloading workers logs:')
        for i, addresses in enumerate(progress):
            for id, address in addresses:
                host = Committee.ip(address)
                c = Connection(host, user='ubuntu', connect_kwargs=self.connect)
                c.get(
                    PathMaker.client_log_file(i, id), 
                    local=PathMaker.client_log_file(i, id)
                )
                c.get(
                    PathMaker.worker_log_file(i, id), 
                    local=PathMaker.worker_log_file(i, id)
                )

        primary_addresses = committee.primary_addresses(0)
        progress = progress_bar(primary_addresses, prefix='Downloading primaries logs:')
        for i, address in enumerate(progress):
            host = Committee.ip(address)
            c = Connection(host, user='ubuntu', connect_kwargs=self.connect)
            c.get(
                PathMaker.primary_log_file(i), 
                local=PathMaker.primary_log_file(i)
            )

        # Parse logs and return the parser.
        Print.info('Parsing logs and computing performance...')
        return LogParser.process(PathMaker.logs_path(), faults=faults)

    def run(self, bench_parameters_dict, node_parameters_dict, debug=False):
        assert isinstance(debug, bool)
        Print.heading('Starting remote benchmark')
        try:
            bench_parameters = BenchParameters(bench_parameters_dict)
            node_parameters = NodeParameters(node_parameters_dict)
        except ConfigError as e:
            raise BenchError('Invalid nodes or bench parameters', e)

        # Select which hosts to use.
        selected_hosts = self._select_hosts(bench_parameters)
        if not selected_hosts:
            Print.warn('There are not enough instances available')
            return

        # Update nodes.
        try:
            self._update(selected_hosts, bench_parameters.collocate)
        except (GroupException, ExecutionError) as e:
            e = FabricError(e) if isinstance(e, GroupException) else e
            raise BenchError('Failed to update nodes', e)

        # Upload all configuration files.
        try:
            committee = self._config(
                selected_hosts, node_parameters, bench_parameters
            )
        except (subprocess.SubprocessError, GroupException) as e:
            e = FabricError(e) if isinstance(e, GroupException) else e
            raise BenchError('Failed to configure nodes', e)

        # Run benchmarks.
        for n in bench_parameters.nodes:
            committee_copy = deepcopy(committee)
            committee_copy.remove_nodes(committee.size() - n)

            for r in bench_parameters.rate:
                Print.heading(f'\nRunning {n} nodes (input rate: {r:,} tx/s)')

                # Run the benchmark.
                for i in range(bench_parameters.runs):
                    Print.heading(f'Run {i+1}/{bench_parameters.runs}')
                    try:
                        self._run_single(
                            r, committee_copy, bench_parameters, node_parameters, debug
                        )

                        faults = bench_parameters.faults
                        logger = self._logs(committee_copy, faults)
                        logger.print(PathMaker.result_file(
                            faults,
                            n, 
                            bench_parameters.workers,
                            bench_parameters.collocate,
                            r, 
                            bench_parameters.tx_size, 
                        ))
                    except (subprocess.SubprocessError, GroupException, ParseError) as e:
                        self.kill(hosts=selected_hosts)
                        if isinstance(e, GroupException):
                            e = FabricError(e)
                        Print.error(BenchError('Benchmark failed', e))
                        continue
