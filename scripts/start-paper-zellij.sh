#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
LAYOUT_PATH="${REPO_ROOT}/ops/zellij/poise-paper.kdl"

SESSION_NAME="${POISE_ZELLIJ_SESSION_NAME:-poise-paper}"
CONFIG_PATH="${POISE_CONFIG_PATH:-configs/binance-testnet.local.toml}"
BASE_URL="${POISE_HEALTH_BASE_URL:-http://127.0.0.1:8000}"
LOG_DIR="${POISE_LOG_DIR:-.logs/paper}"
DRY_RUN=0

usage() {
  cat <<'EOF'
用法:
  scripts/start-paper-zellij.sh [--dry-run]

环境变量:
  POISE_ZELLIJ_SESSION_NAME  zellij session 名称，默认 poise-paper
  POISE_CONFIG_PATH          服务端配置文件，默认 configs/binance-testnet.local.toml
  POISE_HEALTH_BASE_URL      健康巡检基地址，默认 http://127.0.0.1:8000
  POISE_LOG_DIR              日志目录，默认 .logs/paper
EOF
}

while (($# > 0)); do
  case "$1" in
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

cd "$REPO_ROOT"
mkdir -p "$LOG_DIR"

if [[ ! -f "$CONFIG_PATH" ]]; then
  echo "config file not found: $CONFIG_PATH" >&2
  echo "先复制 configs/binance-testnet.demo.toml 到本地 *.local.toml，再填入测试网凭证。" >&2
  exit 1
fi

export POISE_CONFIG_PATH="$CONFIG_PATH"
export POISE_HEALTH_BASE_URL="$BASE_URL"
export POISE_LOG_DIR="$LOG_DIR"
export POISE_SERVER_LOG="${POISE_SERVER_LOG:-${LOG_DIR}/poise-server.log}"
export POISE_HEALTH_LOG="${POISE_HEALTH_LOG:-${LOG_DIR}/health-probe.log}"

if [[ "$DRY_RUN" -eq 1 ]]; then
  cat <<EOF
repo_root=$REPO_ROOT
layout_path=$LAYOUT_PATH
session_name=$SESSION_NAME
config_path=$POISE_CONFIG_PATH
health_base_url=$POISE_HEALTH_BASE_URL
log_dir=$POISE_LOG_DIR
create_command=zellij attach --create-background $SESSION_NAME options --default-layout $LAYOUT_PATH
attach_command=zellij attach $SESSION_NAME
EOF
  exit 0
fi

if ! command -v zellij >/dev/null 2>&1; then
  echo "zellij is not installed or not in PATH" >&2
  exit 1
fi

if zellij attach "$SESSION_NAME" 2>/dev/null; then
  exit 0
fi

zellij attach --create-background "$SESSION_NAME" options --default-layout "$LAYOUT_PATH"
exec zellij attach "$SESSION_NAME"
