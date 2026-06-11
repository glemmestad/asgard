#!/usr/bin/env bash
# Clean-room self-check.
#
# Fails (non-zero exit) if any employer/internal trace from
# scripts/cleanroom-denylist.txt appears in the tracked working tree. With
# --history it additionally scans the full git history.
#
# Frontkeep is generic OSS authored by an individual: no company names, internal
# hostnames, account ids, or internal service names anywhere in the repo or its
# history. This is wired into CI as a hard gate.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DENYLIST="$ROOT/scripts/cleanroom-denylist.txt"

if [[ ! -f "$DENYLIST" ]]; then
  echo "cleanroom: denylist not found at $DENYLIST" >&2
  exit 2
fi

# Build an ERE alternation from non-comment, non-blank lines.
patterns=()
while IFS= read -r line; do
  line="${line%%$'\r'}"
  [[ -z "$line" || "$line" == \#* ]] && continue
  patterns+=("$line")
done < "$DENYLIST"

if [[ ${#patterns[@]} -eq 0 ]]; then
  echo "cleanroom: denylist is empty" >&2
  exit 2
fi

RE="$(IFS='|'; echo "${patterns[*]}")"

# Exclude VCS-meaningless or self-referential paths from every scan.
PATHSPEC=(
  '.'
  ':(exclude)scripts/cleanroom-denylist.txt'
  ':(exclude)scripts/cleanroom-check.sh'
)

status=0

echo "==> Clean-room: scanning working tree (tracked + untracked, excluding ignored)"
# Drive plain grep off git's file list rather than `git grep`: git grep silently
# skips files it deems binary (a single stray NUL byte is enough), which once let
# a contaminated doc through. grep -a forces every file to be scanned as text so a
# leak can never hide behind a NUL; the explicit file list keeps ignore semantics.
wt_hits="$(git -C "$ROOT" ls-files -z --cached --others --exclude-standard -- "${PATHSPEC[@]}" \
            | LC_ALL=C xargs -0 grep -naE -i -e "$RE" -- 2>/dev/null || true)"
if [[ -n "$wt_hits" ]]; then
  printf '%s\n' "$wt_hits" >&2
  echo "FAIL: employer/internal trace(s) found in working tree (above)." >&2
  status=1
else
  echo "OK: working tree clean."
fi

if [[ "${1:-}" == "--history" ]]; then
  # Scan the current branch's publishable lineage (what lands on main), not
  # every local ref: build hosts and IDE tooling may keep auxiliary refs
  # (e.g. editor checkpoints of ignored files) that are never pushed.
  echo "==> Clean-room: scanning git history (current branch lineage)"
  hits="$(git -C "$ROOT" log --no-color -p -- "${PATHSPEC[@]}" \
            | grep -naE -i -e "$RE" || true)"
  if [[ -n "$hits" ]]; then
    echo "FAIL: trace(s) found in git history:" >&2
    printf '%s\n' "$hits" | head -n 40 >&2
    status=1
  else
    echo "OK: history clean."
  fi
fi

if [[ $status -eq 0 ]]; then
  echo "Clean-room check passed."
fi
exit $status
