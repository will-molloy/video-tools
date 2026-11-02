//! Paths in messages, marked so each front end can set them apart from the sentence around them.
//!
//! The TUI shows a marked path in bold, and the CLI in double quotes, since bold doesn't survive
//! a pipe or a log file. Every message goes through [`plain`] or [`runs`] before it's shown.

use std::path::Path;

/// Private-use characters around a path, which no message contains otherwise.
const OPEN: char = '\u{E000}';
const CLOSE: char = '\u{E001}';

/// `path`, marked.
pub fn path(path: &Path) -> String {
    format!("{OPEN}{}{CLOSE}", path.display())
}

/// The last component of `path`, marked, or the whole path if it has none.
pub fn name(path: &Path) -> String {
    self::path(path.file_name().map_or(path, Path::new))
}

/// `text` for the CLI, with paths in double quotes.
pub fn plain(text: &str) -> String {
    text.replace([OPEN, CLOSE], "\"")
}

/// `text` split into its runs, each flagged whether it's a path.
pub fn runs(text: &str) -> impl Iterator<Item = (&str, bool)> {
    text.split([OPEN, CLOSE])
        .enumerate()
        .filter(|(_, run)| !run.is_empty())
        .map(|(i, run)| (run, i % 2 == 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_paths_for_the_cli() {
        let text = format!(
            "{} is in {}",
            name(Path::new("/v/talk 2.srt")),
            path(Path::new("/v"))
        );

        assert_eq!(plain(&text), "\"talk 2.srt\" is in \"/v\"");
    }

    #[test]
    fn splits_paths_from_the_text_around_them() {
        let text = format!("{} has {} lines.", name(Path::new("/v/talk 2.srt")), 12);

        assert_eq!(
            runs(&text).collect::<Vec<_>>(),
            [("talk 2.srt", true), (" has 12 lines.", false)]
        );
    }
}
