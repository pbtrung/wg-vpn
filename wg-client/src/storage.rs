//! Read-only S3-compatible storage wrapper (wg-client.md §6 Retry
//! policy): `GetObject` only, 5-attempt retry, no environment/profile
//! credential fallback.

use aws_sdk_s3::Client;
use aws_sdk_s3::config::retry::RetryConfig;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use thiserror::Error;

use crate::config::R2ReadConfig;

const RETRY_MAX_ATTEMPTS: u32 = 5;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("GetObject {0} failed: {1}")]
    Get(String, String),
}

pub trait ReadStorage {
    /// `Ok(None)` on a clean 404; `Err` for anything else (including a
    /// 403 that can't be distinguished from "not found" — see
    /// wg-client.md §6, handled one layer up since that ambiguity is
    /// resolved differently for the pointer vs. a generation file).
    async fn get_object(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError>;
}

pub struct R2ReadClient {
    client: Client,
    bucket: String,
}

impl R2ReadClient {
    pub fn new(cfg: &R2ReadConfig) -> Self {
        let mut creds_builder =
            Credentials::builder().access_key_id(cfg.read_only_access_key_id.clone());
        creds_builder = creds_builder.secret_access_key(cfg.read_only_secret_access_key.clone());
        if let Some(token) = &cfg.session_token {
            creds_builder = creds_builder.session_token(token.clone());
        }
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(cfg.region.clone()))
            .endpoint_url(cfg.endpoint.clone())
            .credentials_provider(creds_builder.build())
            .force_path_style(true)
            .retry_config(RetryConfig::standard().with_max_attempts(RETRY_MAX_ATTEMPTS))
            .build();
        R2ReadClient {
            client: Client::from_conf(config),
            bucket: cfg.bucket.clone(),
        }
    }
}

impl ReadStorage for R2ReadClient {
    async fn get_object(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await;
        match result {
            Ok(resp) => {
                let bytes = resp
                    .body
                    .collect()
                    .await
                    .map_err(|e| StorageError::Get(key.to_string(), e.to_string()))?
                    .into_bytes()
                    .to_vec();
                Ok(Some(bytes))
            }
            Err(err) => {
                let not_found = err
                    .raw_response()
                    .map(|r| r.status().as_u16() == 404)
                    .unwrap_or(false);
                if not_found {
                    Ok(None)
                } else {
                    Err(StorageError::Get(key.to_string(), err.to_string()))
                }
            }
        }
    }
}

#[cfg(test)]
pub mod mock {
    use super::{ReadStorage, StorageError};
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct MockReadStorage {
        objects: Mutex<HashMap<String, Vec<u8>>>,
    }

    impl MockReadStorage {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn set(&self, key: &str, bytes: Vec<u8>) {
            self.objects.lock().unwrap().insert(key.to_string(), bytes);
        }
    }

    impl ReadStorage for MockReadStorage {
        async fn get_object(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
            Ok(self.objects.lock().unwrap().get(key).cloned())
        }
    }
}
