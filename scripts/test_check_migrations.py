#!/usr/bin/env python3
"""Unit tests for check_migrations.py (KAIROS-T-0639). Stdlib only; run with
`python3 -m unittest scripts/test_check_migrations.py` (or `angreal test
unit`, which includes it).

The tests write small migration trees in a temporary folder. The last test
runs the check on the repository, so a bad new migration fails here too."""

import importlib.util
import os
import re
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "check_migrations.py")
SPEC = importlib.util.spec_from_file_location("check_migrations", SCRIPT)
check = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(check)

NO_TRANSACTION = "run_in_transaction = false\n"


class Tree:
    """A migration tree in a temporary folder, with a before list."""

    def __init__(self):
        self.dir = tempfile.TemporaryDirectory()
        self.root = os.path.join(self.dir.name, "migrations")
        self.before = os.path.join(self.dir.name, "before.txt")
        os.makedirs(os.path.join(self.root, "tenant"))
        os.makedirs(os.path.join(self.root, "public"))
        self.known = []

    def add(self, name, up, down=None, metadata=None, tree="tenant", known=False):
        path = os.path.join(self.root, tree, name)
        os.makedirs(path)
        with open(os.path.join(path, "up.sql"), "w") as f:
            f.write(up)
        if down is not None:
            with open(os.path.join(path, "down.sql"), "w") as f:
                f.write(down)
        if metadata is not None:
            with open(os.path.join(path, "metadata.toml"), "w") as f:
                f.write(metadata)
        if known:
            self.known.append(f"{tree}/{name}")

    def problems(self):
        with open(self.before, "w") as f:
            f.write("# known\n" + "\n".join(self.known) + "\n")
        return check.check(self.root, self.before)

    def close(self):
        self.dir.cleanup()


