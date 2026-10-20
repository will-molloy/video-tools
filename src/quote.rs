//! Paths in messages, quoted so names with spaces stand apart from the sentence around them.

use std::path::Path;

/// `path` in double quotes, such as `"talk 2.srt"`.
pub fn quote(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

/// The last component of `path` in double quotes, or the whole path if it has none.
pub fn quote_name(path: &Path) -> String {
    quote(path.file_name().map_or(path, Path::new))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_a_path() {
        assert_eq!(quote(Path::new("/v/talk 2.srt")), "\"/v/talk 2.srt\"");
    }

    #[test]
    fn quotes_a_file_name() {
        assert_eq!(quote_name(Path::new("/v/talk 2.srt")), "\"talk 2.srt\"");
    }
}
