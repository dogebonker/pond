#!/usr/bin/env bash
# Gate cross-references between docs/spec.md and the Rust source. Rule ids are
# declared as `#### `rule-id`` headings; a prose citation or a `spec.md#anchor`
# in code that names a truncated, misspelled or removed id otherwise rots
# silently. Checks: duplicate declarations, unresolved in-spec references,
# unresolved code references.
set -euo pipefail

spec="${1:-docs/spec.md}"
code_roots=(packages/pond/src packages/pond/tests packages/pond/SKILL.md)

id='[a-z][a-z0-9]*(-[a-z0-9]+)+'
ref="(lance|local-store|storage|creds|model|session|adapter|wire|protocol|mcp|search|cli)-[a-z0-9]+(-[a-z0-9]+)*"

# Legacy free-form anchors that are neither a rule id nor a heading word. New
# entries are not allowed here: add a matching heading or rule id instead.
legacy_anchors=(
)

bad=0
fail() { echo "$1" >&2; bad=1; }

declared=$(grep -nE "^#{3,6} +\`$id\`" "$spec" | sed -E "s/^([0-9]+):#+ +\`($id)\`.*/\1 \2/" || true)
declared_ids=$(awk '{print $2}' <<< "$declared" | sort -u)
n_declared=$(awk 'NF' <<< "$declared_ids" | wc -l | tr -d ' ')

while read -r dup; do
  [ -n "$dup" ] || continue
  lines=$(awk -v d="$dup" '$2==d{printf "%s%s", s, $1; s=","}' <<< "$declared")
  fail "$spec:${lines%%,*}: duplicate rule-id declaration \`$dup\` (lines $lines)"
done < <(awk '{print $2}' <<< "$declared" | sort | uniq -d)

is_declared() { grep -qxF -- "$1" <<< "$declared_ids"; }

n_spec_refs=0
while IFS=: read -r line tok; do
  [ -n "$line" ] || continue
  n_spec_refs=$((n_spec_refs + 1))
  tok=${tok//\`/}
  is_declared "$tok" || fail "$spec:$line: unresolved rule-id reference \`$tok\`"
done < <(grep -noE "\`$ref\`" "$spec" || true)

# A token without a topic prefix that is the tail of a declared id is a
# truncated citation (the prefix was dropped), not a free-standing word.
while IFS=: read -r line tok; do
  [ -n "$line" ] || continue
  tok=${tok//\`/}
  full=$(grep -E -- "-$tok\$" <<< "$declared_ids" | head -n1 || true)
  [ -z "$full" ] || fail "$spec:$line: truncated rule-id reference \`$tok\` (did you mean \`$full\`?)"
done < <(grep -noE "\`$id\`" "$spec" || true)

headings=$(grep -E '^#' "$spec" || true)

n_code_refs=0
while IFS=: read -r file line match; do
  [ -n "$file" ] || continue
  n_code_refs=$((n_code_refs + 1))
  anchor=${match#spec.md#}
  anchor=$(printf '%s' "$anchor" | sed -E 's/[.-]+$//')
  if [[ "$anchor" =~ ^$ref$ ]]; then
    is_declared "$anchor" || fail "$file:$line: spec.md#$anchor is not a declared rule id"
    continue
  fi
  pat=$(printf '%s' "$anchor" | sed -E 's/[.]/\\./g; s/-/[- ]/g')
  grep -qiE "(^|[^A-Za-z0-9])$pat([^A-Za-z0-9]|$)" <<< "$headings" && continue
  for a in ${legacy_anchors[@]+"${legacy_anchors[@]}"}; do [ "$a" = "$anchor" ] && continue 2; done
  fail "$file:$line: spec.md#$anchor matches no rule id or heading word"
done < <(grep -rnoE --exclude-dir=fixtures 'spec\.md#[A-Za-z0-9._-]+' "${code_roots[@]}" | sort -t: -k1,1 -k2,2n || true)

if [ "$bad" -ne 0 ]; then
  echo >&2
  echo "check-spec-refs: fix the reference, or declare the id as a \`####\` heading in $spec." >&2
  exit 1
fi

echo "check-spec-refs: ok ($n_declared declared ids, $n_spec_refs spec refs, $n_code_refs code refs)"
