use anchor_lang::prelude::*;

#[account]
#[derive(InitSpace)]
pub struct GlobalConfig {
    pub admin: Pubkey,
    pub keeper: Pubkey,
    pub target_ltv_bps: u16,
    pub rebalance_threshold_bps: u16,
    pub performance_fee_bps: u16,
    pub is_paused: bool,
    pub bump: u8,
    pub vault_bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct StrategyVault {
    pub total_deposited: u64,
    pub last_rebalance_timestamp: i64,
    pub last_recorded_drift_bps: u16,
}

#[account]
#[derive(InitSpace)]
pub struct UserPosition {
    pub owner: Pubkey,
    pub deposited_amount: u64,
    pub last_deposit_timestamp: i64,
    pub bump: u8,
}