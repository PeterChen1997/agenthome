#!/usr/bin/env bash
# 发版后把落地页 site/ 上的版本号和下载文件名改成新版本，并核对 GitHub Release 确实有这个安装包。
# 每日发版巡检在 script/release-macos.sh 发完之后直接调它；任何一步不对都非零退出。
#
#   script/update-landing.sh <版本，如 0.2.6> [--dry-run]
#
# 改的地方（英文在 site/index.html，中文在 site/assets/main.js 的 i18n 表）：
#   顶部「v<版本> · open source · macOS」、安装第 1 步的 AgentHome-<版本>-macos-arm64.zip、「Download v<版本>」按钮。
# 下载按钮本身固定指向 releases/latest，不用改。
# --dry-run：只打印会改成什么、做回读，不写文件。
#
# 部署：改完后调 script/deploy-site.sh 传到七牛 peter-blog 桶的 agenthome/ 前缀并刷新 CDN，
# 再回读线上 index.html、assets/main.js 与本地逐字节一致。
set -euo pipefail

REPO="PeterChen1997/agenthome"
FILES=(site/index.html site/assets/main.js)

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
fail() { echo "✗ $*" >&2; exit 1; }

version="${1:-}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "用法：$0 <版本，如 0.2.6> [--dry-run]" >&2; exit 2; }
dry_run=0
case "${2:-}" in
  "") ;;
  --dry-run) dry_run=1 ;;
  *) echo "用法：$0 <版本> [--dry-run]" >&2; exit 2 ;;
esac

echo "==> 核对 GitHub Release v${version}"
assets="$(gh api "repos/$REPO/releases/tags/v$version" -q '.assets[].name')" || fail "GitHub 上没有 v$version 的 Release"
zip="AgentHome-$version-macos-arm64.zip"
grep -qx "$zip" <<<"$assets" || fail "v$version 的 Release 里没有 $zip"
echo "✓ v$version 的 Release 带 $zip"

V='[0-9]+\.[0-9]+\.[0-9]+'
edit='s/v'"$V"' · /v'"$version"' · /g; s/AgentHome-'"$V"'-macos-arm64\.zip/'"$zip"'/g; s/(Download|下载) v'"$V"'/\1 v'"$version"'/g'
echo "==> 更新 ${FILES[*]}"
for f in "${FILES[@]}"; do
  if [[ $dry_run == 1 ]]; then
    diff <(cat "$f") <(sed -E "$edit" "$f") || true
  else
    sed -E -i '' "$edit" "$f"
  fi
done

[[ $dry_run == 1 ]] && { echo "--dry-run：没有写文件"; exit 0; }
# 回读：三处 × 中英两份都换成了新版本，且没有别的版本号残留
hits="$(grep -ohE "v$version · |$zip|(Download|下载) v$version" "${FILES[@]}" | wc -l | tr -d ' ')"
[[ "$hits" == 6 ]] || fail "期望 6 处新版本号，实际 ${hits} 处：模板结构变了，检查 ${FILES[*]}"
stale="$(grep -nE "v$V · |AgentHome-$V-macos|(Download|下载) v$V" "${FILES[@]}" | grep -vF "$version" || true)"
[[ -z "$stale" ]] || fail "还有旧版本号：$stale"
echo "✓ 6 处版本号都是 $version"

echo "==> 部署到七牛"
script/deploy-site.sh
URL="https://blog.peterchen97.cn/agenthome/"
for f in index.html assets/main.js; do
  [[ "$(curl -fsS -m 30 "$URL$f" | shasum)" == "$(shasum <"site/$f")" ]] || fail "线上 $URL$f 与本地不一致（CDN 可能还没刷新，稍后重跑）"
done
echo "✓ 线上 ${URL} 已是 $version（记得提交 site/ 的改动）"
