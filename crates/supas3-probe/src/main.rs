// SPDX-License-Identifier: AGPL-3.0-or-later
//! Headless S3 compatibility probe. Builds the client the same way the
//! imported Brows3 backend does and records how each provider answers the
//! calls supaS3 depends on. Reads credentials from `~/.env`; never prints them.

use aws_sdk_s3::config::{Region, RequestChecksumCalculation, ResponseChecksumValidation};
use aws_sdk_s3::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    CompletedMultipartUpload, CompletedPart, Delete, EncodingType, ObjectIdentifier, Tag, Tagging,
};
use aws_sdk_s3::Client;
use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

struct Target {
    name: String,
    endpoint: String,
    region: String,
    bucket: String,
    ak: String,
    sk: String,
}

/// Targets live in an untracked file so bucket names and account ids stay out
/// of the repository. One target per line:
/// `name endpoint region bucket access_key_var secret_key_var`
fn load_targets() -> Vec<Target> {
    let path =
        std::env::var("SUPAS3_PROBE_TARGETS").unwrap_or_else(|_| "probe-targets.local".into());
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}\nsee crates/supas3-probe/probe-targets.example");
        std::process::exit(2);
    });
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() == 6).then(|| Target {
                name: f[0].into(),
                endpoint: f[1].into(),
                region: f[2].into(),
                bucket: f[3].into(),
                ak: f[4].into(),
                sk: f[5].into(),
            })
        })
        .collect()
}

fn load_env() -> HashMap<String, String> {
    let path = format!("{}/.env", std::env::var("HOME").unwrap_or_default());
    let mut map = HashMap::new();
    for line in std::fs::read_to_string(path).unwrap_or_default().lines() {
        let line = line.trim().trim_start_matches("export ");
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
            map.insert(k.trim().to_string(), v.to_string());
        }
    }
    map
}

async fn client(t: &Target, env: &HashMap<String, String>, path_style: bool) -> Client {
    let creds =
        aws_credential_types::Credentials::new(&env[&t.ak], &env[&t.sk], None, None, "probe");
    let sdk = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(Region::new(t.region.clone()))
        .credentials_provider(creds)
        .load()
        .await;
    let cfg = aws_sdk_s3::config::Builder::from(&sdk)
        .endpoint_url(&t.endpoint)
        .force_path_style(path_style)
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
        .build();
    Client::from_conf(cfg)
}

fn outcome<T, E>(r: &Result<T, SdkError<E>>) -> String
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
{
    match r {
        Ok(_) => "OK".into(),
        Err(e) => {
            let status = e.raw_response().map(|r| r.status().as_u16());
            match (status, e.code()) {
                (None, None) => {
                    let mut s = format!("{}", DisplayErrorContext(e));
                    s.truncate(160);
                    format!("ERR {s}")
                }
                (st, code) => {
                    let mut msg = e.message().unwrap_or("").to_string();
                    msg.truncate(110);
                    format!(
                        "ERR http={} code={} {}",
                        st.map(|s| s.to_string()).unwrap_or("-".into()),
                        code.unwrap_or("-"),
                        msg
                    )
                }
            }
        }
    }
}

struct Report<'a>(&'a str);
impl Report<'_> {
    fn row(&self, test: &str, result: impl AsRef<str>) {
        println!("{:<11}| {:<44}| {}", self.0, test, result.as_ref());
    }
    /// For calls that a conforming server must reject with 412.
    fn expect_412<T, E>(&self, test: &str, r: &Result<T, SdkError<E>>)
    where
        E: ProvideErrorMetadata + std::error::Error + 'static,
    {
        let verdict = match r {
            Ok(_) => "CONDITION IGNORED (request succeeded)".to_string(),
            Err(e) if e.raw_response().map(|r| r.status().as_u16()) == Some(412) => {
                "OK rejected 412".to_string()
            }
            Err(_) => format!("OTHER {}", outcome(r)),
        };
        self.row(test, verdict);
    }
}

