use configs::{conf, Conf, ConfName};

pub mod configs;
pub mod provider;
pub mod rss_transcodizer;
pub mod server;
pub mod transcoder;

#[cfg(test)]
#[ctor::ctor]
fn init_crypto_provider() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls ring crypto provider");
}

pub async fn get_redis_client() -> Result<redis::aio::MultiplexedConnection, eyre::Error> {
    let redis_address = conf().get(ConfName::RedisAddress).unwrap();
    let redis_port = conf().get(ConfName::RedisPort).unwrap();
    let client = redis::Client::open(format!("redis://{}:{}/", redis_address, redis_port))?;
    let con = client.get_multiplexed_async_connection().await?;
    Ok(con)
}
