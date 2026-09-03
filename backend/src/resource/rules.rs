//! 受限规则集的解析、规范化和不可变匹配索引。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::sync::Arc;

use serde::Deserialize;
use thiserror::Error;

use super::hosts::CanonicalDomain;
use crate::config::model::RuleSetFormat;

const DEFAULT_MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_RULES: usize = 131_072;
const DEFAULT_MAX_RULE_BYTES: usize = 4 * 1024;
const DEFAULT_MAX_REGEX_BYTES: usize = 2 * 1024;
const DEFAULT_MAX_REGEX_PROGRAM: usize = 4_096;
const DEFAULT_MAX_SELECTORS: usize = 4_096;
const DEFAULT_MAX_SELECTOR_BYTES: usize = 128;
const WIRE_VARINT: u8 = 0;
const WIRE_I64: u8 = 1;
const WIRE_LEN_DELIM: u8 = 2;
const WIRE_I32: u8 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuleLimits {
    pub max_input_bytes: usize,
    pub max_rules: usize,
    pub max_rule_bytes: usize,
    pub max_regex_bytes: usize,
    pub max_regex_program: usize,
    pub max_selectors: usize,
    pub max_selector_bytes: usize,
}

impl Default for RuleLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_rules: DEFAULT_MAX_RULES,
            max_rule_bytes: DEFAULT_MAX_RULE_BYTES,
            max_regex_bytes: DEFAULT_MAX_REGEX_BYTES,
            max_regex_program: DEFAULT_MAX_REGEX_PROGRAM,
            max_selectors: DEFAULT_MAX_SELECTORS,
            max_selector_bytes: DEFAULT_MAX_SELECTOR_BYTES,
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
    #[error("rule resource dat payload is invalid")]
    InvalidDat,
    #[error("rule resource dat contains too many selectors")]
    TooManySelectors,
    #[error("rule resource dat selector is invalid")]
    InvalidDatSelector,
    #[error("rule resource dat selector is too long")]
    DatSelectorTooLong,
    #[error("rule resource dat contains an unsupported domain type")]
    UnsupportedDatDomainType,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuleMatch {
    Exact,
    Suffix { label_count: usize },
    Keyword { ordinal: usize },
    Regex { ordinal: usize },
}

#[derive(Clone, Eq, PartialEq)]
pub struct RuleIndex {
    exact: BTreeSet<CanonicalDomain>,
    suffix: BTreeSet<CanonicalDomain>,
    keywords: BTreeSet<String>,
    regex: Vec<CompiledRegex>,
    selectors: BTreeMap<String, Arc<RuleIndex>>,
    rule_count: usize,
}

