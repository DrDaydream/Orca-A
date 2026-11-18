# Five-protocol EC2 experiment tools

This independent distribution branch contains reviewed experiment tools. It does not change the protocol branches.

Latest: `five-protocol-tools-v3.3.zip` runs every case for 60 seconds and sets Orca-A, Orca-B and Bullshark `header_size` to 20 bytes at the scoped native runner boundary. Each instance pool still follows create -> install -> repeated remote -> destroy. See [v3.3 EC2 download, update and run commands](EC2_v3.3_60秒20B_更新命令.md). Use a new output directory, `/home/ubuntu/experiments/paper-results-v3.3-60s-h20`: previous 100-second results are not reused.

The v3.2 A1 native-summary fixes remain in Orca-A `c617ac3cfeb69a0dd32ca096ca53c8d62cbb6a83` and Orca-B `015346bb6e57af916ec9a0c698d7e34f0236a7a8`, both on `adversary-experiments`. A1 summaries exclude fixed silent replicas; 10 nodes with 3 silent identities report 7 included commit replicas. See [A1 repository update and log reparse commands](EC2_A1统计更新说明.md). v3.3 needs only the toolkit update if these repository revisions are already installed.

- `stop_after_current.py`: For the released v2.1 controller, wait for the active case archive to complete, then request controller exit and wait for recorded resource cleanup. Run as the experiment user on Ubuntu. The following case can begin before the stop is observed; retry unfinished cases when resuming.
- `five-protocol-tools-v3.3.zip`: Serial 60-second experiments, 20-byte header threshold for Orca-A/B/Bullshark, 480-case plan, parameter validation, archive and plotting tools, instructions and offline tests.
- `EC2_v3.3_60秒20B_更新命令.md`: Download/install the new toolkit and start the new plan in Screen.
- `five-protocol-tools-v3.2.zip`: Previous 100-second release with A1 native-summary compatibility; retained unchanged.
- `five-protocol-tools-v3.1.zip`: Previous release, retained unchanged.
- `EC2_A1统计更新说明.md`: Update the original EC2 directories, reparse old A1 logs, and install/resume v3.2.
- `SHA256SUMS`: File checksums.

The v3.1 Orca-A `57a6ccc` and Orca-B `a970a8d` revisions are still accepted for historical campaign migration. Use the latest revisions above for native A1 summary exclusion. Bullshark `63fe54b`, Mysticeti `b813f80`, and Mahi-Mahi `db58361` keep their existing behavior.

Stop and clean up the old run before replacing tools. v3.3 changes experimental parameters and rejects old campaigns even with `--upgrade-pool-policy`. Keep old results and start the new output directory. Subsequent v3.3 interruptions can resume that same new directory with `--retry-failed`.

No AWS credentials, private keys, settings files or experiment measurements are included.
