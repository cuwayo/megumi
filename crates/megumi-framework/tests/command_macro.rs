//! The `#[command]` expansion: typed parameters, subcommands, checks, cooldowns.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use megumi::{
    Args, Check, ChoiceParameter, CommandParameter, Context, Error, Framework, Permission, command,
};

#[command(
    name = "add",
    description = "Adds two numbers",
    help_text = "Both numbers are required.",
    group = "math"
)]
async fn add(
    ctx: Context,
    #[description = "left hand side"] a: i32,
    #[description = "right hand side"] b: i32,
) -> Result<(), Error> {
    let _ = (ctx, a, b);
    Ok(())
}

#[command]
async fn rest_cmd(ctx: Context, #[rest] text: String) -> Result<(), Error> {
    let _ = (ctx, text);
    Ok(())
}

#[command]
async fn flag_cmd(ctx: Context, #[flag] verbose: bool) -> Result<(), Error> {
    let _ = (ctx, verbose);
    Ok(())
}

#[command]
async fn optional_cmd(ctx: Context, maybe: Option<i32>) -> Result<(), Error> {
    let _ = (ctx, maybe);
    Ok(())
}

#[command]
async fn args_cmd(ctx: Context, args: Args, rest: &str) -> Result<(), Error> {
    let _ = (ctx, args, rest);
    Ok(())
}

async fn only_in_groups(ctx: Context) -> Result<bool, Error> {
    Ok(ctx.message.info.source.is_group)
}

#[command(check = only_in_groups, user_cooldown = 5)]
async fn gated(ctx: Context) -> Result<(), Error> {
    let _ = ctx;
    Ok(())
}

#[command(name = "get")]
async fn get(ctx: Context) -> Result<(), Error> {
    let _ = ctx;
    Ok(())
}

/// Writes a value.
#[command(name = "set")]
async fn set(ctx: Context, #[rest] value: String) -> Result<(), Error> {
    let _ = (ctx, value);
    Ok(())
}

#[command(
    name = "config",
    subcommands(get, set),
    subcommand_required,
    group = "admin",
    check = only_in_groups
)]
async fn config(ctx: Context) -> Result<(), Error> {
    let _ = ctx;
    Ok(())
}

#[command(reuse_response, hide_in_help, permission = Permission::Owner)]
async fn status(ctx: Context) -> Result<(), Error> {
    let _ = ctx;
    Ok(())
}

/// Adds two numbers together.
///
/// Both operands are required; there is no default.
#[command]
async fn documented(ctx: Context, a: i32, b: i32) -> Result<(), Error> {
    let _ = (ctx, a, b);
    Ok(())
}

/// The attribute wins over the doc comment.
#[command(description = "Overridden", help_text = "Explicit help")]
async fn documented_override(ctx: Context) -> Result<(), Error> {
    let _ = ctx;
    Ok(())
}

#[command]
async fn lazy_optional(ctx: Context, #[lazy] maybe: Option<i32>, tail: i32) -> Result<(), Error> {
    let _ = (ctx, maybe, tail);
    Ok(())
}

#[command(discard_spare_arguments)]
async fn tolerant(ctx: Context, a: i32) -> Result<(), Error> {
    let _ = (ctx, a);
    Ok(())
}

#[command(guild_only, permission = Permission::GroupAdmin, subcommands(only_admin))]
async fn guarded(ctx: Context) -> Result<(), Error> {
    let _ = ctx;
    Ok(())
}

#[command(name = "only_admin")]
async fn only_admin(ctx: Context) -> Result<(), Error> {
    let _ = ctx;
    Ok(())
}

#[test]
fn typed_parameters_are_advertised() {
    let command = add().into_command();
    assert_eq!(command.name, "add");
    assert_eq!(command.description.as_deref(), Some("Adds two numbers"));
    assert_eq!(
        command.help_text.as_deref(),
        Some("Both numbers are required.")
    );
    assert_eq!(
        command
            .parameters
            .iter()
            .map(|parameter| (
                parameter.name.as_str(),
                parameter.required,
                parameter.rest,
                parameter.flag
            ))
            .collect::<Vec<_>>(),
        vec![("a", true, false, false), ("b", true, false, false)]
    );
    assert_eq!(
        command.parameters[0].description.as_deref(),
        Some("left hand side")
    );
}

