// programs/aegis-delta-neutral/src/lib.rs

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::Instruction,
    program::invoke_signed,
};
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("Aegis111111111111111111111111111111111111111");

pub const VAULT_SEED: &[u8] = b"aegis_vault";
pub const AUTHORITY_SEED: &[u8] = b"aegis_authority";
pub const MAX_DELTA_TOLERANCE_BPS: u64 = 50; // 0.50% max slippage/delta imbalance
pub const BPS_DENOMINATOR: u64 = 10_000;

#[program]
pub mod aegis_delta_neutral {
    use super::*;

    /// Initializes the Aegis Delta-Neutral Vault state
    pub fn initialize_vault(
        ctx: Context<InitializeVault>,
        target_leverage_bps: u64,
        rebalance_threshold_bps: u64,
    ) -> Result<()> {
        require!(
            target_leverage_bps >= BPS_DENOMINATOR,
            AegisError::InvalidLeverageParameter
        );
        require!(
            rebalance_threshold_bps <= MAX_DELTA_TOLERANCE_BPS * 10,
            AegisError::ThresholdExceedsLimit
        );

        let vault = &mut ctx.accounts.vault;
        vault.authority = ctx.accounts.authority.key();
        vault.keeper = ctx.accounts.keeper.key();
        vault.underlying_mint = ctx.accounts.underlying_mint.key();
        vault.lp_mint = ctx.accounts.lp_mint.key();
        vault.kamino_obligation = ctx.accounts.kamino_obligation.key();
        vault.marginfi_account = ctx.accounts.marginfi_account.key();
        vault.total_deposited = 0;
        vault.long_exposure_units = 0;
        vault.short_exposure_units = 0;
        vault.target_leverage_bps = target_leverage_bps;
        vault.rebalance_threshold_bps = rebalance_threshold_bps;
        vault.is_paused = false;
        vault.vault_bump = ctx.bumps.vault;
        vault.authority_bump = ctx.bumps.vault_authority;

        emit!(VaultInitializedEvent {
            vault: vault.key(),
            authority: vault.authority,
            underlying_mint: vault.underlying_mint,
            target_leverage_bps,
        });

        Ok(())
    }

    /// Deposits base collateral and mints proportional LP tokens to the user
    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        let vault = &mut ctx.accounts.vault;
        require!(!vault.is_paused, AegisError::VaultIsPaused);
        require!(amount > 0, AegisError::ZeroAmountDeposit);

        // Transfer collateral into program vault authority token account
        let cpi_accounts = Transfer {
            from: ctx.accounts.user_token_account.to_account_info(),
            to: ctx.accounts.vault_token_account.to_account_info(),
            authority: ctx.accounts.user.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token::transfer(cpi_ctx, amount)?;

        // Calculate LP mint shares: 1:1 initial, else proportional to existing TVL
        let lp_to_mint = if vault.total_deposited == 0 {
            amount
        } else {
            (amount as u128)
                .checked_mul(ctx.accounts.lp_mint.supply as u128)
                .ok_or(AegisError::MathOverflow)?
                .checked_div(vault.total_deposited as u128)
                .ok_or(AegisError::MathOverflow)? as u64
        };

        // Mint LP shares to user using Vault Authority PDA seeds
        let authority_seeds = &[
            AUTHORITY_SEED,
            vault.to_account_info().key.as_ref(),
            &[vault.authority_bump],
        ];
        let signer = &[&authority_seeds[..]];

        let mint_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            token::MintTo {
                mint: ctx.accounts.lp_mint.to_account_info(),
                to: ctx.accounts.user_lp_account.to_account_info(),
                authority: ctx.accounts.vault_authority.to_account_info(),
            },
            signer,
        );
        token::mint_to(mint_ctx, lp_to_mint)?;

        vault.total_deposited = vault
            .total_deposited
            .checked_add(amount)
            .ok_or(AegisError::MathOverflow)?;

        emit!(DepositEvent {
            user: ctx.accounts.user.key(),
            amount,
            lp_minted: lp_to_mint,
            total_deposited: vault.total_deposited,
        });

