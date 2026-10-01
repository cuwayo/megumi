//! Parsing command arguments out of a text message, the way poise's
//! `prefix_argument` module does.
//!
//! A parameter type implements [`PopArgument`]: it pops itself off the front of
//! the remaining argument string and hands back what it did not consume. The
//! `#[command]` macro generates the code that walks a command's parameters this
//! way, so a command states its inputs as ordinary Rust types instead of
//! reading them out of an [`Args`](crate::Args) bag by hand.
//!
//! The parsing here is synchronous. Poise's trait is async because resolving a
//! Discord mention is a network call; a WhatsApp argument is text, so nothing
//! here needs to wait on anything.

use std::collections::HashMap;

/// The result of [`PopArgument::pop_from`].
///
/// On success, the remainder of the argument string and the parsed value. On
/// failure, the error and, where there was one, the input the parse failed on.
pub type PopArgumentResult<'a, T> =
    Result<(&'a str, T), (Box<dyn std::error::Error + Send + Sync>, Option<String>)>;

/// Parse a value out of a string by popping it off the front.
///
/// Implementors should assume the string never starts with whitespace, and fail
/// to parse if it does. The generated code trims between parameters, so a
/// parameter that accepted leading whitespace would disagree with the rest.
///
/// Similar in spirit to [`std::str::FromStr`], except that it also reports how
/// much of the string it consumed, which is what lets several parameters share
/// one argument string.
pub trait PopArgument<'a>: Sized {
    /// Pops an argument from the front of `args` and parses it as `Self`.
    fn pop_from(args: &'a str) -> PopArgumentResult<'a, Self>;
}

/// Pop a whitespace-separated word from the front of the arguments.
///
/// Quotes group a word that contains spaces, and a backslash escapes the
/// character after it. Leading whitespace is trimmed; trailing whitespace is
/// not consumed, so the next parameter sees exactly the gap the user typed.
///
/// This is poise's `pop_string`, with one difference: an apostrophe quotes the
/// way a double quote does, matching the shell-like splitting [`Args`](crate::Args)
/// already does.
pub fn pop_string(args: &str) -> Result<(&str, String), TooFewArguments> {
    let args = args.trim_start();
    if args.is_empty() {
        return Err(TooFewArguments::default());
    }

    let mut output = String::new();
    let mut quote: Option<char> = None;
    let mut escaping = false;

    let mut chars = args.chars();
    // `.clone().next()` is a poor man's `.peek()`, but a peekable iterator
    // cannot give the remainder back with `as_str`.
    while let Some(c) = chars.clone().next() {
        if escaping {
            output.push(c);
            escaping = false;
        } else if quote.is_none() && c.is_whitespace() {
            break;
        } else if quote == Some(c) {
            quote = None;
        } else if quote.is_none() && (c == '"' || c == '\'') {
            quote = Some(c);
        } else if c == '\\' {
            escaping = true;
        } else {
            output.push(c);
        }

        chars.next();
    }

    Ok((chars.as_str(), output))
}

/// Error thrown if the user passes too many arguments to a command.
#[derive(Default, Debug)]
pub struct TooManyArguments {
    #[doc(hidden)]
    pub __non_exhaustive: (),
}
impl std::fmt::Display for TooManyArguments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Too many arguments were passed")
    }
}
impl std::error::Error for TooManyArguments {}

/// Error thrown if the user passes too few arguments to a command.
#[derive(Default, Debug)]
pub struct TooFewArguments {
    #[doc(hidden)]
    pub __non_exhaustive: (),
}
impl std::fmt::Display for TooFewArguments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Too few arguments were passed")
    }
}
impl std::error::Error for TooFewArguments {}

/// Error thrown when the user enters a string that is not recognized as a boolean.
#[derive(Default, Debug)]
pub struct InvalidBool {
    #[doc(hidden)]
    pub __non_exhaustive: (),
}
impl std::fmt::Display for InvalidBool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Expected a string like `yes` or `no` for the boolean parameter")
    }
}
impl std::error::Error for InvalidBool {}

