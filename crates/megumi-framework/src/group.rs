//! Optional explicit container for grouping commands.

use crate::command::{Command, IntoCommand, IntoCommands};
use crate::context::{FrameworkData, NoData};

/// A named set of commands, so a module of related commands is declared in one
/// place instead of repeating `group = "..."` on each.
///
/// Registering a group through [`FrameworkBuilder::groups`](crate::FrameworkBuilder::groups)
/// stamps the group's name onto every member that declares no group of its own,
/// and keeps the group's description for the help listing. The `#[group]` macro
/// builds one of these from a module of `#[command]` functions.
pub struct CommandGroup<U: FrameworkData = NoData> {
    /// The group name commands are listed under, and the default `group` of its
    /// members.
    pub name: String,
    /// A one-line description rendered next to the group's header in help.
    pub description: Option<String>,
    /// The commands that belong to the group.
    pub commands: Vec<Command<U>>,
}

impl<U: FrameworkData> CommandGroup<U> {
    /// A group named `name`, with no description and no commands yet.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            commands: Vec::new(),
        }
    }

    /// Sets the description shown next to the group's header in help.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Adds every command the value converts into.
    pub fn commands(mut self, commands: impl IntoCommands<U>) -> Self {
        self.commands.extend(commands.into_commands());
        self
    }

    /// Adds one command.
    pub fn command(mut self, command: impl IntoCommand<U>) -> Self {
        self.commands.push(command.into_command());
        self
    }

    /// Merges `other`'s commands into this group.
    ///
    /// The group's own name and description are kept; only the members move.
    pub fn add_group(mut self, other: CommandGroup<U>) -> Self {
        self.commands.extend(other.commands);
        self
    }

    /// Splits the group into the parts
    /// [`FrameworkBuilder::add_group`](crate::FrameworkBuilder::add_group) needs.
    pub(crate) fn into_parts(self) -> (String, Option<String>, Vec<Command<U>>) {
        (self.name, self.description, self.commands)
    }
}
