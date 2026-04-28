#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/lib/poise-instance.sh"
REPO_ROOT="$(poise_repo_root_from_script_dir "$SCRIPT_DIR")"

RAW_INSTANCE_DIR="${POISE_INSTANCE_DIR:-}"
DRY_RUN=0

usage() {
  cat <<'EOF'
用法:
  scripts/run-instance-server.sh [--dry-run]

环境变量:
  POISE_INSTANCE_DIR  实例目录，必须包含 config.toml
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

CONFIG_PATH="${INSTANCE_DIR}/config.toml"
LOG_DIR="${POISE_LOG_DIR:-${INSTANCE_DIR}/.logs}"
LOG_PATH="${POISE_SERVER_LOG:-${LOG_DIR}/poise-server.log}"

mkdir -p "$LOG_DIR"

if [[ "$DRY_RUN" -ne 1 && ! -f "$CONFIG_PATH" ]]; then
  echo "config file not found: $CONFIG_PATH" >&2
  exit 1
fi

SERVER_ARGS=(--instance-dir "$INSTANCE_DIR")

if [[ "$DRY_RUN" -eq 1 ]]; then
  cat <<EOF
repo_root=$REPO_ROOT
instance_dir=$INSTANCE_DIR
config_path=$CONFIG_PATH
log_path=$LOG_PATH
command=cargo run --manifest-path $REPO_ROOT/Cargo.toml -p poise-server -- ${SERVER_ARGS[*]}
EOF
  exit 0
fi

echo "[$(date '+%Y-%m-%d %H:%M:%S')] starting poise-server instance_dir=$INSTANCE_DIR" | tee -a "$LOG_PATH"
cargo run --manifest-path "$REPO_ROOT/Cargo.toml" -p poise-server -- "${SERVER_ARGS[@]}" 2>&1 | tee -a "$LOG_PATH"