#[test]
fn rest_and_flag_and_optional_parameters() {
    let rest = rest_cmd().into_command();
    assert_eq!(names(&rest.parameters), vec![("text", true, true, false)]);

    let flag = flag_cmd().into_command();
    assert_eq!(
        names(&flag.parameters),
        vec![("verbose", false, false, true)]
    );

    let optional = optional_cmd().into_command();
    assert_eq!(
        names(&optional.parameters),
        vec![("maybe", false, false, false)]
    );

    // `Args` plus a bare `&str` is the rest of the message, not a typed parameter.
    let args = args_cmd().into_command();
    assert_eq!(names(&args.parameters), vec![("rest", true, true, false)]);
}

#[test]
fn checks_and_cooldowns_are_stored() {
    let command = gated().into_command();
    assert_eq!(command.checks.len(), 1);
    assert_eq!(command.cooldown_config.user, Some(Duration::from_secs(5)));
}

#[test]
fn subcommands_nest_on_the_parent_and_inherit_checks() {
    let command = config().into_command();
    assert!(command.subcommand_required);
    assert_eq!(command.subcommands.len(), 2);
    assert_eq!(command.subcommands[0].name, "get");
    assert_eq!(command.subcommands[1].name, "set");
    assert_eq!(command.checks.len(), 1);
    assert_eq!(command.subcommands[0].checks.len(), 1);
    assert_eq!(
        command.subcommands[0].group.as_deref(),
        Some("admin"),
        "a child without a group inherits its parent's"
    );
}

#[test]
fn help_lists_subcommands_and_parameters() {
    let framework = Framework::builder()
        .prefix("!")
        .commands([config(), add(), status()])
        .build();

    let parent = framework.command_help("config").unwrap();
    assert!(parent.contains("*Subcommands:*"), "{parent}");
    assert!(parent.contains("`!config get`"), "{parent}");
    assert!(
        parent.contains("`!config set` — Writes a value."),
        "{parent}"
    );
    assert!(
        parent.contains("*Usage:* `!config <subcommand>`"),
        "{parent}"
    );

    let child = framework.command_help("config set").unwrap();
    assert!(
        child.contains("*Usage:* `!config set <value...>`"),
        "{child}"
    );

    let by_child_name = framework.command_help("get").unwrap();
    assert!(by_child_name.contains("!config get"), "{by_child_name}");

    let typed = framework.command_help("add").unwrap();
    assert!(typed.contains("*Parameters:*"), "{typed}");
    assert!(typed.contains("`a` — left hand side"), "{typed}");
    assert!(typed.contains("*Usage:* `!add <a> <b>`"), "{typed}");

    assert!(
        !framework.help_text().contains("status"),
        "hide_in_help omits the command from the listing"
    );
    let hidden = framework.command_help("status").unwrap();
    assert!(hidden.contains("DMs and groups"), "{hidden}");
}

struct Data {
    invocations: AtomicU64,
}

type DataContext = Context<Data>;

