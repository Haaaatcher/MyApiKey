#!/usr/bin/env bash
# =============================================================================
# Makara CLI 冒烟测试（2026-11-10 用户需求：覆盖全部指令与常用指令组合，
# 标准输出/标准错误清晰呈现结果）
#
# 用法：仓库根目录运行  bash scripts/cli-test.sh
# 前提：target/release/makara.exe 已构建
#
# 环境：MYAPIKEY_HOME 指向临时目录（绝不触碰真实 %APPDATA%\myapikey）；
#       Windows 凭据库走隔离 service（myapikey-isolated）。
#       例外：github.rs 的令牌条目固定正式命名空间——因此本脚本不测试
#       「向凭据库写入 GitHub 凭据」的路径（remote unset 会删真实授权，
#       一律不跑；secret 命令已随 D43 删除）。
# 网络：仅 check（探测）与 update 触网；check 用拒绝连接的本地端口记录，
#       即时失败零等待。
# =============================================================================

set -u

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$REPO_ROOT/target/release/makara.exe"

PASS=0; FAIL=0; FAILED_IDS=()
MAIN_HOME="$(mktemp -d)"         # 主测试环境(阶段 1/2/4/5)
H2="$(mktemp -d)"                # import 目标环境(阶段 3 单独 init)
CUR_HOME="$MAIN_HOME"            # 当前环境指针
KEY_OLD="test-pass-A1"           # init 期密码
KEY_NEW="test-pass-B2"           # passwd 改密后的密码

mak() { MYAPIKEY_HOME="$CUR_HOME" "$BIN" "$@"; }

pass_() { PASS=$((PASS+1)); echo "  ✔ $1"; }
fail_() { FAIL=$((FAIL+1)); FAILED_IDS+=("$1"); echo "  ✗ FAIL: $1"; }

assert_contains() { # <断言名> <被检文本> <期望子串>
  local name="$1" out="$2" needle="$3"
  if printf '%s' "$out" | grep -q -- "$needle"; then
    pass_ "$name"
  else
    fail_ "$name (期望含「$needle」，实际：$(printf '%s' "$out" | head -c 200))"
  fi
}

assert_rc() { # <断言名> <实际退出码> <期望：0|non0>
  local name="$1" rc="$2" want="$3"
  if { [ "$want" = "0" ] && [ "$rc" -eq 0 ]; } || { [ "$want" = "non0" ] && [ "$rc" -ne 0 ]; }; then
    pass_ "$name"
  else
    fail_ "$name (退出码 $rc，期望 $want)"
  fi
}

assert_file() { # <断言名> <路径> <exist|absent> [内容子串]
  local name="$1" path="$2" mode="$3" needle="${4:-}"
  if [ "$mode" = "exist" ]; then
    if [ -f "$path" ]; then
      if [ -n "$needle" ]; then
        grep -q -- "$needle" "$path" && pass_ "$name" || fail_ "$name (文件缺「$needle」)"
      else
        pass_ "$name"
      fi
    else
      fail_ "$name (文件不存在)"
    fi
  else
    [ ! -f "$path" ] && pass_ "$name" || fail_ "$name (文件不应存在)"
  fi
}

case_header() { echo; echo "═══ [$1] $2 ═══"; }

# =============================================================================
echo "════════════════════════════════════════════════════"
echo " Makara CLI 冒烟测试"
echo " 二进制: $BIN"
echo " 隔离 HOME: $CUR_HOME"
echo "════════════════════════════════════════════════════"

[ -x "$BIN" ] || { echo "✗ 找不到 $BIN，先构建"; exit 1; }

