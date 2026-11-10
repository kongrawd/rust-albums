//! User-provided storage layout, e.g. `{album_name}/{file_name}` or
//! `year={year}/{album_name}/{file_name}`.
//!
//! Each `/`-separated segment is a template of literal text and `{name}`
//! placeholders. All segments but the last describe the album folder; the last
//! describes the file. Placeholder values become album / item metadata.

use regex::Regex;
use std::collections::{BTreeMap, HashSet};

pub type Metadata = BTreeMap<String, String>;

pub struct Pattern {
    segments: Vec<Segment>,
}

/// Placeholder values captured from one path.
pub struct Captures {
    /// From the folder segments.
    pub album_metadata: Metadata,
    /// From the file segment.
    pub item_metadata: Metadata,
}

struct Segment {
    regex: Regex,
    placeholders: Vec<String>,
}

impl Pattern {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim().trim_matches('/');
        let templates: Vec<&str> = trimmed.split('/').collect();
        if templates.len() < 2 {
            return Err(format!(
                "pattern {raw:?} needs at least one folder and one file segment, e.g. {{album_name}}/{{file_name}}"
            ));
        }
        let mut seen = HashSet::new();
        let segments = templates
            .iter()
            .map(|template| Segment::parse(template, &mut seen))
            .collect::<Result<_, _>>()?;
        Ok(Self { segments })
    }

    /// Number of path segments (folders + file) a matching file has below the root.
    pub fn depth(&self) -> usize {
        self.segments.len()
    }

    pub fn matches(&self, segments: &[&str]) -> Option<Captures> {
        if segments.len() != self.segments.len() {
            return None;
        }
        let mut album_metadata = Metadata::new();
        let last = self.segments.len() - 1;
        for (i, (segment, value)) in self.segments.iter().zip(segments).enumerate() {
            let caps = segment.regex.captures(value)?;
            if i == last {
                return Some(Captures {
                    album_metadata,
                    item_metadata: segment.collect(&caps),
                });
            }
            album_metadata.extend(segment.collect(&caps));
        }
        unreachable!("pattern always has a file segment")
    }
}

impl Segment {
    fn parse(template: &str, seen: &mut HashSet<String>) -> Result<Self, String> {
        if template.is_empty() {
            return Err("pattern contains an empty segment (`//`)".into());
        }
        let mut re = String::from("^");
        let mut placeholders = Vec::new();
        let mut rest = template;
        while let Some(open) = rest.find('{') {
            re.push_str(&regex::escape(&rest[..open]));
            let close = rest[open..]
                .find('}')
                .map(|i| open + i)
                .ok_or_else(|| format!("unclosed `{{` in segment {template:?}"))?;
            let name = &rest[open + 1..close];
            let valid = name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !valid {
                return Err(format!(
                    "invalid placeholder name {name:?} in segment {template:?}"
                ));
            }
            if !seen.insert(name.to_string()) {
                return Err(format!("placeholder {{{name}}} is used more than once"));
            }
            re.push_str(&format!("(?P<{name}>[^/]+?)"));
            placeholders.push(name.to_string());
            rest = &rest[close + 1..];
        }
        if rest.contains('}') {
            return Err(format!("unmatched `}}` in segment {template:?}"));
        }
        re.push_str(&regex::escape(rest));
        re.push('$');
        let regex = Regex::new(&re).map_err(|e| e.to_string())?;
        Ok(Self {
            regex,
            placeholders,
        })
    }

    fn collect(&self, caps: &regex::Captures<'_>) -> Metadata {
        self.placeholders
            .iter()
            .map(|name| (name.clone(), caps[name.as_str()].to_string()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn users_example_pattern() {
        let p = Pattern::parse("/{album_name}/{file_name}/").unwrap();
        let m = p.matches(&["Summer", "beach.jpg"]).unwrap();
        assert_eq!(m.album_metadata["album_name"], "Summer");
        assert_eq!(m.item_metadata["file_name"], "beach.jpg");
    }

    #[test]
    fn partitioned_layout_with_literals() {
        let p = Pattern::parse("year={year}/{date}_{album}/{file}").unwrap();
        let m = p
            .matches(&["year=2024", "2024-07-01_Lake Trip", "a.png"])
            .unwrap();
        assert_eq!(m.album_metadata["year"], "2024");
        assert_eq!(m.album_metadata["date"], "2024-07-01");
        assert_eq!(m.album_metadata["album"], "Lake Trip");
        assert!(p.matches(&["2024", "x_y", "a.png"]).is_none());
        assert!(p.matches(&["year=2024", "a.png"]).is_none());
    }

    #[test]
    fn rejects_bad_patterns() {
        assert!(Pattern::parse("{file}").is_err());
        assert!(Pattern::parse("{album}/{album}").is_err());
        assert!(Pattern::parse("{al-bum}/{file}").is_err());
        assert!(Pattern::parse("{album/{file}").is_err());
        assert!(Pattern::parse("{album}//{file}").is_err());
    }
}
