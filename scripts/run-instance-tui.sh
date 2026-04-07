#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/lib/poise-instance.sh"
REPO_ROOT="$(poise_repo_root_from_script_dir "$SCRIPT_DIR")"

RAW_INSTANCE_DIR="${POISE_INSTANCE_DIR:-}"
BASE_URL="${POISE_BASE_URL:-http://127.0.0.1:8000}"
DRY_RUN=0

usage() {
  cat <<'EOF'
用法:
  scripts/run-instance-tui.sh [--dry-run]

环境变量:
  POISE_INSTANCE_DIR  实例目录，用于日志落点
  POISE_BASE_URL      实例 HTTP 服务基地址，默认 http://127.0.0.1:8000
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

if [[ -z "$RAW_INSTANCE_DIR" ]]; then
  echo "missing required POISE_INSTANCE_DIR" >&2
  usage >&2
  exit 1
fi

INSTANCE_DIR="$(poise_absolutize_path "$RAW_INSTANCE_DIR")"

LOG_DIR="${POISE_LOG_DIR:-${INSTANCE_DIR}/.logs}"
LOG_PATH="${POISE_TUI_LOG:-${LOG_DIR}/poise-tui.log}"

mkdir -p "$LOG_DIR"

if [[ "$DRY_RUN" -eq 1 ]]; then
  cat <<EOF
repo_root=$REPO_ROOT
instance_dir=$INSTANCE_DIR
base_url=$BASE_URL
log_path=$LOG_PATH
command=cargo run --manifest-path $REPO_ROOT/Cargo.toml -p poise-tui
EOF
  exit 0
fi

export POISE_BASE_URL="$BASE_URL"
export POISE_TUI_LOG="$LOG_PATH"

exec cargo run --manifest-path "$REPO_ROOT/Cargo.toml" -p poise-tui
