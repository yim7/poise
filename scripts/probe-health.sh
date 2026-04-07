#!/usr/bin/env bash

set -euo pipefail

BASE_URL="${POISE_BASE_URL:-http://127.0.0.1:8000}"
INSTANCE_DIR="${POISE_INSTANCE_DIR:-}"
INTERVAL_SECS="${POISE_HEALTH_INTERVAL_SECS:-60}"
FAILURE_THRESHOLD="${POISE_HEALTH_FAILURE_THRESHOLD:-3}"
ALERT_HOOK="${POISE_HEALTH_ALERT_HOOK:-}"
LOG_DIR="${POISE_LOG_DIR:-${INSTANCE_DIR}/.logs}"
LOG_PATH="${POISE_HEALTH_LOG:-${LOG_DIR}/health-probe.log}"
RUN_ONCE=0
DRY_RUN=0

usage() {
  cat <<'EOF'
用法:
  scripts/probe-health.sh [--once] [--dry-run]

环境变量:
  POISE_INSTANCE_DIR          实例目录，用于默认日志路径
  POISE_BASE_URL              服务端基地址，默认 http://127.0.0.1:8000
  POISE_HEALTH_INTERVAL_SECS  巡检间隔秒数，默认 60
  POISE_HEALTH_FAILURE_THRESHOLD  连续失败阈值，默认 3
  POISE_HEALTH_ALERT_HOOK     达到失败阈值后执行的 shell command，可选
  POISE_LOG_DIR               日志目录，默认 <instance-dir>/.logs
  POISE_HEALTH_LOG            巡检日志文件，默认 <instance-dir>/.logs/health-probe.log
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

if [[ -z "$INSTANCE_DIR" && -z "${POISE_LOG_DIR:-}" ]]; then
  echo "missing required POISE_INSTANCE_DIR or POISE_LOG_DIR" >&2
  usage >&2
  exit 1
fi

mkdir -p "$LOG_DIR"

if [[ ! "$FAILURE_THRESHOLD" =~ ^[1-9][0-9]*$ ]]; then
  echo "POISE_HEALTH_FAILURE_THRESHOLD must be a positive integer" >&2
  exit 1
fi

LAST_PROBE_RESULT=""
LAST_PROBE_STATUS=""
LAST_PROBE_BODY=""

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
    LAST_PROBE_RESULT="transport_error"
    LAST_PROBE_STATUS="transport_error"
    LAST_PROBE_BODY=""
    printf '[%s] transport_error base_url=%s\n' "$now" "$BASE_URL" | tee -a "$LOG_PATH" >&2
    return 2
  fi

  status="${response##*$'\n'}"
  body="${response%$'\n'*}"
  LAST_PROBE_STATUS="$status"
  LAST_PROBE_BODY="$body"
  printf '[%s] http_status=%s body=%s\n' "$now" "$status" "$body" | tee -a "$LOG_PATH"

  case "$status" in
    200)
      LAST_PROBE_RESULT="ok"
      return 0
      ;;
    503)
      LAST_PROBE_RESULT="attention_required"
      return 3
      ;;
    *)
      LAST_PROBE_RESULT="unexpected_status"
      return 4
      ;;
  esac
}

emit_alert() {
  local now failure_count exit_code
  failure_count="$1"
  exit_code="$2"
  now="$(date '+%Y-%m-%d %H:%M:%S')"

  printf '[%s] ALERT failure_count=%s result=%s status=%s exit_code=%s base_url=%s\n' \
    "$now" \
    "$failure_count" \
    "$LAST_PROBE_RESULT" \
    "$LAST_PROBE_STATUS" \
    "$exit_code" \
    "$BASE_URL" | tee -a "$LOG_PATH" >&2

  if [[ -n "$ALERT_HOOK" ]]; then
    if ! POISE_HEALTH_FAILURE_COUNT="$failure_count" \
      POISE_HEALTH_LAST_RESULT="$LAST_PROBE_RESULT" \
      POISE_HEALTH_LAST_STATUS="$LAST_PROBE_STATUS" \
      POISE_HEALTH_LAST_BODY="$LAST_PROBE_BODY" \
      POISE_HEALTH_LAST_EXIT_CODE="$exit_code" \
      POISE_BASE_URL="$BASE_URL" \
      bash -lc "$ALERT_HOOK"; then
      printf '[%s] ALERT_HOOK_FAILED command=%s\n' "$now" "$ALERT_HOOK" | tee -a "$LOG_PATH" >&2
    fi
  fi
}

if [[ "$DRY_RUN" -eq 1 ]]; then
  cat <<EOF
base_url=$BASE_URL
interval_secs=$INTERVAL_SECS
failure_threshold=$FAILURE_THRESHOLD
alert_hook=$([[ -n "$ALERT_HOOK" ]] && echo configured || echo disabled)
log_path=$LOG_PATH
mode=$([[ "$RUN_ONCE" -eq 1 ]] && echo once || echo loop)
EOF
  exit 0
fi

if [[ "$RUN_ONCE" -eq 1 ]]; then
  probe_once
  exit $?
fi

consecutive_failures=0
while true; do
  if probe_once; then
    if (( consecutive_failures > 0 )); then
      printf '[%s] recovered consecutive_failures=%s base_url=%s\n' \
        "$(date '+%Y-%m-%d %H:%M:%S')" \
        "$consecutive_failures" \
        "$BASE_URL" | tee -a "$LOG_PATH"
    fi
    consecutive_failures=0
  else
    exit_code="$?"
    consecutive_failures=$((consecutive_failures + 1))
    if (( consecutive_failures >= FAILURE_THRESHOLD )); then
      emit_alert "$consecutive_failures" "$exit_code"
      exit "$exit_code"
    fi
  fi
  sleep "$INTERVAL_SECS"
done
