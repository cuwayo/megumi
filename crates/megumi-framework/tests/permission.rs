use megumi::{author_jids, is_admin_participant, participant_matches};
use whatsapp_rust::types::message::MessageSource;
use whatsapp_rust::{GroupParticipant, ParticipantType};

fn participant(
    jid: &str,
    phone_number: Option<&str>,
    lid: Option<&str>,
    participant_type: ParticipantType,
) -> GroupParticipant {
    GroupParticipant {
        jid: jid.parse().unwrap(),
        phone_number: phone_number.map(|value| value.parse().unwrap()),
        lid: lid.map(|value| value.parse().unwrap()),
        username: None,
        participant_type,
        details: None,
    }
}

#[test]
fn matches_participant_across_phone_and_lid_forms() {
    let member = participant(
        "628111@s.whatsapp.net",
        Some("628111@s.whatsapp.net"),
        Some("999@lid"),
        ParticipantType::Member,
    );
    assert!(participant_matches(
        &member,
        &"628111@s.whatsapp.net".parse().unwrap()
    ));
    assert!(participant_matches(&member, &"999@lid".parse().unwrap()));
    assert!(!participant_matches(
        &member,
        &"628222@s.whatsapp.net".parse().unwrap()
    ));
}

#[test]
fn detects_admin_addressed_by_lid() {
    let participants = vec![
        participant(
            "628111@s.whatsapp.net",
            Some("628111@s.whatsapp.net"),
            Some("999@lid"),
            ParticipantType::Admin,
        ),
        participant(
            "628222@s.whatsapp.net",
            Some("628222@s.whatsapp.net"),
            None,
            ParticipantType::Member,
        ),
    ];

    assert!(is_admin_participant(
        &participants,
        &["999@lid".parse().unwrap()]
    ));
    assert!(is_admin_participant(
        &participants,
        &["628111@s.whatsapp.net".parse().unwrap()]
    ));
    assert!(!is_admin_participant(
        &participants,
        &["628222@s.whatsapp.net".parse().unwrap()]
    ));
    assert!(!is_admin_participant(
        &participants,
        &["628333@s.whatsapp.net".parse().unwrap()]
    ));
}

#[test]
fn detects_admin_from_alternate_sender_addressing() {
    let participants = vec![participant(
        "628111@s.whatsapp.net",
        Some("628111@s.whatsapp.net"),
        None,
        ParticipantType::SuperAdmin,
    )];

    assert!(!is_admin_participant(
        &participants,
        &["999@lid".parse().unwrap()]
    ));
    assert!(is_admin_participant(
        &participants,
        &[
            "999@lid".parse().unwrap(),
            "628111@s.whatsapp.net".parse().unwrap()
        ]
    ));
}

fn source(sender: &str, sender_alt: Option<&str>, is_from_me: bool) -> MessageSource {
    MessageSource {
        chat: "120363192628408070@g.us".parse().unwrap(),
        sender: sender.parse().unwrap(),
        sender_alt: sender_alt.map(|value| value.parse().unwrap()),
        is_from_me,
        is_group: true,
        ..Default::default()
    }
}

#[test]
fn own_message_is_checked_against_the_admin_list() {
    const OWN_LID: &str = "226173044961444@lid";
    const OWN_PN: &str = "6281395685501@s.whatsapp.net";
    // The list holds only the LID, while the stanza addressed us by number.
    let participants = vec![participant(
        OWN_LID,
        None,
        None,
        ParticipantType::SuperAdmin,
    )];

    let authors = author_jids(
        &source(OWN_PN, None, true),
        Some(OWN_PN.parse().unwrap()),
        Some(OWN_LID.parse().unwrap()),
    );
    assert!(is_admin_participant(&participants, &authors));
}

#[test]
fn other_members_do_not_inherit_this_accounts_admin_role() {
    const OWN_LID: &str = "226173044961444@lid";
    let participants = vec![
        participant(OWN_LID, None, None, ParticipantType::SuperAdmin),
        participant("999@lid", None, None, ParticipantType::Member),
    ];

    let authors = author_jids(
        &source("999@lid", None, false),
        None,
        Some(OWN_LID.parse().unwrap()),
    );
    assert_eq!(authors.len(), 1);
    assert!(!is_admin_participant(&participants, &authors));
}