class CheckTest(unittest.TestCase):
    def setUp(self):
        self.tree = Tree()

    def tearDown(self):
        self.tree.close()

    def assertRefused(self, fragment):
        problems = self.tree.problems()
        self.assertTrue(
            any(fragment in p for p in problems),
            f"no problem with {fragment!r} in {problems}",
        )

    def assertPasses(self):
        self.assertEqual(self.tree.problems(), [])

    # --- refused -----------------------------------------------------------

    def test_a_plain_index_on_a_large_table(self):
        self.tree.add(
            "2026-10-18-000000_idx",
            "CREATE INDEX IF NOT EXISTS n_age ON notifications (created_at);",
        )
        self.assertRefused("plain CREATE INDEX on the large table notifications")

    def test_a_unique_index_on_a_large_public_table_with_its_schema(self):
        self.tree.add(
            "2026-10-18-000000_idx",
            'CREATE UNIQUE INDEX jobs_key ON public."jobs" (dedupe_key);',
            tree="public",
        )
        self.assertRefused("large table jobs")

    def test_a_check_without_not_valid(self):
        self.tree.add(
            "2026-10-18-000000_kind",
            "ALTER TABLE notifications DROP CONSTRAINT IF EXISTS notifications_kind_check;\n"
            "ALTER TABLE notifications ADD CONSTRAINT notifications_kind_check "
            "CHECK (kind IN ('a', 'b'));",
        )
        self.assertRefused("adds a CHECK to the large table notifications without NOT VALID")

    def test_a_foreign_key_without_not_valid(self):
        self.tree.add(
            "2026-10-18-000000_fk",
            "ALTER TABLE activity_log ADD COLUMN x UUID, "
            "ADD FOREIGN KEY (x) REFERENCES tasks (id);",
        )
        self.assertRefused("adds a FOREIGN KEY to the large table activity_log")

    def test_validate_in_the_migration_that_adds(self):
        self.tree.add(
            "2026-10-18-000000_kind",
            "ALTER TABLE notifications ADD CONSTRAINT k CHECK (kind <> '') NOT VALID;\n"
            "ALTER TABLE notifications VALIDATE CONSTRAINT k;",
        )
        self.assertRefused("adds the constraint k NOT VALID and validates it")

    def test_concurrently_in_a_transaction(self):
        self.tree.add(
            "2026-10-18-000000_idx",
            "CREATE INDEX CONCURRENTLY n_age ON notifications (created_at);",
        )
        self.assertRefused("up.sql has CONCURRENTLY")

    def test_concurrently_in_the_down_in_a_transaction(self):
        self.tree.add(
            "2026-10-18-000000_idx",
            "CREATE INDEX n_age ON boards (created_at);",
            down="DROP INDEX CONCURRENTLY IF EXISTS n_age;",
        )
        self.assertRefused("down.sql has CONCURRENTLY")

    def test_two_statements_outside_a_transaction(self):
        self.tree.add(
            "2026-10-18-000000_idx",
            "CREATE INDEX CONCURRENTLY a ON notifications (created_at);\n"
            "CREATE INDEX CONCURRENTLY b ON notifications (read_at);",
            metadata=NO_TRANSACTION,
        )
        self.assertRefused("up.sql has 2 statements")

    def test_no_transaction_without_concurrently(self):
        self.tree.add(
            "2026-10-18-000000_col",
            "ALTER TABLE boards ADD COLUMN x TEXT;",
            metadata=NO_TRANSACTION,
        )
        self.assertRefused("up.sql has no CONCURRENTLY")

    def test_concurrently_with_if_not_exists(self):
        self.tree.add(
            "2026-10-18-000000_idx",
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS n_age ON notifications (created_at);",
            metadata=NO_TRANSACTION,
        )
        self.assertRefused("IF NOT EXISTS hides it")

    def test_no_up_sql(self):
        os.makedirs(os.path.join(self.tree.root, "tenant", "2026-10-18-000000_empty"))
        self.assertRefused("it has no up.sql")

    # --- passes ------------------------------------------------------------

    def test_an_index_concurrently_in_a_migration_of_its_own(self):
        self.tree.add(
            "2026-10-18-000000_idx",
            "-- The age of a notification.\n"
            "CREATE INDEX CONCURRENTLY n_age ON notifications (created_at)\n"
            "    WHERE read_at IS NULL;\n",
            down="DROP INDEX CONCURRENTLY IF EXISTS n_age;",
            metadata=NO_TRANSACTION,
        )
        self.assertPasses()

    def test_not_valid_then_validate_in_the_next_migration(self):
        self.tree.add(
            "2026-10-18-000000_kind",
            "ALTER TABLE notifications DROP CONSTRAINT IF EXISTS k;\n"
            "ALTER TABLE notifications ADD CONSTRAINT k CHECK (kind IN ('a', 'b')) NOT VALID;",
        )
        self.tree.add(
            "2026-10-18-000001_kind_validate",
            "ALTER TABLE notifications VALIDATE CONSTRAINT k;",
        )
        self.assertPasses()

    def test_a_table_of_the_same_migration(self):
        self.tree.add(
            "2026-10-18-000000_notes",
            "CREATE TABLE IF NOT EXISTS item_notes (id UUID PRIMARY KEY, item_id UUID,\n"
            "    CONSTRAINT c CHECK (item_id IS NOT NULL));\n"
            "CREATE INDEX item_notes_item ON item_notes (item_id);\n"
            "ALTER TABLE item_notes ADD CONSTRAINT d CHECK (id IS NOT NULL);",
        )
        self.assertPasses()

    def test_a_plain_index_on_a_small_table(self):
        self.tree.add(
            "2026-10-18-000000_idx",
            "CREATE UNIQUE INDEX IF NOT EXISTS one_parent ON item_relationships (target_id)\n"
            "    WHERE relationship = 'parent';",
        )
        self.assertPasses()

    def test_comments_strings_and_bodies_are_not_statements(self):
        self.tree.add(
            "2026-10-18-000000_text",
            "-- CREATE INDEX a ON notifications (x);\n"
            "/* ALTER TABLE notifications ADD CHECK (x); /* nested */ */\n"
            "INSERT INTO boards (name) VALUES ('CREATE INDEX b ON notifications (x); it''s');\n"
            "DO $move$ BEGIN EXECUTE 'CREATE INDEX c ON notifications (x)'; END $move$;\n"
            "CREATE FUNCTION f() RETURNS void AS $$ SELECT 1; $$ LANGUAGE sql;",
        )
        self.assertPasses()

    def test_a_migration_of_the_before_list(self):
        self.tree.add(
            "2026-10-12-000002_webhook_deliveries",
            "CREATE INDEX n_age ON notifications (created_at);",
            known=True,
        )
        self.assertPasses()

    def test_statements_ignore_case_and_space(self):
        self.assertEqual(
            check.statements("Create   INDEX\n x ON t (a);;\n\n"),
            ["create index x on t (a)"],
        )

    # --- the repository ----------------------------------------------------

    def test_the_tree_of_the_repository_passes(self):
        repo = os.path.dirname(HERE)
        problems = check.check(
            os.path.join(repo, check.ROOT), os.path.join(repo, check.BEFORE)
        )
        self.assertEqual(problems, [])

    def test_the_before_list_has_names_of_migrations(self):
        # A name can be of an open pull request, not yet in the tree.
        repo = os.path.dirname(HERE)
        before = check.known(os.path.join(repo, check.BEFORE))
        form = re.compile(r"^(public|tenant)/\d{4}-\d{2}-\d{2}-\d{6}_[a-z0-9_]+$")
        self.assertEqual(sorted(n for n in before if not form.match(n)), [])


if __name__ == "__main__":
    unittest.main()
