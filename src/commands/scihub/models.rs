use serde::Deserialize;

/// Subset of a Crossref work object we actually use.
#[derive(Debug, Deserialize)]
pub struct CrossrefWork {
    #[serde(rename = "DOI")]
    pub doi: Option<String>,

    pub title: Option<Vec<String>>,

    pub author: Option<Vec<Author>>,

    #[serde(rename = "container-title")]
    pub container_title: Option<Vec<String>>,

    pub published: Option<DateField>,

    #[serde(rename = "published-print")]
    pub published_print: Option<DateField>,

    #[serde(rename = "published-online")]
    pub published_online: Option<DateField>,

    #[serde(rename = "abstract")]
    pub abstract_text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Author {
    pub given: Option<String>,
    pub family: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DateField {
    /// Crossref sends [[year], [year, month], [year, month, day]], etc.
    pub date_parts: Option<Vec<Vec<u32>>>,
}
