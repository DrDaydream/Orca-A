# Primary write-buffer headroom

Primary stores now allow up to four RocksDB write buffers instead of the default
two. Worker stores retain the default. The buffer size remains the RocksDB
default of 64 MiB, so the extra potential memtable capacity is about 128 MiB per
primary. Validation, atomic storage acknowledgement, WAL and write options,
voting thresholds and final ordering use the existing paths.

The preceding A1 experiment showed primary writes stopping while two immutable
memtables waited for a flush. Allowing additional immutable buffers gives a
short flush backlog room before proposal storage has to wait. It does not make
disk I/O free or guarantee that sustained overload can be absorbed indefinitely.

Two opposite-order local comparisons used the published Orca-A 0ed4c9a source,
changing only the primary buffer count. Both used the same keys and published
client/worker binaries, 10 nodes with 7 active, nominal 50,000 tx/s, 512-byte
transactions, 100 ms synthetic one-way primary network delay, and
warmup/measurement/drain of 10/60/10 seconds. All processes shared one WSL host.

| Run order | Buffers | Final-window E2E mean | Final-window TPS | Release cohort complete by drain |
|---|---:|---:|---:|---:|
| Pair 1, first | 2 | 8,938.190 ms | 28,929.333 | 93.52% |
| Pair 1, second | 4 | 2,473.239 ms | 35,028.017 | 100% |
| Pair 2, first | 4 | 3,244.281 ms | 34,474.367 | 100% |
| Pair 2, second | 2 | 4,627.777 ms | 33,543.717 | 100% |

Pair 2 supplies the comparison with complete cohorts on both sides: E2E fell
29.9% and TPS rose 2.8%. Its release-cohort E2E means were 4,616.529 and
3,224.509 ms respectively. All replica output prefixes agreed. The default
primary stores in pair 2 reported seven write stops; the four-buffer stores
reported none. Pair 1's default stores reported no write stops, so write-stop
elimination does not account for all of the observed difference.

This is a reversible storage setting for further EC2 evaluation. Two local
pairs do not establish the less-than-3-second EC2 goal or a throughput bound at
other loads and committee sizes. Read the clocked final-commit measurement,
throughput and unfinished release cohort together. See A1-RUNTIME-CLOCK.md and
benchmark/A1_ACCEPTANCE.md for the retained measurement definitions.
