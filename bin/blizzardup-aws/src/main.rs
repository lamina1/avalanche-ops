mod apply;
mod default_spec;
mod delete;
mod query;

use clap::{crate_version, Command};

const APP_NAME: &str = "blizzardup-aws";

/// Should be able to run with idempotency
/// (e.g., multiple restarts should not recreate the same CloudFormation stacks)
fn main() {
    let matches = Command::new(APP_NAME)
        .version(crate_version!())
        .about("Blizzard control plane on AWS (requires blizzard)")
        .subcommands(vec![
            default_spec::command(),
            apply::command(),
            delete::command(),
            query::command(),
        ])
        .get_matches();

    match matches.subcommand() {
        Some((default_spec::NAME, sub_matches)) => {
            let keys_to_generate = sub_matches
                .get_one::<usize>("KEYS_TO_GENERATE")
                .unwrap_or(&5)
                .clone();

            let nodes = sub_matches.get_one::<usize>("NODES").unwrap_or(&2).clone();
            let network_id = sub_matches
                .get_one::<u32>("NETWORK_ID")
                .unwrap_or(&2000777)
                .clone();

            let blizzard_http_rpcs_str = sub_matches
                .get_one::<String>("BLIZZARD_HTTP_RPCS")
                .unwrap()
                .clone();
            let blizzard_http_rpcs_str: Vec<&str> = blizzard_http_rpcs_str.split(',').collect();
            let mut blizzard_http_rpcs: Vec<String> = Vec::new();
            for rpc in blizzard_http_rpcs_str.iter() {
                blizzard_http_rpcs.push(rpc.to_string());
            }

            let blizzard_subnet_evm_blockchain_id_str = sub_matches
                .get_one::<String>("BLIZZARD_SUBNET_EVM_BLOCKCHAIN_ID")
                .unwrap_or(&String::new())
                .to_string();
            let blizzard_subnet_evm_blockchain_id =
                if blizzard_subnet_evm_blockchain_id_str.is_empty() {
                    None
                } else {
                    Some(blizzard_subnet_evm_blockchain_id_str.clone())
                };

            let blizzard_load_kinds_str = sub_matches
                .get_one::<String>("BLIZZARD_LOAD_KINDS")
                .unwrap()
                .clone();
            let blizzard_load_kinds_str: Vec<&str> = blizzard_load_kinds_str.split(',').collect();
            let mut blizzard_load_kinds: Vec<String> = Vec::new();
            for lk in blizzard_load_kinds_str.iter() {
                blizzard_load_kinds.push(lk.to_string());
            }

            let blizzard_metrics_push_interval_seconds = sub_matches
                .get_one::<u64>("BLIZZARD_METRICS_PUSH_INTERVAL_SECONDS")
                .unwrap_or(&60)
                .clone();

            let blizzard_gas = sub_matches
                .get_one::<u64>("BLIZZARD_GAS")
                .unwrap_or(&0)
                .clone();

            let blizzard_gas_price = sub_matches
                .get_one::<u64>("BLIZZARD_GAS_PRICE")
                .unwrap_or(&0)
                .clone();

            let opt = blizzardup_aws::DefaultSpecOption {
                log_level: sub_matches
                    .get_one::<String>("LOG_LEVEL")
                    .unwrap_or(&String::from("info"))
                    .clone(),

                keys_to_generate,

                region: sub_matches.get_one::<String>("REGION").unwrap().clone(),
                use_spot_instance: sub_matches.get_flag("USE_SPOT_INSTANCE"),

                nodes,
                network_id,

                install_artifacts_blizzard_bin: sub_matches
                    .get_one::<String>("INSTALL_ARTIFACTS_BLIZZARD_BIN")
                    .unwrap_or(&String::new())
                    .to_string(),
                blizzard_log_level: sub_matches
                    .get_one::<String>("BLIZZARD_LOG_LEVEL")
                    .unwrap_or(&String::from("info"))
                    .to_string(),
                blizzard_http_rpcs,
                blizzard_subnet_evm_blockchain_id,
                blizzard_load_kinds,
                blizzard_metrics_push_interval_seconds,
                blizzard_gas,
                blizzard_gas_price,

                spec_file_path: sub_matches
                    .get_one::<String>("SPEC_FILE_PATH")
                    .unwrap_or(&String::new())
                    .clone(),
            };
            default_spec::execute(opt).expect("failed to execute 'default-spec'");
        }

        Some((apply::NAME, sub_matches)) => {
            apply::execute(
                &sub_matches
                    .get_one::<String>("LOG_LEVEL")
                    .unwrap_or(&String::from("info"))
                    .clone(),
                &sub_matches
                    .get_one::<String>("SPEC_FILE_PATH")
                    .unwrap()
                    .clone(),
                sub_matches.get_flag("SKIP_PROMPT"),
            )
            .expect("failed to execute 'apply'");
        }

        Some((delete::NAME, sub_matches)) => {
            delete::execute(
                &sub_matches
                    .get_one::<String>("LOG_LEVEL")
                    .unwrap_or(&String::from("info"))
                    .clone(),
                &sub_matches
                    .get_one::<String>("SPEC_FILE_PATH")
                    .unwrap()
                    .clone(),
                sub_matches.get_flag("DELETE_CLOUDWATCH_LOG_GROUP"),
                sub_matches.get_flag("DELETE_S3_OBJECTS"),
                sub_matches.get_flag("DELETE_S3_BUCKET"),
                sub_matches.get_flag("SKIP_PROMPT"),
            )
            .expect("failed to execute 'delete'");
        }

        Some((query::NAME, sub_matches)) => {
            query::execute(
                &sub_matches
                    .get_one::<String>("LOG_LEVEL")
                    .unwrap_or(&String::from("info"))
                    .clone(),
                &sub_matches
                    .get_one::<String>("SPEC_FILE_PATH")
                    .unwrap()
                    .clone(),
            )
            .expect("failed to execute 'delete'");
        }

        _ => unreachable!("unknown subcommand"),
    }
}
