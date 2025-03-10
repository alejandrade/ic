use crate::cli::{print_height_info, read_optional_ip, read_optional_version};
use crate::file_sync_helper::create_dir;
use crate::recovery_iterator::RecoveryIterator;
use crate::RecoveryResult;
use crate::{error::RecoveryError, RecoveryArgs};
use clap::Parser;
use ic_base_types::SubnetId;
use ic_types::ReplicaVersion;
use slog::Logger;
use std::net::IpAddr;
use std::path::PathBuf;
use strum::IntoEnumIterator;
use strum_macros::EnumIter;

use crate::{Recovery, Step};

#[derive(Debug, Copy, Clone, EnumIter)]
pub enum StepType {
    StopReplica,
    DownloadState,
    ICReplay,
    ValidateReplayOutput,
    UpdateRegistryLocalStore,
    CreateTars,
    CopyIcState,
    GetRecoveryCUP,
    UploadCUPandRegistry,
    WaitForCUP,
    UploadState,
    Cleanup,
}

#[derive(Parser)]
#[clap(version = "1.0")]
pub struct NNSRecoverySameNodesArgs {
    /// Id of the broken subnet
    #[clap(long, parse(try_from_str=crate::util::subnet_id_from_str))]
    pub subnet_id: SubnetId,

    /// Replica version to upgrade the broken subnet to
    #[clap(long, parse(try_from_str=::std::convert::TryFrom::try_from))]
    pub upgrade_version: Option<ReplicaVersion>,

    /// Public ssh key to be deployed to the subnet for read only access
    #[clap(long)]
    pub pub_key: Option<String>,

    /// IP address of the node to download the subnet state from. Should be different to node used in nns-url.
    #[clap(long)]
    pub download_node: Option<IpAddr>,

    /// IP address of the node to upload the new subnet state to
    #[clap(long)]
    pub upload_node: Option<IpAddr>,
}

pub struct NNSRecoverySameNodes {
    step_iterator: Box<dyn Iterator<Item = StepType>>,
    pub params: NNSRecoverySameNodesArgs,
    pub recovery: Recovery,
    interactive: bool,
    logger: Logger,
    new_state_dir: PathBuf,
}

impl NNSRecoverySameNodes {
    pub fn new(
        logger: Logger,
        recovery_args: RecoveryArgs,
        subnet_args: NNSRecoverySameNodesArgs,
        test: bool,
        interactive: bool,
    ) -> Self {
        let recovery = Recovery::new(logger.clone(), recovery_args, None, !test)
            .expect("Failed to init recovery");
        recovery.init_registry_local_store();
        let new_state_dir = recovery.work_dir.join("new_ic_state");
        create_dir(&new_state_dir).expect("Failed to create state directory for upload.");
        Self {
            step_iterator: Box::new(StepType::iter()),
            params: subnet_args,
            recovery,
            logger,
            new_state_dir,
            interactive,
        }
    }

    pub fn get_recovery_api(&self) -> &Recovery {
        &self.recovery
    }
}

impl RecoveryIterator<StepType> for NNSRecoverySameNodes {
    fn get_step_iterator(&mut self) -> &mut Box<dyn Iterator<Item = StepType>> {
        &mut self.step_iterator
    }

    fn get_logger(&self) -> &Logger {
        &self.logger
    }

    fn interactive(&self) -> bool {
        self.interactive
    }

    fn read_step_params(&mut self, step_type: StepType) {
        match step_type {
            StepType::StopReplica => {
                print_height_info(
                    &self.logger,
                    self.recovery.registry_client.clone(),
                    self.params.subnet_id,
                );

                if self.params.download_node.is_none() {
                    self.params.download_node =
                        read_optional_ip(&self.logger, "Enter download IP:");
                }
            }

            StepType::ICReplay => {
                if self.params.upgrade_version.is_none() {
                    self.params.upgrade_version =
                        read_optional_version(&self.logger, "Upgrade version: ");
                }
            }

            StepType::WaitForCUP => {
                if self.params.upload_node.is_none() {
                    self.params.upload_node = read_optional_ip(&self.logger, "Enter upload IP:");
                }
            }

            _ => {}
        }
    }

    fn get_step_impl(&self, step_type: StepType) -> RecoveryResult<Box<dyn Step>> {
        match step_type {
            StepType::StopReplica => {
                if let Some(node_ip) = self.params.download_node {
                    Ok(Box::new(self.recovery.get_stop_replica_step(node_ip)))
                } else {
                    Err(RecoveryError::StepSkipped)
                }
            }

            StepType::DownloadState => {
                if let Some(node_ip) = self.params.download_node {
                    Ok(Box::new(
                        self.recovery.get_download_state_step(node_ip, false),
                    ))
                } else {
                    Err(RecoveryError::StepSkipped)
                }
            }

            StepType::ICReplay => {
                if let Some(upgrade_version) = self.params.upgrade_version.clone() {
                    Ok(Box::new(self.recovery.get_replay_with_upgrade_step(
                        self.params.subnet_id,
                        upgrade_version,
                    )?))
                } else {
                    Ok(Box::new(self.recovery.get_replay_step(
                        self.params.subnet_id,
                        None,
                        None,
                    )))
                }
            }
            StepType::ValidateReplayOutput => Ok(Box::new(self.recovery.get_validate_replay_step(
                self.params.subnet_id,
                if self.params.upgrade_version.is_some() {
                    1
                } else {
                    0
                },
            ))),

            StepType::UpdateRegistryLocalStore => {
                if self.params.upgrade_version.is_none() {
                    Err(RecoveryError::StepSkipped)
                } else {
                    Ok(Box::new(
                        self.recovery
                            .get_update_local_store_step(self.params.subnet_id),
                    ))
                }
            }

            StepType::CreateTars => Ok(Box::new(self.recovery.get_create_tars_step())),

            StepType::CopyIcState => Ok(Box::new(
                self.recovery.get_copy_ic_state(self.new_state_dir.clone()),
            )),

            StepType::GetRecoveryCUP => Ok(Box::new(
                self.recovery.get_recovery_cup_step(self.params.subnet_id)?,
            )),

            StepType::UploadCUPandRegistry => Ok(Box::new(
                self.recovery
                    .get_upload_cup_and_tar_step(self.params.subnet_id),
            )),

            StepType::WaitForCUP => {
                if let Some(node_ip) = self.params.upload_node {
                    Ok(Box::new(self.recovery.get_wait_for_cup_step(node_ip)))
                } else {
                    Err(RecoveryError::StepSkipped)
                }
            }

            StepType::UploadState => {
                if let Some(node_ip) = self.params.upload_node {
                    Ok(Box::new(
                        self.recovery.get_upload_and_restart_step_with_data_src(
                            node_ip,
                            self.new_state_dir.clone(),
                        ),
                    ))
                } else {
                    Err(RecoveryError::StepSkipped)
                }
            }

            StepType::Cleanup => Ok(Box::new(self.recovery.get_cleanup_step())),
        }
    }
}
