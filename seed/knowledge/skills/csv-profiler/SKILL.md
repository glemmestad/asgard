---
name: csv-profiler
description: Use when asked to profile, summarize, or sanity-check a CSV file — column types, null counts, ranges, cardinality, and obvious data-quality flags.
---

# CSV profiler

Produce a fast, dependency-free profile of a CSV so you can reason about a dataset
before loading or transforming it. The heavy lifting is a bundled script that uses
only the Python standard library.

## Steps

1. Run the bundled profiler against the file the user names:

   ```bash
   python3 scripts/profile.py path/to/data.csv
   ```

   It prints, per column: inferred type (int / float / bool / date / string),
   non-null count, null count, distinct count, and min/max (numeric) or a few
   sample values (string). It finishes with dataset-level flags.

2. Read the flags it surfaces and call out the ones that matter:
   - columns that are entirely null (candidates to drop),
   - numeric columns stored as strings (parsing/locale issues),
   - high-null-rate columns,
   - a column that looks like a unique key (distinct == row count).

3. Summarize for the user in plain language: shape (rows × columns), the type of
   each column, and the top data-quality risks — then suggest the next step
   (cleaning, a schema, or a load).

## Notes

- The script streams the file row by row, so it is safe on large CSVs.
- It assumes a header row and comma delimiter; pass `--delimiter ';'` to override.
- It never modifies the input file.
