//! Thin S3-compatible storage wrapper around `aws-sdk-s3`, configured
//! directly from `r2_config` (no environment/profile credential
//! fallback — wg-server.md §12), with the retry policy from
//! wg-server.md §10. Behind the [`Storage`] trait so `apply`'s logic can
//! be tested against an in-memory fake instead of a real bucket.

use aws_sdk_s3::Client;
use aws_sdk_s3::config::retry::RetryConfig;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use thiserror::Error;
use wg_common::topology::R2Config;

const RETRY_MAX_ATTEMPTS: u32 = 5;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("GetObject {0} failed: {1}")]
    Get(String, String),
    #[error("PutObject {0} failed: {1}")]
    Put(String, String),
    #[error("ListObjectsV2 (prefix {0}) failed: {1}")]
    List(String, String),
    #[error("DeleteObject {0} failed: {1}")]
    Delete(String, String),
    #[error("object {0} has no ETag in the response")]
    MissingETag(String),
}

/// Outcome of a conditional `PutObject`.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum PutOutcome {
    /// The write landed; carries the new object's ETag.
    Written(String),
    /// The precondition (`If-None-Match: *` or `If-Match: <etag>`) failed
    /// — someone else already wrote (or deleted/replaced) first.
    PreconditionFailed,
}

#[derive(Debug, Clone)]
pub struct FetchedObject {
    pub bytes: Vec<u8>,
    pub etag: String,
}

/// Storage operations `apply`'s algorithm needs, independent of the
/// concrete backend — lets the algorithm be tested against an in-memory
/// fake (see `mock` below) instead of a real bucket.
pub trait Storage {
    async fn get_object(&self, key: &str) -> Result<Option<FetchedObject>, StorageError>;

    /// Create `key` only if it does not already exist (`If-None-Match: *`).
    async fn put_if_absent(
        &self,
        key: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<PutOutcome, StorageError>;

    /// Replace `key` only if its current ETag matches `etag` (`If-Match`).
    async fn put_if_match(
        &self,
        key: &str,
        body: Vec<u8>,
        content_type: &str,
        etag: &str,
    ) -> Result<PutOutcome, StorageError>;

    /// List every object key under `prefix`.
    async fn list_all(&self, prefix: &str) -> Result<Vec<String>, StorageError>;

    /// List every object key under `prefix` along with its LastModified
    /// time — used by retention cleanup's minimum-age grace period
    /// (wg-server.md §10 "Retention").
    async fn list_all_with_last_modified(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, std::time::SystemTime)>, StorageError>;

    async fn delete_object(&self, key: &str) -> Result<(), StorageError>;
}

pub struct R2Client {
    client: Client,
    bucket: String,
}

impl R2Client {
    pub fn new(cfg: &R2Config) -> Self {
        let credentials = Credentials::new(
            cfg.read_write_access_key_id.clone(),
            cfg.read_write_secret_access_key.clone(),
            cfg.session_token.clone(),
            None,
            "wg-server-config",
        );

        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(cfg.region.clone()))
            .endpoint_url(cfg.endpoint.clone())
            .credentials_provider(credentials)
            .force_path_style(true)
            .retry_config(RetryConfig::standard().with_max_attempts(RETRY_MAX_ATTEMPTS))
            .build();

        R2Client {
            client: Client::from_conf(config),
            bucket: cfg.bucket.clone(),
        }
    }

    fn interpret_put(
        &self,
        key: &str,
        result: Result<
            aws_sdk_s3::operation::put_object::PutObjectOutput,
            aws_sdk_s3::error::SdkError<
                aws_sdk_s3::operation::put_object::PutObjectError,
                aws_smithy_runtime_api::http::Response,
            >,
        >,
    ) -> Result<PutOutcome, StorageError> {
        match result {
            Ok(resp) => {
                let etag = resp
                    .e_tag()
                    .ok_or_else(|| StorageError::MissingETag(key.to_string()))?
                    .to_string();
                Ok(PutOutcome::Written(etag))
            }
            Err(err) => {
                if is_precondition_failed(&err) {
                    Ok(PutOutcome::PreconditionFailed)
                } else {
                    Err(StorageError::Put(key.to_string(), err.to_string()))
                }
            }
        }
    }
}

