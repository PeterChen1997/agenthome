#!/usr/bin/env bash
# Mac 后台拉取的端到端验收（CARM-299）。和 test-remote-ssh.sh 一样，以当前用户在 127.0.0.1 起一个
# 临时 sshd（不需要 root、不动系统 sshd 和 ~/.ssh），走真实的受限 SSH 导出，用 `agenthome remote pull`
# （与 App 后台拉取同一个 `remote_pull::pull_ssh`）验证：
#   1. 首次拉取分页拉完，合计等于采集端真值；
#   2. 断网（sshd 停掉）时拉取失败、类别为 sync/unreachable，已入账的不变；
#   3. 断网期间采集端新增与修订，恢复后续拉，合计等于真值、不重复；
#   4. 采集端用量库重建（store_id 变了）自动走完整快照，合计等于真值。
#
#   cargo build -p agenthome-cli && script/test-remote-pull.sh [target/debug/agenthome]
set -uo pipefail

BIN=$(cd "$(dirname "${1:-target/debug/agenthome}")" && pwd)/$(basename "${1:-target/debug/agenthome}")
SSHD=${SSHD:-/usr/sbin/sshd}
PORT=${PORT:-22960}
LAB=$(mktemp -d)
trap '[ -f "$LAB/sshd.pid" ] && kill "$(cat "$LAB/sshd.pid")" 2>/dev/null; rm -rf "$LAB"' EXIT
RUNNER=$LAB/runner
MAC=$LAB/mac
AK=$LAB/authorized_keys
FAILS=0

step() { printf '\n=== %s\n' "$*"; }
fail() { echo "!! $*"; FAILS=$((FAILS+1)); }

start_sshd() {
  cat >"$LAB/sshd_config" <<EOF
Port $PORT
ListenAddress 127.0.0.1
HostKey $LAB/hostkeys/ssh_host_ed25519_key
AuthorizedKeysFile $AK
PidFile $LAB/sshd.pid
UsePAM no
StrictModes no
PasswordAuthentication no
KbdInteractiveAuthentication no
EOF
  "$SSHD" -f "$LAB/sshd_config" -E "$LAB/sshd.log"
  sleep 0.5
}
stop_sshd() { kill "$(cat "$LAB/sshd.pid")" 2>/dev/null; rm -f "$LAB/sshd.pid"; sleep 0.5; }

# 采集端用量库的操作：init <条数> / add <起> <止> / revise <起> <止> <输出> / rebuild / truth
runner_db() {
  python3 - "$RUNNER/usage/usage.db" "$@" <<'EOF'
import sqlite3, sys, os
db, cmd, args = sys.argv[1], sys.argv[2], sys.argv[3:]
c = sqlite3.connect(db)
def key(i): return ("%064x" % (i * 7919 + 12345))[-64:]
def next_seq():
    s = int(c.execute("SELECT value FROM meta WHERE key='last_seq'").fetchone()[0]) + 1
    c.execute("UPDATE meta SET value=? WHERE key='last_seq'", (str(s),))
    return s
def add(lo, hi, rev=1, out=300):
    for i in range(lo, hi):
        c.execute("INSERT INTO usage VALUES ('claude-code', ?, ?, ?, 'upsert', 'anthropic',"
                  " 'claude-opus-4-5-20251101', ?, 1, 10, ?, 40000, 100, 0, 0)",
                  (key(i), next_seq(), rev, 1763500000000 + i, out))
if cmd == "init":
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
    c.execute("INSERT INTO meta VALUES ('machine_id', ?), ('last_seq', '0'), ('store_id', ?)",
              (args[1], os.urandom(16).hex()))
    add(0, int(args[0]))
elif cmd == "add":
    add(int(args[0]), int(args[1]))
elif cmd == "revise":
    for i in range(int(args[0]), int(args[1])):
        c.execute("UPDATE usage SET seq=?, revision=revision+1, output_tokens=? WHERE event_key=?",
                  (next_seq(), int(args[2]), key(i)))
elif cmd == "rebuild":
    # 新库：新 store_id，seq 从 1 重新编号，修订从 1 重新开始（同一批事件重新采集）。
    rows = c.execute("SELECT event_key FROM usage ORDER BY seq").fetchall()
    c.execute("UPDATE meta SET value=? WHERE key='store_id'", (os.urandom(16).hex(),))
    c.execute("UPDATE meta SET value='0' WHERE key='last_seq'")
    c.execute("UPDATE usage SET seq = -seq")
    for (k,) in rows:
        c.execute("UPDATE usage SET seq=?, revision=1 WHERE event_key=?", (next_seq(), k))
elif cmd == "truth":
    r = c.execute("SELECT COUNT(*), SUM(requests), SUM(input_tokens), SUM(output_tokens),"
                  " SUM(cache_read_tokens), SUM(cache_write_5m_tokens), SUM(cache_write_1h_tokens),"
                  " SUM(reasoning_tokens) FROM usage WHERE op='upsert'").fetchone()
    print("runner\tevents=%d\trequests=%d\tinput=%d\toutput=%d\tcache_read=%d\tcache_write_5m=%d\tcache_write_1h=%d\treasoning=%d" % r)
c.commit()
EOF
}

