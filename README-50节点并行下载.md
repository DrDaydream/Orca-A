# Orca-A 50 节点并行下载与更新

本文说明如何从 Linux 控制机 `node0`，通过 SSH config 同时让 50 台 Ubuntu 服务器下载或更新 Orca-A。

适用环境：

- Windows 只负责登录 `node0`；
- `node0` 同时是协议节点和集群控制机；
- 所有 Linux 服务器的用户为 `ubuntu`；
- 五个 AWS Region 分别使用对应的 PEM；
- `node0` 已通过 `/home/ubuntu/.ssh/config` 配置各区域的 SSH 密钥；
- 节点之间使用跨 Region 私网互联后可达的 Private IPv4。

PEM 是敏感文件，禁止加入仓库或上传到 GitHub。

## 1. 登录 node0

Windows 的默认 SSH config 位于：

~~~text
C:\Users\YOUR_NAME\.ssh\config
~~~

如果 config 已为 node0 的公网 IP 配置 `User ubuntu` 和 `IdentityFile`，在 PowerShell 中执行：

~~~powershell
ssh NODE0_PUBLIC_IP
~~~

## 2. 检查 node0 的 SSH config

在 node0 上，不需要为每台服务器定义别名。可以按私网 CIDR选择区域 PEM：

~~~sshconfig
Host 10.10.*
    User ubuntu
    IdentityFile /home/ubuntu/.ssh/us-east-1.pem
    IdentitiesOnly yes

Host 10.20.*
    User ubuntu
    IdentityFile /home/ubuntu/.ssh/sa-east-1.pem
    IdentitiesOnly yes

Host 10.30.*
    User ubuntu
    IdentityFile /home/ubuntu/.ssh/eu-west-2.pem
    IdentitiesOnly yes

Host 10.40.*
    User ubuntu
    IdentityFile /home/ubuntu/.ssh/ap-southeast-1.pem
    IdentitiesOnly yes

Host 10.50.*
    User ubuntu
    IdentityFile /home/ubuntu/.ssh/ap-southeast-2.pem
    IdentitiesOnly yes

Host 10.*
    BatchMode yes
    StrictHostKeyChecking accept-new
    ConnectTimeout 10
    ServerAliveInterval 10
    ServerAliveCountMax 3
~~~

设置权限：

~~~bash
chmod 700 /home/ubuntu/.ssh
chmod 600 /home/ubuntu/.ssh/config
chmod 400 /home/ubuntu/.ssh/*.pem
~~~

检查某个地址实际使用的用户和密钥：

~~~bash
ssh -G 10.10.1.20 | grep -E '^(user|hostname|identityfile) '
ssh 10.10.1.20 hostname
~~~

因为 config 已设置 `User ubuntu`，后续命令只使用 `ssh IP`，无需再写 `ubuntu@IP`。

## 3. 准备 hosts-50.txt

在 node0 上执行：

~~~bash
git clone https://github.com/DrDaydream/Orca-A.git /home/ubuntu/Orca-A
cd /home/ubuntu/Orca-A
cp deploy/hosts-50.txt.example deploy/hosts-50.txt
nano deploy/hosts-50.txt
~~~

文件必须包含 50 个可路由的私网 IPv4，每行一个。第一行是 node0 的私网 IP。不能填写 `ubuntu@`、SSH 端口、逗号或说明文字。

检查数量、重复项和非法行：

~~~bash
sed -e 's/#.*//' -e '/^[[:space:]]*$/d' deploy/hosts-50.txt | wc -l
sed -e 's/#.*//' -e '/^[[:space:]]*$/d' deploy/hosts-50.txt | sort | uniq -d
sed -e 's/#.*//' -e '/^[[:space:]]*$/d' deploy/hosts-50.txt \
  | awk '!/^([0-9]{1,3}\.){3}[0-9]{1,3}$/ { print "Invalid host:", $0; bad=1 } END { exit bad }'
~~~

## 4. 并行检查 SSH

~~~bash
cd /home/ubuntu/Orca-A
sed -e 's/#.*//' -e '/^[[:space:]]*$/d' deploy/hosts-50.txt \
  | xargs -P 50 -I {} ssh {} 'printf "%s: connected\n" "$(hostname)"'
~~~

如果跨洲连接不稳定，将 `-P 50` 改为 `-P 10` 或 `-P 20`。

## 5. 并行安装 Git

~~~bash
sed -e 's/#.*//' -e '/^[[:space:]]*$/d' deploy/hosts-50.txt \
  | xargs -P 50 -I {} ssh {} \
    'command -v git >/dev/null || { sudo apt-get update && sudo apt-get install -y git; }'
~~~

## 6. 50 个节点同时下载或更新 Orca-A

