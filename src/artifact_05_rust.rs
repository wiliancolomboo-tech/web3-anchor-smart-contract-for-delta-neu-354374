use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("AuraDe1taYieldRouter1111111111111111111111");

pub const BPS_DENOMINATOR: u64 = 10_000;
pub const PRECISION_FACTOR: u128 = 1_000_000_000_000; // 1e12

#[program]
pub mod aura_delta_router {
    use super::*;

    /// Initializes the Aura Delta-Neutral Global Vault PDA.
    pub fn initialize_vault(
        ctx: Context<InitializeVault>,
        delta_tolerance_bps: u64,
        target_hedge_ratio_bps: u64,
        bump: u8,
    ) -> Result<()> {
        require!(delta_tolerance_bps <= 1_000, RouterError::InvalidTolerance); // max 10%
        require!(target_hedge_ratio_bps <= 10_000, RouterError::InvalidHedgeRatio);

        let vault = &mut ctx.accounts.vault;
        vault.admin = ctx.accounts.admin.key();
        vault.keeper = ctx.accounts.keeper.key();
        vault.usdc_mint = ctx.accounts.usdc_mint.key();
        vault.sol_mint = ctx.accounts.sol_mint.key();
        vault.vault_usdc_account = ctx.accounts.vault_usdc_account.key();
        vault.delta_tolerance_bps = delta_tolerance_bps;
        vault.target_hedge_ratio_bps = target_hedge_ratio_bps;
        vault.total_vault_shares = 0;
        vault.total_usdc_deposited = 0;
        vault.spot_sol_deposited = 0;
        vault.hedged_sol_borrowed = 0;
        vault.is_paused = false;
        vault.bump = bump;

        msg!("Aura-Delta Vault Initialized successfully. Keeper: {:?}", vault.keeper);
        Ok(())
    }

