//! The health-probe contract (docs/adr/0003-redis-hard-dependency.md):
//!
//! - plain `/health` is the k8s liveness probe — process-only, green while
//!   the server runs, regardless of Redis state;
//! - `/health?deep=1` is the readiness probe — it pings Redis, the app's one
//!   hard dependency, and answers 503 exactly when Redis is unreachable.
//!
//! This lives in its own test binary on purpose: the unreachable-Redis test
//! pins `REDIS_ADDRESS`/`REDIS_PORT` env vars, and separate test binaries are
//! separate processes, so the pin can never race the other integration
//! suites' real-Redis requests.

use std::net::TcpListener;

/// Serialize the env-pinning tests: env is process-global while this binary's
/// tests run on parallel threads, so every test that mutates it holds the
/// lock for its whole body.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn spawn_app() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind random port");
    let port = listener.local_addr().unwrap().port();
    let server = vod2pod_rss::server::spawn_server(listener).expect("Failed to bind address");
    tokio::spawn(server);
    format!("http://127.0.0.1:{}", port)
}

/// Restore the ambient env exactly as found, so a parallel test never sees
/// the pin.
struct EnvPin(Option<String>, Option<String>);

impl EnvPin {
    fn redis_dead() -> Self {
        let ambient = (
            std::env::var("REDIS_ADDRESS").ok(),
            std::env::var("REDIS_PORT").ok(),
        );
        std::env::set_var("REDIS_ADDRESS", "127.0.0.1");
        // Port 1 hosts no Redis in any environment, so the pin is
        // deterministic in CI (connection refused) and in sandboxes.
        std::env::set_var("REDIS_PORT", "1");
        Self(ambient.0, ambient.1)
    }
}

impl Drop for EnvPin {
    fn drop(&mut self) {
        match self.0.take() {
            Some(v) => std::env::set_var("REDIS_ADDRESS", v),
            None => std::env::remove_var("REDIS_ADDRESS"),
        }
        match self.1.take() {
            Some(v) => std::env::set_var("REDIS_PORT", v),
            None => std::env::remove_var("REDIS_PORT"),
        }
    }
}

// The env pin must span the whole request (the spawned server reads env at
// request time), so the std guard is held across awaits. Safe here: the
// lock's contenders are other tests' threads — blocking serializes them,
// nothing on this test's own runtime ever locks it.
#[allow(clippy::await_holding_lock)]
#[actix_rt::test]
async fn deep_health_serves_when_redis_reachable() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // CI and the dev environment run Redis on the default localhost:6379.
    let address = spawn_app();
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{}/health", address))
        .query(&[("deep", "1")])
        .send()
        .await
        .expect("Failed to execute request.");

    assert_eq!(200, response.status().as_u16());
}

// Same env-pin-must-span-the-request reasoning as above.
#[allow(clippy::await_holding_lock)]
#[actix_rt::test]
async fn deep_health_fails_when_redis_unreachable() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _pin = EnvPin::redis_dead();
    let address = spawn_app();
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{}/health", address))
        .query(&[("deep", "1")])
        .send()
        .await
        .expect("Failed to execute request.");

    assert_eq!(503, response.status().as_u16());
}

// Same env-pin-must-span-the-request reasoning as above.
#[allow(clippy::await_holding_lock)]
#[actix_rt::test]
async fn plain_health_stays_green_when_redis_unreachable() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _pin = EnvPin::redis_dead();
    let address = spawn_app();
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{}/health", address))
        .send()
        .await
        .expect("Failed to execute request.");

    assert_eq!(200, response.status().as_u16());
}
