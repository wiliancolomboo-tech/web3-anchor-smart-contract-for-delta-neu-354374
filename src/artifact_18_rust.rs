use anchor_lang::prelude::*;

pub mod constants;
pub mod errors;
pub mod instructions;
pub mod interfaces;
pub mod math;
pub mod state;

use instructions::*;

declare_id!("DeltaRouter1111111111111111111111111111111111");

#[program]
pub mod delta_neutral_router {
    use super::*;

    pub fn initialize_vault(
        ctx: Context<InitializeVault>,
        vault_id: u64,
        drift_threshold_bps: u64,
        target_ltv_bps: u64,
    ) -> Result<()> {
        instructions::initialize_vault::handler(ctx, vault_id, drift_threshold_bps, target_ltv_bps)
    }

    pub fn deposit_and_hedge(
        ctx: Context<DepositAndHedge>,
        collateral_amount: u64,
        hedge_borrow_amount: u64,
    ) -> Result<()> {
        instructions::deposit_and_hedge::handler(ctx, collateral_amount, hedge_borrow_amount)
    }

    pub fn rebalance_drift(
        ctx: Context<RebalanceDrift>,
        spot_val_usd: u128,
        hedge_val_usd: u128,
        rebalance_amount: u64,
    ) -> Result<()> {
        instructions::rebalance_drift::handler(ctx, spot_val_usd, hedge_val_usd, rebalance_amount)
    }

    pub fn harvest_and_compound(
        ctx: Context<HarvestAndCompound>,
        harvest_amount: u64,
    ) -> Result<()> {
        instructions::harvest_and_compound::handler(ctx, harvest_amount)
    }

    pub fn withdraw_and_unwind(
        ctx: Context<WithdrawAndUnwind>,
        shares_to_burn: u64,
    ) -> Result<()> {
        instructions::withdraw_and_unwind::handler(ctx, shares_to_burn)
    }

    pub fn emergency_pause(ctx: Context<EmergencyControl>, pause_status: bool) -> Result<()> {
        instructions::emergency_unwind::handler(ctx, pause_status)
    }
}