# Orca-A 在 AWS EC2 部署 10 / 20 / 50 节点（单文件完整教程）

本文只依赖当前这一份 README。所有需要创建的配置文件和脚本都完整放在本文中。

最终架构：每台 EC2 运行一个 Primary、一个 Worker 和一个 benchmark client；Node 0 同时作为控制机，负责通过私网 SSH 管理所有节点、收集日志和输出 TPS/延迟。

> 重要：50 台 EC2 会产生明显费用，公网 IPv4、EBS 和跨可用区流量也可能收费。第一次务必先用 10 台、20 秒、10,000 TPS 跑通，并在测试后停止或终止实例。先在 AWS 的 Service Quotas 检查按需实例 vCPU 配额。

## 当前代码版本与本次优化

本文适用于包含提交 `d446a42` 或更新版本的 Orca-A。该版本已包含：

- VDag 使用依赖计数和事件队列晋级，不再反复扫描整个 VDag；
- READY 使用固定大小验签池与无界工作队列；
- pending leader 和缺失因果历史采用定点唤醒；
- 生产节点以完整的有序 `Vec<Certificate>` 批量交给应用层，Cleanup 也批量发送。

这些优化不改变命令行参数、端口或日志解析格式，因此旧的 AWS committee 和 benchmark 命令仍然可用。但所有服务器必须运行同一次编译产生的版本，不能混用优化前后的二进制。

已经部署过 Orca-A 的服务器可在停止整个集群后统一更新：

```bash
cd /home/ubuntu/Orca-A
git pull --ff-only origin main
git rev-parse --short HEAD
source "$HOME/.cargo/env"
LIBCLANG_PATH=/usr/lib/llvm-14/lib \
CLANG_PATH=/usr/bin/clang-14 \
CC=/usr/bin/clang-14 \
CXX=/usr/bin/clang++-14 \
CXXFLAGS='-include cstdint' \
cargo build --release --features benchmark
```

`git rev-parse` 应显示 `d446a42` 或更新的提交。

## 0. 本教程统一使用的约定

- 系统：Ubuntu Server 24.04 LTS（22.04 也可以）；
- 登录用户：`ubuntu`；
- 项目路径：`/home/ubuntu/Orca-A`；
- Node 0：既是第 0 个共识节点，也是控制机；
- 所有节点在同一个 Region、同一个 VPC，建议同一个 Availability Zone；
- SSH 可以走公网；Orca 通信必须走 Private IPv4；
- 一个 EC2 只运行一个 Orca 节点；
- 示例仓库：`https://github.com/DrDaydream/Orca-A.git`。

需要部署 Orca-B 时，在全文中把 `Orca-A` 和仓库地址替换成 Orca-B，其他流程相同。

## 1. 在 AWS 控制台创建安全组

