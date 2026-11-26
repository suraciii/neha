//! Bounded tool output.

use crate::action::ToolOutput;

/// How many bytes of the beginning an output keeps.
pub const HEAD_BYTES: usize = 8 * 1024;
/// How many bytes of the end an output keeps.
pub const TAIL_BYTES: usize = 24 * 1024;

/// Keeps a bounded head and tail, naming the bytes in between.
#[must_use]
pub fn truncate(text: String) -> ToolOutput {
    let bytes = text.as_bytes();
    if bytes.len() <= HEAD_BYTES + TAIL_BYTES {
        return ToolOutput::full(text);
    }
    let head_end = floor_boundary(&text, HEAD_BYTES);
    let tail_start = ceil_boundary(&text, bytes.len() - TAIL_BYTES);
    let omitted = tail_start - head_end;
    let mut output = String::with_capacity(HEAD_BYTES + TAIL_BYTES + 64);
    output.push_str(&text[..head_end]);
    output.push_str(&format!("\n[... {omitted} bytes omitted ...]\n"));
    output.push_str(&text[tail_start..]);

    ToolOutput {
        text: output,
        truncated: true,
        omitted_bytes: omitted as u64,
    }
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::{HEAD_BYTES, TAIL_BYTES, truncate};

    #[test]
    fn a_short_output_is_whole() {
        let output = truncate("hello".to_owned());

        assert_eq!(output.text, "hello");
        assert!(!output.truncated);
    }

    #[test]
    fn a_long_output_keeps_the_head_and_tail() {
        let text = format!("{}{}", "a".repeat(HEAD_BYTES), "z".repeat(TAIL_BYTES + 100));
        let output = truncate(text);

        assert!(output.truncated);
        assert!(output.text.starts_with("aaa"));
        assert!(output.text.ends_with("zzz"));
        assert_eq!(output.omitted_bytes, 100);
    }
}