下面的命令不会删除已有仓库：目录中存在 Git 仓库时执行 `git pull --ff-only`，否则执行 `git clone`。

~~~bash
sed -e 's/#.*//' -e '/^[[:space:]]*$/d' deploy/hosts-50.txt \
  | xargs -P 50 -I {} ssh {} \
    'if [ -d /home/ubuntu/Orca-A/.git ]; then
         git -C /home/ubuntu/Orca-A pull --ff-only;
     elif [ -e /home/ubuntu/Orca-A ]; then
         echo "ERROR: /home/ubuntu/Orca-A exists but is not a Git repository" >&2;
         exit 1;
     else
         git clone https://github.com/DrDaydream/Orca-A.git /home/ubuntu/Orca-A;
     fi'
~~~

如果 GitHub 对 50 个同时连接限速，改用 10 个并发任务：

~~~bash
sed -e 's/#.*//' -e '/^[[:space:]]*$/d' deploy/hosts-50.txt \
  | xargs -P 10 -I {} ssh {} \
    'if [ -d /home/ubuntu/Orca-A/.git ]; then
         git -C /home/ubuntu/Orca-A pull --ff-only;
     else
         git clone https://github.com/DrDaydream/Orca-A.git /home/ubuntu/Orca-A;
     fi'
~~~

## 7. 核对所有节点版本

~~~bash
sed -e 's/#.*//' -e '/^[[:space:]]*$/d' deploy/hosts-50.txt \
  | xargs -P 50 -I {} ssh {} \
    'printf "%s: " "$(hostname)"; git -C /home/ubuntu/Orca-A rev-parse --short HEAD'
~~~

所有节点应输出相同的提交号。也可以检查远程仓库地址和分支：

~~~bash
sed -e 's/#.*//' -e '/^[[:space:]]*$/d' deploy/hosts-50.txt \
  | xargs -P 50 -I {} ssh {} \
    'printf "%s: " "$(hostname)"; git -C /home/ubuntu/Orca-A remote get-url origin; git -C /home/ubuntu/Orca-A branch --show-current'
~~~

## 8. 常见错误

- `Permission denied (publickey)`：检查目标 IP 匹配的 `Host` CIDR、`User ubuntu`、PEM 路径和 PEM 权限。
- `Connection timed out`：检查跨 Region 路由、VPC Peering/Transit Gateway、网络 ACL和安全组 TCP 22。
- `Host key verification failed`：先手动执行一次 `ssh IP`，或确认 config 中存在 `StrictHostKeyChecking accept-new`。
- `repository not found`：确认仓库 URL正确；私有仓库还需要为节点配置只读 deploy key。
- node0 无法 SSH 到自己的私网 IP：为 node0 配置本机 SSH 密钥，或确保 node0 对应的区域 PEM可以认证该实例。

完成代码下载后，继续按照 [AWS 10/20/50 节点完整部署文档](README-AWS-10-20-50节点完整部署.md) 安装依赖、编译、生成配置并运行基准测试。

## 9. 并行安装依赖和编译

建议系统依赖使用 10 台并发，Rust 编译使用 10 到 20 台并发；`CARGO_BUILD_JOBS=2` 限制每台机器的编译线程，避免内存耗尽。

~~~bash
cd /home/ubuntu/Orca-A
HOSTS=deploy/hosts-50.txt

sed -e 's/#.*//' -e '/^[[:space:]]*$/d' "$HOSTS" | xargs -P 10 -I {} ssh {} '
  set -e
  sudo apt-get update
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
    build-essential clang libclang-dev cmake pkg-config libssl-dev \
    librocksdb-dev git curl
'

sed -e 's/#.*//' -e '/^[[:space:]]*$/d' "$HOSTS" | xargs -P 10 -I {} ssh {} '
  if ! command -v cargo >/dev/null 2>&1; then
    curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  fi
'

sed -e 's/#.*//' -e '/^[[:space:]]*$/d' "$HOSTS" | xargs -P 20 -I {} ssh {} '
  set -e; cd /home/ubuntu/Orca-A
  . "$HOME/.cargo/env" 2>/dev/null || true
  cargo fetch
'

sed -e 's/#.*//' -e '/^[[:space:]]*$/d' "$HOSTS" | xargs -P 10 -I {} ssh {} '
  set -e; cd /home/ubuntu/Orca-A
  . "$HOME/.cargo/env" 2>/dev/null || true
  CARGO_BUILD_JOBS=2 cargo build --quiet --release --features benchmark
'
~~~

检查编译结果：

~~~bash
sed -e 's/#.*//' -e '/^[[:space:]]*$/d' "$HOSTS" | xargs -P 50 -I {} ssh {} '
  printf "%s: " "$(hostname)"
  test -x /home/ubuntu/Orca-A/target/release/node && echo "build ok" || echo "build failed"
'
~~~
