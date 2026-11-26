#!/usr/bin/env bash
# 受限 SSH 导出与配对的端到端验收（CARM-295）。
# 以当前用户在 127.0.0.1 起一个临时 sshd（不需要 root、不动系统 sshd 和 ~/.ssh），
# 造一个采集端数据目录，然后逐条验证：专用钥匙只能读用量页；交互 shell、转发、任意文件读取、
# 错误指纹、撤销授权、非允许网段、主机密钥被换、把本机当远端都被拒。
#
#   cargo build -p agenthome-cli && script/test-remote-ssh.sh [target/debug/agenthome]
set -uo pipefail

BIN=$(cd "$(dirname "${1:-target/debug/agenthome}")" && pwd)/$(basename "${1:-target/debug/agenthome}")
SSHD=${SSHD:-/usr/sbin/sshd}
PORT=${PORT:-22950}
LAB=$(mktemp -d)
trap '[ -f "$LAB/sshd.pid" ] && kill "$(cat "$LAB/sshd.pid")" 2>/dev/null; rm -rf "$LAB"' EXIT
RUNNER=$LAB/runner
MAC=$LAB/mac
AK=$LAB/authorized_keys
FAILS=0

step() { printf '\n=== %s\n' "$*"; }
expect_fail() { # 期望命令失败且输出含关键字
  local want=$1; shift
  local out; out=$("$@" 2>&1); local rc=$?
  printf '%s\n(exit=%s)\n' "$out" "$rc"
  if [ "$rc" -eq 0 ] || ! grep -q -- "$want" <<<"$out"; then echo "!! 未按预期被拒（应含 ${want}）"; FAILS=$((FAILS+1)); fi
}
expect_ok() {
  local out; out=$("$@" 2>&1); local rc=$?
  printf '%s\n(exit=%s)\n' "$(head -c 600 <<<"$out")" "$rc"
  if [ "$rc" -ne 0 ]; then echo "!! 应成功"; FAILS=$((FAILS+1)); fi
}

start_sshd() { # $1 = 主机密钥
  [ -f "$LAB/sshd.pid" ] && kill "$(cat "$LAB/sshd.pid")" 2>/dev/null && sleep 0.5
  cat >"$LAB/sshd_config" <<EOF
Port $PORT
ListenAddress 127.0.0.1
HostKey $1
AuthorizedKeysFile $AK
PidFile $LAB/sshd.pid
UsePAM no
StrictModes no
PasswordAuthentication no
KbdInteractiveAuthentication no
LogLevel VERBOSE
EOF
  "$SSHD" -f "$LAB/sshd_config" -E "$LAB/sshd.log"
  sleep 0.5
}

# 采集端数据：设备身份 + 1500 条用量（超过一页）。
mkdir -p "$RUNNER/usage" "$MAC" "$LAB/hostkeys" "$LAB/other"
MID=5b1f0c2e-8d3a-4c7b-9e21-0a6f4d2c9b11
printf '{"schema":1,"machine_id":"%s","session_label":"runner","aliases":[],"event_key_secret":"%s"}' \
  "$MID" "$(openssl rand -hex 32)" >"$RUNNER/device.json"
python3 - "$RUNNER/usage/usage.db" "$MID" <<'EOF'
import sqlite3, sys, os
c = sqlite3.connect(sys.argv[1])
c.executescript("""
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE usage (source TEXT NOT NULL, event_key TEXT NOT NULL, seq INTEGER NOT NULL UNIQUE,
  revision INTEGER NOT NULL, op TEXT NOT NULL, vendor TEXT, model TEXT, ts_ms INTEGER NOT NULL,
  requests INTEGER NOT NULL, input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
  cache_read_tokens INTEGER NOT NULL, cache_write_5m_tokens INTEGER NOT NULL,
  cache_write_1h_tokens INTEGER NOT NULL, reasoning_tokens INTEGER NOT NULL,
  PRIMARY KEY (source, event_key));
CREATE TABLE sources (source TEXT NOT NULL, path TEXT NOT NULL, size INTEGER NOT NULL,
  mtime_ms INTEGER NOT NULL, visible INTEGER NOT NULL, PRIMARY KEY (source, path));
PRAGMA user_version = 1;""")
n = 1500
c.execute("INSERT INTO meta VALUES ('machine_id', ?), ('last_seq', ?), ('store_id', ?)",
          (sys.argv[2], str(n), os.urandom(16).hex()))
for i in range(1, n + 1):
    c.execute("INSERT INTO usage VALUES ('claude-code', ?, ?, 1, 'upsert', 'anthropic',"
              " 'claude-opus-4-5-20251101', ?, 1, 10, 300, 40000, 100, 0, 0)",
              (os.urandom(32).hex(), i, 1763500000000 + i))
