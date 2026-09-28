#!/usr/bin/env bash
# 列出代理模块内被 app 其它模块按名引用的 pub(crate) 项（P3 物理搬迁前需改为 pub 或迁移）。
#
# 用法（仓库根目录）：
#   scripts/check-proxy-core-visibility.sh [PROXY_DIR] [APP_SRC]
#   默认 PROXY_DIR=src-tauri/src/proxy，APP_SRC=src-tauri/src
#
# 输出格式：<item>: <app 侧文件列表>；有命中时以 1 退出。按名匹配会有同名误报，需人工排除
# （基线误报：acquire、extract_reasoning_field_text）。不覆盖 `pub(crate) mod` 声明。
# 设计见 docs/standalone-proxy-core-design-zh.md（§5.8、附录 B）。
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
PROXY_DIR=${1:-src-tauri/src/proxy}
APP_SRC=${2:-src-tauri/src}
PROXY_DIR=${PROXY_DIR%/}

if [ ! -d "$PROXY_DIR" ] || [ ! -d "$APP_SRC" ]; then
  echo "目录不存在: PROXY_DIR=${PROXY_DIR} APP_SRC=${APP_SRC}" >&2
  exit 2
fi

items=$(grep -rhoE 'pub\(crate\) (async )?(fn|struct|enum|const|static|type|trait) [A-Za-z_0-9]+' \
  "$PROXY_DIR" --include='*.rs' | awk '{print $NF}' | sort -u)

hits=0
for it in $items; do
  files=$(grep -rlw "$it" "$APP_SRC" --include='*.rs' | grep -v "^${PROXY_DIR}/" || true)
  if [ -n "$files" ]; then
    echo "${it}: $(echo "$files" | tr '\n' ' ')"
    hits=$((hits + 1))
  fi
done
echo "total: ${hits} item(s)" >&2
[ "$hits" -eq 0 ]
