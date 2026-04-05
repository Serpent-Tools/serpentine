#!/usr/bin/env bash
set -euo pipefail

suggest_output=$(cargo vet suggest 2>&1 || true)

if [[ -z "$suggest_output" ]]; then
    echo "No suggestions from cargo vet. You're either fully vetted or something went wrong."
    exit 0
fi

declare -a small_gaps=()
declare -a audit_queue=()

while IFS= read -r line; do
    if [[ "$line" =~ ^[[:space:]]+cargo\ vet\ diff\ ([^ ]+)\ ([^ ]+)\ ([^ ]+) ]]; then
        crate="${BASH_REMATCH[1]}"
        v1="${BASH_REMATCH[2]}"
        v2="${BASH_REMATCH[3]}"

        insertions=0
        deletions=0
        if [[ "$line" =~ ([0-9]+)\ insertions?\(\+\) ]]; then
            insertions="${BASH_REMATCH[1]}"
        fi
        if [[ "$line" =~ ([0-9]+)\ deletions?\(-\) ]]; then
            deletions="${BASH_REMATCH[1]}"
        fi
        total_lines=$((insertions + deletions))

        IFS='.' read -r maj1 min1 pat1 <<< "$v1"
        IFS='.' read -r maj2 min2 pat2 <<< "$v2"

        gap_kind=""
        if [[ "$maj1" == "$maj2" && "$min1" == "$min2" && "$pat1" != "$pat2" ]]; then
            gap_kind="patch"
        elif [[ "$maj1" != "0" && "$maj1" == "$maj2" && "$min1" != "$min2" ]]; then
            gap_kind="minor"
        fi

        if [[ -n "$gap_kind" ]]; then
            small_gaps+=("$(printf '%d\t%s\t%s\t%s\t%s' "$total_lines" "$gap_kind" "$crate" "$v1" "$v2")")
        fi

        audit_queue+=("$(printf '%d\tdiff\t%s\t%s -> %s' "$total_lines" "$crate" "$v1" "$v2")")

    elif [[ "$line" =~ ^[[:space:]]+cargo\ vet\ inspect\ ([^ ]+)\ ([^ ]+) ]]; then
        crate="${BASH_REMATCH[1]}"
        version="${BASH_REMATCH[2]}"

        total_lines=0
        if [[ "$line" =~ ([0-9]+)\ lines ]]; then
            total_lines="${BASH_REMATCH[1]}"
        fi

        audit_queue+=("$(printf '%d\tinspect\t%s\t%s' "$total_lines" "$crate" "$version")")
    fi
done <<< "$suggest_output"

if [[ ${#audit_queue[@]} -gt 0 ]]; then
    echo "=== AUDIT QUEUE (sorted by effort, easiest at bottom) ==="
    echo ""
    printf '%s\n' "${audit_queue[@]}" | sort -rn | while IFS=$'\t' read -r lines kind crate versions; do
        printf '  %6d lines  %-8s %-40s %s\n' "$lines" "[$kind]" "$crate" "$versions"
    done
    echo ""
fi

if [[ ${#small_gaps[@]} -gt 0 ]]; then
    echo "=== SMALL GAPS (consider downgrading or auditing the diff) ==="
    echo ""
    printf '%s\n' "${small_gaps[@]}" | sort -rn | while IFS=$'\t' read -r lines kind crate v1 v2; do
        printf '  %6d lines  [%s]  %-40s %s -> %s\n' "$lines" "$kind" "$crate" "$v1" "$v2"
    done
fi
