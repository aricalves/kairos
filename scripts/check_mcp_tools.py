#!/usr/bin/env python3
"""check_mcp_tools.py — the MCP tools page names each tool of the code
(KAIROS-T-0563).

`tests/mcp.rs` compares `tools/list` with the page, but it needs a database.
This check reads two files as text. It needs no Rust build and no database:

  * `crates/kairos-server/src/mcp/tools.rs`: each `#[tool(` attribute and the
    name of the function after it.
  * `docs/src/content/docs/reference/mcp-tools.md`: each section heading
    `### \`name\``. A heading in a code fence is not a section.

The rules:

  1. Each tool of the code has a section, and each section names a tool of
     the code.
  2. The page gives the count of the code: "exactly <count> tools" and
     "returns these <count> and no others", in words or in digits.
  3. The two profiles of the page add up to the count: "<n> tools." of the
     work profile and "the other <n> tools." of the admin profile.
  4. The module docs of `tools.rs` give the count: "(<count> in all" and
     "// The tools (<count>;".

  check_mcp_tools.py        check the repository; exit 1 with a list of the
                            problems
"""

import os
import re
import sys

TOOLS = os.path.join("crates", "kairos-server", "src", "mcp", "tools.rs")
PAGE = os.path.join("docs", "src", "content", "docs", "reference", "mcp-tools.md")

TOOL_ATTRIBUTE = re.compile(r"^\s*#\[tool\(")
FUNCTION = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([a-z_][a-z0-9_]*)")
SECTION = re.compile(r"^### `([a-z_][a-z0-9_]*)`\s*$")
FENCE = re.compile(r"^\s*(```|~~~)")

UNITS = {
    "zero": 0, "one": 1, "two": 2, "three": 3, "four": 4, "five": 5, "six": 6,
    "seven": 7, "eight": 8, "nine": 9, "ten": 10, "eleven": 11, "twelve": 12,
    "thirteen": 13, "fourteen": 14, "fifteen": 15, "sixteen": 16,
    "seventeen": 17, "eighteen": 18, "nineteen": 19,
}
TENS = {
    "twenty": 20, "thirty": 30, "forty": 40, "fifty": 50, "sixty": 60,
    "seventy": 70, "eighty": 80, "ninety": 90,
}
NUMBER = r"([0-9]+|[a-z]+(?:-[a-z]+)?)"


def number(text):
    """The value of a count in digits or in words ("seventy", "sixty-seven"),
    or None."""
    text = text.lower()
    if text.isdigit():
        return int(text)
    if text in UNITS:
        return UNITS[text]
    if text in TENS:
        return TENS[text]
    tens, _, unit = text.partition("-")
    if tens in TENS and unit in UNITS and 0 < UNITS[unit] < 10:
        return TENS[tens] + UNITS[unit]
    return None


def code_tools(source):
    """The names of the functions with a `#[tool(` attribute, in order."""
    names = []
    waiting = False
    for line in source.splitlines():
        if TOOL_ATTRIBUTE.match(line):
            if waiting:
                names.append(None)
            waiting = True
            continue
        if waiting:
            found = FUNCTION.match(line)
            if found:
                names.append(found.group(1))
                waiting = False
    if waiting:
        names.append(None)
    return names


def page_tools(page):
    """The tool names of the `### \\`name\\`` headings outside code fences."""
    names = []
    fence = None
    for line in page.splitlines():
        marker = FENCE.match(line)
        if marker:
            if fence is None:
                fence = marker.group(1)
            elif marker.group(1) == fence:
                fence = None
            continue
        if fence is None:
            found = SECTION.match(line)
            if found:
                names.append(found.group(1))
    return names


def stated(pattern, text):
    """Each count that the pattern finds in the text, as a number (None for a
    word that is not a number)."""
    return [number(found) for found in re.findall(pattern, text)]


def problems(source, page):
    """Each disagreement between `tools.rs` and the page, as a line of text."""
    out = []
    code = code_tools(source)
    if None in code:
        out.append(f"{TOOLS}: a #[tool( attribute has no function after it")
    code = [name for name in code if name]
    sections = page_tools(page)

    for name in sorted(set(code) - set(sections)):
        out.append(f"{PAGE}: no section for the tool `{name}` of {TOOLS}")
    for name in sorted(set(sections) - set(code)):
        out.append(f"{PAGE}: the section `{name}` names no tool of {TOOLS}")
    for name in sorted({name for name in code if code.count(name) > 1}):
        out.append(f"{TOOLS}: the tool `{name}` is there more than one time")
    for name in sorted({name for name in sections if sections.count(name) > 1}):
        out.append(f"{PAGE}: the section `{name}` is there more than one time")

    count = len(code)
    checks = [
        (PAGE, "exactly <count> tools", r"exactly\s+" + NUMBER + r"\s+tools", page),
        (PAGE, "returns these <count> and no others",
         r"returns\s+these\s+" + NUMBER + r"\s+and\s+no\s+others", page),
        (TOOLS, "(<count> in all", r"\((" + r"[0-9]+" + r")\s+in\s+all", source),
        (TOOLS, "// The tools (<count>;", r"//\s*The tools \(([0-9]+);", source),
    ]
    for path, form, pattern, text in checks:
        values = stated(pattern, text)
        if not values:
            out.append(f"{path}: no count of the form {form!r}")
        for value in values:
            if value != count:
                out.append(f"{path}: {form!r} gives {value}, and {TOOLS} has {count} tools")

    work = stated(r"team pages to read:\s+" + NUMBER + r"\s+tools\.", page)
    admin = stated(r"the other\s+" + NUMBER + r"\s+tools\.", page)
    if len(work) != 1 or len(admin) != 1 or None in work + admin:
        out.append(f"{PAGE}: the profile table does not give the work and the admin count")
    elif work[0] + admin[0] != count:
        out.append(
            f"{PAGE}: the profiles give {work[0]} + {admin[0]} tools, "
            f"and {TOOLS} has {count} tools"
        )
    return out


def main():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    with open(os.path.join(root, TOOLS), encoding="utf-8") as handle:
        source = handle.read()
    with open(os.path.join(root, PAGE), encoding="utf-8") as handle:
        page = handle.read()
    found = problems(source, page)
    for line in found:
        print(line)
    if found:
        return 1
    print(f"{PAGE} names each of the {len(code_tools(source))} tools of {TOOLS}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
