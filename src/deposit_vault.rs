//! Credits BIP300 deposits (Bitcoin mainchain -> this sidechain) onto `DepositVault` (see
//! `contracts/DepositVault.sol`) via [`crate::evm`]'s "system call" — the same
//! `msg.sender == SYSTEM_ADDRESS`-gated mechanism [`crate::withdrawal_bundle`] uses for the
//! withdrawal-bundle lifecycle, and for the identical reason: every node independently
//! recomputes the same `DepositVault.systemCreditDeposits(...)` call from the same
//! already-agreed data (the enforcer's report of mainchain deposit events for this block's own
//! committed mainchain range), so a forged or missing credit is caught by standard state-root
//! validation rather than needing a bespoke consensus check.
//!
//! This sidechain previously minted deposits as EIP-4895 `Withdrawal`s — a direct, unconditional
//! protocol-level balance mutation with no EVM execution and no possibility of failure. Routing
//! deposits through this contract instead is a deliberate, known behavior change: crediting now
//! happens via a real `CALL`, so a recipient whose receive/fallback rejects the transfer (no
//! payable fallback, or one that reverts) simply doesn't get credited — see
//! `DepositVault.DepositCredited`'s doc comment for why that's accepted rather than retried,
//! mirroring `WithdrawalRequestQueue.WithdrawalRefunded`'s identical tradeoff.

use alloy_primitives::{Address, U256, address};
use alloy_sol_types::{SolCall, sol};

use crate::enforcer::Deposit;

/// The `DepositVault` predeploy address (see `genesis.json`) — genesis-funded with
/// `21_000_000 * 10**18` wei (21,000,000 BTC's worth, at this contract's `WEI_PER_SAT` scaling),
/// matching Bitcoin's own hard issuance cap. See the contract's doc comment.
pub const DEPOSIT_VAULT_ADDRESS: Address = address!("0x000000000000000000000000000000000000d300");

sol! {
    /// Matches `DepositVault.systemCreditDeposits` exactly — see its doc comment in
    /// `contracts/DepositVault.sol`.
    function systemCreditDeposits(address[] recipients, uint256[] amountsSats) external;
}

/// ABI-encodes `deposits` as a `systemCreditDeposits` call, ready for
/// `Evm::transact_system_call`. Returns `None` if `deposits` is empty (nothing to credit — the
/// caller can skip the system call entirely for this block).
pub fn deposits_calldata(deposits: &[Deposit]) -> Option<Vec<u8>> {
    if deposits.is_empty() {
        return None;
    }
    Some(
        systemCreditDepositsCall {
            recipients: deposits.iter().map(|deposit| deposit.address).collect(),
            amountsSats: deposits
                .iter()
                .map(|deposit| U256::from(deposit.value_sats))
                .collect(),
        }
        .abi_encode(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deposits_calldata_is_none_when_empty() {
        assert!(deposits_calldata(&[]).is_none());
    }

    #[test]
    fn deposits_calldata_round_trips_via_sol_decode() {
        let deposits = vec![
            Deposit {
                address: Address::repeat_byte(1),
                value_sats: 100,
            },
            Deposit {
                address: Address::repeat_byte(2),
                value_sats: 200,
            },
        ];
        let calldata = deposits_calldata(&deposits).expect("non-empty");
        let decoded = systemCreditDepositsCall::abi_decode(&calldata).expect("valid ABI");
        assert_eq!(
            decoded.recipients,
            vec![Address::repeat_byte(1), Address::repeat_byte(2)]
        );
        assert_eq!(
            decoded.amountsSats,
            vec![U256::from(100u64), U256::from(200u64)]
        );
    }
}
