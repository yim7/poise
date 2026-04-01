#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

BASE_URL="${POISE_BASE_URL:-${POISE_HEALTH_BASE_URL:-http://127.0.0.1:8000}}"
WS_URL="${POISE_TUI_WS_URL:-${POISE_WS_URL:-}}"
LOG_DIR="${POISE_LOG_DIR:-.logs/paper}"
LOG_PATH="${POISE_TUI_LOG:-${LOG_DIR}/poise-tui.log}"
DRY_RUN=0

usage() {
  cat <<'EOF'
用法:
  scripts/run-paper-tui.sh [--dry-run]

环境变量:
  POISE_BASE_URL     TUI 连接的 HTTP 基地址，默认继承 POISE_HEALTH_BASE_URL 或 http://127.0.0.1:8000
  POISE_TUI_WS_URL   TUI 连接的 WebSocket 地址，可选；为空时由 TUI 自己从 base url 推导
  POISE_LOG_DIR      日志目录，默认 .logs/paper
  POISE_TUI_LOG      TUI tracing 日志文件，默认 .logs/paper/poise-tui.log
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

if [[ "$DRY_RUN" -eq 1 ]]; then
  cat <<EOF
repo_root=$REPO_ROOT
base_url=$BASE_URL
ws_url=${WS_URL:-derived}
log_path=$LOG_PATH
command=cargo run -p poise-tui
EOF
  exit 0
fi

export POISE_BASE_URL="$BASE_URL"
export POISE_TUI_LOG="$LOG_PATH"
if [[ -n "$WS_URL" ]]; then
  export POISE_TUI_WS_URL="$WS_URL"
fi

exec cargo run -p poise-tui