/// Error thrown when the user types a word that is not one of a choice
/// parameter's options.
///
/// A choice parameter is an enum deriving [`ChoiceParameter`](crate::ChoiceParameter),
/// and the only words it accepts are its variants' names and their `#[name]`
/// aliases.
#[derive(Default, Debug)]
pub struct InvalidChoice {
    #[doc(hidden)]
    pub __non_exhaustive: (),
}
impl std::fmt::Display for InvalidChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("You entered a non-existent choice")
    }
}
impl std::error::Error for InvalidChoice {}

/// Error thrown when parsing a malformed [`CodeBlock`].
#[derive(Default, Debug, Clone)]
pub struct CodeBlockError {
    #[doc(hidden)]
    pub __non_exhaustive: (),
}
impl std::fmt::Display for CodeBlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("couldn't find a valid code block")
    }
}
impl std::error::Error for CodeBlockError {}

/// Parses `args` as `T` via [`FromStr`](std::str::FromStr).
///
/// Types the crate does not implement [`PopArgument`] for can call this from
/// their own implementation, the way poise defers to `ArgumentConvert`.
pub fn pop_from_str<'a, T>(args: &'a str) -> PopArgumentResult<'a, T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let (args, string) = pop_string(args).map_err(|_| (TooFewArguments::default().into(), None))?;
    match string.parse() {
        Ok(value) => Ok((args.trim_start(), value)),
        Err(error) => Err((error.into(), Some(string))),
    }
}

/// A parameter whose value is one of a fixed set of words.
///
/// Derive it on a fieldless enum with [`ChoiceParameter`](megumi_framework_macros::ChoiceParameter)
/// and the enum becomes a command parameter: the user types one of the
/// variants' names, and the framework hands the command the matching variant.
/// This is poise's `ChoiceParameter`, minus the slash-command half — WhatsApp
/// has no option menu, so the choice is a word in the message.
///
/// ```rust
/// use megumi::ChoiceParameter;
///
/// #[derive(ChoiceParameter)]
/// enum Ephemeral {
///     #[name = "off"]
///     Off,
///     #[name = "24h"]
///     Day,
///     #[name = "7d"]
///     #[name = "week"]
///     Week,
/// }
/// ```
///
/// `!group ephemeral 7d` binds `Ephemeral::Week`. `week` does too, because a
/// later `#[name]` is an alias, but only the first name is the one help lists.
/// A variant without `#[name]` is chosen by its own name. Matching ignores
/// ASCII case.
pub trait ChoiceParameter: Sized {
    /// Every choice, in declaration order, as `(name, description)`.
    ///
    /// `name` is the word the user types and the one help shows. `description`
    /// comes from the variant's `#[description = "..."]` and is `None` when the
    /// variant has none.
    fn list() -> Vec<(&'static str, Option<&'static str>)>;

    /// The choice named `name`, matching the canonical name and every alias,
    /// ignoring ASCII case.
    fn from_name(name: &str) -> Option<Self>;

    /// The canonical name of this choice, the one [`list`](Self::list) reports.
    fn name(&self) -> &'static str;
}