impl Storage for R2Client {
    async fn get_object(&self, key: &str) -> Result<Option<FetchedObject>, StorageError> {
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await;
        match result {
            Ok(resp) => {
                let etag = resp
                    .e_tag()
                    .ok_or_else(|| StorageError::MissingETag(key.to_string()))?
                    .to_string();
                let bytes = resp
                    .body
                    .collect()
                    .await
                    .map_err(|e| StorageError::Get(key.to_string(), e.to_string()))?
                    .into_bytes()
                    .to_vec();
                Ok(Some(FetchedObject { bytes, etag }))
            }
            Err(err) => {
                if is_not_found(&err) {
                    Ok(None)
                } else {
                    Err(StorageError::Get(key.to_string(), err.to_string()))
                }
            }
        }
    }

    async fn put_if_absent(
        &self,
        key: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<PutOutcome, StorageError> {
        let result = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(body))
            .content_type(content_type)
            .cache_control("no-store")
            .if_none_match("*")
            .send()
            .await;
        self.interpret_put(key, result)
    }

    async fn put_if_match(
        &self,
        key: &str,
        body: Vec<u8>,
        content_type: &str,
        etag: &str,
    ) -> Result<PutOutcome, StorageError> {
        let result = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(body))
            .content_type(content_type)
            .cache_control("no-store")
            .if_match(etag)
            .send()
            .await;
        self.interpret_put(key, result)
    }

    async fn list_all(&self, prefix: &str) -> Result<Vec<String>, StorageError> {
        let mut keys = Vec::new();
        let mut continuation_token = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);
            if let Some(token) = &continuation_token {
                req = req.continuation_token(token);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| StorageError::List(prefix.to_string(), e.to_string()))?;
            for obj in resp.contents() {
                if let Some(k) = obj.key() {
                    keys.push(k.to_string());
                }
            }
            match resp.next_continuation_token() {
                Some(t) => continuation_token = Some(t.to_string()),
                None => break,
            }
        }
        Ok(keys)
    }

    async fn list_all_with_last_modified(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, std::time::SystemTime)>, StorageError> {
        let mut out = Vec::new();
        let mut continuation_token = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);
            if let Some(token) = &continuation_token {
                req = req.continuation_token(token);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| StorageError::List(prefix.to_string(), e.to_string()))?;
            for obj in resp.contents() {
                if let (Some(k), Some(dt)) = (obj.key(), obj.last_modified()) {
                    out.push((k.to_string(), datetime_to_system_time(dt)));
                }
            }
            match resp.next_continuation_token() {
                Some(t) => continuation_token = Some(t.to_string()),
                None => break,
            }
        }
        Ok(out)
    }

    async fn delete_object(&self, key: &str) -> Result<(), StorageError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| StorageError::Delete(key.to_string(), e.to_string()))?;
        Ok(())
    }
}

fn datetime_to_system_time(dt: &aws_smithy_types::DateTime) -> std::time::SystemTime {
    std::time::SystemTime::UNIX_EPOCH
        + std::time::Duration::new(dt.secs().max(0) as u64, dt.subsec_nanos())
}

fn is_not_found<E>(
    err: &aws_sdk_s3::error::SdkError<E, aws_smithy_runtime_api::http::Response>,
) -> bool {
    err.raw_response()
        .map(|r| r.status().as_u16() == 404)
        .unwrap_or(false)
}

fn is_precondition_failed<E>(
    err: &aws_sdk_s3::error::SdkError<E, aws_smithy_runtime_api::http::Response>,
) -> bool {
    err.raw_response()
        .map(|r| r.status().as_u16() == 412)
        .unwrap_or(false)
}

