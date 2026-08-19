# Orca-A

[![Rust](https://github.com/DrDaydream/Orca-A/actions/workflows/rust.yml/badge.svg)](https://github.com/DrDaydream/Orca-A/actions/workflows/rust.yml)
[![Ubuntu](https://img.shields.io/badge/Ubuntu-24.04-E95420?style=flat-square&logo=ubuntu)](https://ubuntu.com/)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg?style=flat-square)](LICENSE)

This repository provides an experimental implementation of **Orca-A**, built on the [Narwhal and Tusk](https://arxiv.org/pdf/2105.11827.pdf) codebase. Orca-A extends the original DAG protocol with graded reliable broadcast, VDag storage, strong and virtual references, and three leader commit rules.

The codebase is intended for protocol research, benchmarking, and modification. It is not production software, although it uses real cryptography ([dalek](https://doc.dalek.rs/ed25519_dalek)), asynchronous networking ([Tokio](https://docs.rs/tokio)), and persistent storage ([RocksDB](https://rocksdb.org/)).

## Quick Start

The protocol is implemented in Rust. Benchmark orchestration and result parsing are implemented in Python using [Fabric](https://www.fabfile.org/).

The following commands target Ubuntu 24.04 and install dependencies directly into the current user environment:

~~~bash
git clone https://github.com/DrDaydream/Orca-A.git
cd Orca-A

sudo apt-get update
sudo apt-get install -y \
  build-essential cmake clang-14 libclang-14-dev curl git tmux \
  python3 python3-pip

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

python3 -m pip install --user --break-system-packages \
  -r benchmark/requirements.txt
export PATH="$HOME/.local/bin:$PATH"
~~~

RocksDB's bindgen step must use a compatible Clang installation:

~~~bash
export LIBCLANG_PATH=/usr/lib/llvm-14/lib
export CLANG_PATH=/usr/bin/clang-14
export CC=/usr/bin/clang-14
export CXX=/usr/bin/clang++-14
export CXXFLAGS='-include cstdint'
~~~

The local benchmark parameters are in `benchmark/fabfile.py`. In particular:

~~~python
bench_params = {
    'faults': 0,
    'nodes': 4,
    'workers': 1,
    'rate': 50_000,
    'tx_size': 512,
    'duration': 20,
}
~~~

The `faults` value enables the dynamic adversary schedule; it does not remove the last f processes. A valid Byzantine configuration must satisfy `nodes >= 3 * faults + 1`.

Run the default local benchmark from the benchmark directory:

~~~bash
cd benchmark
fab local
~~~

The first run compiles the workspace in release mode with the `benchmark` feature and may take several minutes.

### Local adversary options

The adversary count for `fab local` comes from `benchmark/fabfile.py`. Set `'faults': 0` for a no-adversary baseline, or set it to a positive value before using the following commands:

~~~bash
# Reproducible default: Rule-3 adversarial leaders are mixed between
# silence and participation. Clients continue sending during silence.
ORCA_ADVERSARY_SEED=42 \
ORCA_RULE3_BEHAVIOR=mixed \
ORCA_CLIENT_DURING_SILENCE=send \
fab local

# The same protocol schedule, but clients pause according to the
# pre-generated wall-clock silence schedule.
ORCA_ADVERSARY_SEED=42 \
ORCA_RULE3_BEHAVIOR=mixed \
ORCA_CLIENT_DURING_SILENCE=pause \
ORCA_CLIENT_SILENCE_SLOT_MS=200 \
fab local

# Force every adversarial Rule-3 leader to remain silent.
ORCA_ADVERSARY_SEED=42 \
ORCA_RULE3_BEHAVIOR=silent \
ORCA_CLIENT_DURING_SILENCE=pause \
fab local

# Control case: adversarial Rule-3 leaders continue participating.
ORCA_ADVERSARY_SEED=42 \
ORCA_RULE3_BEHAVIOR=participate \
ORCA_CLIENT_DURING_SILENCE=send \
fab local
~~~

The options are:

| Variable | Default | Meaning |
|---|---|---|
| `ORCA_ADVERSARY_SEED` | `0` | Deterministic per-round schedule seed |
| `ORCA_RULE3_BEHAVIOR` | `mixed` | `mixed`, `silent`, or `participate` |
| `ORCA_CLIENT_DURING_SILENCE` | `send` | Keep client input or pause it in silent slots |
| `ORCA_CLIENT_SILENCE_SLOT_MS` | `max_header_delay` | Wall-clock schedule slot in milliseconds |

With `faults > 0`, every node derives the same f adversarial authorities for each round. An adversarial leader is routed to Rule 3. The non-adversarial Rule-1/Rule-2 schedule approaches a 1:1 split over longer runs. Percentages are calculated over all leaders, including Rule 3 leaders.

### No-adversary baseline (`faults = 0`)

Set `'faults': 0` in `benchmark/fabfile.py` and run from the benchmark directory:

~~~bash
RUST_LOG=info fab local
~~~

The following output was produced by a 4-node, 50,000 tx/s, 20-second local run:

~~~text
-----------------------------------------
 SUMMARY:
-----------------------------------------
 + CONFIG:
 Faults: 0 node(s)
 Committee size: 4 node(s)
 Worker(s) per node: 1 worker(s)
 Collocate primary and workers: True
 Input rate: 50,000 tx/s
 Transaction size: 512 B
 Execution time: 20 s

 Header size: 1,000 B
 Max header delay: 200 ms
 GC depth: 50 round(s)
 Sync retry delay: 10,000 ms
 Sync retry nodes: 3 node(s)
 batch size: 500,000 B
 Max batch delay: 200 ms

 + RESULTS:
 Consensus TPS: 48,566 tx/s
 Consensus BPS: 24,866,043 B/s
 Consensus latency: 397 ms
 Leader commit latency: 224 ms
 Non-leader commit latency: 572 ms
 All committed headers latency: 505 ms
 Leader commit interval: 192 ms
 Non-leader rule-order latency: 453 ms
 Rule 1 leader ratio: 80.00%
 Rule 2 leader ratio: 8.18%
 Rule 3 commit leader ratio: 0.00%
 Rule 3 skip leader ratio: 11.82%
 Rule 1 block ratio: 90.31%
 Rule 2 block ratio: 9.69%
 Rule 3 block ratio: 0.00%

 End-to-end TPS: 48,250 tx/s
 End-to-end BPS: 24,703,968 B/s
 End-to-end latency: 546 ms
-----------------------------------------
~~~

### Preserved adversarial result (`faults = 1`)

For comparison, this is the previously recorded 4-node, 1-fault, 20-second local result using the adversary commands above:

~~~text
-----------------------------------------
 SUMMARY:
-----------------------------------------
 + CONFIG:
 Faults: 1 node(s)
 Committee size: 4 node(s)
 Worker(s) per node: 1 worker(s)
 Collocate primary and workers: True
 Input rate: 50,000 tx/s
 Transaction size: 512 B
 Execution time: 20 s

 Header size: 1,000 B
 Max header delay: 200 ms
 GC depth: 50 round(s)
 Sync retry delay: 10,000 ms
 Sync retry nodes: 3 node(s)
 batch size: 500,000 B
 Max batch delay: 200 ms

 + RESULTS:
 Consensus TPS: 37,766 tx/s
 Consensus BPS: 19,336,237 B/s
 Consensus latency: 506 ms
 Leader commit latency: 499 ms
 Non-leader commit latency: 785 ms
 All committed headers latency: 700 ms
 Leader commit interval: 210 ms
 Non-leader rule-order latency: 503 ms
 Rule 1 leader ratio: 41.58%
 Rule 2 leader ratio: 39.60%
 Rule 3 commit leader ratio: 6.93%
 Rule 3 skip leader ratio: 11.88%
 Rule 1 block ratio: 47.13%
 Rule 2 block ratio: 45.22%
 Rule 3 block ratio: 7.64%

 End-to-end TPS: 37,487 tx/s
 End-to-end BPS: 19,193,387 B/s
 End-to-end latency: 711 ms
-----------------------------------------
~~~

`Consensus latency` measures header creation to consensus commit. `End-to-end latency` begins when the benchmark client submits a sampled transaction. The additional leader, rule-order, and Rule 1/2/3 fields are Orca-A-specific statistics produced after the run.

## Next Steps

- Read [Narwhal and Tusk: A DAG-based Mempool and Efficient BFT Consensus](https://arxiv.org/pdf/2105.11827.pdf).
- See [benchmark/README.md](benchmark/README.md) for benchmark parameters and log interpretation.
- See [README-AWS-10-20-50节点完整部署.md](README-AWS-10-20-50节点完整部署.md) for complete AWS 10/20/50-node deployment, cross-Region networking, and adversary examples.
- See [README-WINDOWS五区域PEM部署.md](README-WINDOWS五区域PEM部署.md) when a Windows computer controls five AWS Regions using one PEM per Region.
- See [README-50节点并行下载.md](README-50节点并行下载.md) to download or update Orca-A concurrently on 50 servers through node0.
- Inspect the [primary](primary), [worker](worker), and [consensus](consensus) crates for protocol implementation details.

## License

This software is licensed under [Apache License 2.0](LICENSE).
