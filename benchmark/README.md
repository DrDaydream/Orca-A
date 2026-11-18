# Running Benchmarks
This document explains how to benchmark the codebase and read benchmarks' results. It also provides a step-by-step tutorial to run benchmarks on [Amazon Web Services (AWS)](https://aws.amazon.com) accross multiple data centers (WAN).

## Local Benchmarks
Run the benchmark tasks from this directory with Fabric. `fab local` keeps the
default 4 nodes, one worker per node, 50,000 tx/s, 512-byte transactions, and
20-second duration. It builds the current Orca-A checkout in release mode,
launches local processes, waits for the measurement window to finish, and then
reaps them before parsing logs. The traditional SUMMARY is followed by a fixed
20-second final-ordered comparison and a `logs/comparison.json` file. The comparison
uses the first client start as the window origin, counts first-replica finalized
transactions, and leaves missing sampled completions unfinished rather than
assigning them zero latency.

Edit the parameter dictionaries in `fabfile.py` for other run configurations. The
legacy SUMMARY remains useful for protocol diagnostics; use the final-ordered
comparison when comparing ordered throughput or client-perceived latency.

When running benchmarks, the codebase is automatically compiled with the feature flag `benchmark`. This enables the node to print some special log entries that are then read by the python scripts and used to compute performance. These special log entries are clearly indicated with comments in the code: make sure to not alter them (otherwise the benchmark scripts will fail to interpret the logs).


### Native Ubuntu 24.04 setup

Run these commands in Bash on the controller. Use your existing Python
installation; a new virtual environment is not required.

```bash
sudo apt-get update
sudo apt-get install -y build-essential cmake clang-14 libclang-14-dev \
  pkg-config libssl-dev curl ca-certificates git python3 python3-pip tmux chrony
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --profile minimal --default-toolchain 1.85.0
fi
source "$HOME/.cargo/env"
rustup toolchain install 1.85.0 --profile minimal
git clone --branch orca-a-grbc-echo-ready https://github.com/DrDaydream/Orca-A.git
cd Orca-A/benchmark
python3 -m pip install --user --break-system-packages -r requirements.txt
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS TOKIO_WORKER_THREADS
fab local
```

The repository pins Rust 1.85.0 and the Clang 14 build environment. On x86_64
Linux its RocksDB build enables SSE4.2 and PCLMUL; use CPUs supporting these
features. Tokio worker counts remain at their defaults. From this directory,
local builds use `../node` and place release binaries under `../target/release`.
`cargo build --locked` prevents silent dependency resolution changes. The first
release build can take considerably longer than the measurement itself.

