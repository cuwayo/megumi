use megumi::parse_command_text;
use megumi_whatsapp::framework;

#[test]
fn scihub_and_its_aliases_are_registered() {
    for trigger in ["!scihub", "!sci", "!paper", "!doi"] {
        let (name, _) = parse_command_text(trigger, "!").unwrap();
        assert!(
            framework().command_help(name).is_some(),
            "`{name}` is not registered"
        );
    }
}

#[test]
fn scihub_declares_its_reaction() {
    use megumi_whatsapp::commands;
    assert_eq!(
        commands::scihub().into_command().react.as_deref(),
        Some("🔎")
    );
}

// The extract_doi helper is private, so we test it indirectly through the
// command's argument parsing contract: a bare DOI, a doi.org URL, and a
// Sci-Hub URL all start with a recognisable DOI prefix.

#[test]
fn doi_prefixes_are_recognised_as_doi_args() {
    let cases = [
        ("!scihub 10.1038/nature12373", "10.1038/nature12373"),
        (
            "!scihub https://doi.org/10.1038/nature12373",
            "https://doi.org/10.1038/nature12373",
        ),
        (
            "!scihub https://sci-hub.se/10.1038/nature12373",
            "https://sci-hub.se/10.1038/nature12373",
        ),
    ];

    for (input, expected_arg) in cases {
        let (name, args) = parse_command_text(input, "!").unwrap();
        assert_eq!(name, "scihub");
        assert_eq!(args.trim(), expected_arg);
    }
}

#[test]
fn pasted_links_resolve_to_a_bare_doi() {
    use megumi_whatsapp::commands::scihub::extract_doi;

    let cases = [
        ("10.1038/nature12373", "10.1038/nature12373"),
        ("https://doi.org/10.1038/nature12373", "10.1038/nature12373"),
        (
            "http://dx.doi.org/10.1038/nature12373.",
            "10.1038/nature12373",
        ),
        (
            "https://sci-hub.se/10.1038/nature12373",
            "10.1038/nature12373",
        ),
        (
            "https://doi.org/10.1038/nature12373?utm=1",
            "10.1038/nature12373",
        ),
        ("10.1038/nature12373, extra text", "10.1038/nature12373"),
    ];

    for (input, expected) in cases {
        assert_eq!(extract_doi(input).as_deref(), Some(expected), "{input}");
    }

    assert_eq!(extract_doi("machine learning survey"), None);
    assert_eq!(extract_doi("10.12/too-short"), None);
}
