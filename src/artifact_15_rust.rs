use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};
use crate::state::*;
use crate::errors::RouterError;
use crate::interfaces::marginfi::MarginfiCPI;
use crate::interfaces::kamino::KaminoCPI;

#[derive(Accounts)]
pub struct WithdrawAndClose<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [GLOBAL_CONFIG_SEED],
        bump = config.bump
    )]
    pub config: Account<'info, GlobalConfig>,

    #[account(
        mut,
        seeds = [POSITION_SEED, user.key().as_ref()],
        bump = position.bump,
        close = user
    )]
    pub position: Account<'info, StrategyPosition>,

    /// CHECK: PDA signer authority
    #[account(
        seeds = [VAULT_AUTHORITY_SEED],
        bump = config.vault_authority_bump
    )]
    pub vault_authority: AccountInfo<'info>,

    pub usdc_mint: Account<'info, Mint>,

    #[account(
        mut,
        constraint = vault_usdc_ata.owner == vault_authority.key()
    )]
    pub vault_usdc_ata: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_usdc_ata.owner == user.key()
    )]
    pub user_usdc_ata: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = vault_sol_ata.owner == vault_authority.key()
    )]
    pub vault_sol_ata: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = vault_kamino_ktoken_ata.owner == vault_authority.key()
    )]
    pub vault_kamino_ktoken_ata: Account<'info, TokenAccount>,

    // Kamino CPI Accounts
    /// CHECK: Kamino Program
    pub kamino_program: AccountInfo<'info>,
    /// CHECK: Lending Market
    pub kamino_lending_market: AccountInfo<'info>,
    /// CHECK: Lending Market Authority
    pub kamino_lending_market_authority: AccountInfo<'info>,
    /// CHECK: SOL Reserve
    #[account(mut)]
    pub kamino_sol_reserve: AccountInfo<'info>,
    /// CHECK: Liquidity Supply
    #[account(mut)]
    pub kamino_sol_supply: AccountInfo<'info>,
    /// CHECK: Collateral Mint
    #[account(mut)]
    pub kamino_ktoken_mint: AccountInfo<'info>,

    // Marginfi CPI Accounts
    /// CHECK: Marginfi Program
    pub marginfi_program: AccountInfo<'info>,
    /// CHECK: Marginfi Group
    pub marginfi_group: AccountInfo<'info>,
    /// CHECK: Marginfi Account
    #[account(mut)]
    pub marginfi_account: AccountInfo<'info>,
    /// CHECK: Marginfi SOL Bank
    #[account(mut)]
    pub marginfi_sol_bank: AccountInfo<'info>,
    /// CHECK: Marginfi SOL Vault
    #[account(mut)]
    pub marginfi_sol_vault: AccountInfo<'info>,
    /// CHECK: Marginfi USDC Bank
    #[account(mut)]
    pub marginfi_usdc_bank: AccountInfo<'info>,
    /// CHECK: Marginfi USDC Vault
    #[account(mut)]
    pub marginfi_usdc_vault: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<WithdrawAndClose>) -> Result<()> {
    let position = &ctx.accounts.position;
    require!(!position.reentrancy_lock, RouterError::ReentrancyViolation);

    let vault_bump = ctx.accounts.config.vault_authority_bump;
    let signer_seeds: &[&[&[u8]]] = &[&[VAULT_AUTHORITY_SEED, &[vault_bump]]];

    // 1. Redeem 100% of Kamino kToken shares to recover SOL
    KaminoCPI::redeem_reserve_collateral(
        ctx.accounts.kamino_program.to_account_info(),
        ctx.accounts.vault_authority.to_account_info(),
        ctx.accounts.kamino_lending_market.to_account_info(),
        ctx.accounts.kamino_lending_market_authority.to_account_info(),
        ctx.accounts.kamino_sol_reserve.to_account_info(),
        ctx.accounts.kamino_sol_supply.to_account_info(),
        ctx.accounts.kamino_ktoken_mint.to_account_info(),
        ctx.accounts.vault_kamino_ktoken_ata.to_account_info(),
        ctx.accounts.vault_sol_ata.to_account_info(),
        ctx.accounts.token_program.to_account_info(),
        position.kamino_shares,
        signer_seeds,
    )?;

    // 2. Repay 100% of Marginfi SOL borrow obligation
    MarginfiCPI::repay(
        ctx.accounts.marginfi_program.to_account_info(),
        ctx.accounts.marginfi_group.to_account_info(),
        ctx.accounts.marginfi_account.to_account_info(),
        ctx.accounts.vault_authority.to_account_info(),
        ctx.accounts.marginfi_sol_bank.to_account_info(),
        ctx.accounts.vault_sol_ata.to_account_info(),
        ctx.accounts.marginfi_sol_vault.to_account_info(),
        ctx.accounts.token_program.to_account_info(),
        position.borrowed_sol,
        signer_seeds,
    )?;

    // 3. Return principal USDC from vault to user
    token::transfer(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.vault_usdc_ata.to_account_info(),
                to: ctx.accounts.user_usdc_ata.to_account_info(),
                authority: ctx.accounts.vault_authority.to_account_info(),
            },
            signer_seeds,
        ),
        position.principal_usdc,
    )?;

    // Update global state tracking
    let config = &mut ctx.accounts.config;
    config.total_collateral_usdc = config.total_collateral_usdc.saturating_sub(position.principal_usdc);
    config.total_debt_sol = config.total_debt_sol.saturating_sub(position.borrowed_sol);

    Ok(())
}