fn enc(key: &str) -> String {
    key.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Decodes an `encoding-type=url` listing key. A literal `+` is always sent as
/// `%2B`, so a bare `+` can only mean a space.
fn dec(key: &str) -> String {
    let b = key.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Adds the Content-MD5 header that OSS and COS still require on DeleteObjects.
fn with_md5(req: &mut aws_sdk_s3::config::http::HttpRequest) {
    use base64::Engine;
    use md5::Digest;
    if let Some(body) = req.body().bytes() {
        let digest = base64::engine::general_purpose::STANDARD.encode(md5::Md5::digest(body));
        req.headers_mut().insert("content-md5", digest);
    }
}

async fn round2(t: &Target, env: &HashMap<String, String>, run_id: &str) {
    let rep = Report(&t.name);
    let c = client(t, env, false).await;
    let b = t.bucket.as_str();
    let pfx = format!("supas3-probe/{run_id}-r2/");
    let k = |s: &str| format!("{pfx}{s}");
    for n in ["d1", "d2", "d3", "keep"] {
        let _ = c
            .put_object()
            .bucket(b)
            .key(k(n))
            .body(ByteStream::from_static(b"v1"))
            .send()
            .await;
    }

    // Batch delete with an explicit Content-MD5.
    let ids: Vec<_> = ["d1", "d2", "d3"]
        .iter()
        .map(|n| ObjectIdentifier::builder().key(k(n)).build().unwrap())
        .collect();
    let r = c
        .delete_objects()
        .bucket(b)
        .delete(Delete::builder().set_objects(Some(ids)).build().unwrap())
        .customize()
        .mutate_request(with_md5)
        .send()
        .await;
    rep.row(
        "DeleteObjects + Content-MD5",
        match &r {
            Ok(o) => format!(
                "OK deleted={} errors={}",
                o.deleted().len(),
                o.errors().len()
            ),
            Err(_) => outcome(&r),
        },
    );

    // Vendor no-overwrite headers as a substitute for If-None-Match:*.
    for h in ["x-oss-forbid-overwrite", "x-cos-forbid-overwrite"] {
        let r = c
            .put_object()
            .bucket(b)
            .key(k("keep"))
            .body(ByteStream::from_static(b"v2"))
            .customize()
            .mutate_request(move |req| {
                req.headers_mut().insert(h, "true");
            })
            .send()
            .await;
        let now = match c.get_object().bucket(b).key(k("keep")).send().await {
            Ok(o) => o
                .body
                .collect()
                .await
                .map(|d| String::from_utf8_lossy(&d.into_bytes()).to_string())
                .unwrap_or_default(),
            Err(_) => "?".into(),
        };
        rep.row(
            &format!("PutObject {h} on existing"),
            format!("{} / content now {now:?}", outcome(&r)),
        );
        let _ = c
            .put_object()
            .bucket(b)
            .key(k("keep"))
            .body(ByteStream::from_static(b"v1"))
            .send()
            .await;
        let r = c
            .put_object()
            .bucket(b)
            .key(k(&format!("new-{h}")))
            .body(ByteStream::from_static(b"v1"))
            .customize()
            .mutate_request(move |req| {
                req.headers_mut().insert(h, "true");
            })
            .send()
            .await;
        rep.row(&format!("PutObject {h} on new key"), outcome(&r));
        let r = c
            .copy_object()
            .bucket(b)
            .key(k("keep"))
            .copy_source(format!("{b}/{}", enc(&k(&format!("new-{h}")))))
            .customize()
            .mutate_request(move |req| {
                req.headers_mut().insert(h, "true");
            })
            .send()
            .await;
        rep.row(&format!("CopyObject {h} on existing"), outcome(&r));
    }

    // Vendor no-overwrite headers on CompleteMultipartUpload.
    for h in ["x-oss-forbid-overwrite", "x-cos-forbid-overwrite"] {
        for (label, target) in [("existing", k("keep")), ("new key", k(&format!("mpu-{h}")))] {
            let Ok(created) = c
                .create_multipart_upload()
                .bucket(b)
                .key(&target)
                .send()
                .await
            else {
                rep.row(
                    &format!("CompleteMultipartUpload {h} on {label}"),
                    "could not create upload",
                );
                continue;
            };
            let upload_id = created.upload_id().unwrap_or_default().to_string();
            let part = c
                .upload_part()
                .bucket(b)
                .key(&target)
                .upload_id(&upload_id)
                .part_number(1)
                .body(ByteStream::from_static(b"multipart"))
                .send()
                .await;
            let parts = vec![CompletedPart::builder()
                .part_number(1)
                .set_e_tag(part.ok().and_then(|p| p.e_tag().map(str::to_string)))
                .build()];
            let r = c
                .complete_multipart_upload()
                .bucket(b)
                .key(&target)
                .upload_id(&upload_id)
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(parts))
                        .build(),
                )
                .customize()
                .mutate_request(move |req| {
                    req.headers_mut().insert(h, "true");
                })
                .send()
                .await;
            rep.row(
                &format!("CompleteMultipartUpload {h} on {label}"),
                outcome(&r),
            );
            let _ = c
                .abort_multipart_upload()
                .bucket(b)
                .key(&target)
                .upload_id(&upload_id)
                .send()
                .await;
        }
        let _ = c
            .put_object()
            .bucket(b)
            .key(k("keep"))
            .body(ByteStream::from_static(b"v1"))
            .send()
            .await;
    }

    // If-Match round trip with the server's own ETag, repeated to rule out flukes.
    let put = c
        .put_object()
        .bucket(b)
        .key(k("etag"))
        .body(ByteStream::from_static(b"abc"))
        .send()
        .await;
    let put_etag = put
        .as_ref()
        .ok()
        .and_then(|o| o.e_tag().map(str::to_string));
    let head = c.head_object().bucket(b).key(k("etag")).send().await.ok();
    let head_etag = head.as_ref().and_then(|o| o.e_tag().map(str::to_string));
    rep.row(
        "ETag of 'abc' (md5 900150983cd2...)",
        format!(
            "put={put_etag:?} head={head_etag:?} sse={:?}",
            head.as_ref()
                .and_then(|o| o.server_side_encryption().map(|s| s.as_str().to_string()))
        ),
    );
    let r = c
        .put_object()
        .bucket(b)
        .key(k("etag"))
        .set_if_match(head_etag.clone())
        .body(ByteStream::from_static(b"abd"))
        .send()
        .await;
    rep.row("PutObject If-Match = HEAD etag", outcome(&r));
    let r = c
        .get_object()
        .bucket(b)
        .key(k("etag"))
        .set_if_match(head_etag)
        .send()
        .await;
    rep.row("GetObject If-Match = HEAD etag", outcome(&r));

    // What happens if the region reported by GetBucketLocation is used for signing.
    if let Ok(o) = c.get_bucket_location().bucket(b).send().await {
        if let Some(loc) = o.location_constraint().map(|l| l.as_str().to_string()) {
            let creds = aws_credential_types::Credentials::new(
                &env[&t.ak],
                &env[&t.sk],
                None,
                None,
                "probe",
            );
            let sdk = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(Region::new(loc.clone()))
                .credentials_provider(creds)
                .load()
                .await;
            let c2 = Client::from_conf(
                aws_sdk_s3::config::Builder::from(&sdk)
                    .endpoint_url(&t.endpoint)
                    .build(),
            );
            let r = c2.list_objects_v2().bucket(b).max_keys(1).send().await;
            rep.row(
                &format!("sign with GetBucketLocation value {loc:?}"),
                outcome(&r),
            );
        }
    }
    let creds =
        aws_credential_types::Credentials::new(&env[&t.ak], &env[&t.sk], None, None, "probe");
    let sdk = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(creds)
        .load()
        .await;
    let c3 = Client::from_conf(
        aws_sdk_s3::config::Builder::from(&sdk)
            .endpoint_url(&t.endpoint)
            .build(),
    );
    let r = c3.list_objects_v2().bucket(b).max_keys(1).send().await;
    rep.row("sign with region us-east-1", outcome(&r));
    // Default SDK checksum behaviour (what happens without WhenRequired).
    let r = c3
        .put_object()
        .bucket(b)
        .key(k("default-checksum"))
        .body(ByteStream::from_static(b"x"))
        .send()
        .await;
    rep.row("PutObject with SDK default checksums", outcome(&r));

    for _ in 0..2 {
        if let Ok(o) = c.list_objects().bucket(b).prefix(&pfx).send().await {
            for x in o.contents() {
                let _ = c
                    .delete_object()
                    .bucket(b)
                    .key(x.key().unwrap_or_default())
                    .send()
                    .await;
            }
        }
    }
    let left = c
        .list_objects()
        .bucket(b)
        .prefix(&pfx)
        .send()
        .await
        .map(|o| o.contents().len())
        .unwrap_or(0);
    rep.row(
        "cleanup",
        if left == 0 {
            "OK prefix empty".to_string()
        } else {
            format!("{left} entries remain under {pfx}")
        },
    );
}

