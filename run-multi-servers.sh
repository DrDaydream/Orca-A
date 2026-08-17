#!/usr/bin/env bash
set -Eeuo pipefail

# 用法：./run-multi-servers.sh <节点数:10|20|50> <持续秒数> <总输入TPS>
# 示例：./run-multi-servers.sh 10 20 10000

NODES="${1:-}"
DURATION="${2:-20}"
TOTAL_RATE="${3:-10000}"

case "$NODES" in
  10|20|50) ;;
  *) echo "用法: $0 <10|20|50> [持续秒数] [总输入TPS]" >&2; exit 2 ;;
esac
[[ "$DURATION" =~ ^[1-9][0-9]*$ ]] || { echo "持续秒数必须是正整数" >&2; exit 2; }
[[ "$TOTAL_RATE" =~ ^[1-9][0-9]*$ ]] || { echo "TPS 必须是正整数" >&2; exit 2; }

REMOTE_USER="${REMOTE_USER:-root}"
REMOTE_DIR="${REMOTE_DIR:-/root/Orca-A}"
HOSTS_FILE="${HOSTS_FILE:-deploy/hosts-${NODES}.txt}"
MAX_PARALLEL="${MAX_PARALLEL:-10}"
READY_TIMEOUT="${READY_TIMEOUT:-180}"
TX_SIZE="${TX_SIZE:-512}"
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=8 -o ServerAliveInterval=5 -o ServerAliveCountMax=2)
LOCAL_LOGS="benchmark/logs"

[[ -f "$HOSTS_FILE" ]] || { echo "找不到 $HOSTS_FILE" >&2; exit 1; }
mapfile -t IPS < <(sed -e 's/#.*//' -e 's/[[:space:]]//g' "$HOSTS_FILE" | awk 'NF')
[[ "${#IPS[@]}" -eq "$NODES" ]] || {
  echo "$HOSTS_FILE 必须正好包含 $NODES 个私网 IP，当前为 ${#IPS[@]} 个" >&2
  exit 1
}
[[ "$(printf '%s\n' "${IPS[@]}" | sort -u | wc -l)" -eq "$NODES" ]] || {
  echo "$HOSTS_FILE 中存在重复 IP" >&2
  exit 1
}
for ip in "${IPS[@]}"; do
  [[ "$ip" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}$ ]] || { echo "无效 IP: $ip" >&2; exit 1; }
done

RATE_SHARE=$(((TOTAL_RATE + NODES - 1) / NODES))
TX_NODES=""
for ip in "${IPS[@]}"; do TX_NODES+="${ip}:3003 "; done

remote() { ssh "${SSH_OPTS[@]}" "${REMOTE_USER}@$1" "$2"; }

# 每启动 MAX_PARALLEL 个后台任务就等待，避免 50 个 SSH 同时冲击控制机。
wait_batch() {
  local running="$1"
  if (( running % MAX_PARALLEL == 0 )); then wait; fi
}

stop_all() {
  echo "[清理] 停止 $NODES 台机器上的 Orca 测试进程……"
  local count=0
  for ip in "${IPS[@]}"; do
    remote "$ip" "tmux kill-session -t orca-client 2>/dev/null || true; tmux kill-session -t orca-primary 2>/dev/null || true; tmux kill-session -t orca-worker 2>/dev/null || true" &
    count=$((count + 1)); wait_batch "$count"
  done
  wait || true
}
trap stop_all EXIT INT TERM

echo "配置：节点=$NODES，时长=${DURATION}s，总输入=${TOTAL_RATE} TPS，每客户端=${RATE_SHARE} TPS"
echo "[1/8] 检查 SSH、二进制、节点密钥和公共配置……"
for i in "${!IPS[@]}"; do
  remote "${IPS[$i]}" "test -x '${REMOTE_DIR}/target/release/node' && test -x '${REMOTE_DIR}/target/release/benchmark_client' && test -f '${REMOTE_DIR}/deploy/node-${i}.json' && test -f '${REMOTE_DIR}/deploy/committee.json' && test -f '${REMOTE_DIR}/deploy/parameters.json'"
  echo "  node-$i ${IPS[$i]} OK"
done

echo "[2/8] 清理上次基准测试的临时数据库和日志……"
count=0
for ip in "${IPS[@]}"; do
  remote "$ip" "tmux kill-session -t orca-client 2>/dev/null || true; tmux kill-session -t orca-primary 2>/dev/null || true; tmux kill-session -t orca-worker 2>/dev/null || true; cd '${REMOTE_DIR}'; rm -rf run/db-primary run/db-worker run/logs; mkdir -p run/logs" &
  count=$((count + 1)); wait_batch "$count"
done
wait

