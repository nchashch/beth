// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @notice Holds the BIP300 two-way peg's Bitcoin-mainchain -> `beth` deposit reserve, and mints
/// (credits) deposits onto this sidechain. Bitcoin's total issuance is capped at 21,000,000 BTC,
/// so this contract is genesis-funded with exactly that many wei-scaled units
/// (`21_000_000 * 10**18` wei, i.e. `21_000_000 * 10**8` sats at this contract's own
/// `WEI_PER_SAT` scaling) — a hard, self-enforcing supply cap: crediting more than that
/// cumulative total is simply impossible here, the same way it's impossible on Bitcoin mainchain
/// itself. See `beth`'s `src/deposit_vault.rs`/`src/evm.rs` for the system call that drives this.
///
/// Deployed at a fixed predeploy address from genesis (see `genesis.json`) — this file is the
/// human-readable source; deployment ships the compiled runtime bytecode directly via
/// `alloc[address].code`, so there is no constructor and no deployment transaction. The genesis
/// balance (the reserve described above) is set the same way, directly on that `alloc` entry.
contract DepositVault {
    /// See `WithdrawalRequestQueue.sol`'s identical constant for why this is safe to trust as
    /// `msg.sender`.
    address private constant SYSTEM_ADDRESS = 0xffffFFFfFFffffffffffffffFfFFFfffFFFfFFfE;

    /// wei per satoshi, so the L2 asset preserves BTC's 8 decimals of precision within the usual
    /// 18-decimal wei denomination. Matches `WithdrawalRequestQueue`'s identical constant.
    uint256 private constant WEI_PER_SAT = 1e10;

    /// `success` is `false` if `recipient`'s receive/fallback rejected the credit (e.g. a
    /// contract with no payable fallback, or one that reverts) — in which case the wei stays
    /// held in this contract rather than being lost, but is not retried. Mirrors
    /// `WithdrawalRequestQueue.WithdrawalRefunded`'s identical tradeoff, for the identical
    /// reason: one recipient rejecting its credit must not block every other recipient's in the
    /// same block.
    event DepositCredited(address indexed recipient, uint256 amountSats, bool success);

    /// Credits `recipients[i]` with `amountsSats[i]` BIP300 deposits (Bitcoin mainchain -> this
    /// sidechain), for every `i` in `[0, recipients.length)`. Callable only via a "system call"
    /// (see `SYSTEM_ADDRESS`) — never by a normal transaction.
    ///
    /// Both arrays are *independently recomputed by every node* from the same already-agreed
    /// data: the BIP300/301 enforcer's report of mainchain deposit events for this block's own
    /// committed mainchain range (the same range `WithdrawalRequestQueue.
    /// systemReportBundleLifecycle` resolves bundle outcomes for — see that function's doc
    /// comment for the full reasoning on why being system-only isn't "trust the caller" here).
    function systemCreditDeposits(address[] calldata recipients, uint256[] calldata amountsSats)
        external
    {
        require(msg.sender == SYSTEM_ADDRESS, "DepositVault: not a system call");
        for (uint256 i = 0; i < recipients.length; i++) {
            uint256 amountWei = amountsSats[i] * WEI_PER_SAT;
            (bool success,) = payable(recipients[i]).call{value: amountWei}("");
            emit DepositCredited(recipients[i], amountsSats[i], success);
        }
    }
}
