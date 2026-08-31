//! Client for a running CUSF BIP300/301 enforcer's `ValidatorService`, generated from the
//! enforcer's own `.proto` definitions (see `build.rs` and `crate::proto`) — the same approach
//! `thunder-rust` uses.

use std::fmt;

use alloy_primitives::{Address, B256};
use tonic::transport::{Channel, Endpoint};

use crate::proto::cusf::{
    common::v1::ReverseHex,
    mainchain::v1::{
        GetBmmHStarCommitmentRequest, GetChainTipRequest, GetTwoWayPegDataRequest, block_info,
        get_bmm_h_star_commitment_response, validator_service_client::ValidatorServiceClient,
    },
};

#[derive(Clone)]
pub struct EnforcerClient {
    base_url: String,
    channel: Channel,
    handle: tokio::runtime::Handle,
}

impl fmt::Debug for EnforcerClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnforcerClient")
            .field("base_url", &self.base_url)
            .finish()
    }
}

impl EnforcerClient {
    /// Must be called from within a Tokio runtime.
    pub fn new(base_url: String) -> Self {
        let channel = Endpoint::from_shared(base_url.clone())
            .expect("invalid enforcer URL")
            .connect_lazy();
        Self {
            base_url,
            channel,
            handle: tokio::runtime::Handle::current(),
        }
    }

    fn client(&self) -> ValidatorServiceClient<Channel> {
        ValidatorServiceClient::new(self.channel.clone())
    }

    /// Returns the current mainchain tip's block hash.
    pub fn chain_tip(&self) -> Result<B256, EnforcerError> {
        self.handle.block_on(async {
            let response = self
                .client()
                .get_chain_tip(GetChainTipRequest {})
                .await?
                .into_inner();
            let header_info = response
                .block_header_info
                .ok_or(EnforcerError::MissingField("block_header_info"))?;
            let block_hash = header_info
                .block_hash
                .ok_or(EnforcerError::MissingField("block_hash"))?;
            let hex = block_hash.hex.ok_or(EnforcerError::MissingField("hex"))?;
            parse_hex32(&hex)
        })
    }

    /// Returns whether `sidechain_block_hash` is the BIP301 BMM h* commitment recorded in
    /// `main_block_hash`, for sidechain `sidechain_id`.
    ///
    /// Note: this only checks the specific mainchain block referenced by the header being
    /// validated. It does not walk mainchain ancestry to tolerate the commitment landing in a
    /// later block (as e.g. `thunder-rust`'s `archive.rs` does for reorg/latency tolerance).
    pub fn is_committed(
        &self,
        sidechain_block_hash: B256,
        main_block_hash: B256,
        sidechain_id: u32,
    ) -> Result<bool, EnforcerError> {
        self.handle.block_on(async {
            let request = GetBmmHStarCommitmentRequest {
                block_hash: Some(ReverseHex {
                    hex: Some(hex::encode(main_block_hash)),
                }),
                sidechain_id: Some(sidechain_id),
                max_ancestors: None,
            };
            let response = self
                .client()
                .get_bmm_h_star_commitment(request)
                .await?
                .into_inner();

            let Some(get_bmm_h_star_commitment_response::Result::Commitment(commitment)) =
                response.result
            else {
                // Covers both "block not found" and "no commitment for this sidechain yet".
                return Ok(false);
            };
            let Some(commitment) = commitment.commitment else {
                return Ok(false);
            };
            let Some(hex) = commitment.hex else {
                return Ok(false);
            };

            Ok(parse_hex32(&hex)? == sidechain_block_hash)
        })
    }

    /// Returns all BIP300 deposits for `sidechain_id` recorded in mainchain blocks after
    /// `start_block_hash` (exclusive) up to and including `end_block_hash`.
    /// `start_block_hash = None` returns deposits from the start of BIP300 history.
    ///
    /// Assumes each deposit's destination `address` is the raw 20 bytes of an L2 `Address`, as
    /// chosen by the depositor — the enforcer treats it as opaque sidechain-defined data.
    pub fn deposits(
        &self,
        sidechain_id: u32,
        start_block_hash: Option<B256>,
        end_block_hash: B256,
    ) -> Result<Vec<Deposit>, EnforcerError> {
        self.handle.block_on(async {
            let request = GetTwoWayPegDataRequest {
                sidechain_id: Some(sidechain_id),
                start_block_hash: start_block_hash.map(|hash| ReverseHex {
                    hex: Some(hex::encode(hash)),
                }),
                end_block_hash: Some(ReverseHex {
                    hex: Some(hex::encode(end_block_hash)),
                }),
            };
            let response = self
                .client()
                .get_two_way_peg_data(request)
                .await?
                .into_inner();

            let mut deposits = Vec::new();
            for block in response.blocks {
                let Some(block_info) = block.block_info else {
                    continue;
                };
                for event in block_info.events {
                    let Some(block_info::event::Event::Deposit(deposit)) = event.event else {
                        // Not a deposit — e.g. a `WithdrawalBundleEvent`, which is a genuine
                        // BIP300 withdrawal (this sidechain -> Bitcoin mainchain, the opposite
                        // direction from what this function collects). Not handled here.
                        continue;
                    };
                    let output = deposit
                        .output
                        .ok_or(EnforcerError::MissingField("output"))?;
                    let value_sats = output
                        .value_sats
                        .ok_or(EnforcerError::MissingField("value_sats"))?;
                    let address_hex = output
                        .address
                        .and_then(|address| address.hex)
                        .ok_or(EnforcerError::MissingField("address"))?;
                    deposits.push(Deposit {
                        address: parse_address(&address_hex)?,
                        value_sats,
                    });
                }
            }
            Ok(deposits)
        })
    }
}

/// A BIP300 two-way-peg deposit onto this sidechain.
pub struct Deposit {
    pub address: Address,
    pub value_sats: u64,
}

fn strip_0x(hex_str: &str) -> &str {
    hex_str
        .strip_prefix("0x")
        .or_else(|| hex_str.strip_prefix("0X"))
        .unwrap_or(hex_str)
}

fn parse_hex32(hex_str: &str) -> Result<B256, EnforcerError> {
    let bytes = hex::decode(strip_0x(hex_str))
        .map_err(|_| EnforcerError::InvalidHex(hex_str.to_owned()))?;
    B256::try_from(bytes.as_slice()).map_err(|_| EnforcerError::InvalidHex(hex_str.to_owned()))
}

fn parse_address(hex_str: &str) -> Result<Address, EnforcerError> {
    let bytes = hex::decode(strip_0x(hex_str))
        .map_err(|_| EnforcerError::InvalidHex(hex_str.to_owned()))?;
    Address::try_from(bytes.as_slice()).map_err(|_| EnforcerError::InvalidHex(hex_str.to_owned()))
}

#[derive(Debug, thiserror::Error)]
pub enum EnforcerError {
    #[error("enforcer RPC failed: {0}")]
    Grpc(#[from] tonic::Status),
    #[error("enforcer response missing field `{0}`")]
    MissingField(&'static str),
    #[error("enforcer returned invalid hex: {0}")]
    InvalidHex(String),
}
