use megumi::{command_text, parse_args, parse_command_text};
use whatsapp_rust::prelude::{MessageBuilderExt, MessageField, wa};

#[test]
fn parses_command_and_arguments() {
    let (command, args) = parse_command_text("!ping hello world", "!").unwrap();
    assert_eq!(command, "ping");
    assert_eq!(args, "hello world");
    assert_eq!(
        parse_args(args).iter().collect::<Vec<_>>(),
        vec!["hello", "world"]
    );
}

#[test]
fn splits_quoted_arguments() {
    let args = parse_args("one \"two words\" 'three words'");
    assert_eq!(
        args.iter().collect::<Vec<_>>(),
        vec!["one", "two words", "three words"]
    );
}

#[test]
fn reads_a_command_from_the_body_or_a_media_caption() {
    assert_eq!(command_text(&wa::Message::text("!ping")), Some("!ping"));

    let captioned = wa::Message {
        image_message: MessageField::some(wa::message::ImageMessage {
            caption: Some("!sticker".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(command_text(&captioned), Some("!sticker"));

    let uncaptioned = wa::Message {
        image_message: MessageField::some(wa::message::ImageMessage::default()),
        ..Default::default()
    };
    assert_eq!(command_text(&uncaptioned), None);
}
