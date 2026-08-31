//! Reads pending BIP300 withdrawal requests (this sidechain -> Bitcoin mainchain) from
//! `WithdrawalRequestQueue` (see `contracts/WithdrawalRequestQueue.sol`), deterministically
//! selects a subset into a withdrawal bundle, and constructs the corresponding BIP300 "M6"
//! Bitcoin transaction — following the same approach as `thunder-rust`:
//! `types/lib.rs::{WithdrawalBundle, AggregatedWithdrawal}` for the M6 transaction shape and
//! weight-bounded selection, and `state/two_way_peg_data.rs::collect_withdrawal_bundle` for the
//! aggregate-by-destination-then-greedily-select algorithm.
//!
//! Differences from thunder's model, driven by `beth` using an account-based (not UTXO-based)
//! chain: thunder aggregates *withdrawal-marked UTXOs*; this aggregates *queue entries* (by
//! index) from `WithdrawalRequestQueue`. Where thunder's M6 commits to the set of consumed
//! `OutPoint`s, this commits to the set of consumed request indices instead (see
//! [`requests_commitment`]). `btcDestination` here is a raw scriptPubKey rather than a parsed
//! `bitcoin::Address`, avoiding an address-format/network round-trip.
//!
//! [`InFlightBundles`] tracks broadcast-but-unresolved bundles and permanently-finalized
//! request indices (paid out, or refunded after a failed bundle), so [`Bip300301PayloadBuilder`]
//! doesn't keep re-selecting requests that are already spoken for. This is **not** a
//! `WithdrawalRequestQueue` write-back — the contract still has no "bundled" status field, so
//! [`read_pending_withdrawals`] always returns every request ever queued, unfiltered.
//! [`InFlightBundles`] lives only in this process's memory: it does not survive a restart, is
//! not shared across multiple block-producing nodes, and is not verified by
//! `Bip300301Consensus` at all. A restart forgets every in-flight/finalized index, so every
//! previously-succeeded-or-refunded request becomes selectable again — risking a duplicate
//! Bitcoin payout (for a previously-succeeded bundle) or a duplicate refund (for a
//! previously-failed one). Fixing this properly needs the bundle/request status to live
//! on-chain in `WithdrawalRequestQueue`, written back by a trusted (signed or system-call)
//! transaction — out of scope here; this in-memory version is the pragmatic middle step.
//!
//! [`Bip300301PayloadBuilder`]: crate::payload::Bip300301PayloadBuilder

use std::collections::{HashMap, HashSet};

use alloy_primitives::{Address, B256, U256, address, keccak256};
use bitcoin::script::PushBytesBuf;
use reth_storage_api::{StateProvider, errors::provider::ProviderResult};

use crate::enforcer::{WithdrawalBundleOutcome, WithdrawalBundleStatus};

/// The `WithdrawalRequestQueue` predeploy address (see `genesis.json`).
pub const WITHDRAWAL_REQUEST_QUEUE_ADDRESS: Address =
    address!("0x000000000000000000000000000000000000B300");

/// Storage slot of `WithdrawalRequestQueue.requestCount`.
const REQUEST_COUNT_SLOT: B256 = B256::ZERO;

/// Storage slot of the `WithdrawalRequestQueue.requests` mapping itself (not an entry's slot).
const REQUESTS_MAPPING_SLOT: u64 = 1;

/// A single pending withdrawal request read from `WithdrawalRequestQueue`.
#[derive(Debug, Clone)]
pub struct PendingWithdrawal {
    pub index: u64,
    pub requester: Address,
    pub value_sats: u64,
    pub main_fee_sats: u64,
    /// A raw Bitcoin scriptPubKey (see the contract's doc comment).
    pub btc_destination: Vec<u8>,
}

