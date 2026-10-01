use megumi::ChoiceParameter;
use megumi_whatsapp::commands::group::{Audience, Ephemeral, Toggle, render_info};
use whatsapp_rust::{GroupEphemeralSettings, GroupMetadata, MemberAddMode};

#[test]
fn toggles_accept_the_words_people_type() {
    for word in ["on", "ON", "yes", "true", "enable", "enabled"] {
        assert_eq!(Toggle::from_name(word), Some(Toggle::On), "{word}");
    }
    for word in ["off", "no", "false", "disable", "disabled"] {
        assert_eq!(Toggle::from_name(word), Some(Toggle::Off), "{word}");
    }
    assert_eq!(Toggle::from_name("maybe"), None);
    assert_eq!(Toggle::from_name(""), None);
}

#[test]
fn ephemeral_maps_to_whatsapp_durations() {
    assert_eq!(Ephemeral::from_name("off"), Some(Ephemeral::Off));
    assert_eq!(Ephemeral::from_name("24h"), Some(Ephemeral::Day));
    assert_eq!(Ephemeral::from_name("1d"), Some(Ephemeral::Day));
    assert_eq!(Ephemeral::from_name("7d"), Some(Ephemeral::Week));
    assert_eq!(Ephemeral::from_name("week"), Some(Ephemeral::Week));
    assert_eq!(Ephemeral::from_name("90d"), Some(Ephemeral::Quarter));
    assert_eq!(Ephemeral::from_name("2d"), None);
}

#[test]
fn audience_is_admins_or_everyone() {
    assert_eq!(Audience::from_name("admin"), Some(Audience::Admin));
    assert_eq!(Audience::from_name("admins"), Some(Audience::Admin));
    assert_eq!(Audience::from_name("all"), Some(Audience::All));
    assert_eq!(Audience::from_name("everyone"), Some(Audience::All));
    assert_eq!(Audience::from_name("members"), Some(Audience::All));
    assert_eq!(Audience::from_name("some"), None);
}

#[test]
fn info_lists_the_settings_an_admin_checks_first() {
    let text = render_info(&GroupMetadata {
        subject: Some("Book club".into()),
        description: Some("Bring a book.".into()),
        size: Some(12),
        is_announcement: true,
        is_locked: true,
        membership_approval: true,
        member_add_mode: Some(MemberAddMode::AdminAdd),
        ephemeral: Some(GroupEphemeralSettings {
            expiration: Some(86_400),
            trigger: None,
        }),
        ..Default::default()
    });

    assert!(text.contains("*Book club*"), "{text}");
    assert!(text.contains("Bring a book."), "{text}");
    assert!(text.contains("12 members"), "{text}");
    assert!(text.contains("Messages: admins only"), "{text}");
    assert!(text.contains("Group info: admins only"), "{text}");
    assert!(text.contains("New members: need approval"), "{text}");
    assert!(text.contains("Who can add members: admins"), "{text}");
    assert!(text.contains("Disappearing messages: 24 hours"), "{text}");
}

#[test]
fn info_without_a_name_or_description_is_still_readable() {
    let text = render_info(&GroupMetadata::default());
    assert!(text.contains("*(no name)*"), "{text}");
    assert!(!text.contains("Bring"), "{text}");
    assert!(text.contains("0 members"), "{text}");
    assert!(text.contains("Messages: everyone"), "{text}");
    assert!(text.contains("Disappearing messages: off"), "{text}");
}
