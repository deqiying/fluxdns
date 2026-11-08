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

    /// 裸路径接受自身或追加一个非空 ID 段；显式 `{client_id}` 仍为必填段。
    pub(crate) fn matches<'a>(&self, path: &'a str) -> Option<DohPathMatch<'a>> {
        let value = match self.placeholder {
            None if path == self.template => return Some(DohPathMatch { client_id: None }),
            None => {
                // 根路径和以斜杠结尾的路径复用已有分隔符，不自动归一化请求路径。
                let prefix = self.template.strip_suffix('/').unwrap_or(&self.template);
                path.strip_prefix(prefix)?.strip_prefix('/')?
            }
            Some((start, end)) => {
                if !path.starts_with(&self.template[..start])
                    || !path.ends_with(&self.template[end..])
                {
                    return None;
                }
                let value_end = path.len().checked_sub(self.template.len() - end)?;
                path.get(start..value_end)?
            }
        };
        if value.is_empty() || value.contains('/') || value.contains('?') {
            return None;
        }
        Some(DohPathMatch {
            client_id: Some(value),
        })
    }

    /// 判断实际匹配集合是否重叠，包含裸路径的隐式 ID 后缀。
    pub(crate) fn overlaps(&self, other: &Self) -> bool {
        let left = [Some(self.segments()), self.implicit_id_segments()];
        let right = [Some(other.segments()), other.implicit_id_segments()];
        left.iter().flatten().any(|left| {
            right
                .iter()
                .flatten()
                .any(|right| segments_overlap(left, right))
        })
    }

    /// 固定段优先于参数段；段结构相同时显式模板优先于隐式 ID 后缀。
    /// 将裸路径的可选后缀纳入比较，装配期即可确定与配置顺序无关的优先级。
    pub(crate) fn specificity_cmp(&self, other: &Self) -> Ordering {
        let left = self
            .implicit_id_segments()
            .unwrap_or_else(|| self.segments());
        let right = other
            .implicit_id_segments()
            .unwrap_or_else(|| other.segments());
        left.iter()
            .map(|segment| matches!(segment, PathSegment::Literal(_)))
            .cmp(
                right
                    .iter()
                    .map(|segment| matches!(segment, PathSegment::Literal(_))),
            )
            .then_with(|| self.placeholder.is_some().cmp(&other.placeholder.is_some()))
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

    /// 与 `matches` 共用尾部分隔符语义，供重叠校验和优先级比较使用。
    fn implicit_id_segments(&self) -> Option<Vec<PathSegment<'_>>> {
        if self.placeholder.is_some() {
            return None;
        }
        let mut segments = self.segments();
        if self.template.ends_with('/') {
            segments.pop();
        }
        segments.push(PathSegment::ClientId);
        Some(segments)
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
    fn detects_overlaps_including_implicit_id_suffixes() {
        let dynamic = DohPathPattern::new("/dns/{client_id}").unwrap();

        assert!(dynamic.overlaps(&DohPathPattern::new("/dns").unwrap()));
        assert!(dynamic.overlaps(&DohPathPattern::new("/dns/client-a").unwrap()));
        assert!(!dynamic.overlaps(&DohPathPattern::new("/dns/client-a/{client_id}").unwrap()));
        assert!(dynamic.overlaps(&DohPathPattern::new("/dns/").unwrap()));
        assert!(!dynamic.overlaps(&DohPathPattern::new("/other/{client_id}").unwrap()));

        let bare = DohPathPattern::new("/dns").unwrap();
        let trailing_slash = DohPathPattern::new("/dns/").unwrap();
        assert!(bare.overlaps(&trailing_slash));
        assert!(bare.specificity_cmp(&trailing_slash).is_eq());
        assert!(bare.overlaps(&DohPathPattern::new("/dns/client-a").unwrap()));
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
    fn bare_paths_accept_optional_ids_but_templates_require_them() {
        let bare = DohPathPattern::new("/doh-query").unwrap();
        let identified = DohPathPattern::new("/doh-query/{client_id}").unwrap();
        assert_eq!(bare.matches("/doh-query").unwrap().client_id, None);
        for pattern in [&bare, &identified] {
            assert_eq!(
                pattern.matches("/doh-query/client-a").unwrap().client_id,
                Some("client-a")
            );
            for path in [
                "/doh-query/",
                "/doh-query/a/b",
                "/doh-query//a",
                "/doh-query-other/a",
            ] {
                assert!(pattern.matches(path).is_none(), "{path}");
            }
        }
        assert!(identified.matches("/doh-query").is_none());
        assert!(identified.specificity_cmp(&bare).is_gt());
        assert!(bare.specificity_cmp(&identified).is_lt());
    }

    #[test]
    fn bare_root_and_trailing_slash_reuse_the_separator_for_ids() {
        for (template, with_id, empty, extra) in [
            ("/", "/client-a", "//", "/client-a/extra"),
            ("/dns/", "/dns/client-a", "/dns//", "/dns/client-a/extra"),
        ] {
            let bare = DohPathPattern::new(template).unwrap();
            assert_eq!(bare.matches(template).unwrap().client_id, None);
            assert_eq!(bare.matches(with_id).unwrap().client_id, Some("client-a"));
            assert!(bare.matches(empty).is_none());
            assert!(bare.matches(extra).is_none());
            let explicit = DohPathPattern::new(format!("{template}{{client_id}}")).unwrap();
            assert!(bare.overlaps(&explicit));
            assert!(explicit.specificity_cmp(&bare).is_gt());
        }
        assert!(
            DohPathPattern::new("/dns/")
                .unwrap()
                .matches("/dns")
                .is_none()
        );
    }

    #[test]
    fn implicit_fixed_prefix_wins_over_an_earlier_parameter() {
        let bare = DohPathPattern::new("/doh-query/inner").unwrap();
        let generic = DohPathPattern::new("/doh-query/{client_id}/query").unwrap();
        assert!(bare.overlaps(&generic));
        assert!(bare.specificity_cmp(&generic).is_gt());
        assert_eq!(
            bare.matches("/doh-query/inner/query").unwrap().client_id,
            Some("query")
        );
    }
}
