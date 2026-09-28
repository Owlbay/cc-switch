#!/usr/bin/env bash
# 检查代理模块（非测试代码）对 app 其它模块、tauri、rusqlite 的耦合。
#
# 用法（仓库根目录）：
#   scripts/check-proxy-core-coupling.sh                 # 默认检查 src-tauri/src/proxy
#   SCOPE=src-tauri/crates/cc-proxy-core/src scripts/check-proxy-core-coupling.sh
#
# 输出为空且退出码为 0 即通过；有命中时逐行打印 `文件:行:内容` 并以 1 退出。
# 剥离 `#[cfg(test)] mod xxx { … }` 整块以及 `#[cfg(test)]` 修饰的单个 item，
# 折叠跨行 use，再匹配 crate::(?!proxy)、tauri、rusqlite。
# 设计与判据见 docs/standalone-proxy-core-design-zh.md（§8.2、附录 A）。
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
SCOPE=${SCOPE:-src-tauri/src/proxy}
case "$SCOPE" in
  /*) ;;
  *) SCOPE="$ROOT/$SCOPE" ;;
esac

if [ ! -d "$SCOPE" ]; then
  echo "SCOPE 不存在: $SCOPE" >&2
  exit 2
fi

python3 - "$SCOPE" "$ROOT" <<'PY'
import re, sys, pathlib

MOD_RE = re.compile(r'^\s*(pub(\([a-z]+\))?\s+)?mod\s+\w+\s*\{')
ITEM_END_RE = re.compile(r';\s*$')
PATTERN = re.compile(r'\bcrate::(?!proxy\b)|\btauri\b|\brusqlite\b')


def skip_braced(lines, start):
    """从 start 行开始按花括号配对，返回块结束行下标。"""
    depth, j, started, n = 0, start, False, len(lines)
    while j < n:
        depth += lines[j].count('{') - lines[j].count('}')
        started = started or '{' in lines[j]
        if started and depth <= 0:
            break
        j += 1
    return j


def strip_tests(lines):
    out, i, n = [], 0, len(lines)
    while i < n:
        line = lines[i]
        if line.strip() == '#[cfg(test)]' and i + 1 < n:
            nxt = lines[i + 1]
            if not MOD_RE.match(nxt) and ITEM_END_RE.search(nxt) and '{' not in nxt:
                # 以 `;` 结尾的单行 item（use / const 等）
                out.extend(['', ''])
                i += 2
                continue
            # mod 块或带花括号的 item：按花括号配对跳过
            j = skip_braced(lines, i + 1)
            out.extend([''] * (j - i + 1))
            i = j + 1
            continue
        out.append(line)
        i += 1
    return out


def fold_multiline_use(lines):
    """把 `use crate::{` … `};` 折叠成一行，保留起始行号。"""
    out, i, n = [], 0, len(lines)
    while i < n:
        line = lines[i]
        if re.match(r'^\s*(pub(\([a-z]+\))?\s+)?use\b', line) and '{' in line and '}' not in line:
            buf, j = [line], i + 1
            while j < n and '}' not in lines[j]:
                buf.append(lines[j].strip())
                j += 1
            if j < n:
                buf.append(lines[j].strip())
            out.append(' '.join(buf))
            out.extend([''] * (j - i))
            i = j + 1
            continue
        out.append(line)
        i += 1
    return out


scope = pathlib.Path(sys.argv[1])
root = pathlib.Path(sys.argv[2])
hits = 0
for path in sorted(scope.rglob('*.rs')):
    lines = path.read_text(encoding='utf-8').split('\n')
    lines = fold_multiline_use(strip_tests(lines))
    for idx, line in enumerate(lines, 1):
        code = line.split('//', 1)[0]  # 去掉行尾注释；文档注释 /// 一并忽略
        if PATTERN.search(code):
            print(f'{path.relative_to(root)}:{idx}:{line.strip()}')
            hits += 1
sys.exit(1 if hits else 0)
PY
