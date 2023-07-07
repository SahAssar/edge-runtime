extern crate core;

use base::commands::start_server;
use base::integration_test;
use base::server::ServerCodes;
use tokio::select;
use tokio::sync::mpsc;

#[tokio::test]
async fn test_custom_readable_stream_response() {
    integration_test!(
        "http://localhost:9000/crates/base/test_cases/main/index.ts",
        8999,
        "readable-stream-resp",
        |resp: Result<reqwest::Response, reqwest::Error>| async {
            assert_eq!(
                resp.unwrap().text().await.unwrap(),
                "Hello world from streams"
            );
        }
    );
}
