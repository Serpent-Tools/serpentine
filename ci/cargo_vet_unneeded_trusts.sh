#!/usr/bin/env bash
set -uo pipefail

CONFIG="supply-chain/audits.toml"

if [[ ! -f "$CONFIG" ]]; then
    echo "No $CONFIG found. Run from your project root."
    exit 1
fi

cp "$CONFIG" "$CONFIG.bak"
trap 'cp "$CONFIG.bak" "$CONFIG"; rm -f "$CONFIG.bak"' EXIT

# Parse trust entries from the backup
trusts=()
current_crate=""
header_line=0
while IFS= read -r line; do
    line_num="${line%%:*}"
    content="${line#*:}"
    if [[ "$content" =~ ^\[\[trusted\.(.+)\]\]$ ]]; then
        current_crate="${BASH_REMATCH[1]}"
        header_line="$line_num"
    elif [[ "$content" =~ ^user-id\ =\ ([0-9]+) && -n "$current_crate" ]]; then
        user_id="${BASH_REMATCH[1]}"
        trusts+=("$current_crate|$user_id|$header_line")
        current_crate=""
    fi
done < <(grep -n '' "$CONFIG.bak")

if [[ ${#trusts[@]} -eq 0 ]]; then
    echo "No trust entries found."
    exit 0
fi

echo "Testing ${#trusts[@]} trust entries..."
echo ""

redundant=()
needed=()
needed_but_small=()

total=${#trusts[@]}
i=0

for entry in "${trusts[@]}"; do
    ((i++))
    IFS='|' read -r crate user_id start_line <<< "$entry"

    # Remove the [[trusted.X]] block starting at start_line
    # Block ends at the next [[ line or EOF
    awk -v start="$start_line" '
    BEGIN { skip=0 }
    NR == start { skip=1; next }
    skip && /^\[\[/ { skip=0 }
    skip { next }
    { print }
    ' "$CONFIG.bak" > "$CONFIG"

    printf '  [%d/%d] %-45s (user %s) ... ' "$i" "$total" "$crate" "$user_id"

    if cargo vet check --locked &>/dev/null; then
        echo "REDUNDANT"
        redundant+=("$crate|$user_id")
    else
        check_output=$(cargo vet check 2>&1)
        audit_size=""
        while IFS= read -r sline; do
            pattern="cargo vet (diff|inspect) ${crate} "
            if [[ "$sline" =~ $pattern ]]; then
                if [[ "$sline" =~ ([0-9]+)\ insertions?\(\+\) ]]; then
                    ins="${BASH_REMATCH[1]}"
                    del=0
                    [[ "$sline" =~ ([0-9]+)\ deletions?\(-\) ]] && del="${BASH_REMATCH[1]}"
                    audit_size=$((ins + del))
                elif [[ "$sline" =~ ([0-9]+)\ lines ]]; then
                    audit_size="${BASH_REMATCH[1]}"
                fi
                break
            fi
        done <<< "$check_output"

        if [[ -n "$audit_size" && "$audit_size" -lt 500 ]]; then
            echo "NEEDED ($audit_size lines to replace)"
            needed_but_small+=("$(printf '%d\t%s (user %s)' "$audit_size" "$crate" "$user_id")")
        else
            echo "NEEDED${audit_size:+ ($audit_size lines)}"
            needed+=("$crate (user $user_id)${audit_size:+ - $audit_size lines}")
        fi
    fi
done

cp "$CONFIG.bak" "$CONFIG"

echo ""
echo "=== SUMMARY ==="
echo ""

if [[ ${#needed[@]} -gt 0 ]]; then
    echo "NEEDED (${#needed[@]} entries still require trust):"
    for entry in "${needed[@]}"; do
        echo "  $entry"
    done
fi

if [[ ${#needed_but_small[@]} -gt 0 ]]; then
    echo "REPLACEABLE (small audit would eliminate trust, easiest at bottom):"
    printf '%s\n' "${needed_but_small[@]}" | sort -rn | while IFS=$'\t' read -r lines entry; do
        printf '  %6d lines  %s\n' "$lines" "$entry"
    done
    echo ""
fi

if [[ ${#redundant[@]} -gt 0 ]]; then
    echo "REDUNDANT (safe to remove, ${#redundant[@]} entries):"
    for entry in "${redundant[@]}"; do
        IFS='|' read -r crate user_id <<< "$entry"
        echo "  $crate (user $user_id)"
    done
    echo ""
fi

