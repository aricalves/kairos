# KAIROS-T-0419: references gives each use of a symbol, not only its calls.
# `callers` missed the uses of a type in a signature and the uses of its
# fields. The Rust uses come from `rust-analyzer scip`, so these scenarios
# need the pinned rust-analyzer and the pinned std source
# (`angreal dev fetch-rust-analyzer`).
Feature: The uses of a symbol

  Scenario: The uses of a Rust type have their kind and their enclosing symbol
    Given the polyglot fixture repository
    When I build the index
    Then the uses of "Queue" in "src/queue.rs" are its import, its types and its fields
    And the uses of "DEFAULT_CAPACITY" in "src/stack.rs" are 1 use in "Stack::new" at line 12

  Scenario: The uses of a Python symbol are its calls only
    Given the polyglot fixture repository
    When I build the index
    Then the uses of "Report" in "python/polyglot/report.py" are its 2 calls, and the result says that it has the calls only

  Scenario: An update with no SCIP run keeps the uses of the unchanged files
    Given a summarized index with Rust edges from SCIP
    When I change one function in a Rust file and update the index with no options
    Then the uses of "Queue" in "src/queue.rs" are its import, its types and its fields
    But 1 Rust file waits for a SCIP run
    And "DEFAULT_CAPACITY" in "src/stack.rs" has no use
    # The change moved the use up 1 line.
    When I update the index with "--rust-edges"
    Then the uses of "DEFAULT_CAPACITY" in "src/stack.rs" are 1 use in "Stack::new" at line 11