/// Reads every request in `WithdrawalRequestQueue` (`requests[0..requestCount]`) from `state`.
///
/// See the module doc comment: this does not know which requests have already been bundled, so
/// it always returns the full queue.
pub fn read_pending_withdrawals(
    state: &dyn StateProvider,
) -> ProviderResult<Vec<PendingWithdrawal>> {
    let request_count = read_u64(state, REQUEST_COUNT_SLOT)?;

    let mut withdrawals = Vec::with_capacity(request_count as usize);
    for index in 0..request_count {
        let base = requests_entry_slot(index);
        let requester = read_address(state, base)?;
        let value_sats = read_u64(state, add_slot(base, 1))?;
        let main_fee_sats = read_u64(state, add_slot(base, 2))?;
        let btc_destination = read_dynamic_bytes(state, add_slot(base, 3))?;
        withdrawals.push(PendingWithdrawal {
            index,
            requester,
            value_sats,
            main_fee_sats,
            btc_destination,
        });
    }
    Ok(withdrawals)
}

/// The base storage slot of `requests[index]`, per Solidity's `mapping(uint256 => T)` layout:
/// `keccak256(abi.encode(index, REQUESTS_MAPPING_SLOT))`.
fn requests_entry_slot(index: u64) -> B256 {
    let mut preimage = [0u8; 64];
    preimage[24..32].copy_from_slice(&index.to_be_bytes());
    preimage[56..64].copy_from_slice(&REQUESTS_MAPPING_SLOT.to_be_bytes());
    keccak256(preimage)
}

fn add_slot(slot: B256, offset: u64) -> B256 {
    B256::from(U256::from_be_bytes(slot.0) + U256::from(offset))
}

fn read_address(state: &dyn StateProvider, slot: B256) -> ProviderResult<Address> {
    let value = state
        .storage(WITHDRAWAL_REQUEST_QUEUE_ADDRESS, slot)?
        .unwrap_or_default();
    Ok(Address::from_slice(&value.to_be_bytes::<32>()[12..32]))
}

fn read_u64(state: &dyn StateProvider, slot: B256) -> ProviderResult<u64> {
    let value = state
        .storage(WITHDRAWAL_REQUEST_QUEUE_ADDRESS, slot)?
        .unwrap_or_default();
    Ok(u64::try_from(value).unwrap_or(u64::MAX))
}

/// Reads a Solidity dynamic `bytes` value stored at `slot`, per Solidity's standard encoding
/// (see [`decode_bytes_head`]).
fn read_dynamic_bytes(state: &dyn StateProvider, slot: B256) -> ProviderResult<Vec<u8>> {
    let word = state
        .storage(WITHDRAWAL_REQUEST_QUEUE_ADDRESS, slot)?
        .unwrap_or_default();
    let length = match decode_bytes_head(word) {
        Ok(short) => return Ok(short),
        Err(length) => length,
    };
    let mut data = Vec::with_capacity(length);
    let mut chunk_slot = keccak256(slot);
    while data.len() < length {
        let chunk = state
            .storage(WITHDRAWAL_REQUEST_QUEUE_ADDRESS, chunk_slot)?
            .unwrap_or_default();
        data.extend_from_slice(&chunk.to_be_bytes::<32>());
        chunk_slot = add_slot(chunk_slot, 1);
    }
    data.truncate(length);
    Ok(data)
}

/// Decodes the head word of a Solidity dynamic `bytes` value: inline in `word` (left-aligned,
/// with `length * 2` in the low-order byte) if `length <= 31` (returned directly as `Ok`), else
/// `word` holds `length * 2 + 1` and the data starts at `keccak256(slot)` (the length is
/// returned as `Err`, for the caller to then read those subsequent slots).
fn decode_bytes_head(word: U256) -> Result<Vec<u8>, usize> {
    let word_bytes = word.to_be_bytes::<32>();
    if word_bytes[31] & 1 == 0 {
        let length = (word_bytes[31] / 2) as usize;
        Ok(word_bytes[..length].to_vec())
    } else {
        Err(usize::try_from(word >> 1).unwrap_or(usize::MAX))
    }
}

