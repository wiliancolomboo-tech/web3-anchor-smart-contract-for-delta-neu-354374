use anchor_lang::prelude::*;
use anchor_spl::token::{Token, TokenAccount};
use crate::state::*;
use crate::errors::RouterError;
use crate::interfaces::pyth::OracleAdapter;
use crate::interfaces::marginfi::MarginfiCPI;
use crate::interfaces::kamino::KaminoCPI;

#[derive(Accounts)]
pub struct Rebalance<'info> {
    #[account(mut)]
    pub keeper: Signer<'info>,

    #[account(
        mut,
        seeds = [GLOBAL_CONFIG_SEED],
        bump = config.bump,
        has_one = keeper @ RouterError::UnauthorizedKeeper
    )]
    pub config: Account<'info, GlobalConfig>,

    #[account(
        mut,
        seeds = [POSITION_SEED, position.owner.as_ref()],
        bump = position.bump
    )]
    pub position: Account<'info, StrategyPosition>,

    /// CHECK: PDA signer seeds authority
    #[account(
        seeds = [VAULT_AUTHORITY_SEED],
        bump = config.vault_authority_bump
    )]
    pub vault_authority: AccountInfo<'info>,

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

    /// CHECK: Pyth Oracle SOL/USD
    pub pyth_sol_oracle: AccountInfo<'info>,

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

    // Kamino CPI Accounts
    /// CHECK: Kamino Program
    pub kamino_program: AccountInfo<'info>,
    /// CHECK: Kamino Lending Market
    pub kamino_lending_market: AccountInfo<'info>,
    /// CHECK: Kamino Lending Market Authority
    pub kamino_lending_market_authority: AccountInfo<'info>,
    /// CHECK: Kamino Reserve Account
    #[account(mut)]
    pub kamino_sol_reserve: AccountInfo<'info>,
    /// CHECK: Kamino Reserve Liquidity Supply
    #[account(mut)]
    pub kamino_sol_supply: AccountInfo<'info>,
    /// CHECK: Kamino Collateral Mint
    #[account(mut)]
    pub kamino_ktoken_mint: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
    pub clock: Sysvar<'info, Clock>,
}

