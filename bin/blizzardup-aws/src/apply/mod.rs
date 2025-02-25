use std::{
    env,
    fs::File,
    io::{self, stdout, Error, ErrorKind},
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use aws_manager::{self, cloudformation, ec2, s3, sts};
use aws_sdk_cloudformation::model::{Capability, OnFailure, Parameter, StackStatus, Tag};
use clap::{Arg, Command};
use crossterm::{
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
};
use dialoguer::{theme::ColorfulTheme, Select};
use rust_embed::RustEmbed;
use tokio::runtime::Runtime;

pub const NAME: &str = "apply";

pub fn command() -> Command {
    Command::new(NAME)
        .about("Applies/creates resources based on configuration")
        .arg(
            Arg::new("LOG_LEVEL")
                .long("log-level")
                .short('l')
                .help("Sets the log level")
                .required(false)
                .num_args(1)
                .value_parser(["debug", "info"])
                .default_value("info"),
        )
        .arg(
            Arg::new("SPEC_FILE_PATH")
                .long("spec-file-path")
                .short('s')
                .help("The spec file to load and update")
                .required(true)
                .num_args(1),
        )
        .arg(
            Arg::new("SKIP_PROMPT")
                .long("skip-prompt")
                .short('s')
                .help("Skips prompt mode")
                .required(false)
                .num_args(0),
        )
}

// 50-minute
const MAX_WAIT_SECONDS: u64 = 50 * 60;

pub fn execute(log_level: &str, spec_file_path: &str, skip_prompt: bool) -> io::Result<()> {
    #[derive(RustEmbed)]
    #[folder = "cfn-templates/"]
    #[prefix = "cfn-templates/"]
    struct Asset;

    // ref. https://github.com/env-logger-rs/env_logger/issues/47
    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, log_level),
    );

    let mut spec = blizzardup_aws::Spec::load(spec_file_path).expect("failed to load spec");
    spec.validate()?;

    let rt = Runtime::new().unwrap();

    let mut aws_resources = spec.aws_resources.clone().unwrap();
    let shared_config = rt
        .block_on(aws_manager::load_config(Some(aws_resources.region.clone())))
        .expect("failed to aws_manager::load_config");

    let sts_manager = sts::Manager::new(&shared_config);
    let current_identity = rt.block_on(sts_manager.get_identity()).unwrap();

    // validate identity
    match aws_resources.clone().identity {
        Some(identity) => {
            // AWS calls must be made from the same caller
            if identity != current_identity {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!(
                        "config identity {:?} != currently loaded identity {:?}",
                        identity, current_identity
                    ),
                ));
            }
        }
        None => {
            aws_resources.identity = Some(current_identity);
        }
    }

    // set defaults based on ID
    if aws_resources.ec2_key_name.is_none() {
        aws_resources.ec2_key_name = Some(format!("{}-ec2-key", spec.id));
    }
    if aws_resources.cloudformation_ec2_instance_role.is_none() {
        aws_resources.cloudformation_ec2_instance_role =
            Some(blizzardup_aws::StackName::Ec2InstanceRole(spec.id.clone()).encode());
    }
    if aws_resources.cloudformation_vpc.is_none() {
        aws_resources.cloudformation_vpc =
            Some(blizzardup_aws::StackName::Vpc(spec.id.clone()).encode());
    }
    if aws_resources.cloudformation_asg_blizzards.is_none() {
        aws_resources.cloudformation_asg_blizzards =
            Some(blizzardup_aws::StackName::AsgBlizzards(spec.id.clone()).encode());
    }
    spec.aws_resources = Some(aws_resources.clone());
    spec.sync(spec_file_path)?;

    execute!(
        stdout(),
        SetForegroundColor(Color::Blue),
        Print(format!("\nLoaded Spec: '{}'\n", spec_file_path)),
        ResetColor
    )?;
    let spec_contents = spec.encode_yaml()?;
    println!("{}\n", spec_contents);

    if !skip_prompt {
        let options = &[
            "No, I am not ready to create resources!",
            "Yes, let's create resources!",
        ];
        let selected = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Select your 'apply' option")
            .items(&options[..])
            .default(0)
            .interact()
            .unwrap();
        if selected == 0 {
            return Ok(());
        }
    }

    let exec_path = env::current_exe().expect("unexpected None current_exe");

    log::info!("creating resources (with spec path {})", spec_file_path);
    let s3_manager = s3::Manager::new(&shared_config);
    let ec2_manager = ec2::Manager::new(&shared_config);
    let cloudformation_manager = cloudformation::Manager::new(&shared_config);

    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&term))
        .expect("failed to register os signal");

    execute!(
        stdout(),
        SetForegroundColor(Color::Green),
        Print("\n\n\nSTEP: create S3 buckets\n"),
        ResetColor
    )?;
    rt.block_on(s3_manager.create_bucket(&aws_resources.s3_bucket))
        .unwrap();

    thread::sleep(Duration::from_secs(1));
    execute!(
        stdout(),
        SetForegroundColor(Color::Green),
        Print("\n\n\nSTEP: upload artifacts to S3 bucket\n"),
        ResetColor
    )?;

    if let Some(v) = &spec.install_artifacts.blizzard_bin {
        // don't compress since we need to download this in user data
        // while instance bootstrapping
        rt.block_on(s3_manager.put_object(
            Arc::new(v.to_string()),
            Arc::new(aws_resources.s3_bucket.clone()),
            Arc::new(blizzardup_aws::StorageNamespace::BlizzardBin(spec.id.clone()).encode()),
        ))
        .expect("failed put_object install_artifacts.blizzard_bin");
    } else {
        log::info!("skipping uploading blizzard_bin, will be downloaded on remote machines...");
    }

    log::info!("uploading blizzardup spec file...");
    rt.block_on(s3_manager.put_object(
        Arc::new(spec_file_path.to_string()),
        Arc::new(aws_resources.s3_bucket.clone()),
        Arc::new(blizzardup_aws::StorageNamespace::ConfigFile(spec.id.clone()).encode()),
    ))
    .unwrap();

    if aws_resources.ec2_key_path.is_none() {
        execute!(
            stdout(),
            SetForegroundColor(Color::Green),
            Print("\n\n\nSTEP: create EC2 key pair\n"),
            ResetColor
        )?;

        let ec2_key_path = get_ec2_key_path(spec_file_path);
        rt.block_on(ec2_manager.create_key_pair(
            aws_resources.ec2_key_name.clone().unwrap().as_str(),
            ec2_key_path.as_str(),
        ))
        .unwrap();

        rt.block_on(s3_manager.put_object(
            Arc::new(ec2_key_path.clone()),
            Arc::new(aws_resources.s3_bucket.clone()),
            Arc::new(blizzardup_aws::StorageNamespace::Ec2AccessKey(spec.id.clone()).encode()),
        ))
        .unwrap();

        aws_resources.ec2_key_path = Some(ec2_key_path);
        spec.aws_resources = Some(aws_resources.clone());
        spec.sync(spec_file_path)?;

        rt.block_on(s3_manager.put_object(
            Arc::new(spec_file_path.to_string()),
            Arc::new(aws_resources.s3_bucket.clone()),
            Arc::new(blizzardup_aws::StorageNamespace::ConfigFile(spec.id.clone()).encode()),
        ))
        .unwrap();
    }

    if aws_resources
        .cloudformation_ec2_instance_profile_arn
        .is_none()
    {
        execute!(
            stdout(),
            SetForegroundColor(Color::Green),
            Print("\n\n\nSTEP: create EC2 instance role\n"),
            ResetColor
        )?;

        let ec2_instance_role_yaml = Asset::get("cfn-templates/ec2_instance_role.yaml").unwrap();
        let ec2_instance_role_tmpl =
            std::str::from_utf8(ec2_instance_role_yaml.data.as_ref()).unwrap();
        let ec2_instance_role_stack_name = aws_resources
            .cloudformation_ec2_instance_role
            .clone()
            .unwrap();

        let role_params = Vec::from([
            build_param("Id", &spec.id),
            build_param("S3BucketName", &aws_resources.s3_bucket),
        ]);
        rt.block_on(cloudformation_manager.create_stack(
            ec2_instance_role_stack_name.as_str(),
            Some(vec![Capability::CapabilityNamedIam]),
            OnFailure::Delete,
            ec2_instance_role_tmpl,
            Some(Vec::from([
                Tag::builder().key("KIND").value("blizzardup").build(),
            ])),
            Some(role_params),
        ))
        .unwrap();

        thread::sleep(Duration::from_secs(10));
        let stack = rt
            .block_on(cloudformation_manager.poll_stack(
                ec2_instance_role_stack_name.as_str(),
                StackStatus::CreateComplete,
                Duration::from_secs(500),
                Duration::from_secs(30),
            ))
            .unwrap();

        for o in stack.outputs.unwrap() {
            let k = o.output_key.unwrap();
            let v = o.output_value.unwrap();
            log::info!("stack output key=[{}], value=[{}]", k, v,);
            if k.eq("InstanceProfileArn") {
                aws_resources.cloudformation_ec2_instance_profile_arn = Some(v)
            }
        }
        spec.aws_resources = Some(aws_resources.clone());
        spec.sync(spec_file_path)?;

        rt.block_on(s3_manager.put_object(
            Arc::new(spec_file_path.to_string()),
            Arc::new(aws_resources.s3_bucket.clone()),
            Arc::new(blizzardup_aws::StorageNamespace::ConfigFile(spec.id.clone()).encode()),
        ))
        .unwrap();
    }

    if aws_resources.cloudformation_vpc_id.is_none()
        && aws_resources.cloudformation_vpc_security_group_id.is_none()
        && aws_resources.cloudformation_vpc_public_subnet_ids.is_none()
    {
        execute!(
            stdout(),
            SetForegroundColor(Color::Green),
            Print("\n\n\nSTEP: create VPC\n"),
            ResetColor
        )?;

        let vpc_yaml = Asset::get("cfn-templates/vpc.yaml").unwrap();
        let vpc_tmpl = std::str::from_utf8(vpc_yaml.data.as_ref()).unwrap();
        let vpc_stack_name = aws_resources.cloudformation_vpc.clone().unwrap();
        let vpc_params = Vec::from([
            build_param("Id", &spec.id),
            build_param("VpcCidr", "10.0.0.0/16"),
            build_param("PublicSubnetCidr1", "10.0.64.0/19"),
            build_param("PublicSubnetCidr2", "10.0.128.0/19"),
            build_param("PublicSubnetCidr3", "10.0.192.0/19"),
            build_param("IngressIpv4Range", "0.0.0.0/0"),
        ]);
        rt.block_on(cloudformation_manager.create_stack(
            vpc_stack_name.as_str(),
            None,
            OnFailure::Delete,
            vpc_tmpl,
            Some(Vec::from([
                Tag::builder().key("KIND").value("blizzardup").build(),
            ])),
            Some(vpc_params),
        ))
        .expect("failed create_stack for VPC");

        thread::sleep(Duration::from_secs(10));
        let stack = rt
            .block_on(cloudformation_manager.poll_stack(
                vpc_stack_name.as_str(),
                StackStatus::CreateComplete,
                Duration::from_secs(300),
                Duration::from_secs(30),
            ))
            .expect("failed poll_stack for VPC");

        for o in stack.outputs.unwrap() {
            let k = o.output_key.unwrap();
            let v = o.output_value.unwrap();
            log::info!("stack output key=[{}], value=[{}]", k, v,);
            if k.eq("VpcId") {
                aws_resources.cloudformation_vpc_id = Some(v);
                continue;
            }
            if k.eq("SecurityGroupId") {
                aws_resources.cloudformation_vpc_security_group_id = Some(v);
                continue;
            }
            if k.eq("PublicSubnetIds") {
                let splits: Vec<&str> = v.split(',').collect();
                let mut pub_subnets: Vec<String> = vec![];
                for s in splits {
                    log::info!("public subnet {}", s);
                    pub_subnets.push(String::from(s));
                }
                aws_resources.cloudformation_vpc_public_subnet_ids = Some(pub_subnets);
            }
        }
        spec.aws_resources = Some(aws_resources.clone());
        spec.sync(spec_file_path)?;

        rt.block_on(s3_manager.put_object(
            Arc::new(spec_file_path.to_string()),
            Arc::new(aws_resources.s3_bucket.clone()),
            Arc::new(blizzardup_aws::StorageNamespace::ConfigFile(spec.id.clone()).encode()),
        ))
        .unwrap();
    }

    if aws_resources
        .cloudformation_asg_blizzards_logical_id
        .is_none()
    {
        execute!(
            stdout(),
            SetForegroundColor(Color::Green),
            Print(format!(
                "\n\n\nSTEP: create ASG for blizzards nodes for network Id {}\n",
                spec.blizzard_spec.network_id
            )),
            ResetColor
        )?;

        let public_subnet_ids = aws_resources
            .cloudformation_vpc_public_subnet_ids
            .clone()
            .unwrap();
        let mut asg_parameters = Vec::from([
            build_param("Id", &spec.id),
            build_param("NodeKind", "worker"),
            build_param("S3BucketName", &aws_resources.s3_bucket),
            build_param(
                "Ec2KeyPairName",
                &aws_resources.ec2_key_name.clone().unwrap(),
            ),
            build_param(
                "InstanceProfileArn",
                &aws_resources
                    .cloudformation_ec2_instance_profile_arn
                    .clone()
                    .unwrap(),
            ),
            build_param(
                "SecurityGroupId",
                &aws_resources
                    .cloudformation_vpc_security_group_id
                    .clone()
                    .unwrap(),
            ),
            build_param("PublicSubnetIds", &public_subnet_ids.join(",")),
            build_param("Arch", &spec.machine.arch),
        ]);

        if !spec.machine.instance_types.is_empty() {
            let instance_types = spec.machine.instance_types.clone();
            asg_parameters.push(build_param("InstanceTypes", &instance_types.join(",")));
            asg_parameters.push(build_param(
                "InstanceTypesCount",
                format!("{}", instance_types.len()).as_str(),
            ));
        }

        let blizzard_download_source = if spec.install_artifacts.blizzard_bin.is_some() {
            "s3"
        } else {
            "github"
        };
        asg_parameters.push(build_param(
            "BlizzardDownloadSource",
            blizzard_download_source,
        ));

        let cloudformation_asg_blizzards_yaml =
            Asset::get("cfn-templates/asg_amd64_ubuntu.yaml").unwrap();
        let cloudformation_asg_blizzards_tmpl =
            std::str::from_utf8(cloudformation_asg_blizzards_yaml.data.as_ref()).unwrap();
        let cloudformation_asg_blizzards_stack_name =
            aws_resources.cloudformation_asg_blizzards.clone().unwrap();

        let desired_capacity = spec.machine.nodes;

        let is_spot_instance = spec.machine.use_spot_instance;
        let on_demand_pct = if is_spot_instance { 0 } else { 100 };
        asg_parameters.push(build_param(
            "AsgSpotInstance",
            format!("{}", is_spot_instance).as_str(),
        ));
        asg_parameters.push(build_param(
            "OnDemandPercentageAboveBaseCapacity",
            format!("{}", on_demand_pct).as_str(),
        ));

        asg_parameters.push(build_param(
            "AsgDesiredCapacity",
            format!("{}", desired_capacity).as_str(),
        ));

        // for CFN template updates
        // ref. "Temporarily setting autoscaling group MinSize and DesiredCapacity to 2."
        // ref. "Rolling update initiated. Terminating 1 obsolete instance(s) in batches of 1, while keeping at least 1 instance(s) in service."
        asg_parameters.push(build_param(
            "AsgMaxSize",
            format!("{}", desired_capacity + 1).as_str(),
        ));

        rt.block_on(cloudformation_manager.create_stack(
            cloudformation_asg_blizzards_stack_name.as_str(),
            None,
            OnFailure::Delete,
            cloudformation_asg_blizzards_tmpl,
            Some(Vec::from([
                Tag::builder().key("KIND").value("blizzardup").build(),
            ])),
            Some(asg_parameters),
        ))
        .unwrap();

        // add 5-minute for ELB creation + volume provisioner
        let mut wait_secs = 700 + 60 * desired_capacity as u64;
        if wait_secs > MAX_WAIT_SECONDS {
            wait_secs = MAX_WAIT_SECONDS;
        }
        thread::sleep(Duration::from_secs(60));
        let stack = rt
            .block_on(cloudformation_manager.poll_stack(
                cloudformation_asg_blizzards_stack_name.as_str(),
                StackStatus::CreateComplete,
                Duration::from_secs(wait_secs),
                Duration::from_secs(30),
            ))
            .unwrap();

        for o in stack.outputs.unwrap() {
            let k = o.output_key.unwrap();
            let v = o.output_value.unwrap();
            log::info!("stack output key=[{}], value=[{}]", k, v,);
            if k.eq("AsgLogicalId") {
                aws_resources.cloudformation_asg_blizzards_logical_id = Some(v);
                continue;
            }
        }
        if aws_resources
            .cloudformation_asg_blizzards_logical_id
            .is_none()
        {
            return Err(Error::new(
                ErrorKind::Other,
                "aws_resources.cloudformation_asg_blizzards_logical_id not found",
            ));
        }

        spec.aws_resources = Some(aws_resources.clone());
        spec.sync(spec_file_path)?;

        let asg_name = aws_resources
            .cloudformation_asg_blizzards_logical_id
            .clone()
            .expect("unexpected None cloudformation_asg_blizzards_logical_id");

        let mut droplets: Vec<ec2::Droplet> = Vec::new();
        let target_nodes = spec.machine.nodes;
        for _ in 0..20 {
            // TODO: better retries
            log::info!(
                "fetching all droplets for non-anchor node SSH access (target nodes {})",
                target_nodes
            );
            droplets = rt.block_on(ec2_manager.list_asg(&asg_name)).unwrap();
            if droplets.len() >= target_nodes {
                break;
            }
            log::info!(
                "retrying fetching all droplets (only got {})",
                droplets.len()
            );
            thread::sleep(Duration::from_secs(30));
        }

        let ec2_key_path = aws_resources.ec2_key_path.clone().unwrap();
        let f = File::open(&ec2_key_path).unwrap();
        f.set_permissions(PermissionsExt::from_mode(0o444)).unwrap();
        println!(
            "
# change SSH key permission
chmod 400 {}",
            ec2_key_path
        );
        for d in droplets {
            // ssh -o "StrictHostKeyChecking no" -i [ec2_key_path] [user name]@[public IPv4/DNS name]
            // aws ssm start-session --region [region] --target [instance ID]
            println!(
                "# instance '{}' ({}, {})
ssh -o \"StrictHostKeyChecking no\" -i {} ubuntu@{}
# download to local machine
scp -i {} ubuntu@{}:REMOTE_FILE_PATH LOCAL_FILE_PATH
scp -i {} -r ubuntu@{}:REMOTE_DIRECTORY_PATH LOCAL_DIRECTORY_PATH
# upload to remote machine
scp -i {} LOCAL_FILE_PATH ubuntu@{}:REMOTE_FILE_PATH
scp -i {} -r LOCAL_DIRECTORY_PATH ubuntu@{}:REMOTE_DIRECTORY_PATH
# SSM session (requires SSM agent)
aws ssm start-session --region {} --target {}
",
                //
                d.instance_id,
                d.instance_state_name,
                d.availability_zone,
                //
                ec2_key_path,
                d.public_ipv4,
                //
                ec2_key_path,
                d.public_ipv4,
                //
                ec2_key_path,
                d.public_ipv4,
                //
                ec2_key_path,
                d.public_ipv4,
                //
                ec2_key_path,
                d.public_ipv4,
                //
                aws_resources.region,
                d.instance_id,
            );
        }
        println!();

        spec.aws_resources = Some(aws_resources.clone());
        spec.sync(spec_file_path)?;

        rt.block_on(s3_manager.put_object(
            Arc::new(spec_file_path.to_string()),
            Arc::new(aws_resources.s3_bucket.clone()),
            Arc::new(blizzardup_aws::StorageNamespace::ConfigFile(spec.id.clone()).encode()),
        ))
        .expect("failed put_object ConfigFile");

        log::info!("waiting for non-anchor nodes bootstrap and ready (to be safe)");
        thread::sleep(Duration::from_secs(20));

        // TODO: check some results by querying metrics

        if term.load(Ordering::Relaxed) {
            log::warn!("received signal {}", signal_hook::consts::SIGINT);
            println!();
            println!("# run the following to delete resources");
            execute!(
                stdout(),
                SetForegroundColor(Color::Green),
                Print(format!(
                    "{} delete \\\n--delete-cloudwatch-log-group \\\n--delete-s3-objects \\\n--spec-file-path {}\n",
                    exec_path.display(),
                    spec_file_path
                )),
                ResetColor
            )?;
        };
    }

    Ok(())
}

fn build_param(k: &str, v: &str) -> Parameter {
    Parameter::builder()
        .parameter_key(k)
        .parameter_value(v)
        .build()
}

fn get_ec2_key_path(spec_file_path: &str) -> String {
    let path = Path::new(spec_file_path);
    let parent_dir = path.parent().unwrap();
    let name = path.file_stem().unwrap();
    let new_name = format!("{}-ec2-access.key", name.to_str().unwrap(),);
    String::from(
        parent_dir
            .join(Path::new(new_name.as_str()))
            .as_path()
            .to_str()
            .unwrap(),
    )
}
