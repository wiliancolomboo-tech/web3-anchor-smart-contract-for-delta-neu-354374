use anchor_lang::prelude::*;

pub mod errors;
pub mod instructions;
pub mod interfaces;
pub mod math;
pub mod state;

use instructions::*;

declare_id!("Aegis11111111111111111111111111111111111111");

#[program]
pub mod aegis_yield {
    use super::*;

    pub fn initialize(
        ctx: Context<Initialize>,
        min_health_factor_bps: u32,
        delta_tolerance_bps: u32,
    ) -> Result<()> {
        instructions::initialize::handler(ctx, min_health_factor_bps, delta_tolerance_bps)
    }

    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        instructions::deposit::handler(ctx, amount)
    }

    pub fn rebalance(
        ctx: Context<Rebalance>,
        oracle_base_price: u64,
        rebalance_amount_base: u64,
        rebalance_amount_quote: u64,
        is_expanding: bool,
    ) -> Result<()> {
        instructions::rebalance::handler(
            ctx,
            oracle_base_price,
            rebalance_amount_base,
            rebalance_amount_quote,
            is_expanding,
        )
    }

    pub fn withdraw(ctx: Context<Withdraw>, shares: u128) -> Result<()> {
        instructions::withdraw::handler(ctx, shares)
    }
}