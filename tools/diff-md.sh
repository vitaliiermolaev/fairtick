#!/usr/bin/env bash
#
# diff-md.sh — dump the diff of local (unpushed) commits into a Markdown file.
#
# Works across several repositories at once (e.g. this backend + the Unity
# client), producing a single combined Markdown document with one section
# per repo.
#
# Usage:
#   tools/diff-md.sh [-o output.md] [-b base-ref] [repo ...]
#
#   -o output.md   Output file (default: local-diff.md)
#   -b base-ref    Compare every repo against this ref instead of its own
#                  upstream (e.g. -b origin/main)
#   repo ...       One or more repo paths. Default: this repo plus the Unity
#                  client at $UNITY_ROOT (default ../unity) if it exists.
#
# Per repo the base is resolved as: -b override > upstream @{u} > origin/main.
#
# Examples:
#   tools/diff-md.sh                              # backend + Unity -> local-diff.md
#   tools/diff-md.sh -o review.md                 # custom output file
#   tools/diff-md.sh -o review.md .               # only this repo
#   tools/diff-md.sh ../unity                     # only the Unity repo
#
set -euo pipefail

out="local-diff.md"
base_override=""

while getopts ":o:b:" opt; do
  case "${opt}" in
    o) out="${OPTARG}" ;;
    b) base_override="${OPTARG}" ;;
    :) echo "diff-md: option -${OPTARG} requires an argument" >&2; exit 2 ;;
    \?) echo "diff-md: unknown option -${OPTARG}" >&2; exit 2 ;;
  esac
done
shift $((OPTIND - 1))

# Default repo list: current repo + Unity client (if present).
repos=("$@")
if [[ ${#repos[@]} -eq 0 ]]; then
  repos=(".")
  unity_root="${UNITY_ROOT:-$(git rev-parse --show-toplevel)/../unity}"
  [[ -d "$unity_root/.git" ]] && repos+=("$unity_root")
fi

# Resolve the comparison base for a repo: override > @{u} > origin/main.
resolve_base() {
  local repo="$1"
  if [[ -n "${base_override}" ]]; then
    echo "${base_override}"
  else
    git -C "${repo}" rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null \
      || echo origin/main
  fi
}

# Append one repo's section to the output (stdout, redirected by caller).
emit_repo() {
  local repo="$1" base branch count name
  name="$(basename "$(git -C "${repo}" rev-parse --show-toplevel 2>/dev/null || echo "${repo}")")"

  echo "## ${name}"
  echo

  if ! git -C "${repo}" rev-parse --git-dir >/dev/null 2>&1; then
    echo "_not a git repository: ${repo}_"
    echo
    return
  fi

  base="$(resolve_base "${repo}")"
  branch="$(git -C "${repo}" rev-parse --abbrev-ref HEAD)"

  if ! git -C "${repo}" rev-parse --verify --quiet "${base}" >/dev/null; then
    echo "_base ref \`${base}\` not found — pass one with \`-b <ref>\`._"
    echo
    return
  fi

  count="$(git -C "${repo}" rev-list --count "${base}..HEAD")"
  echo "_${branch} — ${count} local commit(s) ahead of ${base} (\`${repo}\`)._"
  echo

  echo "### Commits"
  echo
  if [[ "${count}" -eq 0 ]]; then
    echo "_(none)_"
  else
    git -C "${repo}" log --reverse --pretty='- `%h` %s' "${base}..HEAD"
  fi
  echo

  echo "### Changed files"
  echo
  echo '```'
  git -C "${repo}" diff --stat "${base}..HEAD"
  echo '```'
  echo

  echo "### Diff"
  echo
  echo '```diff'
  git -C "${repo}" diff "${base}..HEAD"
  echo '```'
  echo
}

{
  echo "# Local diff"
  echo
  echo "_${#repos[@]} repository(ies)._"
  echo
  for repo in "${repos[@]}"; do
    emit_repo "${repo}"
  done
} > "${out}"

echo "→ ${out} (${#repos[@]} repo(s))"
