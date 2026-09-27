//! DoH route path 模板的共享解析、匹配与冲突检测。

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

    /// 末尾 `{client_id}` 同时接受不带尾斜杠的裸路径，裸路径不产生 client ID。
    pub(crate) fn matches<'a>(&self, path: &'a str) -> Option<DohPathMatch<'a>> {
        let client_id = match self.placeholder {
            None if path == self.template => None,
            Some((_start, end)) if end == self.template.len() && path == self.bare_path()? => None,
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
        self.path_variants().iter().any(|left| {
            other
                .path_variants()
                .iter()
                .any(|right| variants_overlap(left, right))
        })
    }

    fn bare_path(&self) -> Option<&str> {
        let (start, end) = self.placeholder?;
        if end != self.template.len() {
            return None;
        }
        let prefix = &self.template[..start];
        if prefix == "/" {
            Some(prefix)
        } else {
            prefix.strip_suffix('/')
        }
    }

    fn path_variants(&self) -> Vec<Vec<PathSegment<'_>>> {
        let mut variants = vec![self.segments(false)];
        if self.bare_path().is_some() {
            variants.push(self.segments(true));
        }
        variants
    }

    fn segments(&self, bare: bool) -> Vec<PathSegment<'_>> {
        let path = if bare {
            self.bare_path().expect("bare variant is available")
        } else {
            &self.template
        };
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

fn variants_overlap(left: &[PathSegment<'_>], right: &[PathSegment<'_>]) -> bool {
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
    fn terminal_client_id_accepts_value_and_bare_path() {
        let pattern = DohPathPattern::new("/doh-query/inner/{client_id}").unwrap();

        assert_eq!(
            pattern
                .matches("/doh-query/inner/client-a")
                .unwrap()
                .client_id,
            Some("client-a")
        );
        assert_eq!(pattern.matches("/doh-query/inner").unwrap().client_id, None);
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
    fn root_is_the_bare_path_for_root_client_id_template() {
        let pattern = DohPathPattern::new("/{client_id}").unwrap();

        assert!(pattern.matches("/").is_some());
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
    fn detects_literal_wildcard_and_bare_path_overlaps() {
        let dynamic = DohPathPattern::new("/dns/{client_id}").unwrap();

        assert!(dynamic.overlaps(&DohPathPattern::new("/dns").unwrap()));
        assert!(dynamic.overlaps(&DohPathPattern::new("/dns/client-a").unwrap()));
        assert!(dynamic.overlaps(&DohPathPattern::new("/dns/client-a/{client_id}").unwrap()));
        assert!(!dynamic.overlaps(&DohPathPattern::new("/dns/").unwrap()));
        assert!(!dynamic.overlaps(&DohPathPattern::new("/other/{client_id}").unwrap()));
    }
}
