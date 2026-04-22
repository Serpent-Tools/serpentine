//! Module containing all built-in nodes for serpentine.

pub mod shell_escape;

pub use shell_escape::ShellEscape;

// TODO: Add to node registry if not already handled elsewhere.
// For example:
// pub fn create_node(name: &str) -> Option<Box<dyn Node>> {
//     match name {
//         "shell_escape" => Some(Box::new(ShellEscape)),
//         _ => None,
//     }
// }
