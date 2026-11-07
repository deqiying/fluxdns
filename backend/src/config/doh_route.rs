//! DoH route path 模板的共享解析、匹配与冲突检测。

use std::cmp::Ordering;

const CLIENT_ID_MARKER: &str = "{client_id}";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DohPathPatternError {
    InvalidPath,
    InvalidPlaceholder,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DohPathPattern {
    template: String,
    placeholder: Option<(usize, usize)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DohPathMatch<'a> {
    pub(crate) client_id: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PathSegment<'a> {
    Literal(&'a str),
    ClientId,
}

impl DohPathPattern {
    pub(crate) fn new(template: impl Into<String>) -> Result<Self, DohPathPatternError> {
        let template = template.into();
        if template.is_empty()
            || !template.starts_with('/')
            || template.contains('?')
            || template.contains('#')
            || template
                .as_bytes()
                .iter()
                .any(|byte| *byte < 0x20 || *byte == 0x7f)
        {
            return Err(DohPathPatternError::InvalidPath);
        }

        let first = template.find(CLIENT_ID_MARKER);
        if first.is_some_and(|index| {
            template[index + CLIENT_ID_MARKER.len()..].contains(CLIENT_ID_MARKER)
        }) {
            return Err(DohPathPatternError::InvalidPlaceholder);
        }
        let placeholder = first.map(|start| (start, start + CLIENT_ID_MARKER.len()));
        if let Some((start, end)) = placeholder {
            let segment_start = start > 0 && template.as_bytes()[start - 1] == b'/';
            let segment_end = end == template.len() || template.as_bytes()[end] == b'/';
            if !segment_start || !segment_end {
                return Err(DohPathPatternError::InvalidPlaceholder);
            }
        }

        Ok(Self {
            template,
            placeholder,
        })
    }

    pub(crate) fn template(&self) -> &str {
        &self.template
    }

    /// 裸路径仅精确匹配；模板中的 `{client_id}` 必须提供一个非空路径段。
    pub(crate) fn matches<'a>(&self, path: &'a str) -> Option<DohPathMatch<'a>> {
        let client_id = match self.placeholder {
            None if path == self.template => None,
            Some((start, end)) => {
                if !path.starts_with(&self.template[..start])
                    || !path.ends_with(&self.template[end..])
                {
                    return None;
                }
                let value_end = path.len().checked_sub(self.template.len() - end)?;
                let value = path.get(start..value_end)?;
                if value.is_empty() || value.contains('/') || value.contains('?') {
                    return None;
                }
                Some(value)
            }
            _ => return None,
        };
        Some(DohPathMatch { client_id })
    }

    /// 判断两个模板是否存在至少一个会同时命中的 HTTP path。
    pub(crate) fn overlaps(&self, other: &Self) -> bool {
        segments_overlap(&self.segments(), &other.segments())
    }

    /// 从左到右比较段类型，固定段优先于参数段；值越大越优先，不依赖配置顺序。
    /// 字面量内容由匹配/重叠检查处理，这里不按路径字符串字典序决定策略。
    pub(crate) fn specificity_cmp(&self, other: &Self) -> Ordering {
        self.segments()
            .iter()
            .map(|segment| matches!(segment, PathSegment::Literal(_)))
            .cmp(
                other
                    .segments()
                    .iter()
                    .map(|segment| matches!(segment, PathSegment::Literal(_))),
            )
    }

    fn segments(&self) -> Vec<PathSegment<'_>> {
        let path = &self.template;
        path.split('/')
            .skip(1)
            .map(|segment| {
                if segment == CLIENT_ID_MARKER {
                    PathSegment::ClientId
                } else {
                    PathSegment::Literal(segment)
                }
            })
            .collect()
    }
}

fn segments_overlap(left: &[PathSegment<'_>], right: &[PathSegment<'_>]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| match (left, right) {
                (PathSegment::ClientId, PathSegment::ClientId) => true,
                (PathSegment::ClientId, PathSegment::Literal(value))
                | (PathSegment::Literal(value), PathSegment::ClientId) => !value.is_empty(),
                (PathSegment::Literal(left), PathSegment::Literal(right)) => left == right,
            })
}

