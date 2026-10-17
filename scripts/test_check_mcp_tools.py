#!/usr/bin/env python3
"""Unit tests for check_mcp_tools.py (KAIROS-T-0563). Stdlib only; run with
`python3 -m unittest scripts/test_check_mcp_tools.py` (or `angreal test unit`,
which includes it).

The tests drive the check with small texts. The last test runs it on the
repository's own files, so a tool with no section on the page fails here too."""

import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "check_mcp_tools.py")
SPEC = importlib.util.spec_from_file_location("check_mcp_tools", SCRIPT)
check = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(check)

SOURCE = """\
//! The tool surface: later tasks added more (2 in all, in the profiles).

// The tools (2; the list of each profile is asserted in tests/mcp.rs)

impl KairosMcp {
    #[tool(
        description = "Who am I."
    )]
    pub async fn whoami(&self) {}

    #[tool(description = "Find items.")]
    pub async fn search(&self) {}

    async fn helper(&self) {}
}
"""

PAGE = """\
The surface is exactly
two tools. A drift gate in the test suite
asserts that `tools/list` returns these two and no others.

| `/mcp` | work | team pages to read: 1 tools. |
| `/mcp?profile=admin` | admin | purge: the other 1 tools. |

### `whoami`

Who am I.

### `search`

Find items.
"""


class CheckTest(unittest.TestCase):
    def test_the_texts_agree(self):
        self.assertEqual(check.problems(SOURCE, PAGE), [])

    def test_a_tool_with_no_section(self):
        page = PAGE.replace("### `search`", "### `seek`")
        found = check.problems(SOURCE, page)
        self.assertTrue(any("no section for the tool `search`" in line for line in found), found)
        self.assertTrue(any("the section `seek` names no tool" in line for line in found), found)

    def test_a_heading_in_a_code_fence_is_not_a_section(self):
        page = PAGE.replace("### `search`", "```text\n### `search`\n```")
        found = check.problems(SOURCE, page)
        self.assertTrue(any("no section for the tool `search`" in line for line in found), found)

    def test_a_section_after_a_closed_fence_counts(self):
        page = PAGE.replace("Who am I.", "```text\n## Renames\n```")
        self.assertEqual(check.problems(SOURCE, page), [])

    def test_a_wrong_count_on_the_page(self):
        page = PAGE.replace("exactly\ntwo tools", "exactly\nthree tools")
        found = check.problems(SOURCE, page)
        self.assertTrue(any("'exactly <count> tools' gives 3" in line for line in found), found)

    def test_a_wrong_count_in_the_module_docs(self):
        source = SOURCE.replace("(2 in all", "(67 in all")
        found = check.problems(source, PAGE)
        self.assertTrue(any("'(<count> in all' gives 67" in line for line in found), found)

    def test_a_missing_count(self):
        source = SOURCE.replace("// The tools (2;", "// The tools;")
        found = check.problems(source, PAGE)
        self.assertTrue(any("no count of the form '// The tools" in line for line in found), found)

    def test_profiles_that_do_not_add_up(self):
        page = PAGE.replace("the other 1 tools.", "the other 2 tools.")
        found = check.problems(SOURCE, page)
        self.assertTrue(any("the profiles give 1 + 2 tools" in line for line in found), found)

    def test_numbers_in_words(self):
        self.assertEqual(check.number("seventy"), 70)
        self.assertEqual(check.number("sixty-seven"), 67)
        self.assertEqual(check.number("seventeen"), 17)
        self.assertEqual(check.number("70"), 70)
        self.assertIsNone(check.number("many"))

    def test_the_repository_agrees(self):
        root = os.path.dirname(HERE)
        with open(os.path.join(root, check.TOOLS), encoding="utf-8") as handle:
            source = handle.read()
        with open(os.path.join(root, check.PAGE), encoding="utf-8") as handle:
            page = handle.read()
        self.assertEqual(check.problems(source, page), [])


if __name__ == "__main__":
    unittest.main()
