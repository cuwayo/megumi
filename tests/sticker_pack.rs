use megumi::{parse_args, parse_command_text};
use megumi_whatsapp::commands::sticker::telegram;

#[test]
fn parses_telegram_pack_args_with_custom_name() {
    let (_name, args) = parse_command_text(
        "!sticker https://t.me/addstickers/NachoTheCat Nacho Pack",
        "!",
    )
    .unwrap();
    let args = parse_args(args);
    let slug = telegram::extract_pack_name(args.get(0).unwrap()).unwrap();
    assert_eq!(slug, "NachoTheCat");

    let custom_name = args.iter().skip(1).collect::<Vec<_>>().join(" ");
    assert_eq!(custom_name, "Nacho Pack");
}

#[test]
fn auto_split_naming_pattern() {
    let base_name = "Nacho Pack";
    let total_packs = 2;
    let names: Vec<String> = (0..total_packs)
        .map(|i| {
            if total_packs == 1 || i == 0 {
                base_name.to_string()
            } else {
                format!("{base_name} {}", i + 1)
            }
        })
        .collect();

    assert_eq!(names, vec!["Nacho Pack", "Nacho Pack 2"]);
}
