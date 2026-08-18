# Orca-A：AWS EC2 10 / 20 / 50 节点完整部署

本文对应当前仓库的 `run-multi-servers.sh`。每台 EC2 运行一个 Primary、一个 Worker 和一个 benchmark client；node-0 同时是第 0 个节点和控制机。配置文件与协议通信全部使用 Private IPv4。

## 1. 部署约定

| 项目 | 值 |
|---|---|
| AMI | Ubuntu Server 24.04 LTS，x86_64 |
| 节点数 | 10、20 或 50 |
| 登录用户 | `root` |
| 项目目录 | `/root/Orca-A` |
| 仓库 | `https://github.com/DrDaydream/Orca-A.git` |
| 推荐实例 | 至少 4 vCPU / 16 GiB，50 节点建议 8 vCPU |
| 磁盘 | 至少 30 GiB gp3 |
| 控制机 | node-0，同时参与协议 |
| 网络 | 同一 Region、同一 VPC，建议同一 AZ |

先用 10 节点、20 秒、总输入 10,000 TPS 验证。创建 20/50 台前，在 AWS Service Quotas 检查目标实例系列的 On-Demand vCPU 配额。

本文假设服务器已经允许 `root` 使用密钥直接 SSH。AWS 官方 Ubuntu AMI 通常默认禁用 root 直登；继续部署前必须先确认 `ssh -i KEY root@PUBLIC_IP` 成功，否则应在镜像层启用 root 登录或改用服务器实际允许的账户。

协议要求 `n >= 3f+1`。建议使用：

| 节点数 | 最大建议敌手数 f |
|---:|---:|
| 10 | 3 |
| 20 | 6 |
| 50 | 16 |

## 2. AWS 控制台与安全组

1. 登录 AWS Console，选择一个 Region。
2. EC2 -> Network & Security -> Security Groups -> Create security group。
3. 名称填写 `orca-a-sg`，选择所有实例所在的 VPC。
4. 添加下列入站规则，出站保留默认 All traffic。
5. EC2 -> Key Pairs 创建 ED25519 密钥 `orca-a-aws.pem`。
6. Launch instances，选择 Ubuntu 24.04 x86_64、相同 VPC/子网/安全组，实例数量填 10、20 或 50。
7. 建议所有实例使用同一实例类型和同一 AZ；存储至少 30 GiB gp3。
8. 实例通过 2/2 status checks 后，依次命名为 `orca-a-node-0` 到 `orca-a-node-N-1`。

安全组入站规则：

| 协议/端口 | Source | 用途 |
|---|---|---|
| TCP 22 | 你的公网 IP /32 | 从本地电脑登录 |
| TCP 22 | `orca-a-sg` 自身 | node-0 通过私网管理集群 |
| TCP 3000-3004 | `orca-a-sg` 自身 | Orca-A 集群内部通信 |

不要向 `0.0.0.0/0` 开放 3000-3004。

| 端口 | 用途 |
|---:|---|
| 3000 | Primary <-> Primary |
| 3001 | Worker -> Primary |
| 3002 | Primary -> Worker |
| 3003 | Client -> Worker |
| 3004 | Worker <-> Worker |

## 2.1 五大洲跨 Region 部署

上面的安全组自身引用只适用于单 VPC 基线。五大洲实验可按 10/20/50 节点分别在 5 个 Region 放置 2/4/10 台，例如北美 `us-east-1`、南美 `sa-east-1`、欧洲 `eu-west-2`、亚洲 `ap-southeast-1`、大洋洲 `ap-southeast-2`。

每个 Region 创建不重叠的 VPC CIDR，例如 `10.10.0.0/16`、`10.20.0.0/16`、`10.30.0.0/16`、`10.40.0.0/16`、`10.50.0.0/16`。使用 AWS Cloud WAN 或 Transit Gateway inter-Region peering 连接，并在每个 VPC route table 中添加其他四个 CIDR 的双向路由。5 个 VPC 做全互联 VPC peering 需要 10 条 peering，维护成本更高。

跨 Region 的安全组不是同一个对象，不能只依赖 `orca-a-sg` 自引用。每个 Region 都创建安全组，并允许：