/// A withdrawal bundle: the BIP300 "M6" transaction, plus the request indices it consumes.
///
/// The transaction has no inputs — it is "blinded", matching `thunder_types::WithdrawalBundle`.
/// The enforcer attaches the actual mainchain CTIP-spending input when including it in a block
/// (see `WalletService.BroadcastWithdrawalBundle`).
#[derive(Debug, Clone)]
pub struct WithdrawalBundle {
    pub request_indices: Vec<u64>,
    pub tx: bitcoin::Transaction,
}

impl WithdrawalBundle {
    /// The bundle's M6 identifier — its (blinded) transaction id.
    pub fn m6id(&self) -> bitcoin::Txid {
        self.tx.compute_txid()
    }
}

/// A withdrawal destination aggregated from one or more pending requests, mirroring
/// `thunder_types::AggregatedWithdrawal`.
#[derive(Debug, Clone)]
struct AggregatedWithdrawal {
    btc_destination: Vec<u8>,
    value_sats: u64,
    main_fee_sats: u64,
    request_indices: Vec<u64>,
}

/// Aggregates `pending` by destination, then greedily selects destinations (highest fee first)
/// into a bundle, stopping once the M6 transaction would exceed Bitcoin's standard weight limit.
/// Mirrors `state/two_way_peg_data.rs::collect_withdrawal_bundle`.
///
/// Returns `None` if there are no pending withdrawals, or none fit in a single bundle.
pub fn select_withdrawal_bundle(
    pending: &[PendingWithdrawal],
    block_height: u32,
) -> Option<WithdrawalBundle> {
    if pending.is_empty() {
        return None;
    }

    // Aggregate all requests by destination.
    let mut by_destination: HashMap<Vec<u8>, AggregatedWithdrawal> = HashMap::new();
    for request in pending {
        let aggregated = by_destination
            .entry(request.btc_destination.clone())
            .or_insert_with(|| AggregatedWithdrawal {
                btc_destination: request.btc_destination.clone(),
                value_sats: 0,
                main_fee_sats: 0,
                request_indices: Vec::new(),
            });
        aggregated.value_sats = aggregated.value_sats.saturating_add(request.value_sats);
        aggregated.main_fee_sats = aggregated
            .main_fee_sats
            .saturating_add(request.main_fee_sats);
        aggregated.request_indices.push(request.index);
    }

    // A *total* order (fee, then value, then destination bytes) so the selected/ordered bundle
    // is canonical regardless of hashmap iteration order — see the comment on
    // `thunder_types::AggregatedWithdrawal`'s `Ord` impl, which this mirrors.
    let mut aggregated: Vec<_> = by_destination.into_values().collect();
    aggregated.sort_by(|a, b| {
        (b.main_fee_sats, b.value_sats, &b.btc_destination).cmp(&(
            a.main_fee_sats,
            a.value_sats,
            &a.btc_destination,
        ))
    });

    let mut request_indices = Vec::new();
    let mut bundle_outputs = Vec::new();
    let mut bundle_txouts_size: u32 = 0;
    let mut total_main_fee_sats: u64 = 0;
    for destination in &aggregated {
        let Ok(spk_size) = u32::try_from(destination.btc_destination.len()) else {
            // This scriptPubKey is invalid, but others might be ok.
            continue;
        };
        let Some(this_txout_size) = txout_size(spk_size) else {
            continue;
        };
        let Ok(n_outputs) = u32::try_from(bundle_outputs.len() + 1) else {
            break;
        };
        let Some(sum_txout_sizes) = bundle_txouts_size.checked_add(this_txout_size) else {
            break;
        };
        if predict_weight(n_outputs, sum_txout_sizes).is_none() {
            break;
        }
        bundle_txouts_size = sum_txout_sizes;
        bundle_outputs.push(bitcoin::TxOut {
            value: bitcoin::Amount::from_sat(destination.value_sats),
            script_pubkey: bitcoin::ScriptBuf::from_bytes(destination.btc_destination.clone()),
        });
        request_indices.extend(destination.request_indices.iter().copied());
        total_main_fee_sats = total_main_fee_sats.saturating_add(destination.main_fee_sats);
    }
    if bundle_outputs.is_empty() {
        return None;
    }
    request_indices.sort_unstable();

    Some(build_withdrawal_bundle(
        block_height,
        total_main_fee_sats,
        request_indices,
        bundle_outputs,
    ))
}

