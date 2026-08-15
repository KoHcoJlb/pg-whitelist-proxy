pub mod variable_template {
    #[derive(pest_derive::Parser)]
    #[grammar_inline = r#"
variable_template = { SOI ~ part* ~ EOI }

part = _{ variable | text }

variable = _{
    "@@"
    ~ (
          token
        | token_any
    )
    ~ "@@"
}

token = { "Token(" ~ token_name ~ ")" }
token_any = { "TokenAny(" ~ (token_name ~ ","? ~ " "?)+ ~ ")" }

token_name = { (ASCII_ALPHANUMERIC | "_")+ }

text = { (!variable ~ ANY)+ }
"#]
    pub struct Parser;
}