/// An in-memory [`Storage`] fake for testing `apply`'s logic without a
/// real bucket (wg-server.md's design is tested this way in
/// docs/milestones.md §3; a real-R2 conformance pass is a separate,
/// credentialed check this fake does not replace).
#[cfg(test)]
pub mod mock {
    use super::{FetchedObject, PutOutcome, Storage, StorageError};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime};

    struct Entry {
        bytes: Vec<u8>,
        etag: String,
        created_at: SystemTime,
    }

    #[derive(Default)]
    pub struct MockStorage {
        objects: Mutex<HashMap<String, Entry>>,
        next_etag: Mutex<u64>,
        /// Force the next N put_if_absent/put_if_match calls to fail with
        /// a generic (non-precondition) storage error, to test upload
        /// failure handling.
        pub fail_next_puts: Mutex<u32>,
    }

    impl MockStorage {
        pub fn new() -> Self {
            Self::default()
        }

        fn fresh_etag(&self) -> String {
            let mut n = self.next_etag.lock().unwrap();
            *n += 1;
            format!("etag-{n}")
        }

        pub fn object_count(&self) -> usize {
            self.objects.lock().unwrap().len()
        }

        /// Test-only: move `key`'s recorded creation time `age` further
        /// into the past, to exercise the retention grace period without
        /// a real sleep.
        pub fn backdate(&self, key: &str, age: Duration) {
            if let Some(entry) = self.objects.lock().unwrap().get_mut(key) {
                entry.created_at -= age;
            }
        }
    }

    impl Storage for MockStorage {
        async fn get_object(&self, key: &str) -> Result<Option<FetchedObject>, StorageError> {
            Ok(self
                .objects
                .lock()
                .unwrap()
                .get(key)
                .map(|e| FetchedObject {
                    bytes: e.bytes.clone(),
                    etag: e.etag.clone(),
                }))
        }

        async fn put_if_absent(
            &self,
            key: &str,
            body: Vec<u8>,
            _content_type: &str,
        ) -> Result<PutOutcome, StorageError> {
            {
                let mut n = self.fail_next_puts.lock().unwrap();
                if *n > 0 {
                    *n -= 1;
                    return Err(StorageError::Put(
                        key.to_string(),
                        "injected failure".into(),
                    ));
                }
            }
            let mut objects = self.objects.lock().unwrap();
            if objects.contains_key(key) {
                return Ok(PutOutcome::PreconditionFailed);
            }
            let etag = self.fresh_etag();
            objects.insert(
                key.to_string(),
                Entry {
                    bytes: body,
                    etag: etag.clone(),
                    created_at: SystemTime::now(),
                },
            );
            Ok(PutOutcome::Written(etag))
        }

        async fn put_if_match(
            &self,
            key: &str,
            body: Vec<u8>,
            _content_type: &str,
            etag: &str,
        ) -> Result<PutOutcome, StorageError> {
            {
                let mut n = self.fail_next_puts.lock().unwrap();
                if *n > 0 {
                    *n -= 1;
                    return Err(StorageError::Put(
                        key.to_string(),
                        "injected failure".into(),
                    ));
                }
            }
            let mut objects = self.objects.lock().unwrap();
            let current = objects.get(key).map(|e| e.etag.clone());
            if current.as_deref() != Some(etag) {
                return Ok(PutOutcome::PreconditionFailed);
            }
            let new_etag = self.fresh_etag();
            objects.insert(
                key.to_string(),
                Entry {
                    bytes: body,
                    etag: new_etag.clone(),
                    created_at: SystemTime::now(),
                },
            );
            Ok(PutOutcome::Written(new_etag))
        }

        async fn list_all(&self, prefix: &str) -> Result<Vec<String>, StorageError> {
            Ok(self
                .objects
                .lock()
                .unwrap()
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect())
        }

        async fn list_all_with_last_modified(
            &self,
            prefix: &str,
        ) -> Result<Vec<(String, SystemTime)>, StorageError> {
            Ok(self
                .objects
                .lock()
                .unwrap()
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, e)| (k.clone(), e.created_at))
                .collect())
        }

        async fn delete_object(&self, key: &str) -> Result<(), StorageError> {
            self.objects.lock().unwrap().remove(key);
            Ok(())
        }
    }
}
