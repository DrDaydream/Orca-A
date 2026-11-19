# EC2：复用 Orca-A v3.4 日志，只补跑新版 A1

本流程把 v3.4 中 Orca-A 的 A0、A3、A5 共 72 组有效结果导入 v3.5 结果目录，然后补跑新版 A1 的 24 组。随后串行运行 Orca-B、Bullshark、Mysticeti、Mahi-Mahi 的剩余实验。每组一次、60 秒；前三协议 header_size=20 B。

原结果目录为 `/home/ubuntu/experiments/paper-results-v3.4-60s-h20-a20-50`；续跑目录为 `/home/ubuntu/experiments/paper-results-v3.5-60s-h20-crash-a1`。

## 1. 在另一个 Instance Connect 页面查找主进程

保留当前实验页面，在新终端执行：

```bash
ps -u ubuntu -o pid,ppid,stat,etime,args \
  | grep -E '[r]un_matrix.py|[f]ab .*matrix'
```

主进程是命令中带 `run_matrix.py` 的那一行。Screen 进程、父层 `fab matrix`、当前组的 `protocol_tasks remote` 都不是这里要停止的主进程。

## 2. 下载新工具，暂不替换正在运行的工具目录

```bash
(
  set -e
  FIVE_PROTOCOL_DIST=/home/ubuntu/experiments/figure-tools-download-v3.5
  if [ -d "$FIVE_PROTOCOL_DIST/.git" ]; then
    git -C "$FIVE_PROTOCOL_DIST" fetch origin experiment-tools
    git -C "$FIVE_PROTOCOL_DIST" merge --ff-only FETCH_HEAD
  else
    git -c core.autocrlf=false clone --branch experiment-tools --single-branch \
      https://github.com/DrDaydream/Orca-A.git "$FIVE_PROTOCOL_DIST"
  fi
  cd "$FIVE_PROTOCOL_DIST"
  sha256sum -c SHA256SUMS
  python3 stop_matrix_v35.py
)
```

末尾命令只显示匹配的主进程 PID、结果目录和完整命令，不发送信号。

## 3. 停止当前实验，等待清理，再更新仓库和运行工具

```bash
(
  set -e
  cd /home/ubuntu/experiments/figure-tools-download-v3.5
  python3 stop_matrix_v35.py --stop
  fab -r /home/ubuntu/experiments/figure-tools cleanup \
    --output /home/ubuntu/experiments/paper-results-v3.4-60s-h20-a20-50
  fab -r /home/ubuntu/experiments/figure-tools cleanup \
    --output /home/ubuntu/experiments/paper-results-v3.5-60s-h20-crash-a1
  python3 update_ec2_v35.py --root /home/ubuntu/experiments
)
```

停止脚本识别已发布的 v3.4/v3.5 控制器，用 PID 文件描述符向确认的主进程发送一次 SIGTERM，等待退出，再运行该次结果目录的 `fab cleanup`。若正在销毁实例，先等待该阶段结束；最多等待 15 分钟。任何错误会使上面这段命令停止，后面的更新不会继续。

当前未完成的一组可能记为 `interrupted`；已完成结果保留。若没有主进程，脚本显示 `No live run_matrix.py found`，随后两条 cleanup 检查已有目录的实例记录。cleanup 只处理工具记录的实验实例池。

若发现多个主进程，脚本会列出它们并拒绝自动选择；确认目标后用 `python3 stop_matrix_v35.py --stop --pid 实际PID`。不要使用历史截图中的 PID，不要发送 `kill -9`，也不要结束 Screen 服务器来代替清理。

更新脚本仍使用既有的三个仓库目录和 `figure-tools`，保留可合并的本地参数；新日志复用功能位于分发目录的独立脚本中，v3.5 的运行脚本和 ZIP 未被改写。

## 4. 创建并进入新的 Screen

```bash
screen -S "five-protocols-v35-a1-$(date +%m%d-%H%M%S)"
```

这条命令会进入新的终端会话。下面的实验命令在这个新页面执行。

## 5. 导入旧结果，再串行运行五个协议