#[command(name = "counted", description = "Increments a shared counter")]
async fn counted(ctx: DataContext) -> Result<(), Error> {
    ctx.data().invocations.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn data_check(ctx: DataContext) -> megumi::BoxFuture<Result<bool, Error>> {
    Box::pin(async move {
        let _ = ctx.data();
        Ok(true)
    })
}

#[test]
fn a_typed_context_projects_its_data_type_onto_the_framework() {
    // `setup` runs on the first event, not here, so this asserts only that the
    // builder accepts the async setup and projects `Data` through the command;
    // `events.rs` drives the same framework with a real client to check the data
    // actually reaches the command.
    let framework = Framework::builder()
        .setup(|_client| async move {
            Ok(Data {
                invocations: AtomicU64::new(7),
            })
        })
        .prefix("!")
        .command_check(data_check)
        .commands([counted()])
        .build();

    let help = framework.command_help("counted").unwrap();
    assert!(help.contains("!counted"), "{help}");
}

#[test]
#[should_panic(expected = "setup")]
fn setup_refuses_commands_already_registered() {
    let _ = Framework::builder()
        .commands([add()])
        .setup(|_client| async move {
            Ok(Data {
                invocations: AtomicU64::new(0),
            })
        });
}

#[test]
fn doc_comments_become_description_and_help_text() {
    let command = documented().into_command();
    assert_eq!(
        command.description.as_deref(),
        Some("Adds two numbers together.")
    );
    assert_eq!(
        command.help_text.as_deref(),
        Some("Both operands are required; there is no default.")
    );

    // An explicit attribute still wins over the doc comment.
    let overridden = documented_override().into_command();
    assert_eq!(overridden.description.as_deref(), Some("Overridden"));
    assert_eq!(overridden.help_text.as_deref(), Some("Explicit help"));
}

#[test]
fn lazy_and_discard_spare_arguments_are_accepted() {
    // `#[lazy]` leaves the parameter optional in the listing.
    let lazy = lazy_optional().into_command();
    assert_eq!(
        names(&lazy.parameters),
        vec![("maybe", false, false, false), ("tail", true, false, false)]
    );

    // `discard_spare_arguments` only changes dispatch, not the advertised
    // parameters, so the listing is the same as any one-argument command.
    let tolerant = tolerant().into_command();
    assert_eq!(names(&tolerant.parameters), vec![("a", true, false, false)]);
}

#[test]
fn a_parent_gate_is_inherited_by_its_children() {
    let command = guarded().into_command();
    assert!(command.guild_only);
    assert_eq!(command.permission, Permission::GroupAdmin);

    let child = &command.subcommands[0];
    assert!(child.guild_only, "guild_only is inherited");
    assert_eq!(
        child.permission,
        Permission::GroupAdmin,
        "permission is inherited"
    );
}

fn names(parameters: &[CommandParameter]) -> Vec<(&str, bool, bool, bool)> {
    parameters
        .iter()
        .map(|parameter| {
            (
                parameter.name.as_str(),
                parameter.required,
                parameter.rest,
                parameter.flag,
            )
        })
        .collect()
}

// Keep `Check` in the type namespace so a mistaken expansion that stores a
// future instead of a function pointer fails this crate's tests, not the bot's.
#[allow(dead_code)]
fn _check_type(_: Check) {}

#[derive(megumi::ChoiceParameter, Debug, PartialEq)]
enum Audience {
    #[name = "admins"]
    #[description = "group admins only"]
    Admins,
    #[name = "all"]
    #[name = "everyone"]
    Everyone,
}

#[command]
async fn announce(ctx: Context, audience: Audience, #[rest] note: &str) -> Result<(), Error> {
    let _ = (ctx, audience, note);
    Ok(())
}

#[test]
fn a_choice_parameter_advertises_its_variants() {
    let command = announce().into_command();
    let audience = &command.parameters[0];
    assert_eq!(
        audience.choices,
        vec![
            ("admins".to_string(), Some("group admins only".to_string())),
            ("all".to_string(), None),
        ]
    );

    // An alias selects the variant but is not a choice of its own.
    assert_eq!(
        <Audience as ChoiceParameter>::from_name("everyone"),
        Some(Audience::Everyone)
    );
    assert_eq!(
        <Audience as ChoiceParameter>::from_name("ALL"),
        Some(Audience::Everyone)
    );
    assert_eq!(
        <Audience as ChoiceParameter>::name(&Audience::Admins),
        "admins"
    );

    let help = Framework::builder()
        .prefix("!")
        .commands([announce()])
        .build()
        .command_help("announce")
        .unwrap();
    assert!(
        help.contains("`admins` (group admins only), `all`"),
        "{help}"
    );
}

fn expect_err<T, E>(result: Result<T, E>) -> E {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(error) => error,
    }
}

#[test]
fn a_choice_parameter_parses_its_names_and_rejects_the_rest() {
    // The parser takes raw text, so it needs no context and no WhatsApp session.
    let (audience, rest, _) = __megumi_parse_announce("everyone please").unwrap();
    assert_eq!(audience, Audience::Everyone);
    assert_eq!(rest.trim(), "please");

    // A word that names no variant fails before the command body runs, quoting
    // the word the user typed.
    let error = expect_err(__megumi_parse_announce("nobody"));
    assert_eq!(
        error.to_string(),
        "Could not parse `nobody`: You entered a non-existent choice"
    );

    // Nothing typed at all is a missing argument, not a bad choice.
    let error = expect_err(__megumi_parse_announce(""));
    assert_eq!(
        error.to_string(),
        "Could not parse arguments: Too few arguments were passed"
    );
}