### Parametrize the benchmark
After cloning the repo and [installing all dependencies](../README-GRBC.md), you can use [Fabric](http://www.fabfile.org/) to run benchmarks on your local machine.  Locate the task called `local` in the file [fabfile.py](fabfile.py):
```python
@task
def local(ctx):
    ...
```
The task specifies two types of parameters, the *benchmark parameters* and the *nodes parameters*. The benchmark parameters look as follows:
```python
bench_params = {
    'nodes': 4,
    'workers': 1,
    'rate': 50_000,
    'tx_size': 512,
    'faults': 0,
    'duration': 20,
}
```
They specify the number of primaries (`nodes`) and workers per primary (`workers`) to deploy, the input rate (tx/s) at which the clients submits transactions to the system (`rate`), the size of each transaction in bytes (`tx_size`), the adversarial-scheduling parameter (`faults`), and the duration of the benchmark in seconds (`duration`). The minimum transaction size is 9 bytes, this ensure that the transactions of a client are all different. The benchmarking script will deploy as many clients as workers and divide the input rate equally amongst each client. For instance, the configuration above deploys 4 clients, each submitting transactions to one node at a rate of 12,500 tx/s, for a total input rate of 50,000 tx/s.

Orca-A note: when `faults > 0`, all nodes remain online. In every round, all primaries use `ORCA_ADVERSARY_SEED`, the round, and the ordered committee to select the same pseudo-random set of `faults` adversarial authorities. A selected adversarial leader is forced into Rule 3 and cannot complete through Rule 1 or Rule 2; a non-adversarial leader checks Rule 1 and Rule 2 normally before falling back to Rule 3. Non-leader adversaries are silent. `ORCA_RULE3_BEHAVIOR=mixed` (the default) deterministically chooses whether an adversarial Rule-3 leader is silent or participates; `silent` and `participate` force either behavior. Silent authorities suppress their header and pause local batch production while continuing to receive messages. The default seed is `0`; set, for example, `ORCA_ADVERSARY_SEED=42 fab local` to obtain another reproducible schedule. Here `faults` controls the adversary set rather than omitting the last nodes.

When `faults > 0`, a separate deterministic coin defers approximately half of the non-adversarial leaders past Rule 1 so that they wait for Rule 2. The remaining leaders attempt Rule 1 normally, and a failed Rule-1 attempt may still use Rule 2. Consequently the measured Rule-1-to-Rule-2 ratio approaches 1:1 over long runs without preventing Rule 2 from preserving liveness. The printed percentages use all leaders, including Rule 3, as their denominator, so Rule 1 and Rule 2 are not expected to each print exactly 50%.

Client traffic during adversarial silence is selected with `ORCA_CLIENT_DURING_SILENCE`. Its default, `send`, keeps clients sending while the local BatchMaker is paused. `pause` gives each client a pre-generated deterministic silence bitmap, so it sends no transactions during its silent time slots. This is one-way and uses no Worker-to-Client control traffic. A slot defaults to `max_header_delay` milliseconds and can be overridden with `ORCA_CLIENT_SILENCE_SLOT_MS`. For example: `ORCA_CLIENT_DURING_SILENCE=pause ORCA_CLIENT_SILENCE_SLOT_MS=200 fab local`. The bitmap is a wall-clock workload schedule; it does not query the protocol's live round.

The nodes parameters determine the configuration for the primaries and workers:
```python
node_params = {
    'header_size': 1_000,
    'max_header_delay': 200,
    'gc_depth': 50,
    'sync_retry_delay': 10_000,
    'sync_retry_nodes': 3,
    'batch_size': 500_000,
    'max_batch_delay': 200
}
```
They are defined as follows:
* `header_size`: The preferred header size. The primary creates a new header when it has enough parents and enough batches' digests to reach `header_size`. Denominated in bytes.
* `max_header_delay`: The maximum delay that the primary waits between generating two headers, even if the header did not reach `max_header_size`. Denominated in ms.
* `gc_depth`: The depth of the garbage collection (Denominated in number of rounds).
* `sync_retry_delay`: The delay after which the synchronizer retries to send sync requests. Denominated in ms.
* `sync_retry_nodes`: Determine with how many nodes to sync when re-trying to send sync-request. These nodes are picked at random from the committee.
* `batch_size`: The preferred batch size. The workers seal a batch of transactions when it reaches this size. Denominated in bytes.
* `max_batch_delay`: The delay after which the workers seal a batch of transactions, even if `max_batch_size` is not reached. Denominated in ms.

### Run the benchmark
Once you specified both `bench_params` and `node_params` as desired, run:
```
$ fab local
```
The command recompiles the checked-out code in release mode with the benchmark
feature, generates fresh hidden JSON key/configuration files, and launches clients,
primaries, and workers. It keeps the processes alive until the complete window
starting at the earliest client `Start` has elapsed. A process failure or excessive
client startup skew rejects the run. Cleanup terminates and reaps owned children
before any logs are parsed, including after errors or interruption.

The traditional SUMMARY reports rule-ready timing for compatibility. The
FINAL ORDERED COMPARISON uses explicit `commit` timestamps when present and the
committed log timestamp for older logs. It counts batches finalized during
`[first client Start, Start + duration)`, divides transaction count by that full
configured duration, and reports mean/p95 sampled latency, completed samples,
and unfinished samples. A plateau never shortens the denominator; no completed
samples means `null` JSON latency (`N/A` on screen), rather than zero latency.
Comparison uses the earliest observed commit across replicas, not confirmation
by every replica. Check matching ordered prefixes separately when validating
replica agreement. All machines need synchronized clocks for meaningful latency.

`fab logs` can reprint the legacy SUMMARY from the `logs/` directory. To parse a
saved run with a known 20-second duration and write comparison JSON:

```bash
python3 - <<'PYTHON'
from benchmark.logs import LogParser
LogParser.process('logs', faults=0).print_comparison(20, 'logs/comparison.json')
PYTHON
```

## Ubuntu AWS benchmarks

Run all Fabric tasks from this outer `benchmark` directory. The controller needs
Python requirements, AWS credentials via Boto3's normal credential lookup (for
example an attached IAM role), and the RSA private key for the testbed. The SSH
key pair name is independent of the AWS credential profile. Import the same
public key in every configured region.

