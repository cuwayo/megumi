//! The `#[group]` expansion: a module of commands becomes a `CommandGroup`.

use megumi::{CommandGroup, Context, Error, Framework, NoData, command, group};

#[group(description = "Everyday commands")]
mod utility {
    use super::*;

    /// Replies with pong.
    #[command(name = "ping")]
    pub async fn ping(ctx: Context) -> Result<(), Error> {
        let _ = ctx;
        Ok(())
    }

    /// Repeats what it is given.
    #[command(name = "echo")]
    pub async fn echo(ctx: Context, text: String) -> Result<(), Error> {
        let _ = (ctx, text);
        Ok(())
    }
}

/// A group that names its members explicitly rather than scanning the module.
#[group(name = "admin", description = "Group management", commands(only))]
mod moderation {
    use super::*;

    #[command(name = "only")]
    pub async fn only(ctx: Context) -> Result<(), Error> {
        let _ = ctx;
        Ok(())
    }

    // Not a `#[command]`, so it must not be pulled into the group.
    pub fn helper() -> u32 {
        7
    }
}

mod elsewhere {
    use super::*;

    #[command(name = "far")]
    pub async fn far(ctx: Context) -> Result<(), Error> {
        let _ = ctx;
        Ok(())
    }
}

/// A barrel group: an empty module whose members live elsewhere, so it declares
/// the context type itself.
#[group(
    description = "Commands from another module",
    context = Context,
    commands(elsewhere::far)
)]
mod barrel {}

#[test]
fn a_group_scans_its_module_and_stamps_its_name() {
    let group: CommandGroup<NoData> = utility();
    assert_eq!(group.name, "utility");
    assert_eq!(group.description.as_deref(), Some("Everyday commands"));
    assert_eq!(group.commands.len(), 2);

    let framework = Framework::builder().groups([group]).build();
    let help = framework.help_text();
    // The help header title-cases the group name.
    assert!(help.contains("Utility"), "{help}");
    assert!(help.contains("Everyday commands"), "{help}");
    assert!(help.contains("ping"), "{help}");
    assert!(help.contains("echo"), "{help}");

    // The group name is stamped onto each member that had no group of its own.
    let command = framework.command_help("ping").unwrap();
    assert!(command.contains("ping"), "{command}");
}

#[test]
fn a_group_honours_an_explicit_member_list() {
    let group: CommandGroup<NoData> = moderation();
    assert_eq!(group.name, "admin");
    assert_eq!(group.description.as_deref(), Some("Group management"));
    assert_eq!(
        group
            .commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        vec!["only"],
        "only the named command joins the group"
    );
    assert_eq!(moderation::helper(), 7);
}

#[test]
fn a_barrel_group_reaches_members_in_another_module() {
    let group: CommandGroup<NoData> = barrel();
    assert_eq!(group.name, "barrel");
    assert_eq!(
        group
            .commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        vec!["far"]
    );

    let framework = Framework::builder().groups([group]).build();
    assert!(framework.command_help("far").is_some());
    assert!(framework.help_text().contains("far"));
}
