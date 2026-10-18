#!/usr/bin/env python3
"""check_migrations.py — a new migration takes no long lock on a large table
(KAIROS-T-0639, KAIROS-A-0062).

The convention:

  * An index on a large table is built with CONCURRENTLY, in a migration of
    its own. Its `metadata.toml` has `run_in_transaction = false`, and each
    SQL file has one statement.
  * A CHECK or a FOREIGN KEY on a large table is added NOT VALID. A later
    migration runs VALIDATE CONSTRAINT.
  * The migration connection has a `lock_timeout` (kairos-db, pool.rs). A
    migration that waits for a lock fails, and the server log names it.

The check reads the SQL files as text. It needs no build and no database.
It checks each migration that `scripts/migrations-before-lint.txt` does not
name: the migrations of main from before this check stay as they are.

The rules for a new migration:

  1. No plain CREATE INDEX on a large table.
  2. No ADD CHECK or ADD FOREIGN KEY without NOT VALID on a large table.
  3. No VALIDATE CONSTRAINT in the migration that adds the constraint.
  4. CONCURRENTLY only with `run_in_transaction = false`.
  5. A migration with `run_in_transaction = false` has CONCURRENTLY and one
     statement in each SQL file.
  6. No CREATE INDEX CONCURRENTLY IF NOT EXISTS. After a failure, Postgres
     keeps an INVALID index, and IF NOT EXISTS hides it.

A table that the same migration creates is empty, so rules 1 and 2 do not
apply to it. The large tables are in LARGE_TABLES: the tables that the
server writes for each event, not for each item that a person makes.

  check_migrations.py                  check the repository; exit 1 with a
                                       list of the problems
  check_migrations.py --root DIR --before FILE
                                       check another tree
"""

import argparse
import os
import re
import sys

ROOT = os.path.join("crates", "kairos-db", "migrations")
BEFORE = os.path.join("scripts", "migrations-before-lint.txt")

# The server writes a row of these tables for each event: a log, a history,
# a notification, a delivery, a vector, a code index, a session or a job.
# Each one grows without a limit, and an index build on it takes long.
LARGE_TABLES = frozenset({
    # tenant schema
    "activity_log",
    "code_index_builds",
    "code_index_summaries",
    "code_indexes",
    "forge_refs",
    "github_pr_comments",
    "item_chunks",
    "item_embeddings",
    "item_history",
    "item_notes",
    "notification_reads",
    "notifications",
    "team_page_history",
    "webhook_deliveries",
    "webhook_delivery_tries",
    # public schema
    "auth_failures",
    "jobs",
    "local_sessions",
})

DOLLAR_TAG = re.compile(r"\$([A-Za-z_][A-Za-z0-9_]*)?\$")
NO_TRANSACTION = re.compile(r"^\s*run_in_transaction\s*=\s*false\s*$", re.MULTILINE)
CONCURRENTLY = re.compile(r"\bconcurrently\b")
CREATE_TABLE = re.compile(
    r"^create\s+(?:unlogged\s+)?table\s+(?:if\s+not\s+exists\s+)?([\w\".]+)"
)
CREATE_INDEX = re.compile(
    r"^create\s+(?:unique\s+)?index\s+(concurrently\s+)?(if\s+not\s+exists\s+)?"
    r"(?:(?!on\b)[\w\".]+\s+)?on\s+(?:only\s+)?([\w\".]+)"
)
ALTER_TABLE = re.compile(
    r"^alter\s+table\s+(?:if\s+exists\s+)?(?:only\s+)?([\w\".]+)\s+(.*)$", re.DOTALL
)
ADD_CHECK_OR_KEY = re.compile(
    r"^add\s+(?:constraint\s+([\w\"]+)\s+)?(check|foreign\s+key)\b"
)
ADD_CONSTRAINT = re.compile(r"^add\s+constraint\s+([\w\"]+)\s")
VALIDATE = re.compile(r"^validate\s+constraint\s+([\w\"]+)")


def blank_sql(text):
    """`text` without comments, and with each string and each dollar-quoted
    body made empty. The statements and their `;` stay."""
    out = []
    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        if text.startswith("--", i):
            end = text.find("\n", i)
            i = n if end < 0 else end
            out.append(" ")
            continue
        if text.startswith("/*", i):
            depth = 0
            while i < n:
                if text.startswith("/*", i):
                    depth += 1
                    i += 2
                elif text.startswith("*/", i):
                    depth -= 1
                    i += 2
                    if depth == 0:
                        break
                else:
                    i += 1
            out.append(" ")
            continue
        if c == "'":
            escapes = i > 0 and text[i - 1] in "eE"
            i += 1
            while i < n:
                if escapes and text[i] == "\\":
                    i += 2
                    continue
                if text[i] == "'":
                    if text.startswith("''", i):
                        i += 2
                        continue
                    break
                i += 1
            i += 1
            out.append("''")
            continue
        if c == "$":
            tag = DOLLAR_TAG.match(text, i)
            if tag:
                close = text.find(tag.group(0), tag.end())
                i = n if close < 0 else close + len(tag.group(0))
                out.append("$$")
                continue
        out.append(c)
        i += 1
    return "".join(out)


def statements(text):
    """The statements of an SQL file, lower case, one space between words."""
    parts = blank_sql(text).split(";")
    return [" ".join(p.split()).lower() for p in parts if p.strip()]


