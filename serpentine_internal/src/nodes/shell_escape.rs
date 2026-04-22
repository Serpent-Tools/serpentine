use crate::nodes::{Node, Value, Error, Type, Context};

#[derive(Clone, Debug, Default)]
pub struct ShellEscape;

impl Node for ShellEscape {
    fn name(&self) -> &'static str {
        "shell_escape"
    }

    fn input_types(&self) -> &'static [Type] {
        &[Type::String]
    }

    fn output_types(&self) -> &'static [Type] {
        &[Type::String]
    }

    fn run(&self, inputs: &[Value], _ctx: &Context) -> Result<Vec<Value>, Error> {
        if inputs.len() != 1 {
            return Err(Error::InvalidNumberOfInputs { expected: 1, got: inputs.len() });
        }
        match &inputs[0] {
            Value::String(s) => {
                let inner = s.replace("'", "'\\''");
                let result = format!("'{inner}'");
                Ok(vec![Value::String(result)])
            }
            _ => Err(Error::InvalidInputType { expected: Type::String, got: inputs[0].type_() }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Context;
    use crate::value::{Type, Value};

    #[test]
    fn empty_string() {
        let node = ShellEscape;
        let inputs = vec![Value::String(String::new())];
        let output = node.run(&inputs, &Context::default()).unwrap();
        assert_eq!(output, vec![Value::String("''".to_string())]);
    }

    #[test]
    fn string_with_spaces() {
        let node = ShellEscape;
        let inputs = vec![Value::String("hello world".to_string())];
        let output = node.run(&inputs, &Context::default()).unwrap();
        assert_eq!(output, vec![Value::String("'hello world'".to_string())]);
    }

    #[test]
    fn string_with_single_quote() {
        let node = ShellEscape;
        let inputs = vec![Value::String("o'reilly".to_string())];
        let output = node.run(&inputs, &Context::default()).unwrap();
        assert_eq!(output, vec![Value::String("'o'\''reilly'".to_string())]);
    }

    #[test]
    fn string_with_newline() {
        let node = ShellEscape;
        let inputs = vec![Value::String("hello\nworld".to_string())];
        let output = node.run(&inputs, &Context::default()).unwrap();
        assert_eq!(output, vec![Value::String("'hello\\nworld'".to_string())]);
    }
}