# 拉取并与真值逐项比较。
pull_and_compare() {
  local out rc
  out=$("$BIN" remote pull --name runner --data-dir "$MAC" 2>"$LAB/pull.err"); rc=$?
  cat "$LAB/pull.err"
  [ "$rc" -eq 0 ] || { fail "拉取失败"; return; }
  local want; want=$(runner_db truth | cut -f2-)
  local got; got=$(cut -f2- <<<"$out")
  echo "Mac 远端库：$got"
  echo "采集端真值：$want"
  [ "$got" = "$want" ] && echo "（逐项一致）" || fail "合计与采集端真值不一致"
}

mkdir -p "$RUNNER/usage" "$MAC" "$LAB/hostkeys"
MID=5b1f0c2e-8d3a-4c7b-9e21-0a6f4d2c9b11
printf '{"schema":1,"machine_id":"%s","session_label":"runner","aliases":[],"event_key_secret":"%s"}' \
  "$MID" "$(openssl rand -hex 32)" >"$RUNNER/device.json"
printf '{"schema":1,"machine_id":"%s","session_label":"mac","aliases":[],"event_key_secret":"%s"}' \
  "7d2e4a10-3b5c-4f6e-8a9b-1c2d3e4f5a6b" "$(openssl rand -hex 32)" >"$MAC/device.json"
runner_db init 1500 "$MID"
ssh-keygen -q -t ed25519 -N "" -f "$LAB/hostkeys/ssh_host_ed25519_key"
: >"$AK"
start_sshd
FP=$("$BIN" remote host-fingerprint --dir "$LAB/hostkeys" | awk '{print $1}')
"$BIN" remote pair --name runner --host 127.0.0.1 --port "$PORT" --user "$USER" \
  --fingerprint "$FP" --allow 127.0.0.0/8 --data-dir "$MAC" >/dev/null || fail "配对失败"
"$BIN" remote authorize --name mac --pubkey "$MAC/remote/runner/id_ed25519.pub" --from 127.0.0.1 \
  --authorized-keys "$AK" --data-dir "$RUNNER" --exe "$BIN" >/dev/null || fail "授权失败"

step "1. 首次拉取：1500 条分页拉完"
pull_and_compare

step "2. 断网（停掉 sshd）：拉取失败，类别 sync/unreachable，已入账的不变"
stop_sshd
runner_db add 1500 1700
runner_db revise 0 10 999
out=$("$BIN" remote pull --name runner --data-dir "$MAC" 2>&1); rc=$?
echo "$out (exit=$rc)"
{ [ "$rc" -ne 0 ] && grep -q "sync/unreachable" <<<"$out"; } || fail "应以 sync/unreachable 失败"

step "3. 网络恢复：续拉断网期间的 200 条新增与 10 条修订，只计一次"
start_sshd
pull_and_compare
step "3b. 再拉一次：没有新变更，合计不变"
pull_and_compare

step "4. 采集端用量库重建（store_id 变了）：自动完整快照"
runner_db rebuild
runner_db add 1700 1750
pull_and_compare
grep -q "完整快照 1" "$LAB/pull.err" || fail "应走一次完整快照"

step "结果"
if [ "$FAILS" -eq 0 ]; then echo "全部通过"; else echo "失败 $FAILS 项"; exit 1; fi