/// Commits to the set of request indices a bundle consumes, plus the L2 block height it was
/// proposed at (so two bundles with identical outputs, proposed at different heights, don't
/// collide) — this sidechain's counterpart to thunder's `hash(spend_utxos ++ block_height)`
/// input commitment (see `WithdrawalBundle::new` in `thunder-rust`'s `types/lib.rs`), adapted
/// since `beth` has queue indices rather than UTXOs to commit to.
fn requests_commitment(request_indices: &[u64], block_height: u32) -> B256 {
    let mut preimage = Vec::with_capacity(8 * request_indices.len() + 4);
    for index in request_indices {
        preimage.extend_from_slice(&index.to_be_bytes());
    }
    preimage.extend_from_slice(&block_height.to_be_bytes());
    keccak256(preimage)
}

/// Builds the M6 transaction: `[mainchain-fee commitment, requests commitment, ...payouts]`,
/// with no inputs. Mirrors `thunder_types::WithdrawalBundle::new`.
fn build_withdrawal_bundle(
    block_height: u32,
    total_main_fee_sats: u64,
    request_indices: Vec<u64>,
    bundle_outputs: Vec<bitcoin::TxOut>,
) -> WithdrawalBundle {
    let mainchain_fee_txout = {
        let push = PushBytesBuf::try_from(total_main_fee_sats.to_be_bytes().to_vec())
            .expect("8-byte push always fits");
        let script_pubkey = bitcoin::script::Builder::new()
            .push_opcode(bitcoin::opcodes::all::OP_RETURN)
            .push_slice(push)
            .into_script();
        bitcoin::TxOut {
            value: bitcoin::Amount::ZERO,
            script_pubkey,
        }
    };
    let requests_commitment_txout = {
        let commitment = requests_commitment(&request_indices, block_height);
        let push = PushBytesBuf::try_from(commitment.to_vec()).expect("32-byte push always fits");
        let script_pubkey = bitcoin::script::Builder::new()
            .push_opcode(bitcoin::opcodes::all::OP_RETURN)
            .push_slice(push)
            .into_script();
        bitcoin::TxOut {
            value: bitcoin::Amount::ZERO,
            script_pubkey,
        }
    };
    let output = std::iter::once(mainchain_fee_txout)
        .chain(std::iter::once(requests_commitment_txout))
        .chain(bundle_outputs)
        .collect();
    let tx = bitcoin::Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: Vec::new(),
        output,
    };
    WithdrawalBundle {
        request_indices,
        tx,
    }
}

/// Size of a single txout with a `spk_size`-byte scriptPubKey.
/// Mirrors `thunder_types::WithdrawalBundle::txout_size`.
const fn txout_size(spk_size: u32) -> Option<u32> {
    let Some(size) =
        (bitcoin::Amount::SIZE as u32).checked_add(bitcoin::VarInt(spk_size as u64).size() as u32)
    else {
        return None;
    };
    size.checked_add(spk_size)
}

