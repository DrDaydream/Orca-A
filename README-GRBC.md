# Orca-A complete-proposal GRBC branch

> Current experiment (2026-11-20): local dependency waits are removed from GRBC, round advancement, Dag admission and final output. See [NO-DEPENDENCY-WAIT.md](NO-DEPENDENCY-WAIT.md) for the exact changes, quorum semantics and effects on missing-history output. The dependency waiting behavior and historical validation results below describe the earlier baseline.

This branch starts at Orca-A `54bd1f29fc03414a16da7044e5eb31df92e6212b` and ports the retained broadcast and benchmark changes from Orca-B `a309da2`. The protocol reference is the supplied Orca draft, Section II (GRBC), Section IV (Orca-A), and Algorithm 5 (ordered output). Orca-A keeps its Good, Deferred, and three `r mod 3` pessimistic recovery lanes. It has no ABA subsystem.

## Broadcast and storage

```text
worker seals and stores a local batch
  -> primary fixes the header, round, proposer, strong/weak/virtual edges
  -> primary broadcasts Proposal(header + all referenced batch bytes)
  -> each replica verifies and stores the complete proposal, then broadcasts ECHO
  -> ECHO quorum: deliver local READY and broadcast a signed READY vote
  -> READY quorum: deliver local FINISH
```

The normal path has no separate worker-to-worker ACK quorum. ECHO and READY are all-to-all signed votes with independent deduplication. The standard `f+1` READY relay is retained; it can trigger READY delivery and relay before a local ECHO quorum. FINISH still requires the READY quorum. A complete proposal, valid dependencies, and consistent `(round, proposer, digest)` binding are required before voting. The Rust names `GradeOne` and `GradeTwo` mean READY and FINISH respectively. `Certificate` remains the name of a local delivery record; aggregate certificates are not broadcast.

Large proposals and small votes have separate processing queues and sender connections. Fixed committee thresholds and decoded public keys are reused. Core processes at most 32 work items per burst, including individual votes inside a batch. Queued votes from the same sender, phase and round can be coalesced; no timer waits for a full batch. Proposal verification, batch hashing and storage occur outside the vote loop. A valid local proposal begins broadcasting concurrently with local storage; local voting waits for storage completion.

Each primary stores batch contents once by digest and stores compact header metadata separately. Dependencies read metadata. Repair requests ask for votes when the proposal already exists; the first request for missing data targets an authenticated voter, with subsequent retries preserving recovery. In-flight tracking has a fixed grace period, and only successfully verified/stored proposals enter the bounded duplicate cache.

## Orca-A finalization

Good's observed-quorum condition requires the leader already in the accepted DAG. Its accepted-vote alternative retains the `f+1` threshold. Deferred can recover an authenticated, complete observed proposal from strong or virtual support. Force acceptance under GDP evidence does not fabricate a transport FINISH event.

Actual local commit feedback can complete stored dependencies and advance the primary, without soliciting obsolete GRBC votes or manufacturing grade events. Strong and weak edges determine causal output; virtual edges only supply GDP evidence. Weak-edge selection respects Orca-A's `f` bound and retains the remaining candidates for later proposals.

Only the continuous resolved leader prefix is output. For example, skipping round 2 never releases round 3 while round 1 remains undecided. A pending leader also waits for its entire required causal history, including missing ancestors shared with another pending leader. When the history arrives, its owner is requeued and drained immediately.

## Ubuntu 24.04

Use a fresh checkout and fresh benchmark databases for this branch. Its wire and storage formats differ from the original branch, so every primary and worker in an experiment must use the same revision.

```bash
set -euo pipefail
sudo apt-get update
sudo apt-get install -y \
  build-essential cmake clang-14 libclang-14-dev pkg-config \
  libssl-dev libffi-dev curl ca-certificates git tmux \
  python3 python3-dev python3-pip

mkdir -p "$HOME/experiments"
cd "$HOME/experiments"
git clone --branch orca-a-grbc-echo-ready --single-branch \
  https://github.com/DrDaydream/Orca-A.git Orca-A-grbc
cd Orca-A-grbc

export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --profile minimal --default-toolchain 1.85.0
fi
rustup toolchain install 1.85.0 --profile minimal

python3 - <<'PY'
from pathlib import Path
import platform
flags = set(Path('/proc/cpuinfo').read_text().split())
assert platform.machine() == 'x86_64', 'These commands target Ubuntu x86_64'
assert {'sse4_2', 'pclmulqdq'} <= flags, 'The checked-in Linux CRC build requires SSE4.2/PCLMUL'
PY

python3 -m pip install --user --break-system-packages -r benchmark/requirements.txt
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS
unset TOKIO_WORKER_THREADS
git log -1 --oneline
cargo --version
cd benchmark
fab local
```

