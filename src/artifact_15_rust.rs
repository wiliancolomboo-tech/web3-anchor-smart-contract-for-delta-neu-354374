use anchor_lang::prelude::*;

pub mod errors;
pub mod events;
pub mod instructions;
pub mod state;

use instructions::*;

declare_id!("AegisDeltaNeutrav11111111111111111111111111");

#[program]
pub mod delta_neutral_router {
    use super::*;

    pub fn initialize_vault(
        ctx: Context<InitializeVault>,
        target_ltv_bps: u16,
        max_ltv_bps: u16,
        drift_tolerance_bps: u16,
    ) -> Result<()> {
        instructions::initialize::handler(ctx, target_ltv_bps, max_ltv_bps, drift_tolerance_bps)
    }

    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        instructions::deposit::handler(ctx, amount)
    }

    pub fn rebalance_route(
        ctx: Context<RebalanceRoute>,
        amount_base_to_deposit: u64,
        amount_target_to_borrow: u64,
        effective_target_price_scaled: u64,
    ) -> Result<()> {
        instructions::rebalance_route::handler(
            ctx,
            amount_base_to_deposit,
            amount_target_to_borrow,
            effective_target_price_scaled,
        )
    }

    pub fn harvest_and_compound(
        ctx: Context<HarvestAndCompound>,
        gross_yield_amount: u64,
        borrow_interest_liability: u64,
    ) -> Result<()> {
        instructions::harvest_and_compound::handler(
            ctx,
            gross_yield_amount,
            borrow_interest_liability,
        )
    }

    pub fn withdraw(ctx: Context<Withdraw>, shares_to_burn: u64) -> Result<()> {
        instructions::withdraw::handler(ctx, shares_to_burn)
    }

    pub fn emergency_unwind(ctx: Context<EmergencyUnwind>, pause: bool) -> Result<()> {
        instructions::emergency_unwind::handler(ctx, pause)
    }
}