// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @notice Queues BIP300 withdrawal requests — this sidechain (`beth`) to Bitcoin mainchain —
/// for the block-production layer to batch into withdrawal bundles (M6 transactions) and submit
/// to the BIP300/301 enforcer. See `beth`'s `src/payload.rs`/`src/enforcer.rs` for the deposit
/// side (mainchain -> sidechain), which this contract is the reverse-direction counterpart to,
/// and `src/withdrawal_bundle.rs` for the bundle-selection/M6-construction logic that reads and
/// writes this queue.
///
/// Deployed at a fixed predeploy address from genesis (see `genesis.json`) — this file is the
/// human-readable source; deployment ships the compiled runtime bytecode directly via
/// `alloc[address].code`, so there is no constructor and no deployment transaction.
///
/// A request's lifecycle (`status`) is written back by `systemReportBundleLifecycle` — see its
/// doc comment for why that's restricted to `SYSTEM_ADDRESS` and how every node computes
/// identical calls to it, rather than trusting whatever the block producer claims.
contract WithdrawalRequestQueue {
    /// The sender every EIP-4788/2935/7002-style "system call" (a call executed directly by the
    /// block-building/validation machinery, with no transaction and no signature) uses as
    /// `msg.sender`. No externally-owned account controls this address's key, so nothing but the
    /// protocol itself can ever satisfy `msg.sender == SYSTEM_ADDRESS`.
    address private constant SYSTEM_ADDRESS = 0xffffFFFfFFffffffffffffffFfFFFfffFFFfFFfE;

    /// wei per satoshi, matching the deposit side's `10^10` scaling (see `payload.rs`), so the L2
    /// asset preserves BTC's 8 decimals of precision.
    uint256 private constant WEI_PER_SAT = 1e10;

    /// A request's position in the BIP300 withdrawal-bundle lifecycle.
    ///
    /// `Pending`: eligible for selection into a new bundle.
    /// `Bundled`: assigned to `bundleM6id`, awaiting that bundle's mainchain outcome.
    /// `Confirmed`: its bundle succeeded — the BTC payout already happened, so the locked wei
    /// stays in this contract permanently (its job is done; there is nothing left to refund).
    /// `Refunded`: its bundle failed — the locked wei has been transferred back to `requester`.
    /// `Bundled`, `Confirmed`, and `Refunded` are all permanently ineligible for reselection.
    uint8 private constant STATUS_PENDING = 0;
    uint8 private constant STATUS_BUNDLED = 1;
    uint8 private constant STATUS_CONFIRMED = 2;
    uint8 private constant STATUS_REFUNDED = 3;

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
        /// One of the `STATUS_*` constants above.
        uint8 status;
        /// The bundle this request is (or was) assigned to. Meaningless while `status ==
        /// STATUS_PENDING`.
        bytes32 bundleM6id;
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
    /// else a `length * 2 + 1` marker with data starting at `keccak256(entry_slot + 3)`),
    /// `status` (slot 4, right-aligned single byte — alone in its slot since the following field
    /// is a full `bytes32`), `bundleM6id` (slot 5).
    mapping(uint256 => Request) public requests;

    event WithdrawalRequested(
        uint256 indexed index,
        address indexed requester,
        uint256 valueSats,
        uint256 mainFeeSats,
        bytes btcDestination
    );

    event WithdrawalBundled(uint256 indexed index, bytes32 indexed m6id);
    event WithdrawalConfirmed(uint256 indexed index, bytes32 indexed m6id);
    event WithdrawalRefunded(uint256 indexed index, bytes32 indexed m6id, bool transferSucceeded);

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
        requests[index] = Request(msg.sender, valueSats, mainFeeSats, btcDestination, STATUS_PENDING, bytes32(0));
        emit WithdrawalRequested(index, msg.sender, valueSats, mainFeeSats, btcDestination);
    }

    /// Advances the withdrawal-bundle lifecycle for one block. Callable only via a "system
    /// call" (see `SYSTEM_ADDRESS`) — never by a normal transaction.
    ///
    /// Every argument here is *independently recomputed by every node* from data they already
    /// have to agree on: `bundledIndices`/`newM6id` from a pure, deterministic function of this
    /// contract's own `Pending` requests (`beth`'s `select_withdrawal_bundle`); the resolution
    /// lists from the BIP300/301 enforcer's report of mainchain events for this block's own
    /// committed mainchain range. Because every node computes the same call independently, this
    /// being system-only isn't "trust the caller" — it's "only the protocol can trigger this
    /// state transition," the same property EIP-4895 withdrawals already have for balances. See
    /// `src/withdrawal_bundle.rs`'s module doc comment for the full picture.
    ///
    /// - `bundledIndices`: requests newly assigned to `newM6id` (empty if no bundle was selected
    ///   this block). Each must currently be `STATUS_PENDING`.
    /// - `succeededIndices` / `failedIndices`: requests whose bundle just resolved. Each must
    ///   currently be `STATUS_BUNDLED`. Failed requests are refunded here, from this contract's
    ///   own held balance (locked at request time) — not minted.
    function systemReportBundleLifecycle(
        uint256[] calldata bundledIndices,
        bytes32 newM6id,
        uint256[] calldata succeededIndices,
        uint256[] calldata failedIndices
    ) external {
        require(msg.sender == SYSTEM_ADDRESS, "WithdrawalRequestQueue: not a system call");

        for (uint256 i = 0; i < bundledIndices.length; i++) {
            uint256 index = bundledIndices[i];
            require(requests[index].status == STATUS_PENDING, "WithdrawalRequestQueue: not pending");
            requests[index].status = STATUS_BUNDLED;
            requests[index].bundleM6id = newM6id;
            emit WithdrawalBundled(index, newM6id);
        }

        for (uint256 i = 0; i < succeededIndices.length; i++) {
            uint256 index = succeededIndices[i];
            require(requests[index].status == STATUS_BUNDLED, "WithdrawalRequestQueue: not bundled");
            requests[index].status = STATUS_CONFIRMED;
            emit WithdrawalConfirmed(index, requests[index].bundleM6id);
        }

        for (uint256 i = 0; i < failedIndices.length; i++) {
            uint256 index = failedIndices[i];
            Request storage request = requests[index];
            require(request.status == STATUS_BUNDLED, "WithdrawalRequestQueue: not bundled");
            bytes32 m6id = request.bundleM6id;
            // Checks-effects-interactions: mark refunded *before* the external call below, so a
            // reentrant call can't observe or act on a still-"bundled" request.
            request.status = STATUS_REFUNDED;
            uint256 amountWei = (request.valueSats + request.mainFeeSats) * WEI_PER_SAT;
            (bool success,) = payable(request.requester).call{value: amountWei}("");
            // A failed transfer (e.g. `requester` is a contract that reverts) does not revert
            // this call — that would block unrelated requests' resolutions in the same block.
            // The wei stays locked in this contract; `status` is still `STATUS_REFUNDED`, so
            // this is never retried.
            emit WithdrawalRefunded(index, m6id, success);
        }
    }
}