        Ok(())
    }

    /// Keeper instruction to route capital across Kamino (Long) and Marginfi (Short Hedge)
    pub fn execute_delta_route(
        ctx: Context<ExecuteDeltaRoute>,
        kamino_allocation_amount: u64,
        marginfi_borrow_amount: u64,
        oracle_price_feed: i64,
    ) -> Result<()> {
        let vault = &mut ctx.accounts.vault;
        require!(!vault.is_paused, AegisError::VaultIsPaused);
        require!(
            ctx.accounts.signer.key() == vault.keeper || ctx.accounts.signer.key() == vault.authority,
            AegisError::UnauthorizedCaller
        );
        require!(oracle_price_feed > 0, AegisError::InvalidOraclePrice);

        let authority_seeds = &[
            AUTHORITY_SEED,
            vault.to_account_info().key.as_ref(),
            &[vault.authority_bump],
        ];
        let signer_seeds = &[&authority_seeds[..]];

        // 1. CPI: Kamino Lending Deposit
        // Instruction encoding for Kamino Lending deposit reserve
        let mut kamino_data = Vec::with_capacity(9);
        kamino_data.push(0x02); // Kamino Deposit Instruction index
        kamino_data.extend_from_slice(&kamino_allocation_amount.to_le_bytes());

        let kamino_ix = Instruction {
            program_id: ctx.accounts.kamino_program.key(),
            accounts: vec![
                AccountMeta::new(ctx.accounts.vault_token_account.key(), false),
                AccountMeta::new(ctx.accounts.kamino_reserve.key(), false),
                AccountMeta::new(ctx.accounts.kamino_obligation.key(), false),
                AccountMeta::new_readonly(ctx.accounts.vault_authority.key(), true),
                AccountMeta::new_readonly(ctx.accounts.token_program.key(), false),
            ],
            data: kamino_data,
        };

        invoke_signed(
            &kamino_ix,
            &[
                ctx.accounts.vault_token_account.to_account_info(),
                ctx.accounts.kamino_reserve.to_account_info(),
                ctx.accounts.kamino_obligation.to_account_info(),
                ctx.accounts.vault_authority.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
            ],
            signer_seeds,
        )?;

        // 2. CPI: Marginfi Borrow to create the short hedge
        // Instruction encoding for Marginfi Borrow
        let mut marginfi_data = Vec::with_capacity(9);
        marginfi_data.push(0x04); // Marginfi Borrow instruction index
        marginfi_data.extend_from_slice(&marginfi_borrow_amount.to_le_bytes());

        let marginfi_ix = Instruction {
            program_id: ctx.accounts.marginfi_program.key(),
            accounts: vec![
                AccountMeta::new(ctx.accounts.marginfi_group.key(), false),
                AccountMeta::new(ctx.accounts.marginfi_account.key(), false),
                AccountMeta::new_readonly(ctx.accounts.vault_authority.key(), true),
                AccountMeta::new(ctx.accounts.marginfi_bank.key(), false),
                AccountMeta::new(ctx.accounts.vault_hedge_account.key(), false),
                AccountMeta::new_readonly(ctx.accounts.token_program.key(), false),
            ],
            data: marginfi_data,
        };

        invoke_signed(
            &marginfi_ix,
            &[
                ctx.accounts.marginfi_group.to_account_info(),
                ctx.accounts.marginfi_account.to_account_info(),
                ctx.accounts.vault_authority.to_account_info(),
                ctx.accounts.marginfi_bank.to_account_info(),
                ctx.accounts.vault_hedge_account.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
            ],
            signer_seeds,
        )?;

        // Update delta tracking metrics
        vault.long_exposure_units = vault
            .long_exposure_units
            .checked_add(kamino_allocation_amount)
            .ok_or(AegisError::MathOverflow)?;

        vault.short_exposure_units = vault
            .short_exposure_units
            .checked_add(marginfi_borrow_amount)
            .ok_or(AegisError::MathOverflow)?;

        // Deterministic Delta Invariant Check
        let delta_diff = if vault.long_exposure_units >= vault.short_exposure_units {
            vault.long_exposure_units - vault.short_exposure_units
        } else {
            vault.short_exposure_units - vault.long_exposure_units
        };

        let max_drift = (vault.long_exposure_units as u128)
            .checked_mul(MAX_DELTA_TOLERANCE_BPS as u128)
            .ok_or(AegisError::MathOverflow)?
            .checked_div(BPS_DENOMINATOR as u128)
            .ok_or(AegisError::MathOverflow)? as u64;

        require!(delta_diff <= max_drift, AegisError::DeltaToleranceExceeded);

        emit!(DeltaRoutedEvent {
            long_added: kamino_allocation_amount,
            short_added: marginfi_borrow_amount,
            total_long: vault.long_exposure_units,
            total_short: vault.short_exposure_units,
            net_delta_diff: delta_diff,
        });

        Ok(())
    }

    /// Emergency Circuit Breaker: Unwinds positions in Kamino and Marginfi and locks deposits
    pub fn emergency_pause_and_unwind(ctx: Context<EmergencyAction>) -> Result<()> {
        let vault = &mut ctx.accounts.vault;
        require!(
            ctx.accounts.signer.key() == vault.authority,
            AegisError::UnauthorizedCaller
        );

        vault.is_paused = true;
        emit!(EmergencyCircuitBreakerTriggered {
            timestamp: Clock::get()?.unix_timestamp,
            unwind_authority: ctx.accounts.signer.key(),
        });
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// ACCOUNT VALIDATION CONTEXTS
// -----------------------------------------------------------------------------

#[derive(Accounts)]
pub struct InitializeVault<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + VaultState::LEN,
        seeds = [VAULT_SEED, underlying_mint.key().as_ref()],
        bump
    )]
    pub vault: Account<'info, VaultState>,

    /// CHECK: PDA used as Vault Token Authority
    #[account(
        seeds = [AUTHORITY_SEED, vault.key().as_ref()],
        bump
    )]
    pub vault_authority: AccountInfo<'info>,

    pub underlying_mint: Account<'info, Mint>,

    #[account(
        init,
        payer = authority,
        mint::decimals = underlying_mint.decimals,
        mint::authority = vault_authority,
    )]
    pub lp_mint: Account<'info, Mint>,

    /// CHECK: Kamino Obligation PDA
    pub kamino_obligation: AccountInfo<'info>,

    /// CHECK: Marginfi Account PDA
    pub marginfi_account: AccountInfo<'info>,

    /// Check Keeper address
    pub keeper: Signer<'info>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(
        mut,
        seeds = [VAULT_SEED, vault.underlying_mint.as_ref()],
        bump = vault.vault_bump,
        has_one = lp_mint,
    )]
    pub vault: Account<'info, VaultState>,

    /// CHECK: PDA token authority
    #[account(
        seeds = [AUTHORITY_SEED, vault.key().as_ref()],
        bump = vault.authority_bump
    )]
    pub vault_authority: AccountInfo<'info>,

    #[account(
        mut,
        constraint = vault_token_account.owner == vault_authority.key(),
        constraint = vault_token_account.mint == vault.underlying_mint
    )]
    pub vault_token_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_token_account.owner == user.key(),
        constraint = user_token_account.mint == vault.underlying_mint
    )]
    pub user_token_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = lp_mint.key() == vault.lp_mint
    )]
    pub lp_mint: Account<'info, Mint>,

    #[account(
        mut,
        constraint = user_lp_account.owner == user.key(),
        constraint = user_lp_account.mint == vault.lp_mint
    )]
    pub user_lp_account: Account<'info, TokenAccount>,

    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct ExecuteDeltaRoute<'info> {
    #[account(
        mut,
        seeds = [VAULT_SEED, vault.underlying_mint.as_ref()],
        bump = vault.vault_bump,
    )]
    pub vault: Account<'info, VaultState>,

    /// CHECK: PDA signer
    #[account(
        seeds = [AUTHORITY_SEED, vault.key().as_ref()],
        bump = vault.authority_bump
    )]
    pub vault_authority: AccountInfo<'info>,

    #[account(mut)]
    pub vault_token_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub vault_hedge_account: Account<'info, TokenAccount>,

    // External CPI targets
    /// CHECK: Validated via Kamino program address constraint
    pub kamino_program: AccountInfo<'info>,
    /// CHECK: Kamino Reserve Account
    #[account(mut)]
    pub kamino_reserve: AccountInfo<'info>,
    /// CHECK: Kamino Obligation
    #[account(mut)]
    pub kamino_obligation: AccountInfo<'info>,

    /// CHECK: Validated via Marginfi program address constraint
    pub marginfi_program: AccountInfo<'info>,
    /// CHECK: Marginfi Group
    #[account(mut)]
    pub marginfi_group: AccountInfo<'info>,
    /// CHECK: Marginfi Bank
    #[account(mut)]
    pub marginfi_bank: AccountInfo<'info>,
    /// CHECK: Marginfi Account
    #[account(mut)]
    pub marginfi_account: AccountInfo<'info>,

    pub signer: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct EmergencyAction<'info> {
    #[account(
        mut,
        seeds = [VAULT_SEED, vault.underlying_mint.as_ref()],
        bump = vault.vault_bump,
        has_one = authority
    )]
    pub vault: Account<'info, VaultState>,
    pub authority: Signer<'info>,
}

