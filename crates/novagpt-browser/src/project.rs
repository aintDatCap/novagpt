//! Validated ChatGPT project and conversation URLs.
//!
//! NovaGPT never fetches these URLs itself: they only select the page the
//! dedicated, user-authenticated Chrome opens. Validation is therefore strict
//! and shape-based so a stored or user-supplied value cannot redirect the
//! worker tab to another origin, another account path or an authenticated
//! endpoint with side effects.

const ORIGIN: &str = "https://chatgpt.com";
const PROJECT_PREFIX: &str = "g-p-";
const MAX_SEGMENT: usize = 128;

fn safe_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_SEGMENT
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Accept only `https://chatgpt.com`, no credentials, query or fragment.
fn path_segments(raw: &str) -> Option<Vec<String>> {
    let url = reqwest::Url::parse(raw).ok()?;
    if url.origin().ascii_serialization() != ORIGIN {
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    if url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    let mut segments: Vec<String> = url.path_segments()?.map(str::to_owned).collect();
    if segments.last().is_some_and(|segment| segment.is_empty()) {
        segments.pop();
    }
    if segments.iter().any(String::is_empty) {
        return None;
    }
    Some(segments)
}

/// Normalize a ChatGPT project page (`/g/g-p-<id>/project`).
/// Returns `None` for any other origin, shape or decoration.
pub fn project_url(raw: &str) -> Option<String> {
    let segments = path_segments(raw)?;
    if segments.len() != 3 || segments[0] != "g" || segments[2] != "project" {
        return None;
    }
    let id = segments[1].strip_prefix(PROJECT_PREFIX)?;
    if !safe_segment(id) {
        return None;
    }
    Some(format!("{ORIGIN}/g/{PROJECT_PREFIX}{id}/project"))
}

/// Normalize a ChatGPT conversation (`/c/<id>`).
pub fn conversation_url(raw: &str) -> Option<String> {
    let segments = path_segments(raw)?;
    if segments.len() != 2 || segments[0] != "c" || !safe_segment(&segments[1]) {
        return None;
    }
    Some(format!("{ORIGIN}/c/{}", segments[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_urls_are_normalized_or_rejected() {
        let expected = Some("https://chatgpt.com/g/g-p-abc123/project".to_owned());
        assert_eq!(
            project_url("https://chatgpt.com/g/g-p-abc123/project"),
            expected
        );
        assert_eq!(
            project_url("https://chatgpt.com/g/g-p-abc123/project/"),
            expected
        );
        for rejected in [
            "http://chatgpt.com/g/g-p-abc123/project",
            "https://chatgpt.com.evil.test/g/g-p-abc123/project",
            "https://evil.test/g/g-p-abc123/project",
            "https://user:pass@chatgpt.com/g/g-p-abc123/project",
            "https://chatgpt.com/g/g-p-abc123/project?next=evil",
            "https://chatgpt.com/g/g-p-abc123/project#fragment",
            "https://chatgpt.com/g/g-p-abc123/project/extra",
            "https://chatgpt.com/g/g-p-/project",
            "https://chatgpt.com/g/g-p-abc%2F123/project",
            "https://chatgpt.com/c/abc123",
            "not a url",
        ] {
            assert_eq!(project_url(rejected), None, "rejected: {rejected}");
        }
    }

    #[test]
    fn conversation_urls_are_normalized_or_rejected() {
        let expected = Some("https://chatgpt.com/c/68f0-abc".to_owned());
        assert_eq!(conversation_url("https://chatgpt.com/c/68f0-abc"), expected);
        assert_eq!(
            conversation_url("https://chatgpt.com/c/68f0-abc/"),
            expected
        );
        for rejected in [
            "https://chat.openai.com/c/68f0-abc",
            "https://chatgpt.com/c/68f0-abc?model=x",
            "https://chatgpt.com/c/",
            "https://chatgpt.com/c/a/b",
            "https://chatgpt.com/g/g-p-abc/project",
            "javascript:alert(1)",
        ] {
            assert_eq!(conversation_url(rejected), None, "rejected: {rejected}");
        }
    }
}
