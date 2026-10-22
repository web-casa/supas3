// SPDX-License-Identifier: AGPL-3.0-or-later
//! Retrying, on its own, a transfer the network cut short: which failures
//! qualify, how long to wait, and which transfers are left alone because
//! another part of the app waits on them.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// How many times a transfer is retried on its own.
pub const MAX_ATTEMPTS: u32 = 3;

/// Whether a failure is the network's or a busy service's, which a later
/// attempt may get past. Anything that says the request was understood and
/// refused is not.
pub fn transient(message: &str) -> bool {
    let text = message.to_ascii_lowercase();
    let refused = [
        "precondition",
        "alreadyexists",
        "already exists",
        "accessdenied",
        "access denied",
        "forbidden",
        "nosuchbucket",
        "nosuchkey",
        "checksum",
        "changed",
        "cancel",
        "invalid",
        "signaturedoesnotmatch",
        // This computer's own trouble, which waiting does not mend.
        "no space left",
        "disk full",
        "permission denied",
        "read-only file system",
        "no such file or directory",
        "cannot read upload file",
    ];
    if refused.iter().any(|word| text.contains(word))
        || ["412", "409", "403", "404"]
            .iter()
            .any(|code| has_code(&text, code))
    {
        return false;
    }
    let passing = [
        "dispatch failure",
        "timed out",
        "timeout",
        "connection reset",
        "connection refused",
        "connection closed",
        "broken pipe",
        "no response",
        "io error",
        "network",
        "dns",
        // The SDK's protection against a stalled stream, and the
        // transfer's own.
        "stalled",
        "throughput",
        "streaming error",
        "slowdown",
        "requesttimeout",
        "internalerror",
        "serviceunavailable",
    ];
    passing.iter().any(|word| text.contains(word))
        || ["500", "502", "503", "504"]
            .iter()
            .any(|code| has_code(&text, code))
}

/// Whether `code` stands in `text` as a status, as in "HTTP 503" or
/// "StatusCode(409)", not as part of a size, a path or a name.
fn has_code(text: &str, code: &str) -> bool {
    text.match_indices(code).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + code.len()..].chars().next();
        before.is_none_or(|c| c == ' ' || c == '(' || c == ':' || c == '=')
            && after.is_none_or(|c| c == ' ' || c == ')' || c == ',' || c == ':' || c == '.')
    })
}

/// How long to wait before attempt `attempt` (from 1), spread by `jitter`
/// in [0, 1) so transfers cut off together do not all come back at once.
pub fn delay(attempt: u32, jitter: f64) -> Duration {
    let base: f64 = match attempt {
        0 | 1 => 5.0,
        2 => 30.0,
        _ => 120.0,
    };
    Duration::from_secs_f64(base * (0.8 + 0.4 * jitter.clamp(0.0, 1.0)))
}

fn awaited() -> &'static Mutex<HashSet<String>> {
    static AWAITED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    AWAITED.get_or_init(Default::default)
}

/// Notes that the app waits on a transfer itself (a move), so it is never
/// retried behind its back.
pub fn awaited_by_app(id: &str) {
    if let Ok(mut set) = awaited().lock() {
        set.insert(id.to_string());
    }
}

pub fn is_awaited_by_app(id: &str) -> bool {
    awaited().lock().is_ok_and(|set| set.contains(id))
}

fn claimed() -> &'static Mutex<HashSet<String>> {
    static CLAIMED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    CLAIMED.get_or_init(Default::default)
}

/// Takes a failed transfer for one retry, by hand or on its own. False when
/// it is already taken, so the two never retry the same transfer twice.
pub fn claim(id: &str) -> bool {
    claimed()
        .lock()
        .is_ok_and(|mut set| set.insert(id.to_string()))
}

/// Gives a failed transfer back when its retry did not happen.
pub fn release(id: &str) {
    if let Ok(mut set) = claimed().lock() {
        set.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::{claim, delay, release, transient};

    #[test]
    fn network_failures_are_retried_and_refusals_are_not() {
        for message in [
            "S3 error: dispatch failure: io error: connection reset by peer",
            "Upload timed out after 600s",
            "service error: unhandled error (SlowDown)",
            "HTTP 503 Service Unavailable",
            "No response from the service",
            "S3 error: streaming error: minimum throughput was specified at 1 B/s, but throughput of 0 B/s was observed",
            "Download stalled for 60 seconds. Check the connection and retry.",
        ] {
            assert!(transient(message), "{message}");
        }
        for message in [
            "PreconditionFailed: At least one of the pre-conditions you specified did not hold",
            "FileAlreadyExists",
            "AccessDenied: Access Denied",
            "The download destination already exists",
            "The object changed since the transfer started",
            "Cancelled by the user",
            "Something unexplained",
            "IO error: No space left on device (os error 28)",
            "Cannot read upload file '/home/a/2500/x.bin': Permission denied",
            "The key photos/5003.jpg failed: unexplained",
        ] {
            assert!(!transient(message), "{message}");
        }
        // A number in a name says nothing either way.
        assert!(transient("dispatch failure: io error for logs/404/a.txt"));
    }

    #[test]
    fn a_transfer_is_retried_by_one_hand() {
        assert!(claim("t-claimed"));
        assert!(!claim("t-claimed"), "taken while the first retry runs");
        release("t-claimed");
        assert!(claim("t-claimed"));
        release("t-claimed");
    }

    #[test]
    fn waits_grow_and_spread() {
        assert!(delay(1, 0.0).as_secs_f64() >= 4.0 && delay(1, 0.99).as_secs_f64() <= 6.0);
        assert!(delay(2, 0.5) > delay(1, 0.5));
        assert!(delay(3, 0.5) > delay(2, 0.5));
        assert_eq!(delay(9, 0.5), delay(3, 0.5));
    }
}