echo "[3/8] 启动 $NODES 个 Worker……"
count=0
for i in "${!IPS[@]}"; do
  remote "${IPS[$i]}" "cd '${REMOTE_DIR}' && tmux new-session -d -s orca-worker \"RUST_LOG=info ./target/release/node -vv run --keys deploy/node-${i}.json --committee deploy/committee.json --parameters deploy/parameters.json --store run/db-worker worker --id 0 |& tee run/logs/worker-${i}-0.log\"" &
  count=$((count + 1)); wait_batch "$count"
done
wait

echo "[4/8] 启动 $NODES 个 Primary……"
count=0
for i in "${!IPS[@]}"; do
  remote "${IPS[$i]}" "cd '${REMOTE_DIR}' && tmux new-session -d -s orca-primary \"RUST_LOG=info ./target/release/node -vv run --keys deploy/node-${i}.json --committee deploy/committee.json --parameters deploy/parameters.json --store run/db-primary primary |& tee run/logs/primary-${i}.log\"" &
  count=$((count + 1)); wait_batch "$count"
done
wait
sleep 5

echo "[5/8] 检查 Worker 的 3003 端口……"
for i in "${!IPS[@]}"; do
  if ! remote "${IPS[$i]}" "ss -ltn | grep -q ':3003 '"; then
    echo "错误：node-$i (${IPS[$i]}) 未监听 3003" >&2
    remote "${IPS[$i]}" "tail -100 '${REMOTE_DIR}/run/logs/worker-${i}-0.log'" || true
    exit 1
  fi
  echo "  node-$i 3003 OK"
done

echo "启动 $NODES 个 benchmark_client……"
count=0
for i in "${!IPS[@]}"; do
  remote "${IPS[$i]}" "cd '${REMOTE_DIR}' && tmux new-session -d -s orca-client \"RUST_LOG=info ./target/release/benchmark_client '${IPS[$i]}:3003' --size '${TX_SIZE}' --rate '${RATE_SHARE}' --nodes ${TX_NODES} |& tee run/logs/client-${i}-0.log\"" &
  count=$((count + 1)); wait_batch "$count"
done
wait

echo "[等待] 所有 Client 需要连通全部 $NODES 个交易端口……"
ready=0
for ((elapsed=0; elapsed<READY_TIMEOUT; elapsed+=3)); do
  ready=0
  waiting=()
  for i in "${!IPS[@]}"; do
    if remote "${IPS[$i]}" "grep -q 'Start sending transactions' '${REMOTE_DIR}/run/logs/client-${i}-0.log'"; then
      ready=$((ready + 1))
    else
      waiting+=("$i")
    fi
  done
  echo "  ${elapsed}s: ready=${ready}/${NODES}; waiting=${waiting[*]:-none}"
  (( ready == NODES )) && break
  sleep 3
done
if (( ready != NODES )); then
  echo "错误：${READY_TIMEOUT}s 内仅 ${ready}/${NODES} 个 Client 就绪" >&2
  for i in "${waiting[@]}"; do
    echo "===== node-$i client ====="
    remote "${IPS[$i]}" "tail -80 '${REMOTE_DIR}/run/logs/client-${i}-0.log'" || true
    echo "===== node-$i worker ====="
    remote "${IPS[$i]}" "tail -80 '${REMOTE_DIR}/run/logs/worker-${i}-0.log'" || true
  done
  exit 1
fi

echo "[6/8] 全部就绪，正式运行 ${DURATION} 秒……"
for ((left=DURATION; left>0; left--)); do printf '\r  剩余 %3d 秒' "$left"; sleep 1; done
echo

echo "[7/8] 停止进程并下载日志……"
stop_all
trap - EXIT INT TERM
rm -rf "$LOCAL_LOGS"
mkdir -p "$LOCAL_LOGS"
for i in "${!IPS[@]}"; do
  scp "${SSH_OPTS[@]}" "${REMOTE_USER}@${IPS[$i]}:${REMOTE_DIR}/run/logs/primary-${i}.log" "$LOCAL_LOGS/"
  scp "${SSH_OPTS[@]}" "${REMOTE_USER}@${IPS[$i]}:${REMOTE_DIR}/run/logs/worker-${i}-0.log" "$LOCAL_LOGS/"
  scp "${SSH_OPTS[@]}" "${REMOTE_USER}@${IPS[$i]}:${REMOTE_DIR}/run/logs/client-${i}-0.log" "$LOCAL_LOGS/"
done

echo "[8/8] 解析结果……"
cd benchmark
python3 - "$NODES" <<'PY'
import sys
from benchmark.logs import LogParser

nodes = int(sys.argv[1])
print(LogParser.process("logs", faults=0).result())
print(f"Parsed {nodes} active nodes with faults=0")
PY

