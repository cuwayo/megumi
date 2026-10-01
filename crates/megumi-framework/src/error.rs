//! What the dispatcher hands to the error handler when a command does not run
//! to completion.

use std::time::Duration;

/// The error type every command returns.
///
/// This is poise's canonical `type Error = Box<dyn std::error::Error + Send + Sync>`:
/// a command is written as `-> Result<(), Error>`, and any `?` on a `Result<_, E>`
/// whose error implements [`std::error::Error`] plus `Send` and `Sync` boxes itself.
/// The macro enforces the `Result`, so an error is never swallowed by a command
/// that returns nothing.
pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

/// A failure the framework reports to the error handler.
///
/// Modelled on poise's `FrameworkError`: dispatch rejections are their own
/// variants, and anything a command body returns arrives as
/// [`FrameworkError::Command`].
#[derive(Debug)]
pub enum FrameworkError {
    /// A command returned an error from its own body.
    Command(Error),
    /// A command argument failed to parse from the message.
    ArgumentParse {
        /// The error the parameter type's parser produced.
        error: Error,
        /// The input the parser failed on, when there was one.
        input: Option<String>,
    },
    /// Command was invoked before its cooldown expired.
    CooldownHit {
        /// Time until the command may be invoked again in this context.
        remaining: Duration,
    },
    /// A pre-command check returned `false`, or itself returned an error.
    CommandCheckFailed {
        /// `None` when the check returned `Ok(false)`; `Some` when it errored.
        error: Option<Error>,
    },
    /// Invoked without a subcommand, but the command requires one.
    SubcommandRequired {
        /// The parent command that was typed.
        command: String,
        /// The subcommands that would have been valid.
        subcommands: Vec<String>,
    },
    /// Invoked in a DM, but the command is `guild_only`.
    GuildOnly,
    /// Invoked in a group, but the command is `dm_only`.
    DmOnly,
    /// A non-owner invoked a command gated by `Permission::Owner`.
    NotAnOwner,
    /// The author lacks the permission the command requires.
    MissingPermissions,
    /// The framework could not read the group's admins to check a permission,
    /// so calling the author a non-admin would be a guess.
    PermissionFetchFailed(Error),
    /// The prefix matched, but the name is not a registered command.
    UnknownCommand {
        /// The name that was typed.
        command: String,
    },
    /// A command body panicked.
    ///
    /// The panic is caught so it does not unwind the dispatcher task; the
    /// payload is the panic message, as poise's `CommandPanic` carries it.
    CommandPanic {
        /// The text the panic was raised with.
        payload: String,
    },
}

impl FrameworkError {
    /// Wraps whatever a command returned as [`FrameworkError::Command`].
    ///
    /// Accepts anything that converts into [`Error`], including a value already
    /// boxed as one, so the `#[command]` macro can hand the value straight over.
    pub fn command(error: impl Into<Error>) -> Self {
        Self::Command(error.into())
    }

    /// Wraps a parameter parse failure as [`FrameworkError::ArgumentParse`].
    pub fn argument_parse(error: impl Into<Error>, input: Option<String>) -> Self {
        Self::ArgumentParse {
            error: error.into(),
            input,
        }
    }
}

impl std::fmt::Display for FrameworkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Command(error) => write!(f, "{error}"),
            Self::ArgumentParse { error, input } => match input {
                Some(input) => write!(f, "Could not parse `{input}`: {error}"),
                None => write!(f, "Could not parse arguments: {error}"),
            },
            Self::CooldownHit { remaining } => {
                let secs = remaining.as_secs().max(1);
                write!(
                    f,
                    "You can use this command again in {secs} second{}.",
                    if secs == 1 { "" } else { "s" }
                )
            }
            Self::CommandCheckFailed { error } => match error {
                Some(error) => write!(f, "{error}"),
                None => f.write_str("You cannot use this command."),
            },
            Self::SubcommandRequired {
                command,
                subcommands,
            } => {
                write!(f, "`{command}` needs a subcommand")?;
                if !subcommands.is_empty() {
                    write!(f, ": {}", subcommands.join(", "))?;
                }
                write!(f, ".")
            }
            Self::GuildOnly => f.write_str("This command works only in groups."),
            Self::DmOnly => f.write_str("This command works only in DMs."),
            Self::NotAnOwner => f.write_str("Only the bot owner can use this command."),
            Self::MissingPermissions => {
                f.write_str("You do not have permission to use this command.")
            }
            Self::PermissionFetchFailed(error) => {
                write!(f, "Could not check this group's admins: {error}")
            }
            Self::UnknownCommand { command } => write!(f, "Unknown command `{command}`."),
            Self::CommandPanic { payload } => write!(f, "The command panicked: {payload}"),
        }
    }
}

impl std::error::Error for FrameworkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Command(error)
            | Self::ArgumentParse { error, .. }
            | Self::PermissionFetchFailed(error) => Some(&**error),
            Self::CommandCheckFailed { error: Some(error) } => Some(&**error),
            _ => None,
        }
    }
}

/// What [`Command::invoke`](crate::Command::invoke) and the dispatcher return.
pub type CommandResult = Result<(), FrameworkError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_parse_quotes_the_input() {
        let error = FrameworkError::argument_parse("expected a number", Some("abc".into()));
        assert_eq!(
            error.to_string(),
            "Could not parse `abc`: expected a number"
        );
    }

    #[test]
    fn cooldown_hit_rounds_up_to_a_second() {
        let error = FrameworkError::CooldownHit {
            remaining: Duration::from_millis(200),
        };
        assert_eq!(
            error.to_string(),
            "You can use this command again in 1 second."
        );
    }

    #[test]
    fn subcommand_required_lists_the_children() {
        let error = FrameworkError::SubcommandRequired {
            command: "config".into(),
            subcommands: vec!["get".into(), "set".into()],
        };
        assert_eq!(error.to_string(), "`config` needs a subcommand: get, set.");
    }
}
