//! 受限规则集的解析、规范化和不可变匹配索引。

use std::collections::{BTreeSet, HashSet};
use std::fmt;

use serde::Deserialize;
use thiserror::Error;

use super::hosts::CanonicalDomain;
use crate::config::model::RuleSetFormat;

const DEFAULT_MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_RULES: usize = 131_072;
const DEFAULT_MAX_RULE_BYTES: usize = 4 * 1024;
const DEFAULT_MAX_REGEX_BYTES: usize = 2 * 1024;
const DEFAULT_MAX_REGEX_PROGRAM: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuleLimits {
    pub max_input_bytes: usize,
    pub max_rules: usize,
    pub max_rule_bytes: usize,
    pub max_regex_bytes: usize,
    pub max_regex_program: usize,
}

impl Default for RuleLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_rules: DEFAULT_MAX_RULES,
            max_rule_bytes: DEFAULT_MAX_RULE_BYTES,
            max_regex_bytes: DEFAULT_MAX_REGEX_BYTES,
            max_regex_program: DEFAULT_MAX_REGEX_PROGRAM,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum RuleParseError {
    #[error("rule resource format is unsupported")]
    UnsupportedFormat,
    #[error("rule resource exceeds the input size limit")]
    InputTooLarge,
    #[error("rule resource exceeds the rule limit")]
    TooManyRules,
    #[error("rule line {line} exceeds the rule size limit")]
    RuleTooLong { line: usize },
    #[error("rule JSON is invalid")]
    InvalidJson,
    #[error("rule JSON does not contain a supported rule field")]
    MissingRuleField,
    #[error("rule line {line} is empty")]
    EmptyRule { line: usize },
    #[error("rule line {line} has an invalid field count")]
    InvalidFieldCount { line: usize },
    #[error("rule line {line} has an unsupported rule type")]
    UnknownRuleType { line: usize },
    #[error("rule line {line} contains an invalid domain")]
    InvalidDomain { line: usize },
    #[error("rule line {line} contains an invalid regex")]
    InvalidRegex { line: usize },
    #[error("rule line {line} contains an unsupported regex construct")]
    UnsupportedRegex { line: usize },
    #[error("rule line {line} contains too many regex instructions")]
    RegexProgramTooLarge { line: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuleMatch {
    Exact,
    Suffix { label_count: usize },
    Regex { ordinal: usize },
}

#[derive(Clone, Eq, PartialEq)]
pub struct RuleIndex {
    exact: BTreeSet<CanonicalDomain>,
    suffix: BTreeSet<CanonicalDomain>,
    regex: Vec<CompiledRegex>,
    rule_count: usize,
}

impl fmt::Debug for RuleIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuleIndex")
            .field("exact_count", &self.exact.len())
            .field("suffix_count", &self.suffix.len())
            .field("regex_count", &self.regex.len())
            .field("rule_count", &self.rule_count)
            .finish()
    }
}

impl RuleIndex {
    pub fn parse(input: &str, format: RuleSetFormat) -> Result<Self, RuleParseError> {
        Self::parse_with_limits(input, format, RuleLimits::default())
    }

    pub fn parse_with_limits(
        input: &str,
        format: RuleSetFormat,
        limits: RuleLimits,
    ) -> Result<Self, RuleParseError> {
        if input.len() > limits.max_input_bytes {
            return Err(RuleParseError::InputTooLarge);
        }
        match format {
            RuleSetFormat::Json => Self::parse_json_with_limits(input, limits),
            RuleSetFormat::Clash => Self::parse_clash_with_limits(input, limits),
            RuleSetFormat::Dat => Err(RuleParseError::UnsupportedFormat),
        }
    }

    pub fn parse_json(input: &str) -> Result<Self, RuleParseError> {
        Self::parse_json_with_limits(input, RuleLimits::default())
    }

