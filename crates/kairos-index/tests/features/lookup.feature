# KAIROS-T-0417: the code tools find one symbol by its name, its file and its line.
Feature: Choose one symbol by its name

  Scenario: A type is chosen over its impl blocks
    Given the polyglot fixture repository
    When I build the structure
    Then "Checksum" in "src/checksum.rs" gives the struct
    And "Checksum" with no file gives the 2 structs as candidates
    And "Checksum" at line 10 of "src/checksum.rs" gives the impl block
    And each candidate of "Checksum", given back as the name, gives that symbol

  # KAIROS-T-0468: a Go method has its receiver type as its container.
  Scenario: A Go method is found by its receiver type
    Given the polyglot fixture repository
    When I build the structure
    Then "Greeter.Greet" gives the method of "Greeter" at line 21 of "go/greet/greet.go"
    And "Greet" with no file gives the function and the method as candidates
