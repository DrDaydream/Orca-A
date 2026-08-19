# Windows 使用五个 Region PEM 控制 Orca-A

本文适用于以下环境：本地控制电脑是 Windows；Linux 服务器用户是 `ubuntu`；节点分布在五个 AWS Region；每个 Region 使用不同 PEM；Linux `node-0` 同时是控制机和协议节点 0。

五个 PEM 对应五个 Region，而不是五个协议。Orca-A、Orca-B、Bullshark 和 Tusk 可以共用同一套区域密钥配置。

## 1. 连接方式

~~~text
Windows + node-0 所在 Region 的 PEM
                 |
                 | node-0 公网 IP，TCP 22
                 v
Linux node-0（ubuntu 用户）
                 |
                 | 根据目标私网 CIDR自动选择五个 PEM之一
                 v
五个 Region 的 Linux 节点
~~~

Windows 只需直接登录 node-0。`hosts` 和 committee 使用跨 Region 私网互联后可达的 Private IPv4。

## 2. Windows PowerShell 登录 node-0

确认 Windows 已安装 OpenSSH 客户端：

~~~powershell
ssh -V
scp -V
~~~

假设 node-0 位于欧洲，欧洲 PEM 为 `C:\Users\YOUR_NAME\Downloads\eu-west-2.pem`：

~~~powershell
$Pem = "C:\Users\YOUR_NAME\Downloads\eu-west-2.pem"
icacls $Pem /inheritance:r
icacls $Pem /grant:r "$($env:USERNAME):(R)"
ssh -i $Pem ubuntu@NODE0_PUBLIC_IP
~~~

## 3. 上传五个区域 PEM

假设五个文件分别为：

~~~text
us-east-1.pem
sa-east-1.pem
eu-west-2.pem
ap-southeast-1.pem
ap-southeast-2.pem
~~~

在 PowerShell 执行：

~~~powershell
$Node0Pem = "C:\Users\YOUR_NAME\Downloads\eu-west-2.pem"
$PemDir = "C:\Users\YOUR_NAME\Downloads"
scp -i C:\keys\node0.pem C:\keys\region-*.pem ubuntu@NODE0_PUBLIC_IP:/home/ubuntu/.ssh/
ssh -i $Node0Pem ubuntu@NODE0_PUBLIC_IP "mkdir -p /home/ubuntu/.ssh && chmod 700 /home/ubuntu/.ssh"
scp -i $Node0Pem "$PemDir\us-east-1.pem" ubuntu@NODE0_PUBLIC_IP:/home/ubuntu/.ssh/
scp -i $Node0Pem "$PemDir\sa-east-1.pem" ubuntu@NODE0_PUBLIC_IP:/home/ubuntu/.ssh/
scp -i $Node0Pem "$PemDir\eu-west-2.pem" ubuntu@NODE0_PUBLIC_IP:/home/ubuntu/.ssh/
scp -i $Node0Pem "$PemDir\ap-southeast-1.pem" ubuntu@NODE0_PUBLIC_IP:/home/ubuntu/.ssh/
scp -i $Node0Pem "$PemDir\ap-southeast-2.pem" ubuntu@NODE0_PUBLIC_IP:/home/ubuntu/.ssh/
ssh -i $Node0Pem ubuntu@NODE0_PUBLIC_IP
~~~

PEM 是敏感文件，禁止提交到 GitHub。

## 4. node-0 按区域选择密钥

以下示例假设五个 VPC CIDR 是 `10.10.0.0/16` 到 `10.50.0.0/16`。在 node-0 执行：

~~~bash
chmod 400 /home/ubuntu/.ssh/*.pem
nano /home/ubuntu/.ssh/config
~~~

写入：

~~~sshconfig
Host 10.10.*
    User ubuntu
    IdentityFile /home/ubuntu/.ssh/us-east-1.pem

Host 10.20.*
    User ubuntu
    IdentityFile /home/ubuntu/.ssh/sa-east-1.pem

Host 10.30.*
    User ubuntu
    IdentityFile /home/ubuntu/.ssh/eu-west-2.pem

Host 10.40.*
    User ubuntu
    IdentityFile /home/ubuntu/.ssh/ap-southeast-1.pem

Host 10.50.*
    User ubuntu
    IdentityFile /home/ubuntu/.ssh/ap-southeast-2.pem

Host 10.*
    StrictHostKeyChecking accept-new
    ConnectTimeout 8
    ServerAliveInterval 5
    ServerAliveCountMax 2
~~~

`Host` 必须按你的真实 VPC CIDR修改。更具体的网段必须写在通用 `Host 10.*` 前面。

~~~bash
chmod 600 /home/ubuntu/.ssh/config
ssh -G 10.10.1.10 | grep -E '^(user|identityfile) '
ssh -G 10.20.1.10 | grep -E '^(user|identityfile) '
ssh ubuntu@NODE_IN_EACH_REGION_PRIVATE_IP hostname
~~~

## 5. hosts 文件

~~~bash
git clone https://github.com/DrDaydream/Orca-A.git /home/ubuntu/Orca-A
cd /home/ubuntu/Orca-A
cp deploy/hosts-10.txt.example deploy/hosts-10.txt
nano deploy/hosts-10.txt
~~~

每行只写一个可路由的私网 IPv4，第一行必须是 node-0。可以按 Region 分组，但不能写 `ubuntu@`、端口、逗号或主机名：

~~~text
10.30.1.10
10.30.1.11
10.10.1.10
10.10.1.11
10.20.1.10
10.20.1.11
10.40.1.10
10.40.1.11
10.50.1.10
10.50.1.11
~~~

~~~bash
wc -l deploy/hosts-10.txt
sort deploy/hosts-10.txt | uniq -d
while read -r ip; do ssh ubuntu@"$ip" hostname; done < deploy/hosts-10.txt
~~~

## 6. 运行 Orca-A

先按 [AWS 完整部署文档](README-AWS-10-20-50节点完整部署.md) 安装、编译、生成并分发配置。运行脚本默认读取 `~/.ssh/config`，不要传单个 `SSH_KEY`：

~~~bash
cd /home/ubuntu/Orca-A
REMOTE_USER=ubuntu \
REMOTE_DIR=/home/ubuntu/Orca-A \
HOSTS_FILE=/home/ubuntu/Orca-A/deploy/hosts-10.txt \
./run-multi-servers.sh 10 20 10000
~~~

三个参数依次为节点数、运行秒数、集群总输入 TPS。安全组和跨 Region 路由还必须满足 AWS 完整部署文档中的要求。

