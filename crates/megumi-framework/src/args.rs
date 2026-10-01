//! Positional arguments parsed out of a command message, plus the raw text they
//! came from.

use std::sync::{Arc, OnceLock};

/// The positional arguments of a command, and the raw text they were split from.
///
/// A command that declares typed parameters never reads this bag at all — the
/// generated code parses each parameter straight out of the raw text. A command
/// that takes `args: Args` (or reaches for [`Args::raw`]) gets the shell-like
/// word split on demand: the split is deferred until the first call that needs
/// the individual words, and the words are shared behind an [`Arc`], so cloning
/// an `Args` is a refcount bump rather than a `Vec<String>` copy.
#[derive(Clone, Debug, Default)]
pub struct Args(Arc<ArgsInner>);

#[derive(Debug, Default)]
struct ArgsInner {
    raw: String,
    /// The split words, filled on first use. A [`OnceLock`] rather than a
    /// `Mutex` because the value is only ever written once and reads are
    /// lock-free.
    words: OnceLock<Vec<String>>,
}

impl Args {
    /// Splits `rest` into arguments, keeping `rest` trimmed for [`raw`](Self::raw).
    pub fn from_rest(rest: &str) -> Self {
        Self::from_owned(rest.trim_start().to_string())
    }

    /// Takes an already-owned remainder so dispatch does not copy it a second
    /// time after classifying the message.
    pub(crate) fn from_owned(raw: String) -> Self {
        Self(Arc::new(ArgsInner {
            raw,
            words: OnceLock::new(),
        }))
    }

    /// The argument text exactly as the user typed it, minus the leading
    /// whitespace after the command name.
    pub fn raw(&self) -> &str {
        &self.0.raw
    }

    /// The split words, computed on first call and cached for the rest of the
    /// invocation.
    fn words(&self) -> &[String] {
        self.0.words.get_or_init(|| shell_words_split(&self.0.raw))
    }

    /// How many words the arguments hold.
    pub fn len(&self) -> usize {
        self.words().len()
    }

    /// Whether there are no arguments.
    pub fn is_empty(&self) -> bool {
        self.words().is_empty()
    }

    /// The word at `index`, if there is one.
    pub fn get(&self, index: usize) -> Option<&str> {
        self.words().get(index).map(String::as_str)
    }

    /// Every word in order.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.words().iter().map(String::as_str)
    }

    /// The last word.
    ///
    /// This is not the remainder of the message — use [`raw`](Self::raw) for
    /// that. It is here because a trailing word is often the one that matters.
    pub fn last(&self) -> &str {
        self.words().last().map_or("", String::as_str)
    }

    /// Parses the first word as `T`.
    pub fn parse<T>(&self) -> Option<T>
    where
        T: std::str::FromStr,
    {
        self.words().first()?.parse().ok()
    }

    /// Parses the whole raw argument text as `T`.
    ///
    /// Where [`parse`](Self::parse) reads one word, this reads the entire
    /// remainder, which is what a parameter spanning spaces wants.
    pub fn parse_rest<T>(&self) -> Option<T>
    where
        T: std::str::FromStr,
    {
        self.raw().trim().parse().ok()
    }
}

fn shell_words_split(input: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut quote = None;

    while let Some(ch) = chars.next() {
        match quote {
            Some(quote_char) => {
                if ch == quote_char {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            None => match ch {
                '"' | '\'' => quote = Some(ch),
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                c if c.is_whitespace() => {
                    if !current.is_empty() {
                        values.push(std::mem::take(&mut current));
                    }
                }
                c => current.push(c),
            },
        }
    }

    if !current.is_empty() {
        values.push(current);
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_split_lazily_and_quotes_group() {
        let args = Args::from_rest(r#"  one "two words" three"#);
        assert_eq!(args.raw(), r#"one "two words" three"#);
        assert_eq!(args.len(), 3);
        assert_eq!(args.get(0), Some("one"));
        assert_eq!(args.get(1), Some("two words"));
        assert_eq!(args.last(), "three");
        assert!(!args.is_empty());
    }

    #[test]
    fn empty_args_report_empty() {
        let args = Args::from_rest("   ");
        assert_eq!(args.raw(), "");
        assert!(args.is_empty());
        assert_eq!(args.last(), "");
        assert_eq!(args.parse::<u32>(), None);
    }

    #[test]
    fn parse_reads_one_word_and_parse_rest_the_remainder() {
        let args = Args::from_rest("42 the rest");
        assert_eq!(args.parse::<u32>(), Some(42));
        assert_eq!(args.parse_rest::<String>(), Some("42 the rest".to_string()));
    }

    #[test]
    fn cloning_shares_the_same_words() {
        let args = Args::from_rest("a b c");
        let clone = args.clone();
        assert!(std::ptr::eq(args.words().as_ptr(), clone.words().as_ptr()));
    }
}
