//! Utilities to help with testing interleavings of calls to the governance
//! canister
use async_trait::async_trait;
use futures::channel::{
    mpsc::UnboundedSender as USender,
    oneshot::{self, Sender as OSender},
};
use ic_base_types::CanisterId;
use ic_icrc1::{Account, Subaccount};
use ic_ledger_core::Tokens;
use ic_nervous_system_common::NervousSystemError;
use ic_sns_governance::ledger::ICRC1Ledger;
use std::sync::{atomic, atomic::Ordering as AtomicOrdering};

/// Reifies the methods of the Ledger trait, such that they can be sent over a
/// channel
#[derive(Debug)]
pub enum LedgerMessage {
    Transfer {
        amount_e8s: u64,
        fee_e8s: u64,
        from_subaccount: Option<Subaccount>,
        to: Account,
        memo: u64,
    },
    TotalSupply,
    BalanceQuery(Account),
}

pub type LedgerControlMessage = (LedgerMessage, OSender<Result<(), NervousSystemError>>);

pub type LedgerObserver = USender<LedgerControlMessage>;

/// A mock ledger to test interleavings of governance method calls.
pub struct InterleavingTestLedger {
    underlying: Box<dyn ICRC1Ledger>,
    observer: LedgerObserver,
}

impl InterleavingTestLedger {
    /// The ledger intercepts calls to an underlying ledger implementation,
    /// sends the reified calls over the provided observer channel, and
    /// blocks. The receiver side of the channel can then inspect the
    /// results, and decide at what point to go ahead with the call to the
    /// underlying ledger, or, alternatively, return an error. This is done
    /// through a one-shot channel, the sender side of which is sent to the
    /// observer.
    pub fn new(underlying: Box<dyn ICRC1Ledger>, observer: LedgerObserver) -> Self {
        InterleavingTestLedger {
            underlying,
            observer,
        }
    }

    // Notifies the observer that a ledger method has been called, and blocks until
    // it receives a message to continue.
    async fn notify(&self, msg: LedgerMessage) -> Result<(), NervousSystemError> {
        let (tx, rx) = oneshot::channel::<Result<(), NervousSystemError>>();
        self.observer.unbounded_send((msg, tx)).unwrap();
        rx.await
            .map_err(|_e| NervousSystemError::new_with_message("Operation unavailable"))?
    }
}

#[async_trait]
impl ICRC1Ledger for InterleavingTestLedger {
    async fn transfer_funds(
        &self,
        amount_e8s: u64,
        fee_e8s: u64,
        from_subaccount: Option<Subaccount>,
        to: Account,
        memo: u64,
    ) -> Result<u64, NervousSystemError> {
        let msg = LedgerMessage::Transfer {
            amount_e8s,
            fee_e8s,
            from_subaccount,
            to: to.clone(),
            memo,
        };
        atomic::fence(AtomicOrdering::SeqCst);
        self.notify(msg).await?;
        self.underlying
            .transfer_funds(amount_e8s, fee_e8s, from_subaccount, to, memo)
            .await
    }

    async fn total_supply(&self) -> Result<Tokens, NervousSystemError> {
        atomic::fence(AtomicOrdering::SeqCst);
        self.notify(LedgerMessage::TotalSupply).await?;
        self.underlying.total_supply().await
    }

    async fn account_balance(&self, account: Account) -> Result<Tokens, NervousSystemError> {
        atomic::fence(AtomicOrdering::SeqCst);
        self.notify(LedgerMessage::BalanceQuery(account.clone()))
            .await?;
        self.underlying.account_balance(account).await
    }

    fn canister_id(&self) -> CanisterId {
        CanisterId::from_u64(1)
    }
}
