#!/usr/bin/env bash
# 断言代理引擎相关 crate 的常规依赖图中不含 tauri / rusqlite。
#
# 用法（仓库根目录）：scripts/check-proxy-core-deps.sh
#
# 判定：stdout 非空表示存在依赖路径（失败）；配合 -p 且不依赖时 cargo 以 101 退出并提示
# “did not match any packages”（通过）；某些情况下只在 stderr 打印
# “nothing to print” 且退出码为 0（通过）。
# 设计见 docs/standalone-proxy-core-design-zh.md（§3）。
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
MANIFEST="$ROOT/src-tauri/Cargo.toml"

status=0

# 前置检查：被禁止的依赖名必须存在于 workspace 依赖图中（app 依赖它们），
# 否则下面的 “did not match any packages” 会把拼写错误误判为通过。
for dep in tauri rusqlite; do
  if [ -z "$(cargo tree --manifest-path "$MANIFEST" -e normal -i "$dep" --depth 0 2>/dev/null)" ]; then
    echo "workspace 依赖图中找不到 ${dep}，请检查脚本中的依赖名" >&2
    exit 2
  fi
done

for pkg in cc-switch-domain cc-proxy-core; do
  for dep in tauri rusqlite; do
    err=$(mktemp)
    if ! out=$(cargo tree --manifest-path "$MANIFEST" -p "$pkg" -e normal -i "$dep" 2>"$err"); then
      # 配合 -p 时，-i 只在所选包的依赖图里解析；不依赖该 crate 时以 101 退出并提示
      # “did not match any packages”，这正是期望结果。其它失败（包名错误等）不能放行。
      if grep -q "did not match any packages" "$err"; then
        rm -f "$err"
        continue
      fi
      echo "cargo tree 执行失败：-p ${pkg} -i ${dep}" >&2
      cat "$err" >&2
      rm -f "$err"
      status=1
      continue
    fi
    rm -f "$err"
    if [ -n "$out" ]; then
      echo "${pkg} 不允许依赖 ${dep}：" >&2
      echo "$out" >&2
      status=1
    fi
  done
done

if [ "$status" -eq 0 ]; then
  echo "OK: cc-switch-domain / cc-proxy-core 不依赖 tauri、rusqlite"
fi
exit "$status"
