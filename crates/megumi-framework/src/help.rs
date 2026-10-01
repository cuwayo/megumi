//! Rendering the command registry as help text.

use std::sync::Arc;

use itertools::Itertools;

use crate::command::{Command, Registry};
use crate::context::FrameworkData;

/// Every command that is not hidden from help, grouped by its `group` metadata.
pub(crate) fn help_text<U: FrameworkData>(registry: &Registry<U>, prefix: &str) -> String {
    let commands = registry.commands.values();
    let mut commands: Vec<&Arc<Command<U>>> =
        commands.filter(|command| !command.hide_in_help).collect();
    commands.sort_by(|left, right| left.name.cmp(&right.name));
    commands.dedup_by(|left, right| left.name == right.name);

    let mut groups: Vec<&str> = commands
        .iter()
        .map(|command| command.group.as_deref().unwrap_or("general"))
        .collect();
    groups.sort();
    groups.dedup();

    let mut out = String::from("📚 *Available Commands*\n");
    for group in groups {
        let group_commands: Vec<&&Arc<Command<U>>> = commands
            .iter()
            .filter(|command| command.group.as_deref().unwrap_or("general") == group)
            .collect();
        // The group description rides the header line, so a group with nothing to
        // say about itself still costs exactly one line.
        out.push_str(&format!("• *{}*", title_case(group)));
        if let Some(description) = registry.group_descriptions.get(group) {
            out.push_str(" — ");
            out.push_str(description);
        }
        for command in group_commands {
            out.push('\n');
            out.push_str(&format!(
                "`{}{}{}`",
                prefix,
                command.name,
                usage_suffix(command)
            ));
            if let Some(description) = &command.description {
                out.push_str(" — ");
                out.push_str(description);
            }
        }
        out.push('\n');
    }

    out.push_str("\n💡 `");
    out.push_str(prefix);
    out.push_str("help <command>` shows a command's details.");
    out
}

pub(crate) fn command_help<U: FrameworkData>(
    registry: &Registry<U>,
    name: &str,
    prefix: &str,
) -> Option<String> {
    let (command, path) = lookup_command(registry, name)?;
    let mut out = format!("📖 *{}{}*", prefix, path);
    if let Some(description) = &command.description {
        out.push_str(" — ");
        out.push_str(description);
    }
    out.push('\n');

    if let Some(help_text) = &command.help_text {
        out.push('\n');
        out.push_str(help_text);
        out.push('\n');
    }

    out.push_str(&format!(
        "\n*Usage:* `{}{}{}`",
        prefix,
        path,
        usage_suffix(command)
    ));

    let aliases: Vec<&str> = command
        .aliases
        .iter()
        .filter(|alias| alias.as_str() != command.name)
        .map(String::as_str)
        .collect();
    if !aliases.is_empty() {
        out.push_str(&format!(
            "\n*Aliases:* {}",
            aliases.iter().map(|alias| format!("`{alias}`")).join(", ")
        ));
    }

    if let Some(group) = &command.group {
        out.push_str(&format!("\n*Group:* {}", title_case(group)));
    }
    out.push_str(&format!(
        "\n*Available in:* {}",
        match (command.guild_only, command.dm_only) {
            (false, false) => "DMs and groups",
            (true, false) => "groups only",
            (false, true) => "DMs only",
            (true, true) => "nowhere",
        }
    ));

    let subcommands: Vec<&Arc<Command<U>>> = command
        .subcommands
        .iter()
        .filter(|child| !child.hide_in_help)
        .collect();
    if !subcommands.is_empty() {
        out.push_str("\n\n*Subcommands:*");
        for child in subcommands {
            out.push_str(&format!("\n`{prefix}{path} {}`", child.name));
            if let Some(description) = &child.description {
                out.push_str(" — ");
                out.push_str(description);
            }
        }
    }

    if command
        .parameters
        .iter()
        .any(|parameter| parameter.description.is_some() || !parameter.choices.is_empty())
    {
        out.push_str("\n\n*Parameters:*");
        for parameter in &command.parameters {
            out.push_str(&format!("\n`{}`", parameter.name));
            if let Some(description) = &parameter.description {
                out.push_str(&format!(" — {description}"));
            }
            // A choice parameter's value is one of a fixed set of words, so the
            // help lists them rather than leaving the user to guess.
            if !parameter.choices.is_empty() {
                out.push('\n');
                for (index, (name, description)) in parameter.choices.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    out.push('`');
                    out.push_str(name);
                    out.push('`');
                    if let Some(description) = description {
                        out.push_str(&format!(" ({description})"));
                    }
                }
            }
        }
    }

    Some(out)
}

/// Resolves `name`, which may be a top-level alias, a nested `parent child`
/// path, or a subcommand's own name.
fn lookup_command<'a, U: FrameworkData>(
    registry: &'a Registry<U>,
    name: &str,
) -> Option<(&'a Command<U>, String)> {
    let commands = &registry.commands;
    let mut parts = name.split_whitespace();
    let first = parts.next()?;
    if let Some(command) = commands.get(&first.to_ascii_lowercase()).map(Arc::as_ref) {
        let mut path = command.name.clone();
        let mut current = command;
        for part in parts {
            current = current.find_subcommand(part)?;
            path.push(' ');
            path.push_str(&current.name);
        }
        return Some((current, path));
    }

    let mut seen = std::collections::HashSet::new();
    for command in commands.values() {
        if !seen.insert(Arc::as_ptr(command)) {
            continue;
        }
        if let Some(found) = find_in(command, first) {
            return Some(found);
        }
    }
    None
}

fn find_in<'a, U: FrameworkData>(
    command: &'a Command<U>,
    name: &str,
) -> Option<(&'a Command<U>, String)> {
    if let Some(child) = command.find_subcommand(name) {
        return Some((child, format!("{} {}", command.name, child.name)));
    }
    command.subcommands.iter().find_map(|child| {
        find_in(child, name).map(|(found, rest)| (found, format!("{} {rest}", command.name)))
    })
}

fn usage_suffix<U: FrameworkData>(command: &Command<U>) -> String {
    if command.subcommand_required {
        return String::from(" <subcommand>");
    }
    if command.parameters.is_empty() {
        return String::new();
    }
    let mut suffix = String::new();
    for parameter in &command.parameters {
        suffix.push(' ');
        // `rest` is the name the macros give a bare `&str` after `Args`, so it
        // says nothing about the command; the word it stands in for does.
        let name = if parameter.rest && parameter.name == "rest" {
            "text"
        } else {
            parameter.name.as_str()
        };
        let rendered = if parameter.flag {
            format!("[{name}]")
        } else if parameter.rest {
            format!("<{name}...>")
        } else if parameter.required {
            format!("<{name}>")
        } else {
            format!("[{name}]")
        };
        suffix.push_str(&rendered);
    }
    suffix
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    first.to_uppercase().chain(chars).collect()
}
