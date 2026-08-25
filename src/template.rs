use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug)]
pub struct Template {
    pub overrides: TemplateOverrides,
    pub elements: Vec<Element>,
}

#[derive(Debug, Clone)]
pub enum Element {
    Block(String),
    Image(String),
    Separator,
    Text(String),
}

/// A resolved piece of the report, ready for rendering.
#[derive(Debug, Clone)]
pub enum Segment {
    Text(String),
    Image(Vec<u8>),
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TemplateOverrides {
    pub location: Option<String>,
    pub endpoint: Option<String>,
    pub days: Option<u8>,
}

const FALLBACK: &str = "{{date}}\n";

pub fn load(path: &Path) -> Result<Template> {
    let raw = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::debug!(
                "template `{}` not found, using built-in fallback",
                path.display()
            );
            FALLBACK.to_string()
        }
        Err(e) => {
            return Err(
                anyhow::Error::new(e).context(format!("reading template `{}`", path.display()))
            );
        }
    };
    parse(&raw)
}

fn parse(input: &str) -> Result<Template> {
    let (frontmatter, body) = extract_frontmatter(input)?;

    let overrides: TemplateOverrides = if frontmatter.is_empty() {
        TemplateOverrides::default()
    } else {
        toml::from_str(&frontmatter).context("parsing template overrides")?
    };

    let elements = parse_body(body);

    Ok(Template {
        overrides,
        elements,
    })
}

fn extract_frontmatter(input: &str) -> Result<(String, &str)> {
    let trimmed = input.trim_start();
    if !trimmed.starts_with("+++") {
        return Ok((String::new(), input));
    }

    let after_open = &trimmed[3..];
    let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);

    let Some(end) = after_open.find("\n+++") else {
        bail!("template has an opening `+++` but no closing `+++`");
    };

    let frontmatter = after_open[..end].to_string();
    let body = &after_open[end + 4..];
    let body = body.strip_prefix('\n').unwrap_or(body);

    Ok((frontmatter, body))
}

fn parse_body(body: &str) -> Vec<Element> {
    let mut elements = Vec::new();

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed == "---" {
            elements.push(Element::Separator);
        } else if let Some(element) = parse_block(trimmed) {
            elements.push(element);
        } else {
            elements.push(Element::Text(line.to_string()));
        }
    }

    elements
}

fn parse_block(line: &str) -> Option<Element> {
    let s = line.trim();
    if !s.starts_with("{{") || !s.ends_with("}}") {
        return None;
    }
    let inner = s[2..s.len() - 2].trim();
    if inner.is_empty() {
        return None;
    }

    if let Some(rest) = inner.strip_prefix("image ") {
        let path = rest.trim().trim_matches('"').trim_matches('\'');
        if !path.is_empty() {
            return Some(Element::Image(path.to_string()));
        }
    }

    Some(Element::Block(inner.to_string()))
}

/// Assemble the template into a list of segments ready for rendering.
/// Text blocks are resolved via the callback; images are passed through as-is
/// (fetching happens later so it can be async).
pub fn assemble(elements: &[Element], resolve: &dyn Fn(&str) -> Option<String>) -> Vec<Segment> {
    let mut parts: Vec<RenderedElement> = Vec::new();

    for element in elements {
        match element {
            Element::Block(name) => {
                if let Some(content) = resolve(name) {
                    if !content.trim().is_empty() {
                        parts.push(RenderedElement::Content(content));
                    } else {
                        parts.push(RenderedElement::Empty);
                    }
                } else {
                    parts.push(RenderedElement::Empty);
                }
            }
            Element::Image(path) => {
                parts.push(RenderedElement::Image(path.clone()));
            }
            Element::Separator => {
                parts.push(RenderedElement::Separator);
            }
            Element::Text(text) => {
                parts.push(RenderedElement::Content(text.clone()));
            }
        }
    }

    collapse(parts)
}

/// Flatten assembled segments to plain text (for --stdout without --imageText).
/// Images are represented as a placeholder line.
pub fn segments_to_text(segments: &[Segment]) -> String {
    let mut out = String::new();
    for seg in segments {
        match seg {
            Segment::Text(text) => out.push_str(text),
            Segment::Image(_) => out.push_str("[image]\n"),
        }
    }
    out
}

#[derive(Debug)]
enum RenderedElement {
    Content(String),
    Image(String),
    Separator,
    Empty,
}

