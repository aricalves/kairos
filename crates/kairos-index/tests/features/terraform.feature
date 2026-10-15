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

  # KAIROS-T-0471: a reference is an edge from the block that holds it to
  # the block that it names, in the same folder.
  Scenario: callers and callees follow the Terraform references
    Given an index of the polyglot fixture with no summaries
    Then the callees of "module.store" in "infra/main.tf" are:
      """
      var.name (infra/modules/bucket/variables.tf:1)
      local.name (infra/main.tf:6)
      var.owner (infra/modules/bucket/variables.tf:6)
      data.aws_caller_identity.current (infra/main.tf:9)
      """
    And the callees of "aws_s3_bucket.this" in "infra/modules/bucket/main.tf" are:
      """
      var.name (infra/modules/bucket/variables.tf:1)
      var.owner (infra/modules/bucket/variables.tf:6)
      """
    And the callers of "var.name" in "infra/modules/bucket/variables.tf" are:
      """
      module.store (infra/main.tf:11)
      aws_s3_bucket.this (infra/modules/bucket/main.tf:1)
      aws_sns_topic.events (infra/modules/bucket/main.tf:7)
      """
    And the callers of "local.name" in "infra/main.tf" are:
      """
      module.store (infra/main.tf:11)
      module.network (infra/main.tf:17)
      aws_sns_topic.events (infra/main.tf:23)
      """
    And the callers of "aws_sns_topic.events" in "infra/modules/bucket/main.tf" are:
      """
      """
    And the callees of "module.network" in "infra/main.tf" have the external "var.name"
    And the callees of "bucket_arn" in "infra/main.tf" are:
      """
      arn (infra/modules/bucket/outputs.tf:1)
      """
