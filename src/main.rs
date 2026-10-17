use log::{debug, info, warn};
use simple_logger::SimpleLogger;
use std::{env, net::TcpListener, process::exit};
use vod2pod_rss::{
    configs::{conf, AudioCodec, Conf, ConfName},
    server,
};

fn main() -> std::io::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls ring crypto provider");

    actix_rt::System::new().block_on(async_main())
}

async fn async_main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if let Some("--version") = args.get(1).cloned().as_deref() {
        let app_version = env!("CARGO_PKG_VERSION");
        println!("{}", app_version);
        exit(0);
    }

    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .env()
        .init()
        .unwrap();

    // Sweep away the staged temp files a crashed predecessor left before the
    // server accepts its first request: at this point this process has no
    // staged sources, so nothing in flight can be caught by the sweep
    // (issue #172). Best-effort — a failure must not block startup.
    vod2pod_rss::transcoder::sweep_stale_staged_sources();

    // The staging gate's capacity has one owner and one place: initialized
    // exactly once from the configured staging concurrency (issue #177),
    // beside the sweep and before the server accepts its first request — no
    // transcode can stage before the gate exists. A second initialization
    // fails loudly.
    vod2pod_rss::transcoder::init_stage_gate();

    // Apple Podcasts (and most podcatchers) play MP3/M4A only: OGG/Opus
    // enclosures fail silently there. Keep the trade-off visible at startup.
    let codec: AudioCodec = conf().get(ConfName::AudioCodec).unwrap().into();
    match codec {
        AudioCodec::MP3 => {}
        AudioCodec::Opus => warn!(
            "AUDIO_CODEC=OPUS: Apple Podcasts and most podcatchers cannot play Opus/OGG; use AUDIO_CODEC=MP3 for maximum compatibility"
        ),
        AudioCodec::OGGVorbis => warn!(
            "AUDIO_CODEC=OGG_VORBIS: Apple Podcasts and most podcatchers cannot play Vorbis/OGG; use AUDIO_CODEC=MP3 for maximum compatibility"
        ),
    }

    if let Err(err) = flush_redis_on_new_version().await {
        panic!(
            "Error interacting with Redis (redis is required): {:?}",
            err
        );
    }

    let host = conf()
        .get(ConfName::Host)
        .unwrap_or_else(|_| "0.0.0.0".to_string());
    let port = conf()
        .get(ConfName::Port)
        .unwrap_or_else(|_| "8080".to_string());
    let bind_addr = format!("{}:{}", host, port);

    let listener = TcpListener::bind(&bind_addr).expect("Failed to bind");
    info!("listening on http://{}", listener.local_addr().unwrap());
    server::spawn_server(listener)
        .expect("could not setup server")
        .await?;
    Ok(())
}

async fn flush_redis_on_new_version() -> eyre::Result<()> {
    let app_version = env!("CARGO_PKG_VERSION");
    info!("app version {app_version}");

    let mut con = vod2pod_rss::get_redis_client().await?;

    let cached_version: Option<String> = redis::cmd("GET")
        .arg("version")
        .query_async(&mut con)
        .await?;
    debug!("cached app version {:?}", cached_version);

    if let Some(ref cached_version) = cached_version {
        if cached_version != app_version {
            info!("detected version change ({cached_version} != {app_version}) flushing redis DB");
            () = redis::cmd("FLUSHDB").query_async(&mut con).await?;
        }
    }

    () = redis::cmd("SET")
        .arg("version")
        .arg(app_version)
        .query_async(&mut con)
        .await?;
    debug!("set cached app version to {app_version}");

    Ok(())
}
