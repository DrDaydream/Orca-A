# Reuse validated dependencies at round advancement

Status: independently retained experiment; not promoted as a performance gain.
Operational branch: orca-a-grbc-echo-ready at bf69ba4305ae4752d223cf8203f40f4c261c47c5.

The primary already validates all strong and weak dependencies before adding
a complete authenticated header to validated. Local FINISH used to scan those
same dependencies again. try_advance_completed now reuses that membership;
consensus commit feedback for an unvalidated header still checks completion
records and retains the existing pending/retry path. Completion is monotonic,
and both completion callers bind the full header before reaching this code.
GC of parent metadata does not revoke a child's prior validation.

Production change: one existing condition and its explanation, net seven
lines after formatting. No new cache, task, queue, timer, protocol message,
thread setting, quorum or experimental parameter is introduced.

The new FINISH regression first fails on the baseline when parent metadata
has been GC'd and the dependency store cannot answer. The candidate completes
through the real final READY-vote path without a dependency store read. A
second test keeps an unvalidated committed child pending with READY-only
strong/weak metadata, then releases it only after completion records arrive.
Workspace verification: 146 passed, 0 failed, 1 manual microbenchmark ignored;
release build succeeded. Independent read-only review found no blocker. An
optional test note is to isolate the missing-strong arrival order in the
second regression; existing helper-level coverage checks that case.

Same-key late smoke: 698 identical headers, round 194, no lag, last output
within 101 ms of shutdown. One-absent smoke: 517 common headers, round 173,
no lag. Both use GC depth 50 and the final candidate binary.

## Controlled load setup and interference

Windows loopback, default Tokio threads, serial startup, 20-second fixed
window, 50,000 tx/s, 512 bytes, header size 20, GC 50, batch 500,000 bytes,
header/batch delay 200 ms. Keys, client executable and parameters match
between variants. Stores/ports are fresh and run directories start without
NTFS compression. Completed evidence directories may be losslessly compressed
between runs to free disk space; no active store is compressed.

The initial four-node sequence detected another chat's benchmark starting
during candidate B and baseline B. Those results are retained and not used
to attribute a variant effect. After extending the pre-run idle observation
from 5 to 30 seconds, the entire four-node sequence was repeated. The repeat
and ten-node runs have no observed foreign benchmark overlap under one-second
process sampling. This sampling cannot exclude every background host effect.

| Run | Final TPS | Final consensus ms | Final E2E ms | E2E p95 ms | Completed/sent | Rate warnings | Foreign overlap |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| n4-baseline-a | 48,654.60 | 273.42 | 313.79 | 621 | 1557/1600 | 0 | no |
| n4-candidate-a | 49,516.30 | 245.35 | 288.86 | 569 | 1584/1600 | 0 | no |
| n4-candidate-b | 26,396.15 | 353.22 | 444.79 | 892 | 845/859 | 596 | yes |
| n4-baseline-b | 28,907.15 | 462.57 | 613.02 | 2024 | 925/1185 | 500 | yes |
| n4-repeat-baseline-a | 49,436.20 | 244.30 | 288.58 | 526 | 1581/1600 | 0 | no |
| n4-repeat-candidate-a | 48,850.00 | 268.83 | 313.57 | 578 | 1563/1600 | 0 | no |
| n4-repeat-candidate-b | 49,627.65 | 188.59 | 227.20 | 613 | 1587/1600 | 0 | no |
| n4-repeat-baseline-b | 49,316.95 | 256.02 | 295.34 | 494 | 1578/1599 | 0 | no |
| n10-baseline-a | 48,167.00 | 831.31 | 1020.97 | 2223 | 3853/3988 | 214 | no |
| n10-candidate-a | 45,542.20 | 1182.16 | 1370.74 | 3961 | 3645/3991 | 61 | no |
| n10-candidate-b | 48,223.90 | 866.77 | 1070.56 | 2819 | 3857/3941 | 163 | no |
| n10-baseline-b | 47,591.00 | 874.55 | 1072.13 | 2279 | 3808/3988 | 279 | no |

The repeated four-node run means are 49,376.57
versus 49,238.82 final TPS, and
291.96 versus
270.39 ms final E2E. All four runs
have zero rate warnings. Candidate latency varies substantially between its
two runs; these repetitions do not prove a stable improvement or throughput
equivalence.

Ten-node means are 47,879.00 versus
46,883.05 final TPS and
1046.55 versus
1220.65 ms final E2E. Every ten-node
run reports rate warnings. The two candidate runs have last-commit ages of
1.58 and 1.40 seconds, versus 0.65 and 0.68 seconds for the baseline. Shared
ordered prefixes agree and no duplicate header output was found, but these
tail gaps must not be described as uniform near-shutdown progress.

Existing logs also support a completed-sample decomposition into client to
batch, batch to header, header to the committing leader, that leader to rule
readiness, and readiness to final output. The component means sum exactly to
the fixed-window E2E mean; unfinished samples and the last-five-second cohort
are recorded separately. This identifies where observed time is spent, not
the causal contribution of this one condition or individual network steps.

The candidate removes verified duplicate work but has not demonstrated the
combined lower-final-latency/maintained-throughput objective. Keep it separate.
Windows results do not establish Ubuntu/WAN performance. The base's unbounded
never-output-history retention and deterministic-leader/GPC limitations remain.
