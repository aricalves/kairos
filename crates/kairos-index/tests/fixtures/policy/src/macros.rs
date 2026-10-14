// A macro that makes a rule. This file has no doc comment, no mod or use
// line and no symbol that gets a summary.
macro_rules! rule {
    ($text:expr) => {
        String::from($text)
    };
}