```bash
cp -n settings.example.json settings.json
# Edit settings.json for your SSH key, AWS regions, and instance type.
fab info
fab install
fab remote
```

The example names `DrDaydream/Orca-A`, branch `orca-a-grbc-echo-ready`, and
`m5.8xlarge` instances. `settings.json` is ignored by Git. Keep private keys and
AWS access tokens outside the repository. Existing benchmark parameter edits
in `fabfile.py` are used directly; no command-line parameter changes are needed.

`fab install` prepares Ubuntu dependencies, Rust 1.85.0 and chrony on the
controller and testbed, fetches the configured branch, checks out the controller's
exact HEAD commit remotely, and compiles the node and client binaries on every
machine. Only this preparation step requires sudo for packages/services.

`fab remote` reuses that build. It verifies the recorded commit and executables,
generates and uploads configuration, runs the experiment and downloads results.
It does not install dependencies, update source or compile on either the
controller or experiment machines. All existing experiment and adversary flags
remain available. Repeat `fab remote` directly when only workload parameters
change; repeat `fab install` for new machines or a different source commit.
After updating from an older installer, run `fab install` once to record the
successful-build marker. Missing or stale builds fail with that instruction.

The controller's native sources and build configuration must match HEAD, and
that commit must be available from the configured remote repository. Installation
rejects remote repositories with uncommitted tracked changes. Build flags and
locked dependencies remain the same; changing orchestration does not change
the native protocol or latency measurement code.

Builds clear inherited Rust flag and Tokio-thread overrides and use the
repository configuration with locked dependencies. Local key generation uses
`../node` and `../target/release`; remote builds run in `<repo>/node` and keep
binaries in `<repo>/target/release`. No new virtual environment is created and
application Tokio thread counts use their defaults.

The existing remote task parameters are preserved: `faults=3`, 10 nodes, one
collocated worker each, rates `[10_000, 110_000]`, 512-byte transactions,
300-second duration, two runs, 1,000-byte headers, and 200-ms header/batch delay.
Edit the `remote()` dictionaries in `fabfile.py` for another experiment. Record
the exact commit, parameter dictionaries, machine types, regions, and logs with
your results. Preparation/build/download time is outside the measurement window.
After launch, the remote runner polls every client log for its Start timestamp
with bounded SSH reads and a 120-second startup deadline. It rejects absent
starts or startup skew spanning the measurement window, then keeps nodes alive
until first Start plus the configured duration before stopping them. Extra
collection time does not shorten or extend the metric cutoff.

The remote runner prints the SUMMARY and comparison, appends legacy text results
under `results/`, and writes a separate comparison JSON for each rate/node/run
combination. A later invocation with the same parameters overwrites those JSON
files, so archive them with the original logs before rerunning. `fab kill` stops
the remote benchmark; `fab plot` aggregates the legacy text results.

### Instance management

`fab remote` and `fab install` use existing instances. To create instances,
`fab create --nodes 2` creates two machines in **each configured region**. In
five regions this is ten machines. `fab info` shows the current testbed;
`fab start` and `fab stop` change its running state; `fab destroy` terminates it.
The manager identifies testbed instances by `Name=dag-node`, uses the `dag`
security group, and allocates 200-GiB gp2 root volumes. Keep unrelated instances
outside that tag. Configure ingress sources appropriately for your deployment.
The committee retains Orca-A's primary/worker address fields, with no separate
fallback-protocol socket.

## Harness tests

```bash
python3 -m unittest discover -s tests -v
```

The tests cover final timing, fixed cutoffs, missing metadata and unfinished
samples, parser compatibility, process cleanup/startup failures, and a mocked
remote command/configuration flow. These tests do not contact AWS or SSH hosts.
