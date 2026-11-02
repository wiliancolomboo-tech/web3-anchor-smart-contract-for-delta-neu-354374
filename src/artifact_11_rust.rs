use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};
use crate::errors::AegisError;
use crate::math::FixedPointMath;
use crate::state::*;

#[derive(Accounts)]
pub struct DepositCapital<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [STRATEGY_SEED, strategy.base_mint.as_ref(), strategy.volatile_mint.as_ref()],
        bump = strategy.bump
    )]
    pub strategy: Account<'info, StrategyState>,

    #[account(
        mut,
        constraint = user_base_token.mint == strategy.base_mint
    )]
    pub user_base_token: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = vault_base_token.mint == strategy.base_mint
    )]
    pub vault_base_token: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<DepositCapital>, amount: u64) -> Result<()> {
    let strategy = &mut ctx.accounts.strategy;

    require!(strategy.is_locked == 0, AegisError::ReentrancyGuardTriggered);
    strategy.is_locked = 1;

    // 1. Transfer user funds into strategy vault
    let cpi_accounts = Transfer {
        from: ctx.accounts.user_base_token.to_account_info(),
        to: ctx.accounts.vault_base_token.to_account_info(),
        authority: ctx.accounts.user.to_account_info(),
    };
    let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
    token::transfer(cpi_ctx, amount)?;

    // 2. Share Minting Calculation
    let total_strategy_val = strategy
        .kamino_supplied_amount
        .checked_add(strategy.marginfi_collateral_amount)
        .unwrap()
        .saturating_sub(strategy.marginfi_borrowed_amount);

    let shares_to_mint = FixedPointMath::compute_shares_to_mint(
        amount,
        total_strategy_val,
        strategy.total_shares,
    )?;

    require!(shares_to_mint > 0, AegisError::ZeroSharesMinted);

    // 3. Update State
    strategy.total_shares = strategy.total_shares.checked_add(shares_to_mint).unwrap();
    // Split incoming capital: 50% Kamino liquidity, 50% Marginfi collateral
    let split_capital = amount / 2;
    strategy.kamino_supplied_amount = strategy.kamino_supplied_amount.checked_add(split_capital).unwrap();
    strategy.marginfi_collateral_amount = strategy.marginfi_collateral_amount.checked_add(split_capital).unwrap();

    strategy.is_locked = 0;
    Ok(())
}