// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @notice Queues BIP300 withdrawal requests — this sidechain (`beth`) to Bitcoin mainchain —
/// for the block-production layer to batch into withdrawal bundles (M6 transactions) and submit
/// to the BIP300/301 enforcer. See `beth`'s `src/payload.rs`/`src/enforcer.rs` for the deposit
/// side (mainchain -> sidechain), which this contract is the reverse-direction counterpart to,
/// and `src/withdrawal_bundle.rs` for the bundle-selection/M6-construction logic that reads this
/// queue.
///
/// Deployed at a fixed predeploy address from genesis (see `genesis.json`) — this file is the
/// human-readable source; deployment ships the compiled runtime bytecode directly via
/// `alloc[address].code`, so there is no constructor and no deployment transaction.
///
/// This contract only accepts and records requests. It does not yet mark requests as bundled,
/// confirmed, or failed — that requires the block-production layer to write back bundle
/// outcomes (learned from the enforcer's `WithdrawalBundleEvent`s) once that side is built. Until
/// then, every request in the queue is a candidate on every block (see `src/withdrawal_bundle.rs`
/// module doc comment).
contract WithdrawalRequestQueue {
    /// wei per satoshi, matching the deposit side's `10^10` scaling (see `payload.rs`), so the L2
    /// asset preserves BTC's 8 decimals of precision.
    uint256 private constant WEI_PER_SAT = 1e10;

    /// A single pending withdrawal request. `btcDestination` is opaque to this contract — it is
    /// interpreted by the block-production layer as a raw Bitcoin scriptPubKey when constructing
    /// the M6 transaction.
    struct Request {
        address requester;
        /// Amount paid out to `btcDestination` on Bitcoin mainchain, in satoshis.
        uint256 valueSats;
        /// This request's contribution to the Bitcoin miner fee for the bundle it ends up in,
        /// in satoshis. Mirrors `thunder_types::AggregatedWithdrawal`'s `main_fee` — see
        /// `thunder-rust`'s `lib/wallet.rs::create_withdrawal` for the reference semantics:
        /// `msg.value` locks `valueSats + mainFeeSats` together (converted to wei), same as
        /// thunder's wallet selecting coins covering `value + main_fee`.
        uint256 mainFeeSats;
        bytes btcDestination;
    }

    /// Total number of requests ever queued; also the next index to be assigned.
    ///
    /// Storage layout (for the off-chain reader, which reads raw slots rather than calling
    /// this contract): slot 0.
    uint256 public requestCount;

    /// `requests[i]` for `i` in `[0, requestCount)`.
    ///
    /// Storage layout: mapping at slot 1. Entry `i`'s fields start at
    /// `keccak256(abi.encode(i, uint256(1)))`: `requester` (slot 0 of the entry, address
    /// right-aligned — alone in its slot since the following field is a full `uint256`),
    /// `valueSats` (slot 1), `mainFeeSats` (slot 2), `btcDestination` (slot 3, Solidity's
    /// standard dynamic `bytes` encoding — inline with a `length * 2` marker if `length <= 31`,
    /// else a `length * 2 + 1` marker with data starting at `keccak256(entry_slot + 3)`).
    mapping(uint256 => Request) public requests;

    event WithdrawalRequested(
        uint256 indexed index,
        address indexed requester,
        uint256 valueSats,
        uint256 mainFeeSats,
        bytes btcDestination
    );

    /// Queues a withdrawal request. `msg.value` must equal `(valueSats + mainFeeSats) *
    /// 10^10` wei, locked in this contract until bundled and confirmed — or released back to
    /// `requester` on bundle failure — by the block-production layer.
    function requestWithdrawal(bytes calldata btcDestination, uint256 mainFeeSats)
        external
        payable
        returns (uint256 index)
    {
        require(msg.value > 0, "WithdrawalRequestQueue: zero amount");
        require(msg.value % WEI_PER_SAT == 0, "WithdrawalRequestQueue: not a whole number of satoshis");
        uint256 totalSats = msg.value / WEI_PER_SAT;
        require(mainFeeSats < totalSats, "WithdrawalRequestQueue: fee exceeds withdrawal value");
        require(totalSats <= type(uint64).max, "WithdrawalRequestQueue: amount too large");
        uint256 valueSats = totalSats - mainFeeSats;

        index = requestCount++;
        requests[index] = Request(msg.sender, valueSats, mainFeeSats, btcDestination);
        emit WithdrawalRequested(index, msg.sender, valueSats, mainFeeSats, btcDestination);
    }
}
