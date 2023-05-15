use crate::api;
use crate::api::{cage::CagesClient, cage::CreateCageDeploymentIntentRequest};
use crate::common::{resolve_output_path, OutputPath};
use crate::config::ValidatedCageBuildConfig;
use crate::describe::describe_eif;
use crate::enclave::{EIFMeasurements, ENCLAVE_FILENAME};
use crate::progress::{get_tracker, poll_fn_and_report_status, ProgressLogger, StatusReport};
use std::io::Write;
mod error;
use crate::docker::command::get_git_hash;
use crate::docker::command::get_source_date_epoch;
use async_stream::__private::AsyncStream;
use error::DeployError;
use reqwest::Body;
use std::path::Path;
use tokio::fs::File;
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tokio_util::codec::{BytesCodec, FramedRead};

const ENCLAVE_ZIP_FILENAME: &str = "enclave.zip";
const DEPLOY_WATCH_TIMEOUT_SECONDS: u64 = 600; //10 minutes

pub async fn deploy_eif(
    validated_config: &ValidatedCageBuildConfig,
    cage_api: CagesClient,
    output_path: OutputPath,
    eif_measurements: &EIFMeasurements,
    data_plane_version: String,
    installer_version: String,
) -> Result<(), DeployError> {
    let progress_bar = get_tracker("Zipping Cage...", None);
    create_zip_archive_for_eif(output_path.path())?;
    progress_bar.finish_with_message("Cage zipped.");

    let zip_path = output_path.path().join(ENCLAVE_ZIP_FILENAME);
    let zip_file = File::open(&zip_path).await?;
    let zip_len_bytes = zip_file.metadata().await?.len();
    let zip_upload_stream = create_zip_upload_stream(zip_file, zip_len_bytes);

    let eif_size_bytes = get_eif_size_bytes(output_path.path()).await?;

    let cage_deployment_intent_payload = CreateCageDeploymentIntentRequest::new(
        eif_measurements.pcrs(),
        validated_config.clone(),
        eif_size_bytes,
        data_plane_version,
        installer_version,
        get_source_date_epoch(),
        get_git_hash(),
    );

    let deployment_intent = cage_api
        .create_cage_deployment_intent(validated_config.cage_uuid(), cage_deployment_intent_payload)
        .await?;

    let s3_upload_url = deployment_intent.signed_url();
    let reqwest_client = api::Client::builder().build().unwrap();
    let s3_response = reqwest_client
        .put(s3_upload_url)
        .header("Content-Type", "application/zip")
        .header("Content-Length", zip_len_bytes)
        .body(Body::wrap_stream(zip_upload_stream))
        .send()
        .await?;

    tokio::fs::remove_file(zip_path).await?;

    if s3_response.status().is_success() {
        log::info!("Cage uploaded to Evervault.");
    } else {
        return Err(DeployError::UploadError(s3_response.text().await?));
    };

    let progress_bar_for_build =
        get_tracker("Building Cage Docker Image on Evervault Infra...", None);

    watch_build(
        cage_api.clone(),
        deployment_intent.cage_uuid(),
        deployment_intent.deployment_uuid(),
        progress_bar_for_build,
    )
    .await;

    let progress_bar_for_deploy = get_tracker(
        "Deploying Cage into a Trusted Execution Environment...",
        None,
    );

    timed_operation(
        "Cage Deployment",
        DEPLOY_WATCH_TIMEOUT_SECONDS,
        watch_deployment(
            cage_api,
            deployment_intent.cage_uuid(),
            deployment_intent.deployment_uuid(),
            progress_bar_for_deploy,
        ),
    )
    .await?
}

