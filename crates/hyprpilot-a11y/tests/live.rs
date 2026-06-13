//! Live AT-SPI checks. Ignored by default — they require a running
//! accessibility bus (`org.a11y.Bus`). Run during manual validation with:
//!
//! ```sh
//! cargo test -p hyprpilot-a11y -- --ignored
//! ```

use hyprpilot_a11y::{A11y, WalkOpts};

/// Connecting proves the bus is reachable and our connection code is correct.
/// Walking a pid that can't exist exercises the full app-lookup path
/// (registry root → children → per-app pid query) and must end in
/// `NoApplication`, not a transport error.
#[tokio::test]
#[ignore = "requires a live a11y bus (org.a11y.Bus)"]
async fn connects_and_rejects_unknown_pid() {
    let a11y = A11y::connect().await.expect("connect to a11y bus");
    let result = a11y.snapshot_app(-1, WalkOpts::default()).await;
    assert!(result.is_err(), "pid -1 cannot match any application");
}
