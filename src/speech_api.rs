//! Shared transport for explicitly configured OpenAI-compatible speech APIs.
use crate::config::SpeechApi;
use anyhow::{bail, Context, Result};
use reqwest::{Client, RequestBuilder, Response};
use std::time::Duration;

#[derive(Clone)]
pub(crate) struct AudioApi {
    pub settings: SpeechApi,
    client: Client,
}

impl AudioApi {
    pub fn new(settings: &SpeechApi) -> Result<Self> {
        Ok(Self {
            settings: settings.clone(),
            client: Client::builder()
                .timeout(Duration::from_secs(settings.timeout_seconds))
                .connect_timeout(Duration::from_secs(10))
                // Never redirect recordings or credentials to a different endpoint.
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }

    pub fn post(&self, endpoint: &str) -> Result<RequestBuilder> {
        let request = self.client.post(format!(
            "{}/audio/{endpoint}",
            self.settings.base_url.trim_end_matches('/')
        ));
        if self.settings.api_key_env.is_empty() {
            return Ok(request);
        }
        let key = std::env::var(&self.settings.api_key_env).with_context(|| {
            format!(
                "speech API credential environment variable {} is not set",
                self.settings.api_key_env
            )
        })?;
        if key.trim().is_empty() {
            bail!("speech API credential is empty");
        }
        Ok(request.bearer_auth(key))
    }

    pub async fn send(request: RequestBuilder) -> Result<Response> {
        let response = request
            .send()
            .await
            .map_err(|e| e.without_url())
            .context("speech API request failed")?;
        if !response.status().is_success() {
            // Bodies may echo transcripts, input text or credentials. Do not log them.
            bail!("speech API returned HTTP {}", response.status());
        }
        Ok(response)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    pub async fn server(status: &str, body: &str) -> (SpeechApi, JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let settings = SpeechApi {
            base_url: format!("http://{}/v1/", listener.local_addr().unwrap()),
            model: "test-model".into(),
            voice: "test-voice".into(),
            ..SpeechApi::default()
        };
        let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let count = stream.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
                if let Some(end) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap_or("0")
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            stream.write_all(response.as_bytes()).await.unwrap();
            request
        });
        (settings, task)
    }

    #[tokio::test]
    async fn provider_errors_do_not_echo_private_bodies() {
        for status in [
            "401 Unauthorized",
            "429 Too Many Requests",
            "302 Found",
            "500 Server Error",
        ] {
            let (settings, request) = server(status, "private transcript and secret").await;
            let api = AudioApi::new(&settings).unwrap();
            let error = AudioApi::send(api.post("speech").unwrap())
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(&status[..3]));
            assert!(!error.contains("private"));
            request.await.unwrap();
        }
    }

    #[test]
    fn missing_credentials_fail_before_network_io() {
        let api = AudioApi::new(&SpeechApi {
            base_url: "http://localhost/v1".into(),
            api_key_env: format!("ORCHRD_MISSING_{}", uuid::Uuid::new_v4().simple()),
            ..SpeechApi::default()
        })
        .unwrap();
        assert!(api.post("transcriptions").is_err());
    }
}
