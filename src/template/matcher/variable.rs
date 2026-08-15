use derive_more::{Display, Error};
use eyre::{Result, eyre};
use pest::Parser;
use pg_query::protobuf::{ScanResult, ScanToken, Token};

use crate::template::parser::variable_template;

#[derive(Debug, Display)]
pub enum MatchErrorReason {
    #[display("unexpected end of query")]
    QueryEof,
    #[display("expected token is missing: {_0:?}")]
    TokenMissing(Token),
    #[display("exact text does not match")]
    ExactMismatch,
    #[display("wrong token type: {_0:?}")]
    WrongTokenType(Token),
    #[display("array is not terminated")]
    ArrayNotTerminated,
}

#[derive(Debug, Display, Error)]
#[display(r#"expected={expected:?} actual="{actual}" reason={reason:?}"#)]
pub struct MatchError {
    expected: PatternRule,
    actual: String,
    reason: MatchErrorReason,
}

#[derive(Debug, Clone)]
enum PatternRule {
    Exact(String),
    Token(Token),
    TokenAny(Vec<Token>),
}

pub(super) fn divergent_suffixes(expected: &str, actual: &str) -> Option<(String, String)> {
    const CONTEXT_CHARS: usize = 20;
    const MARKER: &str = "<DIVERGENCE>";

    if actual.starts_with(expected) {
        return None;
    }

    let common_chars = expected
        .chars()
        .zip(actual.chars())
        .take_while(|(expected, actual)| expected == actual)
        .count();
    let context_start = common_chars.saturating_sub(CONTEXT_CHARS);
    let expected_context_pos =
        expected.char_indices().nth(context_start).map_or(expected.len(), |(pos, _)| pos);
    let actual_context_pos =
        actual.char_indices().nth(context_start).map_or(actual.len(), |(pos, _)| pos);
    let expected_divergence_pos =
        expected.char_indices().nth(common_chars).map_or(expected.len(), |(pos, _)| pos);
    let actual_divergence_pos =
        actual.char_indices().nth(common_chars).map_or(actual.len(), |(pos, _)| pos);

    Some((
        format!(
            "{}{MARKER}{}",
            &expected[expected_context_pos..expected_divergence_pos],
            &expected[expected_divergence_pos..]
        ),
        format!(
            "{}{MARKER}{}",
            &actual[actual_context_pos..actual_divergence_pos],
            &actual[actual_divergence_pos..]
        ),
    ))
}

fn consume_tokens<'a>(
    mut iter: impl Iterator<Item = &'a ScanToken>, expected: &[Token],
) -> Result<&'a ScanToken, MatchErrorReason> {
    let mut last_consumed = None;

    for expected in expected {
        let Some(actual) = iter.next() else {
            return Err(MatchErrorReason::TokenMissing(*expected));
        };

        if expected != &actual.token() {
            return Err(MatchErrorReason::WrongTokenType(actual.token()));
        }

        last_consumed = Some(actual);
    }

    Ok(last_consumed.expect("consume_tokens requires at least one expected token"))
}

#[derive(Debug)]
pub struct VariableTemplateMatcher {
    pattern: Vec<PatternRule>,
}

impl VariableTemplateMatcher {
    pub fn parse(template: &str) -> Result<Self> {
        use variable_template::Rule;

        let rules = variable_template::Parser::parse(Rule::variable_template, template)?;

        let pattern = rules
            .flatten()
            .filter_map(|r| match r.as_rule() {
                Rule::text => Some(Ok(PatternRule::Exact(r.as_str().into()))),
                Rule::token => {
                    let inner = r.clone().into_inner().next()?;

                    let Some(token) = Token::from_str_name(inner.as_str()) else {
                        return Some(Err(eyre!("unknown token type: {inner}")));
                    };

                    Some(Ok(PatternRule::Token(token)))
                }
                Rule::token_any => Some(
                    r.into_inner()
                        .map(|p| {
                            let token_str = p.as_str();
                            Token::from_str_name(token_str)
                                .ok_or_else(|| eyre!("unknown token type: {token_str}"))
                        })
                        .collect::<Result<_, _>>()
                        .map(PatternRule::TokenAny),
                ),
                _ => None,
            })
            .collect::<Result<_>>()?;

        Ok(Self { pattern })
    }

