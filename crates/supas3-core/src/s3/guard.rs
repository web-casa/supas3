// SPDX-License-Identifier: AGPL-3.0-or-later
//! Overwrite protection for services whose conditional writes differ from S3.
//!
//! The imported backend protects every write with `If-None-Match: *` (do not
//! overwrite) or `If-Match: <etag>` (only replace what was read). Measured
//! against real buckets, those headers are rejected by OSS, silently ignored
//! by COS, and only partly honoured elsewhere. Two pieces keep the callers
//! unchanged:
//!
//! * an interceptor on the client rewrites or removes the headers a service
//!   cannot take, using the provider's own no-overwrite header where one
//!   exists (still atomic);
//! * [`before_write`] performs a check-then-write where no header can do the
//!   job. That check is not atomic: a concurrent writer can still slip in
//!   between the check and the write.

use crate::credentials::WriteGuard;
use crate::error::AppError;
use aws_sdk_s3::config::interceptors::BeforeTransmitInterceptorContextMut;
use aws_sdk_s3::config::{ConfigBag, Intercept, RuntimeComponents};
use aws_sdk_s3::error::BoxError;
use aws_sdk_s3::Client;

/// How writes through one client are protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Protection {
    /// Amazon S3 itself: every conditional header works.
    Native,
    /// A custom endpoint configured with the given guard.
    Custom(WriteGuard),
}

/// The write being protected. Services differ per operation, not only per
/// header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Put,
    CompleteMultipart,
    Copy,
    Delete,
}

/// Why [`before_write`] refused a write.
#[derive(Debug)]
pub(crate) enum Refusal {
    /// The object is not in the state the caller expected.
    Conflict(String),
    /// The state could not be checked.
    Unchecked(AppError),
}

impl From<Refusal> for AppError {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::Conflict(message) => AppError::S3Error(message),
            Refusal::Unchecked(error) => error,
        }
    }
}

impl Protection {
    /// Reads back the protection a client was built with.
    pub(crate) fn of(client: &Client) -> Self {
        client
            .config()
            .interceptors()
            .find_map(|interceptor| WriteGuardInterceptor::guard_named(interceptor.name()))
            .map_or(Self::Native, Self::Custom)
    }

    /// Whether the service itself refuses to overwrite an existing object for
    /// this operation. Based on measurements: custom endpoints honour
    /// `If-None-Match` on PutObject but ignore it on CopyObject, and not all
    /// honour it when completing a multipart upload.
    fn refuses_overwrite_atomically(self, operation: Operation) -> bool {
        match self {
            Self::Native => true,
            Self::Custom(WriteGuard::OssForbidOverwrite | WriteGuard::CosForbidOverwrite) => true,
            Self::Custom(WriteGuard::Conditional) => operation == Operation::Put,
            Self::Custom(WriteGuard::CheckFirst) => false,
        }
    }

    /// Whether the service itself enforces `If-Match` for this write. Measured:
    /// OSS rejects the header on uploads, COS ignores it on uploads but honours
    /// it on DeleteObject, and generic endpoints ignored it on deletes.
    fn enforces_if_match(self, operation: Operation) -> bool {
        match self {
            Self::Native => true,
            Self::Custom(WriteGuard::CosForbidOverwrite) => operation == Operation::Delete,
            Self::Custom(_) => false,
        }
    }
}

/// What the interface needs to know about how a client writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteTraits {
    /// The client talks to a custom endpoint, whether entered in the
    /// connection or picked up from the environment or shared configuration.
    pub custom_endpoint: bool,
    /// The service itself refuses to save over an object that has changed.
    /// Where it does not, the check and the write are two requests.
    pub refuses_changed_saves: bool,
}

/// Reads the traits back from the client that will actually be used.
pub fn write_traits(client: &Client) -> WriteTraits {
    let protection = Protection::of(client);
    WriteTraits {
        custom_endpoint: protection != Protection::Native,
        refuses_changed_saves: protection.enforces_if_match(Operation::Put),
    }
}

