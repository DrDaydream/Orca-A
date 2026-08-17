# Orca-A：阿里云四服务器部署（零基础版）

这份说明从购买/配置阿里云 ECS 开始。最终效果是：四台 ECS 各运行一个 Orca-A 节点，管理机运行脚本，测试 20 秒或 60 秒后自动停止并输出 TPS 和延迟。

> 先不要同时部署 Orca-A 和 Orca-B。确认 Orca-A 跑通后，再复制同样流程部署 Orca-B。

## 一、先准备这张表

打开阿里云控制台，在每台 ECS 的“实例详情”中记录公网 IP 和私网 IP：

| 编号 | 公网 IP（SSH 使用） | 私网 IP（节点通信使用） |
|---|---|---|
| 0 | `请填写` | `请填写` |
| 1 | `请填写` | `请填写` |
| 2 | `请填写` | `请填写` |
| 3 | `请填写` | `请填写` |

四台机器必须在同一个 VPC，最好在同一个可用区。下面假设系统为 Ubuntu 22.04/24.04，登录用户为 `root`。如果实际用户是 `ubuntu`，把所有 `root@IP` 和 `/root/Orca-A` 改成 `ubuntu@IP` 和 `/home/ubuntu/Orca-A`。

## 二、在阿里云网页中配置安全组

