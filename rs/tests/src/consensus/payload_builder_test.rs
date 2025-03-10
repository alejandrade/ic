/* tag::catalog[]
Title:: Payload Builder Size Tests

Goal:: Test the consensus payload builder and the accompaning payload validator.

Runbook::
. Set up two subnets with one fast node each
. Install a universal canister in both, one is called target canister the other assist canister.
. The assist canister will be used to send the xnet data to the target canister.
. Send ingress message to target canister, that is slightly below maximum size. Expect it to succeed.
. Send xnet message to target canister, that is slightly below maximum size. Expect it to succeed.
. Send a bunch of large xnet and ingress messages to the same canister. Expect it to handle all of them eventually.

Success:: The payload builder respects the boundaries set by the registry, while the payload validator
accepts all payloads generated by the payload builder.

Coverage::
. The maximum size of an individual ingress message is respected.
. The maximum size of an individual xnet message is respected.
. The system handles well under the load of large ingress messages and xnet messages at the same time.

end::catalog[] */

use crate::{
    driver::{
        ic::{InternetComputer, Subnet},
        test_env::TestEnv,
        test_env_api::{
            HasGroupSetup, HasPublicApiUrl, HasTopologySnapshot, IcNodeContainer, TopologySnapshot,
        },
    },
    util::UniversalCanister,
};
use futures::{join, stream::FuturesUnordered, StreamExt};
use ic_agent::{Agent, AgentError};
use ic_base_types::PrincipalId;
use ic_registry_subnet_type::SubnetType;
use ic_universal_canister::{call_args, wasm};
use slog::{info, Logger};
use std::sync::Arc;

const INGRESS_MAX_SIZE: usize = 4 * 1024 * 1024;
const XNET_MAX_SIZE: usize = 2 * 1024 * 1024;

// NOTE: Unfortunately, the maximum sizes can not be used in the test.
// In both cases, we have to account for the extra command bytes of the unican,
// that preceed the message. In the case of the ingress messages, we also have
// to account for the ingress message header.
const INGRESS_MSG_SIZE: usize = 4 * 1024 * 1024 - 360;
const XNET_MSG_SIZE: usize = 2 * 1024 * 1024 - 20;

/// Configuration that is used for the maximum payload size tests.
/// In this test, the payload size is set to 2MiB and the ingress size to 4MiB.
/// This allows us to test, that in a misconfigured setting, the specified
/// max_block_payload_size still fits through.
/// It also allows us to test the limit in the XNet setting properly.
pub fn max_payload_size_config(env: TestEnv) {
    env.ensure_group_setup_created();
    InternetComputer::new()
        .add_subnet(
            Subnet::new(SubnetType::System)
                .add_nodes(1)
                .with_max_block_payload_size(XNET_MAX_SIZE as u64)
                .with_max_ingress_message_size(INGRESS_MAX_SIZE as u64),
        )
        .add_subnet(
            Subnet::new(SubnetType::Application)
                .add_nodes(1)
                .with_max_block_payload_size(XNET_MAX_SIZE as u64)
                .with_max_ingress_message_size(INGRESS_MAX_SIZE as u64),
        )
        .setup_and_start(&env)
        .expect("failed to setup IC under test");
}

const DW_NUM_MSGS: usize = 32;
const DW_MAX_SIZE: usize = 2 * 1024 * 1024;
const DW_MSG_SIZE: usize = 2 * 1000 * 1000;

/// The configuration that is used for the dual workload test.
/// In this configuration, all sizes are set to 2MiB.
pub fn dual_workload_config(env: TestEnv) {
    env.ensure_group_setup_created();
    InternetComputer::new()
        .add_subnet(
            Subnet::new(SubnetType::System)
                .add_nodes(1)
                .with_max_block_payload_size(DW_MAX_SIZE as u64)
                .with_max_ingress_message_size(DW_MAX_SIZE as u64),
        )
        .add_subnet(Subnet::new(SubnetType::Application).add_nodes(1))
        .setup_and_start(&env)
        .expect("failed to setup IC under test");
}

