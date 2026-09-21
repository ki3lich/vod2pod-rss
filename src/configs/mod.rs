use log::warn;
use serde::Serialize;

pub fn conf() -> impl Conf {
    EnvConf {}
}

pub trait Conf {
    fn get(&self, key: ConfName) -> eyre::Result<String>;
}

/// Default Fresh TTL of the feed cache, seconds (1 hour).
pub const DEFAULT_CACHE_TTL_SECS: u64 = 3600;
/// Default Stale Window, seconds (7 days).
pub const DEFAULT_STALE_MAX_AGE_SECS: u64 = 7 * 24 * 3600;
/// Default Fresh Period bound, seconds (24 hours).
pub const DEFAULT_MAX_FRESH_PERIOD_SECS: u64 = 24 * 3600;

pub enum ConfName {
    RedisAddress,
    RedisPort,
    RedisUrl,
    Mp3Bitrate,
    YoutubeApiKey,
    YoutubeMaxResults,
    YoutubeFilterShorts,
    TwitchClientId,
    TwitchSecretKey,
    TranscodingEnabled,
    SubfolderPath,
    ValidUrlDomains,
    AudioCodec,
    PeerTubeValidHosts,
    YoutubeYtDlpExtraArgs,
    CacheTTL,
    StaleMaxAge,
    MaxFreshPeriod,
    FfmpegTimeoutSeconds,
    PreflightTimeoutSeconds,
    Host,
    Port,
}

struct EnvConf {}

