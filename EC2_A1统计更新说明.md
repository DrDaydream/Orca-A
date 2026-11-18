# Orca-A / Orca-B：排除 A1 静默节点的统计更新

这次更新后，A1 的固定静默节点不参与原生提交统计。10 节点、`--byzantine 3` 时，`End-to-end latency` 和 `Final end-to-end mean` 都只使用节点 3～9 的提交观测。全部客户端和 worker 的输入仍保留，诚实节点未完成的交易仍记录为 unfinished。

Orca-A 提交：`c617ac3cfeb69a0dd32ca096ca53c8d62cbb6a83`。

Orca-B 提交：`015346bb6e57af916ec9a0c698d7e34f0236a7a8`。

两个仓库均使用 GitHub 的 `adversary-experiments` 分支。此次修改涉及 Python 日志统计及参数传递、测试、说明；没有修改 Rust、Cargo、fabfile.py、settings.json 或安装命令。

## 更新仓库

在当前实验结束、批处理退出后执行。若仍要续跑已有一键实验，先用旧工具完成 cleanup；不要在正在运行的组之间直接替换代码。下面会检查一键实验的运行锁。

```bash
(
  set -e
  exec 9>/tmp/five-protocol-figure-matrix.lock
  flock -n 9 || { echo '一键实验仍在运行，请等其退出后更新。'; exit 1; }

  git -C /home/ubuntu/experiments/Orca-A fetch origin adversary-experiments
  git -C /home/ubuntu/experiments/Orca-A merge --ff-only c617ac3cfeb69a0dd32ca096ca53c8d62cbb6a83

  git -C /home/ubuntu/experiments/Orca-B-grbc fetch origin adversary-experiments
  git -C /home/ubuntu/experiments/Orca-B-grbc merge --ff-only 015346bb6e57af916ec9a0c698d7e34f0236a7a8
)
```

这两次快进不会改动 fabfile.py 和 settings.json。若本次涉及的 logs.py、local.py 或 remote.py 有本地修改，Git 会保留本地文件并拒绝覆盖；不要执行强制重置或盲目恢复 stash。

## 本地测试

在同一台机器上按顺序运行，先等 Orca-A 完成，再执行 Orca-B。无需 fab create 或 fab install。

```bash
cd /home/ubuntu/experiments/Orca-A/benchmark && \
fab local --nodes 10 --workers 1 --rate 10000 --tx-size 512 \
  --duration 100 --faults 0 --adversary A1 --byzantine 3 \
  --seed 7 --start-round 1 --attack-rounds 0
```

```bash
cd /home/ubuntu/experiments/Orca-B-grbc/benchmark && \
fab local --nodes 10 --workers 1 --rate 10000 --tx-size 512 \
  --duration 100 --faults 0 --adversary A1 --byzantine 3 \
  --seed 7 --start-round 1 --attack-rounds 0
```

摘要应包含：

```text
Committee size: 10 node(s)
Commit replicas included: 7 node(s)
Excluded A1 silent replica IDs: 0,1,2
```

日志目录新增 `measurement-population.json`，与原始日志一起保存。之后 `fab logs` 会沿用这份排除名单。静默身份按整次运行排除，有限攻击窗口也采用同一名单；其它敌手模式的原生统计仍沿用原口径。池化样本均值与最早副本均值各自的时间起点、聚合方式仍按原定义，仅改变提交观测节点集合。

直接在已有实例上运行 `fab remote` 时，当前构建标记绑定仓库提交，更新提交后先执行一次 `fab install`，再连续运行多次 remote。一键脚本在每批创建实例后会自动 install，无需额外手动安装。

## 重新计算刚才的旧日志

如果保留的是刚才 **10 节点、A1、byzantine=3、100 秒** 的日志，可在对应协议的 benchmark 目录执行以下命令，无需重跑实验。它新增排除名单和 comparison-honest.json；更早的终端输出与旧报告文件不会自动改写。

```bash
python3 - <<'PY'
from benchmark.logs import LogParser

parser = LogParser.process(
    'logs', faults=0,
    adversary={'mode': 'silent', 'byzantine_ids': [0, 1, 2]},
)
print(parser.result())
parser.print_comparison(100, 'logs/comparison-honest.json')
PY
```

若使用了其它 `--byzantine-ids` 或时长，必须按那一次运行的真实参数替换名单和 100。需要保留全部原始 client/worker/primary 日志。

## 一键工具更新至 v3.2

v3.1 对 remote.py 有精确指纹检查，更新两个仓库后应配合 v3.2。绘图本来就排除了静默副本，所以已有的 measurement.json 和图数据可在校验后保留；历史原生 summary 保留原样。

先下载并验证，下载过程不会修改正在使用的工具：

```bash
(
  set -e
  FIVE_PROTOCOL_DIST=/home/ubuntu/experiments/figure-tools-download-v3.2
  if [ -d "$FIVE_PROTOCOL_DIST/.git" ]; then
    git -C "$FIVE_PROTOCOL_DIST" fetch origin experiment-tools
    git -C "$FIVE_PROTOCOL_DIST" merge --ff-only FETCH_HEAD
  else
    git -c core.autocrlf=false clone --branch experiment-tools --single-branch \
      https://github.com/DrDaydream/Orca-A.git "$FIVE_PROTOCOL_DIST"
  fi
  cd "$FIVE_PROTOCOL_DIST"
  sha256sum -c SHA256SUMS
  cp five-protocol-tools-v3.2.zip /home/ubuntu/experiments/
)
```

确认旧批处理已退出、实例清理已完成，再安装。下面只备份和替换 figure-tools，不改动结果目录：

```bash
python3 - <<'PY'
import fcntl
from datetime import datetime, timezone
from pathlib import Path
import tempfile
import zipfile

base = Path('/home/ubuntu/experiments')
with open('/tmp/five-protocol-figure-matrix.lock', 'a') as lock:
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        raise SystemExit('批处理正在运行，请在其退出后更新。')
    with zipfile.ZipFile(base / 'five-protocol-tools-v3.2.zip') as archive:
        if archive.testzip() is not None:
            raise SystemExit('ZIP 校验失败。')
        stage = Path(tempfile.mkdtemp(prefix='figure-tools-update-', dir=base))
        archive.extractall(stage)
    fresh = stage / 'five-protocol-tools'
    for name in ('fabfile.py', 'run_matrix.py', 'lifecycle.py', 'protocol_tasks.py', 'plot_figures.py'):
        if not (fresh / name).is_file():
            raise SystemExit('工具包缺少 ' + name)
    target = base / 'figure-tools'
    if target.exists():
        stamp = datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S.%fZ')
        target.rename(base / ('figure-tools-backup-' + stamp))
    fresh.rename(target)
    stage.rmdir()
    print('v3.2 已安装到', target)
PY
```

继续原来的结果目录：

```bash
fab -r /home/ubuntu/experiments/figure-tools matrix \
  --output /home/ubuntu/experiments/paper-results-v2-fix1 \
  --upgrade-pool-policy --retry-failed
```

旧实验协议版本、设置、计划、已完成归档哈希必须通过检查；失败/中断组重试，已完成组跳过。实例仍按协议 × 节点数串行创建，每批 create → install → 多次 remote → destroy。使用已有 Screen 会话运行这条续跑命令，即可之后重新连接查看动态输出。

本次验证：Orca-A 70 项、Orca-B 55 项 Python 测试通过；工具包 16 项资源池/迁移测试和 9 项归档/指标/绘图测试通过。未在用户 EC2 上启动实验。
