# v3.5：A1 整场进程故障，Orca-B 仅 A5 延迟 ABA

执行这些 EC2 命令前，需先完成本版 GitHub 发布。随发布包提供的 `publish_v35.py` 会顺序推送三个协议到 `adversary-experiments`，再推送 Orca-A 的 `experiment-tools` 分发分支，并验证每个提交已在远端。脚本只进行普通快进推送；若远端分叉则停止。

本版三协议提交：Orca-A `7a93ba7e433728653fe9eee3fb714c62911ab53a`；Orca-B `d47cedd565376be499bf2f6a5e9b24bbcec290d1`；Bullshark `44d59d324e8cb097b70f11a470376f1d4d7c1149`。Mysticeti、Mahi-Mahi 保持上一版源码。

## 1. 退出旧批次并核验清理

如果旧实验还在运行，在原 Screen 中按一次 Ctrl+C，等待清理结束后再继续。已确认旧进程结束的情况，直接核验旧结果目录记录的实例池即可：

```bash
fab -r /home/ubuntu/experiments/figure-tools cleanup \
  --output /home/ubuntu/experiments/paper-results-v3.4-60s-h20-a20-50
```

若旧批次实际使用其他目录，请填该次的真实 `--output`。此命令只清理该结果目录记录的实例池；控制机和旧结果保留。更新脚本还会检查批处理锁，避免在运行中替换代码。

## 2. 下载发行工具并更新原目录

GitHub 发布成功后执行以下完整命令：

```bash
(
  set -e
  cd /home/ubuntu/experiments
  FIVE_PROTOCOL_DIST=/home/ubuntu/experiments/figure-tools-download-v3.5
  if [ -d "$FIVE_PROTOCOL_DIST/.git" ]; then
    git -C "$FIVE_PROTOCOL_DIST" fetch origin experiment-tools
    git -C "$FIVE_PROTOCOL_DIST" merge --ff-only FETCH_HEAD
  else
    git -c core.autocrlf=false clone --branch experiment-tools --single-branch \
      https://github.com/DrDaydream/Orca-A.git "$FIVE_PROTOCOL_DIST"
  fi
  cd "$FIVE_PROTOCOL_DIST"
  test -f release-v3.5.json
  sha256sum -c SHA256SUMS
  python3 update_ec2_v35.py --root /home/ubuntu/experiments
)
```

脚本使用既有目录：

- `/home/ubuntu/experiments/Orca-A`
- `/home/ubuntu/experiments/Orca-B-grbc`
- `/home/ubuntu/experiments/narwhal-bullshark-ubuntu24`
- `/home/ubuntu/experiments/figure-tools`

更新前先在临时文件中检查所有受影响的本地修改。可自动合并的 fabfile 参数及旧 `join` 修补会保留；遇到无法自动协调的同位置修改，预检查会退出，原文件保持原样，不会写入冲突标记或建立 Git 合并状态。已有未解决合并、已暂存修改或本地独有提交也会在切换前停止。源码切换到上述确切提交；本地配置和结果保留，settings 中仅在必要时将 `repo.branch` 设为 `adversary-experiments`。

备份保存在 `/home/ubuntu/experiments/update-backups/v3.5-<时间>/`，包括本地修改、已有 settings 和旧工具。可以先执行 `python3 update_ec2_v35.py --check-only`，只下载、校验和检查，不切换仓库或工具。

## 3. 检查计划，进入 Screen，运行全部协议

先查看计划，此步不访问 AWS：

```bash
fab -r /home/ubuntu/experiments/figure-tools matrix --dry-run \
  --output /home/ubuntu/experiments/paper-results-v3.5-60s-h20-crash-a1
```

创建新的 Screen 会话：

```bash
screen -S five-protocols-v35
```

在打开的 Screen 中执行：

```bash
fab -r /home/ubuntu/experiments/figure-tools matrix \
  --output /home/ubuntu/experiments/paper-results-v3.5-60s-h20-crash-a1
```

每一批自动执行：`fab create → SSH 就绪检查 → fab install → 多次 fab remote → fab destroy`。remote 不安装、不编译。五协议严格串行，顺序为 Orca-A → Orca-B → Bullshark → Mysticeti → Mahi-Mahi。每个协议依次创建恰好 10、20、35 台实验实例，完成该规模的 32 组后销毁，再创建下一批。

总计 480 组，每组一次、60 秒；纯测量时间 8 小时，安装、启动、归档和销毁另计。前三协议 header_size=20 B。已有 v3.4 结果留在原目录；新模型使用新的 v3.5 结果目录。

暂时离开：按 Ctrl+A，再按 D。重新连接后查看动态输出：

```bash
screen -d -r five-protocols-v35
```

同名会话有多个时，先 `screen -ls`，再用完整的 `PID.five-protocols-v35` 连接。

## 4. 失败后续跑或重绘

查明失败原因并完成修复后，使用同一个 v3.5 结果目录续跑，已完成的新配置会跳过：

```bash
fab -r /home/ubuntu/experiments/figure-tools matrix --retry-failed \
  --output /home/ubuntu/experiments/paper-results-v3.5-60s-h20-crash-a1
```

只重绘已有结果，不创建实例：

```bash
fab -r /home/ubuntu/experiments/figure-tools plot \
  --output /home/ubuntu/experiments/paper-results-v3.5-60s-h20-crash-a1
```

## 本版敌手与横坐标

Orca-A/B/Bullshark 的 A1 不启动选中身份的 primary、全部 worker 和客户端，委员会仍保留完整 n 个身份。`--nodes 10 --adversary A1 --byzantine 3 --faults 0` 默认使 7、8、9 三个身份全程故障。A1 不使用临时秒级窗口，要求 `start-round=1, attack-rounds=0`。Bullshark A1 关闭额外交替 leader 注入；A3/A5 保留严格交替 leader slot 不可用。

Orca-B 的 A3 正常传递 ABA，只调度 GRBC；仅 A5 可直接延迟 ABA 的 BVal、Aux、Decide 消息。缺失节点或 GRBC 延迟仍可能间接影响 ABA 启动和推进。

A1 按原 Bullshark faults 保留每个客户端的发压分摊方式，故障身份对应客户端不会补发。10 节点、3 个故障、rate=10000 时，7 个活动客户端合计配置 7000 tx/s。summary/CSV 分别保存请求 rate 和合计配置发压，图以 `offered_rate_tps / 1000` 为横坐标并标注实际配置值；它不是测量得到的入口吞吐量。Mysticeti/Mahi-Mahi 保持原来的 20～50 秒敌手窗口。

离线验证：三个协议 105 项 Python 检查通过，9 项真实 Fabric CLI 检查因缺少依赖跳过；工具包 37 项测试及更新器 3 项测试通过；Orca-B 实际调度/消息/加密/网络模块 14 项 Rust 测试通过。未运行 EC2 性能实验，完整 primary 工程编译受本机缺少 libclang 限制。
