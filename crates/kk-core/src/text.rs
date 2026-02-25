/// Split text into chunks that fit within `max_len` bytes.
///
/// Split strategy (per protocol spec):
/// 1. Prefer splitting at last newline before limit
/// 2. Fall back to last space before limit
/// 3. Hard cut if no good break point in first half
pub fn split_text(text: &str, max_len: usize) -> Vec<String> {
    if max_len == 0 {
        return Vec::new();
    }
    if text.len() <= max_len {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }

        let half = max_len / 2;

        // Try to split at last newline before max_len
        let split_at = if let Some(pos) = remaining[..max_len].rfind('\n') {
            if pos >= half {
                pos
            } else {
                find_space_or_hard(remaining, max_len, half)
            }
        } else {
            find_space_or_hard(remaining, max_len, half)
        };

        chunks.push(remaining[..split_at].to_string());
        remaining = remaining[split_at..].trim_start();
    }

    chunks
}

fn find_space_or_hard(text: &str, max_len: usize, half: usize) -> usize {
    if let Some(pos) = text[..max_len].rfind(' ') {
        if pos >= half { pos } else { max_len }
    } else {
        max_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string() {
        assert_eq!(split_text("", 100), vec![""]);
    }

    #[test]
    fn zero_max_len() {
        assert!(split_text("hello", 0).is_empty());
    }

    #[test]
    fn under_limit() {
        assert_eq!(split_text("hello world", 100), vec!["hello world"]);
    }

    #[test]
    fn exact_limit() {
        assert_eq!(split_text("hello", 5), vec!["hello"]);
    }

    #[test]
    fn split_at_newline() {
        let text = "line one\nline two\nline three";
        let chunks = split_text(text, 18);
        assert_eq!(chunks, vec!["line one\nline two", "line three"]);
    }

    #[test]
    fn split_at_space() {
        let text = "hello world foo bar baz";
        let chunks = split_text(text, 15);
        // Last space within [..15] is at position 11 (>= half=7), split there
        assert_eq!(chunks, vec!["hello world", "foo bar baz"]);
    }

    #[test]
    fn hard_cut_no_breaks() {
        let text = "abcdefghijklmnop";
        let chunks = split_text(text, 5);
        assert_eq!(chunks, vec!["abcde", "fghij", "klmno", "p"]);
    }

    #[test]
    fn prefers_newline_over_space() {
        let text = "aaa bbb\nccc ddd";
        let chunks = split_text(text, 10);
        // Should split at newline (pos 7) not space (pos 3)
        assert_eq!(chunks, vec!["aaa bbb", "ccc ddd"]);
    }

    #[test]
    fn newline_too_early_falls_to_space() {
        // Newline at position 1 is less than half of 10 = 5
        let text = "a\nbcdef ghijk";
        let chunks = split_text(text, 10);
        // Newline at pos 1 < half(5), so fall to space at pos 7 >= 5
        assert_eq!(chunks, vec!["a\nbcdef", "ghijk"]);
    }

    #[test]
    fn trims_leading_whitespace_on_next_chunk() {
        let text = "hello world";
        let chunks = split_text(text, 6);
        // Split at space (pos 5), then trim leading space from "world"
        assert_eq!(chunks, vec!["hello", "world"]);
    }

    #[test]
    fn telegram_limit() {
        let text = "x".repeat(8192);
        let chunks = split_text(&text, 4096);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 4096);
        assert_eq!(chunks[1].len(), 4096);
    }
}
