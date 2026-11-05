#!/bin/sh
# AgentHome statusline 包装脚本。
#
# Claude Code 每次 API 响应后把一段 JSON 写进 statusline 命令的 stdin（见 docs/06-statusline-安装.md）。
# 本脚本把这段 JSON 原样落到 ~/AgentHome/signals/claude-statusline.json（原子写：先 .tmp 再 mv），
# 然后把同一段 JSON 转给用户原来的 statusline 命令；没有原命令时输出一行极简状态。
#
# 原 statusline 命令通过环境变量 AGENTHOME_ORIG_STATUSLINE 指定，或直接改下面的 ORIG 变量。

ORIG="${AGENTHOME_ORIG_STATUSLINE:-}"
DIR="${AGENTHOME_SIGNALS_DIR:-$HOME/AgentHome/signals}"
OUT="$DIR/claude-statusline.json"

mkdir -p "$DIR" 2>/dev/null
TMP="$OUT.tmp.$$"
# 把 stdin 读进临时文件；落盘失败不能影响 statusline 本身，所以错误只吞到 /dev/null。
if cat > "$TMP" 2>/dev/null; then
    mv -f "$TMP" "$OUT" 2>/dev/null || rm -f "$TMP"
else
    rm -f "$TMP"
    exit 0
fi

if [ -n "$ORIG" ]; then
    exec sh -c "$ORIG" < "$OUT"
fi

# 极简兜底：模型名 + 5h/7d 已用百分比（没有 jq 时只打印模型名）。
if command -v jq >/dev/null 2>&1; then
    jq -r '
      ((.model.display_name // .model.id // "claude") | tostring) as $m
      | [ (.rate_limits.five_hour.used_percentage? // empty | "5h \(.|floor)%"),
          (.rate_limits.seven_day.used_percentage? // empty | "7d \(.|floor)%") ]
      | if length > 0 then "\($m) · \(join(" · "))" else $m end
    ' < "$OUT" 2>/dev/null || echo "claude"
else
    echo "claude"
fi