async fn run(t: &Target, env: &HashMap<String, String>, run_id: &str) {
    let rep = Report(&t.name);
    if env.get(&t.ak).is_none_or(|v| v.is_empty()) {
        rep.row("credentials", "MISSING, skipped");
        return;
    }
    let b = t.bucket.as_str();

    // --- addressing
    let mut chosen = None;
    for (label, path) in [("virtual", false), ("path", true)] {
        let c = client(t, env, path).await;
        let r = c.list_objects_v2().bucket(b).max_keys(1).send().await;
        rep.row(&format!("addressing {label}: ListObjectsV2"), outcome(&r));
        let r1 = c.list_objects().bucket(b).max_keys(1).send().await;
        rep.row(&format!("addressing {label}: ListObjects v1"), outcome(&r1));
        if chosen.is_none() && (r.is_ok() || r1.is_ok()) {
            chosen = Some((label, c));
        }
    }
    let Some((style, c)) = chosen else {
        rep.row("suite", "SKIPPED, no addressing style worked");
        return;
    };
    rep.row("suite runs with", style);

    let pfx = format!("supas3-probe/{run_id}/");
    let k = |s: &str| format!("{pfx}{s}");

    // --- account-level
    let r = c.list_buckets().send().await;
    rep.row(
        "ListBuckets",
        match &r {
            Ok(o) => format!("OK {} buckets", o.buckets().len()),
            Err(_) => outcome(&r),
        },
    );
    let r = c.get_bucket_location().bucket(b).send().await;
    rep.row(
        "GetBucketLocation",
        match &r {
            Ok(o) => format!("OK {:?}", o.location_constraint().map(|l| l.as_str())),
            Err(_) => outcome(&r),
        },
    );

    // --- basic put/get/head
    let r = c
        .put_object()
        .bucket(b)
        .key(k("zero"))
        .body(ByteStream::from_static(b""))
        .send()
        .await;
    rep.row("PutObject zero-byte", outcome(&r));
    let r = c
        .put_object()
        .bucket(b)
        .key(k("small.txt"))
        .body(ByteStream::from_static(b"hello supas3"))
        .content_type("text/plain")
        .send()
        .await;
    let small_etag = r
        .as_ref()
        .ok()
        .and_then(|o| o.e_tag().map(str::to_string))
        .unwrap_or_default();
    // md5("hello supas3") lets us see ETag casing/quoting.
    rep.row(
        "PutObject small",
        format!("{} etag={small_etag}", outcome(&r)),
    );
    let r = c.head_object().bucket(b).key(k("small.txt")).send().await;
    rep.row(
        "HeadObject",
        match &r {
            Ok(o) => format!(
                "OK len={:?} etag={:?} type={:?}",
                o.content_length(),
                o.e_tag(),
                o.content_type()
            ),
            Err(_) => outcome(&r),
        },
    );
    let r = c
        .get_object()
        .bucket(b)
        .key(k("small.txt"))
        .range("bytes=0-4")
        .send()
        .await;
    let body = match r {
        Ok(o) => o
            .body
            .collect()
            .await
            .map(|d| String::from_utf8_lossy(&d.into_bytes()).to_string())
            .unwrap_or_default(),
        Err(e) => outcome::<(), _>(&Err(e)),
    };
    rep.row("GetObject range 0-4", format!("{body:?}"));

    // --- streaming upload from a file (aws-chunked risk)
    let tmp = std::env::temp_dir().join(format!("supas3-probe-{run_id}.bin"));
    std::fs::write(&tmp, vec![7u8; 6 * 1024 * 1024]).unwrap();
    let r = match ByteStream::from_path(&tmp).await {
        Ok(bs) => {
            c.put_object()
                .bucket(b)
                .key(k("stream.bin"))
                .body(bs)
                .send()
                .await
        }
        Err(e) => {
            rep.row("PutObject streaming 6MiB", format!("local error {e}"));
            return;
        }
    };
    rep.row("PutObject streaming 6MiB", outcome(&r));
    let _ = std::fs::remove_file(&tmp);

    // --- listing
    let names = [
        "a.txt",
        "b 空格.txt",
        "中文/文件.txt",
        "sp+ec&ial=%25#.txt",
        "dir/x",
        "dir/y",
        "e",
    ];
    let mut put_ok = 0;
    for n in names {
        let r = c
            .put_object()
            .bucket(b)
            .key(k(&format!("list/{n}")))
            .body(ByteStream::from_static(b"x"))
            .send()
            .await;
        if r.is_ok() {
            put_ok += 1
        } else {
            rep.row(&format!("PutObject key {n:?}"), outcome(&r));
        }
    }
    rep.row(
        "PutObject unicode/special keys",
        format!("{put_ok}/{} ok", names.len()),
    );
    let expected: BTreeSet<String> = names.iter().map(|n| k(&format!("list/{n}"))).collect();
    let lp = k("list/");

    for url_enc in [false, true] {
        let label = if url_enc {
            "ListObjectsV2 paged, encoding=url"
        } else {
            "ListObjectsV2 paged"
        };
        let (mut seen, mut token, mut pages, mut err) = (BTreeSet::new(), None::<String>, 0, None);
        loop {
            let mut q = c
                .list_objects_v2()
                .bucket(b)
                .prefix(&lp)
                .max_keys(2)
                .set_continuation_token(token.clone());
            if url_enc {
                q = q.encoding_type(EncodingType::Url);
            }
            let r = q.send().await;
            match &r {
                Ok(o) => {
                    pages += 1;
                    for obj in o.contents() {
                        let key = obj.key().unwrap_or_default();
                        seen.insert(if url_enc { dec(key) } else { key.to_string() });
                    }
                    token = o.next_continuation_token().map(str::to_string);
                    if o.is_truncated() != Some(true) {
                        break;
                    }
                    if token.is_none() {
                        err = Some("truncated but no NextContinuationToken".to_string());
                        break;
                    }
                    if pages > 20 {
                        err = Some("pagination did not terminate".into());
                        break;
                    }
                }
                Err(_) => {
                    err = Some(outcome(&r));
                    break;
                }
            }
        }
        rep.row(
            label,
            match err {
                Some(e) => format!("FAIL {e} (pages={pages}, keys={})", seen.len()),
                None if seen == expected => {
                    format!("OK {} keys in {pages} pages, exact match", seen.len())
                }
                None => format!(
                    "MISMATCH got {} keys in {pages} pages: {:?}",
                    seen.len(),
                    seen.symmetric_difference(&expected).collect::<Vec<_>>()
                ),
            },
        );
    }
    {
        let (mut seen, mut marker, mut pages, mut err) = (BTreeSet::new(), None::<String>, 0, None);
        loop {
            let r = c
                .list_objects()
                .bucket(b)
                .prefix(&lp)
                .max_keys(2)
                .set_marker(marker.clone())
                .send()
                .await;
            match &r {
                Ok(o) => {
                    pages += 1;
                    for obj in o.contents() {
                        seen.insert(obj.key().unwrap_or_default().to_string());
                    }
                    if o.is_truncated() != Some(true) {
                        break;
                    }
                    marker = o.next_marker().map(str::to_string).or_else(|| {
                        o.contents()
                            .last()
                            .and_then(|x| x.key().map(str::to_string))
                    });
                    if pages > 20 {
                        err = Some("pagination did not terminate".to_string());
                        break;
                    }
                }
                Err(_) => {
                    err = Some(outcome(&r));
                    break;
                }
            }
        }
        rep.row(
            "ListObjects v1 paged (marker)",
            match err {
                Some(e) => format!("FAIL {e}"),
                None if seen == expected => {
                    format!("OK {} keys in {pages} pages, exact match", seen.len())
                }
                None => format!(
                    "MISMATCH got {} keys: {:?}",
                    seen.len(),
                    seen.symmetric_difference(&expected).collect::<Vec<_>>()
                ),
            },
        );
    }
    let r = c
        .list_objects_v2()
        .bucket(b)
        .prefix(&lp)
        .delimiter("/")
        .send()
        .await;
    rep.row(
        "ListObjectsV2 delimiter",
        match &r {
            Ok(o) => format!(
                "OK files={} prefixes={:?}",
                o.contents().len(),
                o.common_prefixes()
                    .iter()
                    .filter_map(|p| p.prefix())
                    .map(|p| p.trim_start_matches(&lp).to_string())
                    .collect::<Vec<_>>()
            ),
            Err(_) => outcome(&r),
        },
    );

    // --- conditional writes
    let r = c
        .put_object()
        .bucket(b)
        .key(k("small.txt"))
        .if_none_match("*")
        .body(ByteStream::from_static(b"overwrite"))
        .send()
        .await;
    rep.expect_412("PutObject If-None-Match:* on existing", &r);
    let r = c
        .put_object()
        .bucket(b)
        .key(k("cond-new.txt"))
        .if_none_match("*")
        .body(ByteStream::from_static(b"new"))
        .send()
        .await;
    rep.row("PutObject If-None-Match:* on new key", outcome(&r));
    let r = c
        .put_object()
        .bucket(b)
        .key(k("small.txt"))
        .if_match("\"00000000000000000000000000000000\"")
        .body(ByteStream::from_static(b"overwrite"))
        .send()
        .await;
    rep.expect_412("PutObject If-Match wrong etag", &r);
    let cur = c
        .head_object()
        .bucket(b)
        .key(k("small.txt"))
        .send()
        .await
        .ok()
        .and_then(|o| o.e_tag().map(str::to_string));
    rep.row(
        "  object after rejected puts",
        if cur.as_deref() == Some(small_etag.as_str()) {
            "unchanged"
        } else {
            "CHANGED (overwritten)"
        },
    );
    let r = c
        .put_object()
        .bucket(b)
        .key(k("small.txt"))
        .set_if_match(cur.clone())
        .body(ByteStream::from_static(b"hello supas3"))
        .send()
        .await;
    rep.row("PutObject If-Match current etag", outcome(&r));
    let r = c
        .get_object()
        .bucket(b)
        .key(k("small.txt"))
        .if_match("\"00000000000000000000000000000000\"")
        .send()
        .await;
    rep.expect_412("GetObject If-Match wrong etag", &r);

    // --- copy
    let src = format!("{b}/{}", enc(&k("small.txt")));
    let r = c
        .copy_object()
        .bucket(b)
        .key(k("copy.txt"))
        .copy_source(&src)
        .if_none_match("*")
        .send()
        .await;
    rep.row("CopyObject If-None-Match:* to new key", outcome(&r));
    let r = c
        .copy_object()
        .bucket(b)
        .key(k("copy.txt"))
        .copy_source(&src)
        .if_none_match("*")
        .send()
        .await;
    rep.expect_412("CopyObject If-None-Match:* on existing", &r);
    let r = c
        .copy_object()
        .bucket(b)
        .key(k("copy2.txt"))
        .copy_source(&src)
        .copy_source_if_match("\"00000000000000000000000000000000\"")
        .send()
        .await;
    rep.expect_412("CopyObject copy-source-if-match wrong", &r);
    let r = c
        .copy_object()
        .bucket(b)
        .key(k("copy-plain.txt"))
        .copy_source(&src)
        .send()
        .await;
    rep.row("CopyObject unconditional", outcome(&r));
    let r = c
        .copy_object()
        .bucket(b)
        .key(k("copy-cn.txt"))
        .copy_source(format!("{b}/{}", enc(&k("list/中文/文件.txt"))))
        .send()
        .await;
    rep.row("CopyObject from unicode key", outcome(&r));

    // --- multipart
    let mk = k("multipart.bin");
    let r = c.create_multipart_upload().bucket(b).key(&mk).send().await;
    rep.row("CreateMultipartUpload", outcome(&r));
    if let Ok(o) = r {
        let uid = o.upload_id().unwrap_or_default().to_string();
        let mut parts = vec![];
        for (n, size) in [(1, 5 * 1024 * 1024), (2, 1024)] {
            let r = c
                .upload_part()
                .bucket(b)
                .key(&mk)
                .upload_id(&uid)
                .part_number(n)
                .body(ByteStream::from(vec![n as u8; size]))
                .send()
                .await;
            if let Ok(p) = &r {
                parts.push(
                    CompletedPart::builder()
                        .part_number(n)
                        .set_e_tag(p.e_tag().map(str::to_string))
                        .build(),
                );
            }
            rep.row(&format!("UploadPart {n}"), outcome(&r));
        }
        let r = c
            .list_parts()
            .bucket(b)
            .key(&mk)
            .upload_id(&uid)
            .send()
            .await;
        rep.row(
            "ListParts",
            match &r {
                Ok(o) => format!("OK {} parts", o.parts().len()),
                Err(_) => outcome(&r),
            },
        );
        let r = c
            .complete_multipart_upload()
            .bucket(b)
            .key(&mk)
            .upload_id(&uid)
            .if_none_match("*")
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            )
            .send()
            .await;
        rep.row(
            "CompleteMultipartUpload If-None-Match:*",
            format!(
                "{} etag={:?}",
                outcome(&r),
                r.as_ref().ok().and_then(|o| o.e_tag())
            ),
        );
        if r.is_err() {
            let _ = c
                .abort_multipart_upload()
                .bucket(b)
                .key(&mk)
                .upload_id(&uid)
                .send()
                .await;
        }
    }
    // Completing onto an existing key with If-None-Match must fail.
    if let Ok(o) = c
        .create_multipart_upload()
        .bucket(b)
        .key(k("small.txt"))
        .send()
        .await
    {
        let uid = o.upload_id().unwrap_or_default().to_string();
        if let Ok(p) = c
            .upload_part()
            .bucket(b)
            .key(k("small.txt"))
            .upload_id(&uid)
            .part_number(1)
            .body(ByteStream::from_static(b"mp-overwrite"))
            .send()
            .await
        {
            let parts = vec![CompletedPart::builder()
                .part_number(1)
                .set_e_tag(p.e_tag().map(str::to_string))
                .build()];
            let r = c
                .complete_multipart_upload()
                .bucket(b)
                .key(k("small.txt"))
                .upload_id(&uid)
                .if_none_match("*")
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(parts))
                        .build(),
                )
                .send()
                .await;
            rep.expect_412("CompleteMultipartUpload INM:* on existing", &r);
        }
        let r = c
            .abort_multipart_upload()
            .bucket(b)
            .key(k("small.txt"))
            .upload_id(&uid)
            .send()
            .await;
        rep.row("AbortMultipartUpload", outcome(&r));
    }
    let r = c
        .upload_part_copy()
        .bucket(b)
        .key(k("upc.bin"))
        .upload_id("x")
        .part_number(1)
        .copy_source(format!("{b}/{}", enc(&mk)))
        .send()
        .await;
    rep.row("UploadPartCopy (bogus upload id)", outcome(&r));
    let r = c
        .list_multipart_uploads()
        .bucket(b)
        .prefix(&pfx)
        .send()
        .await;
    rep.row(
        "ListMultipartUploads",
        match &r {
            Ok(o) => format!("OK {} pending", o.uploads().len()),
            Err(_) => outcome(&r),
        },
    );

    // --- tagging / acl
    let r = c
        .put_object_tagging()
        .bucket(b)
        .key(k("small.txt"))
        .tagging(
            Tagging::builder()
                .tag_set(Tag::builder().key("probe").value("1").build().unwrap())
                .build()
                .unwrap(),
        )
        .send()
        .await;
    rep.row("PutObjectTagging", outcome(&r));
    let r = c
        .get_object_tagging()
        .bucket(b)
        .key(k("small.txt"))
        .send()
        .await;
    rep.row(
        "GetObjectTagging",
        match &r {
            Ok(o) => format!("OK {} tags", o.tag_set().len()),
            Err(_) => outcome(&r),
        },
    );
    let r = c
        .get_object_acl()
        .bucket(b)
        .key(k("small.txt"))
        .send()
        .await;
    rep.row(
        "GetObjectAcl",
        match &r {
            Ok(o) => format!("OK {} grants", o.grants().len()),
            Err(_) => outcome(&r),
        },
    );

    // --- presign
    match c
        .get_object()
        .bucket(b)
        .key(k("small.txt"))
        .presigned(PresigningConfig::expires_in(Duration::from_secs(300)).unwrap())
        .await
    {
        Ok(p) => {
            let want = match c.get_object().bucket(b).key(k("small.txt")).send().await {
                Ok(o) => o
                    .body
                    .collect()
                    .await
                    .map(|d| String::from_utf8_lossy(&d.into_bytes()).to_string())
                    .unwrap_or_default(),
                Err(_) => String::new(),
            };
            let res = reqwest::get(p.uri()).await;
            rep.row(
                "Presigned GET fetched over HTTP",
                match res {
                    Ok(r) => {
                        let st = r.status();
                        let body = r.text().await.unwrap_or_default();
                        if body == want {
                            format!("OK {st}, content matches")
                        } else {
                            format!("FAIL {st}, body {} bytes", body.len())
                        }
                    }
                    Err(e) => format!("FAIL {e}"),
                },
            );
        }
        Err(e) => rep.row("Presigned GET", format!("sign error {e}")),
    }

    // --- versions
    let r = c.get_bucket_versioning().bucket(b).send().await;
    rep.row(
        "GetBucketVersioning",
        match &r {
            Ok(o) => format!("OK status={:?}", o.status().map(|s| s.as_str())),
            Err(_) => outcome(&r),
        },
    );
    let r = c
        .list_object_versions()
        .bucket(b)
        .prefix(k("small.txt"))
        .max_keys(10)
        .send()
        .await;
    rep.row(
        "ListObjectVersions",
        match &r {
            Ok(o) => format!(
                "OK versions={} delete_markers={}",
                o.versions().len(),
                o.delete_markers().len()
            ),
            Err(_) => outcome(&r),
        },
    );

    // --- deletes
    let r = c
        .delete_object()
        .bucket(b)
        .key(k("small.txt"))
        .if_match("\"00000000000000000000000000000000\"")
        .send()
        .await;
    rep.expect_412("DeleteObject If-Match wrong etag", &r);
    let still = c
        .head_object()
        .bucket(b)
        .key(k("small.txt"))
        .send()
        .await
        .is_ok();
    rep.row(
        "  object after rejected delete",
        if still {
            "still exists"
        } else {
            "GONE (deleted)"
        },
    );
    let ids: Vec<_> = ["copy.txt", "copy-plain.txt", "cond-new.txt"]
        .iter()
        .map(|n| ObjectIdentifier::builder().key(k(n)).build().unwrap())
        .collect();
    let r = c
        .delete_objects()
        .bucket(b)
        .delete(Delete::builder().set_objects(Some(ids)).build().unwrap())
        .send()
        .await;
    rep.row(
        "DeleteObjects batch of 3",
        match &r {
            Ok(o) => format!(
                "OK deleted={} errors={}",
                o.deleted().len(),
                o.errors().len()
            ),
            Err(_) => outcome(&r),
        },
    );

    // --- cleanup: everything under this run's prefix, including versions when present
    let mut left = 0;
    for _ in 0..3 {
        let mut keys = vec![];
        if let Ok(o) = c.list_objects().bucket(b).prefix(&pfx).send().await {
            keys.extend(
                o.contents()
                    .iter()
                    .filter_map(|x| x.key().map(|s| (s.to_string(), None))),
            );
        }
        if let Ok(o) = c.list_object_versions().bucket(b).prefix(&pfx).send().await {
            keys.extend(o.versions().iter().filter_map(|v| {
                v.key()
                    .map(|s| (s.to_string(), v.version_id().map(str::to_string)))
            }));
            keys.extend(o.delete_markers().iter().filter_map(|v| {
                v.key()
                    .map(|s| (s.to_string(), v.version_id().map(str::to_string)))
            }));
        }
        left = keys.len();
        if keys.is_empty() {
            break;
        }
        for (key, vid) in keys {
            let vid = vid.filter(|v| v != "null");
            let _ = c
                .delete_object()
                .bucket(b)
                .key(key)
                .set_version_id(vid)
                .send()
                .await;
        }
    }
    if let Ok(o) = c
        .list_multipart_uploads()
        .bucket(b)
        .prefix(&pfx)
        .send()
        .await
    {
        for u in o.uploads() {
            let _ = c
                .abort_multipart_upload()
                .bucket(b)
                .key(u.key().unwrap_or_default())
                .upload_id(u.upload_id().unwrap_or_default())
                .send()
                .await;
        }
    }
    rep.row(
        "cleanup",
        if left == 0 {
            "OK prefix empty".to_string()
        } else {
            format!("{left} entries may remain under {pfx}")
        },
    );
}

