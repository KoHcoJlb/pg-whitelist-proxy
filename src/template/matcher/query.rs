use std::{collections::HashMap, sync::Arc};

use aho_corasick::{AhoCorasickBuilder, MatchKind};
use derive_more::{Display, Error, From};
use eyre::{Result, ensure};

use crate::template::matcher::{
    variable,
    variable::{VariableTemplateMatcher, divergent_suffixes},
};

#[derive(Debug, Display)]
pub enum MatchErrorReason {
    #[display("failed to scan query: {_0}")]
    QueryScanFailed(String),
    #[display("unexpected end of query")]
    QueryEof,
    #[display("exact text does not match")]
    ExactMismatch,
    #[display("unknown variable: {_0}")]
    UnknownVariable(String),
    #[display("variable does not match: {_0}")]
    VariableMismatch(variable::MatchError),
    #[display("query has unmatched suffix")]
    UnmatchedSuffix,
}

#[derive(Debug, Display, Error)]
#[display(r#"expected={expected:?} actual="{actual}" reason={reason:?}"#)]
pub struct MatchError {
    expected: QueryPatternRule,
    actual: String,
    reason: MatchErrorReason,
}

#[derive(Debug, Display, Error, From)]
pub enum MatchQueryError {
    Match(MatchError),
    Scan(pg_query::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum QueryPatternRule {
    Exact(String),
    Variable(String),
}

#[derive(Debug)]
pub struct QueryTemplateMatcher {
    name: Option<String>,
    pattern: Vec<QueryPatternRule>,
    variables: Arc<HashMap<String, VariableTemplateMatcher>>,
}

impl QueryTemplateMatcher {
    pub fn parse(
        template: &str, variable_templates: Arc<HashMap<String, VariableTemplateMatcher>>,
    ) -> Result<Self> {
        let template = template.trim();

        let name = Self::query_name(template).map(str::to_owned);
        let variable_templates = variable_templates.clone();

        if variable_templates.is_empty() {
            return Ok(Self {
                name,
                pattern: vec![QueryPatternRule::Exact(template.into())],
                variables: variable_templates,
            });
        }

        let variable_names: Vec<_> = variable_templates.keys().collect();
        for name in &variable_names {
            ensure!(!name.is_empty(), "variable name cannot be empty");
        }

        let searcher = AhoCorasickBuilder::new()
            .match_kind(MatchKind::LeftmostLongest)
            .build(variable_names.iter())?;

        let mut pattern = Vec::new();
        let mut pos = 0;

        for m in searcher.find_iter(template) {
            if pos < m.start() {
                pattern.push(QueryPatternRule::Exact(template[pos..m.start()].into()));
            }

            pattern.push(QueryPatternRule::Variable(
                variable_names[m.pattern().as_usize()].to_string(),
            ));
            pos = m.end();
        }

        if pos < template.len() {
            pattern.push(QueryPatternRule::Exact(template[pos..].into()));
        }

        Ok(Self { name, pattern, variables: variable_templates })
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn query_name(query: &str) -> Option<&str> {
        let first_line = query.lines().next()?.trim();
        first_line.strip_prefix("--").map(str::trim)
    }

    pub fn match_query(&self, query: &str) -> Result<(), MatchQueryError> {
        let query = query.trim();
        let scan_result = pg_query::scan(query).map_err(MatchQueryError::Scan)?;
        let mut pos = 0;

        for rule in &self.pattern {
            let query_substr = query.get(pos..).ok_or_else(|| MatchError {
                expected: rule.clone(),
                actual: String::new(),
                reason: MatchErrorReason::QueryEof,
            })?;
            let create_match_error =
                |reason| MatchError { expected: rule.clone(), actual: query_substr.into(), reason };

            match rule {
                QueryPatternRule::Exact(expected) => {
                    if let Some((expected, actual)) = divergent_suffixes(expected, query_substr) {
                        return Err(MatchError {
                            expected: QueryPatternRule::Exact(expected),
                            actual,
                            reason: MatchErrorReason::ExactMismatch,
                        })?;
                    }
                    pos += expected.len();
                }
                QueryPatternRule::Variable(variable) => {
                    let matcher = self.variables.get(variable).ok_or_else(|| {
                        create_match_error(MatchErrorReason::UnknownVariable(variable.clone()))
                    })?;
                    matcher.match_query(query, &scan_result, &mut pos).map_err(|err| {
                        create_match_error(MatchErrorReason::VariableMismatch(err))
                    })?;
                }
            }
        }

        if let Some(suffix) = query.get(pos..)
            && !suffix.is_empty()
        {
            return Err(MatchError {
                expected: QueryPatternRule::Exact("".into()),
                actual: suffix.into(),
                reason: MatchErrorReason::UnmatchedSuffix,
            })?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, LazyLock};

    use super::*;

    static VARIABLES: LazyLock<Arc<HashMap<String, VariableTemplateMatcher>>> = LazyLock::new(
        || {
            Arc::new(HashMap::from([
            (
                "$__timeFrom()".into(),
                VariableTemplateMatcher::parse("@@Token(SCONST)@@").unwrap(),
            ),
            (
                "$__timeTo()".into(),
                VariableTemplateMatcher::parse("@@Token(SCONST)@@").unwrap(),
            ),
            (
                "$filter_date".into(),
                VariableTemplateMatcher::parse(
                    "between @@Token(SCONST)@@::date and @@Token(SCONST)@@::date + '1 days'::interval",
                )
                .unwrap(),
            ),
        ]))
        },
    );

    #[test]
    fn matches_query_with_simple_variables() {
        let matcher = QueryTemplateMatcher::parse(
            "SELECT * FROM events WHERE ts >= $__timeFrom() AND ts <= $__timeTo()",
            VARIABLES.clone(),
        )
        .unwrap();

        matcher
            .match_query(
                "SELECT * FROM events WHERE ts >= '2026-08-07T15:41:31.159Z' AND ts <= '2026-08-09T15:41:31.159Z'",
            )
            .unwrap();
    }

    #[test]
    fn matches_query_with_complex_variable() {
        let matcher = QueryTemplateMatcher::parse(
            "SELECT * FROM events WHERE created_at $filter_date ORDER BY created_at",
            VARIABLES.clone(),
        )
        .unwrap();

        matcher
            .match_query(concat!(
                "SELECT * FROM events WHERE created_at ",
                "between '2026-08-07T15:41:31.159Z'::date ",
                "and '2026-08-09T15:41:31.159Z'::date + '1 days'::interval ",
                "ORDER BY created_at"
            ))
            .unwrap();
    }

    #[test]
    fn rejects_changed_fixed_query_text() {
        let matcher = QueryTemplateMatcher::parse(
            "SELECT public_name FROM users WHERE created_at >= $__timeFrom()",
            VARIABLES.clone(),
        )
        .unwrap();

        assert!(
            matcher
                .match_query(
                    "SELECT password_hash FROM users WHERE created_at >= '2026-08-07T15:41:31.159Z'"
                )
                .is_err()
        );
    }

    #[test]
    fn rejects_expression_instead_of_variable_expansion() {
        let matcher = QueryTemplateMatcher::parse(
            "SELECT * FROM events WHERE created_at >= $__timeFrom() AND active = true",
            VARIABLES.clone(),
        )
        .unwrap();

        assert!(
            matcher
                .match_query(
                    "SELECT * FROM events WHERE created_at >= '2026-08-07' OR true -- AND active = true"
                )
                .is_err()
        );
    }

    #[test]
    fn matches_exact_template_without_variables() {
        let matcher =
            QueryTemplateMatcher::parse("SELECT public_name FROM users", HashMap::new().into())
                .unwrap();

        matcher.match_query("SELECT public_name FROM users").unwrap();
        assert!(matcher.match_query("SELECT password_hash FROM users").is_err());
        assert!(matcher.match_query("SELECT public_name FROM users; SELECT 1").is_err());
    }

    #[test]
    fn errors_include_rule_and_query_context() {
        let matcher = QueryTemplateMatcher::parse("SELECT 1", HashMap::new().into()).unwrap();

        let err = matcher.match_query("SELECT 2").unwrap_err();
        let MatchQueryError::Match(err) = err else {
            panic!("expected match error");
        };

        assert!(matches!(err.reason, MatchErrorReason::ExactMismatch));
        assert_eq!(err.expected, QueryPatternRule::Exact("SELECT <DIVERGENCE>1".into()));
        assert_eq!(err.actual, "SELECT <DIVERGENCE>2");
    }

    #[test]
    fn variable_errors_are_statically_typed() {
        let matcher =
            QueryTemplateMatcher::parse("SELECT $__timeFrom()", Arc::clone(&VARIABLES)).unwrap();

        let err = matcher.match_query("SELECT 123").unwrap_err();
        let MatchQueryError::Match(err) = err else {
            panic!("expected match error");
        };

        assert!(matches!(err.reason, MatchErrorReason::VariableMismatch(_)));
    }

    #[test]
    fn unmatched_suffix_error_is_statically_typed() {
        let matcher =
            QueryTemplateMatcher::parse("SELECT $__timeFrom()", Arc::clone(&VARIABLES)).unwrap();

        let err = matcher.match_query("SELECT '2026-08-07'; DROP TABLE events").unwrap_err();
        let MatchQueryError::Match(err) = err else {
            panic!("expected match error");
        };

        assert!(matches!(err.reason, MatchErrorReason::UnmatchedSuffix));
        assert_eq!(err.expected, QueryPatternRule::Exact(String::new()));
        assert_eq!(err.actual, "; DROP TABLE events");
    }

    #[test]
    fn matches_repeated_variable_occurrences() {
        let matcher = QueryTemplateMatcher::parse(
            "SELECT * FROM events WHERE ts BETWEEN $__timeFrom() AND $__timeFrom()",
            VARIABLES.clone(),
        )
        .unwrap();

        matcher
            .match_query("SELECT * FROM events WHERE ts BETWEEN '2026-08-07' AND '2026-08-07'")
            .unwrap();
    }

    #[test]
    fn matches_variable_at_start_of_template() {
        let matcher =
            QueryTemplateMatcher::parse("$__timeFrom()::timestamp", Arc::clone(&VARIABLES))
                .unwrap();

        matcher.match_query("'2026-08-07'::timestamp").unwrap();
    }

    #[test]
    fn matches_adjacent_variable_placeholders() {
        let variables = Arc::new(HashMap::from([
            ("$value".into(), VariableTemplateMatcher::parse("@@Token(SCONST)@@::").unwrap()),
            ("$type".into(), VariableTemplateMatcher::parse("@@Token(TEXT_P)@@").unwrap()),
        ]));
        let matcher = QueryTemplateMatcher::parse("SELECT $value$type", variables).unwrap();

        matcher.match_query("SELECT 'hello'::text").unwrap();
    }

    #[test]
    fn rejects_wrong_token_type_for_variable() {
        let matcher =
            QueryTemplateMatcher::parse("SELECT $__timeFrom()", Arc::clone(&VARIABLES)).unwrap();

        assert!(matcher.match_query("SELECT 123").is_err());
    }

    #[test]
    fn rejects_unmatched_suffix_after_final_variable() {
        let matcher =
            QueryTemplateMatcher::parse("SELECT $__timeFrom()", Arc::clone(&VARIABLES)).unwrap();

        assert!(matcher.match_query("SELECT '2026-08-07'; DROP TABLE events").is_err());
    }

    #[test]
    fn rejects_empty_variable_name() {
        let variables = Arc::new(HashMap::from([(
            String::new(),
            VariableTemplateMatcher::parse("@@Token(SCONST)@@").unwrap(),
        )]));

        assert!(QueryTemplateMatcher::parse("SELECT 1", variables).is_err());
    }

    #[test]
    fn longest_variable_name_wins() {
        let variables = Arc::new(HashMap::from([
            ("$filter".to_string(), VariableTemplateMatcher::parse("@@Token(SCONST)@@").unwrap()),
            (
                "$filter_date".to_string(),
                VariableTemplateMatcher::parse("@@Token(ICONST)@@").unwrap(),
            ),
        ]));

        let matcher = QueryTemplateMatcher::parse("SELECT $filter_date", variables).unwrap();

        matcher.match_query("SELECT 123").unwrap();
        assert!(matcher.match_query("SELECT '123'_date").is_err());
    }
}