The toolchain file selects Rust/Cargo 1.85.0 and the tracked lockfile permits `--locked` builds. The repository selects Clang 14 for RocksDB. Linux x86_64 builds enable SSE4.2 and PCLMUL for hardware CRC32C; the Windows target is unaffected. The first build can take several minutes. To display build progress before the first benchmark, run `cargo build --locked --release -p node --features benchmark` from the repository root. Subsequent unchanged runs reuse Cargo's cache.

Edit experiment parameters in `benchmark/fabfile.py` and keep using `fab local`. The existing defaults are four nodes, one worker per node, 50,000 tx/s, 512-byte transactions, a 1,000-byte header threshold, 500,000-byte batches, and 200 ms header/batch delays. Thread counts use Tokio's default. The `faults` parameter selects the existing experimental adversary schedule; it does not simply stop that many processes.

For an existing checkout of this new branch, save any experiment logs you need, then update with `git pull --ff-only` and run `fab local` from `benchmark/`. Git will stop if an incoming change conflicts with local parameter edits; do not discard those edits. Remote setup and configuration are described in `benchmark/README.md`.

## Measurement and validation

The legacy SUMMARY retains its rule-ready latency convention: its endpoint is when the carrying leader first satisfies a rule. The additional `FINAL ORDERED COMPARISON` and `logs/comparison.json` use final ordered commit timestamps, a fixed window beginning at the first client's Start event, and the earliest committing replica. Unfinished samples are reported separately; zero completed samples produce an unavailable latency, not zero latency. P95 uses the nearest-rank completed sample. Clock alignment is required for multi-host comparisons.

Local benchmark processes are owned and reaped by the benchmark before logs are parsed. The measurement window begins at the client Start event, so process launch delay does not shorten the run. This also avoids terminating unrelated tmux sessions.

```bash
cargo test --locked --workspace -- --test-threads=1
cargo build --locked --release -p node --features benchmark
PYTHONPATH=benchmark python3 -m unittest discover -s benchmark/tests -v
python3 scripts/grbc_smoke.py --binary target/release/node --scenario all
python3 scripts/grbc_smoke.py --binary target/release/node --scenario silent
python3 scripts/grbc_smoke.py --binary target/release/node --scenario late
python3 scripts/grbc_smoke.py --binary target/release/node --nodes 10 --scenario all
```

These tests check grade thresholds, signatures, payload integrity, deduplication, missing history, vote repair, ordered output and process progress. Smoke tests are correctness checks with small loads; they do not measure a latency or TPS improvement. Ubuntu/AWS performance must be measured on the intended hosts with the same parameters as the comparison baseline.

Validation on 2026-11-16 used Windows x86_64, Rust 1.85.0 and RocksDB 6.20.3. The release workspace suite with `benchmark` enabled passed 138 tests, with one manual microbenchmark intentionally ignored. The default-feature workspace check, release node build, and all 28 Python harness tests passed. The Windows build used local compiler overrides and matching prebuilt RocksDB libraries; the Ubuntu installation and AWS/SSH paths were not run on real remote hosts.

Each real-process smoke ran for 12 seconds using default Tokio thread counts. All active replicas emitted the same ordered sequence, had no duplicate header output, committed payloads, and continued making progress near shutdown:

| Committee / scenario | Active replicas | Common committed-header prefix | Final round lag |
| --- | ---: | ---: | ---: |
| 4 nodes, all present | 4 | 699 | 0 |
| 4 nodes, one absent throughout | 3 | 550 | 0 |
| 4 nodes, one joins after 3 seconds | 4 | 666 | 0 |
| 10 nodes, all present | 10 | 1,672 | 0 |

This remains an experimental implementation. Leader selection uses the repository's deterministic schedule, not the paper's threshold GPC; this branch does not establish the full cryptographic GPC or orchestration-resistance claim. The supplied paper's prose takes precedence where its draft pseudocode is inconsistent.

### GC boundary regression — 2026-11-17

