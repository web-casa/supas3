// SPDX-License-Identifier: AGPL-3.0-or-later
//! Runs an operation against a named connection instead of the globally
//! selected one.
//!
//! The imported commands were written for one window showing one connection:
//! each resolves "the active profile" and then checks it against the profile
//! the caller expected. supaS3 shows two panes that can use different
//! connections at once, so a single global selection cannot serve both.
//!
//! [`with_profile`] sets, for the duration of one operation, which profile
//! that lookup returns. It is scoped to the calling task, so two panes can run
//! operations on different connections concurrently without changing the
//! global selection and without touching the command bodies.

use std::future::Future;

tokio::task_local! {
    static OPERATION_PROFILE: String;
}

/// Runs `operation` with `profile_id` as the active profile for every command
/// it calls directly. Pass the same id as the command's `expected_profile_id`.
///
/// Tasks spawned by the operation do not inherit the scope; queued transfers
/// carry their own profile id instead.
pub async fn with_profile<F: Future>(profile_id: impl Into<String>, operation: F) -> F::Output {
    OPERATION_PROFILE.scope(profile_id.into(), operation).await
}

/// The profile id set by an enclosing [`with_profile`], if any.
pub(crate) fn current() -> Option<String> {
    OPERATION_PROFILE.try_with(Clone::clone).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_scope_is_visible_only_inside_the_operation() {
        assert_eq!(current(), None);
        let inside = with_profile("a", async { current() }).await;
        assert_eq!(inside.as_deref(), Some("a"));
        assert_eq!(current(), None);
    }

    #[tokio::test]
    async fn concurrent_operations_each_see_their_own_profile() {
        let observe = |id: &'static str| {
            with_profile(id, async move {
                let mut seen = Vec::new();
                for _ in 0..50 {
                    tokio::task::yield_now().await;
                    seen.push(current());
                }
                seen.into_iter()
                    .all(|profile| profile.as_deref() == Some(id))
            })
        };
        let (left, right) = tokio::join!(
            tokio::spawn(observe("left")),
            tokio::spawn(observe("right"))
        );
        assert!(left.unwrap() && right.unwrap());
    }

    #[tokio::test]
    async fn spawned_tasks_do_not_inherit_the_scope() {
        let spawned = with_profile("a", async {
            tokio::spawn(async { current() }).await.unwrap()
        })
        .await;
        assert_eq!(spawned, None);
    }
}
