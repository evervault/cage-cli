pub mod error;
use error::BuildError;

use crate::common::{resolve_output_path, OutputPath};
use crate::config::ValidatedCageBuildConfig;
use crate::docker::error::DockerError;
use crate::docker::parse::{Directive, DockerfileDecoder, Mode};
use crate::docker::utils::verify_docker_is_running;
use crate::enclave;
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::AsyncRead;

const EV_USER_DOCKERFILE_PATH: &str = "enclave.Dockerfile";
const INSTALLER_DIRECTORY: &str = "/opt/evervault";
const USER_ENTRYPOINT_SERVICE_PATH: &str = "/etc/service/user-entrypoint";
const DATA_PLANE_SERVICE_PATH: &str = "/etc/service/data-plane";

pub async fn build_enclave_image_file(
    cage_config: &ValidatedCageBuildConfig,
    context_path: &str,
    output_dir: Option<&str>,
    verbose: bool,
    docker_build_args: Option<Vec<&str>>,
    data_plane_version: String,
    installer_version: String,
    timestamp: String,
    from_existing: Option<String>,
) -> Result<(enclave::BuiltEnclave, OutputPath), BuildError> {
    let context_path = Path::new(&context_path);
    if !context_path.exists() {
        log::error!(
            "The build context directory {} does not exist.",
            &context_path.display()
        );
        return Err(BuildError::ContextPathDoesNotExist);
    }

    // temporary directory must remain in scope for the whole
    // function so it isn't deleted until all the builds are finished.
    let output_path = resolve_output_path(output_dir)?;

    let signing_info = enclave::EnclaveSigningInfo::try_from(cage_config.signing_info())?;

    match from_existing {
        Some(path) => {
            let user_dockerfile_path = output_path.path().join(path);
            enclave::build_user_image(
                &user_dockerfile_path,
                context_path,
                verbose,
                docker_build_args,
                timestamp,
            )?;
        }
        None => {
            build_from_scratch(
                cage_config,
                context_path,
                verbose,
                docker_build_args,
                data_plane_version,
                installer_version,
                output_path.path(),
                timestamp,
            )
            .await?;
        }
    };

    log::debug!(
        "Building Nitro CLI image... {}",
        output_path.path().as_os_str().to_str().unwrap()
    );

    enclave::build_nitro_cli_image(output_path.path(), Some(&signing_info), verbose)?;
    log::info!("Converting docker image to EIF...");
    enclave::run_conversion_to_enclave(output_path.path(), verbose)
        .map(|built_enc| (built_enc, output_path))
        .map_err(|e| e.into())
}

pub async fn build_from_scratch(
    cage_config: &ValidatedCageBuildConfig,
    context_path: &Path,
    verbose: bool,
    docker_build_args: Option<Vec<&str>>,
    data_plane_version: String,
    installer_version: String,
    output_path: &PathBuf,
    timestamp: String,
) -> Result<(), BuildError> {
    if !verify_docker_is_running()? {
        return Err(DockerError::DaemonNotRunning.into());
    }

    // read dockerfile
    let dockerfile_path = Path::new(cage_config.dockerfile());
    if !dockerfile_path.exists() {
        return Err(BuildError::DockerfileAccessError(
            cage_config.dockerfile().to_string(),
        ));
    }

    let dockerfile = File::open(dockerfile_path)
        .await
        .map_err(|_| BuildError::DockerfileAccessError(cage_config.dockerfile().to_string()))?;

    let processed_dockerfile = process_dockerfile(
        cage_config,
        dockerfile,
        data_plane_version,
        installer_version,
    )
    .await?;

    // write new dockerfile to fs
    let user_dockerfile_path = output_path.as_path().join(EV_USER_DOCKERFILE_PATH);

    let mut ev_user_dockerfile = std::fs::File::create(&user_dockerfile_path)
        .map_err(BuildError::FailedToWriteCageDockerfile)?;

    processed_dockerfile.iter().for_each(|instruction| {
        writeln!(ev_user_dockerfile, "{}", instruction).unwrap();
    });

    log::debug!(
        "Processed dockerfile saved at {}.",
        user_dockerfile_path.display()
    );

    log::info!("Building docker image...");

    enclave::build_user_image(
        &user_dockerfile_path,
        context_path,
        verbose,
        docker_build_args,
        timestamp,
    )?;
    log::debug!("User image built...");
    Ok(())
}

