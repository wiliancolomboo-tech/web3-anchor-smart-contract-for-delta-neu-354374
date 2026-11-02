use anchor_lang::prelude::*;
use anchor_spl::token::{Mint, Token, TokenAccount};
use crate::state::*;

#[derive(Accounts)]
pub struct InitializeStrategy<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    pub base_mint: Account<'info, Mint>,
    pub volatile_mint: Account<'info, Mint>,

    #[account(
        init,
        payer = admin,
        space = StrategyState::LEN,
        seeds = [STRATEGY_SEED, base_mint.key().as_ref(), volatile_mint.key().as_ref()],
        bump
    )]
    pub strategy: Account<'info, StrategyState>,

    /// CHECK: Vault Authority Seed Checked
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, strategy.key().as_ref()],
        bump
    )]
    pub vault_authority: AccountInfo<'info>,

    #[account(
        init,
        payer = admin,
        token::mint = base_mint,
        token::authority = vault_authority
    )]
    pub vault_base_token: Account<'info, TokenAccount>,

    pub pyth_oracle_base: AccountInfo<'info>,
    pub pyth_oracle_volatile: AccountInfo<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

pub fn handler(
    ctx: Context<InitializeStrategy>,
    target_ltv_bps: u16,
    max_drift_bps: u16,
) -> Result<()> {
    let strategy = &mut ctx.accounts.strategy;
    strategy.admin = ctx.accounts.admin.key();
    strategy.base_mint = ctx.accounts.base_mint.key();
    strategy.volatile_mint = ctx.accounts.volatile_mint.key();
    strategy.auth_bump = ctx.bumps.vault_authority;
    strategy.bump = ctx.bumps.strategy;
    strategy.target_ltv_bps = target_ltv_bps;
    strategy.max_drift_bps = max_drift_bps;
    strategy.is_locked = 0;
    strategy.total_shares = 0;
    strategy.kamino_supplied_amount = 0;
    strategy.marginfi_collateral_amount = 0;
    strategy.marginfi_borrowed_amount = 0;
    strategy.pyth_oracle_base = ctx.accounts.pyth_oracle_base.key();
    strategy.pyth_oracle_volatile = ctx.accounts.pyth_oracle_volatile.key();

    Ok(())
}