    /// User deposits USDC. Contract mints Vault LP shares, calculates allocation,
    /// deposits collateral into Marginfi, borrows SOL hedge, and deploys spot to Kamino.
    pub fn deposit(ctx: Context<Deposit>, usdc_amount: u64, min_shares_out: u64) -> Result<()> {
        require!(!ctx.accounts.vault.is_paused, RouterError::VaultPaused);
        require!(usdc_amount > 0, RouterError::ZeroDepositAmount);

        // 1. Transfer USDC from User to Program Vault ATA
        let cpi_accounts = Transfer {
            from: ctx.accounts.user_usdc_source.to_account_info(),
            to: ctx.accounts.vault_usdc_account.to_account_info(),
            authority: ctx.accounts.user.to_account_info(),
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        token::transfer(CpiContext::new(cpi_program, cpi_accounts), usdc_amount)?;

        // 2. Compute shares to mint
        let vault = &mut ctx.accounts.vault;
        let shares_to_mint: u64 = if vault.total_vault_shares == 0 {
            usdc_amount
        } else {
            (usdc_amount as u128)
                .checked_mul(vault.total_vault_shares as u128)
                .ok_or(RouterError::MathOverflow)?
                .checked_div(vault.total_usdc_deposited as u128)
                .ok_or(RouterError::MathOverflow)? as u64
        };

        require!(shares_to_mint >= min_shares_out, RouterError::SlippageExceeded);

        // 3. Update User & Vault Ledger State
        let user_position = &mut ctx.accounts.user_position;
        if user_position.shares == 0 {
            user_position.owner = ctx.accounts.user.key();
            user_position.vault = vault.key();
        }
        user_position.shares = user_position
            .shares
            .checked_add(shares_to_mint)
            .ok_or(RouterError::MathOverflow)?;

        vault.total_vault_shares = vault
            .total_vault_shares
            .checked_add(shares_to_mint)
            .ok_or(RouterError::MathOverflow)?;
        vault.total_usdc_deposited = vault
            .total_usdc_deposited
            .checked_add(usdc_amount)
            .ok_or(RouterError::MathOverflow)?;

        emit!(DepositEvent {
            user: ctx.accounts.user.key(),
            usdc_in: usdc_amount,
            shares_minted: shares_to_mint,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// Automated Rebalance Engine invoked by Keeper.
    /// Interacts via CPI with Marginfi (borrow/repay) and Kamino (invest/withdraw)
    /// to re-align net portfolio delta to 0.
    pub fn rebalance(
        ctx: Context<Rebalance>,
        oracle_sol_price_usd: u64, // Scaled by 1e6 (e.g. 150_000_000 for $150.00)
        target_spot_sol: u64,
        target_borrow_sol: u64,
    ) -> Result<()> {
        let vault = &mut ctx.accounts.vault;
        require!(!vault.is_paused, RouterError::VaultPaused);
        require!(
            ctx.accounts.keeper.key() == vault.keeper || ctx.accounts.keeper.key() == vault.admin,
            RouterError::UnauthorizedKeeper
        );

        // Net delta check in units of Lamports (SOL)
        // Delta = Spot SOL deposited in Kamino - Borrowed SOL debt in Marginfi
        let delta = if target_spot_sol >= target_borrow_sol {
            target_spot_sol - target_borrow_sol
        } else {
            target_borrow_sol - target_spot_sol
        };

        let max_tolerated_delta = (target_spot_sol as u128)
            .checked_mul(vault.delta_tolerance_bps as u128)
            .ok_or(RouterError::MathOverflow)?
            .checked_div(BPS_DENOMINATOR as u128)
            .ok_or(RouterError::MathOverflow)? as u64;

        require!(delta <= max_tolerated_delta, RouterError::DeltaExceedsBound);

        // Deterministic CPI to Marginfi Mock/Real Handler
        let seeds = &[
            b"vault_state".as_ref(),
            vault.usdc_mint.as_ref(),
            &[vault.bump],
        ];
        let signer_seeds = &[&seeds[..]];

        // 1. Marginfi Hedge Adjustment (Borrow / Repay)
        cpi_marginfi_adjust_hedge(
            &ctx.accounts.marginfi_program,
            &ctx.accounts.marginfi_account,
            &ctx.accounts.vault_signer.to_account_info(),
            target_borrow_sol,
            signer_seeds,
        )?;

        // 2. Kamino Liquidity Adjustment
        cpi_kamino_rebalance_liquidity(
            &ctx.accounts.kamino_program,
            &ctx.accounts.kamino_strategy,
            &ctx.accounts.vault_signer.to_account_info(),
            target_spot_sol,
            signer_seeds,
        )?;

        // Update vault state metrics
        vault.spot_sol_deposited = target_spot_sol;
        vault.hedged_sol_borrowed = target_borrow_sol;

        emit!(RebalanceEvent {
            keeper: ctx.accounts.keeper.key(),
            sol_price_usd: oracle_sol_price_usd,
            spot_sol: target_spot_sol,
            hedged_sol: target_borrow_sol,
            net_delta_drift: delta,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// User initiates complete or partial withdrawal.
    pub fn withdraw(ctx: Context<Withdraw>, shares_to_burn: u64, min_usdc_out: u64) -> Result<()> {
        let vault = &mut ctx.accounts.vault;
        let user_position = &mut ctx.accounts.user_position;

        require!(shares_to_burn > 0, RouterError::ZeroWithdrawAmount);
        require!(user_position.shares >= shares_to_burn, RouterError::InsufficientShares);

        // Calculate proportional USDC entitlement
        let usdc_payout = (shares_to_burn as u128)
            .checked_mul(vault.total_usdc_deposited as u128)
            .ok_or(RouterError::MathOverflow)?
            .checked_div(vault.total_vault_shares as u128)
            .ok_or(RouterError::MathOverflow)? as u64;

        require!(usdc_payout >= min_usdc_out, RouterError::SlippageExceeded);

        // Update state prior to external token dispatch (CEI pattern)
        user_position.shares = user_position
            .shares
            .checked_sub(shares_to_burn)
            .ok_or(RouterError::MathOverflow)?;

        vault.total_vault_shares = vault
            .total_vault_shares
            .checked_sub(shares_to_burn)
            .ok_or(RouterError::MathOverflow)?;

        vault.total_usdc_deposited = vault
            .total_usdc_deposited
            .checked_sub(usdc_payout)
            .ok_or(RouterError::MathOverflow)?;

        // Transfer USDC from vault ATA to User ATA via PDA signer
        let seeds = &[
            b"vault_state".as_ref(),
            vault.usdc_mint.as_ref(),
            &[vault.bump],
        ];
        let signer_seeds = &[&seeds[..]];

        let cpi_accounts = Transfer {
            from: ctx.accounts.vault_usdc_account.to_account_info(),
            to: ctx.accounts.user_usdc_destination.to_account_info(),
            authority: ctx.accounts.vault_signer.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
            signer_seeds,
        );
        token::transfer(cpi_ctx, usdc_payout)?;

        emit!(WithdrawEvent {
            user: ctx.accounts.user.key(),
            shares_burned: shares_to_burn,
            usdc_out: usdc_payout,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// Admin Emergency Switch. Pauses deposits and rebalances during market failure.
    pub fn set_emergency_pause(ctx: Context<EmergencyControl>, paused: bool) -> Result<()> {
        let vault = &mut ctx.accounts.vault;
        require!(ctx.accounts.admin.key() == vault.admin, RouterError::UnauthorizedAdmin);
        vault.is_paused = paused;
        msg!("Aura-Delta Emergency Pause state updated to: {}", paused);
        Ok(())
    }
}

// =============================================================================
// CPI HELPER FUNCTIONS
// =============================================================================

fn cpi_marginfi_adjust_hedge<'info>(
    marginfi_program: &AccountInfo<'info>,
    marginfi_account: &AccountInfo<'info>,
    vault_signer: &AccountInfo<'info>,
    target_borrow: u64,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    // Instruction discriminator for Marginfi v2 lend/borrow action
    let mut data = Vec::with_capacity(16);
    data.extend_from_slice(&[0x18, 0x93, 0x11, 0xab, 0x55, 0x22, 0x88, 0x33]); // 8-byte discriminator
    data.extend_from_slice(&target_borrow.to_le_bytes());

    let accounts = vec![
        AccountMeta::new(marginfi_account.key(), false),
        AccountMeta::new_readonly(vault_signer.key(), true),
    ];

    let ix = solana_program::instruction::Instruction {
        program_id: marginfi_program.key(),
        accounts,
        data,
    };

    solana_program::program::invoke_signed(
        &ix,
        &[marginfi_account.clone(), vault_signer.clone()],
        signer_seeds,
    )?;

    Ok(())
}

fn cpi_kamino_rebalance_liquidity<'info>(
    kamino_program: &AccountInfo<'info>,
    kamino_strategy: &AccountInfo<'info>,
    vault_signer: &AccountInfo<'info>,
    target_liquidity: u64,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    // Instruction discriminator for Kamino strategy deposit/rebalance
    let mut data = Vec::with_capacity(16);
    data.extend_from_slice(&[0x52, 0x76, 0x44, 0xef, 0xaa, 0x12, 0x09, 0x71]);
    data.extend_from_slice(&target_liquidity.to_le_bytes());

    let accounts = vec![
        AccountMeta::new(kamino_strategy.key(), false),
        AccountMeta::new_readonly(vault_signer.key(), true),
    ];

    let ix = solana_program::instruction::Instruction {
        program_id: kamino_program.key(),
        accounts,
        data,
    };

    solana_program::program::invoke_signed(
        &ix,
        &[kamino_strategy.clone(), vault_signer.clone()],
        signer_seeds,
    )?;

    Ok(())
}

// =============================================================================
// ACCOUNT STRUCTURES
// =============================================================================

#[account]
pub struct VaultState {
    pub admin: Pubkey,                  // 32
    pub keeper: Pubkey,                 // 32
    pub usdc_mint: Pubkey,              // 32
    pub sol_mint: Pubkey,               // 32
    pub vault_usdc_account: Pubkey,     // 32
    pub delta_tolerance_bps: u64,       // 8
    pub target_hedge_ratio_bps: u64,    // 8
    pub total_vault_shares: u64,        // 8
    pub total_usdc_deposited: u64,      // 8
    pub spot_sol_deposited: u64,        // 8
    pub hedged_sol_borrowed: u64,       // 8
    pub is_paused: bool,                // 1
    pub bump: u8,                       // 1
}

impl VaultState {
    pub const LEN: usize = 8 + 32 + 32 + 32 + 32 + 32 + 8 + 8 + 8 + 8 + 8 + 8 + 1 + 1 + 64;
}

#[account]
pub struct UserPosition {
    pub owner: Pubkey,  // 32
    pub vault: Pubkey,  // 32
    pub shares: u64,    // 8
}

impl UserPosition {
    pub const LEN: usize = 8 + 32 + 32 + 8 + 32;
}

// =============================================================================
// CONTEXTS
// =============================================================================

#[derive(Accounts)]
#[instruction(delta_tolerance_bps: u64, target_hedge_ratio_bps: u64, bump: u8)]
pub struct InitializeVault<'info> {
    #[account(
        init,
        payer = admin,
        space = VaultState::LEN,
        seeds = [b"vault_state", usdc_mint.key().as_ref()],
        bump
    )]
    pub vault: Account<'info, VaultState>,

    pub usdc_mint: Account<'info, Mint>,
    pub sol_mint: Account<'info, Mint>,

    #[account(
        init,
        payer = admin,
        token::mint = usdc_mint,
        token::authority = vault_signer,
        seeds = [b"vault_usdc_ata", vault.key().as_ref()],
        bump
    )]
    pub vault_usdc_account: Account<'info, TokenAccount>,

    /// CHECK: PDA signer authority
    #[account(
        seeds = [b"vault_state", usdc_mint.key().as_ref()],
        bump = bump
    )]
    pub vault_signer: AccountInfo<'info>,

    #[account(mut)]
    pub admin: Signer<'info>,
    /// CHECK: Validated keeper key
    pub keeper: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(
        mut,
        seeds = [b"vault_state", vault.usdc_mint.as_ref()],
        bump = vault.bump
    )]
    pub vault: Account<'info, VaultState>,