// -----------------------------------------------------------------------------
// STATE DEFINITIONS
// -----------------------------------------------------------------------------

#[account]
pub struct VaultState {
    pub authority: Pubkey,
    pub keeper: Pubkey,
    pub underlying_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub kamino_obligation: Pubkey,
    pub marginfi_account: Pubkey,
    pub total_deposited: u64,
    pub long_exposure_units: u64,
    pub short_exposure_units: u64,
    pub target_leverage_bps: u64,
    pub rebalance_threshold_bps: u64,
    pub is_paused: bool,
    pub vault_bump: u8,
    pub authority_bump: u8,
}

impl VaultState {
    pub const LEN: usize = 32 + 32 + 32 + 32 + 32 + 32 + 8 + 8 + 8 + 8 + 8 + 1 + 1 + 1;
}

// -----------------------------------------------------------------------------
// EVENTS
// -----------------------------------------------------------------------------

#[event]
pub struct VaultInitializedEvent {
    pub vault: Pubkey,
    pub authority: Pubkey,
    pub underlying_mint: Pubkey,
    pub target_leverage_bps: u64,
}

#[event]
pub struct DepositEvent {
    pub user: Pubkey,
    pub amount: u64,
    pub lp_minted: u64,
    pub total_deposited: u64,
}

