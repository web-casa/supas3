use crate::credentials::ListVersion;
use crate::error::{AppError, Result};
use aws_sdk_s3::config::Intercept;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::list_objects::ListObjectsError;
use aws_sdk_s3::operation::list_objects_v2::builders::ListObjectsV2FluentBuilder;
use aws_sdk_s3::operation::list_objects_v2::{ListObjectsV2Error, ListObjectsV2Output};
use aws_sdk_s3::{types::Object, Client};
use std::collections::HashSet;

/// Bounded preflight: callers must finish enumeration before mutating anything.
pub async fn list_recursive(client: &Client, bucket: &str, prefix: &str) -> Result<Vec<Object>> {
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        let mut objects = Vec::new();
        let mut token = None;
        let mut seen = HashSet::new();
        for _ in 0..100 {
            let page = send(
                client,
                client
                    .list_objects_v2()
                    .bucket(bucket)
                    .prefix(prefix)
                    .max_keys(1000)
                    .set_continuation_token(token),
            )
            .await
            .map_err(|error| AppError::S3Error(error.to_string()))?;
            objects.extend_from_slice(page.contents());
            if objects.len() > 100_000 {
                return Err(AppError::ConfigError(
                    "Folder exceeds 100,000 objects. Select a smaller prefix.".into(),
                ));
            }
            if !page.is_truncated().unwrap_or(false) {
                return Ok(objects);
            }
            let next = page
                .next_continuation_token()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    AppError::S3Error("Incomplete listing: missing continuation token".into())
                })?;
            if !seen.insert(next.to_string()) {
                return Err(AppError::S3Error(
                    "Incomplete listing: repeated continuation token".into(),
                ));
            }
            token = Some(next.to_string());
        }
        Err(AppError::ConfigError(
            "Folder exceeds 100 listing pages. Select a smaller prefix.".into(),
        ))
    })
    .await
    .map_err(|_| {
        AppError::S3Error("Folder listing timed out; nothing was queued or changed.".into())
    })?
}

/// Marks a client whose service has no ListObjectsV2 (Huawei OBS documents
/// only marker paging). It changes no request; [`send`] reads it back.
#[derive(Debug)]
pub(crate) struct ListVersionMarker;

impl ListVersionMarker {
    const NAME: &'static str = "supas3-list-version:v1";
}

impl Intercept for ListVersionMarker {
    fn name(&self) -> &'static str {
        Self::NAME
    }
}

pub(crate) fn list_version_of(client: &Client) -> ListVersion {
    if client
        .config()
        .interceptors()
        .any(|interceptor| interceptor.name() == ListVersionMarker::NAME)
    {
        ListVersion::V1
    } else {
        ListVersion::V2
    }
}

/// Sends a listing request built for ListObjectsV2, using ListObjects (V1)
/// instead on clients marked for it. The V1 answer is returned in V2 shape so
/// every caller pages the same way: its marker travels as the continuation
/// token.
pub(crate) async fn send(
    client: &Client,
    request: ListObjectsV2FluentBuilder,
) -> std::result::Result<ListObjectsV2Output, SdkError<ListObjectsV2Error>> {
    if list_version_of(client) == ListVersion::V2 {
        return request.send().await;
    }
    let input = request.as_input();
    let marker = input
        .get_continuation_token()
        .clone()
        .or_else(|| input.get_start_after().clone());
    let output = client
        .list_objects()
        .set_bucket(input.get_bucket().clone())
        .set_prefix(input.get_prefix().clone())
        .set_delimiter(input.get_delimiter().clone())
        .set_max_keys(*input.get_max_keys())
        .set_encoding_type(input.get_encoding_type().clone())
        .set_marker(marker)
        .send()
        .await
        .map_err(|error| {
            error.map_service_error(|service| match service {
                ListObjectsError::NoSuchBucket(inner) => ListObjectsV2Error::NoSuchBucket(inner),
                // `generic` keeps the service's code, message and request id.
                other => ListObjectsV2Error::generic(other.meta().clone()),
            })
        })?;

    let truncated = output.is_truncated().unwrap_or(false);
    let url_encoded = output.encoding_type() == Some(&aws_sdk_s3::types::EncodingType::Url);
    let next = truncated
        .then(|| next_marker(&output, url_encoded))
        .flatten();
    let count = output.contents().len() + output.common_prefixes().len();
    Ok(ListObjectsV2Output::builder()
        .set_name(output.name().map(str::to_string))
        .set_prefix(output.prefix().map(str::to_string))
        .set_delimiter(output.delimiter().map(str::to_string))
        .set_max_keys(output.max_keys())
        .set_encoding_type(output.encoding_type().cloned())
        .set_is_truncated(output.is_truncated())
        .set_next_continuation_token(next)
        .key_count(i32::try_from(count).unwrap_or(i32::MAX))
        .set_common_prefixes(Some(output.common_prefixes().to_vec()))
        .set_contents(Some(output.contents().to_vec()))
        .build())
}

