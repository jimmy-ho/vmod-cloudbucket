use std::time::Duration;

use anyhow::{Error, Result};
use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::{
    presigning::{PresignedRequest, PresigningConfig},
    Client,
};

use varnish::vcl::ctx::{log, LogTag};
use varnish_sys::VSL_tag_e_SLT_VCL_Log;

#[derive(Clone)]
pub struct S3 {
    bucket: String,
    region: Region,

    client: Client,
}

impl S3 {
    pub async fn new(bucket: String, region: String) -> Self {
        let region = Region::new(region);
        let client = Self::init_client(&region).await;

        Self {
            bucket,
            region,

            client,
        }
    }

    async fn init_client(region: &Region) -> Client {
        let config = aws_config::defaults(BehaviorVersion::latest())
            .region(region.to_owned())
            .load()
            .await;

        Client::new(&config)
    }

    pub async fn get_presigned_request<'a>(&self, key: &'a str) -> Result<PresignedRequest> {
        let key = if let Some(key) = key.strip_prefix("/") {
            key
        } else {
            key
        };
        let bucket = self.bucket.as_str();
        let presigned_request_res = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .presigned(PresigningConfig::expires_in(Duration::from_secs(60)).unwrap())
            .await;

        if presigned_request_res.is_ok() {
            let presigned_request = presigned_request_res.unwrap();
            let presigned_uri = presigned_request.uri();
            log(
                LogTag::Any(VSL_tag_e_SLT_VCL_Log),
                format!(
                    "Presigned URL for region: '{}', bucket: '{}', key: '{}', presigned: '{}'",
                    &self.region, bucket, key, presigned_uri,
                )
                .as_str(),
            );

            Ok(presigned_request)
        } else {
            let err = presigned_request_res.unwrap_err();
            let err_message = format!(
                "Presigning URL of region: '{}', bucket: '{}', key: '{}', got error: {}",
                &self.region,
                bucket,
                key,
                err.to_string()
            );

            Err(Error::msg(err_message))
        }
    }
}
