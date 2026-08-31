// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @notice Queues BIP300 withdrawal requests — this sidechain (`beth`) to Bitcoin mainchain —
/// for the block-production layer to batch into withdrawal bundles (M6 transactions) and submit
/// to the BIP300/301 enforcer. See `beth`'s `src/payload.rs`/`src/enforcer.rs` for the deposit
/// side (mainchain -> sidechain), which this contract is the reverse-direction counterpart to.
///
/// Deployed at a fixed predeploy address from genesis (see `genesis.json`) — this file is the
/// human-readable source; deployment ships the compiled runtime bytecode directly via
/// `alloc[address].code`, so there is no constructor and no deployment transaction.
///
/// This contract only accepts and records requests. It does not yet mark requests as bundled,
/// confirmed, or failed — that requires the block-production layer to write back bundle
/// outcomes (learned from the enforcer's `WithdrawalBundleEvent`s) once that side is built.
contract WithdrawalRequestQueue {
    /// A single pending withdrawal request. `btcDestination` is opaque to this contract — it is
    /// interpreted by the block-production layer when constructing the M6 transaction (e.g. a
    /// scriptPubKey).
    struct Request {
        address requester;
        uint256 amountWei;
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
    /// right-aligned), `amountWei` (slot 1), `btcDestination` (slot 2, Solidity's standard
    /// dynamic `bytes` encoding — inline with a `length * 2` marker if `length <= 31`, else a
    /// `length * 2 + 1` marker with data starting at `keccak256(entry_slot + 2)`).
    mapping(uint256 => Request) public requests;

    event WithdrawalRequested(
        uint256 indexed index, address indexed requester, uint256 amountWei, bytes btcDestination
    );

    /// Queues a withdrawal request for `msg.value` wei, locked in this contract until bundled
    /// and confirmed — or released back to `requester` on bundle failure — by the
    /// block-production layer.
    function requestWithdrawal(bytes calldata btcDestination) external payable returns (uint256 index) {
        require(msg.value > 0, "WithdrawalRequestQueue: zero amount");
        index = requestCount++;
        requests[index] = Request(msg.sender, msg.value, btcDestination);
        emit WithdrawalRequested(index, msg.sender, msg.value, btcDestination);
    }
}
