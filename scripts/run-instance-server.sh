#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

INSTANCE_DIR="${POISE_INSTANCE_DIR:-}"
REBUILD_STATE="${POISE_REBUILD_STATE:-0}"
DRY_RUN=0

usage() {
  cat <<'EOF'
用法:
  scripts/run-instance-server.sh [--dry-run]

环境变量:
  POISE_INSTANCE_DIR  实例目录，必须包含 config.toml
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

if [[ -z "$INSTANCE_DIR" ]]; then
  echo "missing required POISE_INSTANCE_DIR" >&2
  usage >&2
  exit 1
fi

CONFIG_PATH="${INSTANCE_DIR}/config.toml"
LOG_DIR="${POISE_LOG_DIR:-${INSTANCE_DIR}/.logs}"
LOG_PATH="${POISE_SERVER_LOG:-${LOG_DIR}/poise-server.log}"

mkdir -p "$LOG_DIR"

if [[ "$DRY_RUN" -ne 1 && ! -f "$CONFIG_PATH" ]]; then
  echo "config file not found: $CONFIG_PATH" >&2
  exit 1
fi

if [[ "$REBUILD_STATE" != "0" && "$REBUILD_STATE" != "1" ]]; then
  echo "POISE_REBUILD_STATE must be 0 or 1" >&2
  exit 1
fi

SERVER_ARGS=(--instance-dir "$INSTANCE_DIR")
if [[ "$REBUILD_STATE" == "1" ]]; then
  SERVER_ARGS+=(--rebuild-state)
fi

if [[ "$DRY_RUN" -eq 1 ]]; then
  cat <<EOF
repo_root=$REPO_ROOT
instance_dir=$INSTANCE_DIR
config_path=$CONFIG_PATH
log_path=$LOG_PATH
rebuild_state=$REBUILD_STATE
command=cargo run --manifest-path $REPO_ROOT/Cargo.toml -p poise-server -- ${SERVER_ARGS[*]}
EOF
  exit 0
fi

echo "[$(date '+%Y-%m-%d %H:%M:%S')] starting poise-server instance_dir=$INSTANCE_DIR rebuild_state=$REBUILD_STATE" | tee -a "$LOG_PATH"
cargo run --manifest-path "$REPO_ROOT/Cargo.toml" -p poise-server -- "${SERVER_ARGS[@]}" 2>&1 | tee -a "$LOG_PATH"