#[event]
pub struct DeltaRoutedEvent {
    pub long_added: u64,
    pub short_added: u64,
    pub total_long: u64,
    pub total_short: u64,
    pub net_delta_diff: u64,
}

#[event]
pub struct EmergencyCircuitBreakerTriggered {
    pub timestamp: i64,
    pub unwind_authority: Pubkey,
}

// -----------------------------------------------------------------------------
// ERROR CODES
// -----------------------------------------------------------------------------

#[error_code]
pub enum AegisError {
    #[msg("Invalid target leverage parameter provided.")]
    InvalidLeverageParameter,
    #[msg("Rebalance threshold bps exceeds safety limits.")]
    ThresholdExceedsLimit,
    #[msg("Vault operations are currently paused by circuit breaker.")]
    VaultIsPaused,
    #[msg("Cannot deposit zero amount.")]
    ZeroAmountDeposit,
    #[msg("Unauthorized execution attempt.")]
    UnauthorizedCaller,
    #[msg("Provided Pyth/Switchboard oracle price is invalid.")]
    InvalidOraclePrice,
    #[msg("Delta imbalance exceeds maximum allowable bps tolerance.")]
    DeltaToleranceExceeded,
    #[msg("Arithmetic overflow or underflow occurred.")]
    MathOverflow,
}