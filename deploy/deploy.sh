#!/usr/bin/env bash
# 将当前 git HEAD 构建并升级到远程 Linux 服务器（不动 poise.toml 与数据库）。
#
# 用法：
#   deploy/deploy.sh <ssh-host> [testnet|mainnet|all|none]
# 示例：
#   deploy/deploy.sh aws-quant-lab testnet   # 升级程序与前端并只重启测试网
#   deploy/deploy.sh aws-quant-lab all       # 升级程序与前端并重启测试网与正式网
#   deploy/deploy.sh aws-quant-lab none      # 只上传并切换软链，不重启服务
set -euo pipefail

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  echo "用法: $0 <ssh-host> [testnet|mainnet|all|none]" >&2
  exit 1
fi

HOST="$1"
TARGET="${2:-testnet}"
case "$TARGET" in
  testnet|mainnet|all|none) ;;
  *)
    echo "未知目标: $TARGET（可选：testnet | mainnet | all | none）" >&2
    exit 1
    ;;
esac

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

if [ -z "${POISE_DEPLOY_DIRTY:-}" ]; then
  DIRTY="$(git status --porcelain --untracked-files=no)"
  if [ -n "$DIRTY" ]; then
    echo "工作区有未提交改动，先提交再部署（确要跳过可设 POISE_DEPLOY_DIRTY=1）：" >&2
    echo "$DIRTY" >&2
    exit 1
  fi
fi

SHORT_SHA="$(git rev-parse --short HEAD)"
echo "▶ 部署 poise @ ${SHORT_SHA} 到 ${HOST}（重启目标: ${TARGET}）"

echo "▶ 构建网页控制台"
(cd console && npm run build)

echo "▶ 同步源码与 console/dist"
ssh "$HOST" "mkdir -p ~/poise-src ~/poise/bin ~/poise/console"
git archive --format=tar HEAD | ssh "$HOST" "rm -rf ~/poise-src/cli ~/poise-src/runtime ~/poise-src/strategy ~/poise-src/hyperliquid ~/poise-src/Cargo.toml ~/poise-src/Cargo.lock && tar -xf - -C ~/poise-src"
tar -C console/dist --no-xattrs --no-mac-metadata -cf - . | ssh "$HOST" "rm -rf ~/poise/console.new && mkdir -p ~/poise/console.new && tar -xf - -C ~/poise/console.new && rm -rf ~/poise/console && mv ~/poise/console.new ~/poise/console"

echo "▶ 在服务器编译 release 二进制"
ssh "$HOST" "set -euo pipefail
export PATH=\"\$HOME/.cargo/bin:\$PATH\"
cd ~/poise-src
nice -n 10 cargo build --release -j 2 --bin poise
install -m 755 target/release/poise ~/poise/bin/poise-${SHORT_SHA}
ln -sfn poise-${SHORT_SHA} ~/poise/bin/poise
"

restart_instance() {
  local name="$1"
  local port="$2"
  echo "▶ 校验并重启 poise-${name}.service"
  ssh "$HOST" "set -euo pipefail
~/poise/bin/poise --instance \"\$HOME/.poise/${name}\" check
sudo systemctl restart poise-${name}.service
for i in \$(seq 20); do
  if curl -fsS -m 2 http://127.0.0.1:${port}/api/instance >/dev/null 2>&1; then
    echo \"poise-${name} 已响应 (127.0.0.1:${port})\"
    exit 0
  fi
  sleep 1
done
echo \"20 秒内未等到 poise-${name} 响应，最近日志：\" >&2
sudo journalctl -u poise-${name}.service --no-pager -n 20 >&2 || true
exit 1
"
}

case "$TARGET" in
  testnet)
    restart_instance testnet 8020
    ;;
  mainnet)
    restart_instance mainnet 8021
    ;;
  all)
    restart_instance testnet 8020
    restart_instance mainnet 8021
    ;;
  none)
    echo "▶ 已更新二进制与前端文件，未重启服务"
    ;;
esac

echo "✓ 部署完成（poise-${SHORT_SHA}）"
