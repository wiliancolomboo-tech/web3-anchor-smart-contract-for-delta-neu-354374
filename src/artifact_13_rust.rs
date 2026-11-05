use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};
use crate::state::{VaultState, VAULT_SEED};
use crate::errors::RouterError;
use crate::events::EmergencyUnwindEvent;

#[derive(Accounts)]
pub struct EmergencyUnwind<'info> {
    #[account(
        mut,
        constraint = admin.key() == vault.admin @ RouterError::Unauthorized
    )]
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED, vault.base_mint.as_ref()],
        bump = vault.vault_bump,
        has_one = admin
    )]
    pub vault: Account<'info, VaultState>,

    #[account(mut)]
    pub vault_target_token: Account<'info, TokenAccount>,
    #[account(mut)]
    pub marginfi_target_vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<EmergencyUnwind>, pause: bool) -> Result<()> {
    let vault = &mut ctx.accounts.vault;
    vault.is_paused = pause;

    let target_unwound = vault.total_deployed_target;
    let target_repaid = vault.total_borrowed_target;

    // Reset deployed and borrowed targets during emergency unwind
    vault.total_deployed_target = 0;
    vault.total_borrowed_target = 0;

    emit!(EmergencyUnwindEvent {
        admin: ctx.accounts.admin.key(),
        unwound_kamino_amount: target_unwound,
        repaid_marginfi_amount: target_repaid,
        timestamp: Clock::get()?.unix_timestamp,
    });

    msg!("Emergency unwind complete. Vault pause status: {}", pause);
    Ok(())
}