/// Checks the destination before a write when the service cannot enforce the
/// condition itself. `expected_etag` is `Some` when a known object is being
/// replaced or removed, `None` when the object must not exist yet.
///
/// Returns immediately, without a request, whenever the service's own header
/// already provides the protection.
pub(crate) async fn before_write(
    client: &Client,
    bucket: &str,
    key: &str,
    expected_etag: Option<&str>,
    operation: Operation,
) -> Result<(), Refusal> {
    let protection = Protection::of(client);
    let needs_check = match expected_etag {
        Some(_) => !protection.enforces_if_match(operation),
        None => !protection.refuses_overwrite_atomically(operation),
    };
    if !needs_check {
        return Ok(());
    }

    let head = client.head_object().bucket(bucket).key(key).send().await;
    let current = match head {
        Ok(output) => Some(output.e_tag().unwrap_or_default().to_string()),
        Err(error) if error.as_service_error().is_some_and(|e| e.is_not_found()) => None,
        Err(error) => {
            return Err(Refusal::Unchecked(AppError::S3Error(format!(
                "Could not check {key} before writing: {}",
                aws_sdk_s3::error::DisplayErrorContext(&error)
            ))))
        }
    };
    match (expected_etag, current) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(Refusal::Conflict(format!(
            "{key} already exists and was not overwritten"
        ))),
        (Some(_), None) => Err(Refusal::Conflict(format!(
            "{key} no longer exists, so it was left untouched"
        ))),
        (Some(expected), Some(current)) if same_etag(expected, &current) => Ok(()),
        (Some(_), Some(_)) => Err(Refusal::Conflict(format!(
            "{key} has changed since it was read, so it was left untouched"
        ))),
    }
}

/// Whether two ETags name the same object state, strictly enough to authorize
/// a write or a delete.
///
/// ETags are opaque and compared exactly, with one measured exception: OSS
/// reports MD5-style ETags in upper case where the same value was read in
/// lower case elsewhere, so values that are hex digests (optionally with a
/// multipart `-N` suffix) compare without case. Empty and weak ETags never
/// match: neither identifies content precisely.
pub(crate) fn same_etag(left: &str, right: &str) -> bool {
    fn strong(etag: &str) -> Option<&str> {
        let etag = etag.trim();
        if etag.starts_with("W/") {
            return None;
        }
        let bare = etag
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap_or(etag);
        (!bare.is_empty()).then_some(bare)
    }
    fn is_digest(etag: &str) -> bool {
        let (hex, parts) = etag.split_once('-').unwrap_or((etag, "1"));
        hex.len() == 32
            && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
            && !parts.is_empty()
            && parts.bytes().all(|byte| byte.is_ascii_digit())
    }
    match (strong(left), strong(right)) {
        (Some(left), Some(right)) if is_digest(left) && is_digest(right) => {
            left.eq_ignore_ascii_case(right)
        }
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Adapts conditional headers on outgoing writes to what the service accepts.
#[derive(Debug)]
pub(crate) struct WriteGuardInterceptor {
    guard: WriteGuard,
}

impl WriteGuardInterceptor {
    pub(crate) fn new(guard: WriteGuard) -> Self {
        Self { guard }
    }

    // The interceptor's name is the only thing readable back from a built
    // client, so it carries the guard.
    fn name_of(guard: WriteGuard) -> &'static str {
        match guard {
            WriteGuard::Conditional => "supas3-write-guard:conditional",
            WriteGuard::OssForbidOverwrite => "supas3-write-guard:oss",
            WriteGuard::CosForbidOverwrite => "supas3-write-guard:cos",
            WriteGuard::CheckFirst => "supas3-write-guard:check-first",
        }
    }

    fn guard_named(name: &str) -> Option<WriteGuard> {
        [
            WriteGuard::Conditional,
            WriteGuard::OssForbidOverwrite,
            WriteGuard::CosForbidOverwrite,
            WriteGuard::CheckFirst,
        ]
        .into_iter()
        .find(|guard| Self::name_of(*guard) == name)
    }
}

impl Intercept for WriteGuardInterceptor {
    fn name(&self) -> &'static str {
        Self::name_of(self.guard)
    }

    fn modify_before_signing(
        &self,
        context: &mut BeforeTransmitInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        _config: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let request = context.request_mut();
        add_content_md5_to_batch_delete(request);
        if self.guard == WriteGuard::Conditional {
            return Ok(());
        }
        // Reads keep their conditions: every measured service honours
        // If-Match on GET and HEAD.
        let method = request.method().to_string();
        if !matches!(method.as_str(), "PUT" | "POST" | "DELETE") {
            return Ok(());
        }
        let headers = request.headers_mut();
        if headers.remove("if-none-match").is_some() {
            if let Some(header) = self.guard.forbid_overwrite_header() {
                headers.insert(header, "true");
            }
        }
        // Replaced by the ETag comparison in `before_write`, except where the
        // service enforces it itself.
        let operation = if method == "DELETE" {
            Operation::Delete
        } else {
            Operation::Put
        };
        if !Protection::Custom(self.guard).enforces_if_match(operation) {
            headers.remove("if-match");
        }
        Ok(())
    }
}

