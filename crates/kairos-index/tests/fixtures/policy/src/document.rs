/// The most rules that a policy document can have.
pub const MAX_RULES: usize = 64;

/// A policy document: a name and its rules.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyDocument {
    pub name: String,
    pub rules: Vec<String>,
}

impl PolicyDocument {
    /// The count of the rules of the document.
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

/// Why a policy document is refused.
#[derive(Debug, Clone, PartialEq)]
pub enum PolicyError {
    Empty,
    NoName,
    NoRules,
    TooManyRules,
}

/// The document in `text`: the name on the first line, one rule on each
/// other line.
pub fn parse_document(text: &str) -> Result<PolicyDocument, PolicyError> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let name = lines.next().ok_or(PolicyError::Empty)?.to_string();
    let rules = lines.map(str::to_string).collect();
    Ok(PolicyDocument { name, rules })
}

/// Check that `doc` has a name and 1 to MAX_RULES rules.
pub fn validate(doc: &PolicyDocument) -> Result<(), PolicyError> {
    if doc.name.is_empty() {
        return Err(PolicyError::NoName);
    }
    if doc.rules.is_empty() {
        return Err(PolicyError::NoRules);
    }
    if doc.rules.len() > MAX_RULES {
        return Err(PolicyError::TooManyRules);
    }
    Ok(())
}
