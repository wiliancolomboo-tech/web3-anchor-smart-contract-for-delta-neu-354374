use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};
use crate::state::{VaultState, VAULT_SEED};
use crate::errors::RouterError;
use crate::events::HarvestEvent;

#[derive(Accounts)]
pub struct HarvestAndCompound<'info> {
    #[account(
        mut,
        constraint = keeper.key() == vault.keeper || keeper.key() == vault.admin @ RouterError::Unauthorized
    )]
    pub keeper: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED, vault.base_mint.as_ref()],
        bump = vault.vault_bump,
        has_one = base_vault
    )]
    pub vault: Account<'info, VaultState>,

    #[account(mut)]
    pub base_vault: Account<'info, TokenAccount>,

    /// Reward or surplus tokens generated from Kamino position
    #[account(mut)]
    pub yield_source_token: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(
    ctx: Context<HarvestAndCompound>,
    gross_yield_amount: u64,
    borrow_interest_liability: u64,
) -> Result<()> {
    require!(!ctx.accounts.vault.is_paused, RouterError::VaultPaused);

    let net_surplus = gross_yield_amount
        .checked_sub(borrow_interest_liability)
        .ok_or(RouterError::InsufficientLiquidity)?;

    let base_mint_key = ctx.accounts.vault.base_mint;
    let seeds = &[
        VAULT_SEED,
        base_mint_key.as_ref(),
        &[ctx.accounts.vault.vault_bump],
    ];
    let signer = &[&seeds[..]];

    if net_surplus > 0 {
        // Transfer net surplus into vault base reserve
        let transfer_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.yield_source_token.to_account_info(),
                to: ctx.accounts.base_vault.to_account_info(),
                authority: ctx.accounts.vault.to_account_info(),
            },
            signer,
        );
        token::transfer(transfer_ctx, net_surplus)?;

        // Auto-compound total managed base
        let vault = &mut ctx.accounts.vault;
        vault.total_managed_base = vault
            .total_managed_base
            .checked_add(net_surplus)
            .ok_or(RouterError::MathOverflow)?;
    }

    emit!(HarvestEvent {
        gross_rewards: gross_yield_amount,
        borrow_fee_paid: borrow_interest_liability,
        net_surplus_compounded: net_surplus,
        timestamp: Clock::get()?.unix_timestamp,
    });

    Ok(())
}