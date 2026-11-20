# EC2 v3.6：gRPC、rule-ready 主图与 final 核对图

v3.6 更新 Orca-A、Orca-B 的事务提交接口，并给 Orca-A、Orca-B、Bullshark
增加独立的 batch rule-ready 打点。主图使用 release → rule-ready；另存 final
E2E、同一样本的 ready → final 等待和样本数量。

## 先单独测试 gRPC

只测试传输改动时，固定使用以下两个提交；它们仍使用原 final E2E 统计：

- Orca-A：`e3620a01a84a22fbc804a826b8f4066745ae7d34`
- Orca-B：`7e79ecb804f969443d8084c6de42828b20258196`

```bash
(
set -e
git -C /home/ubuntu/experiments/Orca-A fetch origin adversary-experiments
git -C /home/ubuntu/experiments/Orca-A switch --detach e3620a01a84a22fbc804a826b8f4066745ae7d34
git -C /home/ubuntu/experiments/Orca-B-grbc fetch origin adversary-experiments
git -C /home/ubuntu/experiments/Orca-B-grbc switch --detach 7e79ecb804f969443d8084c6de42828b20258196
)
```

若本地文件会被覆盖，Git 会停止；请保留输出和原文件，不要执行 stash pop。
本地 A0 测试（local 自动编译）：

```bash
(
set -e
cd /home/ubuntu/experiments/Orca-A/benchmark
fab local --nodes 10 --workers 1 --rate 10000 --tx-size 512 --duration 60 --header-size 20 --faults 0 --adversary A0
cd /home/ubuntu/experiments/Orca-B-grbc/benchmark
fab local --nodes 10 --workers 1 --rate 10000 --tx-size 512 --duration 60 --header-size 20 --faults 0 --adversary A0
)
```

已有 AWS 实验节点须在更新控制机后先运行一次 `fab install`，再运行 `fab remote`。
新建 testbed 仍按 `fab create → fab install → fab remote`。客户端和 worker 必须一起更新。

## 下载 v3.6 完整发行包

下面仅下载，不更新现有仓库，也不启动或停止实验。各命令块作为完整内容复制。

```bash
(
set -e
mkdir -p /home/ubuntu/experiments/figure-tools-download-v3.6
cd /home/ubuntu/experiments/figure-tools-download-v3.6
base=https://raw.githubusercontent.com/DrDaydream/Orca-A/experiment-tools
for file in five-protocol-tools-v3.6.zip release-v3.6.json update_ec2_v36.py stop_matrix_v36.py SHA256SUMS-v3.6; do
  curl --fail --location --retry 3 "$base/$file" -o "$file"
done
sha256sum -c SHA256SUMS-v3.6
)
```

这里使用专门的 v3.6 校验清单，不将 v3.5 的清单与新版文件混用。

## 查主进程、停止旧批次

查看，不发送信号：

```bash
python3 /home/ubuntu/experiments/figure-tools-download-v3.6/stop_matrix_v36.py
```

准备切换版本时再停止：

```bash
python3 /home/ubuntu/experiments/figure-tools-download-v3.6/stop_matrix_v36.py --stop
```

脚本核验当前用户的 v3.4/v3.5/v3.6 主进程，发送一次 SIGTERM，等待归档、退出和
实例清理。不会杀 Screen，也不会使用 kill -9。没有主进程则不发信号。
若列出多个主进程，需要用 `--pid` 明确选择。保留现有运行工具直至清理结束。

## 更新三个原目录和工具包

确认测试/批处理已经退出后执行：

```bash
(
set -e
python3 /home/ubuntu/experiments/figure-tools-download-v3.6/update_ec2_v36.py
git -C /home/ubuntu/experiments/Orca-A log -1 --oneline
git -C /home/ubuntu/experiments/Orca-B-grbc log -1 --oneline
git -C /home/ubuntu/experiments/narwhal-bullshark-ubuntu24 log -1 --oneline
)
```

对应完整提交由 `release-v3.6.json` 固定。三个仓库均使用已发布到
`adversary-experiments` 的版本；更新器使用 detached HEAD 固定本轮源码。
Mysticeti 和 Mahi-Mahi 沿用既有仓库版本。

更新器保留原目录、settings 和可自动合并的本地参数，先完成全部合并预检查，再
替换文件。发现冲突时停止且不写冲突标记；不会使用 `reset --hard` 或 `stash pop`。
备份在 `/home/ubuntu/experiments/update-backups/v3.6-时间戳/`。
若源码、参数或打点与发布版本不一致，工具会在创建 AWS 实例前拒绝运行。

## Screen 中串行运行

先检查 Python 依赖并打印计划；dry-run 不创建实例：

```bash
python3 -c 'import fabric, matplotlib; print("Dependencies ready")'
fab -r /home/ubuntu/experiments/figure-tools matrix --dry-run --output /home/ubuntu/experiments/paper-results-v3.6-grpc-rule-ready
```

进入一个新 Screen：

```bash
screen -S five-protocols-v36
```

然后在里面启动：

```bash
fab -r /home/ubuntu/experiments/figure-tools matrix --output /home/ubuntu/experiments/paper-results-v3.6-grpc-rule-ready
```

五个协议串行执行，每个协议依次用 10、20、35 台实例；每个实例池执行
`create → 等待 SSH → install → 多组 remote → destroy`。`remote` 不自动安装。
全计划 480 组，每组一次、60 秒，前三协议 header_size=20 B。A1 故障选择与
v3.5 相同，未添加诚实/诚实/故障交错分布的补测计划。

`Ctrl+A` 后按 `D` 离开 Screen 并保留运行；重新连接后：

```bash
screen -r five-protocols-v36
```

调查并修复失败原因后，用原 v3.6 目录续跑：

```bash
fab -r /home/ubuntu/experiments/figure-tools matrix --output /home/ubuntu/experiments/paper-results-v3.6-grpc-rule-ready --retry-failed
```

已完成的 v3.6 实验组会跳过。旧 v3.4/v3.5 日志不能直接补出独立 ready 观测，
本批使用新目录，保留原结果用于 final 核对。

## 绘图与结果

```bash
fab -r /home/ubuntu/experiments/figure-tools plot --output /home/ubuntu/experiments/paper-results-v3.6-grpc-rule-ready
```

- `figures/`：A0/A1/A3/A5 四张主图，前三协议 release → rule-ready。
- `figures-final/`：相应四张 final E2E 核对图。
- `measurements.csv`：两种延迟、配对等待、请求及实际配置 rate、来源和状态。
- `replicas.csv`：副本明细、ready/final 样本数、未完成样本等。
- 每组目录保留原始 client/worker/primary 日志、console.log、原生 SUMMARY、
  measurement_stats.json、measurement.json、summary.txt 和 SHA256.json。

ready 只排除该打点之后的等待；达到提交规则所需的回退/ABA 仍计入。它不代表
最终提交或客户端收到确认。final 与 ready 独立统计各自到达的样本，核对等待时
使用 paired_ready_latency_ms、paired_final_latency_ms、ready_to_final_ms。

当前 Bullshark 仓库实际使用 Tusk 共识。Mysticeti/Mahi-Mahi 保留原 shared-commit
指标，在两套图中均明确标注，不能称为五协议完全相同口径的 ready 比较。
完整定义见工具包 `METRICS.md`。