impl<'a, T: ChoiceParameter> PopArgument<'a> for T {
    fn pop_from(args: &'a str) -> PopArgumentResult<'a, Self> {
        let (args, string) =
            pop_string(args).map_err(|_| (TooFewArguments::default().into(), None))?;

        match Self::from_name(&string) {
            Some(value) => Ok((args.trim_start(), value)),
            None => Err((InvalidChoice::default().into(), Some(string))),
        }
    }
}

impl<'a> PopArgument<'a> for bool {
    fn pop_from(args: &'a str) -> PopArgumentResult<'a, Self> {
        let (args, string) =
            pop_string(args).map_err(|_| (TooFewArguments::default().into(), None))?;

        let value = match string.to_ascii_lowercase().trim() {
            "yes" | "y" | "true" | "t" | "1" | "enable" | "on" => true,
            "no" | "n" | "false" | "f" | "0" | "disable" | "off" => false,
            _ => return Err((InvalidBool::default().into(), Some(string))),
        };

        Ok((args.trim_start(), value))
    }
}

impl<'a> PopArgument<'a> for String {
    fn pop_from(args: &'a str) -> PopArgumentResult<'a, Self> {
        match pop_string(args) {
            Ok((args, string)) => Ok((args, string)),
            Err(err) => Err((err.into(), Some(args.to_string()))),
        }
    }
}

macro_rules! impl_fromstr_pop_argument {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl<'a> PopArgument<'a> for $ty {
                fn pop_from(args: &'a str) -> PopArgumentResult<'a, Self> {
                    pop_from_str(args)
                }
            }
        )+
    };
}

impl_fromstr_pop_argument! {
    f32, f64,
    u8, u16, u32, u64, u128, usize,
    i8, i16, i32, i64, i128, isize,
    crate::Jid,
}

impl<'a, T: PopArgument<'a>> PopArgument<'a> for Option<T> {
    /// An optional parameter consumes nothing and yields `None` when nothing
    /// remains, rather than failing the command for a missing argument.
    fn pop_from(args: &'a str) -> PopArgumentResult<'a, Self> {
        if args.trim_start().is_empty() {
            return Ok((args, None));
        }
        T::pop_from(args).map(|(args, value)| (args, Some(value)))
    }
}

impl<'a, T: PopArgument<'a>> PopArgument<'a> for Vec<T> {
    /// A variadic parameter consumes every remaining argument of its type.
    ///
    /// It stops, without failing, at the first word it cannot parse, so a
    /// `Vec<u32>` followed by more text keeps that text for the next parameter.
    /// Leading whitespace before the leftover word is trimmed, matching the
    /// scalar parsers.
    fn pop_from(mut args: &'a str) -> PopArgumentResult<'a, Self> {
        let mut values = Vec::new();
        loop {
            let trimmed = args.trim_start();
            if trimmed.is_empty() {
                break;
            }
            match T::pop_from(trimmed) {
                // A success that consumes nothing would never move the loop
                // forward, so it ends the list the way a failure does. `T` is
                // allowed to succeed without consuming (`KeyValueArgs` does),
                // and that must not hang a command.
                Ok((remaining, value)) if remaining.len() < trimmed.len() => {
                    values.push(value);
                    args = remaining;
                }
                _ => break,
            }
        }
        Ok((args, values))
    }
}

/// A command parameter for a fenced or inline code block.
///
/// Single-line: `` `code here` ``. Multiline:
///
/// ````text
/// ```language
/// code here
/// ```
/// ````
///
/// This is poise's `CodeBlock`. `code` mirrors what the chat rendered, and
/// `language` the tag a multiline block was opened with, when it has one.
#[derive(Default, Debug, PartialEq, Eq, Clone, Hash)]
pub struct CodeBlock {
    /// The text inside the code block.
    pub code: String,
    /// In a multiline code block, the language tag, if one was given.
    pub language: Option<String>,
    #[doc(hidden)]
    pub __non_exhaustive: (),
}

impl std::fmt::Display for CodeBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "```{}\n{}\n```",
            self.language.as_deref().unwrap_or(""),
            self.code
        )
    }
}

