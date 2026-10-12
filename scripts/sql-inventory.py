#!/usr/bin/env python3
"""Inventory SQL-bearing Rust literals and SQL files without executing them.

Includes dynamically formatted SQL and fragments, with their source locations. This
is a lexical inventory, not a Rust interpreter: it does not expand format arguments,
QueryBuilder calls or plugin-generated SQL. The default scope is application code,
migrations and scripts; --tests additionally includes Rust test fixtures.
"""
import argparse
import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# Consume comments before strings so commented-out SQL is not counted. Raw strings
# may contain double quotes; ordinary strings may escape quotes and span lines.
TOKEN = re.compile(
    r'//[^\n]*|/\*.*?\*/|r(?P<hashes>\#*)"(?P<raw>.*?)"(?P=hashes)|"(?P<normal>(?:\\.|[^"\\])*)"',
    re.S,
)
SQL = re.compile(
    r"^\s*(?:SELECT|INSERT|UPDATE|DELETE|WITH|CREATE|ALTER|DROP|SET|ANALYZE|VACUUM|TRUNCATE|REINDEX)\b|\b(?:SELECT|INSERT\s+INTO|UPDATE\s+\w+\s+SET|DELETE\s+FROM|CREATE\s+(?:TABLE|INDEX|FUNCTION)|ALTER\s+TABLE|DROP\s+(?:TABLE|INDEX)|FROM\s+\w+\s+(?:WHERE|JOIN)|SET\s+LOCAL|WHERE\s+|ORDER\s+BY|ON\s+CONFLICT)\b",
    re.I,
)


def inventory(include_tests=False):
    entries = []
    roots = ["src", "migrations", "scripts"] + (["tests"] if include_tests else [])
    for root in roots:
        for path in sorted((ROOT / root).rglob("*")):
            if path.suffix == ".sql":
                entries.append(
                    dict(
                        file=str(path.relative_to(ROOT)),
                        line=1,
                        kind="sql_file",
                        sql=path.read_text(),
                    )
                )
            elif path.suffix == ".rs":
                source = path.read_text()
                for match in TOKEN.finditer(source):
                    literal = (
                        match.group("raw")
                        if match.group("raw") is not None
                        else match.group("normal")
                    )
                    if literal is None or not SQL.search(literal):
                        continue
                    entries.append(
                        dict(
                            file=str(path.relative_to(ROOT)),
                            line=source.count("\n", 0, match.start()) + 1,
                            kind="rust_literal_or_fragment",
                            sql=literal,
                        )
                    )
    return entries


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tests", action="store_true")
    args = parser.parse_args()
    entries = inventory(args.tests)
    print(
        json.dumps(
            {
                "entries": entries,
                "entry_count": len(entries),
                "file_count": len({e["file"] for e in entries}),
            },
            indent=2,
        )
    )
