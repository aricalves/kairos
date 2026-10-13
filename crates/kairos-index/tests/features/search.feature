# COLLIERY-T-1857 (Dylan, 2026-10-01): code_search ranks test code below the
# other code. With no summaries, the search reads the names, the signatures
# and the paths, and the names of test functions often have the words of the
# query. KAIROS-T-0422 (Dylan, 2026-10-13): test code ranks below the other
# code with summaries too, unless the query is about tests (it has a word
# such as test, tested, spec, fixture or mock).
Feature: The search of the code

  Scenario: code_search ranks test code below the other code
    Given an index of the polyglot fixture with no summaries
    When I search the code for "summary counts the rows"
    Then the results have test functions and functions that are not test code
    And each result that is not test code ranks above each test function

  Scenario: A query about tests ranks test code with the other code
    Given an index of the polyglot fixture with no summaries
    When I search the code for "the tests of summary counts the rows"
    Then the first result is test code

  # KAIROS-T-0421: test code has a summary, so the search finds a test by
  # what it checks, not only by its name. The result still marks it as test
  # code.
  Scenario: code_search finds a test by what it checks
    Given a summarized index of the polyglot fixture, where the summary of test_render_joins_rows is "It checks that a report puts each row on its own line."
    When I search the code for "which test puts each row on its own line"
    Then the first result is the test function test_render_joins_rows, with its summary

  # KAIROS-T-0423: the text rank stems the words (FTS5, Porter), so an
  # inflected query finds the code that defines, parses and validates. The
  # fixture and the query are those of KAIROS-T-0418; the summaries are
  # written as the small model writes them. The test functions and the
  # helper in tests/ say the words of the query too; they are test code, so
  # they rank below the defining code (KAIROS-T-0422).
  Scenario: code_search finds the defining code for a query with inflected words
    Given a summarized index of the policy fixture, with these summaries:
      | name                                 | summary                                                                                                      |
      | MAX_RULES                            | Defines the maximum number of rules allowed in a policy document.                                            |
      | PolicyDocument                       | Represents a policy document with a name and a list of rules.                                                |
      | PolicyError                          | Enumerates the errors returned when a policy document is empty or has no name, no rules or too many rules.   |
      | parse_document                       | Parses the text of a policy document into a PolicyDocument, with the name on the first line and one rule per line. |
      | validate                             | Validates a policy document: checks that it has a name and between one and MAX_RULES rules.                  |
      | sample_policy_document               | Returns the text of a sample policy document with a name and two rules.                                      |
      | a_policy_document_is_parsed          | Tests that a sample policy document is parsed into its name and its two rules.                               |
      | a_parsed_policy_document_is_validated | Tests that a parsed policy document is validated with no error.                                             |
      | an_empty_policy_document_is_refused  | Tests that parsing an empty policy document returns the Empty error.                                         |
    When I search the code for "how are policy documents defined, parsed and validated"
    Then parse_document and validate are in the first 3 results
    And sample_policy_document is test code
    And each result that is not test code ranks above each test function

  Scenario: A query about how the code is tested finds the tests first
    Given a summarized index of the policy fixture, with these summaries:
      | name                                 | summary                                                                                                      |
      | MAX_RULES                            | Defines the maximum number of rules allowed in a policy document.                                            |
      | PolicyDocument                       | Represents a policy document with a name and a list of rules.                                                |
      | PolicyError                          | Enumerates the errors returned when a policy document is empty or has no name, no rules or too many rules.   |
      | parse_document                       | Parses the text of a policy document into a PolicyDocument, with the name on the first line and one rule per line. |
      | validate                             | Validates a policy document: checks that it has a name and between one and MAX_RULES rules.                  |
      | sample_policy_document               | Returns the text of a sample policy document with a name and two rules.                                      |
      | a_policy_document_is_parsed          | Tests that a sample policy document is parsed into its name and its two rules.                               |
      | a_parsed_policy_document_is_validated | Tests that a parsed policy document is validated with no error.                                             |
      | an_empty_policy_document_is_refused  | Tests that parsing an empty policy document returns the Empty error.                                         |
    When I search the code for "how is parsing tested"
    Then the first 3 results are test code

  # KAIROS-T-0424: a type and its impl blocks have one name. They give one
  # result, at the place of the type, with the best score of the group. The
  # impl block of PolicyDocument gets the summary of the type here (the fake
  # summarizer gives the summaries by name), so the two match the query in
  # the same way. A method of the impl block stays a result of its own.
  Scenario: code_search gives one result for a type and its impl blocks
    Given a summarized index of the policy fixture, with these summaries:
      | name           | summary                                                        |
      | PolicyDocument | Represents a policy document with a name and a list of rules.  |
      | rule_count     | Returns the number of rules in a policy document.              |
    When I search the code for "what is a policy document and its rules"
    Then the results named PolicyDocument are at "src/document.rs:6"
    And PolicyDocument::rule_count is a result of its own
    And no result is an impl block

  # Two types of one name in two files keep their own impl blocks.
  Scenario: Two types of one name give two results
    Given an index of the polyglot fixture with no summaries
    When I search the code for "checksum value"
    Then the results named Checksum are at "src/checksum.rs:6, src/legacy.rs:4"
    And Checksum::value is a result of its own
    And no result is an impl block