/// Predicts the weight of a withdrawal bundle with `n_outputs` payout outputs summing to
/// `sum_txout_sizes` bytes (not including the two commitment outputs), assuming a single
/// (to-be-attached) treasury input. Returns `None` if the predicted weight would exceed
/// Bitcoin's standard transaction weight limit. Mirrors
/// `thunder_types::WithdrawalBundle::predict_weight`.
const fn predict_weight(n_outputs: u32, sum_txout_sizes: u32) -> Option<bitcoin::Weight> {
    use bitcoin::{VarInt, Weight};
    const fn txin_base_size(script_sig_size: u32) -> Option<u32> {
        const OUTPOINT_SIZE: u8 = 36;
        const SEQUENCE_SIZE: u8 = 4;
        let script_sig_len_size = VarInt(script_sig_size as u64).size() as u8;
        let Some(res) = ((OUTPOINT_SIZE + script_sig_len_size) as u32).checked_add(script_sig_size)
        else {
            return None;
        };
        res.checked_add(SEQUENCE_SIZE as u32)
    }
    const fn tx_base_size(
        n_inputs: u32,
        sum_txin_base_sizes: u32,
        n_outputs: u32,
        sum_txout_sizes: u32,
    ) -> Option<u32> {
        const VERSION_SIZE: u8 = 4;
        const fn vin_base_size(n_inputs: u32, sum_txin_base_sizes: u32) -> Option<u32> {
            let len_size = VarInt(n_inputs as u64).size() as u8;
            (len_size as u32).checked_add(sum_txin_base_sizes)
        }
        const fn vout_size(n_outputs: u32, sum_txout_sizes: u32) -> Option<u32> {
            let len_size = VarInt(n_outputs as u64).size() as u8;
            (len_size as u32).checked_add(sum_txout_sizes)
        }
        const LOCKTIME_SIZE: u8 = bitcoin::absolute::LockTime::SIZE as u8;
        let res = VERSION_SIZE as u32;
        let Some(vin_base_size) = vin_base_size(n_inputs, sum_txin_base_sizes) else {
            return None;
        };
        let Some(res) = res.checked_add(vin_base_size) else {
            return None;
        };
        let Some(vout_size) = vout_size(n_outputs, sum_txout_sizes) else {
            return None;
        };
        let Some(res) = res.checked_add(vout_size) else {
            return None;
        };
        res.checked_add(LOCKTIME_SIZE as u32)
    }
    const N_INPUTS: u32 = 1;
    const SUM_TXIN_BASE_SIZES: u32 = {
        const TREASURY_SCRIPT_SIG_SIZE: u32 = 0;
        match txin_base_size(TREASURY_SCRIPT_SIG_SIZE) {
            Some(size) => size,
            None => unreachable!(),
        }
    };
    let Some(n_outputs) = n_outputs.checked_add(2) else {
        return None;
    };
    let Some(sum_txout_sizes) = ({
        const INPUTS_COMMITMENT_TXOUT_SIZE: u32 = {
            const SPK_SIZE: u8 = 34;
            match txout_size(SPK_SIZE as u32) {
                Some(size) => size,
                None => unreachable!(),
            }
        };
        const MAINCHAIN_FEE_COMMITMENT_TXOUT_SIZE: u32 = {
            const SPK_SIZE: u8 = 10;
            match txout_size(SPK_SIZE as u32) {
                Some(size) => size,
                None => unreachable!(),
            }
        };
        (INPUTS_COMMITMENT_TXOUT_SIZE + MAINCHAIN_FEE_COMMITMENT_TXOUT_SIZE)
            .checked_add(sum_txout_sizes)
    }) else {
        return None;
    };
    let Some(tx_base_size) =
        tx_base_size(N_INPUTS, SUM_TXIN_BASE_SIZES, n_outputs, sum_txout_sizes)
    else {
        return None;
    };
    let Some(tx_weight_wu) = (tx_base_size as u64).checked_mul(Weight::WITNESS_SCALE_FACTOR) else {
        return None;
    };
    if tx_weight_wu <= bitcoin::Transaction::MAX_STANDARD_WEIGHT.to_wu() {
        Some(Weight::from_wu(tx_weight_wu))
    } else {
        None
    }
}

/// Tracks BIP300 withdrawal bundles broadcast but not yet resolved, and every request index
/// that must never be selected again. See the module doc comment for the (significant)
/// restart-safety caveat.
#[derive(Debug, Default)]
pub struct InFlightBundles {
    /// m6id -> the request indices it consumed, for bundles broadcast but not yet resolved.
    pending: HashMap<bitcoin::Txid, Vec<u64>>,
    /// Every request index that must never be selected again: already paid out on Bitcoin, or
    /// already refunded after its bundle failed.
    finalized: HashSet<u64>,
}