async fn watch_build(
    cage_api: CagesClient,
    cage_uuid: &str,
    deployment_uuid: &str,
    progress_bar: impl ProgressLogger,
) {
    async fn check_build_status(
        cage_api: CagesClient,
        args: Vec<String>,
    ) -> Result<StatusReport, DeployError> {
        let cage_uuid = args.get(0).unwrap();
        let deployment_uuid = args.get(1).unwrap();
        match cage_api
            .get_cage_deployment_by_uuid(cage_uuid, deployment_uuid)
            .await
        {
            Ok(deployment_response) if deployment_response.is_built() => Ok(
                StatusReport::complete("Cage built on Evervault!".to_string()),
            ),
            Ok(_) => Ok(StatusReport::no_op()),
            Err(e) => {
                log::error!("Unable to retrieve build status. Error: {:?}", e);
                Ok(StatusReport::Failed)
            }
        }
    }
    let get_deployment_args = vec![cage_uuid.to_string(), deployment_uuid.to_string()];
    let _ = poll_fn_and_report_status(
        cage_api,
        get_deployment_args,
        check_build_status,
        progress_bar,
    )
    .await;
}

async fn watch_deployment(
    cage_api: CagesClient,
    cage_uuid: &str,
    deployment_uuid: &str,
    progress_bar: impl ProgressLogger,
) -> Result<(), DeployError> {
    async fn check_deployment_status(
        cage_api: CagesClient,
        args: Vec<String>,
    ) -> Result<StatusReport, DeployError> {
        let cage_uuid = args.get(0).unwrap();
        let deployment_uuid = args.get(1).unwrap();
        match cage_api
            .get_cage_deployment_by_uuid(cage_uuid, deployment_uuid)
            .await
        {
            Ok(deployment_response) if deployment_response.is_finished() => {
                Ok(StatusReport::complete("Cage deployed!".to_string()))
            }
            Ok(deployment_response) if deployment_response.is_failed().unwrap_or(false) => {
                if let Some(reason) = deployment_response.get_failure_reason() {
                    log::error!("{reason}");
                }
                Err(DeployError::DeploymentError)
            }
            Ok(deployment_response) => {
                let status_report = match deployment_response.get_detailed_status() {
                    Some(status) => StatusReport::update(status),
                    None => StatusReport::NoOp,
                };
                Ok(status_report)
            }
            Err(e) => {
                log::error!("Unable to retrieve deployment status. Error: {:?}", e);
                Ok(StatusReport::Failed)
            }
        }
    }

    let get_deployment_args = vec![cage_uuid.to_string(), deployment_uuid.to_string()];
    poll_fn_and_report_status(
        cage_api,
        get_deployment_args,
        check_deployment_status,
        progress_bar,
    )
    .await
}

fn create_zip_archive_for_eif(output_path: &std::path::Path) -> zip::result::ZipResult<()> {
    let zip_path = output_path.join(ENCLAVE_ZIP_FILENAME);
    let zip_file = if !zip_path.exists() {
        std::fs::File::create(&zip_path)?
    } else {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&zip_path)?
    };

    let mut zip = zip::ZipWriter::new(zip_file);

    let zip_opts =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    let eif_path = output_path.join(ENCLAVE_FILENAME);
    zip.start_file(ENCLAVE_FILENAME, zip_opts)?;
    let eif = std::fs::read(eif_path)?;
    zip.write_all(eif.as_slice())?;

    let _ = zip.finish()?;

    Ok(())
}

fn create_zip_upload_stream(
    zip_file: File,
    zip_len_bytes: u64,
) -> AsyncStream<Result<bytes::BytesMut, std::io::Error>, impl core::future::Future<Output = ()>> {
    let mut stream = FramedRead::new(zip_file, BytesCodec::new());
    let progress_bar = get_tracker("Uploading Cage to Evervault", Some(zip_len_bytes));
    async_stream::stream! {
        let mut bytes_sent = 0;
        while let Some(bytes) = stream.next().await {
            progress_bar.set_position(bytes_sent);
            if let Ok(bytes) = &bytes {
                bytes_sent += bytes.len() as u64;
            }
            yield bytes;
        }
    }
}

