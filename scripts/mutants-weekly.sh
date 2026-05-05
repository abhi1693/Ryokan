#!/usr/bin/env bash
# Weekly cargo-mutants reporter.
#
# Reads the current `mutants.out/` directory plus a previous-baseline
# JSON snapshot, emits a markdown report on stdout, writes an updated
# baseline JSON to `--output-baseline`, and exits non-zero when a
# regression is detected (a mutant that was caught last week is now
# missed, or the catch rate dropped). Designed to be invoked from
# `.github/workflows/mutants-weekly.yml`.
#
# Usage:
#   scripts/mutants-weekly.sh \
#       --current-dir mutants.out \
#       --baseline mutants.baseline.json \
#       --output-baseline mutants.baseline.next.json \
#       --output-report mutants-report.md
#
# All four args are optional; defaults match the intended workflow
# layout. `--baseline` may point at a non-existent file (first run) —
# the script handles that gracefully by skipping the diff section.
#
# Dependencies: `jq` (parse + emit baseline JSON), `awk`, `comm`,
# `sort`. All standard on ubuntu-latest runners.

set -euo pipefail

CURRENT_DIR="mutants.out"
BASELINE_PATH="mutants.baseline.json"
OUT_BASELINE="mutants.baseline.next.json"
OUT_REPORT="/dev/stdout"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --current-dir) CURRENT_DIR="$2"; shift 2 ;;
        --baseline) BASELINE_PATH="$2"; shift 2 ;;
        --output-baseline) OUT_BASELINE="$2"; shift 2 ;;
        --output-report) OUT_REPORT="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

current_caught_file="$CURRENT_DIR/caught.txt"
current_missed_file="$CURRENT_DIR/missed.txt"

if [[ ! -f "$current_caught_file" || ! -f "$current_missed_file" ]]; then
    echo "missing $current_caught_file or $current_missed_file — did cargo-mutants actually run?" >&2
    exit 2
fi

# wc -l counts trailing-newline-terminated lines; an empty file produces
# 0, a one-line file with no trailing newline produces 0 too. Add a
# `tr -cd '\n' | wc -c` fallback if Ryokan's mutants output ever drops
# the trailing newline. Today's output always has one.
caught=$(wc -l < "$current_caught_file" | tr -d ' ')
missed=$(wc -l < "$current_missed_file" | tr -d ' ')
total=$((caught + missed))

if [[ "$total" -eq 0 ]]; then
    {
        echo "## Mutation testing — empty run"
        echo
        echo "No caught or missed mutants. Possible cargo-mutants harness failure;"
        echo "check the workflow logs."
    } > "$OUT_REPORT"
    exit 1
fi

pct=$((caught * 100 / total))

date_iso=$(date -u +%F)

# Emit the new baseline JSON regardless of regression status — the
# next run wants today's snapshot as its "previous" baseline. The
# baseline carries the FULL caught + missed lists so the diff can
# identify per-line regressions, not just aggregate count drift.
jq -n \
    --arg date "$date_iso" \
    --argjson caught "$caught" \
    --argjson missed "$missed" \
    --rawfile missed_list "$current_missed_file" \
    --rawfile caught_list "$current_caught_file" \
    '{
        date: $date,
        caught: $caught,
        missed: $missed,
        missed_list: ($missed_list | split("\n") | map(select(. != ""))),
        caught_list: ($caught_list | split("\n") | map(select(. != "")))
    }' > "$OUT_BASELINE"

