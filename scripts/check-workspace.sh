#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
RUN_FULL=0

usage() {
  cat <<'EOF'
用法:
  scripts/check-workspace.sh [--full]

说明:
  默认执行快速检查:
    - cargo fmt --all --check
    - cargo clippy --workspace -- -D warnings
    - 除 poise-server / poise-tui bin 单测外的 workspace 单元 / 集成测试

  追加 --full 会切换到全量检查:
    - cargo clippy --workspace --all-targets -- -D warnings
    - cargo test --workspace --all-targets
    - cargo test --workspace --doc
    - poise-tui 的慢速真实端到端测试
EOF
}

while (($# > 0)); do
  case "$1" in
    --full)
      RUN_FULL=1
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

cargo fmt --all --check

if [[ "$RUN_FULL" -eq 1 ]]; then
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace --all-targets
  cargo test --workspace --doc
  cargo test -p poise-tui --bin poise-tui real_server_ -- --ignored
else
  cargo clippy --workspace -- -D warnings
  cargo test --workspace --exclude poise-server --exclude poise-tui --all-targets
fi