/// Fills `supas3-demo/` with folders and `count` small objects, so the
/// interface has something to browse and page through. Existing objects with
/// the same keys are replaced; nothing else in the bucket is touched.
/// Write conditions and ACLs on one target, in detail: what an object's ACL
/// says, which headers a HEAD returns, and whether `If-None-Match: *` and
/// `If-Match` are honoured by PutObject, CopyObject and
/// CompleteMultipartUpload, on a new and on an existing key.
async fn conditions(t: &Target, env: &HashMap<String, String>, run_id: &str) {
    let rep = Report(&t.name);
    let c = client(t, env, false).await;
    let base = format!("supas3-probe/{run_id}/cond/");
    let key = |name: &str| format!("{base}{name}");
    let put = |k: String, body: &'static str| {
        c.put_object()
            .bucket(&t.bucket)
            .key(k)
            .body(ByteStream::from_static(body.as_bytes()))
    };

    let first = put(key("a.txt"), "first").send().await;
    rep.row("put a.txt", outcome(&first));
    let etag = first
        .as_ref()
        .ok()
        .and_then(|o| o.e_tag().map(str::to_string));
    rep.row("put ETag", format!("{etag:?}"));

    // What a HEAD says about encryption and checksums.
    let head = c
        .head_object()
        .bucket(&t.bucket)
        .key(key("a.txt"))
        .send()
        .await;
    match &head {
        Ok(h) => {
            rep.row("head ETag", format!("{:?}", h.e_tag()));
            rep.row("head SSE", format!("{:?}", h.server_side_encryption()));
            rep.row(
                "head SSE-KMS key",
                format!("{:?}", h.ssekms_key_id().map(|k| k.len())),
            );
            rep.row("head content-length", format!("{:?}", h.content_length()));
            rep.row("head checksum crc32", format!("{:?}", h.checksum_crc32()));
        }
        Err(_) => rep.row("head", outcome(&head)),
    }

    // The ACL as the SDK reads it.
    let parsed = c
        .get_object_acl()
        .bucket(&t.bucket)
        .key(key("a.txt"))
        .send()
        .await;
    match &parsed {
        Ok(o) => {
            rep.row(
                "acl owner id",
                format!(
                    "{:?}",
                    o.owner().and_then(|o| o.id()).map(|id| if id.is_empty() {
                        "empty"
                    } else {
                        "present"
                    })
                ),
            );
            for g in o.grants() {
                rep.row(
                    "acl grant",
                    format!(
                        "id={} email={:?} name={} type={:?} uri={:?} perm={:?}",
                        g.grantee().and_then(|g| g.id()).is_some(),
                        g.grantee().and_then(|g| g.email_address()),
                        g.grantee().and_then(|g| g.display_name()).is_some(),
                        g.grantee().map(|g| g.r#type().as_str().to_string()),
                        g.grantee().and_then(|g| g.uri()),
                        g.permission().map(|p| p.as_str().to_string())
                    ),
                );
            }
        }
        Err(_) => rep.row("acl parsed", outcome(&parsed)),
    }

    // PutObject.
    let r = put(key("a.txt"), "second").if_none_match("*").send().await;
    rep.expect_412("put If-None-Match on existing", &r);
    let r = put(key("new.txt"), "new").if_none_match("*").send().await;
    rep.row("put If-None-Match on new", outcome(&r));
    let r = put(key("a.txt"), "third")
        .set_if_match(etag.clone())
        .send()
        .await;
    rep.row("put If-Match current ETag", outcome(&r));
    let etag = match &r {
        Ok(o) => o.e_tag().map(str::to_string),
        Err(_) => etag,
    };
    let r = put(key("a.txt"), "fourth")
        .if_match("\"00000000000000000000000000000000\"")
        .send()
        .await;
    rep.expect_412("put If-Match wrong ETag", &r);

    // CopyObject.
    let source = format!("{}/{}", t.bucket, enc(&key("new.txt")));
    let r = c
        .copy_object()
        .bucket(&t.bucket)
        .key(key("a.txt"))
        .copy_source(&source)
        .if_none_match("*")
        .send()
        .await;
    rep.expect_412("copy If-None-Match on existing", &r);
    let r = c
        .copy_object()
        .bucket(&t.bucket)
        .key(key("copied.txt"))
        .copy_source(&source)
        .if_none_match("*")
        .send()
        .await;
    rep.row("copy If-None-Match on new", outcome(&r));
    let r = c
        .copy_object()
        .bucket(&t.bucket)
        .key(key("a.txt"))
        .copy_source(&source)
        .if_match("\"00000000000000000000000000000000\"")
        .send()
        .await;
    rep.expect_412("copy If-Match wrong ETag", &r);
    let r = c
        .copy_object()
        .bucket(&t.bucket)
        .key(key("a.txt"))
        .copy_source(&source)
        .set_if_match(
            // The copies before may have rewritten it; ask for its ETag now.
            c.head_object()
                .bucket(&t.bucket)
                .key(key("a.txt"))
                .send()
                .await
                .ok()
                .and_then(|h| h.e_tag().map(str::to_string))
                .or(etag.clone()),
        )
        .send()
        .await;
    rep.row("copy If-Match current ETag", outcome(&r));

    // CompleteMultipartUpload, one part.
    for (target, label) in [("a.txt", "existing"), ("multi.txt", "new")] {
        let created = c
            .create_multipart_upload()
            .bucket(&t.bucket)
            .key(key(target))
            .send()
            .await;
        let Some(upload) = created
            .as_ref()
            .ok()
            .and_then(|o| o.upload_id().map(str::to_string))
        else {
            rep.row("multipart create", outcome(&created));
            continue;
        };
        let part = c
            .upload_part()
            .bucket(&t.bucket)
            .key(key(target))
            .upload_id(&upload)
            .part_number(1)
            .body(ByteStream::from_static(b"part"))
            .send()
            .await;
        let part_etag = part
            .as_ref()
            .ok()
            .and_then(|o| o.e_tag().map(str::to_string));
        let done = c
            .complete_multipart_upload()
            .bucket(&t.bucket)
            .key(key(target))
            .upload_id(&upload)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .parts(
                        CompletedPart::builder()
                            .part_number(1)
                            .set_e_tag(part_etag)
                            .build(),
                    )
                    .build(),
            )
            .if_none_match("*")
            .send()
            .await;
        if label == "existing" {
            rep.expect_412("complete If-None-Match on existing", &done);
        } else {
            rep.row("complete If-None-Match on new", outcome(&done));
        }
        if done.is_err() {
            let _ = c
                .abort_multipart_upload()
                .bucket(&t.bucket)
                .key(key(target))
                .upload_id(&upload)
                .send()
                .await;
        }
    }

    // User metadata survives a HEAD.
    let r = put(key("meta.txt"), "meta")
        .metadata("supas3-sha256", "abc")
        .send()
        .await;
    rep.row("put with metadata", outcome(&r));
    let h = c
        .head_object()
        .bucket(&t.bucket)
        .key(key("meta.txt"))
        .send()
        .await;
    rep.row(
        "head metadata",
        format!("{:?}", h.as_ref().ok().and_then(|h| h.metadata().cloned())),
    );

    // Clean up.
    for name in ["a.txt", "new.txt", "copied.txt", "multi.txt", "meta.txt"] {
        let _ = c
            .delete_object()
            .bucket(&t.bucket)
            .key(key(name))
            .send()
            .await;
    }
}

