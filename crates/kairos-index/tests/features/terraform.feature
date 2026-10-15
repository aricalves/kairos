# COLLIERY-I-0621: Terraform in the code index.
Feature: Terraform in the code index

  # KAIROS-T-0470: a Terraform file has a language and symbols, and the
  # module map and the search have it.
  Scenario: A Terraform file is in the module map and in the search
    Given an index of the polyglot fixture with no summaries
    Then the module map lists "infra/main.tf" with 7 symbols
    And the module map lists "infra/prod.tfvars" with 0 symbols
    When I search the code for "sns topic events"
    Then a result is "aws_sns_topic.events" in "infra/main.tf"
    When I search the code for "caller identity"
    Then a result is "data.aws_caller_identity.current" in "infra/main.tf"

  # KAIROS-T-0470: a resource gets a summary; a file of variables is a file
  # of declarations, summarized from its comment and its blocks.
  Scenario: Terraform files get summaries
    Given the polyglot fixture and the fake summarizer
    When I summarize the index
    Then the symbol "aws_s3_bucket.this" in "infra/modules/bucket/main.tf" has a summary
    And the input to the summary of "infra/variables.tf" is its declarations:
      """
      # The inputs of the root module.

      variable "region" {
        type    = string
      }
      """
