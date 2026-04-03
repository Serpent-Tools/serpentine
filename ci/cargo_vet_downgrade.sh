#!/usr/bin/env bash
set -uo pipefail

suggest_output=$(cargo vet suggest 2>&1 || true)

if [[ -z "$suggest_output" ]]; then
    echo "No suggestions from cargo vet."
    exit 0
fi

declare -a candidates=()

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

        is_small=false
        if [[ "$maj1" == "$maj2" && "$min1" == "$min2" && "$pat1" != "$pat2" ]]; then
            is_small=true
        elif [[ "$maj1" != "0" && "$maj1" == "$maj2" && "$min1" != "$min2" ]]; then
            is_small=true
        fi

        if $is_small; then
            candidates+=("$(printf '%d\t%s\t%s\t%s' "$total_lines" "$crate" "$v1" "$v2")")
        fi
    fi
done <<< "$suggest_output"

if [[ ${#candidates[@]} -eq 0 ]]; then
    echo "No small-gap candidates found."
    exit 0
fi

succeeded=0
failed=0
skipped=0

sorted=$(printf '%s\n' "${candidates[@]}" | sort -n)

while IFS=$'\t' read -r lines crate v1 v2; do
    printf '%-40s %s -> %s (%d lines) ... ' "$crate" "$v2" "$v1" "$lines"

    if output=$(cargo update -p "$crate@$v2" --precise "$v1" 2>&1); then
        echo "OK"
        ((succeeded++))
    else
        if echo "$output" | grep -qi "no matching package"; then
            echo "SKIP (not a direct dependency)"
            ((skipped++))
        else
            echo "FAIL"
            echo "  $output" | head -3
            ((failed++))
        fi
    fi
done <<< "$sorted"

echo ""
echo "=== SUMMARY ==="
echo "  Downgraded: $succeeded"
echo "  Failed:     $failed"
echo "  Skipped:    $skipped"

if [[ $succeeded -gt 0 ]]; then
    echo ""
    echo "Run 'cargo vet' to verify the new lockfile passes."
fi
