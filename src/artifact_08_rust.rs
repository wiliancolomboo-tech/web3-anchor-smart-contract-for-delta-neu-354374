use anchor_lang::prelude::*;

#[event]
pub struct StrategyInitializedEvent {
    pub admin: Pubkey,
    pub keeper: Pubkey,
    pub target_ltv_bps: u16,
    pub rebalance_threshold_bps: u16,
    pub timestamp: i64,
}

#[event]
pub struct DepositedEvent {
    pub depositor: Pubkey,
    pub amount: u64,
    pub timestamp: i64,
}

#[event]
pub struct RebalancedEvent {
    pub keeper: Pubkey,
    pub spot_price_usd: u64,
    pub collateral_val: u64,
    pub borrow_val: u64,
    pub drift_bps: u16,
    pub timestamp: i64,
}

#[event]
pub struct WithdrawnEvent {
    pub depositor: Pubkey,
    pub amount: u64,
    pub timestamp: i64,
}

#[event]
pub struct EmergencyPauseEvent {
    pub admin: Pubkey,
    pub is_paused: bool,
    pub timestamp: i64,
}