c.commit()
EOF
ssh-keygen -q -t ed25519 -N "" -f "$LAB/hostkeys/ssh_host_ed25519_key"
ssh-keygen -q -t ed25519 -N "" -f "$LAB/other/ssh_host_ed25519_key"
: >"$AK"
start_sshd "$LAB/hostkeys/ssh_host_ed25519_key"

step "采集端：打印主机密钥指纹（经可信渠道交给 Mac）"
FP=$("$BIN" remote host-fingerprint --dir "$LAB/hostkeys" | awk '{print $1}')
echo "$FP"
WRONG=$(ssh-keygen -lf "$LAB/other/ssh_host_ed25519_key.pub" | awk '{print $2}')

step "Mac：错误指纹配对 → 拒绝"
expect_fail host_key_mismatch "$BIN" remote pair --name runner --host 127.0.0.1 --port "$PORT" \
  --user "$USER" --fingerprint "$WRONG" --allow 127.0.0.0/8 --data-dir "$MAC"

step "Mac：目标不在允许网段 → 拒绝（不连接）"
expect_fail net_not_allowed "$BIN" remote pair --name runner --host 127.0.0.1 --port "$PORT" \
  --user "$USER" --fingerprint "$FP" --allow 192.168.0.0/16 --data-dir "$MAC"

step "Mac：允许网段写成公网 → 拒绝"
expect_fail "不是内网" "$BIN" remote pair --name runner --host 127.0.0.1 --port "$PORT" \
  --user "$USER" --fingerprint "$FP" --allow 0.0.0.0/0 --data-dir "$MAC"

step "Mac：正确指纹配对"
expect_ok "$BIN" remote pair --name runner --host 127.0.0.1 --port "$PORT" \
  --user "$USER" --fingerprint "$FP" --allow 127.0.0.0/8 --data-dir "$MAC"
KEY=$MAC/remote/runner/id_ed25519

step "默认关闭：未授权时拉取 → 拒绝"
expect_fail unauthorized "$BIN" remote fetch --name runner --data-dir "$MAC"

step "采集端：授权专用钥匙（来源 127.0.0.1）"
expect_ok "$BIN" remote authorize --name mac --pubkey "$KEY.pub" --from 127.0.0.1 \
  --authorized-keys "$AK" --data-dir "$RUNNER" --exe "$BIN"

step "Mac：按游标拉完所有页（每页 ≤ 262144 字节）"
after=0; pages=0; total=0
while :; do
  "$BIN" remote fetch --name runner --after "$after" --data-dir "$MAC" >"$LAB/page.json" 2>"$LAB/page.err" \
    || { cat "$LAB/page.err"; FAILS=$((FAILS+1)); break; }
  size=$(wc -c <"$LAB/page.json")
  read -r n next more < <(python3 -c 'import json,sys;p=json.load(open(sys.argv[1]));print(len(p["page"]["records"]),p["next"],p["more"])' "$LAB/page.json")
  echo "page $((pages+1)): after=$after records=$n next=$next more=$more bytes=$size"
  [ "$size" -le 262145 ] || { echo "!! 超过页上限"; FAILS=$((FAILS+1)); }
  pages=$((pages+1)); total=$((total+n)); after=$next
  [ "$more" = "True" ] || break
done
echo "合计 $total 条 / $pages 页"
[ "$total" -eq 1500 ] || { echo "!! 条数不对"; FAILS=$((FAILS+1)); }

SSHK=(ssh -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none
  -o StrictHostKeyChecking=yes -o "UserKnownHostsFile=$MAC/remote/runner/known_hosts"
  -o HostKeyAlias=agenthome-runner -i "$KEY" -p "$PORT" -l "$USER")

step "专用钥匙：登录 shell（不带命令）→ 拒绝"
expect_fail "不提供交互 shell" "${SSHK[@]}" 127.0.0.1 </dev/null
step "专用钥匙：强制分配终端 -tt → 拒绝（restrict 不给 pty）"
expect_fail "PTY allocation request failed" "${SSHK[@]}" -tt 127.0.0.1 </dev/null
step "专用钥匙：读任意文件 cat /etc/passwd → 拒绝"
expect_fail "只接受用量导出请求" "${SSHK[@]}" 127.0.0.1 cat /etc/passwd
step "专用钥匙：在导出请求后拼接命令 → 拒绝"
expect_fail "agenthome ssh-export" "${SSHK[@]}" 127.0.0.1 'agenthome-export v1 after=0; cat /etc/passwd'
step "专用钥匙：scp 取文件 → 拒绝"
expect_fail "" scp -F /dev/null -O -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes \
  -o "UserKnownHostsFile=$MAC/remote/runner/known_hosts" -o HostKeyAlias=agenthome-runner \
  -i "$KEY" -P "$PORT" "$USER@127.0.0.1:/etc/passwd" "$LAB/stolen"
