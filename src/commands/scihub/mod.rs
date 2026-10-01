pub mod models;

use megumi::{Error, command};

use crate::Context;
use models::CrossrefWork;

const CROSSREF_API: &str = "https://api.crossref.org/works";

/// Looks up a paper by DOI and replies with a Sci-Hub link.
#[command(name = "scihub", aliases("sci", "paper", "doi"), react = "🔎")]
pub async fn scihub(ctx: Context, #[rest] query: &str) -> Result<(), Error> {
    let query = query.trim();

    if query.is_empty() {
        return Err("Please provide a DOI or search terms.\nExamples:\n  \
             !scihub 10.1038/nature12373\n  \
             !scihub machine learning survey"
            .into());
    }

    // Accept either:
    //   !scihub 10.1000/xyz123          (bare DOI)
    //   !scihub https://doi.org/10.xxx  (doi.org URL)
    //   !scihub https://scihub.se/10.xx (Sci-Hub URL)
    //   !scihub search terms here       (title keyword search via Crossref)
    match extract_doi(query).or_else(|| query.split_whitespace().next().and_then(extract_doi)) {
        Some(doi) => lookup_by_doi(&ctx, &doi).await,
        None => search_by_title(&ctx, query).await,
    }
}

/// Pull a bare DOI (10.xxx/yyy) out of a raw string that might be a URL or
/// already a DOI.
pub fn extract_doi(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Strip common URL prefixes, then look for 10.<registrant>/<suffix>
    let stripped = s
        .strip_prefix("https://doi.org/")
        .or_else(|| s.strip_prefix("http://doi.org/"))
        .or_else(|| s.strip_prefix("https://dx.doi.org/"))
        .or_else(|| s.strip_prefix("http://dx.doi.org/"))
        .or_else(|| {
            // e.g. https://sci-hub.se/10.1000/xyz  →  strip up to the DOI
            if let Some(pos) = s.find("/10.") {
                Some(&s[pos + 1..])
            } else {
                None
            }
        })
        .unwrap_or(s);

    // Drop a query string, fragment, or trailing prose. A DOI suffix never
    // contains whitespace, so the first word is the whole identifier.
    let stripped = stripped.split(['?', '#']).next().unwrap_or(stripped);
    let stripped = stripped.split_whitespace().next().unwrap_or(stripped);
    let stripped = stripped.trim_end_matches(['.', ',', ';', ':', ')', ']']);

    // 10.<registrant of 4+ digits>/<suffix>
    let rest = stripped.strip_prefix("10.")?;
    let (registrant, suffix) = rest.split_once('/')?;
    if registrant.len() >= 4 && !suffix.is_empty() && registrant.chars().all(|c| c.is_ascii_digit())
    {
        Some(format!("10.{registrant}/{suffix}"))
    } else {
        None
    }
}

async fn lookup_by_doi(ctx: &Context, doi: &str) -> Result<(), Error> {
    let url = format!("{CROSSREF_API}/{}", urlencoding(doi));

    let response = reqwest::get(&url)
        .await
        .map_err(|error| format!("Failed to reach Crossref: {error}"))?;

    if response.status() == 404 {
        return Err(
            format!("DOI `{doi}` was not found in Crossref. Check the DOI and try again.").into(),
        );
    }
    if !response.status().is_success() {
        return Err(format!("Crossref returned HTTP {}.", response.status()).into());
    }

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("Failed to parse Crossref response: {error}"))?;

    let work: CrossrefWork = serde_json::from_value(body["message"].clone())
        .map_err(|error| format!("Unexpected Crossref shape: {error}"))?;

    ctx.reply_quoting(&format_work(&work, doi)).await?;
    Ok(())
}

async fn search_by_title(ctx: &Context, keywords: &str) -> Result<(), Error> {
    let encoded = urlencoding(keywords);
    let url = format!(
        "{CROSSREF_API}?query={encoded}&rows=1&select=DOI,title,author,published,container-title,abstract"
    );

    let response = reqwest::get(&url)
        .await
        .map_err(|error| format!("Failed to reach Crossref: {error}"))?;

    if !response.status().is_success() {
        return Err(format!("Crossref returned HTTP {}.", response.status()).into());
    }

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("Failed to parse Crossref response: {error}"))?;

    let items = body["message"]["items"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or("No results found for that query.")?;

    let work: CrossrefWork = serde_json::from_value(items[0].clone())
        .map_err(|error| format!("Unexpected Crossref shape: {error}"))?;

    let doi = work.doi.clone().unwrap_or_default();
    ctx.reply_quoting(&format_work(&work, &doi)).await?;
    Ok(())
}

fn format_work(work: &CrossrefWork, doi: &str) -> String {
    let mut lines: Vec<String> = Vec::new();

    // Title
    let title = work
        .title
        .as_deref()
        .and_then(|t| t.first().map(String::as_str))
        .unwrap_or("Unknown title");
    lines.push(format!("📄 *{title}*"));

    // Authors (up to 5)
    if let Some(authors) = &work.author
        && !authors.is_empty()
    {
        let names: Vec<String> = authors
            .iter()
            .take(5)
            .map(|a| match (&a.given, &a.family) {
                (Some(g), Some(f)) => format!("{g} {f}"),
                (None, Some(f)) => f.clone(),
                (Some(g), None) => g.clone(),
                (None, None) => "Unknown".to_string(),
            })
            .collect();
        let suffix = if authors.len() > 5 {
            format!(" (+{})", authors.len() - 5)
        } else {
            String::new()
        };
        lines.push(format!("✍️ {}{suffix}", names.join(", ")));
    }

    // Journal / container
    if let Some(journal) = work
        .container_title
        .as_deref()
        .and_then(|t| t.first().map(String::as_str))
        && !journal.is_empty()
    {
        lines.push(format!("📰 {journal}"));
    }

    // Year. A direct DOI lookup usually has published-print or published-online
    // rather than the search endpoint's published field.
    if let Some(year) = [
        &work.published,
        &work.published_print,
        &work.published_online,
    ]
    .into_iter()
    .find_map(|date| {
        date.as_ref()?
            .date_parts
            .as_ref()?
            .first()?
            .first()
            .copied()
    }) {
        lines.push(format!("📅 {year}"));
    }

    // DOI + Sci-Hub link
    if !doi.is_empty() {
        lines.push(format!("🔗 DOI: {doi}"));
        lines.push(format!("🚀 Sci-Hub: https://sci-hub.se/{doi}"));
    }

    // Abstract (trimmed to 400 chars)
    if let Some(abs) = &work.abstract_text {
        let clean = strip_jats(abs);
        if !clean.is_empty() {
            // Cut on a char boundary; a byte slice panics mid-codepoint.
            let trimmed = match clean.char_indices().nth(400) {
                Some((end, _)) => format!("{}…", &clean[..end]),
                None => clean,
            };
            lines.push(String::new());
            lines.push(format!("_{trimmed}_"));
        }
    }

    lines.join("\n")
}

/// Minimal JATS/XML tag stripper for Crossref abstracts.
fn strip_jats(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Percent-encode a string for use in a URL path or query value.
fn urlencoding(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                vec![c.to_string()]
            } else {
                c.to_string().bytes().map(|b| format!("%{b:02X}")).collect()
            }
        })
        .collect()
}
