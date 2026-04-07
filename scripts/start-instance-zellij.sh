#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
LAYOUT_PATH="${REPO_ROOT}/ops/zellij/poise-instance.kdl"

INSTANCE_DIR="${POISE_INSTANCE_DIR:-}"
BASE_URL="${POISE_BASE_URL:-http://127.0.0.1:8000}"
REBUILD_STATE="${POISE_REBUILD_STATE:-0}"
DRY_RUN=0

usage() {
  cat <<'EOF'
用法:
  scripts/start-instance-zellij.sh [--dry-run]

环境变量:
  POISE_INSTANCE_DIR         实例目录，必须包含 config.toml
  POISE_BASE_URL             实例 HTTP 服务基地址，默认 http://127.0.0.1:8000
  POISE_REBUILD_STATE        设为 1 时，server pane 启动时追加 --rebuild-state
  POISE_ZELLIJ_SESSION_NAME  zellij session 名称，可选；默认从实例目录 basename 推导
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

if [[ -z "$INSTANCE_DIR" ]]; then
  echo "missing required POISE_INSTANCE_DIR" >&2
  usage >&2
  exit 1
fi

CONFIG_PATH="${INSTANCE_DIR}/config.toml"
INSTANCE_NAME="$(basename "$INSTANCE_DIR")"
SESSION_NAME="${POISE_ZELLIJ_SESSION_NAME:-poise-${INSTANCE_NAME}}"
LOG_DIR="${POISE_LOG_DIR:-${INSTANCE_DIR}/.logs}"

mkdir -p "$LOG_DIR"

if [[ "$DRY_RUN" -ne 1 && ! -f "$CONFIG_PATH" ]]; then
  echo "config file not found: $CONFIG_PATH" >&2
  exit 1
fi

if [[ "$REBUILD_STATE" != "0" && "$REBUILD_STATE" != "1" ]]; then
  echo "POISE_REBUILD_STATE must be 0 or 1" >&2
  exit 1
fi

export POISE_INSTANCE_DIR="$INSTANCE_DIR"
export POISE_BASE_URL="$BASE_URL"
export POISE_LOG_DIR="$LOG_DIR"
export POISE_REBUILD_STATE="$REBUILD_STATE"
export POISE_SERVER_LOG="${POISE_SERVER_LOG:-${LOG_DIR}/poise-server.log}"
export POISE_HEALTH_LOG="${POISE_HEALTH_LOG:-${LOG_DIR}/health-probe.log}"
export POISE_TUI_LOG="${POISE_TUI_LOG:-${LOG_DIR}/poise-tui.log}"

if [[ "$DRY_RUN" -eq 1 ]]; then
  cat <<EOF
repo_root=$REPO_ROOT
instance_dir=$POISE_INSTANCE_DIR
layout_path=$LAYOUT_PATH
session_name=$SESSION_NAME
base_url=$POISE_BASE_URL
log_dir=$POISE_LOG_DIR
rebuild_state=$POISE_REBUILD_STATE
create_command=zellij attach --create-background $SESSION_NAME options --default-layout $LAYOUT_PATH
attach_command=zellij attach $SESSION_NAME
EOF
  exit 0
fi

if ! command -v zellij >/dev/null 2>&1; then
  echo "zellij is not installed or not in PATH" >&2
  exit 1
fi

cd "$REPO_ROOT"

if zellij attach "$SESSION_NAME" 2>/dev/null; then
  exit 0
fi

zellij attach --create-background "$SESSION_NAME" options --default-layout "$LAYOUT_PATH"
exec zellij attach "$SESSION_NAME"
