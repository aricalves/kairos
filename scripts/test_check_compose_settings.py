#!/usr/bin/env python3
"""Unit tests for check_compose_settings.py (KAIROS-T-0496). Stdlib only;
run with `python3 -m unittest scripts/test_check_compose_settings.py` (or
`angreal test unit`, which includes it).

The tests drive the check with small texts. The last test runs it on the
repository's own files, so a setting that the book documents and the Compose
file does not pass fails here too."""

import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "check_compose_settings.py")
SPEC = importlib.util.spec_from_file_location("check_compose_settings", SCRIPT)
check = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(check)

REFERENCE = """\
---
title: Configuration
---

## Server environment variables

### Required

| Variable | Type | Description |
|---|---|---|
| `DATABASE_URL` | Postgres connection URL | The database. |
| `OIDC_ISSUER_URL` | URL | The issuer. |

### Network and logging

| Variable | Type | Default | Description |
|---|---|---|---|
| `KAIROS_BIND_ADDR` | socket address | `127.0.0.1:8080` | Where it listens. |
| `KAIROS_LOG_LEVEL` | tracing filter directive | `info` | The level. |
| `KAIROS_ROLE` | `all` \\| `api` \\| `worker` | `all` | The work of this server. |
| `KAIROS_PUBLIC_URL` | URL | unset | The public URL. |
| `KAIROS_SESSION_TTL_SECS` | integer | `1209600` (14 days) | How long a session lasts. |
| `KAIROS_DEV_UI` | bool | `false` | Swagger UI. |

Prose that names `KAIROS_NOT_A_SETTING` is not a table row.

## CLI configuration

| Variable | Meaning |
|---|---|
| `KAIROS_CONFIG_DIR` | The config directory. |
| `KAIROS_KEY`, then `KAIROS_MCP_KEY` | An agent key. |

## Reference Compose deployment

| `.env` variable | Default in the example | Consumed by |
|---|---|---|
| `KAIROS_VERSION` | `0.15.0` | The image tag. |
"""

COMPOSE = """\
name: kairos

services:
  kairos:
    image: ghcr.io/example/kairos:${KAIROS_VERSION:?pin KAIROS_VERSION}
    environment: &kairos_env
      DATABASE_URL: postgres://kairos:${POSTGRES_PASSWORD:?set POSTGRES_PASSWORD}@postgres:5432/kairos
      # A comment between the keys.
      OIDC_ISSUER_URL: ${OIDC_ISSUER_URL:-}
      KAIROS_BIND_ADDR: 0.0.0.0:8080
      KAIROS_LOG_LEVEL: ${KAIROS_LOG_LEVEL:-info}
      KAIROS_ROLE: ${KAIROS_ROLE:-all}
      KAIROS_PUBLIC_URL: ${KAIROS_PUBLIC_URL:-}
      KAIROS_SESSION_TTL_SECS: ${KAIROS_SESSION_TTL_SECS:-}
      KAIROS_TRUSTED_PROXY: "true"
    volumes:
      - data:/var/lib/kairos

  worker:
    environment:
      <<: *kairos_env
      KAIROS_ROLE: worker
"""

ENV_EXAMPLE = """\
# The example .env.
KAIROS_VERSION=0.15.0
POSTGRES_PASSWORD=change-me
OIDC_ISSUER_URL=https://idp.example.com/
KAIROS_LOG_LEVEL=info
COMPOSE_PROFILES=caddy
# KAIROS_ROLE=api
# KAIROS_PUBLIC_URL=https://kairos.example.com
# KAIROS_SESSION_TTL_SECS=1209600
"""

EXCLUDED = {"KAIROS_DEV_UI": "development only"}


def problems(reference=REFERENCE, compose=COMPOSE, env=ENV_EXAMPLE, excluded=EXCLUDED):
    return check.find_problems(reference, compose, env, excluded)


class ParseTheReference(unittest.TestCase):
    def test_reads_the_table_rows_of_the_server_section_only(self):
        settings = check.documented_settings(REFERENCE)
        self.assertEqual(
            sorted(settings),
            [
                "DATABASE_URL",
                "KAIROS_BIND_ADDR",
                "KAIROS_DEV_UI",
                "KAIROS_LOG_LEVEL",
                "KAIROS_PUBLIC_URL",
                "KAIROS_ROLE",
                "KAIROS_SESSION_TTL_SECS",
                "OIDC_ISSUER_URL",
            ],
        )

    def test_reads_the_first_value_of_the_default_column(self):
        settings = check.documented_settings(REFERENCE)
        self.assertEqual(settings["KAIROS_LOG_LEVEL"], "info")
        self.assertEqual(settings["KAIROS_SESSION_TTL_SECS"], "1209600")
        # The type column holds escaped pipes: the default is still the third cell.
        self.assertEqual(settings["KAIROS_ROLE"], "all")
        self.assertIsNone(settings["KAIROS_PUBLIC_URL"])
        self.assertIsNone(settings["DATABASE_URL"])