impl InFlightBundles {
    /// Whether `index` may be selected into a new bundle: not already finalized, and not
    /// consumed by a bundle that's still awaiting an outcome.
    pub fn is_eligible(&self, index: u64) -> bool {
        !self.finalized.contains(&index)
            && !self
                .pending
                .values()
                .any(|indices| indices.contains(&index))
    }

    /// Records a newly broadcast bundle's consumed request indices.
    pub fn record_submitted(&mut self, m6id: bitcoin::Txid, indices: Vec<u64>) {
        self.pending.insert(m6id, indices);
    }

    /// Resolves `outcomes` against currently-tracked bundles: on `Succeeded`, the bundle's
    /// requests are finalized (paid out, excluded forever). On `Failed`, they're also
    /// finalized, and returned here so the caller can refund them — the caller must actually
    /// credit those refunds, since this method only updates in-memory tracking. `Submitted` and
    /// outcomes for untracked m6ids (e.g. another producer's bundle, or ours from before a
    /// restart) are ignored.
    pub fn resolve(&mut self, outcomes: &[WithdrawalBundleOutcome]) -> Vec<u64> {
        let mut to_refund = Vec::new();
        for outcome in outcomes {
            match outcome.status {
                WithdrawalBundleStatus::Succeeded => {
                    if let Some(indices) = self.pending.remove(&outcome.m6id) {
                        self.finalized.extend(indices);
                    }
                }
                WithdrawalBundleStatus::Failed => {
                    if let Some(indices) = self.pending.remove(&outcome.m6id) {
                        self.finalized.extend(indices.iter().copied());
                        to_refund.extend(indices);
                    }
                }
                WithdrawalBundleStatus::Submitted => {}
            }
        }
        to_refund
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        index: u64,
        main_fee_sats: u64,
        value_sats: u64,
        destination: &[u8],
    ) -> PendingWithdrawal {
        PendingWithdrawal {
            index,
            requester: Address::ZERO,
            value_sats,
            main_fee_sats,
            btc_destination: destination.to_vec(),
        }
    }

    #[test]
    fn requests_entry_slot_places_index_and_mapping_slot_correctly() {
        // keccak256(uint256(0) ++ uint256(1)), per Solidity's `mapping(uint256 => T)` layout.
        let mut preimage = [0u8; 64];
        preimage[63] = 1;
        assert_eq!(requests_entry_slot(0), keccak256(preimage));

        // keccak256(uint256(300) ++ uint256(1))
        let mut preimage = [0u8; 64];
        preimage[30..32].copy_from_slice(&300u16.to_be_bytes());
        preimage[63] = 1;
        assert_eq!(requests_entry_slot(300), keccak256(preimage));

        // Distinct indices must not collide.
        assert_ne!(requests_entry_slot(0), requests_entry_slot(1));
    }

    #[test]
    fn add_slot_is_plain_slot_arithmetic() {
        assert_eq!(add_slot(B256::ZERO, 0), B256::ZERO);
        assert_eq!(add_slot(B256::ZERO, 1), B256::from(U256::from(1u64)));
        let base = keccak256([1u8]);
        assert_eq!(
            add_slot(base, 2),
            B256::from(U256::from_be_bytes(base.0) + U256::from(2u64))
        );
    }

    #[test]
    fn decode_bytes_head_short_form_round_trips() {
        for data in [b"".as_slice(), b"hello".as_slice(), &[0xABu8; 31]] {
            let mut word_bytes = [0u8; 32];
            word_bytes[..data.len()].copy_from_slice(data);
            word_bytes[31] = (data.len() as u8) * 2;
            let word = U256::from_be_bytes(word_bytes);
            assert_eq!(decode_bytes_head(word), Ok(data.to_vec()));
        }
    }

