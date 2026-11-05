use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, Mint, Token, TokenAccount, Transfer};
use crate::state::{VaultState, VAULT_SEED};
use crate::errors::RouterError;
use crate::events::WithdrawEvent;

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED, vault.base_mint.as_ref()],
        bump = vault.vault_bump,
        has_one = base_vault,
        has_one = share_mint
    )]
    pub vault: Account<'info, VaultState>,

    #[account(mut)]
    pub base_vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub share_mint: Account<'info, Mint>,

    #[account(
        mut,
        constraint = user_base_token.owner == user.key(),
        constraint = user_base_token.mint == vault.base_mint
    )]
    pub user_base_token: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_share_token.owner == user.key(),
        constraint = user_share_token.mint == vault.share_mint
    )]
    pub user_share_token: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<Withdraw>, shares_to_burn: u64) -> Result<()> {
    require!(shares_to_burn > 0, RouterError::ZeroShares);

    let total_shares = ctx.accounts.share_mint.supply;
    let total_base = ctx.accounts.vault.total_managed_base;

    require!(total_shares > 0, RouterError::InsufficientLiquidity);

    // Compute base payout = (shares_to_burn * total_base) / total_shares
    let base_to_return = (shares_to_burn as u128)
        .checked_mul(total_base as u128)
        .ok_or(RouterError::MathOverflow)?
        .checked_div(total_shares as u128)
        .ok_or(RouterError::MathOverflow)? as u64;

    require!(base_to_return > 0, RouterError::ZeroDepositAmount);
    require!(
        ctx.accounts.base_vault.amount >= base_to_return,
        RouterError::InsufficientLiquidity
    );

    // 1. Burn user shares
    let burn_ctx = CpiContext::new(
        ctx.accounts.token_program.to_account_info(),
        Burn {
            mint: ctx.accounts.share_mint.to_account_info(),
            from: ctx.accounts.user_share_token.to_account_info(),
            authority: ctx.accounts.user.to_account_info(),
        },
    );
    token::burn(burn_ctx, shares_to_burn)?;

    // 2. Transfer base collateral to user
    let base_mint_key = ctx.accounts.vault.base_mint;
    let seeds = &[
        VAULT_SEED,
        base_mint_key.as_ref(),
        &[ctx.accounts.vault.vault_bump],
    ];
    let signer = &[&seeds[..]];

    let transfer_ctx = CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        Transfer {
            from: ctx.accounts.base_vault.to_account_info(),
            to: ctx.accounts.user_base_token.to_account_info(),
            authority: ctx.accounts.vault.to_account_info(),
        },
        signer,
    );
    token::transfer(transfer_ctx, base_to_return)?;

    // Update managed collateral tracking
    let vault = &mut ctx.accounts.vault;
    vault.total_managed_base = vault
        .total_managed_base
        .checked_sub(base_to_return)
        .ok_or(RouterError::MathOverflow)?;

    emit!(WithdrawEvent {
        user: ctx.accounts.user.key(),
        shares_burned: shares_to_burn,
        base_returned: base_to_return,
        timestamp: Clock::get()?.unix_timestamp,
    });

    Ok(())
}