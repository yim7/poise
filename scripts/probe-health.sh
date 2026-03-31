#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

BASE_URL="${POISE_HEALTH_BASE_URL:-http://127.0.0.1:8000}"
INTERVAL_SECS="${POISE_HEALTH_INTERVAL_SECS:-60}"
LOG_DIR="${POISE_LOG_DIR:-.logs/paper}"
LOG_PATH="${POISE_HEALTH_LOG:-${LOG_DIR}/health-probe.log}"
RUN_ONCE=0
DRY_RUN=0

usage() {
  cat <<'EOF'
用法:
  scripts/probe-health.sh [--once] [--dry-run]

环境变量:
  POISE_HEALTH_BASE_URL       服务端基地址，默认 http://127.0.0.1:8000
  POISE_HEALTH_INTERVAL_SECS  巡检间隔秒数，默认 60
  POISE_LOG_DIR               日志目录，默认 .logs/paper
  POISE_HEALTH_LOG            巡检日志文件，默认 .logs/paper/health-probe.log
EOF
}

while (($# > 0)); do
  case "$1" in
    --once)
      RUN_ONCE=1
      shift
      ;;
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

probe_once() {
  local now response status body
  now="$(date '+%Y-%m-%d %H:%M:%S')"

  if ! response="$(
    curl \
      --silent \
      --show-error \
      --max-time 10 \
      --noproxy "127.0.0.1,localhost,::1" \
      --write-out $'\n%{http_code}' \
      "${BASE_URL}/health"
  )"; then
    printf '[%s] transport_error base_url=%s\n' "$now" "$BASE_URL" | tee -a "$LOG_PATH" >&2
    return 2
  fi

  status="${response##*$'\n'}"
  body="${response%$'\n'*}"
  printf '[%s] http_status=%s body=%s\n' "$now" "$status" "$body" | tee -a "$LOG_PATH"

  case "$status" in
    200)
      return 0
      ;;
    503)
      return 3
      ;;
    *)
      return 4
      ;;
  esac
}

if [[ "$DRY_RUN" -eq 1 ]]; then
  cat <<EOF
base_url=$BASE_URL
interval_secs=$INTERVAL_SECS
log_path=$LOG_PATH
mode=$([[ "$RUN_ONCE" -eq 1 ]] && echo once || echo loop)
EOF
  exit 0
fi

if [[ "$RUN_ONCE" -eq 1 ]]; then
  probe_once
  exit $?
fi

while true; do
  probe_once || true
  sleep "$INTERVAL_SECS"
done
