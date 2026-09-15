#!/usr/bin/env bash

set -euo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/lib/poise-instance.sh"

BASE_URL="${POISE_BASE_URL:-http://127.0.0.1:8000}"
RAW_INSTANCE_DIR="${POISE_INSTANCE_DIR:-}"
INTERVAL_SECS="${POISE_HEALTH_INTERVAL_SECS:-60}"
FAILURE_THRESHOLD="${POISE_HEALTH_FAILURE_THRESHOLD:-3}"
ALERT_HOOK="${POISE_HEALTH_ALERT_HOOK:-}"
CHECK=""
ENDPOINT=""
RUN_ONCE=0
DRY_RUN=0

usage() {
  cat <<'EOF'
用法:
  scripts/probe-health.sh --check health [--once] [--dry-run]
  scripts/probe-health.sh --check readiness [--once] [--dry-run]

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
    --check)
      if (($# < 2)); then
        echo "missing value for --check" >&2
        usage >&2
        exit 1
      fi
      if [[ -n "$CHECK" ]]; then
        echo "--check may only be specified once" >&2
        usage >&2
        exit 1
      fi
      case "$2" in
        health|readiness)
          CHECK="$2"
          ;;
        *)
          echo "invalid --check value: $2" >&2
          usage >&2
          exit 1
          ;;
      esac
      shift 2
      ;;
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

if [[ -z "$CHECK" ]]; then
  echo "missing required --check health|readiness" >&2
  usage >&2
  exit 1
fi

case "$CHECK" in
  health)
    ENDPOINT="/health"
    ;;
  readiness)
    ENDPOINT="/readiness"
    ;;
esac

if [[ -n "$RAW_INSTANCE_DIR" ]]; then
  INSTANCE_DIR="$(poise_absolutize_path "$RAW_INSTANCE_DIR")"
else
  INSTANCE_DIR=""
fi

if [[ -n "${POISE_HEALTH_LOG:-}" ]]; then
  LOG_PATH="${POISE_HEALTH_LOG}"
elif [[ -n "${POISE_LOG_DIR:-}" ]]; then
  LOG_PATH="${POISE_LOG_DIR}/health-probe.log"
elif [[ -n "$INSTANCE_DIR" ]]; then
  LOG_PATH="${INSTANCE_DIR}/.logs/health-probe.log"
else
  LOG_PATH=""
fi

if [[ -n "$LOG_PATH" ]]; then
  mkdir -p "$(dirname "$LOG_PATH")"
fi

if [[ ! "$FAILURE_THRESHOLD" =~ ^[1-9][0-9]*$ ]]; then
  echo "POISE_HEALTH_FAILURE_THRESHOLD must be a positive integer" >&2
  exit 1
fi

LAST_PROBE_RESULT=""
LAST_PROBE_STATUS=""
LAST_PROBE_BODY=""

log_stdout() {
  if [[ -n "$LOG_PATH" ]]; then
    printf "$@" | tee -a "$LOG_PATH"
  else
    printf "$@"
  fi
}

log_stderr() {
  if [[ -n "$LOG_PATH" ]]; then
    printf "$@" | tee -a "$LOG_PATH" >&2
  else
    printf "$@" >&2
  fi
}

response_matches_contract() {
  local body compact_body
  body="$1"
  compact_body="$(printf '%s' "$body" | tr -d '[:space:]')"

  case "$CHECK" in
    health)
      [[ "$compact_body" =~ ^\{\"status\":\"ok\",\"track_count\":[0-9]+,\"attention_required_count\":0,\"tasks\":\[.*\],\"pnl_backfill\":\{.*\}\}$ ]]
      ;;
    readiness)
      [[ "$compact_body" =~ ^\{\"ready_to_trade\":true,\"blockers\":\[\],\"warnings\":\[.*\]\}$ ]]
      ;;
  esac
}

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
      "${BASE_URL}${ENDPOINT}"
  )"; then
    LAST_PROBE_RESULT="transport_error"
    LAST_PROBE_STATUS="transport_error"
    LAST_PROBE_BODY=""
    log_stderr '[%s] transport_error check=%s endpoint=%s base_url=%s\n' \
      "$now" "$CHECK" "$ENDPOINT" "$BASE_URL"
    return 2
  fi

  status="${response##*$'\n'}"
  body="${response%$'\n'*}"
  LAST_PROBE_STATUS="$status"
  LAST_PROBE_BODY="$body"
  log_stdout '[%s] check=%s endpoint=%s http_status=%s body=%s\n' \
    "$now" "$CHECK" "$ENDPOINT" "$status" "$body"

  case "$status" in
    200)
      if ! response_matches_contract "$body"; then
        LAST_PROBE_RESULT="response_contract_mismatch"
        log_stderr '[%s] response_contract_mismatch check=%s endpoint=%s http_status=%s body=%s\n' \
          "$now" "$CHECK" "$ENDPOINT" "$status" "$body"
        return 4
      fi
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

  printf '[%s] ALERT failure_count=%s result=%s status=%s exit_code=%s check=%s endpoint=%s base_url=%s\n' \
    "$now" \
    "$failure_count" \
    "$LAST_PROBE_RESULT" \
    "$LAST_PROBE_STATUS" \
    "$exit_code" \
    "$CHECK" \
    "$ENDPOINT" \
    "$BASE_URL" | {
      if [[ -n "$LOG_PATH" ]]; then
        tee -a "$LOG_PATH" >&2
      else
        cat >&2
      fi
    }

  if [[ -n "$ALERT_HOOK" ]]; then
    if ! POISE_HEALTH_FAILURE_COUNT="$failure_count" \
      POISE_HEALTH_LAST_RESULT="$LAST_PROBE_RESULT" \
      POISE_HEALTH_LAST_STATUS="$LAST_PROBE_STATUS" \
      POISE_HEALTH_LAST_BODY="$LAST_PROBE_BODY" \
      POISE_HEALTH_LAST_EXIT_CODE="$exit_code" \
      POISE_HEALTH_CHECK="$CHECK" \
      POISE_HEALTH_ENDPOINT="$ENDPOINT" \
      POISE_BASE_URL="$BASE_URL" \
      bash -lc "$ALERT_HOOK"; then
      log_stderr '[%s] ALERT_HOOK_FAILED command=%s\n' "$now" "$ALERT_HOOK"
    fi
  fi
}

if [[ "$DRY_RUN" -eq 1 ]]; then
  cat <<EOF
base_url=$BASE_URL
check=$CHECK
endpoint=$ENDPOINT
interval_secs=$INTERVAL_SECS
failure_threshold=$FAILURE_THRESHOLD
alert_hook=$([[ -n "$ALERT_HOOK" ]] && echo configured || echo disabled)
log_path=${LOG_PATH:-disabled}
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
      log_stdout '[%s] recovered consecutive_failures=%s base_url=%s\n' \
        "$(date '+%Y-%m-%d %H:%M:%S')" \
        "$consecutive_failures" \
        "$BASE_URL"
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
