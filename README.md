# Five-protocol EC2 experiment tools

This independent distribution branch contains reviewed experiment tools. It does not change the protocol branches.

Latest: `five-protocol-tools-v3.2.zip` supports the A1 native-summary exclusion updates in Orca-A `c617ac3cfeb69a0dd32ca096ca53c8d62cbb6a83` and Orca-B `015346bb6e57af916ec9a0c698d7e34f0236a7a8`, both on `adversary-experiments`. A1 summaries exclude fixed silent replicas; 10 nodes with 3 silent identities report 7 included commit replicas. Existing plot measurements already use honest replicas and remain compatible after the checked migration. See [EC2 update and reparse commands](EC2_A1统计更新说明.md). The v3.1 archive remains available unchanged.

- `stop_after_current.py`: For the released v2.1 controller, wait for the active case archive to complete, then request controller exit and wait for recorded resource cleanup. Run as the experiment user on Ubuntu. The following case can begin before the stop is observed; retry unfinished cases when resuming.
- `five-protocol-tools-v3.2.zip`: Serial Orca-A, Orca-B, Bullshark, Mysticeti and Mahi-Mahi experiments. Create exactly 10, 20 or 35 instances per pool, install once, run pending cases, archive and destroy. Includes instructions, the 480-case plan, verification metadata and offline tests.
- `five-protocol-tools-v3.1.zip`: Previous release, retained unchanged.
- `EC2_A1统计更新说明.md`: Update the original EC2 directories, reparse old A1 logs, and install/resume v3.2.
- `SHA256SUMS`: File checksums.

The v3.1 Orca-A `57a6ccc` and Orca-B `a970a8d` revisions are still accepted for historical campaign migration. Use the latest revisions above for native A1 summary exclusion. Bullshark `63fe54b`, Mysticeti `b813f80`, and Mahi-Mahi `db58361` keep their existing behavior.

Stop and clean up the old run before replacing tools or updating controller repositories. To preserve compatible completed plot results, resume v3.2 in the same output directory with `--upgrade-pool-policy --retry-failed`. Historical native summaries are kept as originally recorded.

No AWS credentials, private keys, settings files or experiment measurements are included.
