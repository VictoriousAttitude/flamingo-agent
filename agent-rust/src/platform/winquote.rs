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

    /// The parser Windows itself uses, reimplemented so the quoting can be checked on any
    /// OS. These are the post-2008 MSVC CRT rules, which `CommandLineToArgvW` also applies
    /// to every argument after the program name:
    ///
    /// * outside quotes, spaces and tabs separate arguments;
    /// * `2n` backslashes followed by `"` produce `n` backslashes and toggle quoting;
    /// * `2n+1` backslashes followed by `"` produce `n` backslashes and a literal `"`;
    /// * backslashes not followed by `"` are literal.
    ///
    /// The CRT's extra "" rule (a doubled quote inside a quoted region yielding one literal
    /// quote) is deliberately **not** implemented: this parser simply toggles twice, which
    /// produces no character. `quote_arg` escapes every embedded quote as `\"`, so it never
    /// emits a bare `""` inside a quoted region and the two readings cannot disagree on any
    /// string this module produces.
    fn parse_command_line(line: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut current = String::new();
        // Tracks whether an argument has been started, so a lone `""` yields an empty
        // argument rather than nothing at all.
        let mut started = false;
        let mut in_quotes = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                ' ' | '\t' if !in_quotes => {
                    if started {
                        args.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                '\\' => {
                    let mut slashes = 1usize;
                    while chars.peek() == Some(&'\\') {
                        chars.next();
                        slashes += 1;
                    }
                    started = true;
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        current.extend(std::iter::repeat_n('\\', slashes / 2));
                        if slashes % 2 == 1 {
                            current.push('"');
                        } else {
                            in_quotes = !in_quotes;
                        }
                    } else {
                        current.extend(std::iter::repeat_n('\\', slashes));
                    }
                }
                '"' => {
                    started = true;
                    in_quotes = !in_quotes;
                }
                other => {
                    started = true;
                    current.push(other);
                }
            }
        }
        if started {
            args.push(current);
        }
        args
    }

    /// Pins the reference parser itself: one case per rule it implements, so a bug in the
    /// oracle cannot quietly excuse a bug in the quoting.
    #[test]
    fn reference_parser_follows_the_crt_rules() {
        // Runs of whitespace separate arguments and are not themselves arguments.
        assert_eq!(parse_command_line("  a \t b  "), ["a", "b"]);
        // Quoting hides the separators.
        assert_eq!(parse_command_line(r#""has space""#), ["has space"]);
        // An empty quoted region is an argument.
        assert_eq!(parse_command_line(r#""""#), [""]);
        // 2n+1 backslashes before a quote: n backslashes and a literal quote.
        assert_eq!(parse_command_line(r#""a\"b""#), [r#"a"b"#]);
        // 2n backslashes before a quote: n backslashes, and the quote toggles.
        assert_eq!(parse_command_line(r#""c:\dir\\""#), [r"c:\dir\"]);
        // Backslashes that are not followed by a quote stay as they are.
        assert_eq!(parse_command_line(r"a\\b"), [r"a\\b"]);
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

        /// Whatever the arguments are, the command line we build must parse back into
        /// exactly them. Generated characters are printable ASCII, which includes the
        /// space, the backslash and the quote - the three the quoting has to reason about.
        #[test]
        fn quoting_round_trips_through_the_reference_parser(
            args in proptest::collection::vec("[ -~]{0,12}", 0..6)
        ) {
            let line = quote_command_line(&args.iter().map(OsString::from).collect::<Vec<_>>());
            proptest::prop_assert_eq!(parse_command_line(&line), args.clone());
            #[cfg(windows)]
            proptest::prop_assert_eq!(parse_with_windows(&line), args);
        }
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

    /// The 20 arguments below exercise every branch of the quoting; they are checked
    /// against Windows itself where that is possible.
    const TRICKY: [&str; 20] = [
        "",
        "simple",
        "has space",
        "  padded  ",
        " ",
        "a b c",
        "\ttab",
        "new\nline",
        r#"say "hi""#,
        "\"",
        "\"\"",
        r#""quoted""#,
        r"C:\dir\file.log",
        r"C:\my dir\",
        r#"a\"b"#,
        r"\\server\share",
        r"\\server\share with space\",
        r"back\\slashes",
        r"trailing\\",
        r#"\"\\\""#,
    ];

    /// Ask Windows to split a command line exactly as a spawned process would.
    #[cfg(windows)]
    fn parse_with_windows(line: &str) -> Vec<String> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

        let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
        let mut count = 0i32;
        // SAFETY: `wide` is NUL-terminated UTF-16 and `count` is a valid out-pointer.
        let argv = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut count) };
        assert!(!argv.is_null(), "CommandLineToArgvW failed for {line:?}");
        let mut args = Vec::with_capacity(count as usize);
        for i in 0..count as isize {
            // SAFETY: the call returned `count` pointers to NUL-terminated strings.
            let arg = unsafe { *argv.offset(i) };
            let mut len = 0isize;
            // SAFETY: each string is NUL-terminated, so the scan stops inside the buffer.
            while unsafe { *arg.offset(len) } != 0 {
                len += 1;
            }
            // SAFETY: `len` is the length of that string, which is still allocated.
            let slice = unsafe { std::slice::from_raw_parts(arg, len as usize) };
            args.push(String::from_utf16_lossy(slice));
        }
        // SAFETY: the buffer came from CommandLineToArgvW, which documents LocalFree.
        unsafe { LocalFree(argv.cast()) };
        args
    }

    /// The real thing: what Windows hands to a child must be what we asked for. The plain
    /// `prog` in front stands in for the program name, which `CommandLineToArgvW` parses
    /// by different (pre-2008) rules that this module never has to produce.
    #[cfg(windows)]
    #[test]
    fn round_trips_through_the_real_parser() {
        for arg in TRICKY {
            let line = format!("prog {}", quote_arg(arg));
            let parsed = parse_with_windows(&line);
            assert_eq!(parsed, ["prog", arg], "command line was {line:?}");
            // The oracle used by the property test agrees with Windows itself.
            assert_eq!(
                parse_command_line(&line),
                parsed,
                "command line was {line:?}"
            );
        }
    }

    /// Everywhere else the same arguments are checked against the reference parser, so the
    /// tricky cases are not Windows-only coverage.
    #[test]
    fn tricky_arguments_round_trip_through_the_reference_parser() {
        for arg in TRICKY {
            let line = format!("prog {}", quote_arg(arg));
            assert_eq!(parse_command_line(&line), ["prog", arg], "line {line:?}");
        }
    }
}
