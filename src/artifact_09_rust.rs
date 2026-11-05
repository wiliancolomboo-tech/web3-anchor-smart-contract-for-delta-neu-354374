use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, MintTo, Token, TokenAccount, Transfer};
use crate::state::{VaultState, VAULT_SEED};
use crate::errors::RouterError;
use crate::events::DepositEvent;

#[derive(Accounts)]
pub struct Deposit<'info> {
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

pub fn handler(ctx: Context<Deposit>, amount: u64) -> Result<()> {
    require!(!ctx.accounts.vault.is_paused, RouterError::VaultPaused);
    require!(amount > 0, RouterError::ZeroDepositAmount);

    let total_shares = ctx.accounts.share_mint.supply;
    let total_base = ctx.accounts.vault.total_managed_base;

    // Calculate LP shares to mint:
    // If first deposit, shares = amount.
    // Else shares = (amount * total_shares) / total_base
    let shares_to_mint = if total_shares == 0 || total_base == 0 {
        amount
    } else {
        (amount as u128)
            .checked_mul(total_shares as u128)
            .ok_or(RouterError::MathOverflow)?
            .checked_div(total_base as u128)
            .ok_or(RouterError::MathOverflow)? as u64
    };

    require!(shares_to_mint > 0, RouterError::ZeroShares);

    // Transfer base collateral into Vault
    let transfer_ctx = CpiContext::new(
        ctx.accounts.token_program.to_account_info(),
        Transfer {
            from: ctx.accounts.user_base_token.to_account_info(),
            to: ctx.accounts.base_vault.to_account_info(),
            authority: ctx.accounts.user.to_account_info(),
        },
    );
    token::transfer(transfer_ctx, amount)?;

    // Mint LP shares via Vault PDA
    let base_mint_key = ctx.accounts.vault.base_mint;
    let seeds = &[
        VAULT_SEED,
        base_mint_key.as_ref(),
        &[ctx.accounts.vault.vault_bump],
    ];
    let signer = &[&seeds[..]];

    let mint_ctx = CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        MintTo {
            mint: ctx.accounts.share_mint.to_account_info(),
            to: ctx.accounts.user_share_token.to_account_info(),
            authority: ctx.accounts.vault.to_account_info(),
        },
        signer,
    );
    token::mint_to(mint_ctx, shares_to_mint)?;

    // Update vault state accounting
    let vault = &mut ctx.accounts.vault;
    vault.total_managed_base = vault
        .total_managed_base
        .checked_add(amount)
        .ok_or(RouterError::MathOverflow)?;

    emit!(DepositEvent {
        user: ctx.accounts.user.key(),
        base_amount: amount,
        shares_minted: shares_to_mint,
        timestamp: Clock::get()?.unix_timestamp,
    });

    Ok(())
}