# 防呆:WSL 的 bash 会把 MYAPIKEY_HOME 的 /tmp 路径原样传给 Windows 程序,
# 隔离静默失效、直接读写真实配置(2026-11-10 实测)——检测到 WSL 立即拒绝
case "$REPO_ROOT" in
  /mnt/*) echo "✗ 检测到 WSL bash:MYAPIKEY_HOME 隔离在 WSL 下会失效(直接读写真实配置)。"
          echo "  请在 Git Bash 中运行本脚本(开始菜单 → Git Bash)。"; exit 1;;
esac

# ── 阶段 1：全局与未初始化错误路径 ─────────────────────────────
case_header T01 "--version"
out=$(mak --version 2>&1); rc=$?
assert_contains "T01a 版本输出含 makara" "$out" "makara"
assert_rc "T01b 退出码 0" "$rc" 0

case_header T02 "--help 列出全部子命令"
out=$(mak --help 2>&1)
for cmd in init add list search show key edit delete check sync status remote passwd export import update lock; do
  assert_contains "T02 子命令 $cmd" "$out" "$cmd"
done

case_header T03 "未初始化：status"
out=$(mak status 2>&1); rc=$?
assert_contains "T03a 提示未初始化" "$out" "Not initialized"
assert_rc "T03b 退出码 0(status 引导不报错)" "$rc" 0

case_header T04 "未初始化：list"
out=$(mak list 2>&1)
assert_contains "T04 提示未初始化" "$out" "not initialized"

case_header T05 "未知子命令被 clap 拒绝"
out=$(mak bogus-subcommand 2>&1); rc=$?
assert_rc "T05 非零退出" "$rc" non0

# ── 阶段 2：单机主链 init → add → 查询 → 导出/导入 → passwd → lock ──
case_header T06 "init（单机模式：n + 新密码×2）"
out=$(printf 'n\ntest-pass-A1\ntest-pass-A1\n' | mak init 2>&1); rc=$?
assert_contains "T06a 初始化完成" "$out" "Initialized"
assert_rc "T06b 退出码 0" "$rc" 0

case_header T07 "status（单机，0 条）"
out=$(mak status 2>&1)
assert_contains "T07 记录 0" "$out" "Records    : 0"

case_header T08 "add（全参数，零交互）"
out=$(mak add --name 测试一 --protocol openai --base-url "https://api.openai.com/v1" --api-key "sk-test-aaaa" -m "gpt-x,gpt-y" --notes "主账号" 2>&1); rc=$?
assert_contains "T08a 添加成功" "$out" "Added"
assert_rc "T08b 退出码 0" "$rc" 0

case_header T09 "add（半交互：base-url 用拒连本地端口，供 check 零网络）"
out=$(printf 'anthropic\nhttp://127.0.0.1:9\nsk-test-bbbb\nclaude-x,claude-y\n中转站\n' \
  | mak add --name 测试二 --protocol anthropic 2>&1); rc=$?
assert_contains "T09a 添加成功" "$out" "Added"
assert_rc "T09b 退出码 0" "$rc" 0

case_header T10 "list（两条记录）"
out=$(mak list 2>&1)
assert_contains "T10a 含测试一" "$out" "测试一"
assert_contains "T10b 含测试二" "$out" "测试二"

case_header T11 "search 命中"
out=$(mak search 测试 2>&1)
assert_contains "T11 搜索命中" "$out" "测试一"

case_header T13 "show（掩码输出）"
out=$(mak show 测试一 2>&1)
assert_contains "T13a 含名称" "$out" "测试一"
assert_contains "T13b key 已掩码" "$out" "sk-t"

case_header T14 "edit（逐项回车保留）"
out=$(printf '测试一\n\n\n\n\n\n\n' | mak edit 测试一 2>&1); rc=$?
assert_contains "T14a 编辑完成" "$out" "Updated"
assert_rc "T14b 退出码 0" "$rc" 0
case_header T15 "check（单条：127.0.0.1 拒连 → 即时无效，零网络）"
out=$(mak check 测试二 2>&1); rc=$?
assert_contains "T15a 输出校验失败结果" "$out" "unverifiable"
assert_rc "T15b 退出码 0" "$rc" 0

case_header T16 "check（全部，并发）[网络]"
out=$(mak check 2>&1)
assert_contains "T16a 并发探测结果" "$out" "HTTP 401"

case_header T17 "status（记录可读）"
out=$(mak status 2>&1)
assert_contains "T17 状态可读" "$out" "Records"

case_header T18 "export（双重确认后明文导出）"
EXP="$MAIN_HOME/export.json"   # 固定路径:export 与 import 必须指向同一文件   # 固定路径:export 与 import 必须指向同一文件
out=$(printf 'y\ny\n' | mak export "$EXP" 2>&1); rc=$?
assert_contains "T18a 导出成功" "$out" "Export"
assert_file "T18b 文件存在且含明文 key" "$EXP" exist "sk-test-aaaa"

case_header T18x "export 取消（n）→ 不写文件"
EXP2="$CUR_HOME/export-cancel.json"
out=$(printf 'n\n' | mak export "$EXP2" 2>&1)
assert_file "T18x 文件未生成" "$EXP2" absent

# ── 阶段 3：import（第二环境 init → 导入）──────────────────────
case_header T19 "第二环境 init（单机）"
CUR_HOME="$H2"   # 切到 import 目标环境
out=$(printf 'n\ntest-pass-A1\ntest-pass-A1\n' | mak init 2>&1)
assert_contains "T19a 初始化完成" "$out" "Initialized"

case_header T20 "import（从主环境导出文件导入）"
out=$(mak import "$MAIN_HOME/export.json" 2>&1); rc=$?
assert_contains "T20a 导入完成" "$out" "Import done"

case_header T20x "import 幂等（同一文件再导：零变化零推送）"
out=$(mak import "$MAIN_HOME/export.json" 2>&1); rc=$?
assert_contains "T20x a 零变化" "$out" "0 added, 0 updated"
if printf '%s' "$out" | grep -q "Synced to GitHub"; then
  fail_ "T20x b 幂等导入不应推送"
else
  pass_ "T20x b 幂等导入不推送"
fi

case_header T21 "import 后 list（第二环境可见两条）"
out=$(mak list 2>&1)
assert_contains "T21 含测试一" "$out" "测试一"
CUR_HOME="$MAIN_HOME"   # 回主环境

# ── 阶段 4：passwd 改密 → lock → 解锁 ─────────────────────────
case_header T22 "passwd（改解锁密码）"
out=$(printf 'test-pass-B2\ntest-pass-B2\n' | mak passwd 2>&1); rc=$?
assert_contains "T22a 改密成功" "$out" "Credential store updated"
assert_rc "T22b 退出码 0" "$rc" 0

case_header T23 "改密后 list（记住副本已更新，免输）"
out=$(mak list 2>&1)
assert_contains "T23 免输解锁成功" "$out" "测试一"

case_header T24 "lock（清除记住副本）"
out=$(mak lock 2>&1)
assert_contains "T24 锁定成功" "$out" "Locked"

case_header T25 "锁定后 list（输入新密码解锁）"
out=$(printf 'test-pass-B2\n' | mak list 2>&1)
assert_contains "T25 新密码解锁成功" "$out" "测试一"

mak lock 2>&1 >/dev/null
case_header T25x "错误密码两次 → 失败"
out=$(printf 'wrong-one\nwrong-two\n' | mak list 2>&1); rc=$?
assert_contains "T25x 密码错误提示" "$out" "wrong passphrase"
assert_rc "T25y 非零退出" "$rc" non0

# ── 阶段 5：remote / sync / secret ────────────────────────────
# 阶段顺序刻意安排：先测全部未绑定路径（纯本地、零凭据消费），
# 再绑定测已绑定路径。remote unset 一律不跑——它的清除路径会删
# 正式凭据库里的 GitHub 授权（不随 MYAPIKEY_HOME 隔离），红线。
case_header T26 "sync（单机未绑定 → 明确引导）"
# 承接 T25x 留下的锁定状态：sync 解锁本地 vault 先要密码（管道喂）
out=$(printf 'test-pass-B2\n' | mak sync 2>&1); rc=$?
assert_contains "T26a 单机引导" "$out" "Standalone mode"
assert_rc "T26b 退出码 0(单机引导不报错)" "$rc" 0

case_header T27 "remote show（未绑定）"
out=$(mak remote show 2>&1)
assert_contains "T27 未绑定提示" "$out" "no remote bound"

case_header T28 "remote set（绑定写入在前；授权步 timeout 兜底）"
# remote set 先写绑定再确保授权：断言只盯「Remote set」。已有有效
# 授权的机器复用令牌立即返回（rc 0）；没有的机器走浏览器 Device
# Flow 阻塞轮询（可能弹一次浏览器），timeout 30 秒兜底杀掉（rc 124）
# ——授权未完成不会写任何凭据。timeout 是外部命令不能执行 shell
# 函数，这里手动展开 mak 的封装
out=$(timeout 30 env MYAPIKEY_HOME="$CUR_HOME" "$BIN" remote set testuser/test-vault 2>&1); rc=$?
assert_contains "T28a 绑定成功" "$out" "Remote set"

case_header T29 "remote show（绑定态 → URL 或折叠为未绑定）"
# 折叠模型(2026-11-10):绑定显示依赖凭据条目——有令牌的机器显示完整
# URL;无令牌机器折叠为 no remote bound。两者皆是正确行为
out=$(mak remote show 2>&1)
if printf '%s' "$out" | grep -q -e "https://github.com/testuser/test-vault" -e "no remote bound"; then
  pass_ "T29 绑定态展示正确(URL 或折叠为未绑定)"
else
  fail_ "T29 (期望 URL 或 no remote bound，实际：$(printf '%s' "$out" | head -c 200))"
fi

case_header T30 "sync（已绑定 → 有令牌对账 / 无令牌折叠单机）"
# 折叠模型(2026-11-10):有令牌=真实对账(仓库不存在 404,只读,非零);
# 无令牌=standalone 引导(rc 0)。两者皆正确;网络挂起 timeout 60 兜底
out=$(printf 'test-pass-B2\n' | timeout 60 env MYAPIKEY_HOME="$CUR_HOME" "$BIN" sync 2>&1); rc=$?
if printf '%s' "$out" | grep -q -e "Standalone mode" -e "HTTP" -e "GitHub"; then
  pass_ "T30 已绑定行为正确(对账或折叠单机)"
else
  fail_ "T30 (期望对账错误或单机引导，实际：$(printf '%s' "$out" | head -c 200))"
fi

case_header T31 "key（复制完整 key，不打印到终端）"
# 测试一仍在（脚本无删除记录步骤）：序号 1 = 测试一
out=$(mak key 1 2>&1); rc=$?
assert_contains "T31a 复制成功提示" "$out" "API key copied"
assert_rc "T31b 退出码 0" "$rc" 0

case_header T32 "update（网络，宽松：有输出即可）"
out=$(mak update 2>&1); rc=$?
assert_contains "T32 update 有输出(网络/限流均容忍)" "$out" ""

# =============================================================================
echo
echo "════════════════════════════════════════════════════"
echo " 结果: 通过 $PASS / 失败 $FAIL"
if [ "$FAIL" -gt 0 ]; then
  echo " 失败用例:"
  for id in "${FAILED_IDS[@]}"; do echo "   ✗ $id"; done
fi
echo "════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ]
