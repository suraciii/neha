//! Skills: the Agent Skills document a skill is added from.
//!
//! A skill is one `SKILL.md`: a YAML frontmatter between `---` lines, then
//! the body the Bot reads when it loads the skill. Neha reads the two fields
//! the Agent Skills specification requires and ignores every other field.

use neha::record::{MAX_SKILL_BODY_BYTES, SkillAdded};

/// What a `SKILL.md` holds, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillDocument {
    /// The `name` field.
    pub name: String,
    /// The `description` field.
    pub description: String,
    /// Everything after the frontmatter.
    pub body: String,
}

/// Why a document is not a skill.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SkillError {
    /// The document does not open with a frontmatter block.
    #[error("a skill document starts with a line `---` and ends its frontmatter with another")]
    NoFrontmatter,
    /// A required field is absent.
    #[error("the frontmatter names no `{0}`")]
    Missing(&'static str),
    /// A frontmatter line is not `key: value`.
    #[error("the frontmatter line {0:?} is not `key: value`")]
    BadLine(String),
    /// The name is not one the specification allows.
    #[error(
        "a skill name is 1 to 64 lowercase letters, digits, or hyphens, and does not begin or end with a hyphen"
    )]
    BadName,
    /// The description is empty or too long.
    #[error("a skill description has 1 to 1024 characters")]
    BadDescription,
    /// The body is too large to carry in the record.
    #[error("a skill body is at most {MAX_SKILL_BODY_BYTES} bytes")]
    BodyTooLarge,
}

/// Parses a `SKILL.md`.
pub fn parse(document: &str) -> Result<SkillDocument, SkillError> {
    let document = document.strip_prefix('\u{feff}').unwrap_or(document);
    let normalized = document.replace("\r\n", "\n");
    let Some(rest) = normalized.strip_prefix("---") else {
        return Err(SkillError::NoFrontmatter);
    };
    // The opening marker is a line of its own.
    let rest = rest.strip_prefix('\n').ok_or(SkillError::NoFrontmatter)?;
    let mut offset = 0;
    for line in rest.split('\n') {
        let end = offset + line.len();
        if line.trim() == "---" {
            let body = rest.get(end + 1..).unwrap_or("");
            return read(&rest[..offset], body);
        }
        offset = end + 1;
    }
    Err(SkillError::NoFrontmatter)
}

/// Reads the fields of a frontmatter block.
fn read(frontmatter: &str, body: &str) -> Result<SkillDocument, SkillError> {
    let lines: Vec<&str> = frontmatter.split('\n').collect();
    let mut fields: Vec<(&str, String)> = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim();
        index += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(SkillError::BadLine(line.to_owned()));
        };
        let key = key.trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        {
            return Err(SkillError::BadLine(line.to_owned()));
        }
        let value = value.trim();
        let text = if value.starts_with(['|', '>']) {
            let consumed = block(lines.get(index..).unwrap_or_default(), value);
            index += consumed.1;
            consumed.0
        } else if value.is_empty() {
            // A nested list or map Neha does not read: skip the lines that
            // belong to it.
            index += nested(lines.get(index..).unwrap_or_default());
            String::new()
        } else {
            unquote(value)
        };
        fields.push((key, text));
    }
    let field = |wanted: &str| {
        fields
            .iter()
            .find(|(key, _)| *key == wanted)
            .map(|(_, value)| value.clone())
    };
    let name = field("name").ok_or(SkillError::Missing("name"))?;
    let description = field("description").ok_or(SkillError::Missing("description"))?;
    if !SkillAdded::valid_name(&name) {
        return Err(SkillError::BadName);
    }
    let description = description.trim().to_owned();
    if description.is_empty() || description.chars().count() > 1024 {
        return Err(SkillError::BadDescription);
    }
    if body.len() > MAX_SKILL_BODY_BYTES {
        return Err(SkillError::BodyTooLarge);
    }
    Ok(SkillDocument {
        name,
        description,
        body: body.to_owned(),
    })
}

