//! Types used to support the candid API.

use candid::{CandidType, Deserialize};

pub type Satoshi = u64;

/// A reference to a transaction output.
#[derive(CandidType, Clone, Debug, Deserialize, PartialEq, Eq, Hash)]
pub struct OutPoint {
    #[serde(with = "serde_bytes")]
    pub txid: Vec<u8>,
    pub vout: u32,
}

/// An unspent transaction output.
#[derive(CandidType, Debug, Deserialize, PartialEq)]
pub struct Utxo {
    pub outpoint: OutPoint,
    pub value: Satoshi,
    pub height: u32,
    pub confirmations: u32,
}

/// A request for getting the UTXOs for a given address.
#[derive(CandidType, Debug, Deserialize, PartialEq)]
pub struct GetUtxosRequest {
    pub address: String,
    pub min_confirmations: Option<u32>,
}

/// Errors when processing a `get_utxos` request.
#[derive(CandidType, Debug, Deserialize, PartialEq)]
pub struct GetUtxosResponse {
    pub utxos: Vec<Utxo>,
    pub total_count: u32,
}

/// Errors when processing a `get_utxos` request.
#[derive(CandidType, Debug, Deserialize, PartialEq)]
pub enum GetUtxosError {
    MalformedAddress,
    MinConfirmationsTooLarge { given: u32, max: u32 },
}

#[derive(CandidType, Debug, Deserialize, PartialEq)]
pub struct GetBalanceRequest {
    pub address: String,
    pub min_confirmations: Option<u32>,
}

#[derive(CandidType, Debug, Deserialize, PartialEq)]
pub enum GetBalanceError {
    MalformedAddress,
    MinConfirmationsTooLarge { given: u32, max: u32 },
}

impl From<GetUtxosError> for GetBalanceError {
    fn from(err: GetUtxosError) -> Self {
        match err {
            GetUtxosError::MalformedAddress => Self::MalformedAddress,
            GetUtxosError::MinConfirmationsTooLarge { given, max } => {
                Self::MinConfirmationsTooLarge { given, max }
            }
        }
    }
}

#[derive(CandidType, Debug, Deserialize, PartialEq)]
pub struct SendTransactionRequest {
    #[serde(with = "serde_bytes")]
    pub transaction: Vec<u8>,
}

#[derive(CandidType, Debug, Deserialize, PartialEq)]
pub enum SendTransactionError {
    MalformedTransaction,
}
