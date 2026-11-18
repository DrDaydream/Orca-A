# 工具包 v3.3：每组 60 秒，前三个协议 header_size=20 B

五个协议每组只运行一次，duration 统一为 60 秒。Orca-A、Orca-B、Bullshark 的 header_size 在工具包调用原生 remote 时统一设为 20 B；修改发生在生成和上传节点配置之前。无需手工编辑仓库 fabfile.py 或 settings.json。原生仓库中独立执行 fab remote 不经过此覆盖入口。

协议顺序仍为 Orca-A → Orca-B → Bullshark → Mysticeti → Mahi-Mahi，全部串行。每个协议依次创建 10、20、35 台实例；每个节点规模都先 install 一次，完成本规模全部配置后 destroy，再创建下一批。总共 480 组，纯测量时间为 8 小时，安装、编译、启动、归档、销毁额外计时。

新结果目录：`/home/ubuntu/experiments/paper-results-v3.3-60s-h20`。时长和 header_size 改变后，旧 100 秒结果不能当作新实验的已完成配置；旧目录和旧工具包继续保留。

## 1. 停止旧实验并清理

等旧实验结束，或在原 Screen 中按一次 Ctrl+C，等待清理完成。若需要核验旧结果目录的遗留实例，旧工具仍在时执行：

```bash
fab -r /home/ubuntu/experiments/figure-tools cleanup \
  --output /home/ubuntu/experiments/paper-results-v2-fix1
```

如果旧实验使用其它输出目录，替换为该次运行真实的 --output。不要覆盖正在运行的工具文件。

## 2. 从 GitHub 下载工具包

```bash
(
  set -e
  FIVE_PROTOCOL_DIST=/home/ubuntu/experiments/figure-tools-download-v3.3
  if [ -d "$FIVE_PROTOCOL_DIST/.git" ]; then
    git -C "$FIVE_PROTOCOL_DIST" fetch origin experiment-tools
    git -C "$FIVE_PROTOCOL_DIST" merge --ff-only FETCH_HEAD
  else
    git -c core.autocrlf=false clone --branch experiment-tools --single-branch \
      https://github.com/DrDaydream/Orca-A.git "$FIVE_PROTOCOL_DIST"
  fi
  cd "$FIVE_PROTOCOL_DIST"
  sha256sum -c SHA256SUMS
  cp five-protocol-tools-v3.3.zip /home/ubuntu/experiments/
)
```

本次只需更新工具包。如果上一版 Orca-A/B 的 A1 统计修复尚未安装，请先按 EC2_A1统计更新说明.md 将两仓库更新至 c617ac3 / 015346b。已经更新的协议仓库无需再次更新。

## 3. 备份旧工具并安装 v3.3

```bash
python3 - <<'PY'
import fcntl
import hashlib
import json
from datetime import datetime, timezone
from pathlib import Path
import tempfile
import zipfile

base = Path('/home/ubuntu/experiments')
with open('/tmp/five-protocol-figure-matrix.lock', 'a') as lock:
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        raise SystemExit('一键实验仍在运行，请等其退出并清理完成后更新。')
    with zipfile.ZipFile(base / 'five-protocol-tools-v3.3.zip') as archive:
        if archive.testzip() is not None:
            raise SystemExit('ZIP 校验失败。')
        manifest = json.loads(archive.read('five-protocol-tools/verification.json'))
        if manifest['release'] != 'v3.3-60s-header20':
            raise SystemExit('工具包版本不匹配。')
        for name, expected in manifest['sha256'].items():
            content = archive.read('five-protocol-tools/' + name)
            if hashlib.sha256(content).hexdigest() != expected:
                raise SystemExit('工具包文件校验失败：' + name)
        stage = Path(tempfile.mkdtemp(prefix='figure-tools-update-', dir=base))
        archive.extractall(stage)
    fresh = stage / 'five-protocol-tools'
    target = base / 'figure-tools'
    if target.exists():
        stamp = datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S.%fZ')
        target.rename(base / ('figure-tools-backup-' + stamp))
    fresh.rename(target)
    stage.rmdir()
    print('v3.3 已安装：每组 60 秒；Orca-A/B/Bullshark header_size=20 B。')
PY
```

这个步骤备份 figure-tools 并安装新版，不修改 paper-results 等结果目录。

## 4. 检查计划并在 Screen 中运行

先预览，无 AWS 调用：

```bash
fab -r /home/ubuntu/experiments/figure-tools matrix \
  --output /home/ubuntu/experiments/paper-results-v3.3-60s-h20 --dry-run
```

开头应显示 `60 measured seconds` 和 `header size: 20 B`，全部 remote 命令应带 `--duration 60`。header_size 由内部 protocol_tasks 入口设置，因此不会额外显示为原生 remote 不支持的 --header-size 参数。

如果已有一个空闲 Screen 会话，可继续使用它。需要新建时只执行一次：

```bash
screen -S five-protocols-v33
```

在 Screen 中启动：

```bash
fab -r /home/ubuntu/experiments/figure-tools matrix \
  --output /home/ubuntu/experiments/paper-results-v3.3-60s-h20
```

工具包自动执行每个实例池的 fab create → SSH 就绪 → fab install → 多次 fab remote → fab destroy。无需额外手动 install。此次新实验不要使用旧结果目录，也不要加 --upgrade-pool-policy。

离开但保持运行：按 Ctrl+A，松开，再按 D。重新连接查看动态输出：

```bash
screen -d -r five-protocols-v33
```

如果这次 v3.3 实验中断，等待清理完成后，在同一个新结果目录重试：

```bash
fab -r /home/ubuntu/experiments/figure-tools matrix \
  --output /home/ubuntu/experiments/paper-results-v3.3-60s-h20 --retry-failed
```

已完成的 v3.3 配置跳过，失败/中断配置保留旧尝试后重试。

## 5. 验证参数和绘图

每组的 summary.txt 会显示 `Measured duration (s): 60`。前三个协议另显示 `Header size (B): 20`。native-results 中的 parameters.json 记录实际 node_parameters.header_size 和 benchmark_parameters.duration；归档逻辑会校验它们与计划一致。

```bash
fab -r /home/ubuntu/experiments/figure-tools plot \
  --output /home/ubuntu/experiments/paper-results-v3.3-60s-h20
```

图1～图4、measurements.csv、replicas.csv 均从新目录读取数据。使用旧工具包绘制旧 100 秒结果，避免新旧计划混用。

Mysticeti/Mahi-Mahi 的敌手窗口仍按此前配置：节点初始化后第 20～80 秒。测量缩短到 60 秒后，可能在攻击结束前停止，不能保证包含恢复阶段。Orca-A/B/Bullshark 仍按协议轮次注入。

验证：39 项离线测试通过，480 条 Fabric remote 命令解析通过，前三个协议共 288 条任务在原生参数生成边界确认 header_size=20、duration=60。未在用户 EC2 上启动实验。