1. 登录 [阿里云控制台](https://ecs.console.aliyun.com/)。
2. 点击左侧“实例与镜像” → “实例”。
3. 在顶部选择四台服务器所在地域。
4. 点击任意一台实例名称 → “安全组” → 点击安全组名称。
5. 点击“入方向” → “手动添加”。
6. 添加 SSH 规则：协议选 `TCP`，端口填 `22`，源填你自己电脑的公网 IP（临时测试也可填 `0.0.0.0/0`，但安全性较低）。
7. 再添加集群规则：协议选 `TCP`，端口范围填 `3000/3004`，源选择四台 ECS 所在 VPC 的私网网段，例如 `172.16.0.0/16`。
8. 确认四台 ECS 都绑定了这个安全组。

不要把 3000–3004 长期开放给 `0.0.0.0/0`。

### 2.1 确认四台机器处于同一个 VPC

1. 在 ECS 控制台点击左侧“实例与镜像” → “实例”。
2. 依次点击四台实例的名称，查看“网络信息”。
3. 记录“专有网络 VPC”“交换机”和“私网 IP”。
4. 四台机器的 VPC ID（形如 `vpc-xxxx`）必须相同。交换机可以不同，但同一个交换机最省事。

如果 VPC ID 不同，私网 IP 默认不能直接互通。对于第一次部署，最简单可靠的办法是重新创建/迁移实例，让四台都选择同一个 VPC，而不是使用公网 IP 跑共识。已有生产实例不能迁移时，需要另行配置云企业网 CEN 或 VPN，这不属于本教程的简单四节点方案。

### 2.2 推荐：给四台机器绑定同一个普通安全组

1. ECS 实例列表勾选四台实例。
2. 点击列表下方或“更多”中的“网络和安全组” → “加入安全组”。
3. 选择同一个“普通安全组”。如果只能逐台操作，就在每台实例详情的“安全组”页面分别加入同一个安全组。
4. 进入“网络与安全” → “安全组”，点击这个安全组 ID。
5. 如果页面有“组内连通策略”，选择“组内互通”。

普通安全组通常允许组内私网互通，但这里仍建议显式添加 3000–3004 规则，以免安全组类型或默认策略不同导致测试失败。

### 2.3 精确添加入方向规则

进入安全组详情页，点击“访问规则”或“安全组规则” → “入方向” → “手动添加/增加规则”，填写：

| 授权策略 | 优先级 | 协议 | 端口范围 | 授权对象/源 | 说明 |
|---|---:|---|---|---|---|
| 允许 | 1 | 自定义 TCP | `3000/3004` | 四台所在私网 CIDR，例如 `172.16.0.0/16` | Orca 集群通信 |
| 允许 | 1 | SSH / TCP | `22/22` | 你自己电脑的公网 IP `/32` | 管理服务器 |

端口含义：

| 端口 | 谁监听 | 用途 |
|---:|---|---|
| 3000 | Primary | Primary ↔ Primary |
| 3001 | Primary | Worker → Primary |
| 3002 | Worker | Primary → Worker |
| 3003 | Worker | benchmark_client → Worker 交易入口 |
| 3004 | Worker | Worker ↔ Worker |

如果四台机器属于不同安全组，有两种配置方法：

- 普通安全组：在每个目标安全组的入方向添加规则，把另外几个安全组 ID 设为授权对象；
- 通用方法：在每个安全组的入方向允许整个集群私网 CIDR 对 TCP 3000–3004 的访问。

不要在“授权对象”中填写四台机器的公网 IP。集群通信使用私网 IP。

### 2.4 检查出方向规则

点击安全组的“出方向”：

- 普通安全组默认通常允许全部出方向，不需要增加规则；
- 企业级安全组或采用严格白名单时，增加一条“允许、自定义 TCP、目的端口 `3000/3004`、目的地址为集群私网 CIDR”的出方向规则；
- 如果存在高优先级的“拒绝全部”规则，允许规则的优先级数字必须更小，例如允许规则优先级 `1`、拒绝规则优先级 `100`。

阿里云安全组是有状态的：已允许发起并建立的连接，其响应流量会自动放行。可参考阿里云官方的[使用安全组](https://help.aliyun.com/zh/ecs/user-guide/start-using-security-groups)和[安全组规则说明](https://help.aliyun.com/zh/ecs/user-guide/security-group-rules)。

### 2.5 配置服务器系统内的 UFW

阿里云安全组是第一层防火墙，Ubuntu 的 UFW 是第二层。四台分别执行：

```bash
sudo ufw status verbose
```

如果输出 `Status: inactive`，系统防火墙没有拦截，无需修改。如果输出 `Status: active`，把 `172.16.0.0/16` 替换成实际 VPC CIDR，然后执行：

```bash
sudo ufw allow from 172.16.0.0/16 to any port 3000:3004 proto tcp
sudo ufw allow 22/tcp
sudo ufw reload
sudo ufw status numbered
```

不要在未确认 22 端口已放行前启用 UFW，否则可能把自己的 SSH 连接锁在服务器外。

### 2.6 网络测试分为两个阶段

第一阶段是不启动 Orca 时测试基础私网。在 Node 0 上把 IP 换成另外三台的私网 IP：

```bash
ping -c 2 NODE1_PRIVATE_IP
ping -c 2 NODE2_PRIVATE_IP
ping -c 2 NODE3_PRIVATE_IP
ssh -o ConnectTimeout=5 root@NODE1_PRIVATE_IP hostname
ssh -o ConnectTimeout=5 root@NODE2_PRIVATE_IP hostname
ssh -o ConnectTimeout=5 root@NODE3_PRIVATE_IP hostname
```

`ping` 可能被 ICMP 规则禁止，因此关键判断是私网 SSH 能否连接。第二阶段是在 Orca 的 Worker/Primary 启动后测试业务端口：

```bash
nc -vz -w 3 NODE1_PRIVATE_IP 3000
nc -vz -w 3 NODE1_PRIVATE_IP 3003
nc -vz -w 3 NODE1_PRIVATE_IP 3004
```

`succeeded` 表示连通；`Connection refused` 表示网络已经到达，但对应 Orca 进程没有监听；`timed out` 通常表示安全组、UFW、VPC 或 IP 配置错误。自动脚本后面会替你启动服务并重复检查。

## 三、连接服务器

最简单的网页方式：

1. 阿里云 ECS 实例列表中，点击 Node 0 右侧“远程连接”。
2. 选择“Workbench 远程连接”。
3. 输入服务器账号 `root` 和密码，点击“登录”。
4. 登录后页面中间的黑色区域就是终端。命令要一段一段复制进去，然后按 Enter。
5. 对 Node 1、Node 2、Node 3 分别再打开一个浏览器标签页。

以后看到“在四台服务器执行”，就是在四个黑色终端中各粘贴一次。

## 四、四台服务器安装依赖

在四台服务器分别执行：

```bash
sudo apt update
sudo apt install -y build-essential cmake clang-14 libclang-14-dev git curl tmux jq python3 python3-pip netcat-openbsd chrony
sudo systemctl enable --now chrony
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup default stable
```

检查（四台都应输出版本号）：

```bash
rustc --version
cargo --version
python3 --version
```

## 五、四台服务器下载并编译 Orca-A

在四台服务器分别执行：

```bash
cd "$HOME"
git clone https://github.com/DrDaydream/Orca-A.git
cd Orca-A
source "$HOME/.cargo/env"
LIBCLANG_PATH=/usr/lib/llvm-14/lib CLANG_PATH=/usr/bin/clang-14 CC=/usr/bin/clang-14 CXX=/usr/bin/clang++-14 CXXFLAGS='-include cstdint' cargo build --release --features benchmark
```

编译可能需要十几分钟。最后检查：

```bash
ls -lh target/release/node target/release/benchmark_client
```

两个文件都存在才继续。如果仓库已经下载过，使用下面命令更新，不要再次 `git clone`：

```bash
cd "$HOME/Orca-A"
git pull --ff-only
```

## 六、让 Node 0 能免密登录四台服务器

后续把 Node 0 当作管理机。在 Node 0 执行：

```bash
ssh-keygen -t ed25519 -C "orca-benchmark"
```

连续按三次 Enter，不要填写额外内容。然后把下面四个 IP 换成 Node 0–3 的私网 IP。Node 0 也要配置，因为脚本会通过 SSH 统一管理包括自己在内的四台机器：

```bash
ssh-copy-id root@NODE0_PRIVATE_IP
ssh-copy-id root@NODE1_PRIVATE_IP
ssh-copy-id root@NODE2_PRIVATE_IP
ssh-copy-id root@NODE3_PRIVATE_IP
```

每次按提示输入对应服务器密码。测试：

```bash
ssh -o ConnectTimeout=5 root@NODE0_PRIVATE_IP hostname
ssh -o ConnectTimeout=5 root@NODE1_PRIVATE_IP hostname
ssh -o ConnectTimeout=5 root@NODE2_PRIVATE_IP hostname
ssh -o ConnectTimeout=5 root@NODE3_PRIVATE_IP hostname
```

四个命令都应该立即返回主机名。出现 `hostname contains invalid characters` 通常是把 `root@`、引号或说明文字误填进 IP 数组；数组中只能写纯 IP，例如 `172.16.0.12`。

## 七、只在 Node 0 生成密钥和配置

在 Node 0 执行：

```bash
cd "$HOME/Orca-A"
mkdir -p deploy
./target/release/node generate_keys --filename deploy/node-0.json
./target/release/node generate_keys --filename deploy/node-1.json
./target/release/node generate_keys --filename deploy/node-2.json
./target/release/node generate_keys --filename deploy/node-3.json
chmod 600 deploy/node-*.json
```

然后输入：

```bash
nano deploy/make_config.py
```

终端会进入文本编辑器。粘贴以下内容，并把四个 `NODE...PRIVATE_IP` 换成真实私网 IP：

```python
import json
from pathlib import Path

ips = [
    "NODE0_PRIVATE_IP",
    "NODE1_PRIVATE_IP",
    "NODE2_PRIVATE_IP",
    "NODE3_PRIVATE_IP",
]

authorities = {}
for index, ip in enumerate(ips):
    key = json.loads(Path(f"deploy/node-{index}.json").read_text())
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
```

保存方法：按 `Ctrl+O`，按 Enter，再按 `Ctrl+X`。然后运行：

```bash
cd "$HOME/Orca-A"
python3 deploy/make_config.py
jq . deploy/committee.json
```

输出中不应存在 `NODE0_PRIVATE_IP` 之类的占位文字。

## 八、从 Node 0 分发配置

把三个 IP 换成 Node 1–3 的私网 IP，在 Node 0 执行：

```bash
ssh root@NODE1_PRIVATE_IP 'mkdir -p /root/Orca-A/deploy'
ssh root@NODE2_PRIVATE_IP 'mkdir -p /root/Orca-A/deploy'
ssh root@NODE3_PRIVATE_IP 'mkdir -p /root/Orca-A/deploy'

scp deploy/node-1.json deploy/committee.json deploy/parameters.json root@NODE1_PRIVATE_IP:/root/Orca-A/deploy/
scp deploy/node-2.json deploy/committee.json deploy/parameters.json root@NODE2_PRIVATE_IP:/root/Orca-A/deploy/
scp deploy/node-3.json deploy/committee.json deploy/parameters.json root@NODE3_PRIVATE_IP:/root/Orca-A/deploy/
```

检查四台配置是否完全一致。先在 Node 0 执行：

```bash
sha256sum deploy/committee.json deploy/parameters.json
```

然后在 Node 1–3 各执行同一命令。四台输出的哈希必须相同。

## 九、检查四台之间的网络

先不要运行基准测试。在每台服务器上对其他三个私网 IP 执行：

```bash
ping -c 2 NODE1_PRIVATE_IP
```

安全组可能禁止 ping；ping 不通不一定有问题。真正的端口测试要等节点启动后进行。系统防火墙若已启用，在四台分别执行（把网段改成真实私网网段）：

```bash
sudo ufw allow from 172.16.0.0/16 to any port 3000:3004 proto tcp
```

## 十、创建会显示进度的自动测试脚本

先在 Node 0 安装结果解析所需的 Python 包：

```bash
cd "$HOME/Orca-A"
python3 -m pip install --break-system-packages -r benchmark/requirements.txt
```

如果系统没有提示 `externally-managed-environment`，也可以去掉 `--break-system-packages`。然后在 Node 0 执行：

```bash
cd "$HOME/Orca-A"
nano run-four-servers.sh
```

粘贴以下脚本。只修改 `IPS` 中的四个私网 IP；不要写 `root@`，不要写端口，不要使用中文引号：

```bash
#!/usr/bin/env bash
set -Eeuo pipefail

DURATION="${1:-20}"
TOTAL_RATE="${2:-10000}"
TX_SIZE=512
REMOTE_USER="root"
REMOTE_DIR="/root/Orca-A"
IPS=("NODE0_PRIVATE_IP" "NODE1_PRIVATE_IP" "NODE2_PRIVATE_IP" "NODE3_PRIVATE_IP")
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=6 -o ServerAliveInterval=5 -o ServerAliveCountMax=2)
LOCAL_LOGS="benchmark/logs"
RATE_SHARE=$(((TOTAL_RATE + 3) / 4))
TX_NODES="${IPS[0]}:3003 ${IPS[1]}:3003 ${IPS[2]}:3003 ${IPS[3]}:3003"

remote() { ssh "${SSH_OPTS[@]}" "${REMOTE_USER}@$1" "$2"; }
stop_all() {
  echo "[清理] 停止四台服务器上的测试进程……"
  for ip in "${IPS[@]}"; do
    remote "$ip" "tmux kill-session -t orca-client 2>/dev/null || true; tmux kill-session -t orca-primary 2>/dev/null || true; tmux kill-session -t orca-worker 2>/dev/null || true" &
  done
  wait || true
}
trap stop_all EXIT INT TERM

echo "[1/8] 检查 SSH……"
for ip in "${IPS[@]}"; do remote "$ip" "test -x '${REMOTE_DIR}/target/release/node' && test -x '${REMOTE_DIR}/target/release/benchmark_client'"; echo "  $ip OK"; done

echo "[2/8] 清理上次测试（只删除临时测试数据库和日志）……"
for ip in "${IPS[@]}"; do remote "$ip" "tmux kill-session -t orca-client 2>/dev/null || true; tmux kill-session -t orca-primary 2>/dev/null || true; tmux kill-session -t orca-worker 2>/dev/null || true; cd '${REMOTE_DIR}'; rm -rf run/db-primary run/db-worker run/logs; mkdir -p run/logs" & done
wait

echo "[3/8] 启动 Worker……"
for i in 0 1 2 3; do remote "${IPS[$i]}" "cd '${REMOTE_DIR}' && tmux new-session -d -s orca-worker \"RUST_LOG=info ./target/release/node -vv run --keys deploy/node-${i}.json --committee deploy/committee.json --parameters deploy/parameters.json --store run/db-worker worker --id 0 |& tee run/logs/worker-${i}-0.log\"" & done
wait

echo "[4/8] 启动 Primary……"
for i in 0 1 2 3; do remote "${IPS[$i]}" "cd '${REMOTE_DIR}' && tmux new-session -d -s orca-primary \"RUST_LOG=info ./target/release/node -vv run --keys deploy/node-${i}.json --committee deploy/committee.json --parameters deploy/parameters.json --store run/db-primary primary |& tee run/logs/primary-${i}.log\"" & done
wait
sleep 4

echo "[5/8] 检查 3003 端口并启动 Client……"
for i in 0 1 2 3; do
  if ! remote "${IPS[$i]}" "ss -ltn | grep -q ':3003 '"; then
    echo "错误：节点 $i (${IPS[$i]}) 没有监听 3003。Worker 日志如下："
    remote "${IPS[$i]}" "tail -80 '${REMOTE_DIR}/run/logs/worker-${i}-0.log'" || true
    exit 1
  fi
done
for i in 0 1 2 3; do remote "${IPS[$i]}" "cd '${REMOTE_DIR}' && tmux new-session -d -s orca-client \"RUST_LOG=info ./target/release/benchmark_client '${IPS[$i]}:3003' --size '${TX_SIZE}' --rate '${RATE_SHARE}' --nodes ${TX_NODES} |& tee run/logs/client-${i}-0.log\"" & done
wait

echo "[等待] Client 必须连通全部四个 3003 端口；每 2 秒显示一次进度……"
READY_TIMEOUT=120
ready=0
for ((elapsed=0; elapsed<READY_TIMEOUT; elapsed+=2)); do
  ready=0
  line=""
  for i in 0 1 2 3; do
    if remote "${IPS[$i]}" "grep -q 'Start sending transactions' '${REMOTE_DIR}/run/logs/client-${i}-0.log'"; then ready=$((ready + 1)); line+=" node${i}=ready"; else line+=" node${i}=waiting"; fi
  done
  echo "  ${elapsed}s: ready=${ready}/4;${line}"
  [ "$ready" -eq 4 ] && break
  sleep 2
done
if [ "$ready" -ne 4 ]; then
  echo "错误：120 秒内客户端没有全部就绪。以下是客户端和 Worker 日志："
  for i in 0 1 2 3; do echo "===== node $i client ====="; remote "${IPS[$i]}" "tail -60 '${REMOTE_DIR}/run/logs/client-${i}-0.log'" || true; echo "===== node $i worker ====="; remote "${IPS[$i]}" "tail -60 '${REMOTE_DIR}/run/logs/worker-${i}-0.log'" || true; done
  exit 1
fi

echo "[6/8] 全部就绪，正式运行 ${DURATION} 秒，总输入速率 ${TOTAL_RATE} TPS……"
for ((left=DURATION; left>0; left--)); do printf '\r  剩余 %3d 秒' "$left"; sleep 1; done; echo

echo "[7/8] 停止进程并下载日志……"
stop_all
trap - EXIT INT TERM
rm -rf "$LOCAL_LOGS"; mkdir -p "$LOCAL_LOGS"
for i in 0 1 2 3; do
  scp "${SSH_OPTS[@]}" "${REMOTE_USER}@${IPS[$i]}:${REMOTE_DIR}/run/logs/primary-${i}.log" "$LOCAL_LOGS/"
  scp "${SSH_OPTS[@]}" "${REMOTE_USER}@${IPS[$i]}:${REMOTE_DIR}/run/logs/worker-${i}-0.log" "$LOCAL_LOGS/"
  scp "${SSH_OPTS[@]}" "${REMOTE_USER}@${IPS[$i]}:${REMOTE_DIR}/run/logs/client-${i}-0.log" "$LOCAL_LOGS/"
done

echo "[8/8] 解析结果……"
cd benchmark
python3 - <<'PY'
from benchmark.logs import LogParser
print(LogParser.process("logs", faults=0).result())
PY
```

保存：按 `Ctrl+O` → Enter → `Ctrl+X`。授权：

```bash
chmod +x run-four-servers.sh
```

检查占位符是否全部替换：

```bash
grep -n 'NODE.*PRIVATE_IP' run-four-servers.sh deploy/committee.json
```

正确情况下这条命令没有任何输出。

## 十一、第一次运行

先使用较低输入速率，运行 20 秒：

```bash
cd "$HOME/Orca-A"
./run-four-servers.sh 20 10000
```

脚本应依次输出 `[1/8]` 到 `[8/8]`，等待阶段每两秒输出 `ready=x/4`。成功后会显示 TPS 和 latency。再测试 60 秒：

```bash
./run-four-servers.sh 60 10000
```

确认稳定后再提高速率，例如：

```bash
./run-four-servers.sh 20 50000
```

## 十二、如果脚本“没有输出、一直运行”

首先按 `Ctrl+C` 停止，然后用调试方式运行：

```bash
cd "$HOME/Orca-A"
bash -x ./run-four-servers.sh 20 10000 2>&1 | tee run-script-debug.log
```

判断停在哪里：

- 卡在 `[1/8]`：SSH 地址、账号、免密登录或安全组 22 端口有问题。
- 卡在 `[3/8]` 或 `[4/8]`：远程二进制、密钥、配置或节点启动失败。
- 一直显示 `ready=0/4`：Client 正在等待四个 Worker 的 3003 端口，通常是私网 IP、安全组或 Worker 崩溃。
- 已运行完却报 `NoneType has no attribute group`：至少一个客户端日志没有 `Start sending transactions`，不能解析未真正开始的测试。

在 Node 0 查看四台远程状态：

```bash
for ip in NODE0_PRIVATE_IP NODE1_PRIVATE_IP NODE2_PRIVATE_IP NODE3_PRIVATE_IP; do echo "===== $ip ====="; ssh root@$ip 'tmux ls; ss -ltnp | grep -E ":300[0-4] " || true; pgrep -af target/release/node || true'; done
```

查看某个节点的日志（把 IP 和编号改对）：

```bash
ssh root@NODE1_PRIVATE_IP 'tail -100 /root/Orca-A/run/logs/worker-1-0.log'
ssh root@NODE1_PRIVATE_IP 'tail -100 /root/Orca-A/run/logs/primary-1.log'
ssh root@NODE1_PRIVATE_IP 'tail -100 /root/Orca-A/run/logs/client-1-0.log'
```

从 Node 0 测试全部交易端口：

```bash
nc -vz -w 3 NODE0_PRIVATE_IP 3003
nc -vz -w 3 NODE1_PRIVATE_IP 3003
nc -vz -w 3 NODE2_PRIVATE_IP 3003
nc -vz -w 3 NODE3_PRIVATE_IP 3003
```

必须四个都显示 `succeeded`。若失败：检查阿里云安全组 3000–3004、Ubuntu 的 UFW、`committee.json` 私网 IP，以及对应 Worker 日志。

## 十三、日志在哪里

远程原始日志在每台服务器：

```text
/root/Orca-A/run/logs/
```

下载后的完整日志在 Node 0：

```text
/root/Orca-A/benchmark/logs/
```

运行前删除的是 `run/db-primary`、`run/db-worker` 和旧测试日志，它们是临时基准测试数据。不要在需要保留正式数据的环境使用该清理脚本。

## 十四、部署 Orca-B

Orca-A 完全跑通后，部署 Orca-B 时重复本文流程，并把命令中的：

```text
Orca-A → Orca-B
https://github.com/DrDaydream/Orca-A.git → Orca-B 的 GitHub 仓库地址
```

Orca-A 与 Orca-B 不要同时跑；否则 CPU、磁盘和网络互相竞争，TPS 与延迟没有可比性。两者对比时使用相同四台 ECS、相同参数、相同输入速率和相同测试时长。

## 十五、运行 10、20 或 50 个节点

项目根目录现在提供了 `run-multi-servers.sh`。这个版本假设“一台 ECS 运行一个节点”，Node 0 仍同时作为控制机。以 10 节点为例：

```bash
cd /root/Orca-A
cp deploy/hosts-10.txt.example deploy/hosts-10.txt
nano deploy/hosts-10.txt
```

每行填写一个私网 IP，第一行必须是控制机 Node 0，共 10 行。20 和 50 节点分别创建：

```text
deploy/hosts-20.txt（20 行）
deploy/hosts-50.txt（50 行）
```

对应运行命令：

```bash
./run-multi-servers.sh 10 20 10000
./run-multi-servers.sh 20 20 10000
./run-multi-servers.sh 50 20 10000
```

三个参数依次是节点数、测试秒数和全体节点合计的输入 TPS。脚本会自动把总 TPS 平分给所有客户端。

运行前必须已经完成以下准备：

1. 每台服务器已经在相同路径 `/root/Orca-A` 编译项目；
2. 控制机能够通过私网 IP 免密 SSH 登录包括自己在内的全部服务器；
3. `committee.json` 中包含本次运行的全部 10、20 或 50 个节点；
4. 第 `i` 台服务器具有匹配的 `deploy/node-i.json`；
5. 所有服务器的 `committee.json` 和 `parameters.json` 完全相同；
6. 安全组和 UFW 允许集群私网网段访问 TCP 3000–3004。

四节点的 `committee.json` 不能直接用于 10/20/50 节点。切换节点规模时必须重新生成相应数量的节点密钥和委员会配置，再分发到对应服务器。