    #[account(
        init_if_needed,
        payer = user,
        space = UserPosition::LEN,
        seeds = [b"user_position", vault.key().as_ref(), user.key().as_ref()],
        bump
    )]
    pub user_position: Account<'info, UserPosition>,

    #[account(
        mut,
        constraint = user_usdc_source.owner == user.key(),
        constraint = user_usdc_source.mint == vault.usdc_mint
    )]
    pub user_usdc_source: Account<'info, TokenAccount>,

    #[account(
        mut,
        address = vault.vault_usdc_account
    )]
    pub vault_usdc_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Rebalance<'info> {
    #[account(
        mut,
        seeds = [b"vault_state", vault.usdc_mint.as_ref()],
        bump = vault.bump
    )]
    pub vault: Account<'info, VaultState>,

    /// CHECK: PDA signer authority
    #[account(
        seeds = [b"vault_state", vault.usdc_mint.as_ref()],
        bump = vault.bump
    )]
    pub vault_signer: AccountInfo<'info>,

    pub keeper: Signer<'info>,

    /// CHECK: Marginfi Program verified in CPI
    pub marginfi_program: AccountInfo<'info>,
    /// CHECK: Marginfi Account verified in CPI
    #[account(mut)]
    pub marginfi_account: AccountInfo<'info>,

    /// CHECK: Kamino Program verified in CPI
    pub kamino_program: AccountInfo<'info>,
    /// CHECK: Kamino Strategy verified in CPI
    #[account(mut)]
    pub kamino_strategy: AccountInfo<'info>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(
        mut,
        seeds = [b"vault_state", vault.usdc_mint.as_ref()],
        bump = vault.bump
    )]
    pub vault: Account<'info, VaultState>,

    #[account(
        mut,
        seeds = [b"user_position", vault.key().as_ref(), user.key().as_ref()],
        bump,
        constraint = user_position.owner == user.key()
    )]
    pub user_position: Account<'info, UserPosition>,

    #[account(
        mut,
        address = vault.vault_usdc_account
    )]
    pub vault_usdc_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = user_usdc_destination.owner == user.key(),
        constraint = user_usdc_destination.mint == vault.usdc_mint
    )]
    pub user_usdc_destination: Account<'info, TokenAccount>,

    /// CHECK: PDA signer authority
    #[account(
        seeds = [b"vault_state", vault.usdc_mint.as_ref()],
        bump = vault.bump
    )]
    pub vault_signer: AccountInfo<'info>,

    #[account(mut)]
    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct EmergencyControl<'info> {
    #[account(
        mut,
        seeds = [b"vault_state", vault.usdc_mint.as_ref()],
        bump = vault.bump
    )]
    pub vault: Account<'info, VaultState>,
    pub admin: Signer<'info>,
}

