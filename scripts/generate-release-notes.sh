#!/usr/bin/env bash
set -euo pipefail

tag=${1:?usage: generate-release-notes.sh TAG [OUTPUT] [PREVIOUS_TAG]}
output=${2:-RELEASE_NOTES.md}
previous_tag=${3-__AUTO__}
repository=${GITHUB_REPOSITORY:-}
server_url=${GITHUB_SERVER_URL:-https://github.com}

git rev-parse --verify "${tag}^{commit}" >/dev/null

if [[ "$previous_tag" == "__AUTO__" ]]; then
  previous_tag=$(git describe --tags --abbrev=0 "${tag}^{commit}^" 2>/dev/null || true)
fi

if [[ -n "$previous_tag" ]]; then
  git rev-parse --verify "${previous_tag}^{commit}" >/dev/null
  revision="$previous_tag..$tag"
else
  revision=$tag
fi

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT

features="$tmp_dir/features"
fixes="$tmp_dir/fixes"
performance="$tmp_dir/performance"
documentation="$tmp_dir/documentation"
build_ci="$tmp_dir/build-ci"
tests="$tmp_dir/tests"
other="$tmp_dir/other"
touch "$features" "$fixes" "$performance" "$documentation" "$build_ci" "$tests" "$other"

category_file() {
  local subject=$1
  case "$subject" in
    feat:*|feat\(*\):*|feat!:*|feat\(*\)!:*) printf '%s\n' "$features" ;;
    fix:*|fix\(*\):*|fix!:*|fix\(*\)!:*) printf '%s\n' "$fixes" ;;
    perf:*|perf\(*\):*|perf!:*|perf\(*\)!:*) printf '%s\n' "$performance" ;;
    docs:*|docs\(*\):*|docs!:*|docs\(*\)!:*) printf '%s\n' "$documentation" ;;
    build:*|build\(*\):*|build!:*|build\(*\)!:*|ci:*|ci\(*\):*|ci!:*|ci\(*\)!:*)
      printf '%s\n' "$build_ci"
      ;;
    test:*|test\(*\):*|test!:*|test\(*\)!:*) printf '%s\n' "$tests" ;;
    *) printf '%s\n' "$other" ;;
  esac
}

while IFS=$'\t' read -r sha subject; do
  destination=$(category_file "$subject")
  short_sha=${sha:0:7}
  if [[ -n "$repository" ]]; then
    printf -- '- %s ([%s](%s/%s/commit/%s))\n' \
      "$subject" "$short_sha" "$server_url" "$repository" "$sha" >> "$destination"
  else
    printf -- '- %s (`%s`)\n' "$subject" "$short_sha" >> "$destination"
  fi
done < <(git log --reverse --format='%H%x09%s' "$revision")

commit_count=$(git rev-list --count "$revision")
commit_noun=commits
if [[ "$commit_count" == "1" ]]; then
  commit_noun=commit
fi

: > "$output"
{
  printf '## Release notes\n\n'
  if [[ -n "$previous_tag" ]]; then
    printf 'This release contains %s %s since `%s`.\n\n' \
      "$commit_count" "$commit_noun" "$previous_tag"
  else
    printf 'This release contains %s %s from the initial preview.\n\n' \
      "$commit_count" "$commit_noun"
  fi
} >> "$output"

append_section() {
  local title=$1
  local file=$2
  [[ -s "$file" ]] || return 0
  {
    printf '### %s\n\n' "$title"
    cat "$file"
    printf '\n'
  } >> "$output"
}

append_section "Features" "$features"
append_section "Bug Fixes" "$fixes"
append_section "Performance Improvements" "$performance"
append_section "Documentation" "$documentation"
append_section "Build and CI" "$build_ci"
append_section "Tests" "$tests"
append_section "Other Changes" "$other"

authors="$tmp_dir/authors"
contributors="$tmp_dir/contributors"
git log --format='%aE%x09%aN%x09%H' "$revision" \
  | awk -F '\t' '!seen[tolower($1)]++' > "$authors"

while IFS=$'\t' read -r _email name sha; do
  contributor=$name
  if [[ -n "$repository" && -n "${GH_TOKEN:-}" ]] && command -v gh >/dev/null 2>&1; then
    if login=$(gh api "repos/$repository/commits/$sha" --jq '.author.login // empty' 2>/dev/null) \
      && [[ -n "$login" ]]; then
      contributor="@$login"
    fi
  fi
  printf -- '- %s\n' "$contributor"
done < "$authors" | LC_ALL=C sort -fu > "$contributors"

{
  printf '## Contributors\n\n'
  cat "$contributors"
  printf '\n'
} >> "$output"

if [[ -n "$repository" ]]; then
  if [[ -n "$previous_tag" ]]; then
    changelog_url="$server_url/$repository/compare/$previous_tag...$tag"
  else
    changelog_url="$server_url/$repository/commits/$tag"
  fi
  printf '**Full Changelog**: [%s](%s)\n' "$changelog_url" "$changelog_url" >> "$output"
fi
