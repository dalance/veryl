use crate::veryl_grammar::VerylGrammar;
use crate::veryl_grammar_trait::Veryl;
use crate::veryl_parser::parse;
use parol_runtime::miette::{Result, WrapErr};
use std::path::Path;

#[derive(Debug)]
pub struct Parser {
    pub veryl: Veryl,
}

impl Parser {
    pub fn parse<T: AsRef<Path>>(input: &str, file: &T) -> Result<Self> {
        let mut grammar = VerylGrammar::new();
        parse(input, file, &mut grammar).wrap_err(format!(
            "Failed parsing file {}",
            file.as_ref().to_string_lossy()
        ))?;

        let veryl = grammar.veryl.unwrap();

        Ok(Parser { veryl })
    }
}
