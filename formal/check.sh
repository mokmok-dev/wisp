#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/wisp-formal.XXXXXX")"
trap 'rm -rf "$work_dir"' EXIT

tlc_workers="${TLC_WORKERS:-2}"

run_tlc() {
  local module="$1"
  local meta_dir="$work_dir/$module"

  echo "==> TLC: $module"
  (
    cd "$repo_root/formal/tla"
    tlc \
      -workers "$tlc_workers" \
      -metadir "$meta_dir" \
      -config "$module.cfg" \
      "$module.tla"
  )
}

run_z3() {
  local proof="$1"
  local output
  local expected=""
  local label=""
  local line
  local failed=0
  local base_seen=0
  local preservation_seen=0
  local action_count=0
  local expected_action_count=""
  local declared_actions=""
  local witnessed_actions=""
  local action_name=""
  local step_actions=""

  step_actions="$(
    awk '
      /^\(define-fun Step \(\) Bool[[:space:]]*$/ {
        in_step = 1
        next
      }
      in_step {
        is_last = ($0 ~ /\)\)[[:space:]]*$/)
        line = $0
        sub(/;.*/, "", line)
        gsub(/[()]/, " ", line)
        field_count = split(line, fields, /[[:space:]]+/)
        for (field = 1; field <= field_count; field += 1) {
          if (fields[field] != "" && fields[field] != "or") {
            print fields[field]
          }
        }
        if (is_last) {
          exit
        }
      }
    ' "$proof" | paste -sd, -
  )"

  echo "==> Z3: $(basename "$proof")"
  if ! output="$(z3 "$proof" 2>&1)"; then
    echo "Z3 failed to evaluate $proof:" >&2
    printf '%s\n' "$output" >&2
    return 1
  fi
  printf '%s\n' "$output"

  while IFS= read -r line; do
    case "$line" in
      "EXPECT-ACTION-COUNT: "*)
        if [[ -n "$expected_action_count" ]]; then
          echo "duplicate EXPECT-ACTION-COUNT declaration" >&2
          failed=1
        fi
        expected_action_count="${line#EXPECT-ACTION-COUNT: }"
        if ! [[ "$expected_action_count" =~ ^[0-9]+$ ]]; then
          echo "invalid action count '$expected_action_count'" >&2
          failed=1
        fi
        ;;
      "EXPECT-ACTIONS: "*)
        if [[ -n "$declared_actions" ]]; then
          echo "duplicate EXPECT-ACTIONS declaration" >&2
          failed=1
        fi
        declared_actions="${line#EXPECT-ACTIONS: }"
        ;;
      "EXPECT-SAT: "*)
        if [[ -n "$expected" ]]; then
          echo "missing Z3 result after '$label'" >&2
          failed=1
        fi
        expected="sat"
        label="${line#EXPECT-SAT: }"
        if [[ "$label" == action\ * ]]; then
          action_name="${label#action }"
          case ",$witnessed_actions," in
            *",$action_name,"*)
              echo "duplicate action witness '$action_name'" >&2
              failed=1
              ;;
            *)
              if [[ -n "$witnessed_actions" ]]; then
                witnessed_actions+=","
              fi
              witnessed_actions+="$action_name"
              ;;
          esac
          action_count=$((action_count + 1))
        fi
        ;;
      "EXPECT-UNSAT: "*)
        if [[ -n "$expected" ]]; then
          echo "missing Z3 result after '$label'" >&2
          failed=1
        fi
        expected="unsat"
        label="${line#EXPECT-UNSAT: }"
        if [[ "$label" == "base-case" ]]; then
          base_seen=1
        fi
        if [[ "$label" == "preservation" ]]; then
          preservation_seen=1
        fi
        ;;
      sat|unsat|unknown)
        if [[ -z "$expected" ]]; then
          echo "unlabelled Z3 result '$line'" >&2
          failed=1
        elif [[ "$line" != "$expected" ]]; then
          echo "Z3 query '$label': expected $expected, got $line" >&2
          failed=1
        fi
        expected=""
        label=""
        ;;
      *)
        echo "unexpected Z3 output: $line" >&2
        failed=1
        ;;
    esac
  done <<<"$output"

  if [[ -n "$expected" ]]; then
    echo "missing Z3 result after '$label'" >&2
    failed=1
  fi
  if ((base_seen == 0 || preservation_seen == 0 || action_count == 0)); then
    echo "proof must check a base case, action witnesses, and preservation" >&2
    failed=1
  fi
  if [[ -z "$expected_action_count" || -z "$declared_actions" ]]; then
    echo "proof must declare EXPECT-ACTION-COUNT and EXPECT-ACTIONS" >&2
    failed=1
  else
    local -a declared_action_items=()
    local declared_action=""
    local declared_seen=""
    local declared_actual_count=0
    IFS=',' read -r -a declared_action_items <<<"$declared_actions"
    for declared_action in "${declared_action_items[@]}"; do
      if [[ -z "$declared_action" ]]; then
        echo "empty action in EXPECT-ACTIONS declaration" >&2
        failed=1
        continue
      fi
      case ",$declared_seen," in
        *",$declared_action,"*)
          echo "duplicate declared action '$declared_action'" >&2
          failed=1
          ;;
        *)
          if [[ -n "$declared_seen" ]]; then
            declared_seen+=","
          fi
          declared_seen+="$declared_action"
          ;;
      esac
      declared_actual_count=$((declared_actual_count + 1))
    done
    if [[ "$expected_action_count" =~ ^[0-9]+$ ]]; then
      if ((declared_actual_count != expected_action_count)); then
        echo "declared action set has $declared_actual_count entries; expected $expected_action_count" >&2
        failed=1
      fi
      if ((action_count != expected_action_count)); then
        echo "saw $action_count action witnesses; expected $expected_action_count" >&2
        failed=1
      fi
    fi
    if [[ "$witnessed_actions" != "$declared_actions" ]]; then
      echo "action witnesses do not exactly match EXPECT-ACTIONS" >&2
      echo "declared:  $declared_actions" >&2
      echo "witnessed: $witnessed_actions" >&2
      failed=1
    fi
    if [[ -z "$step_actions" ]]; then
      echo "could not extract the Step action list from $proof" >&2
      failed=1
    elif [[ "$step_actions" != "$declared_actions" ]]; then
      echo "Step actions do not exactly match EXPECT-ACTIONS" >&2
      echo "declared: $declared_actions" >&2
      echo "Step:     $step_actions" >&2
      failed=1
    fi
  fi

  if ((failed != 0)); then
    if awk '
      previous == "EXPECT-UNSAT: preservation" && $0 == "sat" { found = 1 }
      { previous = $0 }
      END { exit(found ? 0 : 1) }
    ' <<<"$output"; then
      echo "preservation counterexample model:" >&2
      {
        sed -n '1,$p' "$proof"
        printf '\n(get-model)\n'
      } | z3 -in >&2
    fi
    return 1
  fi
}

run_tlc SessionLifecycle
run_tlc ApplicationLifecycle

for proof in "$repo_root"/formal/z3/*.smt2; do
  run_z3 "$proof"
done

echo "==> all formal checks passed"
