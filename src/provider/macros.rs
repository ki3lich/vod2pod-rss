macro_rules! provider_dispatcher {
            ($name: ident, $self:ident,  $($provider:ident),* ;$($method:tt)*) => {

                //nested macro
                macro_rules! method_name {
                    ($x:ident) => {
                        return $x.$($method)*
                    };
                }
                //provider
                match $self {
                    $(
                        $name::$provider(x) => {method_name!(x);},
                    )*
                }
            };
        }

macro_rules! dispatch_if_match_static {
    ($name: ident, $url: expr, $provider: ident) => {
        let provider = $provider {};
        for regex in provider.domain_whitelist_regexes() {
            if regex.is_match(&$url.to_string()) {
                debug!("using {}", stringify!($provider));
                return $name::$provider(provider);
            }
        }
    };
}

/// this will generate the boilerplate to statically dispatch the provider
///
/// Every `$provider` is dispatched to by matching the url against its own
/// `domain_whitelist_regexes`; `$fallback` is the catch-all every unmatched
/// url ends up at.
macro_rules! generate_static_dispatcher {
    ($name:ident for $($provider:ident),+ ; fallback $fallback:ident $(,)?) => {

#[async_trait]
impl MediaProvider for $name {
    async fn generate_rss_feed(&self, channel_url: Url) -> eyre::Result<GeneratedFeed> {
        provider_dispatcher!($name, self, $($provider,)* $fallback ; generate_rss_feed(channel_url).await);
    }

    async fn canonical_feed_id(&self, channel_url: &Url) -> Option<String> {
        provider_dispatcher!($name, self, $($provider,)* $fallback ; canonical_feed_id(&channel_url).await);
    }

    async fn probe_feed_freshness(
        &self,
        channel_url: &Url,
        state: &FeedProbeState,
    ) -> eyre::Result<bool> {
        provider_dispatcher!($name, self, $($provider,)* $fallback ; probe_feed_freshness(&channel_url, state).await);
    }

    async fn generate_rss_feed_quota_free(&self, channel_url: Url) -> eyre::Result<String> {
        provider_dispatcher!($name, self, $($provider,)* $fallback ; generate_rss_feed_quota_free(channel_url).await);
    }

    fn quota_breaker_key(&self) -> Option<String> {
        provider_dispatcher!($name, self, $($provider,)* $fallback ; quota_breaker_key());
    }

    async fn get_stream_url(&self, media_url: &Url) -> eyre::Result<Url> {
        provider_dispatcher!($name, self, $($provider,)* $fallback ; get_stream_url(&media_url).await);
    }

    fn streams_direct_media(&self) -> bool {
        provider_dispatcher!($name, self, $($provider,)* $fallback ; streams_direct_media());
    }

    async fn evict_stream_url_cache(&self, media_url: &Url) -> eyre::Result<()> {
        provider_dispatcher!($name, self, $($provider,)* $fallback ; evict_stream_url_cache(media_url).await);
    }

    fn domain_whitelist_regexes(&self) -> Vec<Regex> {
        provider_dispatcher!($name, self, $($provider,)* $fallback ; domain_whitelist_regexes());
    }
}

pub fn from(url: &Url) -> $name {
    $(
    dispatch_if_match_static!($name, url, $provider);
    )*
    // reaching this line means no provider's url regexes matched. Landing on
    // the Generic Provider is that provider's legitimate entry for genuinely
    // generic urls and must stay silent for them — but when the url's host
    // sits in a specific provider's domain whitelist while that provider's
    // regexes missed the full url, the miss is worth shouting about: the
    // Generic Provider will likely produce a garbage feed for it.
    $(
        let provider_name = stringify!($provider);
        if ($provider {})
            .domain_whitelist_regexes()
            .iter()
            .any(|regex| host_matches_domain(regex, url))
        {
            warn!(
                "probable regex miss: url ({url}) fell through to the Generic Provider, but its host ({host}) is on {provider_name}'s domain whitelist ({provider_name}'s url regexes did not match the full url, so provider-specific handling was skipped and the resulting feed will likely be broken)",
                host = url.host_str().unwrap_or("<no host>")
            );
        }
    )*
    debug!("using GenericProvider as provider");
    return $name::$fallback($fallback);
}

pub enum $name {
    $(
    $provider($provider),
    )*
    $fallback($fallback),
}
    };
}
