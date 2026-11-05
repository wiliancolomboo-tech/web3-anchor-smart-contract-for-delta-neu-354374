use anchor_lang::prelude::*;

#[error_code]
pub enum ErrorCode {
    #[msg("Provided parameters are outside legitimate bounds.")]
    InvalidParameters,
    #[msg("Deposit amount must be strictly greater than zero.")]
    ZeroDepositAmount,
    #[msg("Withdraw amount must be strictly greater than zero.")]
    ZeroWithdrawAmount,
    #[msg("Insufficient balance for withdrawal.")]
    InsufficientFunds,
    #[msg("Math calculation resulted in an arithmetic overflow.")]
    MathOverflow,
    #[msg("Delta drift is within tolerance limits. Rebalance rejected.")]
    DeltaWithinTolerance,
    #[msg("The strategy is currently paused by the emergency circuit breaker.")]
    StrategyPaused,
    #[msg("Unauthorized signer for this operational instruction.")]
    UnauthorizedSigner,
    #[msg("Token account does not match expected ownership constraint.")]
    UnauthorizedTokenAccount,
    #[msg("Valuation supplied from oracles is non-positive or invalid.")]
    InvalidValuation,
}