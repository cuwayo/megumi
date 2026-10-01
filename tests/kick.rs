use megumi::Jid;
use megumi_whatsapp::commands::group::collect_targets;
use whatsapp_rust::prelude::MessageBuilderExt;
use whatsapp_rust::prelude::wa;

fn command_message(mentions: &[&str], participant: Option<&str>) -> wa::Message {
    let context = wa::ContextInfo {
        mentioned_jid: mentions.iter().map(|jid| (*jid).to_string()).collect(),
        participant: participant.map(str::to_string),
        ..Default::default()
    };
    wa::Message::text_with_context("!group kick", context)
}

#[test]
fn tagged_members_are_the_targets() {
    let message = command_message(&["628111@s.whatsapp.net", "999@lid"], None);
    let targets = collect_targets(&message, "").unwrap();
    assert_eq!(
        targets.iter().map(Jid::to_string).collect::<Vec<_>>(),
        vec!["628111@s.whatsapp.net", "999@lid"]
    );
}

#[test]
fn reply_uses_the_quoted_sender() {
    let message = command_message(&[], Some("628111@s.whatsapp.net"));
    let targets = collect_targets(&message, "").unwrap();
    assert_eq!(
        targets.iter().map(Jid::to_string).collect::<Vec<_>>(),
        vec!["628111@s.whatsapp.net"]
    );
}

#[test]
fn tags_win_over_the_quoted_sender() {
    let message = command_message(&["628222@s.whatsapp.net"], Some("628111@s.whatsapp.net"));
    let targets = collect_targets(&message, "").unwrap();
    assert_eq!(
        targets.iter().map(Jid::to_string).collect::<Vec<_>>(),
        vec!["628222@s.whatsapp.net"]
    );
}

#[test]
fn typed_numbers_are_targets() {
    let message = command_message(&[], None);
    let targets = collect_targets(&message, "@628111222333 +628444555666").unwrap();
    assert_eq!(
        targets.iter().map(Jid::to_string).collect::<Vec<_>>(),
        vec!["628111222333@s.whatsapp.net", "628444555666@s.whatsapp.net"]
    );
}

#[test]
fn without_a_target_there_is_nothing_to_kick() {
    let message = command_message(&[], None);
    assert!(collect_targets(&message, "").is_none());
}
