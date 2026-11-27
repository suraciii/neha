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
    /// The frontmatter is not valid YAML.
    #[error("the frontmatter is not valid YAML: {0}")]
    Yaml(String),
    /// The frontmatter is not a mapping of fields.
    #[error("the frontmatter is a mapping of fields")]
    NotMapping,
    /// A required field is absent.
    #[error("the frontmatter names no `{0}`")]
    Missing(&'static str),
    /// A field is not text.
    #[error("the frontmatter `{0}` is not text")]
    NotText(&'static str),
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
        // Only a marker at column zero closes the block: an indented `---` is
        // content of a YAML block scalar.
        if line == "---" {
            let body = rest.get(end + 1..).unwrap_or("");
            return read(&rest[..offset], body);
        }
        offset = end + 1;
    }
    Err(SkillError::NoFrontmatter)
}

/// Reads the fields of a frontmatter block.
fn read(frontmatter: &str, body: &str) -> Result<SkillDocument, SkillError> {
    let value: serde_norway::Value =
        serde_norway::from_str(frontmatter).map_err(|error| SkillError::Yaml(error.to_string()))?;
    let serde_norway::Value::Mapping(fields) = value else {
        return Err(SkillError::NotMapping);
    };
    let field = |wanted: &'static str| -> Result<Option<String>, SkillError> {
        match fields.get(serde_norway::Value::String(wanted.to_owned())) {
            None | Some(serde_norway::Value::Null) => Ok(None),
            Some(serde_norway::Value::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(SkillError::NotText(wanted)),
        }
    };
    let name = field("name")?.ok_or(SkillError::Missing("name"))?;
    let description = field("description")?.ok_or(SkillError::Missing("description"))?;
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

#[cfg(test)]
mod tests {
    use super::{SkillError, parse};

    #[test]
    fn a_marker_inside_a_literal_scalar_is_content() {
        let document = "---\nname: hello\ndescription: |\n  one\n  ---\n  two\n---\nBody.";
        let parsed = parse(document).unwrap();
        assert_eq!(parsed.description, "one\n---\ntwo");
        assert_eq!(parsed.body, "Body.");
    }

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
        assert!(
            matches!(
                parse("---\nname: [unclosed\n---\n"),
                Err(SkillError::Yaml(_))
            ),
            "broken YAML is reported"
        );
        assert_eq!(parse("---\n- a list\n---\n"), Err(SkillError::NotMapping));
        assert_eq!(
            parse("---\nname: 3\ndescription: d\n---\n"),
            Err(SkillError::NotText("name"))
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
    fn reads_a_yaml_comment_and_escapes() {
        // A comment ends a plain scalar; it is not part of the name.
        let document = "---\nname: weekly-report # the report skill\ndescription: \"Write: the weekly report\"\n---\nbody";
        let parsed = parse(document).unwrap();
        assert_eq!(parsed.name, "weekly-report");
        assert_eq!(parsed.description, "Write: the weekly report");

        // A plain multi-line scalar folds into one line.
        let document =
            "---\nname: folded\ndescription: Write the report\n  from the docs.\n---\nbody";
        assert_eq!(
            parse(document).unwrap().description,
            "Write the report from the docs."
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