/// Where the next V1 page starts. Services return `NextMarker` only for
/// delimited listings; otherwise it is the last entry of this page, which may
/// be an object key or a common prefix, whichever sorts later.
///
/// With `encoding-type=url` the response carries encoded names, but a marker
/// is sent as a plain key, so names are decoded before they are compared and
/// returned.
fn next_marker(
    output: &aws_sdk_s3::operation::list_objects::ListObjectsOutput,
    url_encoded: bool,
) -> Option<String> {
    let plain = |name: &str| {
        if url_encoded {
            decode_listing_name(name)
        } else {
            name.to_string()
        }
    };
    if let Some(marker) = output.next_marker().filter(|marker| !marker.is_empty()) {
        return Some(plain(marker));
    }
    let last_key = output
        .contents()
        .last()
        .and_then(|object| object.key())
        .map(plain);
    let last_prefix = output
        .common_prefixes()
        .last()
        .and_then(|prefix| prefix.prefix())
        .map(plain);
    last_key.max(last_prefix)
}

/// Decodes a name from a URL-encoded listing. A literal `+` is always sent as
/// `%2B`, so a bare `+` (which OSS uses) can only mean a space.
fn decode_listing_name(name: &str) -> String {
    let spaced = name.replace('+', " ");
    urlencoding::decode(&spaced)
        .map(|decoded| decoded.into_owned())
        .unwrap_or(spaced)
}

#[cfg(test)]
mod adapter_tests {
    use super::{list_version_of, send, ListVersionMarker};
    use crate::commands::test_s3::{response, scripted_endpoint};
    use crate::credentials::ListVersion;
    use aws_sdk_s3::Client;