    pub fn parse_json_with_limits(input: &str, limits: RuleLimits) -> Result<Self, RuleParseError> {
        if input.len() > limits.max_input_bytes {
            return Err(RuleParseError::InputTooLarge);
        }
        let document: JsonRuleDocument =
            yaml_serde::from_str(input).map_err(|_| RuleParseError::InvalidJson)?;
        let mut index = Self::empty();
        let mut found = false;
        if let Some(values) = document.domain {
            found = true;
            for value in values.into_values() {
                index.add_domain(RuleKind::Exact, &value, 0, limits)?;
            }
        }
        if let Some(values) = document.domain_suffix {
            found = true;
            for value in values.into_values() {
                index.add_domain(RuleKind::Suffix, &value, 0, limits)?;
            }
        }
        if let Some(values) = document.domain_regex {
            found = true;
            for value in values.into_values() {
                index.add_regex(&value, 0, limits)?;
            }
        }
        if !found {
            return Err(RuleParseError::MissingRuleField);
        }
        Ok(index)
    }

    pub fn parse_clash(input: &str) -> Result<Self, RuleParseError> {
        Self::parse_clash_with_limits(input, RuleLimits::default())
    }

    pub fn parse_clash_with_limits(
        input: &str,
        limits: RuleLimits,
    ) -> Result<Self, RuleParseError> {
        if input.len() > limits.max_input_bytes {
            return Err(RuleParseError::InputTooLarge);
        }
        let mut index = Self::empty();
        for (line_index, raw_line) in input.lines().enumerate() {
            let line = line_index + 1;
            if raw_line.len() > limits.max_rule_bytes {
                return Err(RuleParseError::RuleTooLong { line });
            }
            let content = raw_line
                .split_once('#')
                .map_or(raw_line, |(content, _)| content)
                .trim();
            if content.is_empty() {
                continue;
            }
            let fields = content.split(',').map(str::trim).collect::<Vec<_>>();
            if fields.len() != 2 || fields.iter().any(|field| field.is_empty()) {
                return Err(RuleParseError::InvalidFieldCount { line });
            }
            match fields[0].to_ascii_uppercase().as_str() {
                "DOMAIN" => index.add_domain(RuleKind::Exact, fields[1], line, limits)?,
                "DOMAIN-SUFFIX" => index.add_domain(RuleKind::Suffix, fields[1], line, limits)?,
                "DOMAIN-REGEX" => index.add_regex(fields[1], line, limits)?,
                _ => return Err(RuleParseError::UnknownRuleType { line }),
            }
        }
        Ok(index)
    }

    pub fn is_empty(&self) -> bool {
        self.rule_count == 0
    }

    pub fn rule_count(&self) -> usize {
        self.rule_count
    }

    pub fn exact_count(&self) -> usize {
        self.exact.len()
    }

    pub fn suffix_count(&self) -> usize {
        self.suffix.len()
    }

    pub fn regex_count(&self) -> usize {
        self.regex.len()
    }

    pub fn matches(&self, name: &CanonicalDomain) -> Option<RuleMatch> {
        if self.exact.contains(name) {
            return Some(RuleMatch::Exact);
        }

        let labels = name.as_str().split('.').collect::<Vec<_>>();
        for start in 0..labels.len() {
            let suffix = labels[start..].join(".");
            let Ok(candidate) = CanonicalDomain::parse(&suffix) else {
                continue;
            };
            if self.suffix.contains(&candidate) {
                return Some(RuleMatch::Suffix {
                    label_count: labels.len() - start,
                });
            }
        }

        self.regex.iter().enumerate().find_map(|(ordinal, regex)| {
            regex
                .is_match(name.as_str())
                .then_some(RuleMatch::Regex { ordinal })
        })
    }

    fn empty() -> Self {
        Self {
            exact: BTreeSet::new(),
            suffix: BTreeSet::new(),
            regex: Vec::new(),
            rule_count: 0,
        }
    }