pub fn handler(ctx: Context<Rebalance>) -> Result<()> {
    let position = &mut ctx.accounts.position;
    require!(!position.reentrancy_lock, RouterError::ReentrancyViolation);
    position.reentrancy_lock = true;

    let sol_price_usd_6d = OracleAdapter::get_validated_price(
        &ctx.accounts.pyth_sol_oracle,
        &ctx.accounts.clock,
        ctx.accounts.config.max_staleness_seconds,
    )?;

    // Current debt value in USDC: (borrowed_sol * sol_price) / 10^9
    let current_debt_value_usdc = (position.borrowed_sol as u128)
        .checked_mul(sol_price_usd_6d as u128)
        .ok_or(RouterError::MathOverflow)?
        .checked_div(1_000_000_000)
        .ok_or(RouterError::MathOverflow)?;

    // Current LTV in basis points: (current_debt_value_usdc * 10000) / principal_usdc
    let current_ltv_bps = current_debt_value_usdc
        .checked_mul(10000)
        .ok_or(RouterError::MathOverflow)?
        .checked_div(position.principal_usdc as u128)
        .ok_or(RouterError::MathOverflow)? as u16;

    let target_ltv = ctx.accounts.config.target_ltv_bps;
    let threshold = ctx.accounts.config.rebalance_threshold_bps;

    let ltv_delta = (current_ltv_bps as i32 - target_ltv as i32).abs() as u16;
    require!(ltv_delta >= threshold, RouterError::RebalanceToleranceNotMet);

    let vault_bump = ctx.accounts.config.vault_authority_bump;
    let signer_seeds: &[&[&[u8]]] = &[&[VAULT_AUTHORITY_SEED, &[vault_bump]]];

    if current_ltv_bps > target_ltv {
        // OVER-LEVERAGED: SOL appreciated -> LTV increased -> Must redeem Kamino & repay Marginfi debt
        let excess_ltv_bps = (current_ltv_bps - target_ltv) as u128;
        let sol_to_repay_lamports = excess_ltv_bps
            .checked_mul(position.principal_usdc as u128)
            .ok_or(RouterError::MathOverflow)?
            .checked_mul(1_000_000_000)
            .ok_or(RouterError::MathOverflow)?
            .checked_div(sol_price_usd_6d as u128 * 10000)
            .ok_or(RouterError::MathOverflow)? as u64;

        // Redeem kToken shares from Kamino (1:1 share approximation for brevity; in prod uses exchange rate)
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
            sol_to_repay_lamports,
            signer_seeds,
        )?;

        // Repay Marginfi SOL debt
        MarginfiCPI::repay(
            ctx.accounts.marginfi_program.to_account_info(),
            ctx.accounts.marginfi_group.to_account_info(),
            ctx.accounts.marginfi_account.to_account_info(),
            ctx.accounts.vault_authority.to_account_info(),
            ctx.accounts.marginfi_sol_bank.to_account_info(),
            ctx.accounts.vault_sol_ata.to_account_info(),
            ctx.accounts.marginfi_sol_vault.to_account_info(),
            ctx.accounts.token_program.to_account_info(),
            sol_to_repay_lamports,
            signer_seeds,
        )?;

        position.borrowed_sol = position.borrowed_sol.saturating_sub(sol_to_repay_lamports);
        position.kamino_shares = position.kamino_shares.saturating_sub(sol_to_repay_lamports);
    } else {
        // UNDER-LEVERAGED: SOL depreciated -> LTV decreased -> Safe to borrow more SOL and deposit to Kamino
        let deficit_ltv_bps = (target_ltv - current_ltv_bps) as u128;
        let additional_borrow_lamports = deficit_ltv_bps
            .checked_mul(position.principal_usdc as u128)
            .ok_or(RouterError::MathOverflow)?
            .checked_mul(1_000_000_000)
            .ok_or(RouterError::MathOverflow)?
            .checked_div(sol_price_usd_6d as u128 * 10000)
            .ok_or(RouterError::MathOverflow)? as u64;

        MarginfiCPI::borrow(
            ctx.accounts.marginfi_program.to_account_info(),
            ctx.accounts.marginfi_group.to_account_info(),
            ctx.accounts.marginfi_account.to_account_info(),
            ctx.accounts.vault_authority.to_account_info(),
            ctx.accounts.marginfi_sol_bank.to_account_info(),
            ctx.accounts.vault_sol_ata.to_account_info(),
            ctx.accounts.marginfi_sol_vault.to_account_info(),
            ctx.accounts.token_program.to_account_info(),
            additional_borrow_lamports,
            signer_seeds,
        )?;

        KaminoCPI::deposit_reserve_liquidity(
            ctx.accounts.kamino_program.to_account_info(),
            ctx.accounts.vault_authority.to_account_info(),
            ctx.accounts.marginfi_account.to_account_info(),
            ctx.accounts.kamino_lending_market.to_account_info(),
            ctx.accounts.kamino_lending_market_authority.to_account_info(),
            ctx.accounts.kamino_sol_reserve.to_account_info(),
            ctx.accounts.kamino_sol_supply.to_account_info(),
            ctx.accounts.kamino_ktoken_mint.to_account_info(),
            ctx.accounts.vault_sol_ata.to_account_info(),
            ctx.accounts.vault_kamino_ktoken_ata.to_account_info(),
            ctx.accounts.token_program.to_account_info(),
            additional_borrow_lamports,
            signer_seeds,
        )?;

        position.borrowed_sol = position.borrowed_sol.checked_add(additional_borrow_lamports).ok_or(RouterError::MathOverflow)?;
        position.kamino_shares = position.kamino_shares.checked_add(additional_borrow_lamports).ok_or(RouterError::MathOverflow)?;
    }

    position.last_rebalance_slot = ctx.accounts.clock.slot;
    position.reentrancy_lock = false;

    Ok(())
}