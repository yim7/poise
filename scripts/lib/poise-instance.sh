#!/usr/bin/env bash

poise_repo_root_from_script_dir() {
  local script_dir="$1"
  (
    cd "${script_dir}/.." >/dev/null 2>&1
    pwd
  )
}

poise_absolutize_path() {
  local path="$1"

  if [[ -z "$path" ]]; then
    echo "missing path to resolve" >&2
    return 1
  fi

  if [[ "$path" = /* ]]; then
    printf '%s\n' "$path"
    return 0
  fi

  printf '%s/%s\n' "$(pwd)" "$path"
}
