#!/usr/bin/env python3
"""check_compose_settings.py — the Compose file passes each setting of the book
(KAIROS-T-0496).

The reference deployment lists each setting under `environment: &kairos_env` in
`deploy/docker-compose.yaml`. A setting that is not in that list never reaches
the container, whatever `.env` says. 0.14 added five settings and the list did
not change. This check reads three files as text, needs no Rust build and no
Docker, and fails when they disagree:

  * `docs/src/content/docs/reference/configuration.md`: the table rows under
    `## Server environment variables` name each setting of the server and its
    default.
  * `deploy/docker-compose.yaml`: the keys of the `kairos_env` anchor.
  * `deploy/.env.example`: the names an operator can set, commented or not.

The rules:

  1. Each documented setting is a key of the anchor, unless EXCLUDED names it
     with a reason. A setting in EXCLUDED that is in the anchor is stale.
  2. A key that passes a variable through (`${NAME:-default}`) passes the
     variable of the same name, and its default is empty (the server default
     holds) or equal to the default that the book documents. The retention
     settings need the second form: the server reads an empty value of them as
     an error, not as unset.
  3. Each variable the anchor passes through is in `.env.example`.
  4. Each name in `.env.example` is used somewhere in the Compose file, or
     Compose itself reads it (COMPOSE_OWN).

  check_compose_settings.py        check the repository; exit 1 with a list of
                                   the problems
"""

import os
import re
import sys

# A documented setting that the Compose file does not pass, with the reason.
EXCLUDED = {
    "KAIROS_DEV_UI": "development only: the Swagger UI has no place in a deployment",
    "KAIROS_WEB_DIST": "development only: the image serves the GUI it embeds",
}

REFERENCE = os.path.join("docs", "src", "content", "docs", "reference", "configuration.md")
COMPOSE = os.path.join("deploy", "docker-compose.yaml")
ENV_EXAMPLE = os.path.join("deploy", ".env.example")

# Names of `.env.example` that Compose itself reads, with no `${}` in the file.
COMPOSE_OWN = {"COMPOSE_PROFILES"}

SERVER_SECTION = "## Server environment variables"
ANCHOR = "kairos_env"

NAME = r"[A-Z][A-Z0-9_]*"
ROW = re.compile(r"^\|\s*`(" + NAME + r")`\s*\|(.*)$")
# A `|` inside a cell is written `\|`.
CELL_SEPARATOR = re.compile(r"(?<!\\)\|")
BACKTICKED = re.compile(r"`([^`]*)`")
PASSTHROUGH = re.compile(r"^[\"']?\$\{(" + NAME + r"):-([^}]*)\}[\"']?$")
ENV_LINE = re.compile(r"^#?\s*(" + NAME + r")=")
ANCHOR_LINE = re.compile(r"^(\s*)environment:\s*&" + ANCHOR + r"\s*$")
KEY_LINE = re.compile(r"^(\s*)(" + NAME + r"):\s*(.*?)\s*$")


class CheckError(Exception):
    """A file does not have the shape the check reads."""


def documented_settings(text):
    """The settings of the server section of the book: name -> documented
    default, or None when the table has no Default column or the default is
    not a value (`unset`, `empty`, prose)."""
    settings = {}
    in_section = False
    default_column = None
    for line in text.splitlines():
        if line.startswith("## "):
            in_section = line.strip() == SERVER_SECTION
            continue
        if not in_section:
            continue
        if line.startswith("|"):
            cells = [cell.strip() for cell in CELL_SEPARATOR.split(line.strip().strip("|"))]
            if "Variable" in cells:
                default_column = cells.index("Default") if "Default" in cells else None
                continue
            match = ROW.match(line)
            if not match:
                continue
            name = match.group(1)
            default = None
            if default_column is not None and default_column < len(cells):
                found = BACKTICKED.search(cells[default_column])
                if found:
                    default = found.group(1)
            settings[name] = default
    return settings


def anchor_settings(text):
    """The keys of the `kairos_env` anchor: key -> (variable, default) for a
    passthrough `${VARIABLE:-default}`, or None for a fixed or composed
    value."""
    lines = text.splitlines()
    start = None
    for index, line in enumerate(lines):
        if ANCHOR_LINE.match(line):
            start = index
            break
    if start is None:
        raise CheckError("%s has no `environment: &%s` anchor" % (COMPOSE, ANCHOR))
    outer = len(ANCHOR_LINE.match(lines[start]).group(1))
    settings = {}
    indent = None
    for line in lines[start + 1 :]:
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        this_indent = len(line) - len(line.lstrip())
        if this_indent <= outer:
            break
        if indent is None:
            indent = this_indent
        if this_indent != indent:
            continue
        match = KEY_LINE.match(line)
        if not match:
            raise CheckError("%s: not a key of the anchor: %r" % (COMPOSE, line))
        key, value = match.group(2), match.group(3)
        passthrough = PASSTHROUGH.match(value)
        settings[key] = (passthrough.group(1), passthrough.group(2)) if passthrough else None
    return settings


def env_example_names(text):
    """The names of `.env.example`, commented or not."""
    names = []
    for line in text.splitlines():
        match = ENV_LINE.match(line)
        if match and match.group(1) not in names:
            names.append(match.group(1))
    return names


def find_problems(reference, compose, env, excluded=EXCLUDED):
    """The disagreements between the three texts, one sentence each."""
    documented = documented_settings(reference)
    anchor = anchor_settings(compose)
    env_names = env_example_names(env)
    used_in_compose = set(re.findall(r"\$\{(" + NAME + r")", compose))
    problems = []

    for name in sorted(documented):
        if name in anchor:
            if name in excluded:
                problems.append(
                    "%s is in the %s anchor and excluded from the check (%s): remove one."
                    % (name, ANCHOR, excluded[name])
                )
            continue
        if name in excluded:
            continue
        problems.append(
            "%s is in the book and not in the %s anchor of %s: the container never gets it."
            % (name, ANCHOR, COMPOSE)
        )

    for key in sorted(anchor):
        passthrough = anchor[key]
        if passthrough is None:
            continue
        variable, default = passthrough
        if variable != key:
            problems.append(
                "%s in the %s anchor passes ${%s}: the names must agree." % (key, ANCHOR, variable)
            )
        if default and key in documented and default != documented[key]:
            problems.append(
                "%s has the Compose default %r and the book documents %r: use an empty "
                "default or the documented one." % (key, default, documented[key])
            )
        if variable not in env_names:
            problems.append(
                "%s is passed by the %s anchor and not in %s." % (variable, ANCHOR, ENV_EXAMPLE)
            )

    for name in env_names:
        if name not in used_in_compose and name not in COMPOSE_OWN:
            problems.append("%s is in %s and %s does not use it." % (name, ENV_EXAMPLE, COMPOSE))

    return problems


def read(root, path):
    with open(os.path.join(root, path), encoding="utf-8") as handle:
        return handle.read()


def check_repository(root):
    return find_problems(read(root, REFERENCE), read(root, COMPOSE), read(root, ENV_EXAMPLE))


def main():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    try:
        problems = check_repository(root)
    except CheckError as error:
        print("check_compose_settings: %s" % error, file=sys.stderr)
        return 2
    for problem in problems:
        print(problem)
    if problems:
        print(
            "%d problem(s): %s, %s and %s disagree."
            % (len(problems), REFERENCE, COMPOSE, ENV_EXAMPLE),
            file=sys.stderr,
        )
        return 1
    print("check_compose_settings: the Compose file passes each setting of the book.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