    fn add_domain(
        &mut self,
        kind: RuleKind,
        value: &str,
        line: usize,
        limits: RuleLimits,
    ) -> Result<(), RuleParseError> {
        if value.len() > limits.max_rule_bytes {
            return Err(RuleParseError::RuleTooLong { line });
        }
        let domain =
            CanonicalDomain::parse(value).map_err(|_| RuleParseError::InvalidDomain { line })?;
        if domain.is_wildcard() {
            return Err(RuleParseError::InvalidDomain { line });
        }
        let inserted = match kind {
            RuleKind::Exact => self.exact.insert(domain),
            RuleKind::Suffix => self.suffix.insert(domain),
        };
        if inserted {
            self.bump_rule_count(limits)?;
        }
        Ok(())
    }

    fn add_regex(
        &mut self,
        value: &str,
        line: usize,
        limits: RuleLimits,
    ) -> Result<(), RuleParseError> {
        if value.len() > limits.max_regex_bytes {
            return Err(RuleParseError::RuleTooLong { line });
        }
        let compiled = CompiledRegex::compile(value, limits.max_regex_program)
            .map_err(|error| error.with_line(line))?;
        self.regex.push(compiled);
        self.bump_rule_count(limits)
    }

    fn bump_rule_count(&mut self, limits: RuleLimits) -> Result<(), RuleParseError> {
        if self.rule_count >= limits.max_rules {
            return Err(RuleParseError::TooManyRules);
        }
        self.rule_count += 1;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum RuleKind {
    Exact,
    Suffix,
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum JsonValues {
    One(String),
    Many(Vec<String>),
}

impl JsonValues {
    fn into_values(self) -> Vec<String> {
        match self {
            Self::One(value) => vec![value],
            Self::Many(values) => values,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonRuleDocument {
    #[serde(default)]
    domain: Option<JsonValues>,
    #[serde(default)]
    domain_suffix: Option<JsonValues>,
    #[serde(default)]
    domain_regex: Option<JsonValues>,
}

#[derive(Clone, Eq, PartialEq)]
struct CompiledRegex {
    anchored_start: bool,
    anchored_end: bool,
    tokens: Vec<RegexToken>,
}

#[derive(Clone, Eq, PartialEq)]
struct RegexToken {
    atom: RegexAtom,
    min: usize,
    max: Option<usize>,
}

#[derive(Clone, Eq, PartialEq)]
enum RegexAtom {
    Literal(char),
    Any,
    Class {
        ranges: Vec<(char, char)>,
        negated: bool,
    },
}

#[derive(Clone, Copy)]
enum RegexCompileError {
    Invalid,
    Unsupported,
    TooLarge,
}

impl RegexCompileError {
    fn with_line(self, line: usize) -> RuleParseError {
        match self {
            Self::Invalid => RuleParseError::InvalidRegex { line },
            Self::Unsupported => RuleParseError::UnsupportedRegex { line },
            Self::TooLarge => RuleParseError::RegexProgramTooLarge { line },
        }
    }
}

impl CompiledRegex {
    fn compile(pattern: &str, max_program: usize) -> Result<Self, RegexCompileError> {
        let chars = pattern.chars().collect::<Vec<_>>();
        if chars.is_empty() {
            return Err(RegexCompileError::Invalid);
        }
        let mut position = 0;
        let anchored_start = chars.first() == Some(&'^');
        if anchored_start {
            position += 1;
        }
        let anchored_end = chars.last() == Some(&'$') && !is_escaped(&chars, chars.len() - 1);
        let end = if anchored_end {
            chars.len() - 1
        } else {
            chars.len()
        };
        let mut tokens = Vec::new();
        while position < end {
            let atom = match chars[position] {
                '^' | '$' | '(' | ')' | '|' => return Err(RegexCompileError::Unsupported),
                '.' => {
                    position += 1;
                    RegexAtom::Any
                }
                '[' => {
                    let (atom, next) = parse_class(&chars, position, end)?;
                    position = next;
                    atom
                }
                '\\' => {
                    position += 1;
                    let escaped = *chars.get(position).ok_or(RegexCompileError::Invalid)?;
                    position += 1;
                    match escaped {
                        'd' => RegexAtom::Class {
                            ranges: vec![('0', '9')],
                            negated: false,
                        },
                        'w' => RegexAtom::Class {
                            ranges: vec![('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')],
                            negated: false,
                        },
                        's' => RegexAtom::Class {
                            ranges: vec![('\t', '\t'), (' ', ' ')],
                            negated: false,
                        },
                        other => RegexAtom::Literal(other),
                    }
                }
                literal => {
                    position += 1;
                    RegexAtom::Literal(literal)
                }
            };

            let (min, max) = match chars.get(position).copied() {
                Some('*') => {
                    position += 1;
                    (0, None)
                }
                Some('+') => {
                    position += 1;
                    (1, None)
                }
                Some('?') => {
                    position += 1;
                    (0, Some(1))
                }
                Some('{') => return Err(RegexCompileError::Unsupported),
                _ => (1, Some(1)),
            };
            tokens.push(RegexToken { atom, min, max });
            if tokens.len() > max_program {
                return Err(RegexCompileError::TooLarge);
            }
        }
        if tokens.is_empty() {
            return Err(RegexCompileError::Invalid);
        }
        Ok(Self {
            anchored_start,
            anchored_end,
            tokens,
        })
    }

    fn is_match(&self, text: &str) -> bool {
        let chars = text.chars().collect::<Vec<_>>();
        let starts = if self.anchored_start {
            vec![0]
        } else {
            (0..=chars.len()).collect()
        };
        starts.into_iter().any(|start| {
            let mut failed = HashSet::new();
            match_here(
                &self.tokens,
                0,
                start,
                &chars,
                self.anchored_end,
                &mut failed,
            )
        })
    }
}

fn parse_class(
    chars: &[char],
    start: usize,
    end: usize,
) -> Result<(RegexAtom, usize), RegexCompileError> {
    let mut position = start + 1;
    let negated = chars.get(position) == Some(&'^');
    if negated {
        position += 1;
    }
    let mut ranges = Vec::new();
    while position < end && chars[position] != ']' {
        let first = if chars[position] == '\\' {
            position += 1;
            let escaped = *chars.get(position).ok_or(RegexCompileError::Invalid)?;
            position += 1;
            escaped
        } else {
            let value = chars[position];
            position += 1;
            value
        };
        if chars.get(position) == Some(&'-') && chars.get(position + 1) != Some(&']') {
            position += 1;
            let last = *chars.get(position).ok_or(RegexCompileError::Invalid)?;
            position += 1;
            if first > last {
                return Err(RegexCompileError::Invalid);
            }
            ranges.push((first, last));
        } else {
            ranges.push((first, first));
        }
    }
    if ranges.is_empty() || position >= end || chars[position] != ']' {
        return Err(RegexCompileError::Invalid);
    }
    Ok((RegexAtom::Class { ranges, negated }, position + 1))
}

fn is_escaped(chars: &[char], position: usize) -> bool {
    let mut slash_count = 0;
    let mut cursor = position;
    while cursor > 0 && chars[cursor - 1] == '\\' {
        slash_count += 1;
        cursor -= 1;
    }
    slash_count % 2 == 1
}

fn match_here(
    tokens: &[RegexToken],
    token_index: usize,
    char_index: usize,
    text: &[char],
    anchored_end: bool,
    failed: &mut HashSet<(usize, usize)>,
) -> bool {
    if token_index == tokens.len() {
        return !anchored_end || char_index == text.len();
    }
    if failed.contains(&(token_index, char_index)) {
        return false;
    }
    let token = &tokens[token_index];
    let mut positions = vec![char_index];
    while token.max.is_none_or(|max| positions.len() - 1 < max)
        && positions
            .last()
            .copied()
            .is_some_and(|position| position < text.len())
        && atom_matches(&token.atom, text[positions[positions.len() - 1]])
    {
        positions.push(positions[positions.len() - 1] + 1);
    }
    let max_count = positions.len() - 1;
    for count in (token.min..=max_count).rev() {
        if match_here(
            tokens,
            token_index + 1,
            positions[count],
            text,
            anchored_end,
            failed,
        ) {
            return true;
        }
    }
    failed.insert((token_index, char_index));
    false
}

fn atom_matches(atom: &RegexAtom, character: char) -> bool {
    match atom {
        RegexAtom::Literal(expected) => *expected == character,
        RegexAtom::Any => true,
        RegexAtom::Class { ranges, negated } => {
            let matched = ranges
                .iter()
                .any(|(start, end)| (*start..=*end).contains(&character));
            if *negated { !matched } else { matched }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn domain(value: &str) -> CanonicalDomain {
        CanonicalDomain::parse(value).expect("valid domain")
    }

    #[test]
    fn parses_json_and_applies_exact_suffix_regex_priority() {
        let index = RuleIndex::parse_json(
            r#"{"domain":"WWW.Example.COM.","domain_suffix":["example.com"],"domain_regex":"^api\\.test\\.com$"}"#,
        )
        .expect("valid JSON rules");

        assert_eq!(index.rule_count(), 3);
        assert_eq!(
            index.matches(&domain("www.example.com")),
            Some(RuleMatch::Exact)
        );
        assert_eq!(
            index.matches(&domain("a.example.com")),
            Some(RuleMatch::Suffix { label_count: 2 })
        );
        assert_eq!(
            index.matches(&domain("api.test.com")),
            Some(RuleMatch::Regex { ordinal: 0 })
        );
    }

    #[test]
    fn parses_clash_with_line_numbers_and_longest_suffix() {
        let index = RuleIndex::parse_clash(
            "# comment\nDOMAIN-SUFFIX,example.com\nDOMAIN-SUFFIX,api.example.com\nDOMAIN,exact.test\n",
        )
        .expect("valid clash rules");

        assert_eq!(
            index.matches(&domain("www.api.example.com")),
            Some(RuleMatch::Suffix { label_count: 3 })
        );
        assert_eq!(index.matches(&domain("exact.test")), Some(RuleMatch::Exact));
        assert!(matches!(
            RuleIndex::parse_clash("DOMAIN,example.com,extra\n"),
            Err(RuleParseError::InvalidFieldCount { line: 1 })
        ));
    }

    #[test]
    fn rejects_unknown_json_fields_wildcard_domains_and_unsupported_regex() {
        assert!(matches!(
            RuleIndex::parse_json(r#"{"unknown":"example.com"}"#),
            Err(RuleParseError::InvalidJson)
        ));
        assert!(matches!(
            RuleIndex::parse_clash("DOMAIN,*.example.com\n"),
            Err(RuleParseError::InvalidDomain { line: 1 })
        ));
        assert!(matches!(
            RuleIndex::parse_clash("DOMAIN-REGEX,(foo|bar)\\.example\\.com\n"),
            Err(RuleParseError::UnsupportedRegex { line: 1 })
        ));
    }

    #[test]
    fn supports_restricted_regex_atoms_without_external_dependency() {
        let index = RuleIndex::parse_clash(
            "DOMAIN-REGEX,^api[0-9]+\\.example\\.com$\nDOMAIN-REGEX,.*\\.internal\\.test$\n",
        )
        .expect("valid regex rules");

        assert!(matches!(
            index.matches(&domain("api12.example.com")),
            Some(RuleMatch::Regex { ordinal: 0 })
        ));
        assert!(matches!(
            index.matches(&domain("a.internal.test")),
            Some(RuleMatch::Regex { ordinal: 1 })
        ));
        assert_eq!(index.matches(&domain("api.example.com")), None);
    }

    #[test]
    fn enforces_limits_and_hides_rule_contents_in_debug() {
        let limits = RuleLimits {
            max_rules: 1,
            ..RuleLimits::default()
        };
        assert!(matches!(
            RuleIndex::parse_clash_with_limits("DOMAIN,a.example\nDOMAIN,b.example\n", limits),
            Err(RuleParseError::TooManyRules)
        ));
        let debug = format!(
            "{:?}",
            RuleIndex::parse_clash("DOMAIN,secret.example\n").unwrap()
        );
        assert!(!debug.contains("secret.example"));
    }
}
