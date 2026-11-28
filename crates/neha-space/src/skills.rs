//! Skills: the Agent Skills document a skill is added from.
//!
//! A skill is one `SKILL.md`: a YAML frontmatter between `---` lines, then
//! the body the Bot reads when it loads the skill. Neha reads the two fields
//! the Agent Skills specification requires, validates the type and bounds of
//! the optional fields it defines, and ignores every other field. Only a field
//! the frontmatter does not name is absent: a field written with a null value
//! is present and is refused like any other wrong type.

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
        "a skill name is 1 to 64 lowercase letters, digits, or hyphens, with no leading, trailing, or doubled hyphen"
    )]
    BadName,
    /// The description is empty or too long.
    #[error("a skill description has 1 to 1024 characters")]
    BadDescription,
    /// The compatibility field is outside its length.
    #[error("a skill compatibility has 1 to 500 characters")]
    BadCompatibility,
    /// The metadata field is not a mapping of text to text.
    #[error("the frontmatter `metadata` is a mapping of text keys to text values")]
    BadMetadata,
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
            // Only an absent key is absent: `license:` with no value is a
            // present field of the wrong type, not a missing one.
            None => Ok(None),
            Some(serde_norway::Value::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(SkillError::NotText(wanted)),
        }
    };
    let name = field("name")?.ok_or(SkillError::Missing("name"))?;
    let description = field("description")?.ok_or(SkillError::Missing("description"))?;
    // The name is a directory name: lowercase, digits, single hyphens between,
    // and never a doubled hyphen.
    if !SkillAdded::valid_name(&name) || name.contains("--") {
        return Err(SkillError::BadName);
    }
    let description = description.trim().to_owned();
    if description.is_empty() || description.chars().count() > 1024 {
        return Err(SkillError::BadDescription);
    }
    // The optional fields the specification defines are read for their type
    // and bounds: a wrong type is not a skill, rather than a field the client
    // reads differently from the package's author.
    let _license = field("license")?;
    if let Some(compatibility) = field("compatibility")?
        && !(1..=500).contains(&compatibility.chars().count())
    {
        return Err(SkillError::BadCompatibility);
    }
    let _allowed_tools = field("allowed-tools")?;
    match fields.get(serde_norway::Value::String("metadata".to_owned())) {
        // Only an absent key is absent: an explicit null is not a mapping.
        None => {}
        Some(serde_norway::Value::Mapping(metadata)) => {
            let text = metadata.iter().all(|(key, value)| {
                matches!(key, serde_norway::Value::String(_))
                    && matches!(value, serde_norway::Value::String(_))
            });
            if !text {
                return Err(SkillError::BadMetadata);
            }
        }
        Some(_) => return Err(SkillError::BadMetadata),
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
        let document = "---\nname: weekly-report\ndescription: >-\n  Write the weekly status report\n  from the docs the User names.\nlicense: MIT\nallowed-tools: read_file shell\nmetadata:\n  author: ada\n  reviewed: \"yes\"\n---\n1. Collect the docs.\n2. Summarize under a page.\n";
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
    fn accepts_the_optional_fields_the_specification_defines() {
        let document = "---\nname: pdf-processing\ndescription: Extract PDF text.\nlicense: Apache-2.0\ncompatibility: Requires git and access to the internet.\nmetadata:\n  author: example-org\n  version: \"1.0\"\nallowed-tools: Bash(git:*) Bash(jq:*) Read\n---\nBody.";
        let parsed = parse(document).unwrap();

        assert_eq!(parsed.name, "pdf-processing");
        assert_eq!(parsed.body, "Body.");
        // The longest allowed compatibility is still a skill.
        let longest = "x".repeat(500);
        let document = format!("---\nname: x\ndescription: d\ncompatibility: {longest}\n---\nbody");
        assert!(parse(&document).is_ok());
    }

    #[test]
    fn rejects_optional_fields_of_the_wrong_type() {
        // The optional fields are validated by type; a list where the
        // specification names a string is not a skill.
        for field in [
            "license: [MIT]",
            "license: 3",
            "compatibility: [linux]",
            "compatibility: 3",
            "allowed-tools: [read_file]",
            "allowed-tools: 3",
            "metadata: [author]",
            "metadata: 3",
            "metadata:\n  author: 3",
            "metadata:\n  3: ada",
        ] {
            let document = format!("---\nname: x\ndescription: d\n{field}\n---\nbody");
            assert!(parse(&document).is_err(), "{field:?} must not be a skill");
        }
        assert_eq!(
            parse("---\nname: x\ndescription: d\nlicense: 3\n---\n"),
            Err(SkillError::NotText("license"))
        );
        assert_eq!(
            parse("---\nname: x\ndescription: d\ncompatibility: [linux]\n---\n"),
            Err(SkillError::NotText("compatibility"))
        );
        assert_eq!(
            parse("---\nname: x\ndescription: d\nallowed-tools: [read_file]\n---\n"),
            Err(SkillError::NotText("allowed-tools"))
        );
        assert_eq!(
            parse("---\nname: x\ndescription: d\nmetadata:\n  author: 3\n---\n"),
            Err(SkillError::BadMetadata)
        );
    }

    #[test]
    fn rejects_an_explicit_null_optional_field() {
        // A field is optional only when the frontmatter does not name it: an
        // explicit YAML null is a present field of the wrong type.
        for (field, error) in [
            ("license:", SkillError::NotText("license")),
            ("license: null", SkillError::NotText("license")),
            ("license: ~", SkillError::NotText("license")),
            ("compatibility:", SkillError::NotText("compatibility")),
            ("compatibility: null", SkillError::NotText("compatibility")),
            ("compatibility: ~", SkillError::NotText("compatibility")),
            ("allowed-tools:", SkillError::NotText("allowed-tools")),
            ("allowed-tools: null", SkillError::NotText("allowed-tools")),
            ("allowed-tools: ~", SkillError::NotText("allowed-tools")),
            ("metadata:", SkillError::BadMetadata),
            ("metadata: null", SkillError::BadMetadata),
            ("metadata: ~", SkillError::BadMetadata),
        ] {
            let document = format!("---\nname: x\ndescription: d\n{field}\n---\nbody");
            assert_eq!(
                parse(&document),
                Err(error),
                "{field:?} is an explicit null, not an absent field"
            );
        }
    }

    #[test]
    fn an_absent_optional_field_is_absent() {
        let parsed = parse("---\nname: x\ndescription: d\n---\nbody").unwrap();

        assert_eq!(parsed.name, "x");
        assert_eq!(parsed.description, "d");
    }

    #[test]
    fn rejects_a_compatibility_outside_its_length() {
        let empty = parse("---\nname: x\ndescription: d\ncompatibility: ''\n---\n");
        assert_eq!(empty, Err(SkillError::BadCompatibility));

        let long = "x".repeat(501);
        let document = format!("---\nname: x\ndescription: d\ncompatibility: {long}\n---\n");
        assert_eq!(parse(&document), Err(SkillError::BadCompatibility));
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
        // A required field is not optional and a null value is not text.
        assert_eq!(
            parse("---\nname:\ndescription: d\n---\n"),
            Err(SkillError::NotText("name"))
        );
        assert_eq!(
            parse("---\nname: x\ndescription: ~\n---\n"),
            Err(SkillError::NotText("description"))
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
        for name in ["-pdf", "pdf-", "pdf--processing", "PDF-processing"] {
            assert_eq!(
                parse(&format!("---\nname: {name}\ndescription: d\n---\n")),
                Err(SkillError::BadName),
                "{name:?} is not a skill name"
            );
        }
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
