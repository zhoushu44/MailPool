pub mod extract;
pub mod graph;
pub mod imap;
pub mod mailbox;
pub mod outlook;

pub use extract::{extract_code, extract_code_by_regex, extract_code_from_mail, parse_code_patterns};