async fn process_dockerfile<R: AsyncRead + std::marker::Unpin>(
    build_config: &ValidatedCageBuildConfig,
    dockerfile_src: R,
    data_plane_version: String,
    installer_version: String,
) -> Result<Vec<Directive>, BuildError> {
    // Decode dockerfile from file
    let instruction_set = DockerfileDecoder::decode_dockerfile_from_src(dockerfile_src).await?;

    // Filter out unwanted directives
    let mut last_cmd = None;
    let mut last_entrypoint = None;
    let mut exposed_port: Option<u16> = None;

    let remove_unwanted_directives = |directive: &Directive| -> bool {
        if directive.is_cmd() {
            last_cmd = Some(directive.clone());
        } else if directive.is_entrypoint() {
            last_entrypoint = Some(directive.clone());
        } else if let Directive::Expose { port } = directive {
            exposed_port = *port;
        } else {
            return true;
        }
        false
    };

    let cleaned_instructions: Vec<Directive> = instruction_set
        .into_iter()
        .filter(remove_unwanted_directives)
        .collect();

    let wait_for_env = if build_config.disable_tls_termination {
        "echo TLS termination is off, not waiting for environment to be ready"
    } else {
        r#"while ! grep -q \"EV_CAGE_INITIALIZED\" /etc/customer-env\n do echo \"Env not ready, sleeping user process for one second\"\n sleep 1\n done \n . /etc/customer-env\n"#
    };
    let user_service_builder =
        crate::docker::utils::create_combined_docker_entrypoint(last_entrypoint, last_cmd).map(
            |entrypoint| {
                let entrypoint_script =
                    format!("sleep 5\\necho \\\"Checking status of data-plane\\\"\\nSVDIR=/etc/service sv check data-plane || exit 1\\necho \\\"Data-plane up and running\\\"\\n{wait_for_env}\\necho \\\"Booting user service...\\\"\\ncd %s\\nexec {entrypoint}");
                let user_service_runner = format!("{USER_ENTRYPOINT_SERVICE_PATH}/run");
                let user_service_runit_wrapper = crate::docker::utils::write_command_to_script(
                    entrypoint_script.as_str(),
                    user_service_runner.as_str(),
                    &[r#" "$PWD" "#],
                );
                Directive::new_run(user_service_runit_wrapper)
            },
        )?;

    if let Some(true) = exposed_port.map(|port| port == 443) {
        return Err(DockerError::RestrictedPortExposed(exposed_port.unwrap()).into());
    }

    let ev_domain = std::env::var("EV_DOMAIN").unwrap_or_else(|_| String::from("evervault.com"));

    let data_plane_url = format!(
        "https://cage-build-assets.{}/runtime/{}/data-plane/{}",
        ev_domain,
        data_plane_version,
        build_config.get_dataplane_feature_label()
    );

    let mut data_plane_run_script =
        r#"echo \"Booting Evervault data plane...\"\nexec /opt/evervault/data-plane"#.to_string();
    if let Some(port) = exposed_port {
        data_plane_run_script = format!("{data_plane_run_script} {port}");
    }

    let bootstrap_script_content = r#"ifconfig lo 127.0.0.1\n echo \"enclave.local\" > /etc/hostname \n echo \"127.0.0.1 enclave.local\" >> /etc/hosts \n hostname -F /etc/hostname \necho \"Booting enclave...\"\nexec runsvdir /etc/service"#;

    let installer_bundle_url = format!(
        "https://cage-build-assets.{}/installer/{}.tar.gz",
        ev_domain, installer_version
    );
    let installer_bundle = "runtime-dependencies.tar.gz";
    let installer_destination = format!("{INSTALLER_DIRECTORY}/{installer_bundle}");

    let repro_time = r#"find $( ls / | grep -E -v "^(dev|mnt|proc|sys)$" ) -xdev | xargs touch --date="@0" --no-dereference || true"#.to_string();

    let egress = build_config.clone().egress;

    let egress_obj = if egress.is_enabled() {
        json!({
            "ports": &egress.clone().get_ports(),
            "allowList": &egress.get_destinations()
        })
    } else {
        json!({})
    };

    let dataplane_info = json!({
        "apiKeyAuth":  &build_config.api_key_auth().to_string(),
        "trxLoggingEnabled": &build_config.trx_logging_enabled().to_string(),
        "egress": egress_obj
    })
    .to_string();

    let dataplane_env = format!(
        "echo {} > /etc/dataplane-vars",
        dataplane_info.replace("\"", "\\\"")
    );

    let injected_directives = vec![
        // install dependencies
        Directive::new_run(format!("mkdir -p {INSTALLER_DIRECTORY}")),
        Directive::new_add(&installer_bundle_url, &installer_destination),
        Directive::new_run(format!("cd {INSTALLER_DIRECTORY} ; tar -xzf {installer_bundle} ; sh ./installer.sh ; rm {installer_bundle}")),
        Directive::new_run(dataplane_env),
        // create user service directory
        Directive::new_run(format!("mkdir -p {USER_ENTRYPOINT_SERVICE_PATH}")),
        // add user service runner
        user_service_builder,
        // add data-plane executable
        Directive::new_add(data_plane_url, "/opt/evervault/data-plane".into()),
        Directive::new_run("chmod +x /opt/evervault/data-plane"),
        // add data-plane service directory
        Directive::new_run(format!("mkdir -p {DATA_PLANE_SERVICE_PATH}")),
        // add data-plane service runner
        Directive::new_run(crate::docker::utils::write_command_to_script(
            data_plane_run_script.as_str(),
            format!("{DATA_PLANE_SERVICE_PATH}/run").as_str(),
            &[],
        ))
    ];

    let start_up_directives = vec![
        // Add bootstrap script to configure enclave before starting services
        Directive::new_run(crate::docker::utils::write_command_to_script(
            bootstrap_script_content,
            "/bootstrap",
            &[],
        )),
        Directive::new_run(repro_time),
        // Squash layers for reproducible builds
        Directive::new_from("scratch".to_string()),
        Directive::new_copy("--from=0 / /".to_string()),
        // add entrypoint which starts the runit services
        Directive::new_entrypoint(
            Mode::Exec,
            vec!["/bootstrap".to_string(), "1>&2".to_string()],
        ),
    ];

    // add custom directives to end of dockerfile
    Ok([
        cleaned_instructions,
        injected_directives,
        start_up_directives,
    ]
    .concat())
}

#[cfg(test)]
mod test {
    use super::{process_dockerfile, BuildError};
    use crate::cert::CertValidityPeriod;
    use crate::config::EgressSettings;
    use crate::config::ValidatedCageBuildConfig;
    use crate::config::ValidatedSigningInfo;
    use crate::docker;
    use crate::enclave;
    use crate::test_utils;
    use std::iter::zip;
    use tempfile::TempDir;

    fn get_config() -> ValidatedCageBuildConfig {
        ValidatedCageBuildConfig {
            cage_name: "test".into(),
            cage_uuid: "1234".into(),
            team_uuid: "teamid".into(),
            debug: false,
            app_uuid: "3241".into(),
            dockerfile: "".into(),
            egress: EgressSettings {
                enabled: false,
                destinations: None,
                ports: Some(vec!["433".to_string()]),
            },
            attestation: None,
            signing: ValidatedSigningInfo {
                cert: "".into(),
                key: "".into(),
                cert_validity_period: CertValidityPeriod {
                    not_before: "".into(),
                    not_after: "".into(),
                },
            },
            disable_tls_termination: false,
            api_key_auth: true,
            trx_logging_enabled: true,
            runtime: None,
        }
    }

    #[tokio::test]
    async fn test_process_dockerfile() {
        let sample_dockerfile_contents = r#"FROM alpine

RUN touch /hello-script;\
    /bin/sh -c "echo -e '"'#!/bin/sh\nwhile true; do echo "hello"; sleep 2; done;\n'"' > /hello-script"

ENTRYPOINT ["sh", "/hello-script"]"#;
        let mut readable_contents = sample_dockerfile_contents.as_bytes();

        let config = get_config();

        let data_plane_version = "0.0.0".to_string();
        let installer_version = "abcdef".to_string();

        let processed_file = process_dockerfile(
            &config,
            &mut readable_contents,
            data_plane_version,
            installer_version,
        )
        .await;
        assert_eq!(processed_file.is_ok(), true);
        let processed_file = processed_file.unwrap();

        let expected_output_contents = r##"FROM alpine
RUN touch /hello-script;\
    /bin/sh -c "echo -e '"'#!/bin/sh\nwhile true; do echo "hello"; sleep 2; done;\n'"' > /hello-script"
RUN mkdir -p /opt/evervault
ADD https://cage-build-assets.evervault.com/installer/abcdef.tar.gz /opt/evervault/runtime-dependencies.tar.gz
RUN cd /opt/evervault ; tar -xzf runtime-dependencies.tar.gz ; sh ./installer.sh ; rm runtime-dependencies.tar.gz
RUN echo {\"apiKeyAuth\":\"true\",\"egress\":{},\"trxLoggingEnabled\":\"true\"} > /etc/dataplane-vars
RUN mkdir -p /etc/service/user-entrypoint
RUN printf "#!/bin/sh\nsleep 5\necho \"Checking status of data-plane\"\nSVDIR=/etc/service sv check data-plane || exit 1\necho \"Data-plane up and running\"\nwhile ! grep -q \"EV_CAGE_INITIALIZED\" /etc/customer-env\n do echo \"Env not ready, sleeping user process for one second\"\n sleep 1\n done \n . /etc/customer-env\n\necho \"Booting user service...\"\ncd %s\nexec sh /hello-script\n" "$PWD"  > /etc/service/user-entrypoint/run && chmod +x /etc/service/user-entrypoint/run
ADD https://cage-build-assets.evervault.com/runtime/0.0.0/data-plane/egress-disabled/tls-termination-enabled /opt/evervault/data-plane
RUN chmod +x /opt/evervault/data-plane
RUN mkdir -p /etc/service/data-plane
RUN printf "#!/bin/sh\necho \"Booting Evervault data plane...\"\nexec /opt/evervault/data-plane\n" > /etc/service/data-plane/run && chmod +x /etc/service/data-plane/run
RUN printf "#!/bin/sh\nifconfig lo 127.0.0.1\n echo \"enclave.local\" > /etc/hostname \n echo \"127.0.0.1 enclave.local\" >> /etc/hosts \n hostname -F /etc/hostname \necho \"Booting enclave...\"\nexec runsvdir /etc/service\n" > /bootstrap && chmod +x /bootstrap
RUN find $( ls / | grep -E -v "^(dev|mnt|proc|sys)$" ) -xdev | xargs touch --date="@0" --no-dereference || true
FROM scratch
COPY --from=0 / /
ENTRYPOINT ["/bootstrap", "1>&2"]
"##;

        let expected_directives = docker::parse::DockerfileDecoder::decode_dockerfile_from_src(
            expected_output_contents.as_bytes(),
        )
        .await
        .unwrap();

        assert_eq!(expected_directives.len(), processed_file.len());
        for (expected_directive, processed_directive) in
            zip(expected_directives.iter(), processed_file.iter())
        {
            let expected_directive = expected_directive.to_string();
            let processed_directive = processed_directive.to_string();
            assert_eq!(expected_directive, processed_directive);
        }
    }

    #[tokio::test]
    async fn test_process_dockerfile_with_restricted_reserved_port() {
        let sample_dockerfile_contents = r#"FROM alpine

RUN touch /hello-script;\
    /bin/sh -c "echo -e '"'#!/bin/sh\nwhile true; do echo "hello"; sleep 2; done;\n'"' > /hello-script"
EXPOSE 443
ENTRYPOINT ["sh", "/hello-script"]"#;
        let mut readable_contents = sample_dockerfile_contents.as_bytes();

        let config = get_config();

        let data_plane_version = "0.0.0".to_string();
        let installer_version = "abcdef".to_string();
        let processed_file = process_dockerfile(
            &config,
            &mut readable_contents,
            data_plane_version,
            installer_version,
        )
        .await;
        assert_eq!(processed_file.is_err(), true);

        assert!(matches!(
            processed_file,
            Err(BuildError::DockerError(
                crate::docker::error::DockerError::RestrictedPortExposed(443)
            ))
        ));
    }

    #[tokio::test]
    async fn test_process_dockerfile_with_valid_reserved_port() {
        let sample_dockerfile_contents = r#"FROM alpine

RUN touch /hello-script;\
    /bin/sh -c "echo -e '"'#!/bin/sh\nwhile true; do echo "hello"; sleep 2; done;\n'"' > /hello-script"
EXPOSE 3443
ENTRYPOINT ["sh", "/hello-script"]"#;
        let mut readable_contents = sample_dockerfile_contents.as_bytes();

        let config = get_config();

        let data_plane_version = "0.0.0".to_string();
        let installer_version = "abcdef".to_string();
        let processed_file = process_dockerfile(
            &config,
            &mut readable_contents,
            data_plane_version,
            installer_version,
        )
        .await;
        assert_eq!(processed_file.is_ok(), true);
        let processed_file = processed_file.unwrap();

        let expected_output_contents = r##"FROM alpine
RUN touch /hello-script;\
    /bin/sh -c "echo -e '"'#!/bin/sh\nwhile true; do echo "hello"; sleep 2; done;\n'"' > /hello-script"
RUN mkdir -p /opt/evervault
ADD https://cage-build-assets.evervault.com/installer/abcdef.tar.gz /opt/evervault/runtime-dependencies.tar.gz
RUN cd /opt/evervault ; tar -xzf runtime-dependencies.tar.gz ; sh ./installer.sh ; rm runtime-dependencies.tar.gz
RUN echo {\"apiKeyAuth\":\"true\",\"egress\":{},\"trxLoggingEnabled\":\"true\"} > /etc/dataplane-vars
RUN mkdir -p /etc/service/user-entrypoint
RUN printf "#!/bin/sh\nsleep 5\necho \"Checking status of data-plane\"\nSVDIR=/etc/service sv check data-plane || exit 1\necho \"Data-plane up and running\"\nwhile ! grep -q \"EV_CAGE_INITIALIZED\" /etc/customer-env\n do echo \"Env not ready, sleeping user process for one second\"\n sleep 1\n done \n . /etc/customer-env\n\necho \"Booting user service...\"\ncd %s\nexec sh /hello-script\n" "$PWD"  > /etc/service/user-entrypoint/run && chmod +x /etc/service/user-entrypoint/run
ADD https://cage-build-assets.evervault.com/runtime/0.0.0/data-plane/egress-disabled/tls-termination-enabled /opt/evervault/data-plane
RUN chmod +x /opt/evervault/data-plane
RUN mkdir -p /etc/service/data-plane
RUN printf "#!/bin/sh\necho \"Booting Evervault data plane...\"\nexec /opt/evervault/data-plane 3443\n" > /etc/service/data-plane/run && chmod +x /etc/service/data-plane/run
RUN printf "#!/bin/sh\nifconfig lo 127.0.0.1\n echo \"enclave.local\" > /etc/hostname \n echo \"127.0.0.1 enclave.local\" >> /etc/hosts \n hostname -F /etc/hostname \necho \"Booting enclave...\"\nexec runsvdir /etc/service\n" > /bootstrap && chmod +x /bootstrap
RUN find $( ls / | grep -E -v "^(dev|mnt|proc|sys)$" ) -xdev | xargs touch --date="@0" --no-dereference || true
FROM scratch
COPY --from=0 / /
ENTRYPOINT ["/bootstrap", "1>&2"]
"##;

        let expected_directives = docker::parse::DockerfileDecoder::decode_dockerfile_from_src(
            expected_output_contents.as_bytes(),
        )
        .await
        .unwrap();

        assert_eq!(expected_directives.len(), processed_file.len());
        for (expected_directive, processed_directive) in
            zip(expected_directives.iter(), processed_file.iter())
        {
            let expected_directive = expected_directive.to_string();
            let processed_directive = processed_directive.to_string();
            assert_eq!(expected_directive, processed_directive);
        }
    }

    #[tokio::test]
    async fn test_choose_output_dir() {
        let output_dir = TempDir::new().unwrap();

        let _ = test_utils::build_test_cage(Some(output_dir.path().to_str().unwrap()), None).await;

        let paths = std::fs::read_dir(output_dir.path().to_str().unwrap().to_string()).unwrap();

        for path in paths {
            log::info!("Name: {}", path.unwrap().path().display())
        }

        assert!(output_dir
            .path()
            .join(super::EV_USER_DOCKERFILE_PATH)
            .exists());
        assert!(output_dir
            .path()
            .join(enclave::NITRO_CLI_IMAGE_FILENAME)
            .exists());
        assert!(output_dir.path().join(enclave::ENCLAVE_FILENAME).exists());
    }
}
