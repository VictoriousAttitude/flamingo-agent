//! Quoting for a Windows command line so that `CommandLineToArgvW` (and the MSVC CRT)
//! reconstruct the original arguments. Pure string logic, so it is tested on every OS.

use std::ffi::OsString;

/// Quote one argument if it needs it.
pub fn quote_arg(arg: &str) -> String {
    let needs_quotes = arg.is_empty()
        || arg
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\u{0B}' | '"'));
    if !needs_quotes {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            other => {
                out.extend(std::iter::repeat('\\').take(backslashes));
                out.push(other);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat('\\').take(backslashes * 2));
    out.push('"');
    out
}

/// Join arguments into one command-line string.
pub fn quote_command_line(args: &[OsString]) -> String {
    args.iter()
        .map(|a| quote_arg(&a.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_argument_is_untouched() {
        assert_eq!(quote_arg("simple"), "simple");
        assert_eq!(quote_arg(r"C:\dir\file.log"), r"C:\dir\file.log");
    }

    #[test]
    fn empty_argument_becomes_empty_quotes() {
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn spaces_are_quoted() {
        assert_eq!(quote_arg("has space"), "\"has space\"");
    }

    #[test]
    fn embedded_quotes_are_escaped() {
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
    }

    #[test]
    fn trailing_backslashes_before_closing_quote_are_doubled() {
        assert_eq!(quote_arg(r"C:\my dir\"), r#""C:\my dir\\""#);
    }

    #[test]
    fn backslashes_before_a_quote_are_doubled_plus_escape() {
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
    }

    #[test]
    fn command_line_joins_with_single_spaces() {
        let args = [
            OsString::from("--log-dir"),
            OsString::from(r"C:\Program Files\X"),
        ];
        assert_eq!(
            quote_command_line(&args),
            r#"--log-dir "C:\Program Files\X""#
        );
    }
}