async fn seed(t: &Target, env: &HashMap<String, String>, count: usize) {
    let rep = Report(&t.name);
    let c = client(t, env, false).await;
    let b = t.bucket.as_str();
    let mut keys: Vec<String> = [
        "supas3-demo/读我.txt",
        "supas3-demo/照片/2026/海边.jpg",
        "supas3-demo/照片/2026/山 路.jpg",
        "supas3-demo/docs/report final.pdf",
        "supas3-demo/docs/notes.md",
        "supas3-demo/empty-folder/",
    ]
    .iter()
    .map(|key| key.to_string())
    .collect();
    keys.extend((0..count).map(|index| format!("supas3-demo/many/item-{index:04}.txt")));
    let mut failed = 0;
    for chunk in keys.chunks(16) {
        let results = futures_join(chunk.iter().map(|key| {
            let body = if key.ends_with('/') {
                ByteStream::from_static(b"")
            } else {
                ByteStream::from(key.clone().into_bytes())
            };
            c.put_object().bucket(b).key(key).body(body).send()
        }))
        .await;
        failed += results.iter().filter(|result| result.is_err()).count();
    }
    rep.row(
        "seed supas3-demo/",
        format!("{} objects written, {failed} failed", keys.len() - failed),
    );
}

/// Awaits a batch of requests concurrently.
/// What resuming needs: ranged GETs (with and without If-Match, and past the
/// end), ListParts with paging, and ListParts of an aborted upload.
async fn resume(t: &Target, env: &HashMap<String, String>, run_id: &str) {
    let rep = Report(&t.name);
    let c = client(t, env, false).await;
    let key = format!("supas3-probe/{run_id}/resume/r.bin");
    let body: Vec<u8> = (0..6 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let size = body.len() as i64;
    let put = c
        .put_object()
        .bucket(&t.bucket)
        .key(&key)
        .body(ByteStream::from(body.clone()))
        .send()
        .await;
    rep.row("put 6 MiB", outcome(&put));
    let etag = put.ok().and_then(|o| o.e_tag().map(str::to_string));
    let ranged = |if_match: Option<String>, from: i64| {
        c.get_object()
            .bucket(&t.bucket)
            .key(&key)
            .range(format!("bytes={from}-"))
            .set_if_match(if_match)
            .send()
    };
    for (label, if_match, from) in [
        ("range 1000-", None, 1000),
        ("range 1000- If-Match right ETag", etag.clone(), 1000),
        (
            "range 1000- If-Match wrong ETag",
            Some("\"0000\"".to_string()),
            1000,
        ),
        ("range from the end", None, size),
    ] {
        match ranged(if_match, from).await {
            Ok(o) => {
                let range = o.content_range().map(str::to_string);
                let same = o.e_tag().map(str::to_string) == etag;
                let first = o.body.collect().await.map(|b| b.into_bytes());
                let right = first.as_ref().is_ok_and(|b| {
                    b.len() as i64 == size - from && b.first() == body.get(from as usize)
                });
                rep.row(
                    label,
                    format!("OK range={range:?} same_etag={same} bytes_right={right}"),
                );
            }
            Err(e) => rep.row(label, outcome::<(), _>(&Err(e))),
        }
    }

    // Two parts, then ListParts one per page.
    let mp_key = format!("supas3-probe/{run_id}/resume/mp.bin");
    let created = c
        .create_multipart_upload()
        .bucket(&t.bucket)
        .key(&mp_key)
        .send()
        .await;
    rep.row("create multipart", outcome(&created));
    let Some(upload_id) = created.ok().and_then(|o| o.upload_id().map(str::to_string)) else {
        return;
    };
    let mut parts = Vec::new();
    for (number, range) in [(1, 0..5 * 1024 * 1024), (2, 5 * 1024 * 1024..body.len())] {
        let sent = c
            .upload_part()
            .bucket(&t.bucket)
            .key(&mp_key)
            .upload_id(&upload_id)
            .part_number(number)
            .body(ByteStream::from(body[range].to_vec()))
            .send()
            .await;
        rep.row(&format!("upload part {number}"), outcome(&sent));
        if let Ok(o) = sent {
            parts.push((number, o.e_tag().map(str::to_string)));
        }
    }
    let mut marker: Option<String> = None;
    let mut listed = Vec::new();
    for page in 1..=3 {
        let answer = c
            .list_parts()
            .bucket(&t.bucket)
            .key(&mp_key)
            .upload_id(&upload_id)
            .max_parts(1)
            .set_part_number_marker(marker.clone())
            .send()
            .await;
        match answer {
            Ok(o) => {
                for p in o.parts() {
                    listed.push((p.part_number(), p.e_tag().map(str::to_string), p.size()));
                }
                rep.row(
                    &format!("list parts page {page}"),
                    format!(
                        "OK parts={} truncated={:?} next={:?}",
                        o.parts().len(),
                        o.is_truncated(),
                        o.next_part_number_marker()
                    ),
                );
                if o.is_truncated() != Some(true) {
                    break;
                }
                marker = o.next_part_number_marker().map(str::to_string);
            }
            Err(e) => {
                rep.row(
                    &format!("list parts page {page}"),
                    outcome::<(), _>(&Err(e)),
                );
                break;
            }
        }
    }
    let agree = listed.len() == parts.len()
        && listed
            .iter()
            .zip(&parts)
            .all(|((n, e, _), (m, f))| *n == Some(*m) && e == f);
    rep.row("listed parts match the sent ones", agree.to_string());
    let completed = c
        .complete_multipart_upload()
        .bucket(&t.bucket)
        .key(&mp_key)
        .upload_id(&upload_id)
        .multipart_upload(
            CompletedMultipartUpload::builder()
                .set_parts(Some(
                    parts
                        .iter()
                        .map(|(n, e)| {
                            CompletedPart::builder()
                                .part_number(*n)
                                .set_e_tag(e.clone())
                                .build()
                        })
                        .collect(),
                ))
                .build(),
        )
        .send()
        .await;
    rep.row("complete multipart", outcome(&completed));
    let mp_etag = completed.ok().and_then(|o| o.e_tag().map(str::to_string));
    match c
        .get_object()
        .bucket(&t.bucket)
        .key(&mp_key)
        .range("bytes=6000000-")
        .send()
        .await
    {
        Ok(o) => rep.row(
            "range on the multipart object",
            format!(
                "OK range={:?} same_etag={}",
                o.content_range(),
                o.e_tag().map(str::to_string) == mp_etag
            ),
        ),
        Err(e) => rep.row("range on the multipart object", outcome::<(), _>(&Err(e))),
    }

    // An aborted upload, then ListParts of it.
    if let Ok(o) = c
        .create_multipart_upload()
        .bucket(&t.bucket)
        .key(&mp_key)
        .send()
        .await
    {
        let id = o.upload_id().unwrap_or_default().to_string();
        let aborted = c
            .abort_multipart_upload()
            .bucket(&t.bucket)
            .key(&mp_key)
            .upload_id(&id)
            .send()
            .await;
        rep.row("abort multipart", outcome(&aborted));
        let after = c
            .list_parts()
            .bucket(&t.bucket)
            .key(&mp_key)
            .upload_id(&id)
            .send()
            .await;
        rep.row("list parts after abort", outcome(&after));
    }
    for k in [&key, &mp_key] {
        let _ = c.delete_object().bucket(&t.bucket).key(k).send().await;
    }
}

async fn futures_join<F: std::future::Future>(futures: impl Iterator<Item = F>) -> Vec<F::Output> {
    let handles: Vec<_> = futures.map(Box::pin).collect();
    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        results.push(handle.await);
    }
    results
}

#[tokio::main]
async fn main() {
    let env = load_env();
    let mut only: Vec<String> = std::env::args().skip(1).collect();
    let second = only.iter().any(|a| a == "--round2");
    only.retain(|a| a != "--round2");
    let detailed = only.iter().any(|a| a == "--conditions");
    only.retain(|a| a != "--conditions");
    let resuming = only.iter().any(|a| a == "--resume");
    only.retain(|a| a != "--resume");
    // `--seed N target...` writes demo data instead of probing.
    let seed_count = only.iter().position(|a| a == "--seed").map(|index| {
        only.remove(index);
        only.remove(index)
            .parse::<usize>()
            .expect("--seed needs a count")
    });
    let run_id = format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );
    println!("{:<11}| {:<44}| result", "target", "test");
    for t in &load_targets() {
        if only.is_empty() || only.contains(&t.name) {
            if let Some(count) = seed_count {
                seed(t, &env, count).await;
            } else if resuming {
                resume(t, &env, &run_id).await
            } else if detailed {
                conditions(t, &env, &run_id).await
            } else if second {
                round2(t, &env, &run_id).await
            } else {
                run(t, &env, &run_id).await
            }
        }
    }
}