#[derive(Debug)]
enum PayloadType {
    Ingress(usize),
    XNet(usize),
}

/// Tests, that an ingress message that is close to the maximum size is accepted
/// by the block maker, whereas a message this is exactly the maximum size is
/// not accepted.
pub fn max_ingress_payload_size_test(env: TestEnv) {
    let log = env.logger();
    let topology = env.topology_snapshot();
    info!(log, "Checking readiness of all nodes after the IC setup...");
    topology.subnets().for_each(|subnet| {
        subnet
            .nodes()
            .for_each(|node| node.await_status_is_healthy().unwrap())
    });
    info!(log, "All nodes are ready, IC setup succeeded.");
    let (
        (assist_agent, assist_effective_canister_id),
        (target_agent, target_effective_canister_id),
    ) = setup_agents(topology);
    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");
    rt.block_on(async move {
        let (_, target_unican) = setup_unicans(
            &assist_agent,
            assist_effective_canister_id,
            &target_agent,
            target_effective_canister_id,
        )
        .await;

        // Send a message that is supposed to fit.
        make_ingress_call(&target_unican, INGRESS_MSG_SIZE)
            .await
            .unwrap();
    })
}

/// Tests, that a xnet message that is close to the maximum size is accepted
/// by the block maker, whereas a message this is exactly the maximum size is
/// not accepted.
pub fn max_xnet_payload_size_test(env: TestEnv) {
    let log = env.logger();
    let topology = env.topology_snapshot();
    info!(log, "Checking readiness of all nodes after the IC setup...");
    topology.subnets().for_each(|subnet| {
        subnet
            .nodes()
            .for_each(|node| node.await_status_is_healthy().unwrap())
    });
    info!(log, "All nodes are ready, IC setup succeeded.");
    let (
        (assist_agent, assist_effective_canister_id),
        (target_agent, target_effective_canister_id),
    ) = setup_agents(topology);
    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");
    rt.block_on(async move {
        let (assist_unican, target_unican) = setup_unicans(
            &assist_agent,
            assist_effective_canister_id,
            &target_agent,
            target_effective_canister_id,
        )
        .await;

        // Send a message that is supposed to fit.
        make_xnet_call(&target_unican, &assist_unican, XNET_MSG_SIZE)
            .await
            .unwrap();
    });
}

/// Tests, that the internet computer behaves well, when there is a high load of
/// ingress messages and xnet messages on the same subnet.
pub fn dual_workload_test(env: TestEnv) {
    let log = env.logger();
    let topology = env.topology_snapshot();
    info!(log, "Checking readiness of all nodes after the IC setup...");
    topology.subnets().for_each(|subnet| {
        subnet
            .nodes()
            .for_each(|node| node.await_status_is_healthy().unwrap())
    });
    info!(log, "All nodes are ready, IC setup succeeded.");
    let (
        (assist_agent, assist_effective_canister_id),
        (target_agent, target_effective_canister_id),
    ) = setup_agents(topology);
    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");
    rt.block_on(async move {
        let (assist_unican, target_unican) = setup_unicans(
            &assist_agent,
            assist_effective_canister_id,
            &target_agent,
            target_effective_canister_id,
        )
        .await;

        let calls = (0..DW_NUM_MSGS)
            .flat_map(|x| vec![PayloadType::XNet(x), PayloadType::Ingress(x)])
            .map(|report| {
                (
                    target_unican.clone(),
                    assist_unican.clone(),
                    report,
                    log.clone(),
                )
            })
            .map(|(target_unican, assist_unican, report, logger)| {
                make_dual_call(target_unican, assist_unican, report, DW_MSG_SIZE, logger)
            })
            .collect::<FuturesUnordered<_>>()
            .collect::<Vec<_>>();

        info!(log, "Calls are setup, will be submitted now");
        let reports = calls.await;
        info!(log, "Report: {:?}", reports)
    });
}

