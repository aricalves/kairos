# The policy fixture

The fixture of KAIROS-T-0418 and KAIROS-T-0423: a crate that defines, parses
and validates a policy document, and its tests. The search of "how are policy
documents defined, parsed and validated" must find `parse_document` and
`validate`. It has no `Cargo.toml`, so a build runs no SCIP.

`PolicyDocument` has an impl block with the method `rule_count`: the search
gives the type and its impl block as one result (KAIROS-T-0424).

`src/lib.rs` holds only declarations and a `//!` comment: its file summary
comes from these (KAIROS-T-0425). `src/macros.rs` has none of these, so it
gets no summary, and `module_map` says "declarations only".