    #[test]
    fn decode_bytes_head_long_form_reports_length() {
        for length in [32usize, 100, 1000] {
            let word = U256::from(length as u64) * U256::from(2u64) + U256::from(1u64);
            assert_eq!(decode_bytes_head(word), Err(length));
        }
    }

    #[test]
    fn decode_bytes_head_length_31_is_short_form_boundary() {
        // 31 is the largest length still encoded inline (length*2 = 62, even => short form).
        let mut word_bytes = [0xCDu8; 32];
        word_bytes[31] = 62;
        let word = U256::from_be_bytes(word_bytes);
        assert_eq!(decode_bytes_head(word), Ok(vec![0xCD; 31]));
    }

    #[test]
    fn select_withdrawal_bundle_returns_none_when_empty() {
        assert!(select_withdrawal_bundle(&[], 1).is_none());
    }

    #[test]
    fn select_withdrawal_bundle_aggregates_by_destination() {
        let dest_a = vec![0xAA; 22];
        let dest_b = vec![0xBB; 22];
        let pending = vec![
            request(0, 100, 1_000, &dest_a),
            request(1, 50, 2_000, &dest_a),
            request(2, 10, 500, &dest_b),
        ];
        let bundle = select_withdrawal_bundle(&pending, 1).expect("some requests fit");

        // 2 commitment outputs + 2 aggregated destinations.
        assert_eq!(bundle.tx.output.len(), 4);
        assert_eq!(bundle.request_indices, vec![0, 1, 2]);

        // Higher aggregated fee (dest_a: 100+50=150) sorts before dest_b (10), and comes right
        // after the two commitment outputs.
        let dest_a_txout = &bundle.tx.output[2];
        assert_eq!(dest_a_txout.value, bitcoin::Amount::from_sat(3_000));
        assert_eq!(dest_a_txout.script_pubkey.as_bytes(), dest_a.as_slice());

        let dest_b_txout = &bundle.tx.output[3];
        assert_eq!(dest_b_txout.value, bitcoin::Amount::from_sat(500));
        assert_eq!(dest_b_txout.script_pubkey.as_bytes(), dest_b.as_slice());

        // No inputs — the bundle is blinded until the enforcer attaches the CTIP input.
        assert!(bundle.tx.input.is_empty());
    }

    #[test]
    fn select_withdrawal_bundle_m6id_is_deterministic_and_sensitive_to_height() {
        let pending = vec![request(0, 1, 100, &[0xAA; 22])];
        let a = select_withdrawal_bundle(&pending, 1).unwrap();
        let b = select_withdrawal_bundle(&pending, 1).unwrap();
        assert_eq!(a.m6id(), b.m6id(), "same inputs, same height => same m6id");

        let c = select_withdrawal_bundle(&pending, 2).unwrap();
        assert_ne!(a.m6id(), c.m6id(), "different height => different m6id");
    }

    #[test]
    fn select_withdrawal_bundle_stops_before_exceeding_standard_weight() {
        // Many distinct 34-byte (P2WSH-sized) destinations — far more than fit in one
        // standard-weight bundle.
        let pending: Vec<_> = (0..15_000u64)
            .map(|i| {
                let mut destination = vec![0u8; 34];
                destination[..8].copy_from_slice(&i.to_be_bytes());
                request(i, 1, 1_000, &destination)
            })
            .collect();
        let bundle = select_withdrawal_bundle(&pending, 1).expect("at least some requests fit");

        let n_payout_outputs = bundle.tx.output.len() - 2;
        assert!(
            n_payout_outputs > 0,
            "some requests should have been included"
        );
        assert!(
            n_payout_outputs < pending.len(),
            "not all 15,000 requests should fit"
        );
        assert_eq!(bundle.request_indices.len(), n_payout_outputs);
        assert!(
            predict_weight(n_payout_outputs as u32, 0).is_some() || n_payout_outputs == 0,
            "sanity: selected count should itself be weight-feasible",
        );
    }