impl fmt::Debug for RuleIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuleIndex")
            .field("exact_count", &self.exact.len())
            .field("suffix_count", &self.suffix.len())
            .field("keyword_count", &self.keywords.len())
            .field("regex_count", &self.regex.len())
            .field("selector_count", &self.selectors.len())
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
            RuleSetFormat::Dat => Self::parse_dat_with_limits(input.as_bytes(), limits),
        }
    }

    /// 解析指定格式的原始字节。`dat` 是 protobuf 二进制，不能先转换为 UTF-8。
    pub fn parse_bytes(input: &[u8], format: RuleSetFormat) -> Result<Self, RuleParseError> {
        Self::parse_bytes_with_limits(input, format, RuleLimits::default())
    }

    pub fn parse_bytes_with_limits(
        input: &[u8],
        format: RuleSetFormat,
        limits: RuleLimits,
    ) -> Result<Self, RuleParseError> {
        if input.len() > limits.max_input_bytes {
            return Err(RuleParseError::InputTooLarge);
        }
        match format {
            RuleSetFormat::Dat => Self::parse_dat_with_limits(input, limits),
            RuleSetFormat::Json | RuleSetFormat::Clash => {
                let text = std::str::from_utf8(input).map_err(|_| RuleParseError::InvalidJson)?;
                Self::parse_with_limits(text, format, limits)
            }
        }
    }

    pub fn parse_dat(input: &[u8]) -> Result<Self, RuleParseError> {
        Self::parse_dat_with_limits(input, RuleLimits::default())
    }

    pub fn parse_dat_with_limits(input: &[u8], limits: RuleLimits) -> Result<Self, RuleParseError> {
        if input.len() > limits.max_input_bytes {
            return Err(RuleParseError::InputTooLarge);
        }
        let mut reader = DatReader::new(input);
        let mut selectors: BTreeMap<String, RuleIndex> = BTreeMap::new();
        while let Some((field, wire)) = reader.read_key()? {
            if field == 1 && wire == WIRE_LEN_DELIM {
                let payload = reader.read_length_delimited()?;
                let site = parse_dat_site(payload, limits)?;
                let selector = normalize_dat_selector(&site.selector, limits)?;
                if selectors.contains_key(&selector) {
                    return Err(RuleParseError::InvalidDatSelector);
                }
                if selectors.len() >= limits.max_selectors {
                    return Err(RuleParseError::TooManySelectors);
                }
                let total_rules = selectors
                    .values()
                    .map(|index| index.rule_count)
                    .sum::<usize>()
                    .checked_add(site.index.rule_count)
                    .ok_or(RuleParseError::TooManyRules)?;
                if total_rules > limits.max_rules {
                    return Err(RuleParseError::TooManyRules);
                }
                selectors.insert(selector, site.index);
            } else {
                reader.skip_field(wire)?;
            }
        }
        if !reader.is_exhausted() || selectors.is_empty() {
            return Err(RuleParseError::InvalidDat);
        }
        let rule_count = selectors.values().map(|index| index.rule_count).sum();
        Ok(Self {
            exact: BTreeSet::new(),
            suffix: BTreeSet::new(),
            keywords: BTreeSet::new(),
            regex: Vec::new(),
            selectors: selectors
                .into_iter()
                .map(|(name, index)| (name, Arc::new(index)))
                .collect(),
            rule_count,
        })
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

    pub fn keyword_count(&self) -> usize {
        self.keywords.len()
    }

    pub fn selector_count(&self) -> usize {
        self.selectors.len()
    }

    pub fn is_selector_map(&self) -> bool {
        !self.selectors.is_empty()
    }

    pub fn selector(&self, value: &str) -> Option<&RuleIndex> {
        self.selectors.get(value).map(Arc::as_ref)
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

        if let Some((ordinal, _)) = self
            .keywords
            .iter()
            .enumerate()
            .find(|(_, keyword)| name.as_str().contains(keyword.as_str()))
        {
            return Some(RuleMatch::Keyword { ordinal });
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
            keywords: BTreeSet::new(),
            regex: Vec::new(),
            selectors: BTreeMap::new(),
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

    fn add_keyword(&mut self, value: &str, limits: RuleLimits) -> Result<(), RuleParseError> {
        if value.len() > limits.max_rule_bytes || value.is_empty() {
            return Err(RuleParseError::InvalidDat);
        }
        let value = value.to_ascii_lowercase();
        if self.keywords.insert(value) {
            self.bump_rule_count(limits)?;
        }
        Ok(())
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

/// `geosite.dat` 使用 V2Ray 的 protobuf GeoSiteList schema。
///
/// 这里只解析路由需要的字段，未知字段按 protobuf wire type 跳过；整个输入、
/// selector 数量和每个 selector 的规则数量均由 `RuleLimits` 约束，避免把不可信
/// 二进制内容转换为无界内存结构。
struct DatReader<'a> {
    input: &'a [u8],
    position: usize,
}

impl<'a> DatReader<'a> {
    const fn new(input: &'a [u8]) -> Self {
        Self { input, position: 0 }
    }

    fn read_key(&mut self) -> Result<Option<(u32, u8)>, RuleParseError> {
        if self.position == self.input.len() {
            return Ok(None);
        }
        let key = self.read_varint()?;
        let field = u32::try_from(key >> 3).map_err(|_| RuleParseError::InvalidDat)?;
        let wire = u8::try_from(key & 0x07).map_err(|_| RuleParseError::InvalidDat)?;
        if field == 0 {
            return Err(RuleParseError::InvalidDat);
        }
        Ok(Some((field, wire)))
    }

    fn read_length_delimited(&mut self) -> Result<&'a [u8], RuleParseError> {
        let length =
            usize::try_from(self.read_varint()?).map_err(|_| RuleParseError::InvalidDat)?;
        let end = self
            .position
            .checked_add(length)
            .ok_or(RuleParseError::InvalidDat)?;
        if end > self.input.len() {
            return Err(RuleParseError::InvalidDat);
        }
        let value = &self.input[self.position..end];
        self.position = end;
        Ok(value)
    }

    fn skip_field(&mut self, wire: u8) -> Result<(), RuleParseError> {
        match wire {
            WIRE_VARINT => {
                let _ = self.read_varint()?;
            }
            WIRE_I64 => self.advance(8)?,
            WIRE_LEN_DELIM => {
                let _ = self.read_length_delimited()?;
            }
            WIRE_I32 => self.advance(4)?,
            _ => return Err(RuleParseError::InvalidDat),
        }
        Ok(())
    }

    fn read_varint(&mut self) -> Result<u64, RuleParseError> {
        let mut value = 0_u64;
        for shift in (0..=63).step_by(7) {
            let byte = *self
                .input
                .get(self.position)
                .ok_or(RuleParseError::InvalidDat)?;
            self.position += 1;
            if shift == 63 && byte > 1 {
                return Err(RuleParseError::InvalidDat);
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(RuleParseError::InvalidDat)
    }

    fn advance(&mut self, count: usize) -> Result<(), RuleParseError> {
        let end = self
            .position
            .checked_add(count)
            .ok_or(RuleParseError::InvalidDat)?;
        if end > self.input.len() {
            return Err(RuleParseError::InvalidDat);
        }
        self.position = end;
        Ok(())
    }

    const fn is_exhausted(&self) -> bool {
        self.position == self.input.len()
    }
}

struct DatSite {
    selector: String,
    index: RuleIndex,
}

struct DatDomain {
    kind: u64,
    value: String,
}

fn parse_dat_site(payload: &[u8], limits: RuleLimits) -> Result<DatSite, RuleParseError> {
    let mut reader = DatReader::new(payload);
    let mut selector = None;
    let mut domains = Vec::new();
    while let Some((field, wire)) = reader.read_key()? {
        match (field, wire) {
            (1, WIRE_LEN_DELIM) => {
                if selector.is_some() {
                    return Err(RuleParseError::InvalidDat);
                }
                selector = Some(
                    String::from_utf8(reader.read_length_delimited()?.to_vec())
                        .map_err(|_| RuleParseError::InvalidDat)?,
                );
            }
            (2, WIRE_LEN_DELIM) => {
                let domain = parse_dat_domain(reader.read_length_delimited()?)?;
                domains.push(domain);
            }
            _ => reader.skip_field(wire)?,
        }
    }
    let selector = selector.ok_or(RuleParseError::InvalidDat)?;
    let mut index = RuleIndex::empty();
    for domain in domains {
        match domain.kind {
            0 => index.add_keyword(&domain.value, limits)?,
            1 => index.add_regex(&domain.value, 0, limits)?,
            2 => index.add_domain(RuleKind::Suffix, &domain.value, 0, limits)?,
            3 => index.add_domain(RuleKind::Exact, &domain.value, 0, limits)?,
            _ => return Err(RuleParseError::UnsupportedDatDomainType),
        }
    }
    Ok(DatSite { selector, index })
}

fn parse_dat_domain(payload: &[u8]) -> Result<DatDomain, RuleParseError> {
    let mut reader = DatReader::new(payload);
    let mut kind = 0_u64;
    let mut value = None;
    while let Some((field, wire)) = reader.read_key()? {
        match (field, wire) {
            (1, WIRE_VARINT) => kind = reader.read_varint()?,
            (2, WIRE_LEN_DELIM) => {
                if value.is_some() {
                    return Err(RuleParseError::InvalidDat);
                }
                value = Some(
                    String::from_utf8(reader.read_length_delimited()?.to_vec())
                        .map_err(|_| RuleParseError::InvalidDat)?,
                );
            }
            _ => reader.skip_field(wire)?,
        }
    }
    let value = value.ok_or(RuleParseError::InvalidDat)?;
    Ok(DatDomain { kind, value })
}

fn normalize_dat_selector(selector: &str, limits: RuleLimits) -> Result<String, RuleParseError> {
    if selector.is_empty() {
        return Err(RuleParseError::InvalidDatSelector);
    }
    if selector.len() > limits.max_selector_bytes {
        return Err(RuleParseError::DatSelectorTooLong);
    }
    let selector = selector.to_ascii_lowercase();
    if !selector.is_ascii()
        || !selector.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(RuleParseError::InvalidDatSelector);
    }
    Ok(selector)
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

    fn varint(value: u64) -> Vec<u8> {
        let mut value = value;
        let mut bytes = Vec::new();
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            bytes.push(byte);
            if value == 0 {
                return bytes;
            }
        }
    }

    fn field(number: u64, wire: u64, payload: &[u8]) -> Vec<u8> {
        let mut bytes = varint((number << 3) | wire);
        if wire == u64::from(WIRE_LEN_DELIM) {
            bytes.extend(varint(payload.len() as u64));
        }
        bytes.extend(payload);
        bytes
    }

    fn dat_domain(kind: u64, value: &str) -> Vec<u8> {
        let mut bytes = field(1, u64::from(WIRE_VARINT), &varint(kind));
        bytes.extend(field(2, u64::from(WIRE_LEN_DELIM), value.as_bytes()));
        bytes
    }

    fn dat_site(selector: &str, domains: &[(u64, &str)]) -> Vec<u8> {
        let mut site = field(1, u64::from(WIRE_LEN_DELIM), selector.as_bytes());
        for (kind, value) in domains {
            site.extend(field(
                2,
                u64::from(WIRE_LEN_DELIM),
                &dat_domain(*kind, value),
            ));
        }
        field(1, u64::from(WIRE_LEN_DELIM), &site)
    }

    #[test]
    fn parses_geosite_dat_selectors_and_domain_types() {
        let mut input = dat_site(
            "CN",
            &[
                (0, "keyword"),
                (1, "^api[0-9]+\\.example\\.com$"),
                (2, "suffix.example"),
                (3, "exact.example"),
            ],
        );
        input.extend(dat_site("private", &[(3, "internal.example")]));

        let index = RuleIndex::parse_dat(&input).expect("valid geosite.dat payload");
        assert_eq!(index.selector_count(), 2);
        assert_eq!(index.selector("cn").unwrap().keyword_count(), 1);
        let cn = index.selector("cn").unwrap();
        assert_eq!(
            cn.matches(&domain("keyword.example")),
            Some(RuleMatch::Keyword { ordinal: 0 })
        );
        assert_eq!(
            cn.matches(&domain("api12.example.com")),
            Some(RuleMatch::Regex { ordinal: 0 })
        );
        assert_eq!(
            cn.matches(&domain("a.suffix.example")),
            Some(RuleMatch::Suffix { label_count: 2 })
        );
        assert_eq!(cn.matches(&domain("exact.example")), Some(RuleMatch::Exact));
        assert_eq!(
            index
                .selector("private")
                .unwrap()
                .matches(&domain("internal.example")),
            Some(RuleMatch::Exact)
        );
        assert!(!format!("{index:?}").contains("suffix.example"));
    }

    #[test]
    fn dat_parser_rejects_duplicates_malformed_payload_and_limits() {
        let mut duplicate = dat_site("cn", &[(3, "example.test")]);
        duplicate.extend(dat_site("CN", &[(3, "other.test")]));
        assert!(matches!(
            RuleIndex::parse_dat(&duplicate),
            Err(RuleParseError::InvalidDatSelector)
        ));

        assert!(matches!(
            RuleIndex::parse_dat(&[0x0a, 0x05, 0x0a]),
            Err(RuleParseError::InvalidDat)
        ));
        let input = dat_site("cn", &[(3, "example.test")]);
        assert!(matches!(
            RuleIndex::parse_dat_with_limits(
                &input,
                RuleLimits {
                    max_selectors: 0,
                    ..RuleLimits::default()
                }
            ),
            Err(RuleParseError::TooManySelectors)
        ));
        assert!(matches!(
            RuleIndex::parse_dat_with_limits(
                &input,
                RuleLimits {
                    max_selector_bytes: 1,
                    ..RuleLimits::default()
                }
            ),
            Err(RuleParseError::DatSelectorTooLong)
        ));
    }
}