    async fn scripted(
        v1: bool,
        responses: Vec<String>,
    ) -> (Client, tokio::task::JoinHandle<Vec<String>>) {
        let (endpoint, server) = scripted_endpoint(responses).await;
        let mut config = aws_sdk_s3::config::Builder::new()
            .behavior_version_latest()
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "TEST", "secret", None, None, "test",
            ))
            .endpoint_url(endpoint)
            .force_path_style(true)
            .retry_config(aws_sdk_s3::config::retry::RetryConfig::standard().with_max_attempts(1));
        if v1 {
            config = config.interceptor(ListVersionMarker);
        }
        (Client::from_conf(config.build()), server)
    }

    fn request_line(request: &str) -> &str {
        request.lines().next().unwrap_or_default()
    }

    #[tokio::test]
    async fn clients_use_v2_unless_marked() {
        let (v2, _) = scripted(false, vec![]).await;
        let (v1, _) = scripted(true, vec![]).await;
        assert_eq!(list_version_of(&v2), ListVersion::V2);
        assert_eq!(list_version_of(&v1), ListVersion::V1);
    }

    #[tokio::test]
    async fn v2_clients_send_the_request_unchanged() {
        let body = "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>t2</NextContinuationToken><Contents><Key>a</Key></Contents></ListBucketResult>";
        let (client, server) = scripted(false, vec![response(200, "", body)]).await;
        let page = send(
            &client,
            client
                .list_objects_v2()
                .bucket("b")
                .continuation_token("t1"),
        )
        .await
        .unwrap();
        let requests = server.await.unwrap();
        assert!(request_line(&requests[0]).contains("list-type=2"));
        assert!(request_line(&requests[0]).contains("continuation-token=t1"));
        assert_eq!(page.next_continuation_token(), Some("t2"));
    }

    #[tokio::test]
    async fn v1_clients_page_with_markers_and_report_them_as_tokens() {
        let body = "<ListBucketResult><Name>b</Name><Prefix>p/</Prefix><Delimiter>/</Delimiter><MaxKeys>2</MaxKeys><IsTruncated>true</IsTruncated><NextMarker>p/dir/</NextMarker><Contents><Key>p/a</Key><Size>3</Size></Contents><CommonPrefixes><Prefix>p/dir/</Prefix></CommonPrefixes></ListBucketResult>";
        let (client, server) = scripted(true, vec![response(200, "", body)]).await;
        let page = send(
            &client,
            client
                .list_objects_v2()
                .bucket("b")
                .prefix("p/")
                .delimiter("/")
                .max_keys(2)
                .continuation_token("p/0"),
        )
        .await
        .unwrap();

        let requests = server.await.unwrap();
        let line = request_line(&requests[0]);
        assert!(!line.contains("list-type"), "{line}");
        assert!(!line.contains("continuation-token"), "{line}");
        assert!(line.contains("marker=p%2F0"), "{line}");
        assert!(
            line.contains("prefix=p%2F")
                && line.contains("delimiter=%2F")
                && line.contains("max-keys=2"),
            "{line}"
        );
        assert_eq!(page.is_truncated(), Some(true));
        assert_eq!(page.next_continuation_token(), Some("p/dir/"));
        assert_eq!(page.contents().len(), 1);
        assert_eq!(page.contents()[0].key(), Some("p/a"));
        assert_eq!(page.common_prefixes()[0].prefix(), Some("p/dir/"));
        assert_eq!(page.key_count(), Some(2));
    }

    #[tokio::test]
    async fn v1_pages_without_next_marker_continue_after_the_last_entry() {
        // Undelimited listings carry no NextMarker: continue after the last key.
        let keys_only = "<ListBucketResult><IsTruncated>true</IsTruncated><Contents><Key>a</Key></Contents><Contents><Key>b</Key></Contents></ListBucketResult>";
        let (client, _) = scripted(true, vec![response(200, "", keys_only)]).await;
        let page = send(&client, client.list_objects_v2().bucket("b"))
            .await
            .unwrap();
        assert_eq!(page.next_continuation_token(), Some("b"));

        // A page can end in a common prefix that sorts after its last key.
        let mixed = "<ListBucketResult><IsTruncated>true</IsTruncated><Contents><Key>a</Key></Contents><CommonPrefixes><Prefix>z/</Prefix></CommonPrefixes></ListBucketResult>";
        let (client, _) = scripted(true, vec![response(200, "", mixed)]).await;
        let page = send(&client, client.list_objects_v2().bucket("b").delimiter("/"))
            .await
            .unwrap();
        assert_eq!(page.next_continuation_token(), Some("z/"));
    }

    #[tokio::test]
    async fn url_encoded_v1_pages_return_plain_markers() {
        // OSS encodes a space as `+`, others as `%20`; a literal plus is `%2B`.
        let body = "<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>true</IsTruncated><Contents><Key>a%2Bb</Key></Contents><Contents><Key>%E4%B8%AD+%E6%96%87%25.txt</Key></Contents></ListBucketResult>";
        let (client, _) = scripted(true, vec![response(200, "", body)]).await;
        let page = send(
            &client,
            client
                .list_objects_v2()
                .bucket("b")
                .encoding_type(aws_sdk_s3::types::EncodingType::Url),
        )
        .await
        .unwrap();
        assert_eq!(page.next_continuation_token(), Some("中 文%.txt"));

        // Mixed entries are ordered by their real names, not their encodings.
        let mixed = "<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>true</IsTruncated><Contents><Key>z</Key></Contents><CommonPrefixes><Prefix>%7E%2F</Prefix></CommonPrefixes></ListBucketResult>";
        let (client, _) = scripted(true, vec![response(200, "", mixed)]).await;
        let page = send(
            &client,
            client
                .list_objects_v2()
                .bucket("b")
                .delimiter("/")
                .encoding_type(aws_sdk_s3::types::EncodingType::Url),
        )
        .await
        .unwrap();
        assert_eq!(page.next_continuation_token(), Some("~/"));
    }

    #[tokio::test]
    async fn v1_service_errors_keep_their_code_and_message() {
        use aws_sdk_s3::error::ProvideErrorMetadata;
        let (client, _) = scripted(
            true,
            vec![response(
                403,
                "",
                "<Error><Code>AccessDenied</Code><Message>no list permission</Message><RequestId>req-1</RequestId></Error>",
            )],
        )
        .await;
        let error = send(&client, client.list_objects_v2().bucket("b"))
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some("AccessDenied"));
        assert_eq!(error.message(), Some("no list permission"));
    }

    #[tokio::test]
    async fn the_last_v1_page_has_no_token() {
        let body = "<ListBucketResult><IsTruncated>false</IsTruncated><NextMarker>ignored</NextMarker><Contents><Key>a</Key></Contents></ListBucketResult>";
        let (client, _) = scripted(true, vec![response(200, "", body)]).await;
        let page = send(&client, client.list_objects_v2().bucket("b"))
            .await
            .unwrap();
        assert_eq!(page.is_truncated(), Some(false));
        assert_eq!(page.next_continuation_token(), None);
    }

    #[tokio::test]
    async fn v1_errors_keep_their_status_for_callers() {
        let (client, _) = scripted(
            true,
            vec![response(
                404,
                "",
                "<Error><Code>NoSuchBucket</Code></Error>",
            )],
        )
        .await;
        let error = send(&client, client.list_objects_v2().bucket("b"))
            .await
            .unwrap_err();
        assert_eq!(error.raw_response().map(|r| r.status().as_u16()), Some(404));
        assert!(error
            .as_service_error()
            .is_some_and(|e| e.is_no_such_bucket()));
    }
}
