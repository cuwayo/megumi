use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RecognizeResult {
    #[serde(default)]
    pub matches: Vec<Match>,
    pub track: Option<Track>,
    pub retryms: Option<i32>,
    pub tagid: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Match {
    pub id: String,
    pub offset: f64,
    pub timeskew: f64,
    pub frequencyskew: f64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Track {
    #[serde(default)]
    pub layout: String,
    #[serde(default, rename = "type")]
    pub track_type: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    pub images: Option<TrackImages>,
    pub share: Option<TrackShare>,
    #[serde(default)]
    pub sections: Vec<Section>,
    pub hub: Option<TrackHub>,
    #[serde(default)]
    pub artists: Vec<Artist>,
    #[serde(default)]
    pub url: String,
    pub genres: Option<TrackGenres>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TrackImages {
    #[serde(default)]
    pub background: String,
    #[serde(default)]
    pub coverart: String,
    #[serde(default)]
    pub coverarthq: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TrackShare {
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub href: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub twitter: String,
    #[serde(default)]
    pub html: String,
    #[serde(default)]
    pub avatar: String,
    #[serde(default)]
    pub snapchat: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TrackHub {
    #[serde(default, rename = "type")]
    pub hub_type: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub explicit: bool,
    #[serde(default)]
    pub displayname: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TrackGenres {
    #[serde(default)]
    pub primary: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Section {
    #[serde(default, rename = "type")]
    pub section_type: String,
    pub metadata: Option<Vec<Metadata>>,
    pub text: Option<Vec<String>>,
    pub paragraphs: Option<Vec<Paragraph>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Paragraph {
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Metadata {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Artist {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub adamid: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RecognizeRequestBody {
    pub timezone: String,
    pub signature: RecognizeSignature,
    pub timestamp: i64,
    pub context: serde_json::Value,
    pub geolocation: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RecognizeSignature {
    pub uri: String,
    pub samplems: i32,
}
