#!/usr/bin/env python3
"""Stream a CSV and print a per-column profile plus dataset-level flags.

Standard library only. Usage:
    python3 profile.py data.csv [--delimiter ';'] [--sample 3]
"""
import argparse
import csv
import sys
from datetime import datetime


def is_int(s):
    try:
        int(s)
        return True
    except ValueError:
        return False


def is_float(s):
    try:
        float(s)
        return True
    except ValueError:
        return False


def is_bool(s):
    return s.strip().lower() in {"true", "false", "0", "1", "yes", "no"}


def is_date(s):
    for fmt in ("%Y-%m-%d", "%Y/%m/%d", "%m/%d/%Y", "%Y-%m-%dT%H:%M:%S"):
        try:
            datetime.strptime(s.strip(), fmt)
            return True
        except ValueError:
            continue
    return False


def classify(non_null_values):
    """Infer a column type from its non-null sample of values."""
    if not non_null_values:
        return "empty"
    checks = (("int", is_int), ("float", is_float), ("bool", is_bool), ("date", is_date))
    for name, fn in checks:
        if all(fn(v) for v in non_null_values):
            return name
    return "string"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("path")
    ap.add_argument("--delimiter", default=",")
    ap.add_argument("--sample", type=int, default=3)
    args = ap.parse_args()

    with open(args.path, newline="", encoding="utf-8") as fh:
        reader = csv.reader(fh, delimiter=args.delimiter)
        try:
            header = next(reader)
        except StopIteration:
            print("empty file (no header row)")
            return 1

        cols = len(header)
        nulls = [0] * cols
        non_null = [0] * cols
        distinct = [set() for _ in range(cols)]
        samples = [[] for _ in range(cols)]
        rows = 0

        for row in reader:
            rows += 1
            for i in range(cols):
                val = row[i].strip() if i < len(row) else ""
                if val == "":
                    nulls[i] += 1
                    continue
                non_null[i] += 1
                if len(distinct[i]) < 100000:
                    distinct[i].add(val)
                if len(samples[i]) < args.sample:
                    samples[i].append(val)

    print(f"rows: {rows}    columns: {cols}\n")
    flags = []
    for i, name in enumerate(header):
        col_type = classify(samples[i])
        d = len(distinct[i])
        detail = f"distinct={d} nulls={nulls[i]}"
        print(f"  {name:24} {col_type:7} non_null={non_null[i]:<8} {detail}")
        if non_null[i] == 0:
            flags.append(f"'{name}' is entirely null")
        elif rows and nulls[i] / rows > 0.5:
            flags.append(f"'{name}' is >50% null")
        if col_type == "string" and samples[i] and all(is_float(v) for v in samples[i]):
            flags.append(f"'{name}' looks numeric but is stored as text")
        if rows and non_null[i] == rows and d == rows:
            flags.append(f"'{name}' is unique across all rows (candidate key)")

    print("\nflags:")
    if flags:
        for f in flags:
            print(f"  - {f}")
    else:
        print("  none")
    return 0


if __name__ == "__main__":
    sys.exit(main())
