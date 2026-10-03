use megumi::parse_command_text;
use megumi_whatsapp::commands;

mod common;
use common::framework;

#[test]
fn kick_is_routed_to_the_registered_command() {
    let (name, args) = parse_command_text("!group kick @member", "!").unwrap();
    assert_eq!(name, "group");
    assert_eq!(args, "kick @member");
    assert!(
        framework().command_help("group kick").is_some(),
        "`group kick` is not registered"
    );
}

#[test]
fn sticker_and_its_alias_are_routed_to_the_registered_command() {
    for trigger in ["!sticker", "!stiker", "!stickerpack", "!stikerpack"] {
        let (name, args) = parse_command_text(trigger, "!").unwrap();
        assert_eq!(args, "");
        assert!(
            framework().command_help(name).is_some(),
            "`{name}` is not registered"
        );
    }
}

#[test]
fn the_media_commands_declare_the_reaction_they_work_under() {
    assert_eq!(
        commands::sticker().into_command().react.as_deref(),
        Some("⌛")
    );
    assert_eq!(
        commands::shazam().into_command().react.as_deref(),
        Some("📥")
    );
}

#[test]
fn download_and_its_aliases_are_routed_to_the_registered_command() {
    for trigger in ["!download", "!dl", "!ytdl"] {
        let (name, args) = parse_command_text(trigger, "!").unwrap();
        assert_eq!(args, "");
        assert!(
            framework().command_help(name).is_some(),
            "`{name}` is not registered"
        );
    }

    let (name, args) = parse_command_text("!dl https://youtu.be/abc", "!").unwrap();
    assert_eq!(name, "dl");
    assert_eq!(args, "https://youtu.be/abc");
    assert_eq!(
        commands::download().into_command().react.as_deref(),
        Some("⌛")
    );
}

#[test]
fn kick_is_an_admin_command_restricted_to_groups() {
    let help = framework().command_help("group kick").unwrap();
    assert!(help.contains("!group kick"), "{help}");
    assert!(help.contains("groups only"), "{help}");
    assert!(help.contains("Admin"), "{help}");
}

#[test]
fn group_is_routed_to_the_registered_command() {
    let (name, args) = parse_command_text("!group subject New name", "!").unwrap();
    assert_eq!(name, "group");
    assert_eq!(args, "subject New name");
    assert!(
        framework().command_help(name).is_some(),
        "`{name}` is not registered"
    );
}

#[test]
fn group_and_its_subcommands_are_registered() {
    let framework = framework();

    let parent = framework.command_help("group").unwrap();
    assert!(parent.contains("groups only"), "{parent}");
    assert!(parent.contains("Admin"), "{parent}");
    for subcommand in ["info", "subject", "promote", "kick", "requests", "news"] {
        assert!(
            parent.contains(subcommand),
            "`{subcommand}` missing from group help: {parent}"
        );
    }

    let child = framework.command_help("group subject").unwrap();
    assert!(child.contains("!group subject"), "{child}");
    assert!(child.contains("groups only"), "{child}");
}

#[test]
fn console_is_reachable_in_any_chat_and_hidden_from_help() {
    let framework = framework();
    let help = framework.command_help("console").unwrap();
    assert!(help.contains("DMs and groups"), "{help}");
    assert!(
        !framework.help_text().contains("console"),
        "the owner console must not be advertised in help"
    );
}

#[test]
fn price_and_its_alias_are_routed_to_the_registered_command() {
    for trigger in ["!price", "!px"] {
        let (name, args) = parse_command_text(trigger, "!").unwrap();
        assert_eq!(args, "");
        assert!(
            framework().command_help(name).is_some(),
            "`{name}` is not registered"
        );
    }

    let (name, args) = parse_command_text("!px GC=F", "!").unwrap();
    assert_eq!(name, "px");
    assert_eq!(args, "GC=F");
    assert_eq!(
        commands::price().into_command().react.as_deref(),
        Some("📈")
    );
}

#[test]
fn group_price_is_a_subcommand_of_group() {
    let parent = framework().command_help("group").unwrap();
    assert!(
        parent.contains("price"),
        "`price` missing from group help: {parent}"
    );
    assert!(
        framework().command_help("group price").is_some(),
        "`group price` is not registered"
    );
}

#[test]
fn uptime_is_registered() {
    let (name, args) = parse_command_text("!uptime", "!").unwrap();
    assert_eq!(name, "uptime");
    assert_eq!(args, "");
    let help = framework().command_help(name).unwrap();
    assert!(help.contains("!uptime"), "{help}");
}
