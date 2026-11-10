//! The allocator a process runs on is chosen once, as it starts: mimalloc,
//! or the system's with `HOT_ALLOCATOR=system`.
//!
//! This test is its own binary, with the allocator `hot` installs. The
//! choice is made at the binary's first allocation, so the test starts the
//! binary again with the variable set each way and lets that process say
//! what it runs on.

use hot::lang::runtime::oom_logger::{LoggingAllocator, allocator_name};
use std::process::Command;

#[global_allocator]
static GLOBAL: LoggingAllocator = LoggingAllocator;

/// Set in a process this test started: the allocator it should be on.
const EXPECTED: &str = "HOT_ALLOCATOR_TEST_EXPECTS";

#[test]
fn the_environment_chooses_the_allocator_at_startup() {
    if let Ok(expected) = std::env::var(EXPECTED) {
        assert_eq!(allocator_name(), expected);
        return;
    }
    // With the feature off there is only the system allocator.
    let default = if cfg!(feature = "mimalloc") {
        "mimalloc"
    } else {
        "system"
    };
    let cases = [
        (Some("system"), "system"),
        (None, default),
        // Any other value, a longer one that starts the same included, is
        // not a request for the system allocator.
        (Some("mimalloc"), default),
        (Some("System"), default),
        (Some("system-default"), default),
        (Some(""), default),
    ];
    for (value, expected) in cases {
        let mut again = Command::new(std::env::current_exe().expect("this test's binary"));
        again
            .args([
                "--exact",
                "the_environment_chooses_the_allocator_at_startup",
                "--nocapture",
            ])
            .env(EXPECTED, expected)
            .env_remove("HOT_ALLOCATOR");
        if let Some(value) = value {
            again.env("HOT_ALLOCATOR", value);
        }
        let output = again.output().expect("the test binary runs");
        assert!(
            output.status.success(),
            "HOT_ALLOCATOR={value:?} should run on {expected}:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
