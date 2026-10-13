# COLLIERY-T-1857 (Dylan, 2026-10-01): code_search ranks test code below the
# other code. With no summaries, the search reads the names, the signatures
# and the paths, and the names of test functions often have the words of the
# query.
Feature: The search of the code

  Scenario: code_search ranks test code below the other code
    Given an index of the polyglot fixture with no summaries
    When I search the code for "summary counts the rows"
    Then the results have test functions and functions that are not test code
    And each result that is not test code ranks above each test function

  # KAIROS-T-0421: test code has a summary, so the search finds a test by
  # what it checks, not only by its name. The result still marks it as test
  # code.
  Scenario: code_search finds a test by what it checks
    Given a summarized index of the polyglot fixture, where the summary of test_render_joins_rows is "It checks that a report puts each row on its own line."
    When I search the code for "puts each row on its own line"
    Then the first result is the test function test_render_joins_rows, with its summary

  # KAIROS-T-0423: the text rank stems the words (FTS5, Porter), so an
  # inflected query finds the code that defines, parses and validates. The
  # fixture and the query are those of KAIROS-T-0418; the summaries are
  # written as the small model writes them. A test whose name has the words
  # of the query can still rank above them: KAIROS-T-0422 ranks test code
  # lower.
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
    Then parse_document and validate are in the first 3 results that are not test code
