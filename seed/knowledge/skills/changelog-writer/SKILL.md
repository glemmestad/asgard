---
name: changelog-from-commits
description: Use when cutting a release to draft a changelog from the git log since the last tag, grouped by Conventional Commit type.
---

# Changelog from commits

Turn the commits since the last release tag into a clean, grouped changelog. This
skill is instructions only — it drives `git`, which is already on PATH.

## Steps

1. Find the last release tag and the range to summarize:

   ```bash
   git describe --tags --abbrev=0   # last tag, e.g. v1.2.0
   git log <last-tag>..HEAD --no-merges --pretty=format:'%s'
   ```

2. Bucket each subject line by its Conventional Commit prefix:
   - `feat:` → **Added**
   - `fix:` → **Fixed**
   - `perf:` / `refactor:` → **Changed**
   - `docs:` / `test:` / `chore:` / `build:` / `ci:` → **Internal** (collapse; usually omit from user-facing notes)
   - anything else → **Other**, and flag it so the author can re-title the commit.

3. Within each bucket, rewrite terse subjects into reader-facing lines: drop the
   prefix, start with a verb, keep one line each. Strip trailing issue refs into a
   `(#123)` suffix.

4. Propose the next version from the buckets (any **Added** → minor bump; only
   **Fixed**/**Changed** → patch; a breaking change → major) and emit:

   ```markdown
   ## <next-version> — <today>
   ### Added
   - …
   ### Fixed
   - …
   ### Changed
   - …
   ```

## Notes

- Never invent entries — every line must map to a real commit in the range.
- If the range is empty, say so instead of producing an empty changelog.
- Don't write to `CHANGELOG.md` unless the author asks; print the draft first.