#[cfg(test)]
mod tests {
    use super::{DohPathPattern, DohPathPatternError};

    #[test]
    fn terminal_client_id_requires_a_nonempty_segment() {
        let pattern = DohPathPattern::new("/doh-query/inner/{client_id}").unwrap();

        assert_eq!(
            pattern
                .matches("/doh-query/inner/client-a")
                .unwrap()
                .client_id,
            Some("client-a")
        );
        assert!(pattern.matches("/doh-query/inner").is_none());
        assert!(pattern.matches("/doh-query/inner/").is_none());
        assert!(pattern.matches("/doh-query/inner/a/b").is_none());
    }

    #[test]
    fn non_terminal_client_id_still_requires_a_value() {
        let pattern = DohPathPattern::new("/client/{client_id}/dns-query").unwrap();

        assert_eq!(
            pattern.matches("/client/a/dns-query").unwrap().client_id,
            Some("a")
        );
        assert!(pattern.matches("/client/dns-query").is_none());
    }

    #[test]
    fn root_client_id_template_does_not_match_root_without_id() {
        let pattern = DohPathPattern::new("/{client_id}").unwrap();

        assert!(pattern.matches("/").is_none());
        assert!(pattern.matches("/client-a").is_some());
    }

    #[test]
    fn rejects_embedded_or_repeated_placeholder() {
        assert_eq!(
            DohPathPattern::new("/dns/{client_id}/x/{client_id}"),
            Err(DohPathPatternError::InvalidPlaceholder)
        );
        assert_eq!(
            DohPathPattern::new("/dns/pre{client_id}"),
            Err(DohPathPatternError::InvalidPlaceholder)
        );
    }

    #[test]
    fn detects_overlaps_without_implicit_bare_paths() {
        let dynamic = DohPathPattern::new("/dns/{client_id}").unwrap();

        assert!(!dynamic.overlaps(&DohPathPattern::new("/dns").unwrap()));
        assert!(dynamic.overlaps(&DohPathPattern::new("/dns/client-a").unwrap()));
        assert!(!dynamic.overlaps(&DohPathPattern::new("/dns/client-a/{client_id}").unwrap()));
        assert!(!dynamic.overlaps(&DohPathPattern::new("/dns/").unwrap()));
        assert!(!dynamic.overlaps(&DohPathPattern::new("/other/{client_id}").unwrap()));
    }

    #[test]
    fn fixed_segments_take_priority_from_left_to_right() {
        let generic = DohPathPattern::new("/doh-query/{client_id}").unwrap();
        let fixed = DohPathPattern::new("/doh-query/inner").unwrap();
        assert!(fixed.overlaps(&generic));
        assert!(fixed.specificity_cmp(&generic).is_gt());
        assert!(generic.specificity_cmp(&fixed).is_lt());
        assert!(generic.specificity_cmp(&generic).is_eq());

        let earlier_fixed = DohPathPattern::new("/doh-query/inner/{client_id}").unwrap();
        let later_fixed = DohPathPattern::new("/doh-query/{client_id}/query").unwrap();
        assert!(earlier_fixed.overlaps(&later_fixed));
        assert!(earlier_fixed.specificity_cmp(&later_fixed).is_gt());
        assert!(earlier_fixed.matches("/doh-query/inner/query").is_some());
        assert!(later_fixed.matches("/doh-query/inner/query").is_some());
    }

    #[test]
    fn bare_paths_and_id_templates_have_separate_id_semantics() {
        let bare = DohPathPattern::new("/doh-query").unwrap();
        let identified = DohPathPattern::new("/doh-query/{client_id}").unwrap();
        assert_eq!(bare.matches("/doh-query").unwrap().client_id, None);
        assert!(bare.matches("/doh-query/client-a").is_none());
        assert_eq!(
            identified.matches("/doh-query/client-a").unwrap().client_id,
            Some("client-a")
        );
        for path in ["/doh-query", "/doh-query/", "/doh-query/a/b"] {
            assert!(identified.matches(path).is_none(), "{path}");
        }
    }
}
