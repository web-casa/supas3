// SPDX-License-Identifier: AGPL-3.0-or-later
//! Exercises the backend the way a host application does: through `Core` and
//! the command functions, with events delivered to a sink.

use std::sync::Arc;
use std::time::Duration;
use supas3_core::commands::{jobs, objects, operations, profiles, transfer};
use supas3_core::credentials::{
    with_profile, Compatibility, CredentialType, ListVersion, Profile, Provider,
};
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
async fn a_second_backend_cannot_open_data_that_is_in_use() {
    let dir = tempfile::tempdir().unwrap();
    let (first_options, _events) = options(dir.path());
    let first = Core::init(first_options).await.unwrap();

    let (second_options, _events) = options(dir.path());
    let refused = Core::init(second_options).await;
    let message = refused
        .err()
        .expect("the second backend must be refused")
        .to_string();
    assert!(message.contains("already running"), "{message}");

    // The first backend is unaffected, and the data opens again once it ends.
    assert!(profiles::list_profiles(first.profiles())
        .await
        .unwrap()
        .is_empty());
    first.shutdown(Duration::from_secs(1)).await;
    // `first` is still alive here: shutdown itself must have released both
    // the instance lock and the saved-jobs lock.
    let (third_options, _events) = options(dir.path());
    let third = Core::init(third_options).await.unwrap();
    assert!(jobs::list_saved_jobs(third.jobs())
        .await
        .unwrap()
        .is_empty());
    // Shutting down twice is harmless.
    assert_eq!(first.shutdown(Duration::from_secs(1)).await, 0);
}

#[tokio::test]
async fn a_failed_start_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    // An unreadable profile store: a directory where the file belongs.
    let blocker = dir.path().join("profiles.json");
    std::fs::create_dir(&blocker).unwrap();

    let (failing, _events) = options(dir.path());
    assert!(Core::init(failing).await.is_err());

    // Once the cause is gone the same directory starts normally, which needs
    // the instance lock and every registration of the failed attempt released.
    std::fs::remove_dir(&blocker).unwrap();
    let (working, _events) = options(dir.path());
    let core = Core::init(working).await.unwrap();
    assert!(profiles::list_profiles(core.profiles())
        .await
        .unwrap()
        .is_empty());
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

#[tokio::test]
async fn a_pane_can_operate_on_a_connection_that_is_not_the_selected_one() {
    let dir = tempfile::tempdir().unwrap();
    let (options, _events) = options(dir.path());
    let core = Core::init(options).await.unwrap();
    let selected = profiles::add_profile(shared_profile(), core.profiles())
        .await
        .unwrap();
    let mut other = shared_profile();
    other.name = "other".into();
    let other = profiles::add_profile(other, core.profiles()).await.unwrap();
    profiles::set_active_profile(selected.id.clone(), core.profiles())
        .await
        .unwrap();
    let file = dir.path().join("payload.txt");
    std::fs::write(&file, b"payload").unwrap();
    let queue_for_other = || {
        transfer::queue_upload(
            "bucket".into(),
            None,
            "key".into(),
            file.to_string_lossy().into_owned(),
            0,
            other.id.clone(),
            core.app(),
            core.profiles(),
            core.s3(),
            core.transfers(),
        )
    };

    // Without a scope the command only accepts the selected connection.
    assert!(queue_for_other().await.is_err());

    let job_id = with_profile(other.id.clone(), queue_for_other())
        .await
        .unwrap();

    let job = transfer::list_transfers(core.transfers())
        .await
        .unwrap()
        .into_iter()
        .find(|job| job.id == job_id)
        .unwrap();
    assert_eq!(job.profile_id, other.id);
    assert_eq!(job.profile_identity, Some(other.cache_identity()));
    let active = profiles::get_active_profile(core.profiles())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        active.id, selected.id,
        "the global selection must not change"
    );
    core.shutdown(Duration::from_secs(2)).await;
}

