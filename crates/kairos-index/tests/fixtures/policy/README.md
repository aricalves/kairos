# The policy fixture

The fixture of KAIROS-T-0418 and KAIROS-T-0423: a crate that defines, parses
and validates a policy document, and its tests. The search of "how are policy
documents defined, parsed and validated" must find `parse_document` and
`validate`. It has no `Cargo.toml`, so a build runs no SCIP.

`PolicyDocument` has an impl block with the method `rule_count`: the search
gives the type and its impl block as one result (KAIROS-T-0424).