1. 打开 [AWS Management Console](https://console.aws.amazon.com/)。
2. 右上角选择 Region，例如 `Europe (London) eu-west-2`。以后所有操作都保持同一个 Region。
3. 在顶部搜索框输入 `EC2`，点击“EC2”。
4. 左侧点击 `Network & Security` → `Security Groups`。
5. 点击右上角 `Create security group`。
6. `Security group name` 输入 `orca-cluster-sg`。
7. `Description` 输入 `Orca benchmark cluster`。
8. `VPC` 选择准备运行全部实例的同一个 VPC。
9. 在 `Inbound rules` 点击 `Add rule`，添加第一条：
   - Type：`SSH`
   - Protocol：TCP
   - Port：22
   - Source：`My IP`
   - Description：`SSH from my computer`
10. 再添加第二条：
   - Type：`SSH`
   - Port：22
   - Source：在搜索框选择刚创建的安全组自身 `orca-cluster-sg`
   - Description：`SSH between cluster nodes`
11. 再添加第三条：
   - Type：`Custom TCP`
   - Port range：`3000-3004`
   - Source：选择安全组自身 `orca-cluster-sg`
   - Description：`Orca private cluster traffic`
12. `Outbound rules` 保留默认 `All traffic / 0.0.0.0/0`，以便安装软件和下载代码。
13. 点击 `Create security group`。

安全组最终应包含：

| 方向 | 类型 | 端口 | 来源/目的 |
|---|---|---:|---|
| Inbound | SSH | 22 | 你电脑的公网 IP `/32` |
| Inbound | SSH | 22 | `orca-cluster-sg` 自身 |
| Inbound | Custom TCP | 3000–3004 | `orca-cluster-sg` 自身 |
| Outbound | All traffic | All | `0.0.0.0/0` |

安全组引用自身时，关联该安全组的实例会使用私网地址通信；不是使用公网 IP。AWS 官方说明见[安全组规则](https://docs.aws.amazon.com/vpc/latest/userguide/security-group-rules.html)。

Orca 端口：

| 端口 | 用途 |
|---:|---|
| 3000 | Primary ↔ Primary |
| 3001 | Worker → Primary |
| 3002 | Primary → Worker |
| 3003 | Client → Worker 交易入口 |
| 3004 | Worker ↔ Worker |

不要把 3000–3004 的 Source 设置成 `0.0.0.0/0`。

## 2. 创建 EC2 登录密钥

1. EC2 左侧点击 `Network & Security` → `Key Pairs`。
2. 点击 `Create key pair`。
3. Name 输入 `orca-cluster-key`。
4. Key pair type 选择 `ED25519`（Windows PuTTY 用户可选 RSA）。
5. Private key file format 选择 `.pem`。
6. 点击 `Create key pair`，浏览器会下载 `orca-cluster-key.pem`。
7. 把该文件保存在安全位置。AWS 不会再次提供同一私钥下载。

Linux/macOS 本地终端执行：

```bash
chmod 400 ~/Downloads/orca-cluster-key.pem
```

Windows PowerShell 可以直接使用 Windows OpenSSH，后续把 pem 路径写成实际路径。

## 3. 创建 10、20 或 50 台 EC2

1. EC2 左侧点击 `Instances` → `Instances`。
2. 点击 `Launch instances`。
3. Name 输入 `orca-node`。
4. `Application and OS Images` 选择 `Ubuntu` → `Ubuntu Server 24.04 LTS` → 64-bit x86。
5. `Instance type`：建议至少 4 vCPU、16 GiB 内存。不要一开始选择昂贵实例；先根据预算和配额选择。
6. `Key pair (login)` 选择 `orca-cluster-key`。
7. 点击 `Network settings` 右侧 `Edit`：
   - VPC：选择创建安全组时的同一个 VPC；
   - Subnet：所有实例尽量选择同一个可用区中的同一个子网；
   - Auto-assign public IP：为了新手操作简单选择 `Enable`；
   - Firewall：选择 `Select existing security group`；
   - Common security groups：选择 `orca-cluster-sg`。
8. `Configure storage` 建议至少 30 GiB gp3。编译会占用较多空间。
9. 右侧 `Summary` 中的 `Number of instances` 输入 `10`、`20` 或 `50`。
10. 仔细查看预计实例数量，然后点击 `Launch instance`。

如果无法创建 20/50 台，通常是 On-Demand vCPU quota 不够：顶部搜索 `Service Quotas` → `AWS services` → `Amazon Elastic Compute Cloud (Amazon EC2)`，查找对应实例系列的 Running On-Demand vCPU quota 并申请提升。

等待所有实例的 `Instance state` 变成 `Running`，`Status check` 变成 `2/2 checks passed`。

## 4. 给节点编号并记录 IP

在 EC2 实例列表依次选择实例，点击铅笔图标编辑 Name：`orca-node-0`、`orca-node-1`……直到 `orca-node-N-1`。

点击每台实例，在 `Details`/`Networking` 中记录：

- `Public IPv4 address`：仅供你从电脑登录；
- `Private IPv4 address`：写入 Orca 配置和 hosts 文件。

Node 0 必须记录公网和私网 IP。其他节点至少记录私网 IP。务必按照编号排序，不能让 Node 3 的 IP 写到第 4 行。

## 5. 登录 Node 0

网页方式：实例列表选择 `orca-node-0` → 点击 `Connect` → `EC2 Instance Connect` → Username 确认为 `ubuntu` → `Connect`。Ubuntu 20.04 及更新版本通常已包含 EC2 Instance Connect。

推荐的本地终端方式：

```bash
ssh -i ~/Downloads/orca-cluster-key.pem ubuntu@NODE0_PUBLIC_IP
```

第一次提示是否信任主机时输入：

```text
yes
```

## 6. 把 AWS 私钥安全复制到 Node 0

因为 Node 0 要控制其他节点，需要让它持有这次测试专用的 pem。请在你自己电脑的终端执行，不是在 EC2 网页终端：

```bash
scp -i ~/Downloads/orca-cluster-key.pem ~/Downloads/orca-cluster-key.pem ubuntu@NODE0_PUBLIC_IP:/home/ubuntu/.ssh/orca-cluster-key.pem
ssh -i ~/Downloads/orca-cluster-key.pem ubuntu@NODE0_PUBLIC_IP 'chmod 400 /home/ubuntu/.ssh/orca-cluster-key.pem'
```

这个密钥只能由 `ubuntu` 读取，不要提交到 GitHub或聊天工具。集群销毁后同时删除该密钥。

## 7. 在 Node 0 创建私网 IP 清单

登录 Node 0 后执行：

```bash
mkdir -p /home/ubuntu/orca-deploy
nano /home/ubuntu/orca-deploy/hosts-10.txt
```

每行填写一个 Private IPv4，第一行必须是 Node 0，例如：

```text
10.0.1.10
10.0.1.11
10.0.1.12
10.0.1.13
10.0.1.14
10.0.1.15
10.0.1.16
10.0.1.17
10.0.1.18
10.0.1.19
```

保存：`Ctrl+O` → Enter → `Ctrl+X`。20/50 节点分别使用：

```text
/home/ubuntu/orca-deploy/hosts-20.txt
/home/ubuntu/orca-deploy/hosts-50.txt
```

检查行数和重复地址：

```bash
wc -l /home/ubuntu/orca-deploy/hosts-10.txt
sort /home/ubuntu/orca-deploy/hosts-10.txt | uniq -d
```

第一条应输出 10；第二条不应有输出。

## 8. 在 Node 0 配置 SSH

创建 SSH 配置：

```bash
nano /home/ubuntu/.ssh/config
```

粘贴：

```text
Host 10.*
    User ubuntu
    IdentityFile /home/ubuntu/.ssh/orca-cluster-key.pem
    StrictHostKeyChecking accept-new
    ConnectTimeout 8
    ServerAliveInterval 5
    ServerAliveCountMax 2
```

保存后执行：

```bash
chmod 600 /home/ubuntu/.ssh/config
```

测试 Node 0 到全部节点的私网 SSH：

```bash
while read -r ip; do echo "===== $ip ====="; ssh "$ip" hostname; done < /home/ubuntu/orca-deploy/hosts-10.txt
```

所有地址都必须返回主机名。若超时，检查：所有实例是否在同一个 VPC、是否绑定 `orca-cluster-sg`、安全组是否有来源为自身的 TCP 22 规则。

## 9. 创建“一键安装和编译所有节点”脚本

在 Node 0 执行：

```bash
nano /home/ubuntu/orca-deploy/install-all.sh
```

粘贴完整脚本：

```bash
#!/usr/bin/env bash
set -Eeuo pipefail

NODES="${1:?用法: $0 <10|20|50>}"
case "$NODES" in 10|20|50) ;; *) echo "只支持 10/20/50"; exit 2;; esac
HOSTS="/home/ubuntu/orca-deploy/hosts-${NODES}.txt"
[[ -f "$HOSTS" ]] || { echo "找不到 $HOSTS"; exit 1; }
mapfile -t IPS < <(awk 'NF && $1 !~ /^#/ {print $1}' "$HOSTS")
[[ "${#IPS[@]}" -eq "$NODES" ]] || { echo "IP 数量不是 $NODES"; exit 1; }

install_one() {
  local ip="$1"
  echo "[$ip] 安装和编译开始"
  ssh "$ip" 'bash -s' <<'REMOTE'
set -Eeuo pipefail
sudo apt-get update
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y build-essential cmake clang-14 libclang-14-dev git curl tmux jq python3 python3-pip netcat-openbsd chrony
sudo systemctl enable --now chrony
if [[ ! -x "$HOME/.cargo/bin/cargo" ]]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi
source "$HOME/.cargo/env"
rustup default stable
if [[ ! -d "$HOME/Orca-A/.git" ]]; then
  git clone https://github.com/DrDaydream/Orca-A.git "$HOME/Orca-A"
else
  git -C "$HOME/Orca-A" pull --ff-only
fi
cd "$HOME/Orca-A"
LIBCLANG_PATH=/usr/lib/llvm-14/lib CLANG_PATH=/usr/bin/clang-14 CC=/usr/bin/clang-14 CXX=/usr/bin/clang++-14 CXXFLAGS='-include cstdint' cargo build --release --features benchmark
test -x target/release/node
test -x target/release/benchmark_client
REMOTE
  echo "[$ip] 完成"
}

# 每批最多 5 台，避免 Node 0 和公网下载被瞬间压满。
running=0
for ip in "${IPS[@]}"; do
  install_one "$ip" &
  running=$((running + 1))
  if (( running % 5 == 0 )); then wait; fi
done
wait
echo "全部 $NODES 台安装编译完成"
```

保存并运行：

```bash
chmod +x /home/ubuntu/orca-deploy/install-all.sh
/home/ubuntu/orca-deploy/install-all.sh 10
```

20/50 节点把最后的 10 换成 20/50。RocksDB 第一次编译可能很久；脚本显示某台“完成”才表示该台成功。

## 10. 创建“生成密钥、委员会并分发”脚本

在 Node 0 执行：

```bash
nano /home/ubuntu/orca-deploy/prepare-cluster.sh
```

粘贴：

```bash
#!/usr/bin/env bash
set -Eeuo pipefail

NODES="${1:?用法: $0 <10|20|50>}"
case "$NODES" in 10|20|50) ;; *) echo "只支持 10/20/50"; exit 2;; esac
HOSTS="/home/ubuntu/orca-deploy/hosts-${NODES}.txt"
PROJECT="/home/ubuntu/Orca-A"
mapfile -t IPS < <(awk 'NF && $1 !~ /^#/ {print $1}' "$HOSTS")
[[ "${#IPS[@]}" -eq "$NODES" ]] || { echo "IP 数量不是 $NODES"; exit 1; }
cd "$PROJECT"
rm -rf deploy
mkdir -p deploy

echo "生成 $NODES 套节点密钥……"
for ((i=0; i<NODES; i++)); do
  ./target/release/node generate_keys --filename "deploy/node-${i}.json"
done
chmod 600 deploy/node-*.json

python3 - "$HOSTS" <<'PY'
import json, sys
from pathlib import Path

ips = [line.split('#', 1)[0].strip() for line in Path(sys.argv[1]).read_text().splitlines()]
ips = [x for x in ips if x]
authorities = {}
for i, ip in enumerate(ips):
    key = json.loads(Path(f"deploy/node-{i}.json").read_text())
    authorities[key["name"]] = {
        "primary": {
            "primary_to_primary": f"{ip}:3000",
            "worker_to_primary": f"{ip}:3001"
        },
        "stake": 1,
        "workers": {"0": {
            "primary_to_worker": f"{ip}:3002",
            "transactions": f"{ip}:3003",
            "worker_to_worker": f"{ip}:3004"
        }}
    }
Path("deploy/committee.json").write_text(json.dumps({"authorities": authorities}, indent=4))
parameters = {
    "batch_size": 500000,
    "gc_depth": 50,
    "header_size": 1000,
    "max_batch_delay": 200,
    "max_header_delay": 200,
    "sync_retry_delay": 10000,
    "sync_retry_nodes": 3
}
Path("deploy/parameters.json").write_text(json.dumps(parameters, indent=4))
PY

echo "向所有服务器分发各自密钥和相同公共配置……"
for ((i=0; i<NODES; i++)); do
  ip="${IPS[$i]}"
  ssh "$ip" 'mkdir -p /home/ubuntu/Orca-A/deploy'
  scp "deploy/node-${i}.json" deploy/committee.json deploy/parameters.json "$ip:/home/ubuntu/Orca-A/deploy/"
done

echo "检查委员会文件哈希……"
expected="$(sha256sum deploy/committee.json | awk '{print $1}')"
for ip in "${IPS[@]}"; do
  actual="$(ssh "$ip" "sha256sum /home/ubuntu/Orca-A/deploy/committee.json | cut -d ' ' -f1")"
  [[ "$actual" == "$expected" ]] || { echo "$ip 委员会配置不一致"; exit 1; }
done
echo "$NODES 节点配置完成，committee sha256=$expected"
```

保存并运行：

```bash
chmod +x /home/ubuntu/orca-deploy/prepare-cluster.sh
/home/ubuntu/orca-deploy/prepare-cluster.sh 10
```

每次从 10 切换到 20/50 节点，都必须重新运行对应规模的准备脚本。

## 11. 创建通用运行脚本

最新仓库已自带经过检查的 `run-multi-servers.sh`，推荐直接使用：

```bash
cd /home/ubuntu/Orca-A
chmod +x run-multi-servers.sh
HOSTS_FILE=/home/ubuntu/orca-deploy/hosts-10.txt ./run-multi-servers.sh 10 20 10000
```

脚本默认使用 `ubuntu` 用户和 `/home/ubuntu/Orca-A`。如果 AMI 使用其他用户或目录，可以显式覆盖：

```bash
REMOTE_USER=ec2-user \
REMOTE_DIR=/home/ec2-user/Orca-A \
HOSTS_FILE=/home/ec2-user/orca-deploy/hosts-10.txt \
./run-multi-servers.sh 10 20 10000
```

下面仍保留完整脚本内容，便于无法直接拉取仓库时手工创建。

在 Node 0 执行：

```bash
cd /home/ubuntu/Orca-A
nano run-multi-servers.sh
```

粘贴以下完整脚本：

```bash
#!/usr/bin/env bash
set -Eeuo pipefail

NODES="${1:-}"
DURATION="${2:-20}"
TOTAL_RATE="${3:-10000}"
case "$NODES" in 10|20|50) ;; *) echo "用法: $0 <10|20|50> [秒数] [总TPS]"; exit 2;; esac
[[ "$DURATION" =~ ^[1-9][0-9]*$ ]] || exit 2
[[ "$TOTAL_RATE" =~ ^[1-9][0-9]*$ ]] || exit 2

REMOTE_DIR="/home/ubuntu/Orca-A"
HOSTS="/home/ubuntu/orca-deploy/hosts-${NODES}.txt"
MAX_PARALLEL=10
READY_TIMEOUT=240
TX_SIZE=512
LOCAL_LOGS="benchmark/logs"
mapfile -t IPS < <(awk 'NF && $1 !~ /^#/ {print $1}' "$HOSTS")
[[ "${#IPS[@]}" -eq "$NODES" ]] || { echo "hosts 文件必须有 $NODES 个 IP"; exit 1; }
[[ "$(printf '%s\n' "${IPS[@]}" | sort -u | wc -l)" -eq "$NODES" ]] || { echo "存在重复 IP"; exit 1; }
RATE_SHARE=$(((TOTAL_RATE + NODES - 1) / NODES))
TX_NODES=""
for ip in "${IPS[@]}"; do TX_NODES+="${ip}:3003 "; done

remote() { ssh "$1" "$2"; }
wait_batch() { (( $1 % MAX_PARALLEL == 0 )) && wait || true; }
stop_all() {
  echo "[清理] 停止所有测试进程……"
  local c=0
  for ip in "${IPS[@]}"; do
    remote "$ip" "tmux kill-session -t orca-client 2>/dev/null || true; tmux kill-session -t orca-primary 2>/dev/null || true; tmux kill-session -t orca-worker 2>/dev/null || true" &
    c=$((c+1)); wait_batch "$c"
  done
  wait || true
}
trap stop_all EXIT INT TERM

echo "节点=$NODES，时长=${DURATION}s，总TPS=$TOTAL_RATE，每客户端TPS=$RATE_SHARE"
echo "[1/8] 检查 SSH、程序和配置……"
for i in "${!IPS[@]}"; do
  remote "${IPS[$i]}" "test -x '$REMOTE_DIR/target/release/node' && test -x '$REMOTE_DIR/target/release/benchmark_client' && test -f '$REMOTE_DIR/deploy/node-${i}.json'"
  echo "  node-$i ${IPS[$i]} OK"
done

echo "[2/8] 清理上次测试数据……"
c=0
for ip in "${IPS[@]}"; do
  remote "$ip" "tmux kill-session -t orca-client 2>/dev/null || true; tmux kill-session -t orca-primary 2>/dev/null || true; tmux kill-session -t orca-worker 2>/dev/null || true; cd '$REMOTE_DIR'; rm -rf run/db-primary run/db-worker run/logs; mkdir -p run/logs" &
  c=$((c+1)); wait_batch "$c"
done
wait

echo "[3/8] 启动 Worker……"
c=0
for i in "${!IPS[@]}"; do
  remote "${IPS[$i]}" "cd '$REMOTE_DIR' && tmux new-session -d -s orca-worker \"RUST_LOG=info ./target/release/node -vv run --keys deploy/node-${i}.json --committee deploy/committee.json --parameters deploy/parameters.json --store run/db-worker worker --id 0 |& tee run/logs/worker-${i}-0.log\"" &
  c=$((c+1)); wait_batch "$c"
done
wait

echo "[4/8] 启动 Primary……"
c=0
for i in "${!IPS[@]}"; do
  remote "${IPS[$i]}" "cd '$REMOTE_DIR' && tmux new-session -d -s orca-primary \"RUST_LOG=info ./target/release/node -vv run --keys deploy/node-${i}.json --committee deploy/committee.json --parameters deploy/parameters.json --store run/db-primary primary |& tee run/logs/primary-${i}.log\"" &
  c=$((c+1)); wait_batch "$c"
done
wait
sleep 6

echo "[5/8] 检查所有 Worker 的 3003 端口……"
for i in "${!IPS[@]}"; do
  if ! remote "${IPS[$i]}" "ss -ltn | grep -q ':3003 '"; then
    echo "node-$i 未监听 3003"
    remote "${IPS[$i]}" "tail -100 '$REMOTE_DIR/run/logs/worker-${i}-0.log'" || true
    exit 1
  fi
done

echo "启动 Client……"
c=0
for i in "${!IPS[@]}"; do
  remote "${IPS[$i]}" "cd '$REMOTE_DIR' && tmux new-session -d -s orca-client \"RUST_LOG=info ./target/release/benchmark_client '${IPS[$i]}:3003' --size '$TX_SIZE' --rate '$RATE_SHARE' --nodes $TX_NODES |& tee run/logs/client-${i}-0.log\"" &
  c=$((c+1)); wait_batch "$c"
done
wait

echo "[等待] 每个 Client 都要连通全部 $NODES 个 Worker……"
ready=0
for ((elapsed=0; elapsed<READY_TIMEOUT; elapsed+=3)); do
  ready=0; waiting=()
  for i in "${!IPS[@]}"; do
    if remote "${IPS[$i]}" "grep -q 'Start sending transactions' '$REMOTE_DIR/run/logs/client-${i}-0.log'"; then
      ready=$((ready+1))
    else
      waiting+=("$i")
    fi
  done
  echo "  ${elapsed}s: ready=${ready}/${NODES}; waiting=${waiting[*]:-none}"
  (( ready == NODES )) && break
  sleep 3
done
if (( ready != NODES )); then
  echo "就绪超时，只达到 $ready/$NODES"
  for i in "${waiting[@]}"; do
    echo "===== node-$i client ====="
    remote "${IPS[$i]}" "tail -80 '$REMOTE_DIR/run/logs/client-${i}-0.log'" || true
    echo "===== node-$i worker ====="
    remote "${IPS[$i]}" "tail -80 '$REMOTE_DIR/run/logs/worker-${i}-0.log'" || true
  done
  exit 1
fi

echo "[6/8] 正式运行 $DURATION 秒……"
for ((left=DURATION; left>0; left--)); do printf '\r剩余 %3d 秒' "$left"; sleep 1; done
echo
echo "[7/8] 停止并下载日志……"
stop_all
trap - EXIT INT TERM
rm -rf "$LOCAL_LOGS"; mkdir -p "$LOCAL_LOGS"
for i in "${!IPS[@]}"; do
  scp "${IPS[$i]}:$REMOTE_DIR/run/logs/primary-${i}.log" "$LOCAL_LOGS/"
  scp "${IPS[$i]}:$REMOTE_DIR/run/logs/worker-${i}-0.log" "$LOCAL_LOGS/"
  scp "${IPS[$i]}:$REMOTE_DIR/run/logs/client-${i}-0.log" "$LOCAL_LOGS/"
done

echo "[8/8] 解析结果……"
cd benchmark
python3 -m pip install --user --break-system-packages -r requirements.txt >/dev/null
python3 - <<'PY'
from benchmark.logs import LogParser
print(LogParser.process("logs", faults=0).result())
PY
```

保存并授权：

```bash
chmod +x /home/ubuntu/Orca-A/run-multi-servers.sh
```

## 12. 正式运行

10 节点、20 秒、总输入 10,000 TPS：

```bash
cd /home/ubuntu/Orca-A
./run-multi-servers.sh 10 20 10000
```

10 节点、60 秒：

```bash
./run-multi-servers.sh 10 60 10000
```

20 节点：

```bash
/home/ubuntu/orca-deploy/install-all.sh 20
/home/ubuntu/orca-deploy/prepare-cluster.sh 20
cd /home/ubuntu/Orca-A
./run-multi-servers.sh 20 20 10000
```

50 节点：

```bash
/home/ubuntu/orca-deploy/install-all.sh 50
/home/ubuntu/orca-deploy/prepare-cluster.sh 50
cd /home/ubuntu/Orca-A
./run-multi-servers.sh 50 20 10000
```

成功后会输出 Consensus TPS、End-to-end TPS 和 latency。下载后的日志位于：

```text
/home/ubuntu/Orca-A/benchmark/logs/
```

应用层批量输出是节点内部接口优化，不会把多条 benchmark 日志合并成一行；`Committed ...` 日志和最终 TPS/latency 输出格式保持不变。

### 12.1 20/50 节点资源建议

READY 验签池最多使用 4 个常驻线程；工作队列无界是为了避免 Core 在突发 READY 流量下阻塞。因此建议每台实例至少 4 vCPU，推荐 8 vCPU，并在高压测试中监控 CPU 和内存：

```bash
while read -r ip; do
  echo "===== $ip ====="
  ssh "$ip" 'free -h; nproc; ps -C node -o pid,%cpu,%mem,rss,etime,cmd'
done < /home/ubuntu/orca-deploy/hosts-20.txt
```

- 10 节点先用 10,000 总 TPS 验证；
- 20/50 节点先保持总 TPS 不变，不要直接按节点数倍增输入；
- 如果内存持续增长且提交延迟同时上升，先降低输入速率；
- 所有实例应使用同一实例类型和同一可用区，避免把硬件差异误认为协议延迟。

## 13. 为什么一直显示 ready=0/N

Client 只有在连通全部 N 个 Worker 的 3003 端口后才打印 `Start sending transactions`。任何一个 Worker 崩溃或安全组不通，所有 Client 都可能一直等待。

Node 0 检查所有 3003：

```bash
while read -r ip; do printf '%s ' "$ip"; nc -vz -w 3 "$ip" 3003; done < /home/ubuntu/orca-deploy/hosts-10.txt
```

结果含义：

- `succeeded`：端口连通；
- `Connection refused`：网络已到达，但 Worker 没有运行或已经崩溃；
- `timed out`：通常是安全组、VPC、Network ACL、UFW 或 IP 写错。

查看远程日志：

```bash
ssh NODE_PRIVATE_IP 'tail -100 /home/ubuntu/Orca-A/run/logs/worker-NODE_NUMBER-0.log'
ssh NODE_PRIVATE_IP 'tail -100 /home/ubuntu/Orca-A/run/logs/primary-NODE_NUMBER.log'
ssh NODE_PRIVATE_IP 'tail -100 /home/ubuntu/Orca-A/run/logs/client-NODE_NUMBER-0.log'
```

如果 Ubuntu 启用了 UFW，在每台执行：

```bash
sudo ufw status verbose
sudo ufw allow from YOUR_VPC_CIDR to any port 3000:3004 proto tcp
sudo ufw allow 22/tcp
sudo ufw reload
```

默认 Ubuntu EC2 通常没有启用 UFW。不要在确认 22 已放行前启用它。

## 14. `NoneType object has no attribute group`

这不是共识算法的直接错误，而是日志解析器没有在至少一份 Client 日志中找到 `Start sending transactions`。不要解析未就绪的测试。新的运行脚本会先检查全部 Client 就绪，超时则显示等待节点和对应日志。

确认日志：

```bash
grep -L 'Start sending transactions' /home/ubuntu/Orca-A/benchmark/logs/client-*.log
```

命令输出的文件就是未真正开始发送的 Client。

## 15. RocksDB / bindgen 编译错误

本教程固定使用 clang-14，并在编译时设置：

```text
LIBCLANG_PATH=/usr/lib/llvm-14/lib
CLANG_PATH=/usr/bin/clang-14
CC=/usr/bin/clang-14
CXX=/usr/bin/clang++-14
CXXFLAGS=-include cstdint
```

如果磁盘不足：

```bash
df -h
du -sh /home/ubuntu/Orca-A/target
```

需要重新编译时可以执行 `cargo clean`，但它会删除编译产物，下次运行需要完整重编译。

## 16. 查看运行状态和强制停止

Node 0 查看所有节点：

```bash
while read -r ip; do echo "===== $ip ====="; ssh "$ip" 'tmux ls 2>/dev/null || true; ss -ltnp | grep -E ":300[0-4] " || true'; done < /home/ubuntu/orca-deploy/hosts-10.txt
```

停止所有测试进程：

```bash
while read -r ip; do ssh "$ip" 'tmux kill-session -t orca-client 2>/dev/null || true; tmux kill-session -t orca-primary 2>/dev/null || true; tmux kill-session -t orca-worker 2>/dev/null || true'; done < /home/ubuntu/orca-deploy/hosts-10.txt
```

## 17. 测试完成后停止 AWS 计费资源

1. 回到 EC2 控制台 → `Instances`。
2. 勾选全部 `orca-node-*`。
3. 点击 `Instance state`。
4. 以后还需要保留磁盘和配置：选择 `Stop instance`。EBS 和公网 IPv4 等资源仍可能收费。
5. 确定不再需要：选择 `Terminate instance`。终止通常不可恢复，先下载所需日志。
6. 到 `Elastic Block Store` → `Volumes` 检查是否遗留未使用卷。
7. 到 `Network & Security` → `Elastic IPs` 检查是否存在未释放的弹性 IP。

## 18. 最安全的第一次执行顺序

```text
创建 10 台 → 配安全组 → 写 hosts-10.txt
→ 测试全部私网 SSH
→ install-all.sh 10
→ prepare-cluster.sh 10
→ run-multi-servers.sh 10 20 10000
→ 检查结果和日志
→ 再测试 60 秒或提高 TPS
→ 最后再扩到 20/50 台
```

不要第一次就创建 50 台并以很高 TPS 运行。先保证 10 节点的 SSH、3003、日志和结果解析全部正常。
