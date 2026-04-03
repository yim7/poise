#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

CONFIG_PATH="${POISE_CONFIG_PATH:-configs/binance-testnet.local.toml}"
LOG_DIR="${POISE_LOG_DIR:-.logs/paper}"
LOG_PATH="${POISE_SERVER_LOG:-${LOG_DIR}/poise-server.log}"
REBUILD_STATE="${POISE_REBUILD_STATE:-0}"
DRY_RUN=0

usage() {
  cat <<'EOF'
用法:
  scripts/run-paper-server.sh [--dry-run]

环境变量:
  POISE_CONFIG_PATH   服务端配置文件，默认 configs/binance-testnet.local.toml
  POISE_LOG_DIR       日志目录，默认 .logs/paper
  POISE_SERVER_LOG    服务端日志文件，默认 .logs/paper/poise-server.log
  POISE_REBUILD_STATE 设为 1 时，启动时追加 --rebuild-state
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

if [[ "$DRY_RUN" -ne 1 && ! -f "$CONFIG_PATH" ]]; then
  echo "config file not found: $CONFIG_PATH" >&2
  echo "先复制 configs/binance-testnet.demo.toml 到本地 *.local.toml，再填入测试网凭证。" >&2
  exit 1
fi

if [[ "$REBUILD_STATE" != "0" && "$REBUILD_STATE" != "1" ]]; then
  echo "POISE_REBUILD_STATE must be 0 or 1" >&2
  exit 1
fi

SERVER_ARGS=(--config "$CONFIG_PATH")
if [[ "$REBUILD_STATE" == "1" ]]; then
  SERVER_ARGS+=(--rebuild-state)
fi

if [[ "$DRY_RUN" -eq 1 ]]; then
  cat <<EOF
repo_root=$REPO_ROOT
config_path=$CONFIG_PATH
log_path=$LOG_PATH
rebuild_state=$REBUILD_STATE
command=cargo run -p poise-server -- ${SERVER_ARGS[*]}
EOF
  exit 0
fi

echo "[$(date '+%Y-%m-%d %H:%M:%S')] starting poise-server with $CONFIG_PATH rebuild_state=$REBUILD_STATE" | tee -a "$LOG_PATH"
cargo run -p poise-server -- "${SERVER_ARGS[@]}" 2>&1 | tee -a "$LOG_PATH"