impl<'a> PopArgument<'a> for CodeBlock {
    fn pop_from(args: &'a str) -> PopArgumentResult<'a, Self> {
        let args = args.trim_start();

        let (rest, mut code_block) = if let Some(block) = args.strip_prefix("```") {
            let end = block.find("```").ok_or_else(|| code_block_error(args))?;
            let rest = &block[(end + 3)..];
            let mut body = &block[..end];

            // A word of [A-Za-z0-9+-._#] sitting between the opening fence and
            // the first newline is the language tag.
            let mut language = None;
            if let Some(newline) = body.find('\n') {
                let tag = &body[..newline];
                let valid = tag
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-._#".contains(c));
                if valid {
                    language = Some(tag);
                    body = &body[(newline + 1)..];
                }
            }

            (
                rest,
                CodeBlock {
                    // Blank lines at either end are padding, not content.
                    code: body.trim_matches('\n').to_owned(),
                    language: language.map(str::to_owned),
                    __non_exhaustive: (),
                },
            )
        } else if let Some(line) = args.strip_prefix('`') {
            let end = line.find('`').ok_or_else(|| code_block_error(args))?;
            (
                &line[(end + 1)..],
                CodeBlock {
                    code: line[..end].to_owned(),
                    language: None,
                    __non_exhaustive: (),
                },
            )
        } else {
            return Err(code_block_error(args));
        };

        // An empty block renders as nothing, so it is not a code block.
        if code_block.code.is_empty() {
            return Err(code_block_error(args));
        }

        // A hair space sometimes rides in at the end of a block for no reason.
        code_block.code = code_block.code.trim_end_matches('\u{200a}').to_owned();
        Ok((rest, code_block))
    }
}

fn code_block_error(args: &str) -> (Box<dyn std::error::Error + Send + Sync>, Option<String>) {
    (CodeBlockError::default().into(), Some(args.to_string()))
}

/// A command parameter for `key=value` arguments.
///
/// For example `key1=value1 key2="value2 with spaces"`. Parsing stops at the
/// first word that is not a pair, so anything after the pairs stays available
/// to the next parameter.
///
/// This is poise's `KeyValueArgs`.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct KeyValueArgs(pub HashMap<String, String>);

impl KeyValueArgs {
    /// Retrieve a single value by its key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// Reads one `key=value` pair from the front of the arguments.
    fn pop_single(args: &str) -> Option<(&str, (String, String))> {
        if args.is_empty() {
            return None;
        }

        let mut key = String::new();
        let mut quote: Option<char> = None;
        let mut escaping = false;

        let mut chars = args.trim_start().chars();
        loop {
            let c = chars.next()?;
            if escaping {
                key.push(c);
                escaping = false;
            } else if quote.is_none() && c.is_whitespace() {
                return None;
            } else if quote == Some(c) {
                quote = None;
            } else if quote.is_none() && (c == '"' || c == '\'') {
                quote = Some(c);
            } else if c == '\\' {
                escaping = true;
            } else if quote.is_none() && c == '=' {
                break;
            } else if quote.is_none() && c.is_ascii_punctuation() {
                // An unquoted key must not contain punctuation, or a code block
                // like `` `0..=5` `` parses as the key "`0.." and the value "5`".
                return None;
            } else {
                key.push(c);
            }
        }

        // `chars` now starts at the value, so pop it the way any word is popped.
        let (rest, value) = pop_string(chars.as_str()).unwrap_or((chars.as_str(), String::new()));
        Some((rest, (key, value)))
    }
}

impl<'a> PopArgument<'a> for KeyValueArgs {
    fn pop_from(mut args: &'a str) -> PopArgumentResult<'a, Self> {
        let mut pairs = HashMap::new();
        while let Some((rest, (key, value))) = Self::pop_single(args) {
            args = rest;
            pairs.insert(key, value);
        }
        Ok((args, Self(pairs)))
    }
}

/// The whole remainder of the argument string, as one value.
///
/// A `#[rest]` parameter takes this instead of a single word, and it is always
/// the command's last parameter: nothing could be parsed after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rest(pub String);

impl std::ops::Deref for Rest {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl<'a> PopArgument<'a> for Rest {
    fn pop_from(args: &'a str) -> PopArgumentResult<'a, Self> {
        Ok(("", Self(args.trim_start().to_string())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pop_string_keeps_trailing_whitespace() {
        assert_eq!(pop_string("AA BB").unwrap().0, " BB");
    }

    #[test]
    fn pop_string_handles_quotes_and_escapes() {
        for &(string, arg) in &[
            (r#"AA BB"#, r#"AA"#),
            (r#""AA BB""#, r#"AA BB"#),
            (r#""AA BB"#, r#"AA BB"#),
            (r#"   AA BB"#, r#"AA"#),
            ("'two words' tail", "two words"),
        ] {
            assert_eq!(pop_string(string).unwrap().1, arg);
        }
    }

    #[derive(Debug, PartialEq)]
    enum Ephemeral {
        Off,
        Day,
        Week,
    }

    impl ChoiceParameter for Ephemeral {
        fn list() -> Vec<(&'static str, Option<&'static str>)> {
            vec![
                ("off", None),
                ("24h", Some("messages vanish after a day")),
                ("7d", Some("messages vanish after a week")),
            ]
        }

        fn from_name(name: &str) -> Option<Self> {
            if name.eq_ignore_ascii_case("off") {
                Some(Self::Off)
            } else if name.eq_ignore_ascii_case("24h") {
                Some(Self::Day)
            } else if name.eq_ignore_ascii_case("7d") || name.eq_ignore_ascii_case("week") {
                Some(Self::Week)
            } else {
                None
            }
        }

        fn name(&self) -> &'static str {
            match self {
                Self::Off => "off",
                Self::Day => "24h",
                Self::Week => "7d",
            }
        }
    }

    #[test]
    fn a_choice_accepts_its_names_and_aliases_ignoring_case() {
        let (rest, choice) = Ephemeral::pop_from("7D please").unwrap();
        assert_eq!(choice, Ephemeral::Week);
        assert_eq!(rest, "please");
        assert_eq!(Ephemeral::pop_from("week").unwrap().1, Ephemeral::Week);

        let error = Ephemeral::pop_from("never").unwrap_err();
        assert_eq!(error.0.to_string(), "You entered a non-existent choice");
        assert_eq!(error.1.as_deref(), Some("never"));
        assert!(Ephemeral::pop_from("").is_err());
    }

    #[test]
    fn bools_accept_yes_no_words() {
        assert!(bool::pop_from("yes please").unwrap().1);
        assert!(!bool::pop_from("off").unwrap().1);
        assert!(bool::pop_from("maybe").is_err());
    }

    #[test]
    fn numbers_and_optional_and_variadic() {
        assert_eq!(i32::pop_from("42 rest").unwrap(), ("rest", 42));
        assert_eq!(Option::<i32>::pop_from("").unwrap().1, None);
        assert_eq!(Option::<i32>::pop_from("7").unwrap().1, Some(7));
        assert_eq!(
            Vec::<i32>::pop_from("1 2 3 x").unwrap(),
            ("x", vec![1, 2, 3])
        );

        // `KeyValueArgs` succeeds without consuming a plain word, so a list of
        // them must stop there rather than loop forever.
        let (rest, pairs) = Vec::<KeyValueArgs>::pop_from("a=1 b=2 plain").unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].get("a"), Some("1"));
        assert_eq!(pairs[0].get("b"), Some("2"));
        assert_eq!(rest.trim_start(), "plain");
    }

    #[test]
    fn code_block_reads_inline_and_fenced() {
        let block = CodeBlock::pop_from("`hello world`").unwrap().1;
        assert_eq!(block.code, "hello world");
        assert_eq!(block.language, None);

        let block = CodeBlock::pop_from("```rust\nhi```").unwrap().1;
        assert_eq!(block.code, "hi");
        assert_eq!(block.language.as_deref(), Some("rust"));

        assert!(CodeBlock::pop_from("``").is_err());
    }

    #[test]
    fn key_value_args_stop_at_plain_words() {
        let (rest, kv) = KeyValueArgs::pop_from(r#"key1=value1 key2="value 2" leftover"#).unwrap();
        assert_eq!(kv.get("key1"), Some("value1"));
        assert_eq!(kv.get("key2"), Some("value 2"));
        assert_eq!(rest.trim_start(), "leftover");
        assert!(KeyValueArgs::pop_from("dummyval").unwrap().1.0.is_empty());
    }

    #[test]
    fn rest_consumes_the_remainder() {
        assert_eq!(Rest::pop_from("  hello world").unwrap().1.0, "hello world");
    }
}