A four-node run with 20-byte headers, GC depth 50 and 50,000 tx/s exposed a
finalization plateau around round 52. New proposals continued, but fallback
history traversal treated already-ordered, pruned ancestors as missing data.
The earlier small-load smoke used GC depth 200 and did not exercise this boundary.

Consensus now retains exact ordered digests (including genesis) as a causal
traversal frontier. It stops repeating history walks at that frontier, accepts
those exact dependencies after header GC, and still waits for unknown digests.
New output invalidates only affected fallback snapshots through the existing
reverse indexes, and the same event processes the resulting work. No quorum,
GDP decision, consecutive-prefix, or wire-format rule changes are required.
The compact ledger retains one digest per emitted vertex, rather than headers
or payloads; its memory grows with total output and is not bounded by GC depth.

Four new regressions failed before their respective fixes and now pass. The
release workspace suite passes 142 tests (one manual microbenchmark ignored).
The smoke script now defaults to GC depth 50, configurable with --gc-depth.
One-absent, one-late and ten-node smokes all pass with common prefixes of 559,
684 and 1,774 headers. The ten-node shutdown snapshot has a two-round tail lag;
all replicas agree on the shared prefix and continue making progress.

Windows loopback runs with the user's configuration and default threads:

| Run | Fixed-window TPS | Completed/sent samples | Last committed round |
| --- | ---: | ---: | ---: |
| Before fix, 20 s | 8,789.1 | 282 / 1,600 | 52 (stalled) |
| Corrected candidate, 20 s | 46,389.9 | 1,486 / 1,600 | 272 |
| Corrected candidate, 60 s | 47,389.1 | 4,555 / 4,800 | 796 |

The two corrected runs have no client rate warnings and agree across all four
replicas. Their fixed-window final consensus/end-to-end means are 281.8/325.1 ms
and 292.5/341.0 ms respectively. These results demonstrate recovery of sustained
output on this host. The old latency excludes nearly all post-stall requests,
so comparing its mean directly to a full successful run is misleading. They
do not establish an Ubuntu/WAN latency improvement.

### Late, never-output history — 2026-11-17

A further same-key late-join test exposed an additional GC case in 008ad5d:
an observed round-1 vertex had never been output when its metadata was
pruned. Final output stopped near round 52 while later leaders continued
becoming rule-ready. The same behavior occurred with and without the
independent Helper reuse experiment.

GC now retains never-output Dag, VDag and observed metadata. It still prunes
ordered history using the existing policy. Both causal traversal and final
pending-sequence filtering use exact ordered-digest membership, matching
Algorithm 5's deliveredVertices set. A newer output from an author does not
imply that all older vertices from that author were output. Two regressions
first failed under the old code and now assert actual old-ancestor output,
prefix advancement and no duplicate output.

This retains metadata already observed by this process. It does not add an
archival recovery path for arbitrary never-observed old vertices or provide
restart recovery. Never-output headers and associated dependency/transitive
ancestor indexes can remain indefinitely; memory and GC scan cost are not
bounded by gc_depth. This is a correctness tradeoff, not a bounded-memory
claim. The exact ordered-digest ledger also still grows with total output.

The final source passes 144 release workspace tests with one manual benchmark
ignored, and its release node build succeeds. A same-key 12-second late-join
smoke emits 709 identical headers across all four replicas, reaches round
199 with no final round lag, and commits within 94 ms of shutdown. The same
retention logic, before consolidating the identical Dag/VDag loops, also
passed a 30-second late-join smoke at round 528 with 1,987 identical headers;
ten-node and one-absent smokes passed with prefixes of 1,848 and 550 and no
final round lag. The smoke runner accepts --keys-from to reproduce a leader
schedule while allocating fresh ports and databases.

The final executable's 20-second Windows loopback run uses the same parameters
listed above and default threads. Its fixed-window final TPS is 49,529.95,
final consensus/end-to-end means are 255.84/296.79 ms, and end-to-end p95 is
497 ms. It completes 1,583 of 1,600 samples, reports no client rate warnings,
and all replicas emit the same 1,040 headers, with new output within 205 ms
of shutdown. This checks sustained output after the correctness fix; it is
not a paired experiment establishing lower latency.

The separate orca-a-repair-reuse-experiment reduces repeated reconstruction
and serialization of adjacent complete-proposal repairs. It is not included
here: its measured latency/throughput tradeoff remains inconclusive. Thread
defaults, fabric parameters, quorum rules and the serial startup procedure
remain unchanged.