```bash
(
  set -e
  python3 /home/ubuntu/experiments/figure-tools-download-v3.5/reuse_orca_a_v34.py \
    --source /home/ubuntu/experiments/paper-results-v3.4-60s-h20-a20-50 \
    --output /home/ubuntu/experiments/paper-results-v3.5-60s-h20-crash-a1 \
    --tools /home/ubuntu/experiments/figure-tools

  fab -r /home/ubuntu/experiments/figure-tools matrix \
    --protocol all --mode all --retry-failed \
    --output /home/ubuntu/experiments/paper-results-v3.5-60s-h20-crash-a1
)
```

首次导入且新目录没有兼容结果时，应看到 `REUSE COMPLETE: 72 imported; 0 already complete`。重复运行会验证并跳过已完成结果。若新目录已有中断/失败的兼容实验组，保留它的归档和旧选择记录，再将有效的旧结果设为当前选中结果；不会覆盖已有成功结果。

此处 `--protocol all --mode all` 是原批量入口：它跳过已导入的 Orca-A A0/A3/A5，只补跑 Orca-A A1，然后依次继续 Orca-B → Bullshark → Mysticeti → Mahi-Mahi。已完成的 v3.5 组也会跳过；`--retry-failed` 允许重试本次主动停止产生的未完成组。

每个协议按节点数依次创建实例，生命周期仍为：

`创建恰好 10 台 → fab install → 本规模剩余组 → 销毁 → 创建 20 台 → fab install → … → 创建 35 台 → fab install → … → 销毁`

Orca-A 每个规模只剩 8 组 A1；每组 `fab remote` 不执行安装或编译。若其余四协议尚未完成任何组，则本次最多新跑 408 组，其中 Orca-A 24 组、其余四协议各 96 组；纯测量时间为 6 小时 48 分钟，安装、启动、收日志和销毁时间另计。

如果只想运行 Orca-A A1 并结束本次命令，可在导入后单独使用以下命令，替代上面的 all-protocol matrix：

```bash
fab -r /home/ubuntu/experiments/figure-tools matrix \
  --protocol Orca-A --mode A1 --retry-failed \
  --output /home/ubuntu/experiments/paper-results-v3.5-60s-h20-crash-a1
```

## 6. 暂离与恢复动态输出

按 `Ctrl+A`，松开后按 `D`，离开 Screen 而保持实验运行。重新登录后执行：

```bash
screen -ls
screen -d -r five-protocols-v35-a1
```

如果同一前缀存在多个会话，用 `screen -ls` 显示的完整 `PID.five-protocols-v35-a1-日期时间` 替换第二条命令里的会话名称。连接恢复后可看到该 Screen 中持续输出的实验终端。

## 7. 汇总、日志来源与绘图

每组仍保存 `console.log`、原始 primary/worker/client 日志、参数快照和 summary。导入组增加 `provenance.json`，保留原提交、原运行时间和源归档路径；被转换的原 metadata、measurement、summary 和校验表保存在 `original-v3.4/` 中。目标目录根部的 `reuse-v3.4-orca-a.json` 列出每个导入点的来源。

工具检查完整 SHA256 文件清单、源代码版本、节点/速率/时长/header/敌手参数，再使用已核对源代码哈希的原测量解析器，从复制的原始日志重新计算延迟并与原结果核对。源目录不写入任何文件。需要复制旧日志，因此目标磁盘须有足够空间。

校验失败会停止导入，不会把缺失结果填成零，也不会开始后面的实验。旧 A1 的故障模型已改变，因此它不会进入新图。这个导入器只接受 Orca-A A0/A3/A5，不处理其他协议的旧结果。

批量命令结束后自动更新 CSV 和四张图；也可在实验结束后执行：

```bash
fab -r /home/ubuntu/experiments/figure-tools plot \
  --output /home/ubuntu/experiments/paper-results-v3.5-60s-h20-crash-a1
```

输出位于该结果目录下的 `measurements.csv`、`replicas.csv`、`figures/figure1_A0.*`、`figure2_A1.*`、`figure3_A3.*`、`figure4_A5.*`。CSV 中每个实验点的 controller_commit 对应实际运行时的原提交。
