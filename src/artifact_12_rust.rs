use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};
use crate::errors::AegisError;
use crate::state::*;

#[derive(Accounts)]
pub struct WithdrawCapital<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [STRATEGY_SEED, strategy.base_mint.as_ref(), strategy.volatile_mint.as_ref()],
        bump = strategy.bump
    )]
    pub strategy: Account<'info, StrategyState>,

    /// CHECK: Vault Authority Seed Signer
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, strategy.key().as_ref()],
        bump = strategy.auth_bump
    )]
    pub vault_authority: AccountInfo<'info>,

    #[account(mut)]
    pub vault_base_token: Account<'info, TokenAccount>,

    #[account(mut)]
    pub user_base_token: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<WithdrawCapital>, shares: u64) -> Result<()> {
    let strategy = &mut ctx.accounts.strategy;

    require!(strategy.is_locked == 0, AegisError::ReentrancyGuardTriggered);
    require!(shares <= strategy.total_shares, AegisError::InsufficientLiquidity);

    strategy.is_locked = 1;

    // Compute base capital redemption value
    let total_strategy_val = strategy
        .kamino_supplied_amount
        .checked_add(strategy.marginfi_collateral_amount)
        .unwrap()
        .saturating_sub(strategy.marginfi_borrowed_amount);

    let payout_amount = (shares as u128)
        .checked_mul(total_strategy_val as u128)
        .unwrap()
        .checked_div(strategy.total_shares as u128)
        .unwrap() as u64;

    strategy.total_shares = strategy.total_shares.checked_sub(shares).unwrap();

    // Authority seeds for signing transfer
    let strategy_key = strategy.key();
    let authority_seeds: &[&[&[u8]]] = &[&[
        VAULT_AUTHORITY_SEED,
        strategy_key.as_ref(),
        &[strategy.auth_bump],
    ]];

    let cpi_accounts = Transfer {
        from: ctx.accounts.vault_base_token.to_account_info(),
        to: ctx.accounts.user_base_token.to_account_info(),
        authority: ctx.accounts.vault_authority.to_account_info(),
    };
    let cpi_ctx = CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        cpi_accounts,
        authority_seeds,
    );
    token::transfer(cpi_ctx, payout_amount)?;

    strategy.is_locked = 0;
    Ok(())
}