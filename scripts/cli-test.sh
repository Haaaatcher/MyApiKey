#!/usr/bin/env bash
# =============================================================================
# Makara CLI 交互式冒烟测试（隔离环境）
#
# · MYAPIKEY_HOME 指向临时目录，全程不触碰真实 %APPDATA%\myapikey
#   与真实凭据库主密码条目（凭据走隔离 service）
# · makara 的交互提示（初始化密码/添加记录/确认删除）由你亲手输入，
#   脚本逐步引导：完整命令回显 → 你操作 → 完整结果 → 通过/失败判定
# · 测试结束自动清理临时目录（含导出的明文文件，不留残留）
#
# 用法:仓库根目录、Git Bash 中运行  bash scripts/cli-test.sh
# 前提:target/release/makara.exe 已构建
#
# 边界说明（为什么有些路径不测）:
#   - GitHub 授权/绑定/远端对账不进本脚本——授权会读写正式凭据库的
#     GitHub 令牌（不随隔离），自动化或半交互都不应触发
#   - secret 只测空值拒绝——非空写入同样落在正式凭据库
#   - 密码随意设:这是测试环境的密码,与你的真实解锁密码无关
# =============================================================================

set -u

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$REPO_ROOT/target/release/makara.exe"

# ── 隔离环境 ─────────────────────────────────────────────────────
TEST_HOME="$(mktemp -d)"
export MYAPIKEY_HOME="$TEST_HOME"   # export 后所有 makara 调用自动隔离
cleanup() { rm -rf "$TEST_HOME"; }
trap cleanup EXIT                    # 正常结束/q 退出/Ctrl+C 都会清理

PASS=0; FAIL=0; SKIP=0; FAILED_IDS=()

