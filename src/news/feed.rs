//! The morning digest: a few headlines pulled from RSS.
//!
//! The feeds are the ones still publishing a plain RSS document in 2026. Reuters
//! and AP both retired theirs, so the digest is BBC top stories, BBC World, and
//! The Guardian's world desk — broad coverage, no API key, and a stable shape.

use chrono::{DateTime, NaiveDate};

/// How many headlines one digest carries. Past this the message is a wall of links.
const HEADLINE_LIMIT: usize = 8;

/// One story, reduced to the two things a chat message can usefully show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Headline {
    /// The story's title, with whitespace collapsed.
    pub title: String,
    /// Where it lives, when the feed gave one.
    pub link: Option<String>,
    /// When it was published, when the feed's date parsed.
    pub published: Option<DateTime<chrono::FixedOffset>>,
}

/// A source the digest reads, and the failure to keep if it doesn't answer.
struct Feed {
    name: &'static str,
    url: &'static str,
}

/// The feeds, in the order their stories are preferred.
const FEEDS: &[Feed] = &[
    Feed {
        name: "BBC News",
        url: "https://feeds.bbci.co.uk/news/rss.xml",
    },
    Feed {
        name: "BBC World",
        url: "https://feeds.bbci.co.uk/news/world/rss.xml",
    },
    Feed {
        name: "The Guardian",
        url: "https://www.theguardian.com/world/rss",
    },
];

/// Fetches every feed and renders one digest.
///
/// A feed that fails is dropped rather than failing the whole morning: one
/// unreachable source should not silence the others. The error is returned only
/// when nothing came back at all, so the caller can leave the morning unsent and
/// try again later instead of delivering an empty digest.
pub async fn fetch_digest(client: &reqwest::Client) -> Result<String, String> {
    let mut headlines = Vec::new();
    let mut failures = Vec::new();

    for feed in FEEDS {
        match fetch_feed(client, feed.url).await {
            Ok(mut parsed) => headlines.append(&mut parsed),
            Err(error) => failures.push(format!("{}: {error}", feed.name)),
        }
    }

    let digest = render(&select(headlines));
    if digest.is_empty() {
        return Err(if failures.is_empty() {
            "every feed returned no headlines".to_string()
        } else {
            failures.join("; ")
        });
    }
    Ok(digest)
}

async fn fetch_feed(client: &reqwest::Client, url: &str) -> Result<Vec<Headline>, String> {
    let response = client
        .get(url)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|error| format!("request failed: {error}"))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }

    let body = response
        .text()
        .await
        .map_err(|error| format!("reading the feed failed: {error}"))?;
    Ok(parse_rss(&body))
}

/// The headlines worth sending: newest first, one per story, capped.
///
/// Stories are deduplicated by link, or by title when a feed gave none, because
/// BBC's top stories and its world desk routinely carry the same piece.
fn select(mut headlines: Vec<Headline>) -> Vec<Headline> {
    headlines.sort_by_key(|headline| std::cmp::Reverse(headline.published));

    let mut seen = std::collections::HashSet::new();
    headlines.retain(|headline| {
        let key = headline
            .link
            .clone()
            .unwrap_or_else(|| headline.title.to_lowercase());
        seen.insert(key)
    });
    headlines.truncate(HEADLINE_LIMIT);
    headlines
}

/// Renders the headlines as one message. An empty list renders as nothing, which
/// is how [`fetch_digest`] tells a dead morning from a live one.
pub fn render(headlines: &[Headline]) -> String {
    if headlines.is_empty() {
        return String::new();
    }

    let mut lines = vec![format!("*Morning news* — {}", today())];
    for headline in headlines {
        let mut line = format!("\n• {}", headline.title);
        if let Some(link) = &headline.link {
            line.push('\n');
            line.push_str(link);
        }
        lines.push(line);
    }
    lines.join("\n")
}

fn today() -> NaiveDate {
    chrono::Local::now().date_naive()
}

/// Pulls every `<item>` out of an RSS document.
///
/// This is a small scanner rather than a parser: the feeds above are RSS 2.0
/// with no namespaces on the elements read here, and a real XML stack would be a
/// dependency for three tags. Anything that isn't an item is ignored, and an item
/// without a title is skipped.
pub fn parse_rss(xml: &str) -> Vec<Headline> {
    let mut headlines = Vec::new();
    let mut rest = xml;

    while let Some(start) = find_tag_end(rest, "item") {
        rest = &rest[start..];
        let Some(end) = find_close(rest, "item") else {
            break;
        };
        let body = &rest[..end];
        rest = &rest[end..];

        let Some(title) = element_text(body, "title") else {
            continue;
        };
        let title = decode_entities(&collapse_whitespace(title));
        if title.is_empty() {
            continue;
        }

        headlines.push(Headline {
            title,
            link: element_text(body, "link").and_then(|link| {
                let link = decode_entities(link.trim());
                (!link.is_empty()).then_some(link)
            }),
            published: element_text(body, "pubDate").and_then(|date| parse_rfc2822(date.trim())),
        });
    }

    headlines
}

/// The index just past the end of the first `<name ...>` tag, if there is one.
fn find_tag_end(xml: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(open) = xml[from..].find('<') {
        let open = from + open;
        let after = &xml[open + 1..];
        if tag_named(after, name) {
            return xml[open..].find('>').map(|end| open + end + 1);
        }
        from = open + 1;
    }
    None
}

