//! Audio transcription via Whisper API (or compatible endpoints).
//!
//! Off by default — enabled via `[transcription] enabled = true` in intendant.toml.
//! Audio is buffered in ~3s chunks by the web gateway, wrapped in WAV, and sent here.

use crate::error::CallerError;
use crate::provider::mask_api_keys;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A single transcription result.
#[derive(Debug, Clone)]
pub struct TranscriptSegment {
    pub text: String,
    #[allow(dead_code)]
    pub language: Option<String>,
    #[allow(dead_code)]
    pub duration_secs: f32,
}

/// Configuration for transcription, parsed from `[transcription]` in intendant.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_provider")]
    #[allow(dead_code)]
    pub provider: String,
    #[serde(default = "default_model")]
    pub model: String,
    /// Custom endpoint URL (for self-hosted whisper.cpp, etc.).
    /// Defaults to OpenAI's API.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Optional language hint (ISO-639-1). Auto-detect if omitted.
    #[serde(default)]
    pub language: Option<String>,
    /// Audio buffer duration in seconds before sending to API.
    #[serde(default = "default_buffer_secs")]
    #[allow(dead_code)]
    pub buffer_secs: f32,
}

fn default_provider() -> String {
    "openai".to_string()
}
fn default_model() -> String {
    "whisper-1".to_string()
}
fn default_buffer_secs() -> f32 {
    3.0
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: default_provider(),
            model: default_model(),
            endpoint: None,
            language: None,
            buffer_secs: default_buffer_secs(),
        }
    }
}

/// Trait for audio transcription backends.
#[async_trait]
pub trait Transcriber: Send + Sync {
    async fn transcribe(&self, audio_wav: &[u8]) -> Result<TranscriptSegment, CallerError>;
}

/// OpenAI Whisper API transcriber.
pub struct WhisperTranscriber {
    client: reqwest::Client,
    api_key: String,
    endpoint: String,
    model: String,
    language: Option<String>,
}

impl WhisperTranscriber {
    pub fn new(config: &TranscriptionConfig) -> Result<Self, CallerError> {
        let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
            CallerError::Config(
                "OPENAI_API_KEY not set (required for Whisper transcription)".to_string(),
            )
        })?;
        let endpoint = config
            .endpoint
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1/audio/transcriptions".to_string());
        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            endpoint,
            model: config.model.clone(),
            language: config.language.clone(),
        })
    }
}

#[derive(Deserialize)]
struct WhisperResponse {
    text: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    duration: Option<f32>,
}

#[async_trait]
impl Transcriber for WhisperTranscriber {
    async fn transcribe(&self, audio_wav: &[u8]) -> Result<TranscriptSegment, CallerError> {
        if std::env::var("RUST_LOG").is_ok() {
            eprintln!(
                "transcription: POST {} (model={}, audio_size={})",
                self.endpoint,
                self.model,
                audio_wav.len()
            );
        }

        let file_part = reqwest::multipart::Part::bytes(audio_wav.to_vec())
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .map_err(|e| CallerError::Provider(format!("multipart error: {}", e)))?;

        let mut form = reqwest::multipart::Form::new()
            .text("model", self.model.clone())
            .text("response_format", "verbose_json")
            .part("file", file_part);

        if let Some(ref lang) = self.language {
            form = form.text("language", lang.clone());
        }

        let resp = self
            .client
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(CallerError::Provider(format!(
                "Whisper API HTTP {}: {}",
                status,
                mask_api_keys(&body)
            )));
        }

        let wr: WhisperResponse = resp.json().await?;
        if std::env::var("RUST_LOG").is_ok() {
            eprintln!(
                "transcription: text={:?} language={:?} duration={:?}",
                wr.text, wr.language, wr.duration
            );
        }

        Ok(TranscriptSegment {
            text: wr.text,
            language: wr.language,
            duration_secs: wr.duration.unwrap_or(0.0),
        })
    }
}

/// Encode raw PCM16 samples into a WAV byte buffer.
///
/// Writes a standard 44-byte RIFF/WAV header followed by the raw PCM data.
pub fn encode_wav(pcm16: &[u8], sample_rate: u32, channels: u16) -> Vec<u8> {
    let data_len = pcm16.len() as u32;
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * channels as u32 * (bits_per_sample as u32 / 8);
    let block_align = channels * (bits_per_sample / 8);
    let file_size = 36 + data_len; // total - 8 bytes for RIFF header

    let mut buf = Vec::with_capacity(44 + pcm16.len());
    // RIFF header
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&file_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    // fmt chunk
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // chunk size
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&bits_per_sample.to_le_bytes());
    // data chunk
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_len.to_le_bytes());
    buf.extend_from_slice(pcm16);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_wav_header_structure() {
        let pcm = vec![0u8; 100];
        let wav = encode_wav(&pcm, 16000, 1);
        assert_eq!(wav.len(), 44 + 100);
        // RIFF header
        assert_eq!(&wav[0..4], b"RIFF");
        let file_size = u32::from_le_bytes([wav[4], wav[5], wav[6], wav[7]]);
        assert_eq!(file_size, 36 + 100);
        assert_eq!(&wav[8..12], b"WAVE");
        // fmt chunk
        assert_eq!(&wav[12..16], b"fmt ");
        let fmt_size = u32::from_le_bytes([wav[16], wav[17], wav[18], wav[19]]);
        assert_eq!(fmt_size, 16);
        let format = u16::from_le_bytes([wav[20], wav[21]]);
        assert_eq!(format, 1); // PCM
        let channels = u16::from_le_bytes([wav[22], wav[23]]);
        assert_eq!(channels, 1);
        let sample_rate = u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]);
        assert_eq!(sample_rate, 16000);
        // data chunk
        assert_eq!(&wav[36..40], b"data");
        let data_size = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]);
        assert_eq!(data_size, 100);
    }

    #[test]
    fn encode_wav_stereo() {
        let pcm = vec![0u8; 200];
        let wav = encode_wav(&pcm, 48000, 2);
        assert_eq!(wav.len(), 44 + 200);
        let channels = u16::from_le_bytes([wav[22], wav[23]]);
        assert_eq!(channels, 2);
        let sample_rate = u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]);
        assert_eq!(sample_rate, 48000);
        let byte_rate = u32::from_le_bytes([wav[28], wav[29], wav[30], wav[31]]);
        assert_eq!(byte_rate, 48000 * 2 * 2); // sr * channels * bytes_per_sample
    }

    #[test]
    fn encode_wav_empty() {
        let wav = encode_wav(&[], 16000, 1);
        assert_eq!(wav.len(), 44);
        let data_size = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]);
        assert_eq!(data_size, 0);
    }

    #[test]
    fn default_transcription_config() {
        let config = TranscriptionConfig::default();
        assert!(config.enabled);
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model, "whisper-1");
        assert!(config.endpoint.is_none());
        assert!(config.language.is_none());
        assert!((config.buffer_secs - 3.0).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_transcription_config_full() {
        let toml_str = r#"
enabled = true
provider = "openai"
model = "whisper-1"
endpoint = "http://localhost:8080/v1/audio/transcriptions"
language = "en"
buffer_secs = 5.0
"#;
        let config: TranscriptionConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.model, "whisper-1");
        assert_eq!(
            config.endpoint.as_deref(),
            Some("http://localhost:8080/v1/audio/transcriptions")
        );
        assert_eq!(config.language.as_deref(), Some("en"));
        assert!((config.buffer_secs - 5.0).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_transcription_config_minimal() {
        let toml_str = "enabled = true\n";
        let config: TranscriptionConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model, "whisper-1");
        assert!(config.endpoint.is_none());
    }
}