pub fn get_eif<S: AsRef<str>>(
    eif_path: S,
    verbose: bool,
) -> Result<(EIFMeasurements, OutputPath), DeployError> {
    let eif = describe_eif(eif_path.as_ref(), verbose)?;
    let output_path = resolve_output_path(None::<&str>)?;
    let output_p = format!("{}/enclave.eif", output_path.path().to_str().unwrap());
    std::fs::copy(eif_path.as_ref(), output_p)?;
    Ok((eif.measurements.measurements, output_path))
}

async fn get_eif_size_bytes(output_path: &Path) -> Result<u64, DeployError> {
    match tokio::fs::metadata(output_path.join(ENCLAVE_FILENAME)).await {
        Ok(metadata) => Ok(metadata.len()),
        Err(err) => Err(DeployError::EifSizeReadError(err)),
    }
}

pub async fn timed_operation<T: std::future::Future>(
    operation_name: &str,
    max_timeout_seconds: u64,
    operation: T,
) -> Result<<T as std::future::Future>::Output, DeployError> {
    let max_timeout = std::time::Duration::from_secs(max_timeout_seconds);
    let result = timeout(max_timeout, operation).await;
    if let Ok(r) = result {
        Ok(r)
    } else {
        Err(DeployError::TimeoutError(
            operation_name.to_string(),
            max_timeout.as_secs(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enclave::PCRs;
    use crate::test_utils;
    use std::time::Duration;

    #[tokio::test]
    async fn test_get_eif_size() {
        let (_, output_path) = test_utils::build_test_cage(None, None).await.unwrap();
        let output_path_as_string = output_path.path().to_str().unwrap().to_string();

        // ensure temp output directory still exists after running function
        assert!(std::path::PathBuf::from(output_path_as_string).exists());
    }

    #[tokio::test]
    async fn test_reproducible_cage_builds_with_pinned_version() {
        let current_dir = std::env::current_dir().unwrap();
        let (build_output, output_path) = test_utils::build_test_cage(
            None,
            Some(format!("{}/testRepro.Dockerfile", current_dir.to_str().unwrap()).to_string()),
        )
        .await
        .unwrap();
        let eif_pcrs = build_output.measurements().pcrs();

        // Compare build measures as certs are generated on the fly to prevent expiry
        let expected_pcrs: PCRs = serde_json::from_str(r#"{
            "PCR0": "04df1505cf8be30f9eb37d9b31e62f585655601b70fef3e197ae818322218d79ce06c503436f8f911bc059b31f86763b",
            "PCR1": "bcdf05fefccaa8e55bf2c8d6dee9e79bbff31e34bf28a99aa19e6b29c37ee80b214a414b7607236edf26fcb78654e63f",
            "PCR2": "a9862d27493410ab272a994181c14750091f60b11859fb92a011ac4d82e72d870d39e2503e44b3bb7a59f511f569e615"
        }"#).unwrap();
        assert_eq!(&eif_pcrs.pcr0, &expected_pcrs.pcr0);
        assert_eq!(&eif_pcrs.pcr1, &expected_pcrs.pcr1);
        assert_eq!(&eif_pcrs.pcr2, &expected_pcrs.pcr2);
        
        // ensure temp output directory still exists after running function
        assert!(output_path.path().exists());
    }

    async fn long_operation(duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    #[tokio::test]
    async fn test_timed_operation_does_timeout() {
        let operation_name = "Long Operation";
        let result =
            timed_operation(operation_name, 1, long_operation(Duration::from_secs(10))).await;
        let correct_result = match result {
            Err(DeployError::TimeoutError(_, _)) => true,
            _ => false,
        };

        assert_eq!(correct_result, true);
    }

    #[tokio::test]
    async fn test_timed_operation_does_not_timeout() {
        let operation_name = "Long Operation";
        let result =
            timed_operation(operation_name, 4, long_operation(Duration::from_secs(2))).await;
        let correct_result = match result {
            Err(DeployError::TimeoutError(_, _)) => false,
            _ => true,
        };

        assert_eq!(correct_result, true);
    }
}
