//! Paths in messages, marked so each front end can set them apart from the sentence around them.
//!
//! The TUI shows a marked path in bold, and the CLI in double quotes, since bold doesn't survive
//! a pipe or a log file. Every message goes through [`plain`] or [`runs`] before it's shown.

use std::path::Path;

/// Private-use characters around a path, which no message contains otherwise.
const OPEN: char = '\u{E000}';
const CLOSE: char = '\u{E001}';

/// `path`, marked.
pub fn quote(path: &Path) -> String {
    format!("{OPEN}{}{CLOSE}", path.display())
}

/// The last component of `path`, marked, or the whole path if it has none.
pub fn quote_name(path: &Path) -> String {
    quote(path.file_name().map_or(path, Path::new))
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
            quote_name(Path::new("/v/talk 2.srt")),
            quote(Path::new("/v"))
        );

        assert_eq!(plain(&text), "\"talk 2.srt\" is in \"/v\"");
    }

    #[test]
    fn splits_paths_from_the_text_around_them() {
        let text = format!(
            "{} has {} lines.",
            quote_name(Path::new("/v/talk 2.srt")),
            12
        );

        assert_eq!(
            runs(&text).collect::<Vec<_>>(),
            [("talk 2.srt", true), (" has 12 lines.", false)]
        );
    }
}