- TCP 22：你的管理公网 IP /32，以及 node-0 所在 VPC CIDR；
- TCP 3000-3004：上述五个集群 VPC CIDR；
- 出站：至少允许上述集群 CIDR、软件仓库和时间同步。

部署前从 node-0 对每个私网 IP 执行 `ssh` 和 `nc` 检查。hosts 与 committee 仍只写可路由的 Private IPv4，不能混用公网和私网地址。若不建立私网互联，只能使用固定公网/Elastic IP 并逐个 /32 放行，这会增加攻击面和公网流量费，不建议作为正式实验方案。跨 Region 流量会计费，且必须在结果中记录 Region 分布和 RTT。


## 3. 准备 node-0 的 SSH

在你自己的电脑执行：

~~~bash
chmod 400 ~/Downloads/orca-a-aws.pem
scp -i ~/Downloads/orca-a-aws.pem ~/Downloads/orca-a-aws.pem \
  root@NODE0_PUBLIC_IP:/root/.ssh/orca-a-aws.pem
ssh -i ~/Downloads/orca-a-aws.pem root@NODE0_PUBLIC_IP
~~~

进入 node-0 后：

~~~bash
chmod 400 ~/.ssh/orca-a-aws.pem
nano ~/.ssh/config
~~~

写入：

~~~sshconfig
Host 10.*
    User root
    IdentityFile /root/.ssh/orca-a-aws.pem
    StrictHostKeyChecking accept-new
    ConnectTimeout 8
    ServerAliveInterval 5
    ServerAliveCountMax 2
~~~

如果私网不是 `10.*`，把 Host 改为对应网段或 `Host *`。然后执行：

~~~bash
chmod 600 ~/.ssh/config
git clone https://github.com/DrDaydream/Orca-A.git ~/Orca-A
cd ~/Orca-A
cp deploy/hosts-10.txt.example deploy/hosts-10.txt
nano deploy/hosts-10.txt
~~~

hosts 文件每行只写一个 Private IPv4，第一行必须是 node-0。不要写 `root@`、主机名、逗号或端口。20/50 节点创建 `deploy/hosts-20.txt` 或 `deploy/hosts-50.txt`。

~~~bash
wc -l deploy/hosts-10.txt
sort deploy/hosts-10.txt | uniq -d
while read -r ip; do ssh "$ip" hostname; done < deploy/hosts-10.txt
~~~

第一条必须输出 10，第二条不应有输出，第三条必须能登录每台机器。失败时检查同 VPC、私网 IP 和安全组自身的 TCP 22 规则。

## 4. 所有节点安装、编译

在 node-0 的 `~/Orca-A` 中，以 10 节点为例执行：

~~~bash
while read -r ip; do
  ssh "$ip" 'bash -s' <<'REMOTE' &
set -Eeuo pipefail
sudo apt-get update
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
  build-essential cmake clang-14 libclang-14-dev git curl tmux jq \
  python3 python3-pip netcat-openbsd chrony
sudo systemctl enable --now chrony
if [[ ! -x "$HOME/.cargo/bin/cargo" ]]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi
source "$HOME/.cargo/env"
rustup default stable
if [[ -d "$HOME/Orca-A/.git" ]]; then
  git -C "$HOME/Orca-A" pull --ff-only
else
  git clone https://github.com/DrDaydream/Orca-A.git "$HOME/Orca-A"
fi
cd "$HOME/Orca-A"
LIBCLANG_PATH=/usr/lib/llvm-14/lib \
CLANG_PATH=/usr/bin/clang-14 \
CC=/usr/bin/clang-14 \
CXX=/usr/bin/clang++-14 \
CXXFLAGS='-include cstdint' \
cargo build --release --features benchmark
test -x target/release/node
test -x target/release/benchmark_client
REMOTE
done < deploy/hosts-10.txt
wait
~~~

20/50 节点替换 hosts 文件名。首次编译 RocksDB 会较久。确认所有机器版本完全一致：

~~~bash
while read -r ip; do
  ssh "$ip" 'git -C ~/Orca-A rev-parse HEAD'
done < deploy/hosts-10.txt
~~~

输出必须全部相同。不同版本的网络消息可能无法反序列化。

## 5. 生成密钥和配置

