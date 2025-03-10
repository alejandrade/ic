/* tag::catalog[]
end::catalog[] */

use crate::{
    driver::{pot_dsl::get_ic_handle_and_ctx, test_env::TestEnv},
    types::RejectCode,
    util::*,
};
use ic_types::Cycles;
use ic_universal_canister::{call_args, wasm};

pub fn can_transfer_cycles_from_a_canister_to_another(env: TestEnv) {
    let (handle, ref ctx) = get_ic_handle_and_ctx(env);
    let mut rng = ctx.rng.clone();
    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");
    rt.block_on({
        async move {
            let endpoint = get_random_nns_node_endpoint(&handle, &mut rng);
            endpoint.assert_ready(ctx).await;
            let agent = assert_create_agent(endpoint.url.as_str()).await;

            // Create a canister, called "Alice", using the provisional API. Alice will
            // receive some cycles.
            let alice = UniversalCanister::new_with_cycles(
                &agent,
                endpoint.effective_canister_id(),
                100_000_000u64,
            )
            .await;

            // Create a canister, called "Bob", using the provisional API. Bob will send
            // some cycles to Alice.
            let bob = UniversalCanister::new(&agent, endpoint.effective_canister_id()).await;

            let initial_alice_balance = get_balance(&alice.canister_id(), &agent).await;
            let initial_bob_balance = get_balance(&bob.canister_id(), &agent).await;

            let cycles_to_send = 500_000_000;
            let accept_cycles = Cycles::from(cycles_to_send / 2);

            // Bob sends Alice some cycles and Alice accepts half of them.
            bob.update(
                wasm()
                    .call_with_cycles(
                        alice.canister_id(),
                        "update",
                        call_args().other_side(wasm().accept_cycles128(accept_cycles.into_parts())),
                        Cycles::from(cycles_to_send).into_parts(),
                    )
                    .reply(),
            )
            .await
            .unwrap();

            // Final cycles balance should reflect the transfer.
            let final_alice_balance = get_balance(&alice.canister_id(), &agent).await;
            let final_bob_balance = get_balance(&bob.canister_id(), &agent).await;

            assert_eq!(
                final_alice_balance,
                initial_alice_balance + cycles_to_send / 2
            );
            assert_eq!(final_bob_balance, initial_bob_balance - cycles_to_send / 2);
        }
    })
}

pub fn trapping_with_large_blob_does_not_cause_cycles_underflow(env: TestEnv) {
    let (handle, ref ctx) = get_ic_handle_and_ctx(env);
    let mut rng = ctx.rng.clone();
    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");
    let initial_balance = 123_000_000_000_000u64;
    rt.block_on({
        async move {
            let endpoint = get_random_verified_app_node_endpoint(&handle, &mut rng);
            endpoint.assert_ready(ctx).await;

            let agent = assert_create_agent(endpoint.url.as_str()).await;
            let canister = UniversalCanister::new_with_cycles(
                &agent,
                endpoint.effective_canister_id(),
                initial_balance,
            )
            .await;

            assert_reject(
                canister
                    .update(wasm().inter_update(
                        canister.canister_id(),
                        // Trap with a large blob.
                        call_args().other_side(wasm().trap_with_blob(&[0; 1024 * 1024 * 3])),
                    ))
                    .await,
                RejectCode::CanisterReject,
            );

            // Assert that the balance did not underflow.
            assert!(get_balance(&canister.canister_id(), &agent).await <= initial_balance as u128);
        }
    });
}

pub fn rejecting_with_large_blob_does_not_cause_cycles_underflow(env: TestEnv) {
    let (handle, ref ctx) = get_ic_handle_and_ctx(env);
    let mut rng = ctx.rng.clone();
    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");
    let initial_balance = 123_000_000_000_000u64;
    rt.block_on({
        async move {
            let endpoint = get_random_verified_app_node_endpoint(&handle, &mut rng);
            endpoint.assert_ready(ctx).await;

            let agent = assert_create_agent(endpoint.url.as_str()).await;
            let canister = UniversalCanister::new_with_cycles(
                &agent,
                endpoint.effective_canister_id(),
                initial_balance,
            )
            .await;

            assert_reject(
                canister
                    .update(wasm().inter_update(
                        canister.canister_id(),
                        call_args().other_side(wasm().push_bytes(&[0; 1024 * 1024 * 2]).reject()),
                    ))
                    .await,
                RejectCode::CanisterReject,
            );

            // Assert that the balance did not underflow.
            assert!(get_balance(&canister.canister_id(), &agent).await <= initial_balance as u128);
        }
    });
}
