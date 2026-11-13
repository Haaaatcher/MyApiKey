#!/usr/bin/env bash
# 同步工作区源码到 Windows 验证靶机（ssh config 别名 `makara-win`）。
#
# 2026-11-13 起 M5 双平台验证的工作流：源码改动需要 Windows 侧验证时，
# Mac 上跑本脚本一条命令完成同步，再按 AGENTS.md「本地验证」节的
# ssh 命令在靶机跑测试/clippy/构建。
#
# 同步语义：
# - 打包排除构建产物（target/node_modules/dist/gen）——靶机保留自己的
#   target/ 编译缓存与 npm 依赖，同步后增量编译，秒级到分钟级
# - 覆盖式解压 + **孤儿清理**（2026-11-13 审查拍板）：打包同时生成源
#   文件清单（manifest），靶机端由 scripts/clean-orphans.ps1（随包
#   传输）对照清单删除「Mac 已删除而本侧残留」的孤儿文件——清理自动
#   完成，不依赖人工记忆。构建产物/依赖目录整体跳过（target 是编译
#   缓存，误删代价是全量重编）
# - 靶机端顺手清理 macOS tar 产生的 `._*` AppleDouble 与 .DS_Store 垃圾
# - 同步后若改过 ui/package.json / package-lock.json，靶机需重跑 npm ci
set -euo pipefail

HOST="makara-win"
DEST="C:/Users/luohang/Projects"
TGZ="makara-sync.tgz"

ROOT="$(git rev-parse --show-toplevel)"
NAME="$(basename "$ROOT")"

# 源文件清单（与 tar 排除规则一致；target/node_modules/dist/gen 的孤儿
# 由 clean-orphans.ps1 的目录正则排除，不进清单也不被删）
(
  cd "$ROOT"
  find . -type f \
    ! -path './target/*' ! -path '*/node_modules/*' ! -path '*/dist/*' ! -path '*/gen/*' \
    ! -name '.write-probe' ! -name '._*' ! -name '.DS_Store' \
    | sed 's|^\./||' | sort
) > "/tmp/makara-manifest.txt"

tar czf "/tmp/$TGZ" \
  --exclude='target' \
  --exclude='node_modules' \
  --exclude='dist' \
  --exclude='gen' \
  --exclude='.write-probe' \
  -C "$(dirname "$ROOT")" "$NAME"

scp -o BatchMode=yes "/tmp/$TGZ" "$HOST:$DEST/$TGZ"
scp -o BatchMode=yes "/tmp/makara-manifest.txt" "$HOST:$DEST/$NAME/manifest.txt"

ssh -o BatchMode=yes "$HOST" "tar -xzf \"$DEST/$TGZ\" -C \"$DEST\"; Get-ChildItem \"$DEST/$NAME\" -Recurse -Force -Include '._*','.DS_Store' -ErrorAction SilentlyContinue | Remove-Item -Force; Remove-Item \"$DEST/$TGZ\" -ErrorAction SilentlyContinue; powershell -ExecutionPolicy Bypass -File \"$DEST/$NAME/scripts/clean-orphans.ps1\""

echo "✔ 已同步 $NAME → $HOST:$DEST/$NAME"
echo "  靶机验证：ssh $HOST 后按 AGENTS.md「本地验证」节的命令跑 test/clippy/构建"
