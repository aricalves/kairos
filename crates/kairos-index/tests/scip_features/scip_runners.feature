# KAIROS-T-0667, KAIROS-T-0668 and KAIROS-T-0669: what the SCIP runners of
# Go, Python and TypeScript do around their tools. The tools are fake
# scripts, so no network and no real Go, uv or scip-typescript is needed.
# The real indexers are in scip_languages.rs.
Feature: The SCIP runners of Go, Python and TypeScript

  # KAIROS-T-0667: GOFLAGS=-mod=mod lets a Go step rewrite go.mod after the
  # build of the structure hashed it.
  Scenario: A Go step that rewrites go.mod leaves the file as it was
    Given a Go project whose go mod download rewrites go.mod and writes go.sum
    When I build the structure with the server tools
    Then go.mod is as it was and there is no go.sum
    And the Go run has the warning ".: go mod download changed go.mod, go.sum; the files are as before"

  # KAIROS-T-0667: a file that still changes is not a failed job.
  Scenario: A file that changed after the build of the structure makes the structure again
    Given a Rust file that a step wrote after the build of the structure
    When I run the summaries
    Then the structure was built again because of "src/lib.rs"
    And the index has 2 symbols

  # KAIROS-T-0668: the warning says what the project asks for.
  Scenario: A failed uv sync names what the project asks for and what the runner has
    Given a Python project with a uv.lock, requires-python "==3.11.*" and .python-version "3.11"
    And a uv whose sync fails
    When I build the structure with the server tools
    Then the Python warning starts with "uv sync failed, the project asks for Python ==3.11.*, .python-version 3.11; the runner has "
    And the Python warning ends with the output of uv "the lock file needs to be updated"

  # KAIROS-T-0668: by default the runner passes its own Python to uv and
  # fetches no Python.
  Scenario: By default uv gets the Python of the runner and downloads none
    Given a Python project with a uv.lock
    And a uv that syncs
    And the server fetches no tool of a project
    When I build the structure with the server tools
    Then uv got the "--python" argument
    And uv ran with UV_PYTHON_DOWNLOADS "never"

  # KAIROS-T-0668, KAIROS_CODE_INDEX_TOOL_DOWNLOADS: uv chooses the Python.
  Scenario: With tool downloads uv chooses the Python of the project
    Given a Python project with a uv.lock
    And a uv that syncs
    And the server may fetch the tools of a project
    When I build the structure with the server tools
    Then uv got no "--python" argument
    And uv ran with UV_PYTHON_DOWNLOADS "automatic"

  # KAIROS-T-0668: a uv that the project excludes is replaced by one that
  # pip fetches, only when the server may fetch.
  Scenario: With tool downloads a uv that the project excludes is replaced
    Given a Python project with a uv.lock and the uv required-version ">=0.9.0,<0.13"
    And a uv that the required-version excludes
    And the server may fetch the tools of a project
    When I build the structure with the server tools
    Then pip fetched "uv>=0.9.0,<0.13" into the dependency cache
    And the sync ran again with the fetched uv

  Scenario: Without tool downloads a uv that the project excludes is a warning
    Given a Python project with a uv.lock and the uv required-version ">=0.9.0,<0.13"
    And a uv that the required-version excludes
    And the server fetches no tool of a project
    When I build the structure with the server tools
    Then the Python warning ends with the output of uv "Required uv version >=0.9.0,<0.13 does not match"
    And nothing was fetched

  # KAIROS-T-0669: scip-typescript's own --infer-tsconfig takes .js and .jsx
  # only.
  Scenario: JavaScript with no tsconfig.json is indexed with the config of Kairos
    Given a JavaScript project with .mjs files and no tsconfig.json
    When I build the structure with the server tools
    Then scip-typescript ran 1 time
    And each run used tsconfig.kairos.json and none used --infer-tsconfig
    And the project has no tsconfig.kairos.json
    And the TypeScript run has no warning

  # KAIROS-T-0669: a tsconfig.json that takes no file gets a second run, and
  # is never changed.
  Scenario: A tsconfig.json that takes no file gets a second run with the config of Kairos
    Given a JavaScript project with a tsconfig.json that has no allowJs
    When I build the structure with the server tools
    Then the first run used no config and the second used tsconfig.kairos.json
    And tsconfig.json is as it was
    And the project has no tsconfig.kairos.json
    And the TypeScript run has no warning

  Scenario: A project with no file that scip-typescript takes says what the runs saw
    Given a project with no file that scip-typescript takes
    When I build the structure with the server tools
    Then the TypeScript warning ends with "(the project at . has no tsconfig.json or jsconfig.json; a run with the tsconfig of Kairos, for each .ts, .tsx, .mts, .cts, .js, .jsx, .mjs and .cjs file of the folder, found none.)"
    And the project has no tsconfig.kairos.json
