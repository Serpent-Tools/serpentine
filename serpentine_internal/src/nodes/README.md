# Nodes

This module contains implementations of the built-in nodes used in serpentine pipelines.

## ShellEscape

The `ShellEscape` node accepts a single string input and outputs a version escaped according to POSIX shell rules for safe use in shell commands. It wraps the string in single quotes and escapes internal single quotes using the `\''` sequence.

This prevents shell injection vulnerabilities when combining strings in `Exec` nodes.

### Example

```rust
use serpentine_internal::nodes::{ShellEscape, Node, Value, Context};
use serpentine_internal::value::Type;

let node = ShellEscape;
let inputs = vec![Value::String("It's a test!".to_string())];
let ctx = Context::default();
let output = node.run(&inputs, &ctx).unwrap();

assert_eq!(
    output[0].as_string().unwrap(),
    "'It'\''s a test!'"
);
```

### Demonstrated Cases

- Empty string: `''`
- String with spaces: `'hello world'`
- String with single quote: `'o'\''reilly'`
- String with newline: `'hello\nworld'` (newlines are preserved within quotes)
