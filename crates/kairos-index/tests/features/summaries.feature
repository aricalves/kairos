# COLLIERY-T-1850: the summaries and the vectors of the index.
# The scenarios use a fake summarizer that gives a fixed text for each input,
# and the deterministic vectors of kairos-embed. Only the @model scenario
# uses the real model. It runs only when the model file is on disk (see
# KAIROS_INDEX_MODEL in tests/bdd.rs) and the test is built with the `llama`
# feature. Otherwise cucumber shows it as skipped, with the reason.
Feature: Summaries

  # KAIROS-T-0421 (Dylan, 2026-10-13): test code gets a summary too. It
  # keeps its test mark.
  Scenario: Each symbol gets one summary, test code too
    Given the polyglot fixture and the fake summarizer
    When I summarize the index
    Then each symbol of a summarized kind has a summary and a vector
    And each test symbol and each test file has a summary, and the test symbols are marked as test code
    And the count of summarizable symbols has the test symbols

  # KAIROS-T-0349: an impl block and a constant get a summary too. The input
  # of an impl block is its header and the declarations of its functions,
  # not their bodies. A `mod x;` line still gets none.
  Scenario: An impl block and a constant get a summary, a mod line gets none
    Given the polyglot fixture and the fake summarizer
    When I summarize the index
    Then the impl block of Checksum in src/checksum.rs has a summary, from its header and the declaration of value, not the body
    And the constant SEED in src/checksum.rs has a summary
    And the mod lines of src/lib.rs have no summary

  Scenario: The same code gets the same key
    Given 2 copies of a Rust function that differ only in whitespace and comments
    When I summarize the index
    Then the 2 symbols have the same key
    And the summarizer ran one time for them

  Scenario: A file summary comes from its symbol summaries
    Given the polyglot fixture and the fake summarizer
    When I summarize the index
    Then the input to each file summary is the summaries of its symbols, not the code of the file

  # KAIROS-T-0425 (Dylan, 2026-10-13): a file of declarations, such as a
  # lib.rs of mod and use lines, gets a summary from these lines and its //!
  # comment. A file with none of these gets no summary, and the module map
  # says "declarations only".
  Scenario: A file of declarations gets a summary from its declarations and its doc comment
    Given the policy fixture and the fake summarizer
    When I summarize the index
    Then the input to the summary of src/lib.rs is its //! comment and its mod and use lines
    And src/macros.rs has no summary, and the module map says that it has declarations only
    When I summarize the index
    Then the summary of src/lib.rs comes from the pool

  Scenario: A change in a callee does not change the key of its caller
    Given a summarized index
    When the body of a function changes and its signature stays the same
    Then the key of the function changes
    And the keys of its callers do not change

  # KAIROS-T-0341: a hosted summarizer takes many requests at once. The run
  # sends the requests of each level in batches of the concurrency of the
  # summarizer, and it writes a batch before the next.
  Scenario: The requests go to the summarizer in batches of its concurrency
    Given the polyglot fixture and a fake summarizer that takes 4 requests at once
    When I summarize the index
    Then each symbol of a summarized kind has a summary and a vector
    And the summarizer got batches of at most 4 requests, and one batch of 4

  # KAIROS-T-0338: the model is part of each key, so a change of model
  # makes new summaries and never mixes them. A link run (no summarizer)
  # uses the model that the pool records.
  Scenario: A summary is reused only for the same model
    Given a summarized index
    When I summarize the index with the fake model "fake/b"
    Then the summarizer ran for each summarizable symbol again, and the pool holds the summaries of both models
    When I summarize the index with the fake model "fake/fixed"
    Then the summarizer did not run
    And each symbol has the key of the first model again
    When I summarize the index with the fake model "fake/b"
    And I link the summaries of the index
    Then each symbol has a key of the model "fake/b", and no summary was made

  @model @allow.skipped
  Scenario: The real model writes a summary
    Given the Qwen3-4B model file is on disk
    When I summarize 3 symbols of the polyglot fixture with the real model
    Then each summary has 1 to 5 sentences
    And no summary is empty