fn collapse(parts: Vec<RenderedElement>) -> Vec<Segment> {
    let mut sections: Vec<Vec<RenderedElement>> = Vec::new();
    let mut current: Vec<RenderedElement> = Vec::new();

    for part in parts {
        match part {
            RenderedElement::Separator => {
                if current.iter().any(|p| {
                    matches!(p, RenderedElement::Content(t) if !t.trim().is_empty())
                        || matches!(p, RenderedElement::Image(_))
                }) {
                    sections.push(current);
                }
                current = Vec::new();
            }
            other => current.push(other),
        }
    }
    if current.iter().any(|p| {
        matches!(p, RenderedElement::Content(t) if !t.trim().is_empty())
            || matches!(p, RenderedElement::Image(_))
    }) {
        sections.push(current);
    }

    let separator = "----------------------------------------";
    let mut segments: Vec<Segment> = Vec::new();

    for (i, section) in sections.iter().enumerate() {
        if i > 0 {
            let sep_text = format!("\n\n{separator}\n");
            push_text(&mut segments, &sep_text);
        }
        for part in section {
            match part {
                RenderedElement::Content(text) => {
                    if !segments.is_empty() && matches!(segments.last(), Some(Segment::Text(_))) {
                        push_text(&mut segments, "\n");
                    }
                    push_text(&mut segments, text);
                }
                RenderedElement::Image(path) => {
                    segments.push(Segment::Image(path.clone().into_bytes()));
                }
                RenderedElement::Empty => {}
                RenderedElement::Separator => unreachable!(),
            }
        }
    }

    // Ensure trailing newline on the last text segment.
    if let Some(Segment::Text(t)) = segments.last_mut() {
        if !t.ends_with('\n') {
            t.push('\n');
        }
        *t = t.trim_end_matches('\n').to_string();
        t.push('\n');
    }

    segments
}

fn push_text(segments: &mut Vec<Segment>, text: &str) {
    if let Some(Segment::Text(existing)) = segments.last_mut() {
        existing.push_str(text);
    } else {
        segments.push(Segment::Text(text.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let input = r#"+++
location = "London"
days = 1
+++

{{address}}
{{date}}

{{weather}}
"#;
        let tpl = parse(input).unwrap();
        assert_eq!(tpl.overrides.location.as_deref(), Some("London"));
        assert_eq!(tpl.overrides.days, Some(1));
        assert!(matches!(tpl.elements[0], Element::Text(ref s) if s.is_empty()));
        assert!(matches!(tpl.elements[1], Element::Block(ref s) if s == "address"));
        assert!(matches!(tpl.elements[2], Element::Block(ref s) if s == "date"));
    }

    #[test]
    fn parses_without_frontmatter() {
        let input = "{{weather}}\n---\n{{bins}}\n";
        let tpl = parse(input).unwrap();
        assert!(tpl.overrides.location.is_none());
        assert!(matches!(tpl.elements[0], Element::Block(ref s) if s == "weather"));
        assert!(matches!(tpl.elements[1], Element::Separator));
        assert!(matches!(tpl.elements[2], Element::Block(ref s) if s == "bins"));
    }

    #[test]
    fn parses_image_element() {
        let input = "{{image \"out.png\"}}\n{{image \"https://example.com/img.png\"}}\n";
        let tpl = parse(input).unwrap();
        assert!(matches!(tpl.elements[0], Element::Image(ref s) if s == "out.png"));
        assert!(
            matches!(tpl.elements[1], Element::Image(ref s) if s == "https://example.com/img.png")
        );
    }

    #[test]
    fn assemble_skips_empty_blocks_and_their_separators() {
        let elements = vec![
            Element::Block("address".into()),
            Element::Separator,
            Element::Block("calendars".into()),
            Element::Separator,
            Element::Block("weather".into()),
        ];

        let resolve = |name: &str| -> Option<String> {
            match name {
                "address" => Some("11 Example Street".into()),
                "calendars" => None,
                "weather" => Some("Sunny 26C".into()),
                _ => None,
            }
        };

        let segments = assemble(&elements, &resolve);
        let text = segments_to_text(&segments);
        assert!(text.contains("11 Example Street"));
        assert!(text.contains("Sunny 26C"));
        assert_eq!(
            text.matches("----------------------------------------")
                .count(),
            1
        );
    }

    #[test]
    fn assemble_passes_through_literal_text() {
        let elements = vec![
            Element::Text("Hello world".into()),
            Element::Separator,
            Element::Block("weather".into()),
        ];

        let resolve = |name: &str| -> Option<String> {
            match name {
                "weather" => Some("Sunny".into()),
                _ => None,
            }
        };

        let segments = assemble(&elements, &resolve);
        let text = segments_to_text(&segments);
        assert!(text.starts_with("Hello world"));
        assert!(text.contains("Sunny"));
    }

    #[test]
    fn assemble_includes_image_segments() {
        let elements = vec![
            Element::Block("date".into()),
            Element::Image("test.png".into()),
            Element::Block("weather".into()),
        ];

        let resolve = |name: &str| -> Option<String> {
            match name {
                "date" => Some("Monday".into()),
                "weather" => Some("Sunny".into()),
                _ => None,
            }
        };

        let segments = assemble(&elements, &resolve);
        assert!(segments.len() >= 3);
        assert!(matches!(&segments[1], Segment::Image(_)));
    }
}
