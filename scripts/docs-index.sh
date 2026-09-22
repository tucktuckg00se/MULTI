#!/usr/bin/env bash
# Keeps docs/INDEX.md honest: recomputes the ~Tokens column (bytes / 4) for every
# indexed row and fails if a doc exists that the index doesn't list.
#
#   scripts/docs-index.sh          rewrite token counts in place
#   scripts/docs-index.sh --check  exit 1 if counts are stale or anything is unindexed
#
# Index rows look like:  | ID | type | [Title](relative/path) | status | ~123 |
# Paths are relative to docs/. A row may point at a directory (e.g. an evidence
# folder); its cost is the sum of the files inside.
set -euo pipefail
cd "$(dirname "$0")/.."

index=docs/INDEX.md
check=false
[[ ${1:-} == --check ]] && check=true

cost() {
  local bytes
  if [[ -d $1 ]]; then
    bytes=$(find "$1" -type f -print0 | xargs -0 -r cat | wc -c)
  else
    bytes=$(wc -c <"$1")
  fi
  echo $(((bytes + 3) / 4))
}

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
indexed=()
errors=0

while IFS= read -r line || [[ -n $line ]]; do
  IFS='|' read -r -a cell <<<"$line"
  link_re='\]\(([^)]+)\)'
  if [[ ${#cell[@]} -ge 6 && ${cell[5]} == *'~'* && ${cell[3]} =~ $link_re ]]; then
    target=$(realpath -m --relative-to=. "docs/${BASH_REMATCH[1]}")
    if [[ ! -e $target ]]; then
      echo "missing: $target (listed in $index)" >&2
      errors=1
      printf '%s\n' "$line" >>"$tmp"
      continue
    fi
    indexed+=("$target")
    printf '|%s|%s|%s|%s| ~%s |\n' "${cell[1]}" "${cell[2]}" "${cell[3]}" "${cell[4]}" "$(cost "$target")" >>"$tmp"
  else
    printf '%s\n' "$line" >>"$tmp"
  fi
done <"$index"

is_indexed() {
  local f=$1 t
  for t in "${indexed[@]}"; do
    [[ $f == "$t" || $f == "$t"/* ]] && return 0
  done
  return 1
}

while IFS= read -r f; do
  if ! is_indexed "$f"; then
    echo "unindexed: $f" >&2
    errors=1
  fi
done < <({
  ls CLAUDE.md PRD.md README.md 2>/dev/null
  find docs -type f ! -path "$index" ! -name '.gitkeep'
} | sort)

if $check; then
  if ! cmp -s "$tmp" "$index"; then
    echo "stale token counts in $index (run scripts/docs-index.sh)" >&2
    errors=1
  fi
else
  cp "$tmp" "$index"
fi

exit $errors
