use crate::commands::shazam::fingerprint::{Signature, SignatureGenerator};
use crate::commands::shazam::models::{RecognizeRequestBody, RecognizeResult, RecognizeSignature};
use crate::commands::shazam::signature::encode_to_uri;
use reqwest::{Client as HttpClient, header};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct ShazamClient {
    http_client: HttpClient,
    language: String,
    endpoint_country: String,
}

impl ShazamClient {
    pub fn new() -> Result<Self, reqwest::Error> {
        let http_client = HttpClient::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        Ok(Self {
            http_client,
            language: "en-US".to_string(),
            endpoint_country: "GB".to_string(),
        })
    }

    pub async fn recognize_from_pcm(
        &self,
        samples: &[i16],
    ) -> Result<RecognizeResult, Box<dyn std::error::Error + Send + Sync>> {
        let sig = self
            .build_signature(samples)
            .ok_or("Not enough samples to generate a signature")?;
        self.send_recognize_request(sig).await
    }

    fn build_signature(&self, samples: &[i16]) -> Option<Signature> {
        let mut sg = SignatureGenerator::new();
        sg.feed_input(samples);
        sg.get_next_signature()
    }

    async fn send_recognize_request(
        &self,
        sig: Signature,
    ) -> Result<RecognizeResult, Box<dyn std::error::Error + Send + Sync>> {
        let uri = encode_to_uri(&sig)?;
        let sample_ms = (sig.number_samples as f64 / sig.sample_rate_hz as f64 * 1000.0) as i32;
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;

        let req_body = RecognizeRequestBody {
            timezone: "Europe/London".to_string(),
            signature: RecognizeSignature {
                uri,
                samplems: sample_ms,
            },
            timestamp,
            context: serde_json::json!({}),
            geolocation: serde_json::json!({}),
        };

        let url = Self::build_search_url(&self.language, &self.endpoint_country);

        let mut headers = header::HeaderMap::new();
        headers.insert(
            "X-Shazam-Platform",
            header::HeaderValue::from_static("IPHONE"),
        );
        headers.insert(
            "X-Shazam-AppVersion",
            header::HeaderValue::from_static("14.1.0"),
        );
        headers.insert(header::ACCEPT, header::HeaderValue::from_static("*/*"));
        headers.insert(
            header::ACCEPT_LANGUAGE,
            header::HeaderValue::from_str(&self.language)?,
        );
        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_str(&Self::random_user_agent())?,
        );

        let response = self
            .http_client
            .post(&url)
            .headers(headers)
            .json(&req_body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await?;
            return Err(format!("Shazam API returned {}: {}", status, error_body).into());
        }

        let result: RecognizeResult = response.json().await?;
        Ok(result)
    }

    fn build_search_url(language: &str, endpoint_country: &str) -> String {
        let device = Self::random_device();
        let uuid1 = uuid::Uuid::new_v4().to_string().to_uppercase();
        let uuid2 = uuid::Uuid::new_v4().to_string().to_uppercase();

        format!(
            "https://amp.shazam.com/discovery/v5/{}/{}/{}/-/tag/{}/{}?sync=true&webv3=true&sampling=true&connected=&shazamapiversion=v3&sharehub=true&hubv5minorversion=v5.1&hidelb=true&video=v3",
            language, endpoint_country, device, uuid1, uuid2
        )
    }

    fn random_device() -> String {
        let devices = ["iphone", "android", "web"];
        let idx = rand::random::<u32>() as usize % devices.len();
        devices[idx].to_string()
    }

    fn random_user_agent() -> String {
        let agents = [
            "Mozilla/5.0 (iPhone; CPU iPhone OS 16_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.0 Mobile/15E148 Safari/604.1",
            "Mozilla/5.0 (Linux; Android 12; Pixel 6) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/112.0.0.0 Mobile Safari/537.36",
            "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1",
            "Mozilla/5.0 (Linux; Android 13; SM-S908B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/114.0.0.0 Mobile Safari/537.36",
        ];
        let idx = rand::random::<u32>() as usize % agents.len();
        agents[idx].to_string()
    }
}