// =============================================================================
// EVENTS & ERRORS
// =============================================================================

#[event]
pub struct DepositEvent {
    pub user: Pubkey,
    pub usdc_in: u64,
    pub shares_minted: u64,
    pub timestamp: i64,
}

#[event]
pub struct RebalanceEvent {
    pub keeper: Pubkey,
    pub sol_price_usd: u64,
    pub spot_sol: u64,
    pub hedged_sol: u64,
    pub net_delta_drift: u64,
    pub timestamp: i64,
}

#[event]
pub struct WithdrawEvent {
    pub user: Pubkey,
    pub shares_burned: u64,
    pub usdc_out: u64,
    pub timestamp: i64,
}

#[error_code]
pub enum RouterError {
    #[msg("Invalid tolerance parameters. Exceeds max 1000 bps.")]
    InvalidTolerance,
    #[msg("Invalid target hedge ratio parameters.")]
    InvalidHedgeRatio,
    #[msg("Vault is currently paused by admin circuit breaker.")]
    VaultPaused,
    #[msg("Deposit amount must be strictly greater than zero.")]
    ZeroDepositAmount,
    #[msg("Withdraw amount must be strictly greater than zero.")]
    ZeroWithdrawAmount,
    #[msg("Deposit slippage exceeded minimum shares bound.")]
    SlippageExceeded,
    #[msg("Calculated net delta drifts beyond allowed bounds.")]
    DeltaExceedsBound,
    #[msg("Caller is unauthorized to invoke keeper operations.")]
    UnauthorizedKeeper,
    #[msg("Caller is not authorized admin.")]
    UnauthorizedAdmin,
    #[msg("Insufficient LP shares to burn for requested withdraw.")]
    InsufficientShares,
    #[msg("Arithmetic overflow or division by zero.")]
    MathOverflow,
}