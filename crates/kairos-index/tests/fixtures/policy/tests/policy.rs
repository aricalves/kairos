use policy::{PolicyError, parse_document, validate};

fn sample_policy_document() -> String {
    "retention\nkeep logs 30 days\ndelete backups after 90 days\n".to_string()
}

#[test]
fn a_policy_document_is_parsed() {
    let doc = parse_document(&sample_policy_document()).unwrap();
    assert_eq!(doc.name, "retention");
    assert_eq!(doc.rules.len(), 2);
}

#[test]
fn a_parsed_policy_document_is_validated() {
    let doc = parse_document(&sample_policy_document()).unwrap();
    assert_eq!(validate(&doc), Ok(()));
}

#[test]
fn an_empty_policy_document_is_refused() {
    assert_eq!(parse_document(""), Err(PolicyError::Empty));
}