fn setup_agents(
    topology_snapshot: TopologySnapshot,
) -> ((Agent, PrincipalId), (Agent, PrincipalId)) {
    let target_node_nns = topology_snapshot.root_subnet().nodes().next().unwrap();
    let assist_node_app = topology_snapshot
        .subnets()
        .find(|s| s.subnet_type() == SubnetType::Application)
        .unwrap()
        .nodes()
        .next()
        .unwrap();
    let assist_agent_app = assist_node_app.with_default_agent(|agent| async move { agent });
    let target_agent_nns = target_node_nns.with_default_agent(|agent| async move { agent });
    (
        (assist_agent_app, assist_node_app.effective_canister_id()),
        (target_agent_nns, target_node_nns.effective_canister_id()),
    )
}

async fn setup_unicans<'a>(
    assist_agent: &'a Agent,
    assist_effective_canister_id: PrincipalId,
    target_agent: &'a Agent,
    target_effective_canister_id: PrincipalId,
) -> (Arc<UniversalCanister<'a>>, Arc<UniversalCanister<'a>>) {
    // Install a `UniversalCanister` on each
    let (assist_unican, target_unican) = join!(
        UniversalCanister::new(assist_agent, assist_effective_canister_id),
        UniversalCanister::new(target_agent, target_effective_canister_id)
    );

    // NOTE: Since we will be making calls to these canisters in parallel, we have
    // to make it `Send`.
    let (assist_unican, target_unican) = (Arc::new(assist_unican), Arc::new(target_unican));

    // Grow the stable memory so it can actually store the amount of data
    join!(
        stable_grow(&assist_unican, 100),
        stable_grow(&target_unican, 100)
    );

    (assist_unican, target_unican)
}

/// Used in the dual workload test. Depending on the `PayloadType`, the function
/// will either make an ingress call to `target_unican` or a Xnet call from
/// `assist_unican` to `target_unican`.
async fn make_dual_call<'a>(
    target_unican: Arc<UniversalCanister<'a>>,
    assist_unican: Arc<UniversalCanister<'a>>,
    call_ctx: PayloadType,
    size: usize,
    logger: Logger,
) -> PayloadType {
    match call_ctx {
        PayloadType::XNet(i) => {
            make_xnet_call(&target_unican, &assist_unican, size)
                .await
                .unwrap();
            info!(logger, "XNet call {:?} finished", i);
        }
        PayloadType::Ingress(i) => {
            make_ingress_call(&target_unican, size).await.unwrap();
            info!(logger, "Ingress call {:?} finished", i);
        }
    }

    call_ctx
}

/// Makes an ingress call to the specified canister with a message of the
/// specified size.
async fn make_ingress_call(
    dst: &UniversalCanister<'_>,
    size: usize,
) -> Result<Vec<u8>, AgentError> {
    // NOTE: We use reply here before stable write, since we don't actually
    // care about the write, we just want to send a large message.
    dst.update(wasm().reply().stable_write(0, &vec![0; size]))
        .await
}

/// Makes a XNet call from the `src` canister to the `dst` canister with a
/// message of the specified size
async fn make_xnet_call(
    dst: &UniversalCanister<'_>,
    src: &UniversalCanister<'_>,
    size: usize,
) -> Result<Vec<u8>, AgentError> {
    src.update(
        wasm().inter_update(
            dst.canister_id(),
            call_args()
                // NOTE: We use reply here before stable write, since we don't actually
                // care about the write, we just want to send a large message.
                .other_side(wasm().reply().stable_write(0, &vec![0; size]))
                .on_reply(wasm().reply()),
        ),
    )
    .await
}

/// Grow the canisters stable memory by the given number of pages
async fn stable_grow(unican: &UniversalCanister<'_>, num_pages: u32) {
    unican
        .update(wasm().stable_grow(num_pages).reply())
        .await
        .unwrap();
}
