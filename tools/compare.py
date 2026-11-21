#!/usr/bin/env python3
"""Exact, order-insensitive diff of an output folder against expected CSVs.

Usage: compare.py OUT_DIR EXPECTED_DIR      (needs: pip install duckdb)

OUT_DIR holds user_daily/*.parquet and campaign_daily/*.parquet, as the
pipeline writes them. EXPECTED_DIR holds user_daily.csv and campaign_daily.csv,
as tools/reference.py and the track's dev-answer.zip do.
"""
import sys

import duckdb

TABLES = {
    "user_daily": "user_id VARCHAR, day DATE, sessions BIGINT, events BIGINT, revenue_cents BIGINT, top_category VARCHAR",
    "campaign_daily": "campaign_id VARCHAR, channel VARCHAR, day DATE, sessions BIGINT, purchasing_sessions BIGINT, "
    "revenue_cents BIGINT, assisted_sessions BIGINT",
}


def main():
    out, expected = sys.argv[1:3]
    same = True
    for table, columns in TABLES.items():
        names = [column.split()[0] for column in columns.split(", ")]
        casts = ", ".join(f"CAST({column.split()[0]} AS {column.split()[1]}) AS {column.split()[0]}" for column in columns.split(", "))
        spec = ", ".join(f"'{column.split()[0]}': 'VARCHAR'" for column in columns.split(", "))
        duckdb.sql(f"CREATE OR REPLACE TABLE a AS SELECT {casts} FROM read_parquet('{out}/{table}/*.parquet')")
        found = [row[0] for row in duckdb.sql("DESCRIBE a").fetchall()]
        if sorted(found) != sorted(names):
            print(f"{table}: columns {found}, expected {names}")
            same = False
            continue
        # Read the CSV as text so an empty top_category stays distinguishable, then cast.
        duckdb.sql(
            f"CREATE OR REPLACE TABLE b AS SELECT {casts} FROM read_csv('{expected}/{table}.csv', header=true, "
            f"columns={{{spec}}}, nullstr='')"
        )
        only_a = duckdb.sql("SELECT count(*) FROM (SELECT * FROM a EXCEPT ALL SELECT * FROM b)").fetchone()[0]
        only_b = duckdb.sql("SELECT count(*) FROM (SELECT * FROM b EXCEPT ALL SELECT * FROM a)").fetchone()[0]
        rows_a = duckdb.sql("SELECT count(*) FROM a").fetchone()[0]
        rows_b = duckdb.sql("SELECT count(*) FROM b").fetchone()[0]
        print(f"{table}: rows {rows_a} vs {rows_b}; only in output {only_a}; only in expected {only_b}")
        if only_a or only_b:
            same = False
            print(duckdb.sql("SELECT * FROM (SELECT * FROM a EXCEPT ALL SELECT * FROM b) ORDER BY 1, 2 LIMIT 5"))
            print(duckdb.sql("SELECT * FROM (SELECT * FROM b EXCEPT ALL SELECT * FROM a) ORDER BY 1, 2 LIMIT 5"))
    print("MATCH" if same else "DIFFER")
    return 0 if same else 1


if __name__ == "__main__":
    sys.exit(main())