impl Conf for EnvConf {
    fn get(&self, key: ConfName) -> eyre::Result<String> {
        match key {
            ConfName::RedisAddress => {
                Ok(std::env::var("REDIS_ADDRESS").unwrap_or_else(|_| "localhost".to_string()))
            }
            ConfName::RedisPort => {
                Ok(std::env::var("REDIS_PORT").unwrap_or_else(|_| "6379".to_string()))
            }
            ConfName::RedisUrl => {
                let redis_address = conf().get(ConfName::RedisAddress).unwrap();
                let redis_port = conf().get(ConfName::RedisPort).unwrap();
                Ok(format!("redis://{redis_address}:{redis_port}/"))
            }
            ConfName::Mp3Bitrate => {
                Ok(std::env::var("MP3_BITRATE").unwrap_or_else(|_| "192".to_string()))
            }
            ConfName::TwitchClientId => std::env::var("TWITCH_CLIENT_ID")
                .map_err(|e| eyre::eyre!(e))
                .and_then(|s| {
                    if s.is_empty() {
                        Err(eyre::eyre!("no TwitchClientId api key"))
                    } else {
                        Ok(s)
                    }
                }),
            ConfName::TwitchSecretKey => std::env::var("TWITCH_SECRET")
                .map_err(|e| eyre::eyre!(e))
                .and_then(|s| {
                    if s.is_empty() {
                        Err(eyre::eyre!("no TwitchSecretKey api key"))
                    } else {
                        Ok(s)
                    }
                }),
            ConfName::YoutubeApiKey => std::env::var("YT_API_KEY")
                .map_err(|e| eyre::eyre!(e))
                .and_then(|s| {
                    if s.is_empty() {
                        Err(eyre::eyre!("no youtube api key"))
                    } else {
                        Ok(s)
                    }
                }),
            ConfName::TranscodingEnabled => {
                Ok(std::env::var("TRANSCODE").unwrap_or_else(|_| "False".to_string()))
            }
            ConfName::SubfolderPath => {
                let mut folder = std::env::var("SUBFOLDER").unwrap_or("".to_string());
                if !folder.starts_with('/') {
                    folder.insert(0, '/');
                }
                while folder.ends_with('/') {
                    folder.pop();
                }
                Ok(folder)
            }
            ConfName::ValidUrlDomains => {
                Ok(std::env::var("VALID_URL_DOMAINS").unwrap_or_else(|_| "".to_string()))
            }
            ConfName::AudioCodec => Ok(std::env::var("AUDIO_CODEC")
                .map(|c| match c.as_str() {
                    "MP3" => c,
                    "OPUS" => c,
                    "OGG" => "OGG_VORBIS".to_string(),
                    "VORBIS" => "OGG_VORBIS".to_string(),
                    "OGG_VORBIS" => c,
                    _ => {
                        warn!("Unrecognized codec \"{c}\". Defaulting to MP3.");
                        "MP3".to_string()
                    }
                })
                .unwrap_or_else(|_| "MP3".to_string())),
            ConfName::PeerTubeValidHosts => {
                Ok(std::env::var("PEERTUBE_VALID_DOMAINS").unwrap_or_else(|_| "".to_string()))
            }
            ConfName::YoutubeMaxResults => {
                Ok(std::env::var("YOUTUBE_MAX_RESULTS").unwrap_or_else(|_| "300".to_string()))
            }
            ConfName::YoutubeFilterShorts => {
                Ok(std::env::var("YOUTUBE_FILTER_SHORTS").unwrap_or_else(|_| "false".to_string()))
            }
            ConfName::YoutubeYtDlpExtraArgs => {
                Ok(std::env::var("YOUTUBE_YT_DLP_GET_URL_EXTRA_ARGS")
                    .unwrap_or_else(|_| "[]".to_string()))
            }
            ConfName::CacheTTL => {
                // one hour: podcatchers poll on their own schedule and a fresh
                // copy is checked with a 1-unit freshness probe after expiry,
                // so a long TTL costs almost no quota (see docs/adr/0001)
                Ok(std::env::var("CACHE_TTL")
                    .unwrap_or_else(|_| DEFAULT_CACHE_TTL_SECS.to_string()))
            }
            ConfName::StaleMaxAge => {
                // Stale Window: how long a cached feed may keep being served
                // when upstream cannot be reached (quota exhausted, provider
                // down, ...). A stale feed is always better than an error.
                Ok(std::env::var("STALE_MAX_AGE")
                    .unwrap_or_else(|_| DEFAULT_STALE_MAX_AGE_SECS.to_string()))
            }
            ConfName::MaxFreshPeriod => {
                // upper bound on how long freshness probes may keep a feed
                // "fresh" without a full regeneration: bounds how long a
                // change the probe cannot see stays invisible
                Ok(std::env::var("MAX_FRESH_PERIOD")
                    .unwrap_or_else(|_| DEFAULT_MAX_FRESH_PERIOD_SECS.to_string()))
            }
            ConfName::FfmpegTimeoutSeconds => {
                Ok(std::env::var("FFMPEG_TIMEOUT_SECONDS").unwrap_or_else(|_| "300".to_string()))
            }
            ConfName::PreflightTimeoutSeconds => {
                // Max seconds to wait for a TCP handshake to the resolved media
                // stream URL before declaring the source unreachable. Guards
                // against CDN edge nodes that resolve but silently drop SYNs
                // (the kernel ~127s ETIMEDOUT that ffmpeg would otherwise hang
                // on). Tunable because some ISPs route to flaky edge nodes.
                Ok(std::env::var("PREFLIGHT_TIMEOUT_SECONDS").unwrap_or_else(|_| "3".to_string()))
            }
            ConfName::Host => {
                Ok(std::env::var("VOD2POD_RSS_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()))
            }
            ConfName::Port => {
                Ok(std::env::var("VOD2POD_RSS_PORT").unwrap_or_else(|_| "8080".to_string()))
            }
        }
    }
}

/// Read a numeric config, falling back to `default` when unset or unparsable.
pub fn conf_u64(key: ConfName, default: u64) -> u64 {
    conf()
        .get(key)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(default)
}

#[derive(Serialize, Clone, Copy, Default)]
pub enum AudioCodec {
    #[default]
    MP3,
    Opus,
    OGGVorbis,
}

impl AudioCodec {
    pub fn get_ffmpeg_codec_str(&self) -> &'static str {
        match self {
            AudioCodec::MP3 => "libmp3lame",
            AudioCodec::Opus => {
                warn!("seeking is not supported with OPUS codec  ");
                "libopus"
            }
            AudioCodec::OGGVorbis => {
                warn!("seeking is not supported with OGG_VORBIS codec ... ");
                "libvorbis"
            }
        }
    }

    pub fn get_extension_str(&self) -> &'static str {
        match self {
            AudioCodec::MP3 => "mp3",
            AudioCodec::Opus => "webm",
            AudioCodec::OGGVorbis => "webm",
        }
    }

    pub fn get_mime_type_str(&self) -> &'static str {
        match self {
            AudioCodec::MP3 => "audio/mpeg",
            AudioCodec::Opus => "audio/webm",
            AudioCodec::OGGVorbis => "audio/webm",
        }
    }
}

impl From<String> for AudioCodec {
    fn from(value: String) -> Self {
        match value.as_str() {
            "MP3" => AudioCodec::MP3,
            "OPUS" => AudioCodec::Opus,
            "OGG_VORBIS" => AudioCodec::OGGVorbis,
            _ => AudioCodec::MP3,
        }
    }
}