在 node-0 执行。每次切换节点规模，都必须重新生成并重新分发：

~~~bash
cd ~/Orca-A
NODES=10
HOSTS_FILE=deploy/hosts-10.txt
rm -f deploy/node-*.json deploy/committee.json deploy/parameters.json
for ((i=0; i<NODES; i++)); do
  ./target/release/node generate_keys --filename "deploy/node-$i.json"
done
chmod 600 deploy/node-*.json

python3 - "$HOSTS_FILE" "$NODES" <<'PY'
import json
import sys
from pathlib import Path

hosts = Path(sys.argv[1])
nodes = int(sys.argv[2])
ips = [line.split("#", 1)[0].strip() for line in hosts.read_text().splitlines()]
ips = [ip for ip in ips if ip]
assert len(ips) == nodes, (len(ips), nodes)
assert len(set(ips)) == nodes, "duplicate private IP"

authorities = {}
for i, ip in enumerate(ips):
    key = json.loads(Path(f"deploy/node-{i}.json").read_text())
    authorities[key["name"]] = {
        "primary": {
            "primary_to_primary": f"{ip}:3000",
            "worker_to_primary": f"{ip}:3001",
        },
        "stake": 1,
        "workers": {
            "0": {
                "primary_to_worker": f"{ip}:3002",
                "transactions": f"{ip}:3003",
                "worker_to_worker": f"{ip}:3004",
            }
        },
    }

Path("deploy/committee.json").write_text(
    json.dumps({"authorities": authorities}, indent=4)
)
Path("deploy/parameters.json").write_text(json.dumps({
    "header_size": 1000,
    "max_header_delay": 200,
    "gc_depth": 50,
    "sync_retry_delay": 10000,
    "sync_retry_nodes": 3,
    "batch_size": 500000,
    "max_batch_delay": 200,
}, indent=4))
PY

mapfile -t IPS < <(awk 'NF && $1 !~ /^#/ {print $1}' "$HOSTS_FILE")
for ((i=0; i<NODES; i++)); do
  ssh "${IPS[$i]}" 'mkdir -p ~/Orca-A/deploy'
  scp "deploy/node-$i.json" deploy/committee.json deploy/parameters.json \
    "${IPS[$i]}:Orca-A/deploy/"
done
~~~

检查公共配置一致：

~~~bash
expected=$(sha256sum deploy/committee.json | awk '{print $1}')
while read -r ip; do
  ssh "$ip" "sha256sum ~/Orca-A/deploy/committee.json"
done < "$HOSTS_FILE"
echo "expected=$expected"
~~~

每台只需要自己的 `node-i.json`，但所有节点的 `committee.json` 和 `parameters.json` 必须相同。

## 6. 敌手选项

`run-multi-servers.sh` 支持：

| 环境变量 | 默认值 | 含义 |
|---|---|---|
| `ORCA_FAULTS` | `0` | 每轮敌手数；0 表示无敌手 |
| `ORCA_ADVERSARY_SEED` | `0` | 确定性调度种子，相同配置与种子可复现 |
| `ORCA_RULE3_BEHAVIOR` | `mixed` | `mixed`、`silent` 或 `participate` |
| `ORCA_CLIENT_DURING_SILENCE` | `send` | `send` 保持输入；`pause` 在静默槽暂停 Client |
| `ORCA_CLIENT_SILENCE_SLOT_MS` | `max_header_delay` | Client 时间表槽宽，正整数毫秒 |

当 `ORCA_FAULTS>0` 时，每轮确定性选择 f 个敌手。敌手 leader 强制进入 Rule 3；`mixed` 让该 leader 按种子选择静默或参与，`silent` 总是静默，`participate` 总是继续参与。非敌手 leader 的 Rule 1/Rule 2 调度长期约为 1:1，但统计分母包括 Rule 3，因此 Rule 1 和 Rule 2 不会各占全部 leader 的 50%。

`send` 用于保持输入工作负载的对照实验；`pause` 使用运行前预生成的单向墙钟时间表，不依赖 Worker 到 Client 的反馈。静默调度不会减少启动的 EC2 数量。

## 7. 运行 10 / 20 / 50 节点

参数依次是节点数、正式运行秒数、集群总输入 TPS。脚本自动按节点数分摊 TPS，并在所有 Client 就绪后才开始计时。