/// The index of the first `</name>` whose name matches exactly.
fn find_close(xml: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    let needle = format!("</{name}");
    while let Some(found) = xml[from..].find(&needle) {
        let at = from + found;
        let after = xml[at + needle.len()..].chars().next();
        if matches!(after, Some('>' | ' ' | '\t' | '\n' | '\r')) {
            return Some(at);
        }
        from = at + needle.len();
    }
    None
}

/// Whether `xml`, positioned just after a `<`, opens an element named `name`.
fn tag_named(after_bracket: &str, name: &str) -> bool {
    let Some(rest) = after_bracket.strip_prefix(name) else {
        return false;
    };
    matches!(
        rest.chars().next(),
        Some('>' | ' ' | '\t' | '\n' | '\r' | '/')
    )
}

/// The text inside the first `<name>...</name>`, decoded of CDATA but not entities.
fn element_text<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let start = find_tag_end(xml, name)?;
    let end = find_close(&xml[start..], name)?;
    let inner = xml[start..start + end].trim();
    Some(strip_cdata(inner))
}

fn strip_cdata(text: &str) -> &str {
    text.strip_prefix("<![CDATA[")
        .and_then(|text| text.strip_suffix("]]>"))
        .unwrap_or(text)
}

/// The date formats RSS actually uses. RFC 2822 is the spec; the numeric form
/// shows up often enough to be worth accepting.
fn parse_rfc2822(text: &str) -> Option<DateTime<chrono::FixedOffset>> {
    DateTime::parse_from_rfc2822(text)
        .ok()
        .or_else(|| DateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f%z").ok())
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The entities that appear in headlines. Numeric references are decoded too.
fn decode_entities(text: &str) -> String {
    let mut decoded = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(amp) = rest.find('&') {
        decoded.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(end) = rest.find(';') else {
            break;
        };
        let entity = &rest[1..end];
        if let Some(ch) = named_entity(entity).or_else(|| numeric_entity(entity)) {
            decoded.push(ch);
            rest = &rest[end + 1..];
        } else {
            decoded.push('&');
            rest = &rest[1..];
        }
    }
    decoded.push_str(rest);
    decoded
}

fn named_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        _ => return None,
    })
}

fn numeric_entity(entity: &str) -> Option<char> {
    let (digits, radix) = entity.strip_prefix('#').map(|digits| {
        digits
            .strip_prefix(['x', 'X'])
            .map_or((digits, 10), |hex| (hex, 16))
    })?;
    char::from_u32(u32::from_str_radix(digits, radix).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
        <rss version="2.0"><channel>
            <title>Example</title>
            <item>
                <title>First &amp; foremost</title>
                <link>https://example.com/first</link>
                <pubDate>Mon, 05 Oct 2026 06:00:00 GMT</pubDate>
            </item>
            <item>
                <title><![CDATA[A quoted "story"]]></title>
                <link>https://example.com/second</link>
                <pubDate>not a date</pubDate>
            </item>
            <item>
                <title>   </title>
                <link>https://example.com/blank</link>
            </item>
            <item>
                <description>No title, so it does not count.</description>
            </item>
        </channel></rss>"#;

    #[test]
    fn parsing_keeps_titled_items_and_decodes_them() {
        let headlines = parse_rss(SAMPLE);

        assert_eq!(headlines.len(), 2);
        assert_eq!(headlines[0].title, "First & foremost");
        assert_eq!(
            headlines[0].link.as_deref(),
            Some("https://example.com/first")
        );
        assert!(headlines[0].published.is_some());
        assert_eq!(headlines[1].title, "A quoted \"story\"");
        assert!(headlines[1].published.is_none());
    }

    #[test]
    fn selection_orders_by_recency_and_drops_duplicates() {
        let older = Headline {
            title: "Older".into(),
            link: Some("https://example.com/old".into()),
            published: parse_rfc2822("Mon, 05 Oct 2026 01:00:00 GMT"),
        };
        let newer = Headline {
            title: "Newer".into(),
            link: Some("https://example.com/new".into()),
            published: parse_rfc2822("Mon, 05 Oct 2026 09:00:00 GMT"),
        };
        // The same story carried by a second feed.
        let repeated = Headline {
            title: "Newer, as another desk ran it".into(),
            link: Some("https://example.com/new".into()),
            published: parse_rfc2822("Mon, 05 Oct 2026 08:00:00 GMT"),
        };
        let undated = Headline {
            title: "No date".into(),
            link: None,
            published: None,
        };

        let selected = select(vec![
            older.clone(),
            newer.clone(),
            repeated,
            undated.clone(),
            undated.clone(),
        ]);

        assert_eq!(
            selected,
            vec![newer, older, undated],
            "newest first, duplicates gone, undated last"
        );
    }

    #[test]
    fn the_digest_is_a_heading_plus_one_block_per_story() {
        let text = render(&[Headline {
            title: "Something happened".into(),
            link: Some("https://example.com/a".into()),
            published: None,
        }]);

        assert!(text.starts_with("*Morning news* — "), "{text}");
        assert!(text.contains("• Something happened"), "{text}");
        assert!(text.contains("https://example.com/a"), "{text}");
    }

    #[test]
    fn an_empty_digest_renders_as_nothing() {
        assert!(render(&[]).is_empty());
    }
}
