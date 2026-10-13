# KAIROS-T-0417: the code tools find one symbol by its name, its file and its line.
Feature: Choose one symbol by its name

  Scenario: A type is chosen over its impl blocks
    Given the polyglot fixture repository
    When I build the structure
    Then "Checksum" in "src/checksum.rs" gives the struct
    And "Checksum" with no file gives the 2 structs as candidates
    And "Checksum" at line 10 of "src/checksum.rs" gives the impl block
    And each candidate of "Checksum", given back as the name, gives that symbol
