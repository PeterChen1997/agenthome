#!/usr/bin/env bash
# 把 site/ 增量上传到七牛 bucket `peter-blog` 的 `agenthome/` 前缀下，再刷新 CDN。
# 页面地址：https://blog.peterchen97.cn/agenthome/
#
# 用法：
#   script/deploy-site.sh            上传并刷新
#   script/deploy-site.sh --dry-run  只列出将要上传的 key 和要刷新的 URL，不碰七牛
#
# 安全约束：这个 bucket 同时是博客根目录。所有 key 必须以 `agenthome/` 开头，
# 上传前逐个检查，任何一个不符合就整体中止，绝不覆盖博客根目录的文件。
set -euo pipefail

QSHELL="${QSHELL:-$HOME/Downloads/qshell}"
BUCKET="peter-blog"
KEY_PREFIX="agenthome/"
SITE_URL="https://blog.peterchen97.cn/agenthome/"

root="$(cd "$(dirname "$0")/.." && pwd)"
src="$root/site"

dry_run=false
case "${1:-}" in
  "") ;;
  --dry-run) dry_run=true ;;
  *) echo "用法：$0 [--dry-run]" >&2; exit 2 ;;
esac

# ---------- 前置检查 ----------
# 前缀写死且必须是 agenthome/：防止有人把变量改空后把 site/index.html 传成博客首页。
if [[ "$KEY_PREFIX" != "agenthome/" ]]; then
  echo "中止：KEY_PREFIX 必须是 agenthome/，当前是 '$KEY_PREFIX'" >&2
  exit 1
fi
if [[ "$SITE_URL" != "https://blog.peterchen97.cn/${KEY_PREFIX}" ]]; then
  echo "中止：SITE_URL 与 KEY_PREFIX 不一致：$SITE_URL" >&2
  exit 1
fi
if [[ ! -f "$src/index.html" ]]; then
  echo "中止：找不到 $src/index.html" >&2
  exit 1
fi

# 要上传的文件（相对 site/ 的路径），跳过隐藏文件（.DS_Store 等）。
# 在 site/ 里用相对路径查找：仓库本身可能位于带点的目录下（如 .claude/worktrees/），
# 用绝对路径匹配 '*/.*' 会把所有文件都排除掉。
files=()
while IFS= read -r -d '' file; do
  files+=("${file#./}")
done < <(cd "$src" && find . -type f ! -name '.*' ! -path '*/.*' -print0 | sort -z)

if [[ ${#files[@]} -eq 0 ]]; then
  echo "中止：site/ 里没有文件" >&2
  exit 1
fi

# 页面会部署在子路径下：以 / 开头的站内引用会指到博客根目录，必须用相对路径。
if grep -rnE --include='*.html' '(href|src)="/[^/]' "$src"; then
  echo "中止：上面这些引用用了根路径 /，部署到 /agenthome/ 下会指到别的站，请改成相对路径" >&2
  exit 1
fi

for rel in "${files[@]}"; do
  key="${KEY_PREFIX}${rel}"
  if [[ "$key" != agenthome/?* || "$rel" == /* || "$rel" == *..* ]]; then
    echo "中止：不安全的 key：$key" >&2
    exit 1
  fi
done

# 要刷新的 URL：每个文件一条，外加目录本身（/agenthome 与 /agenthome/ 都可能被缓存）。
refresh_list="$(mktemp -t agenthome-refresh)"
trap 'rm -f "$refresh_list"' EXIT
{
  echo "${SITE_URL%/}"
  echo "$SITE_URL"
  for rel in "${files[@]}"; do
    echo "${SITE_URL}${rel}"
    # 子目录的 index.html 也会以目录地址被访问（如 agenthome/foo/），一并刷新
    if [[ "$rel" == */index.html ]]; then
      echo "${SITE_URL}${rel%index.html}"
    fi
  done
} > "$refresh_list"

echo "==> 将上传到 bucket=${BUCKET}（共 ${#files[@]} 个文件）："
for rel in "${files[@]}"; do
  echo "    ${KEY_PREFIX}${rel}"
done
echo "==> 将刷新的 URL："
sed 's/^/    /' "$refresh_list"

if $dry_run; then
  echo "==> --dry-run：没有上传，也没有刷新。"
  exit 0
fi

if [[ ! -x "$QSHELL" ]]; then
  echo "中止：找不到可执行的 qshell：${QSHELL}（可用环境变量 QSHELL 指定）" >&2
  exit 1
fi

# ---------- 上传 ----------
# --key-prefix 让 site/index.html 变成 agenthome/index.html；
# --check-hash + --rescan-local 只传内容变了的和新增的文件；--overwrite 覆盖同名旧版本（仅限 agenthome/ 下）。
echo "==> 上传到七牛（增量）..."
"$QSHELL" qupload2 \
  --src-dir="$src" \
  --bucket="$BUCKET" \
  --key-prefix="$KEY_PREFIX" \
  --overwrite \
  --check-hash \
  --rescan-local \
  --skip-fixed-strings=".DS_Store" \
  --thread-count 5

# ---------- 刷新 CDN ----------
echo "==> 刷新 CDN..."
"$QSHELL" cdnrefresh -i "$refresh_list"

echo "==> 完成：$SITE_URL"