无敌手基线：

~~~bash
cd ~/Orca-A
chmod +x run-multi-servers.sh
./run-multi-servers.sh 10 20 10000
./run-multi-servers.sh 20 60 10000
./run-multi-servers.sh 50 60 10000
~~~

推荐的最大容错敌手实验：

~~~bash
ORCA_FAULTS=3 ORCA_ADVERSARY_SEED=42 \
ORCA_RULE3_BEHAVIOR=mixed ORCA_CLIENT_DURING_SILENCE=pause \
./run-multi-servers.sh 10 20 10000

ORCA_FAULTS=6 ORCA_ADVERSARY_SEED=42 \
ORCA_RULE3_BEHAVIOR=mixed ORCA_CLIENT_DURING_SILENCE=pause \
./run-multi-servers.sh 20 60 10000

ORCA_FAULTS=16 ORCA_ADVERSARY_SEED=42 \
ORCA_RULE3_BEHAVIOR=mixed ORCA_CLIENT_DURING_SILENCE=pause \
./run-multi-servers.sh 50 60 10000
~~~

保持交易输入但让协议敌手静默：

~~~bash
ORCA_FAULTS=3 ORCA_RULE3_BEHAVIOR=silent \
ORCA_CLIENT_DURING_SILENCE=send \
./run-multi-servers.sh 10 20 10000
~~~

自定义路径时：

~~~bash
REMOTE_USER=root \
REMOTE_DIR=/root/Orca-A \
HOSTS_FILE=/root/Orca-A/deploy/hosts-10.txt \
./run-multi-servers.sh 10 20 10000
~~~

结果打印到 node-0 终端，日志下载到 `benchmark/logs/`。同一组对比应固定节点规模、实例类型、总 TPS、时长、参数文件和种子，只改变待比较的敌手选项。

## 8. 运行前检查

~~~bash
# 时间同步
while read -r ip; do ssh "$ip" 'chronyc tracking | head -5'; done < deploy/hosts-10.txt

# 3000-3004 私网连通性
while read -r ip; do
  for port in 3000 3001 3002 3003 3004; do nc -vz -w 2 "$ip" "$port"; done
done < deploy/hosts-10.txt

# 二进制、磁盘、内存
while read -r ip; do
  ssh "$ip" 'test -x ~/Orca-A/target/release/node && df -h / && free -h'
done < deploy/hosts-10.txt
~~~

端口只有相应进程启动后才会监听；运行前的 `Connection refused` 不一定代表安全组错误，`timed out` 通常表示网络或安全组不通。

## 9. 常见故障

- `hostname contains invalid characters`：hosts 中只能有纯私网 IPv4，一行一个。
- `ready=0/N`：检查全部 Worker 日志和 3003；一个 Worker 未启动可能使所有 Client 等待。
- `NoneType object has no attribute group`：至少一份 Client 日志没有 `Start sending transactions`，不要解析未就绪的测试。
- `librocksdb-sys` / bindgen 报错：确认 clang-14 的五个编译环境变量完整。
- `Malformed` / `Serialization`：各机器 Git commit 或 committee 不一致。
- TPS/延迟全 0：检查 Primary 是否提交、测试是否过短、Client 是否真正开始发送。
- 内存持续上升：先降低总 TPS，检查 READY 验签和 pending 是否积压。
- SSH 超时：检查 node-0 私钥权限、私网地址、同 VPC 和安全组自身 TCP 22。
- 端口超时：检查安全组自身 TCP 3000-3004、NACL 和 UFW。

查看远程日志：

~~~bash
ssh NODE_PRIVATE_IP 'tail -100 ~/Orca-A/run/logs/primary-INDEX.log'
ssh NODE_PRIVATE_IP 'tail -100 ~/Orca-A/run/logs/worker-INDEX-0.log'
ssh NODE_PRIVATE_IP 'tail -100 ~/Orca-A/run/logs/client-INDEX-0.log'
~~~

测试结束后 Stop 或 Terminate 实例，并检查 EBS、Elastic IP、公网 IPv4 和跨 AZ 流量费用。私钥与 `deploy/node-*.json` 不应提交到 GitHub。
