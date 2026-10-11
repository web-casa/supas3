// SPDX-License-Identifier: AGPL-3.0-or-later
//! Exercises the backend the way a host application does: through `Core` and
//! the command functions, with events delivered to a sink.

use std::sync::Arc;
use std::time::Duration;
use supas3_core::commands::{jobs, operations, profiles, transfer};
use supas3_core::credentials::{CredentialType, Profile};
use supas3_core::host::Event;
use supas3_core::{Core, CoreOptions};
use tokio::sync::mpsc;

fn options(dir: &std::path::Path) -> (CoreOptions, mpsc::UnboundedReceiver<Event>) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let options = CoreOptions {
        config_dir: dir.to_path_buf(),
        events: Arc::new(move |event| {
            let _ = sender.send(event);
        }),
    };
    (options, receiver)
}

#[tokio::test]
async fn init_starts_with_empty_state_in_the_given_directory() {
    let dir = tempfile::tempdir().unwrap();
    let (options, _events) = options(dir.path());

    let core = Core::init(options).await.unwrap();

    assert_eq!(core.data_dir(), dir.path());
    assert!(profiles::list_profiles(core.profiles())
        .await
        .unwrap()
        .is_empty());
    assert!(profiles::get_active_profile(core.profiles())
        .await
        .unwrap()
        .is_none());
    assert!(transfer::list_transfers(core.transfers())
        .await
        .unwrap()
        .is_empty());
}

fn shared_profile() -> Profile {
    // A profile type without a secret, so tests never write to the operating
    // system keychain of the machine running them.
    Profile::new(
        "shared".into(),
        CredentialType::SharedConfig {
            profile_name: Some("default".into()),
        },
        Some("us-east-1".into()),
    )
}

#[tokio::test]
async fn state_survives_shutdown_and_restart_in_the_same_directory() {
    let dir = tempfile::tempdir().unwrap();
    let saved = {
        let (options, _events) = options(dir.path());
        let core = Core::init(options).await.unwrap();
        let saved = profiles::add_profile(shared_profile(), core.profiles())
            .await
            .unwrap();
        assert_eq!(core.shutdown(Duration::from_secs(1)).await, 0);
        saved
    };

    let (options, _events) = options(dir.path());
    let core = Core::init(options).await.unwrap();
    let listed = profiles::list_profiles(core.profiles()).await.unwrap();

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, saved.id);
    // Saved jobs take an exclusive file lock; the first backend must have
    // released it, or this call reports the store as unavailable.
    assert!(jobs::list_saved_jobs(core.jobs()).await.unwrap().is_empty());
}

#[tokio::test]
async fn dropping_without_shutdown_still_releases_the_saved_jobs_lock() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (options, _events) = options(dir.path());
        let core = Core::init(options).await.unwrap();
        drop(core);
    }
    // The aborted scheduler task releases its state once it is next polled.
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (options, _events) = options(dir.path());
    let core = Core::init(options).await.unwrap();

    assert!(jobs::list_saved_jobs(core.jobs()).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_transfer_that_cannot_be_journaled_is_reported_to_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let (options, mut events) = options(dir.path());
    let core = Core::init(options).await.unwrap();
    let profile = profiles::add_profile(shared_profile(), core.profiles())
        .await
        .unwrap();
    profiles::set_active_profile(profile.id.clone(), core.profiles())
        .await
        .unwrap();

    // Make the journal impossible to replace: a non-empty directory in its place.
    let journal = dir.path().join("transfers.json");
    let _ = std::fs::remove_file(&journal);
    std::fs::create_dir(&journal).unwrap();
    std::fs::write(journal.join("blocker"), b"x").unwrap();
    let file = dir.path().join("payload.txt");
    std::fs::write(&file, b"payload").unwrap();

    let result = transfer::queue_upload(
        "bucket".into(),
        None,
        "key".into(),
        file.to_string_lossy().into_owned(),
        0,
        profile.id,
        core.app(),
        core.profiles(),
        core.s3(),
        core.transfers(),
    )
    .await;

    assert!(
        result.is_err(),
        "queueing must fail when the journal cannot be saved"
    );
    // The job is visible as failed, was never queued, and no request was started.
    let listed = transfer::list_transfers(core.transfers()).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(matches!(
        listed[0].status,
        supas3_core::transfer::TransferStatus::Failed(_)
    ));
    assert!(events.try_recv().is_err(), "no transfer event may be sent");
    assert_eq!(core.shutdown(Duration::from_secs(1)).await, 0);
}

/// Uploads through `Core` to a real S3-compatible bucket and waits for the
/// completion event. Needs SUPAS3_SMOKE_ENDPOINT, SUPAS3_SMOKE_REGION,
/// SUPAS3_SMOKE_BUCKET, SUPAS3_SMOKE_ACCESS_KEY and SUPAS3_SMOKE_SECRET_KEY,
/// and an endpoint that accepts path-style requests.
#[tokio::test]
#[ignore]
async fn upload_reports_completion_through_the_event_sink() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
    let bucket = var("SUPAS3_SMOKE_BUCKET");
    let region = var("SUPAS3_SMOKE_REGION");
    let dir = tempfile::tempdir().unwrap();
    let (options, mut events) = options(dir.path());
    let core = Core::init(options).await.unwrap();

    let profile = Profile::new(
        "smoke".into(),
        CredentialType::CustomEndpoint {
            endpoint_url: var("SUPAS3_SMOKE_ENDPOINT"),
            access_key_id: var("SUPAS3_SMOKE_ACCESS_KEY"),
            secret_access_key: var("SUPAS3_SMOKE_SECRET_KEY"),
        },
        Some(region.clone()),
    );
    let profile = profiles::add_profile(profile, core.profiles())
        .await
        .unwrap();
    profiles::set_active_profile(profile.id.clone(), core.profiles())
        .await
        .unwrap();

    let file = dir.path().join("payload.txt");
    std::fs::write(&file, b"supas3 core smoke").unwrap();
    let key = format!("supas3-core-smoke/{}.txt", uuid::Uuid::new_v4());
    let job_id = transfer::queue_upload(
        bucket.clone(),
        Some(region.clone()),
        key.clone(),
        file.to_string_lossy().into_owned(),
        0,
        profile.id.clone(),
        core.app(),
        core.profiles(),
        core.s3(),
        core.transfers(),
    )
    .await
    .unwrap();

    let mut names = Vec::new();
    let status = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let Some(event) = events.recv().await else {
                return serde_json::Value::from("event sink closed");
            };
            names.push(event.name.clone());
            if event.name == "transfer-update" && event.payload["job_id"] == job_id.as_str() {
                let status = event.payload["status"].clone();
                if status != "Pending" && status != "InProgress" {
                    return status;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| serde_json::Value::from("no terminal event within 60s"));

    // Clean up before asserting, so a failed run leaves neither a remote
    // object nor a stored secret behind.
    let object_removed = operations::delete_object(
        bucket,
        Some(region),
        key,
        profile.id.clone(),
        core.profiles(),
        core.s3(),
    )
    .await;
    let secret_removed = profiles::delete_profile(profile.id, core.profiles()).await;
    let unfinished = core.shutdown(Duration::from_secs(5)).await;

    assert_eq!(status, "Completed", "events seen: {names:?}");
    assert!(names.iter().any(|name| name == "transfer-added"));
    object_removed.unwrap();
    secret_removed.unwrap();
    assert_eq!(unfinished, 0);
}