/// OSS, COS and OBS still require `Content-MD5` on DeleteObjects, which the
/// SDK no longer sends. Every measured service accepts it, so it is added for
/// all custom endpoints.
fn add_content_md5_to_batch_delete(request: &mut aws_sdk_s3::config::http::HttpRequest) {
    use md5::Digest;
    let is_batch_delete = request.method() == "POST"
        && request.uri().split_once('?').is_some_and(|(_, query)| {
            query
                .split('&')
                .any(|part| part == "delete" || part == "delete=")
        });
    if !is_batch_delete || request.headers().contains_key("content-md5") {
        return;
    }
    let Some(body) = request.body().bytes() else {
        return;
    };
    let digest = aws_smithy_types::base64::encode(md5::Md5::digest(body));
    request.headers_mut().insert("content-md5", digest);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_s3::{response, scripted_endpoint};

    async fn client_with(
        guard: Option<WriteGuard>,
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
        if let Some(guard) = guard {
            config = config.interceptor(WriteGuardInterceptor::new(guard));
        }
        (Client::from_conf(config.build()), server)
    }

    fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
        request.lines().find_map(|line| {
            let (found, value) = line.split_once(':')?;
            found.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    #[test]
    fn digest_etags_ignore_case_and_quoting() {
        let lower = "\"900150983cd24fb0d6963f7d28e17f72\"";
        let upper = "\"900150983CD24FB0D6963F7D28E17F72\"";
        assert!(same_etag(lower, upper));
        assert!(same_etag("900150983cd24fb0d6963f7d28e17f72", upper));
        assert!(same_etag(
            "\"08f0aef6bc05752969bdedde2e48b6d7-2\"",
            "\"08F0AEF6BC05752969BDEDDE2E48B6D7-2\""
        ));
        // The part count is part of the value.
        assert!(!same_etag(
            "\"08f0aef6bc05752969bdedde2e48b6d7-2\"",
            "\"08f0aef6bc05752969bdedde2e48b6d7-3\""
        ));
        assert!(!same_etag(lower, "\"900150983cd24fb0d6963f7d28e17f73\""));
    }

    #[test]
    fn opaque_weak_and_empty_etags_are_compared_strictly() {
        assert!(same_etag("\"TokenA\"", "\"TokenA\""));
        assert!(
            !same_etag("\"TokenA\"", "\"tokena\""),
            "opaque values keep their case"
        );
        assert!(
            !same_etag("W/\"TokenA\"", "\"TokenA\""),
            "a weak ETag cannot authorize a write"
        );
        assert!(!same_etag("W/\"TokenA\"", "W/\"TokenA\""));
        for empty in ["", "\"\"", "  "] {
            assert!(!same_etag(empty, empty), "{empty:?}");
            assert!(!same_etag(empty, "\"TokenA\""));
        }
    }

    #[tokio::test]
    async fn cos_keeps_if_match_on_delete_and_needs_no_check() {
        let (client, server) = client_with(
            Some(WriteGuard::CosForbidOverwrite),
            vec![response(204, "", "")],
        )
        .await;
        // No scripted HEAD: the service enforces the condition itself.
        before_write(&client, "b", "k", Some("\"etag\""), Operation::Delete)
            .await
            .unwrap();
        client
            .delete_object()
            .bucket("b")
            .key("k")
            .if_match("\"etag\"")
            .send()
            .await
            .unwrap();
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("DELETE "));
        assert_eq!(header(&requests[0], "if-match"), Some("\"etag\""));

        // OSS ignored a conditional delete, so there it is checked and stripped.
        let (client, server) = client_with(
            Some(WriteGuard::OssForbidOverwrite),
            vec![
                response(200, "ETag: \"etag\"\r\n", ""),
                response(204, "", ""),
            ],
        )
        .await;
        before_write(&client, "b", "k", Some("\"etag\""), Operation::Delete)
            .await
            .unwrap();
        client
            .delete_object()
            .bucket("b")
            .key("k")
            .if_match("\"etag\"")
            .send()
            .await
            .unwrap();
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("HEAD "));
        assert_eq!(header(&requests[1], "if-match"), None);
    }

    #[tokio::test]
    async fn the_protection_is_read_back_from_the_client() {
        let (native, _) = client_with(None, vec![]).await;
        assert_eq!(Protection::of(&native), Protection::Native);
        assert_eq!(
            write_traits(&native),
            WriteTraits {
                custom_endpoint: false,
                refuses_changed_saves: true,
            }
        );
        for guard in [
            WriteGuard::Conditional,
            WriteGuard::OssForbidOverwrite,
            WriteGuard::CosForbidOverwrite,
            WriteGuard::CheckFirst,
        ] {
            let (client, _) = client_with(Some(guard), vec![]).await;
            assert_eq!(Protection::of(&client), Protection::Custom(guard));
            // No custom endpoint was measured to enforce `If-Match` on a save.
            assert_eq!(
                write_traits(&client),
                WriteTraits {
                    custom_endpoint: true,
                    refuses_changed_saves: false,
                }
            );
        }
    }

    #[tokio::test]
    async fn provider_headers_replace_if_none_match_on_writes() {
        for (guard, expected) in [
            (
                WriteGuard::OssForbidOverwrite,
                Some("x-oss-forbid-overwrite"),
            ),
            (
                WriteGuard::CosForbidOverwrite,
                Some("x-cos-forbid-overwrite"),
            ),
            (WriteGuard::CheckFirst, None),
        ] {
            let (client, server) = client_with(Some(guard), vec![response(200, "", "")]).await;
            client
                .put_object()
                .bucket("b")
                .key("k")
                .if_none_match("*")
                .send()
                .await
                .unwrap();
            let requests = server.await.unwrap();
            assert_eq!(header(&requests[0], "if-none-match"), None, "{guard:?}");
            for name in ["x-oss-forbid-overwrite", "x-cos-forbid-overwrite"] {
                let wanted = (expected == Some(name)).then_some("true");
                assert_eq!(header(&requests[0], name), wanted, "{guard:?} {name}");
            }
        }
    }

    #[tokio::test]
    async fn batch_delete_carries_the_md5_of_its_body_on_custom_endpoints() {
        use aws_sdk_s3::types::{Delete, ObjectIdentifier};
        use md5::Digest;
        let delete = || {
            Delete::builder()
                .objects(ObjectIdentifier::builder().key("a").build().unwrap())
                .objects(ObjectIdentifier::builder().key("b").build().unwrap())
                .build()
                .unwrap()
        };
        let reply = || response(200, "", "<DeleteResult/>");

        for guard in [WriteGuard::Conditional, WriteGuard::OssForbidOverwrite] {
            let (client, server) = client_with(Some(guard), vec![reply()]).await;
            client
                .delete_objects()
                .bucket("b")
                .delete(delete())
                .send()
                .await
                .unwrap();
            let requests = server.await.unwrap();
            let body = requests[0].split_once("\r\n\r\n").unwrap().1;
            let expected = aws_smithy_types::base64::encode(md5::Md5::digest(body.as_bytes()));
            assert_eq!(
                header(&requests[0], "content-md5"),
                Some(expected.as_str()),
                "{guard:?}"
            );
        }

        // Amazon S3 itself is left exactly as the SDK sends it.
        let (client, server) = client_with(None, vec![reply()]).await;
        client
            .delete_objects()
            .bucket("b")
            .delete(delete())
            .send()
            .await
            .unwrap();
        assert_eq!(header(&server.await.unwrap()[0], "content-md5"), None);
    }

    #[tokio::test]
    async fn other_requests_get_no_content_md5() {
        let (client, server) = client_with(
            Some(WriteGuard::CosForbidOverwrite),
            vec![response(200, "", "")],
        )
        .await;
        client
            .put_object()
            .bucket("b")
            .key("delete")
            .body(aws_sdk_s3::primitives::ByteStream::from_static(b"x"))
            .send()
            .await
            .unwrap();
        assert_eq!(header(&server.await.unwrap()[0], "content-md5"), None);
    }

    #[tokio::test]
    async fn an_unconditional_write_gets_no_provider_header() {
        let (client, server) = client_with(
            Some(WriteGuard::OssForbidOverwrite),
            vec![response(200, "", "")],
        )
        .await;
        client
            .put_object()
            .bucket("b")
            .key("k")
            .send()
            .await
            .unwrap();
        let requests = server.await.unwrap();
        assert_eq!(header(&requests[0], "x-oss-forbid-overwrite"), None);
    }

    #[tokio::test]
    async fn if_match_is_removed_from_writes_but_kept_on_reads() {
        let (client, server) = client_with(
            Some(WriteGuard::OssForbidOverwrite),
            vec![response(200, "", ""), response(200, "", "")],
        )
        .await;
        client
            .put_object()
            .bucket("b")
            .key("k")
            .if_match("\"etag\"")
            .send()
            .await
            .unwrap();
        client
            .get_object()
            .bucket("b")
            .key("k")
            .if_match("\"etag\"")
            .send()
            .await
            .unwrap();
        let requests = server.await.unwrap();
        assert_eq!(header(&requests[0], "if-match"), None);
        assert_eq!(header(&requests[1], "if-match"), Some("\"etag\""));
    }

    #[tokio::test]
    async fn standard_conditional_endpoints_keep_their_headers() {
        let (client, server) =
            client_with(Some(WriteGuard::Conditional), vec![response(200, "", "")]).await;
        client
            .put_object()
            .bucket("b")
            .key("k")
            .if_none_match("*")
            .send()
            .await
            .unwrap();
        let requests = server.await.unwrap();
        assert_eq!(header(&requests[0], "if-none-match"), Some("*"));
    }

    #[tokio::test]
    async fn no_request_is_made_when_the_service_protects_the_write_itself() {
        // An empty script: any request would hang and fail the test.
        let (native, _) = client_with(None, vec![]).await;
        before_write(&native, "b", "k", None, Operation::Copy)
            .await
            .unwrap();
        before_write(&native, "b", "k", Some("\"e\""), Operation::Put)
            .await
            .unwrap();

        let (oss, _) = client_with(Some(WriteGuard::OssForbidOverwrite), vec![]).await;
        before_write(&oss, "b", "k", None, Operation::Put)
            .await
            .unwrap();
        before_write(&oss, "b", "k", None, Operation::Copy)
            .await
            .unwrap();
        before_write(&oss, "b", "k", None, Operation::CompleteMultipart)
            .await
            .unwrap();

        let (generic, _) = client_with(Some(WriteGuard::Conditional), vec![]).await;
        before_write(&generic, "b", "k", None, Operation::Put)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn creating_checks_first_where_the_service_would_overwrite() {
        let exists = || response(200, "ETag: \"abc\"\r\n", "");
        let missing = || response(404, "", "");

        let (client, server) = client_with(Some(WriteGuard::CheckFirst), vec![exists()]).await;
        let refused = before_write(&client, "b", "k", None, Operation::Put).await;
        assert!(matches!(refused, Err(Refusal::Conflict(_))));
        assert!(server.await.unwrap()[0].starts_with("HEAD "));

        let (client, _) = client_with(Some(WriteGuard::CheckFirst), vec![missing()]).await;
        before_write(&client, "b", "k", None, Operation::Put)
            .await
            .unwrap();

        // Copy and multipart completion ignore If-None-Match on generic endpoints.
        let (client, _) = client_with(Some(WriteGuard::Conditional), vec![exists()]).await;
        let refused = before_write(&client, "b", "k", None, Operation::Copy).await;
        assert!(matches!(refused, Err(Refusal::Conflict(_))));
        let (client, _) = client_with(Some(WriteGuard::Conditional), vec![missing()]).await;
        before_write(&client, "b", "k", None, Operation::CompleteMultipart)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn replacing_compares_the_current_etag_on_custom_endpoints() {
        let current = |etag: &str| response(200, &format!("ETag: {etag}\r\n"), "");
        for guard in [
            WriteGuard::Conditional,
            WriteGuard::OssForbidOverwrite,
            WriteGuard::CosForbidOverwrite,
            WriteGuard::CheckFirst,
        ] {
            // OSS reports upper-case ETags; the same content must still match.
            let (client, _) = client_with(
                Some(guard),
                vec![current("\"900150983CD24FB0D6963F7D28E17F72\"")],
            )
            .await;
            before_write(
                &client,
                "b",
                "k",
                Some("\"900150983cd24fb0d6963f7d28e17f72\""),
                Operation::Put,
            )
            .await
            .unwrap();

            let (client, _) = client_with(Some(guard), vec![current("\"other\"")]).await;
            let changed = before_write(
                &client,
                "b",
                "k",
                Some("\"900150983cd24fb0d6963f7d28e17f72\""),
                Operation::Put,
            )
            .await;
            assert!(matches!(changed, Err(Refusal::Conflict(_))), "{guard:?}");

            let (client, _) = client_with(Some(guard), vec![response(404, "", "")]).await;
            let gone = before_write(
                &client,
                "b",
                "k",
                Some("\"900150983cd24fb0d6963f7d28e17f72\""),
                Operation::Put,
            )
            .await;
            assert!(matches!(gone, Err(Refusal::Conflict(_))), "{guard:?}");
        }
    }

    #[tokio::test]
    async fn a_failed_check_is_not_reported_as_a_conflict() {
        let (client, _) =
            client_with(Some(WriteGuard::CheckFirst), vec![response(403, "", "")]).await;
        let refused = before_write(&client, "b", "k", None, Operation::Put).await;
        assert!(matches!(refused, Err(Refusal::Unchecked(_))));
    }
}