    pub fn match_query(
        &self, query: &str, scan_result: &ScanResult, pos: &mut usize,
    ) -> std::result::Result<(), MatchError> {
        for rule in &self.pattern {
            let query_substr = query.get(*pos..).ok_or_else(|| MatchError {
                expected: rule.clone(),
                actual: "".into(),
                reason: MatchErrorReason::QueryEof,
            })?;

            let start_pos = *pos;
            let mut scan_token_iter =
                scan_result.tokens.iter().skip_while(|t| (t.start as usize) < start_pos);

            let create_match_error =
                |reason| MatchError { expected: rule.clone(), actual: query_substr.into(), reason };

            match rule {
                PatternRule::Exact(expected) => {
                    if let Some((expected, actual)) = divergent_suffixes(expected, query_substr) {
                        return Err(MatchError {
                            expected: PatternRule::Exact(expected),
                            actual,
                            reason: MatchErrorReason::ExactMismatch,
                        });
                    }
                    *pos += expected.len();
                }
                PatternRule::Token(expected) => {
                    let actual = consume_tokens(&mut scan_token_iter, &[*expected])
                        .map_err(create_match_error)?;
                    *pos = actual.end as usize;
                }
                PatternRule::TokenAny(expected) => {
                    for actual in scan_token_iter {
                        if !expected.contains(&actual.token()) {
                            *pos = actual.start as usize;
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divergent_suffixes_include_bounded_utf8_context() {
        let common = "0123456789é1234567890123456789";
        let expected = format!("{common}expected");
        let actual = format!("{common}actual");

        assert_eq!(
            divergent_suffixes(&expected, &actual),
            Some((
                "é1234567890123456789<DIVERGENCE>expected".into(),
                "é1234567890123456789<DIVERGENCE>actual".into()
            ))
        );
    }

    fn run_match(
        template: &str, query: &str, start: usize,
    ) -> (std::result::Result<(), MatchError>, usize) {
        let matcher = VariableTemplateMatcher::parse(template).unwrap();
        let scan_result = pg_query::scan(query).unwrap();
        let mut pos = start;
        let result = matcher.match_query(query, &scan_result, &mut pos);
        (result, pos)
    }

    fn assert_full_match(template: &str, query: &str) {
        let (result, pos) = run_match(template, query, 0);
        result.unwrap_or_else(|err| {
            panic!("expected match: template={template:?} query={query:?}: {err}")
        });
        assert_eq!(
            pos,
            query.len(),
            "matcher returned success but did not consume the whole query"
        );
    }

    fn assert_not_full_match(template: &str, query: &str) {
        let (result, pos) = run_match(template, query, 0);
        assert!(
            result.is_err() || pos != query.len(),
            "unexpected full match: template={template:?} query={query:?}"
        );
    }

    #[test]
    fn exact_query_matches() {
        assert_full_match(
            "SELECT id FROM users WHERE active = true",
            "SELECT id FROM users WHERE active = true",
        );
    }

    #[test]
    fn exact_query_is_not_normalized() {
        assert_not_full_match("SELECT id FROM users", "select id FROM users");

        assert_not_full_match("SELECT id FROM users", "SELECT  id FROM users");
    }

    #[test]
    fn sconst_matches_different_string_literals() {
        assert_full_match(
            "SELECT * FROM users WHERE name = @@Token(SCONST)@@",
            "SELECT * FROM users WHERE name = 'alice'",
        );

        assert_full_match(
            "SELECT * FROM users WHERE name = @@Token(SCONST)@@",
            "SELECT * FROM users WHERE name = 'bob'",
        );
    }

    #[test]
    fn multiple_sconst_placeholders_match() {
        let template =
            "between @@Token(SCONST)@@::date and @@Token(SCONST)@@::date + '1 days'::interval";

        assert_full_match(
            template,
            "between '2026-08-07T15:41:31.159Z'::date and \
             '2026-08-09T15:41:31.159Z'::date + '1 days'::interval",
        );
    }

    #[test]
    fn mixed_token_types_match() {
        assert_full_match(
            "VALUES (@@Token(SCONST)@@, @@Token(ICONST)@@, @@Token(IDENT)@@)",
            "VALUES ('hello', 42, some_identifier)",
        );
    }

    #[test]
    fn sconst_array_matches_empty_array() {
        assert_full_match("@@Array(SCONST)@@", "array[]");
        assert_full_match("@@Array(SCONST)@@", "array[ ]");
    }

    #[test]
    fn sconst_array_matches_one_or_more_elements() {
        assert_full_match("@@Array(SCONST)@@", "array['one']");
        assert_full_match("@@Array(SCONST)@@", "array['one', 'two', 'three']");
    }

    #[test]
    fn array_template_composes_with_fixed_sql_and_token_templates() {
        assert_full_match(
            "@@Array(SCONST)@@::text[], @@Array(SCONST)@@::text[], @@Token(SCONST)@@",
            "array[ ]::text[], array['final_ban', 'androidx']::text[], ''",
        );
    }

    #[test]
    fn array_element_token_type_must_match() {
        assert_not_full_match("@@Array(SCONST)@@", "array[1, 2]");
        assert_not_full_match("@@Array(SCONST)@@", "array['one', 2]");
        assert_not_full_match("@@Array(ICONST)@@", "array[1, 'two']");
    }

    #[test]
    fn expressions_inside_array_are_rejected() {
        assert_not_full_match("@@Array(SCONST)@@", "array['one' || 'two']");
        assert_not_full_match("@@Array(SCONST)@@", "array['safe', current_user]");
    }

    #[test]
    fn token_type_must_match() {
        assert_not_full_match(
            "SELECT * FROM users WHERE id = @@Token(ICONST)@@",
            "SELECT * FROM users WHERE id = '42'",
        );

        assert_not_full_match(
            "SELECT * FROM users WHERE name = @@Token(SCONST)@@",
            "SELECT * FROM users WHERE name = 42",
        );
    }

    #[test]
    fn fixed_sql_cannot_change() {
        assert_not_full_match(
            "SELECT public_name FROM users WHERE id = @@Token(ICONST)@@",
            "SELECT password_hash FROM users WHERE id = 1",
        );
    }

    #[test]
    fn injection_after_string_token_is_rejected() {
        assert_not_full_match(
            "SELECT * FROM users WHERE name = @@Token(SCONST)@@ AND active = true",
            "SELECT * FROM users WHERE name = '' OR true --' AND active = true",
        );

        assert_not_full_match(
            "SELECT * FROM users WHERE created_at >= @@Token(SCONST)@@::timestamp",
            "SELECT * FROM users WHERE created_at >= '2026-01-01'; DROP TABLE users; --",
        );
    }

    #[test]
    fn dangerous_looking_text_inside_one_string_literal_is_allowed() {
        // The contents look like SQL, but PostgreSQL scans the whole value
        // as one SCONST token, so it remains data rather than SQL syntax.
        assert_full_match(
            "SELECT * FROM users WHERE name = @@Token(SCONST)@@",
            "SELECT * FROM users WHERE name = ''' OR true --'",
        );
    }

    #[test]
    fn extra_whitespace_before_token_placeholder_are_accepted() {
        assert_full_match("SELECT @@Token(SCONST)@@", "SELECT  'hello'");
    }

    #[test]
    fn comment_before_placeholder_is_rejected() {
        assert_not_full_match("SELECT @@Token(SCONST)@@", "SELECT /* injected */ 'hello'");
    }

    #[test]
    fn can_match_from_nonzero_position() {
        let query = "prefix '2026-08-09'::date suffix";
        let start = "prefix ".len();

        let (result, pos) = run_match("@@Token(SCONST)@@::date", query, start);

        result.unwrap();
        assert_eq!(pos, "prefix '2026-08-09'::date".len());
        assert_eq!(&query[pos..], " suffix");
    }

    #[test]
    fn utf8_before_placeholder_uses_byte_positions_correctly() {
        assert_full_match("SELECT 'привет' || @@Token(SCONST)@@", "SELECT 'привет' || 'мир'");
    }

    #[test]
    fn trailing_sql_is_left_unconsumed() {
        // match_query currently behaves as a segment/prefix matcher.
        // A whole-query authorizer MUST additionally require pos == query.len().
        let query = "SELECT 'ok'; DROP TABLE users";
        let (result, pos) = run_match("SELECT @@Token(SCONST)@@", query, 0);

        assert!(result.is_ok());
        assert_eq!(&query[pos..], "; DROP TABLE users");
    }

    #[test]
    fn unknown_token_type_is_rejected_while_parsing_template() {
        let result = VariableTemplateMatcher::parse("SELECT @@Token(NOT_A_REAL_PG_TOKEN)@@");
        assert!(result.is_err());

        let result = VariableTemplateMatcher::parse("SELECT @@TokenAny(NOT_A_REAL_PG_TOKEN)@@");
        assert!(result.is_err());
    }
}