def table_name(name):
    """The table of a name: no quotes, no schema."""
    return name.replace('"', "").split(".")[-1]


def actions(rest):
    """The actions of an ALTER TABLE: split at each comma outside brackets."""
    found = []
    depth = 0
    start = 0
    for i, c in enumerate(rest):
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
        elif c == "," and depth == 0:
            found.append(rest[start:i].strip())
            start = i + 1
    found.append(rest[start:].strip())
    return [a for a in found if a]


def read(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


def check_migration(path):
    """The problems of one migration folder, as sentences."""
    problems = []
    up_path = os.path.join(path, "up.sql")
    if not os.path.isfile(up_path):
        return ["it has no up.sql."]
    meta_path = os.path.join(path, "metadata.toml")
    no_transaction = os.path.isfile(meta_path) and bool(NO_TRANSACTION.search(read(meta_path)))

    files = [("up.sql", statements(read(up_path)))]
    down_path = os.path.join(path, "down.sql")
    if os.path.isfile(down_path):
        files.append(("down.sql", statements(read(down_path))))

    # Rules 4, 5 and 6: CONCURRENTLY and the transaction.
    for file_name, stmts in files:
        concurrent = [s for s in stmts if CONCURRENTLY.search(s)]
        if concurrent and not no_transaction:
            problems.append(
                f"{file_name} has CONCURRENTLY. Put `run_in_transaction = false` in "
                "metadata.toml: Postgres refuses CONCURRENTLY in a transaction."
            )
        if no_transaction and len(stmts) != 1:
            problems.append(
                f"{file_name} has {len(stmts)} statements. A migration outside a "
                "transaction must have one statement: a failure must not leave half "
                "of it."
            )
        if no_transaction and file_name == "up.sql" and not concurrent:
            problems.append(
                "up.sql has no CONCURRENTLY. Use `run_in_transaction = false` only "
                "for CONCURRENTLY."
            )
        for s in concurrent:
            index = CREATE_INDEX.match(s)
            if index and index.group(2):
                problems.append(
                    f"{file_name} has CREATE INDEX CONCURRENTLY IF NOT EXISTS. Remove "
                    "IF NOT EXISTS: after a failure Postgres keeps an INVALID index, "
                    "and IF NOT EXISTS hides it."
                )

    # Rules 1, 2 and 3: the locks of up.sql.
    up = files[0][1]
    created = {table_name(m.group(1)) for m in map(CREATE_TABLE.match, up) if m}
    added_not_valid = set()
    validated = set()
    for s in up:
        index = CREATE_INDEX.match(s)
        if index:
            table = table_name(index.group(3))
            if table in LARGE_TABLES and table not in created and not index.group(1):
                problems.append(
                    f"up.sql has a plain CREATE INDEX on the large table {table}. "
                    "Use CREATE INDEX CONCURRENTLY in a migration of its own, with "
                    "`run_in_transaction = false`."
                )
            continue
        alter = ALTER_TABLE.match(s)
        if not alter:
            continue
        table = table_name(alter.group(1))
        for action in actions(alter.group(2)):
            named = ADD_CONSTRAINT.match(action)
            if named and "not valid" in action:
                added_not_valid.add(named.group(1).replace('"', ""))
            check = ADD_CHECK_OR_KEY.match(action)
            if (
                check
                and table in LARGE_TABLES
                and table not in created
                and "not valid" not in action
            ):
                kind = "CHECK" if check.group(2) == "check" else "FOREIGN KEY"
                problems.append(
                    f"up.sql adds a {kind} to the large table {table} without NOT "
                    "VALID. Add it NOT VALID, and run VALIDATE CONSTRAINT in the next "
                    "migration."
                )
            validate = VALIDATE.match(action)
            if validate:
                validated.add(validate.group(1).replace('"', ""))
    for name in sorted(added_not_valid & validated):
        problems.append(
            f"up.sql adds the constraint {name} NOT VALID and validates it. Run "
            "VALIDATE CONSTRAINT in the next migration: here it scans the table "
            "under the lock of the ADD."
        )
    return problems


def migrations(root):
    """Each migration of the trees under `root`, as `tree/name`."""
    found = []
    for tree in sorted(os.listdir(root)):
        tree_path = os.path.join(root, tree)
        if not os.path.isdir(tree_path):
            continue
        for name in sorted(os.listdir(tree_path)):
            if os.path.isdir(os.path.join(tree_path, name)) and not name.startswith("."):
                found.append(f"{tree}/{name}")
    return found


def known(before):
    """The migrations that the check does not check."""
    names = set()
    for line in read(before).splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            names.add(line)
    return names


def check(root, before):
    """Each problem of each new migration, as `path: sentence`."""
    skip = known(before)
    problems = []
    for migration in migrations(root):
        if migration in skip:
            continue
        path = os.path.join(root, migration)
        for problem in check_migration(path):
            problems.append(f"{path}: {problem}")
    return problems


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--root", default=ROOT)
    parser.add_argument("--before", default=BEFORE)
    args = parser.parse_args(argv)
    problems = check(args.root, args.before)
    for problem in problems:
        print(problem)
    if problems:
        print(
            f"{len(problems)} problem(s). See the migration convention in "
            "scripts/check_migrations.py."
        )
        return 1
    print("The new migrations follow the migration convention.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