    #[test]
    fn txout_size_matches_amount_plus_varint_plus_script() {
        // 8-byte amount + 1-byte varint length prefix (for spk_size < 0xFD) + script bytes.
        assert_eq!(txout_size(0), Some(9));
        assert_eq!(txout_size(34), Some(43));
        assert_eq!(txout_size(252), Some(261));
        // 0xFD..=0xFFFF uses a 3-byte varint prefix.
        assert_eq!(txout_size(253), Some(264));
    }

    #[test]
    fn predict_weight_rejects_past_standard_weight_limit() {
        assert!(predict_weight(1, 43).is_some());
        // A huge number of outputs must eventually exceed MAX_STANDARD_WEIGHT.
        assert!(predict_weight(u32::MAX / 100, u32::MAX / 2).is_none());
    }

    fn txid(byte: u8) -> bitcoin::Txid {
        use bitcoin::hashes::Hash as _;
        bitcoin::Txid::from_byte_array([byte; 32])
    }

    #[test]
    fn in_flight_bundles_starts_with_everything_eligible() {
        let in_flight = InFlightBundles::default();
        assert!(in_flight.is_eligible(0));
        assert!(in_flight.is_eligible(42));
    }

    #[test]
    fn in_flight_bundles_excludes_submitted_indices() {
        let mut in_flight = InFlightBundles::default();
        in_flight.record_submitted(txid(1), vec![0, 1, 2]);
        assert!(!in_flight.is_eligible(0));
        assert!(!in_flight.is_eligible(1));
        assert!(!in_flight.is_eligible(2));
        assert!(
            in_flight.is_eligible(3),
            "index in a different bundle stays eligible"
        );
    }

    #[test]
    fn in_flight_bundles_succeeded_finalizes_without_refund() {
        let mut in_flight = InFlightBundles::default();
        in_flight.record_submitted(txid(1), vec![0, 1]);

        let refunds = in_flight.resolve(&[WithdrawalBundleOutcome {
            m6id: txid(1),
            status: WithdrawalBundleStatus::Succeeded,
        }]);

        assert!(refunds.is_empty());
        // Permanently excluded, not just "no longer pending".
        assert!(!in_flight.is_eligible(0));
        assert!(!in_flight.is_eligible(1));
    }

    #[test]
    fn in_flight_bundles_failed_finalizes_and_refunds() {
        let mut in_flight = InFlightBundles::default();
        in_flight.record_submitted(txid(1), vec![0, 1]);

        let mut refunds = in_flight.resolve(&[WithdrawalBundleOutcome {
            m6id: txid(1),
            status: WithdrawalBundleStatus::Failed,
        }]);
        refunds.sort_unstable();

        assert_eq!(refunds, vec![0, 1]);
        // Refunded requests must never be selected again either — otherwise a later bundle
        // could pay them out on Bitcoin *and* they'd have already been refunded on L2.
        assert!(!in_flight.is_eligible(0));
        assert!(!in_flight.is_eligible(1));
    }

    #[test]
    fn in_flight_bundles_submitted_status_is_a_no_op() {
        let mut in_flight = InFlightBundles::default();
        in_flight.record_submitted(txid(1), vec![0]);

        let refunds = in_flight.resolve(&[WithdrawalBundleOutcome {
            m6id: txid(1),
            status: WithdrawalBundleStatus::Submitted,
        }]);

        assert!(refunds.is_empty());
        assert!(!in_flight.is_eligible(0), "still pending, not yet resolved");
    }

    #[test]
    fn in_flight_bundles_ignores_outcomes_for_untracked_bundles() {
        let mut in_flight = InFlightBundles::default();
        // No `record_submitted` call — e.g. another producer's bundle, or ours from before a
        // restart (see the module doc comment).
        let refunds = in_flight.resolve(&[WithdrawalBundleOutcome {
            m6id: txid(9),
            status: WithdrawalBundleStatus::Failed,
        }]);
        assert!(refunds.is_empty());
    }
}