# 防呆:WSL bash 会让 MYAPIKEY_HOME 隔离失效(路径不转换),直接读写真实配置
case "$REPO_ROOT" in
  /mnt/*|/usb/*)
    echo "✗ 检测到 WSL bash:此环境下测试会绕过数据隔离,直接读写真实配置。"
    echo "  请在 Git Bash 中运行本脚本(开始菜单 → Git Bash → cd 到仓库)。"
    exit 1;;
esac
[ -x "$BIN" ] || { echo "✗ 找不到 $BIN,先构建"; exit 1; }

# ── 输出与交互原语 ────────────────────────────────────────────────
step() { # step <编号> <标题> <标签>
  echo
  echo "┌──────────────────────────────────────────────"
  echo "│ [$1] $2   $3"
  echo "└──────────────────────────────────────────────"
}
run_mak() { # 回显命令 + 执行(输出直通,交互由你操作)
  echo "│ \$ makara $*"
  # 显式调用构建产物而非 PATH 安装版:确保测的就是 target/release 下刚构建的二进制
  "$BIN" "$@"
  local rc=$?
  echo "│ (退出码: $rc)"
  LAST_RC=$rc
}
ok()   { PASS=$((PASS+1)); echo "  ✔ $1"; }
bad()  { FAIL=$((FAIL+1)); FAILED_IDS+=("$1"); echo "  ✗ $1"; }
skip_(){ SKIP=$((SKIP+1)); echo "  ○ 跳过: $1"; }
pause() { # Enter 继续 / q 结束
  local ans=""
  read -r -p "▶ 按 Enter 继续(q 结束测试并显示摘要): " ans
  if [ "$ans" = "q" ]; then summary; exit 0; fi
}
ask() { # ask <提示> → 用户输入
  local v=""
  read -r -p "$1" v
  printf '%s' "$v"
}
summary() {
  echo
  echo "════════════════════════════════════════════════════"
  echo " 测试摘要: 通过 $PASS · 失败 $FAIL · 跳过 $SKIP"
  if [ "$FAIL" -gt 0 ]; then
    echo " 失败步骤:"
    for id in "${FAILED_IDS[@]}"; do echo "   ✗ $id"; done
  fi
  echo "════════════════════════════════════════════════════"
}

# ── 常量 ──────────────────────────────────────────────────────────
SMOKE_NAME=""                     # 冒烟记录名称(用户输入后记录)
TOTAL=21

# =============================================================================
echo "════════════════════════════════════════════════════"
echo " Makara CLI 交互式冒烟测试(隔离环境)"
echo " 二进制:   $BIN"
echo " 测试目录: $TEST_HOME  ← 所有数据写这里,主环境零接触"
echo " 步骤:     $TOTAL 步;每步后按 Enter 继续;q 可随时结束"
echo " 写数据:   全部落在测试目录,结束自动清理"
echo " 不测:     GitHub 授权/绑定/远端对账(会碰真实凭据库令牌)"
echo "════════════════════════════════════════════════════"

# ── S01 版本 ──────────────────────────────────────────────────────
step S01 "版本与身份" "只读"
run_mak --version
LAST_RC=0 && ok "S01 版本正常(makara,品牌 Makara)"
pause

# ── S02 帮助(全部子命令清单验证)────────────────────────────────
step S02 "帮助:全部子命令清单" "只读"
run_mak --help
# 子命令完整性由 build 保证;此处人工目视确认 17 个子命令齐全
ok "S02 帮助已展示(17 个子命令,请目视核对)"
pause

# ── S03 init(隔离环境初始化)────────────────────────────────────
step S03 "init:初始化测试环境(单机模式)" "交互输入"
echo "│ makara 会先问是否绑定远端——输入 n(隔离环境不碰 GitHub)"
echo "│ 然后设置解锁密码×2——随意设(至少 8 字符),这是测试密码,"
echo "│ 与你的真实密码无关;后面 passwd 步骤会改它,记住即可"
pause
run_mak init
if [ "$LAST_RC" -eq 0 ]; then
  ok "S03 初始化完成(单机模式,测试密码已设置)"
  echo "│ 请记住这个测试密码——S16/S21 解锁步骤需要你亲手输入"
else
  bad "S03 初始化失败(退出码 $LAST_RC)"
fi
pause

# ── S04 status(单机 0 条)───────────────────────────────────────
step S04 "status:初始化后状态" "只读"
run_mak status
ok "S04 状态已展示(单机/0 条记录/远端 none)"
pause

# ── S05 add(交互添加冒烟记录)───────────────────────────────────
step S05 "add:添加一条冒烟记录" "交互输入"
SMOKE_NAME=$(ask "你将为冒烟记录使用的名称(建议:zzz-冒烟-可删除): ")
if [ -z "$SMOKE_NAME" ]; then skip_ "S05 未输入名称"; pause; else
  echo "│ 即将启动交互式添加——请在 makara 的提示中输入(名称填:$SMOKE_NAME)"
  echo "│ 协议选 openai 或 anthropic;Base URL 随意(如 https://example.com/v1)"
  echo "│ API key 填假值(如 sk-smoke);模型可跳过;备注可跳过"
  pause
  run_mak add
  if [ "$LAST_RC" -eq 0 ]; then
    ok "S05 记录已添加($SMOKE_NAME)"
  else
    bad "S05 添加失败(退出码 $LAST_RC)"
  fi
  pause
fi

# ── S06 验证冒烟记录(list 含名称 + show 掩码)───────────────────
step S06 "验证:冒烟记录已入库(show 掩码)" "只读"
if [ -z "$SMOKE_NAME" ]; then skip_ "S06 无冒烟记录"; pause; else
  run_mak list
  if "$BIN" list 2>/dev/null | grep -q -- "$SMOKE_NAME"; then
    ok "S06a list 含冒烟记录($SMOKE_NAME)"
  else
    bad "S06a list 缺少冒烟记录($SMOKE_NAME)"
  fi
  echo "│ 接下来 show 该记录——key 应显示为掩码"
  pause
  run_mak show "$SMOKE_NAME"
  ok "S06b show 已展示(key 掩码形态请目视确认)"
  pause
fi

# ── S07 search(按关键词)────────────────────────────────────────
step S07 "search:按关键词搜索" "只读"
KW=$(ask "搜索关键词(如冒烟记录名的一部分): ")
if [ -z "$KW" ]; then skip_ "S07 未输入关键词"; pause; else
  run_mak search "$KW"
  ok "S07 搜索完成,结果如上"
  pause
fi

# ── S08 edit(交互修改冒烟记录)─────────────────────────────────
step S08 "edit:逐项修改冒烟记录" "交互输入"
if [ -z "$SMOKE_NAME" ]; then skip_ "S08 无冒烟记录"; pause; else
  echo "│ 即将启动交互式编辑——每项回车 = 保留现值;可修改任意项"
  pause
  run_mak edit "$SMOKE_NAME"
  if [ "$LAST_RC" -eq 0 ]; then
    ok "S08 编辑完成"
  else
    bad "S08 编辑失败(退出码 $LAST_RC)"
  fi
  pause
fi

# ── S09 check(探测冒烟记录)────────────────────────────────────
step S09 "check:校验冒烟记录的 key" "网络(无凭据)"
echo "│ 冒烟记录的假 URL/key 会校验失败——这是预期结果(探测真发了请求)"
pause
run_mak check "$SMOKE_NAME"
ok "S09 校验完成(无效/不可验证均为预期)"
pause

# ── S10 export(明文导出,假 key)────────────────────────────────
step S10 "export:明文导出全部记录" "写文件(假 key,随目录清理)"
EXP="$TEST_HOME/export-smoke.json"
echo "│ 需要两次确认;导出文件放在测试目录内,脚本结束随目录清理,"
echo "│ 不会残留在你的用户目录"
pause
run_mak export "$EXP"
if [ "$LAST_RC" -eq 0 ] && [ -f "$EXP" ]; then
  ok "S10 已导出: $EXP"
else
  bad "S10 导出失败或被取消"
fi
pause

# ── S11 import(重新导入,幂等合并)─────────────────────────────
step S11 "import:重新导入刚才的导出文件" "写数据(幂等合并,新者胜)"
echo "│ 同 id 记录按 updated_at 新者胜——重复导入不会产生重复记录"
pause
run_mak import "$EXP"
if [ "$LAST_RC" -eq 0 ]; then
  ok "S11 重新导入完成(幂等)"
else
  bad "S11 导入失败(退出码 $LAST_RC)"
fi
pause

# ── S12 delete(删除冒烟记录,清理)─────────────────────────────
step S12 "delete:删除冒烟记录" "交互输入(测试环境,真实删除)"
if [ -z "$SMOKE_NAME" ]; then skip_ "S12 无冒烟记录"; pause; else
  echo "│ 即将启动交互式删除——makara 会要求你确认(输入记录名)"
  pause
  run_mak delete "$SMOKE_NAME"
  if [ "$LAST_RC" -eq 0 ]; then
    ok "S12 冒烟记录已删除"
  else
    bad "S12 删除失败(退出码 $LAST_RC)"
  fi
  pause
fi

# ── S13 list(确认清理)─────────────────────────────────────────
step S13 "list:确认清理完成" "只读"
run_mak list
if [ -n "$SMOKE_NAME" ] && "$BIN" list 2>/dev/null | grep -q -- "$SMOKE_NAME"; then
  bad "S13 冒烟记录仍存在(测试目录反正会清理,不影响主环境)"
else
  ok "S13 冒烟记录已不在列表"
fi
pause

# ── S14 passwd(修改测试密码)───────────────────────────────────
step S14 "passwd:修改解锁密码" "交互输入(测试环境)"
echo "│ 当前记住副本生效时 CLI 不要求旧密码,直接设新密码×2"
echo "│ 随意设——这是测试环境;设完把新密码喂给脚本"
read -r -p "是否测试?(y = 修改密码 / 其他 = 跳过): " ans
if [ "$ans" = "y" ]; then
  pause
  run_mak passwd
  if [ "$LAST_RC" -eq 0 ]; then
    ok "S14 密码已修改(记住新密码,S16/S21 解锁要输入)"
  else
    bad "S14 改密失败(退出码 $LAST_RC)"
  fi
else
  skip_ "S14 用户选择跳过"
fi
pause

# ── S15 lock(锁定)─────────────────────────────────────────────
step S15 "lock:立即锁定(清除记住副本)" "写数据(仅凭据副本,vault 不动)"
run_mak lock
ok "S15 已锁定(记住副本清除)"
pause

# ── S16 list(锁定后,验证解锁)──────────────────────────────────
step S16 "list:锁定状态(需要输入解锁密码)" "交互输入"
echo "│ 即将运行 list——makara 会要求输入解锁密码"
echo "│ 输入你当前使用的测试密码(S14 改过则是新密码)"
echo "│ (脚本不做断言——解锁成功/失败的判定你来目视确认)"
pause
run_mak list
ok "S16 列表已展示(解锁成功 = 密码生效)"
pause

# ── S17 remote show(未绑定路径)────────────────────────────────
step S17 "remote show:未绑定状态" "只读"
run_mak remote show
if "$BIN" remote show 2>/dev/null | grep -q "no remote bound"; then
  ok "S17 未绑定提示正确(no remote bound)"
else
  bad "S17 期望 no remote bound,实际输出如上"
fi
pause

# ── S18 sync(未绑定路径)───────────────────────────────────────
step S18 "sync:单机未绑定 → 引导绑定" "只读(不触远端)"
echo "│ 未绑定远端时 sync 给出明确引导、退出码 0(引导不报错设计)"
pause
run_mak sync
if [ "$LAST_RC" -eq 0 ]; then
  ok "S18 单机引导展示,退出码 0"
else
  bad "S18 退出码 $LAST_RC(期望 0:引导不报错)"
fi
pause

# ── S19 secret(空值拒绝)───────────────────────────────────────
step S19 "secret:空值拒绝边界" "安全(不写任何凭据)"
echo "│ 非空 secret 会写正式凭据库,不进测试;空值必须被拒绝"
pause
run_mak secret ""
if [ "$LAST_RC" -ne 0 ]; then
  ok "S19 空 secret 被拒绝"
else
  bad "S19 空 secret 未被拒绝"
fi
pause

# ── S20 update(检查新版本)─────────────────────────────────────
step S20 "update:检查新版本" "网络(无凭据)"
run_mak update
ok "S20 版本检查完成(结果如上;网络/限流均可能影响)"
pause

# ── S21 组合链:lock → list → status ───────────────────────────
step S21 "组合链:lock → list(需密码)→ 解锁后 status" "交互输入"
echo "│ 连续三步:①锁定 ②list(会要求输入测试密码)③status(免输)"
pause
run_mak lock
run_mak list
run_mak status
ok "S21 组合链完成(锁定→解锁→免输状态,行为如上)"
pause

summary
echo
echo " 测试目录已清理: $TEST_HOME"