async fn terminal_status(
    events: &mut mpsc::UnboundedReceiver<Event>,
    job_id: &str,
    names: &mut Vec<String>,
) -> serde_json::Value {
    // Large payloads over a slow link need far longer than small ones.
    let limit = std::env::var("SUPAS3_SMOKE_SIZE_MIB")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(90, |mib| 90 + mib * 10);
    tokio::time::timeout(Duration::from_secs(limit), async {
        loop {
            let Some(event) = events.recv().await else {
                return serde_json::Value::from("event sink closed");
            };
            names.push(event.name.clone());
            if event.name == "transfer-update" && event.payload["job_id"] == job_id {
                let status = event.payload["status"].clone();
                if status != "Pending" && status != "InProgress" {
                    return status;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| serde_json::Value::from(format!("no terminal event within {limit}s")))
}

/// The final status of a queued transfer, or why it was never queued.
async fn settled(
    queued: supas3_core::error::Result<String>,
    events: &mut mpsc::UnboundedReceiver<Event>,
) -> serde_json::Value {
    match queued {
        Ok(job_id) => terminal_status(events, &job_id, &mut Vec::new()).await,
        Err(error) => serde_json::Value::from(format!("not queued: {error}")),
    }
}

/// Drives the backend against a real S3-compatible bucket the way the
/// application will: upload, refuse to overwrite, page through a listing, and
/// delete in one batch.
///
/// Needs SUPAS3_SMOKE_ENDPOINT, SUPAS3_SMOKE_REGION, SUPAS3_SMOKE_BUCKET,
/// SUPAS3_SMOKE_ACCESS_KEY and SUPAS3_SMOKE_SECRET_KEY. SUPAS3_SMOKE_PROVIDER
/// selects the preset: `oss`, `cos`, `obs`, or `generic` (the default).
/// SUPAS3_SMOKE_SIZE_MIB sets the main payload size, to exercise multipart
/// upload. SUPAS3_SMOKE_LIST=v1 forces marker-based listing.
/// SUPAS3_SMOKE_BASIC=1 only reports the copy, edit and move results.
#[tokio::test]
#[ignore]
async fn real_bucket_upload_overwrite_protection_listing_and_batch_delete() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
    let bucket = var("SUPAS3_SMOKE_BUCKET");
    let region = var("SUPAS3_SMOKE_REGION");
    let provider = match std::env::var("SUPAS3_SMOKE_PROVIDER").as_deref() {
        Ok("oss") => Provider::Oss,
        Ok("cos") => Provider::Cos,
        Ok("obs") => Provider::Obs,
        _ => Provider::Generic,
    };
    let size_mib: usize = std::env::var("SUPAS3_SMOKE_SIZE_MIB")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let dir = tempfile::tempdir().unwrap();
    let (options, mut events) = options(dir.path());
    let core = Core::init(options).await.unwrap();

    let mut profile = Profile::new(
        "smoke".into(),
        CredentialType::CustomEndpoint {
            endpoint_url: var("SUPAS3_SMOKE_ENDPOINT"),
            access_key_id: var("SUPAS3_SMOKE_ACCESS_KEY"),
            secret_access_key: var("SUPAS3_SMOKE_SECRET_KEY"),
        },
        Some(region.clone()),
    );
    profile.compat = Compatibility::preset(provider);
    if std::env::var("SUPAS3_SMOKE_LIST").as_deref() == Ok("v1") {
        profile.compat.list_version = ListVersion::V1;
    }
    let list_version = profile.compat.list_version;
    let profile = profiles::add_profile(profile, core.profiles())
        .await
        .unwrap();
    profiles::set_active_profile(profile.id.clone(), core.profiles())
        .await
        .unwrap();

    let prefix = format!("supas3-core-smoke/{}/", uuid::Uuid::new_v4());
    let main_file = dir.path().join("main.bin");
    let main_payload = if size_mib == 0 {
        b"supas3 core smoke".to_vec()
    } else {
        vec![7u8; size_mib * 1024 * 1024]
    };
    std::fs::write(&main_file, &main_payload).unwrap();
    let small_file = dir.path().join("small.txt");
    std::fs::write(&small_file, b"x").unwrap();
    let names = ["main.bin", "中文 文件.txt", "z+&=.txt"];
    let keys: Vec<String> = names.iter().map(|name| format!("{prefix}{name}")).collect();

    let upload = |key: &str, file: &std::path::Path| {
        transfer::queue_upload(
            bucket.clone(),
            Some(region.clone()),
            key.to_string(),
            file.to_string_lossy().into_owned(),
            0,
            profile.id.clone(),
            core.app(),
            core.profiles(),
            core.s3(),
            core.transfers(),
        )
    };
    let first = settled(upload(&keys[0], &main_file).await, &mut events).await;
    let second = settled(upload(&keys[0], &main_file).await, &mut events).await;
    let extra_one = settled(upload(&keys[1], &small_file).await, &mut events).await;
    let extra_two = settled(upload(&keys[2], &small_file).await, &mut events).await;

    // Page through the listing one key at a time.
    let mut listed = Vec::new();
    let mut token = None;
    let mut listing_error = None;
    for _ in 0..10 {
        let page = objects::list_objects(
            bucket.clone(),
            Some(region.clone()),
            Some(prefix.clone()),
            Some("/".into()),
            token.clone(),
            Some(1),
            Some(true),
            None,
            None,
            profile.id.clone(),
            core.profiles(),
            core.s3(),
        )
        .await;
        match page {
            Ok(page) => {
                listed.extend(page.objects.into_iter().map(|object| object.key));
                token = page.next_continuation_token;
                if !page.is_truncated {
                    break;
                }
            }
            Err(error) => {
                listing_error = Some(error.to_string());
                break;
            }
        }
    }

    // Copy must create the destination once and refuse to replace it.
    let text_key = keys[1].clone();
    let copy_key = format!("{prefix}copy.txt");
    let copy = |destination: &str| {
        operations::copy_object(
            bucket.clone(),
            Some(region.clone()),
            text_key.clone(),
            bucket.clone(),
            Some(region.clone()),
            destination.to_string(),
            Some(profile.id.clone()),
            core.profiles(),
            core.s3(),
        )
    };
    let first_copy = copy(&copy_key).await.map_err(|e| e.to_string());
    let second_copy = copy(&copy_key).await.map_err(|e| e.to_string());

    // Editing: a save with the ETag that was read succeeds; a save with a
    // stale ETag is a conflict and must leave the newer content in place.
    let read = |key: String| async {
        objects::get_object_content(
            bucket.clone(),
            Some(region.clone()),
            key,
            None,
            profile.id.clone(),
            core.profiles(),
            core.s3(),
        )
        .await
        .map(|text| serde_json::to_value(text).unwrap())
        .map_err(|e| e.to_string())
    };
    let save = |content: &str, etag: String| {
        objects::put_object_content(
            bucket.clone(),
            Some(region.clone()),
            copy_key.clone(),
            content.to_string(),
            etag,
            profile.id.clone(),
            core.profiles(),
            core.s3(),
        )
    };
    let opened = read(copy_key.clone()).await;
    let opened_etag = opened
        .as_ref()
        .ok()
        .and_then(|text| text["e_tag"].as_str())
        .unwrap_or_default()
        .to_string();
    let fresh_save = save("edited once", opened_etag.clone())
        .await
        .map_err(|e| e.to_string());
    let stale_save = save("edited from a stale copy", opened_etag)
        .await
        .map_err(|e| e.to_string());
    let after_edits = read(copy_key.clone()).await;

    // A presigned link must fetch the content without credentials.
    let link = objects::get_presigned_url(
        bucket.clone(),
        Some(region.clone()),
        copy_key.clone(),
        300,
        profile.id.clone(),
        core.profiles(),
        core.s3(),
    )
    .await
    .map_err(|e| e.to_string());
    let fetched = match &link {
        Ok(url) => match reqwest::get(url).await {
            Ok(response) => response.text().await.map_err(|e| e.to_string()),
            Err(error) => Err(error.to_string()),
        },
        Err(error) => Err(error.clone()),
    };

    // Move: the destination appears and the source is removed.
    let moved_key = format!("{prefix}moved.txt");
    let moved = operations::move_object(
        bucket.clone(),
        Some(region.clone()),
        copy_key.clone(),
        bucket.clone(),
        Some(region.clone()),
        moved_key.clone(),
        Some(profile.id.clone()),
        core.profiles(),
        core.s3(),
    )
    .await
    .map_err(|e| e.to_string());
    let moved_content = read(moved_key.clone()).await;
    let source_after_move = read(copy_key.clone()).await;

    // Clean up before asserting, so a failed run leaves neither remote
    // objects nor a stored secret behind.
    let mut cleanup_keys = keys.clone();
    cleanup_keys.extend([copy_key.clone(), moved_key.clone()]);
    let removed = operations::delete_objects(
        bucket.clone(),
        Some(region.clone()),
        cleanup_keys,
        profile.id.clone(),
        core.profiles(),
        core.s3(),
    )
    .await;
    let secret_removed = profiles::delete_profile(profile.id.clone(), core.profiles()).await;
    let unfinished = core.shutdown(Duration::from_secs(5)).await;

    println!("provider={provider:?} list={list_version:?} first={first} second={second}");
    assert_eq!(first, "Completed");
    assert_ne!(
        second, "Completed",
        "the second upload overwrote the object"
    );
    assert!(
        second.get("Failed").is_some(),
        "unexpected second status: {second}"
    );
    assert_eq!(
        (extra_one, extra_two),
        ("Completed".into(), "Completed".into())
    );
    assert_eq!(listing_error, None);
    let mut expected = keys.clone();
    expected.sort();
    listed.sort();
    assert_eq!(listed, expected, "paged listing returned the wrong keys");
    // SUPAS3_SMOKE_BASIC=1 reports the copy, edit and move results without
    // asserting them, for services with known limitations there.
    if std::env::var("SUPAS3_SMOKE_BASIC").as_deref() == Ok("1") {
        println!("copy={first_copy:?} second_copy={second_copy:?}");
        println!("fresh_save={fresh_save:?} stale_save={stale_save:?}");
        println!("presigned={fetched:?} moved={moved:?}");
    } else {
        assert_eq!(first_copy, Ok(()));
        assert!(
            second_copy.is_err(),
            "the second copy replaced the destination"
        );
        assert_eq!(opened.map(|text| text["content"].clone()), Ok("x".into()));
        assert!(
            fresh_save.is_ok(),
            "saving with the current ETag failed: {fresh_save:?}"
        );
        assert!(stale_save.is_err(), "a stale save overwrote newer content");
        assert_eq!(
            after_edits.map(|text| text["content"].clone()),
            Ok("edited once".into())
        );
        assert_eq!(fetched, Ok("edited once".to_string()), "presigned link");
        assert_eq!(moved, Ok(()));
        assert_eq!(
            moved_content.map(|text| text["content"].clone()),
            Ok("edited once".into())
        );
        assert!(
            source_after_move.is_err(),
            "the move left its source behind"
        );
    }
    removed.unwrap();
    secret_removed.unwrap();
    assert_eq!(unfinished, 0);
}