class ParseTheComposeFile(unittest.TestCase):
    def test_reads_the_keys_of_the_anchor(self):
        anchor = check.anchor_settings(COMPOSE)
        self.assertEqual(
            sorted(anchor),
            [
                "DATABASE_URL",
                "KAIROS_BIND_ADDR",
                "KAIROS_LOG_LEVEL",
                "KAIROS_PUBLIC_URL",
                "KAIROS_ROLE",
                "KAIROS_SESSION_TTL_SECS",
                "KAIROS_TRUSTED_PROXY",
                "OIDC_ISSUER_URL",
            ],
        )

    def test_tells_a_passthrough_from_a_fixed_value(self):
        anchor = check.anchor_settings(COMPOSE)
        self.assertEqual(anchor["KAIROS_LOG_LEVEL"], ("KAIROS_LOG_LEVEL", "info"))
        self.assertEqual(anchor["KAIROS_PUBLIC_URL"], ("KAIROS_PUBLIC_URL", ""))
        self.assertIsNone(anchor["KAIROS_BIND_ADDR"])
        self.assertIsNone(anchor["KAIROS_TRUSTED_PROXY"])
        # DATABASE_URL is composed from another variable: not a passthrough.
        self.assertIsNone(anchor["DATABASE_URL"])

    def test_refuses_a_file_with_no_anchor(self):
        with self.assertRaises(check.CheckError):
            check.anchor_settings("services:\n  kairos:\n    environment:\n      A: b\n")

    def test_reads_the_names_of_the_env_example(self):
        self.assertEqual(
            sorted(check.env_example_names(ENV_EXAMPLE)),
            [
                "COMPOSE_PROFILES",
                "KAIROS_LOG_LEVEL",
                "KAIROS_PUBLIC_URL",
                "KAIROS_ROLE",
                "KAIROS_SESSION_TTL_SECS",
                "KAIROS_VERSION",
                "OIDC_ISSUER_URL",
                "POSTGRES_PASSWORD",
            ],
        )


class TheRules(unittest.TestCase):
    def test_clean_fixtures_pass(self):
        self.assertEqual(problems(), [])

    def test_a_documented_setting_missing_from_the_anchor(self):
        row = "| `KAIROS_NEW_SETTING` | integer | `2` | New in this release. |\n"
        reference = REFERENCE.replace("\nProse that names", "\n" + row + "\nProse that names")
        found = problems(reference=reference)
        self.assertEqual(len(found), 1)
        self.assertIn("KAIROS_NEW_SETTING", found[0])
        self.assertIn("kairos_env", found[0])

    def test_an_excluded_setting_is_not_a_problem(self):
        # KAIROS_DEV_UI is documented, not in the anchor, and excluded.
        self.assertEqual(problems(), [])
        found = problems(excluded={})
        self.assertEqual(len(found), 1)
        self.assertIn("KAIROS_DEV_UI", found[0])

    def test_an_exclusion_of_a_setting_in_the_anchor_is_a_problem(self):
        found = problems(excluded={**EXCLUDED, "KAIROS_LOG_LEVEL": "stale"})
        self.assertEqual(len(found), 1)
        self.assertIn("KAIROS_LOG_LEVEL", found[0])
        self.assertIn("excluded", found[0])

    def test_a_compose_default_must_be_empty_or_the_documented_default(self):
        compose = COMPOSE.replace("${KAIROS_LOG_LEVEL:-info}", "${KAIROS_LOG_LEVEL:-debug}")
        found = problems(compose=compose)
        self.assertEqual(len(found), 1)
        self.assertIn("KAIROS_LOG_LEVEL", found[0])
        self.assertIn("debug", found[0])
        self.assertIn("info", found[0])

    def test_a_compose_default_equal_to_the_documented_default_passes(self):
        compose = COMPOSE.replace(
            "${KAIROS_SESSION_TTL_SECS:-}", "${KAIROS_SESSION_TTL_SECS:-1209600}"
        )
        self.assertEqual(problems(compose=compose), [])

    def test_a_passthrough_must_be_in_the_env_example(self):
        env = ENV_EXAMPLE.replace("# KAIROS_PUBLIC_URL=https://kairos.example.com\n", "")
        found = problems(env=env)
        self.assertEqual(len(found), 1)
        self.assertIn("KAIROS_PUBLIC_URL", found[0])
        self.assertIn(".env.example", found[0])

    def test_a_name_of_the_env_example_must_reach_the_compose_file(self):
        env = ENV_EXAMPLE + "KAIROS_FORGOTTEN=1\n"
        found = problems(env=env)
        self.assertEqual(len(found), 1)
        self.assertIn("KAIROS_FORGOTTEN", found[0])

    def test_a_passthrough_under_another_name_is_a_problem(self):
        compose = COMPOSE.replace(
            "KAIROS_PUBLIC_URL: ${KAIROS_PUBLIC_URL:-}", "KAIROS_PUBLIC_URL: ${PUBLIC_URL:-}"
        )
        found = problems(compose=compose)
        self.assertTrue(any("PUBLIC_URL" in p for p in found))


class TheRepository(unittest.TestCase):
    def test_the_repository_files_agree(self):
        root = os.path.dirname(HERE)
        found = check.check_repository(root)
        self.assertEqual(found, [], "\n".join(found))


if __name__ == "__main__":
    unittest.main()