[ -e "$LAB/stolen" ] && { echo "!! 文件被读走了"; FAILS=$((FAILS+1)); }
step "专用钥匙：sftp → 拒绝"
expect_fail "" sftp -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes \
  -o "UserKnownHostsFile=$MAC/remote/runner/known_hosts" -o HostKeyAlias=agenthome-runner \
  -i "$KEY" -P "$PORT" "$USER@127.0.0.1" <<<"get /etc/passwd $LAB/stolen2"
[ -e "$LAB/stolen2" ] && { echo "!! 文件被读走了"; FAILS=$((FAILS+1)); }
# 转发要等真有连接进来才会被服务端拒绝，所以后台建好转发后实际连一次，再看 ssh 报的结果。
try_forward() { # $1 = 说明，其余为 ssh 转发参数；$LPORT 为本地监听端口
  local label=$1; shift
  "${SSHK[@]}" -N "$@" 127.0.0.1 2>"$LAB/fwd.err" &
  local pid=$!
  sleep 1.5
  curl -s -m 3 ${SOCKS:+--socks5 127.0.0.1:$LPORT} "http://127.0.0.1:${TARGET:-$LPORT}/" >/dev/null 2>&1
  sleep 0.5
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  cat "$LAB/fwd.err"
  if grep -q "prohibited" "$LAB/fwd.err"; then echo "（$label 被拒）"; else echo "!! $label 没有被拒"; FAILS=$((FAILS+1)); fi
}
step "专用钥匙：本地端口转发 -L → 拒绝"
LPORT=22951 try_forward "-L" -L 22951:127.0.0.1:"$PORT"
step "专用钥匙：动态转发 -D（SOCKS）→ 拒绝"
LPORT=22952 SOCKS=1 TARGET=$PORT try_forward "-D" -D 22952
step "专用钥匙：远程转发 -R → 拒绝"
expect_fail "forwarding failed" "${SSHK[@]}" -o ExitOnForwardFailure=yes -R 22953:127.0.0.1:"$PORT" -N 127.0.0.1
step "sshd 日志里与限制相关的记录"
grep -iE "forward|refused|not permitted|restrict" "$LAB/sshd.log" | tail -8

step "Mac：把本机当远端（对端 machine_id 与本机相同）→ 拒绝"
cp "$RUNNER/device.json" "$MAC/device.json"
expect_fail self_pairing "$BIN" remote fetch --name runner --data-dir "$MAC"
rm "$MAC/device.json"

step "采集端：授权来源改成 10.99.0.0/16（Mac 不在其中）→ 拉取被拒"
expect_ok "$BIN" remote authorize --name mac --pubkey "$KEY.pub" --from 10.99.0.0/16 \
  --authorized-keys "$AK" --data-dir "$RUNNER" --exe "$BIN"
expect_fail unauthorized "$BIN" remote fetch --name runner --data-dir "$MAC"
grep -i "from" "$LAB/sshd.log" | tail -2

step "采集端：撤销授权 → 拉取被拒"
"$BIN" remote authorize --name mac --pubkey "$KEY.pub" --from 127.0.0.1 \
  --authorized-keys "$AK" --data-dir "$RUNNER" --exe "$BIN" >/dev/null
expect_ok "$BIN" remote fetch --name runner --data-dir "$MAC"
expect_ok "$BIN" remote revoke --name mac --authorized-keys "$AK"
echo "authorized_keys 剩余行数：$(wc -l <"$AK")"
expect_fail unauthorized "$BIN" remote fetch --name runner --data-dir "$MAC"

step "主机密钥被换（同地址换了一台机器 / 中间人）→ 拒绝"
"$BIN" remote authorize --name mac --pubkey "$KEY.pub" --from 127.0.0.1 \
  --authorized-keys "$AK" --data-dir "$RUNNER" --exe "$BIN" >/dev/null
start_sshd "$LAB/other/ssh_host_ed25519_key"
expect_fail host_key_mismatch "$BIN" remote fetch --name runner --data-dir "$MAC"

step "采集端无响应（固定命令卡住）→ Mac 30 秒后超时放弃"
sed -i.bak "s|command=\"[^\"]*\"|command=\"sleep 120\"|" "$AK"
start_sshd "$LAB/hostkeys/ssh_host_ed25519_key"
t0=$(date +%s)
expect_fail "拉取超时" "$BIN" remote fetch --name runner --data-dir "$MAC"
echo "用时 $(( $(date +%s) - t0 )) 秒"

step "结果"
if [ "$FAILS" -eq 0 ]; then echo "全部通过"; else echo "失败 $FAILS 项"; exit 1; fi