/// Reads a block scalar: the indented lines that follow its key. Returns the
/// text and how many lines it took.
fn block(lines: &[&str], marker: &str) -> (String, usize) {
    let fold = marker.starts_with('>');
    let chomp = marker.ends_with('-');
    let mut parts: Vec<String> = Vec::new();
    let mut blanks = 0;
    let mut taken = 0;
    for line in lines {
        if line.trim().is_empty() {
            blanks += 1;
            taken += 1;
            continue;
        }
        if line.starts_with([' ', '\t']) {
            parts.extend((0..blanks).map(|_| String::new()));
            blanks = 0;
            parts.push(line.trim_start().to_owned());
            taken += 1;
            continue;
        }
        break;
    }
    let mut text = if fold {
        let mut folded = String::new();
        for (position, part) in parts.iter().enumerate() {
            if position > 0 {
                folded.push(if part.is_empty() { '\n' } else { ' ' });
            }
            folded.push_str(part);
        }
        folded
    } else {
        parts.join("\n")
    };
    if !chomp {
        text.push('\n');
    }
    (text.trim().to_owned(), taken)
}

/// Counts the lines of a nested list or map: indented lines, or list items.
fn nested(lines: &[&str]) -> usize {
    lines
        .iter()
        .take_while(|line| line.starts_with([' ', '\t']) || line.trim_start().starts_with('-'))
        .count()
}

/// Removes the quotes around a scalar, when it has them.
fn unquote(value: &str) -> String {
    for quote in ['\'', '"'] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            let escaped = format!("{quote}{quote}");
            return inner.replace(&escaped, &quote.to_string());
        }
    }
    value.to_owned()
}

#[cfg(test)]
mod tests {
    use super::{SkillError, parse};

    #[test]
    fn reads_the_two_required_fields_and_ignores_the_rest() {
        let document = "---\nname: weekly-report\ndescription: >-\n  Write the weekly status report\n  from the docs the User names.\nlicense: MIT\nallowed-tools: [read_file, shell]\nmetadata:\n  author: ada\n  reviewed: yes\n---\n1. Collect the docs.\n2. Summarize under a page.\n";
        let parsed = parse(document).unwrap();
        assert_eq!(parsed.name, "weekly-report");
        assert_eq!(
            parsed.description,
            "Write the weekly status report from the docs the User names."
        );
        assert_eq!(
            parsed.body,
            "1. Collect the docs.\n2. Summarize under a page.\n"
        );
    }

    #[test]
    fn keeps_a_literal_description_and_strips_quotes() {
        let document = "---\nname: 'quoted-name'\ndescription: |\n  line one\n\n  line three\nother: \"kept: colon\"\n---\nbody";
        let parsed = parse(document).unwrap();
        assert_eq!(parsed.name, "quoted-name");
        assert_eq!(parsed.description, "line one\n\nline three");
        assert_eq!(parsed.body, "body");
    }

    #[test]
    fn rejects_what_is_not_a_skill() {
        assert_eq!(parse("no frontmatter"), Err(SkillError::NoFrontmatter));
        assert_eq!(parse("---\nname: x\n"), Err(SkillError::NoFrontmatter));
        assert_eq!(
            parse("---\ndescription: d\n---\n"),
            Err(SkillError::Missing("name"))
        );
        assert_eq!(
            parse("---\nname: x\n---\n"),
            Err(SkillError::Missing("description"))
        );
        assert_eq!(
            parse("---\nname x\n---\n"),
            Err(SkillError::BadLine("name x".to_owned()))
        );
        assert_eq!(
            parse("---\nname: Bad Name\ndescription: d\n---\n"),
            Err(SkillError::BadName)
        );
        assert_eq!(
            parse("---\nname: x\ndescription: ''\n---\n"),
            Err(SkillError::BadDescription)
        );
        let long = "x".repeat(1025);
        assert_eq!(
            parse(&format!("---\nname: x\ndescription: {long}\n---\n")),
            Err(SkillError::BadDescription)
        );
        let big = "y".repeat(super::MAX_SKILL_BODY_BYTES + 1);
        assert_eq!(
            parse(&format!("---\nname: x\ndescription: d\n---\n{big}")),
            Err(SkillError::BodyTooLarge)
        );
    }

    #[test]
    fn accepts_windows_line_endings_and_a_bom() {
        let document = "\u{feff}---\r\nname: bom\r\ndescription: fine\r\n---\r\nbody\r\n";
        assert_eq!(parse(document).unwrap().body, "body\n");
    }

    #[test]
    fn a_marker_on_its_own_first_line_leaves_an_empty_body() {
        let parsed = parse("---\nname: x\ndescription: d\n---").unwrap();
        assert_eq!(parsed.body, "");
    }
}