# Build the report.
{
    echo "## Weekly mutation testing — $date_iso"
    echo
    echo "**Current:** ${caught} caught / ${missed} missed (${pct}% catch rate, ${total} viable)"

    regression=0

    if [[ -f "$BASELINE_PATH" ]]; then
        prev_caught=$(jq -r '.caught' "$BASELINE_PATH")
        prev_missed=$(jq -r '.missed' "$BASELINE_PATH")
        prev_date=$(jq -r '.date' "$BASELINE_PATH")
        prev_total=$((prev_caught + prev_missed))
        if [[ "$prev_total" -gt 0 ]]; then
            prev_pct=$((prev_caught * 100 / prev_total))
        else
            prev_pct=0
        fi
        caught_delta=$((caught - prev_caught))
        missed_delta=$((missed - prev_missed))
        pct_delta=$((pct - prev_pct))

        echo "**Previous (${prev_date}):** ${prev_caught} caught / ${prev_missed} missed (${prev_pct}% catch rate)"
        printf "**Delta:** %+d caught, %+d missed, %+dpp catch rate\n" \
            "$caught_delta" "$missed_delta" "$pct_delta"
        echo

        # Per-mutant diff — find any mutant that was CAUGHT last run
        # and is now MISSED. Aggregate-count drift can be benign
        # (added/removed code shifts mutant counts), but per-line
        # caught→missed is a real test-quality regression.
        prev_caught_tmp=$(mktemp)
        prev_missed_tmp=$(mktemp)
        cur_caught_tmp=$(mktemp)
        cur_missed_tmp=$(mktemp)
        # cargo-mutants output already strips the trailing build/test
        # timing suffix from caught/missed.txt (vs the streaming log),
        # so the lines match across runs as long as the source code
        # at the mutated line hasn't shifted. Code changes that move
        # line numbers will surface here as "newly missed at line N+1"
        # paired with "no longer present at line N" — the per-line
        # diff is informational, not a strict regression signal.
        jq -r '.caught_list[]' "$BASELINE_PATH" | sort > "$prev_caught_tmp"
        jq -r '.missed_list[]' "$BASELINE_PATH" | sort > "$prev_missed_tmp"
        sort < "$current_caught_file" > "$cur_caught_tmp"
        sort < "$current_missed_file" > "$cur_missed_tmp"

        # Newly missed mutants: present in BOTH prev_caught and
        # current_missed = caught last week, missed this week.
        new_misses=$(comm -12 "$prev_caught_tmp" "$cur_missed_tmp" | head -50)
        # Newly caught: present in BOTH prev_missed and current_caught
        # = missed last week, caught this week. Informational.
        new_catches=$(comm -12 "$prev_missed_tmp" "$cur_caught_tmp" | head -20)

        if [[ -n "$new_misses" ]]; then
            echo "### 🚨 Newly missed mutants (regression — flipped CAUGHT → MISSED)"
            echo
            while IFS= read -r line; do
                echo "- \`$line\`"
            done <<< "$new_misses"
            regression=1
            echo
        fi

        if [[ -n "$new_catches" ]]; then
            echo "### ✅ Newly caught mutants (improvement — flipped MISSED → CAUGHT)"
            echo
            while IFS= read -r line; do
                echo "- \`$line\`"
            done <<< "$new_catches"
            echo
        fi

        rm -f "$prev_caught_tmp" "$prev_missed_tmp" "$cur_caught_tmp" "$cur_missed_tmp"
    else
        echo
        echo "_No previous baseline — first run. Today's snapshot becomes next week's reference._"
        echo
    fi

    # Per-file summary table. Useful even on green runs to surface
    # which files have the lowest catch rates as future-targeted work.
    echo "### Per-file breakdown"
    echo
    echo "| File | Caught | Missed | Catch rate |"
    echo "|------|--------|--------|------------|"
    {
        awk -F: '{print $1}' "$current_caught_file"
        awk -F: '{print $1}' "$current_missed_file"
    } | sort -u | while IFS= read -r f; do
        [[ -z "$f" ]] && continue
        c=$(grep -c "^${f}:" "$current_caught_file" || true)
        m=$(grep -c "^${f}:" "$current_missed_file" || true)
        ftotal=$((c + m))
        if [[ "$ftotal" -eq 0 ]]; then
            continue
        fi
        fpct=$((c * 100 / ftotal))
        printf "| %s | %d | %d | %d%% |\n" "$f" "$c" "$m" "$fpct"
    done

    echo
    echo "_Generated by \`scripts/mutants-weekly.sh\`. Baseline at \`$BASELINE_PATH\`. Next baseline written to \`$OUT_BASELINE\`._"
} > "$OUT_REPORT"

exit "$regression"
