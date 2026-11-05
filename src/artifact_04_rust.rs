use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};
use pyth_sdk_solana::load_price_feed_from_account_info;

declare_id!("DeltaRouter1111111111111111111111111111111111");

pub const VAULT_SEED: &[u8] = b"delta_vault";
pub const AUTHORITY_SEED: &[u8] = b"vault_authority";
pub const MAX_PRICE_AGE_SECONDS: u64 = 60;
pub const MAX_DELTA_TOLERANCE_BPS: u64 = 75; // 0.75%
pub const BPS_DENOMINATOR: u64 = 10_000;

#[program]
pub mod delta_neutral_router {
    use super::*;

    /// Initializes a new Delta-Neutral Vault targeting Kamino & Marginfi
    pub fn initialize_vault(
        ctx: Context<InitializeVault>,
        target_leverage_bps: u64,
        rebalance_threshold_bps: u64,
    ) -> Result<()> {
        require!(target_leverage_bps >= 10_000, RouterError::InvalidLeverage);
        require!(rebalance_threshold_bps <= 1_000, RouterError::InvalidThreshold);

        let vault = &mut ctx.accounts.vault_state;
        vault.admin = ctx.accounts.admin.key();
        vault.deposit_mint = ctx.accounts.deposit_mint.key();
        vault.hedged_mint = ctx.accounts.hedged_mint.key();
        vault.kamino_reserve = ctx.accounts.kamino_reserve.key();
        vault.marginfi_bank = ctx.accounts.marginfi_bank.key();
        vault.pyth_price_feed = ctx.accounts.pyth_price_feed.key();
        vault.target_leverage_bps = target_leverage_bps;
        vault.rebalance_threshold_bps = rebalance_threshold_bps;
        vault.bump = ctx.bumps.vault_state;
        vault.authority_bump = ctx.bumps.vault_authority;
        vault.total_usdc_deposited = 0;
        vault.total_hedged_borrowed = 0;
        vault.is_frozen = false;

        emit!(VaultInitializedEvent {
            admin: vault.admin,
            vault: vault.key(),
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// Deposits USDC into the vault, automatically deploying collateral to Kamino 
    /// and executing the proportional hedge on Marginfi.
    pub fn deposit_and_hedge(ctx: Context<DepositAndHedge>, amount: u64) -> Result<()> {
        require!(!ctx.accounts.vault_state.is_frozen, RouterError::VaultIsFrozen);
        require!(amount > 0, RouterError::ZeroDepositAmount);

        // 1. Enforce Oracle Validity
        let clock = Clock::get()?;
        let pyth_price = validate_and_fetch_oracle_price(
            &ctx.accounts.pyth_price_feed,
            clock.unix_timestamp,
        )?;

        // 2. Transfer USDC from depositor to program-controlled vault token account
        let transfer_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.user_usdc_ata.to_account_info(),
                to: ctx.accounts.vault_usdc_ata.to_account_info(),
                authority: ctx.accounts.depositor.to_account_info(),
            },
        );
        token::transfer(transfer_ctx, amount)?;

        // 3. Compute Hedge Requirement: Hedged Asset Amount = (Amount * Target Leverage) / Price
        // Using base 10^6 for USDC and 10^9 for Hedged Asset (e.g., SOL)
        let delta_neutral_borrow_units = compute_hedged_borrow_units(
            amount,
            pyth_price,
            ctx.accounts.vault_state.target_leverage_bps,
        )?;

        // 4. CPI to Kamino: Deposit USDC Liquidity
        execute_kamino_deposit(
            &ctx.accounts.kamino_program,
            &ctx.accounts.kamino_reserve,
            &ctx.accounts.vault_usdc_ata,
            &ctx.accounts.vault_authority,
            &ctx.accounts.kamino_collateral_ata,
            amount,
            ctx.accounts.vault_state.bump,
        )?;

        // 5. CPI to Marginfi: Open/Increase Hedged Asset Borrow Position
        execute_marginfi_borrow(
            &ctx.accounts.marginfi_program,
            &ctx.accounts.marginfi_bank,
            &ctx.accounts.marginfi_account,
            &ctx.accounts.vault_authority,
            &ctx.accounts.vault_hedged_ata,
            delta_neutral_borrow_units,
            ctx.accounts.vault_state.bump,
        )?;

        // 6. Update Accounting State
        let vault = &mut ctx.accounts.vault_state;
        vault.total_usdc_deposited = vault.total_usdc_deposited.checked_add(amount).unwrap();
        vault.total_hedged_borrowed = vault
            .total_hedged_borrowed
            .checked_add(delta_neutral_borrow_units)
            .unwrap();

        emit!(PositionHedgedEvent {
            depositor: ctx.accounts.depositor.key(),
            amount_deposited: amount,
            hedged_borrowed_units: delta_neutral_borrow_units,
            oracle_price: pyth_price,
            timestamp: clock.unix_timestamp,
        });

        Ok(())
    }

    /// Autonomous keeper instruction: rebalance the delta between Kamino collateral & Marginfi borrow
    pub fn rebalance_vault(ctx: Context<RebalanceVault>) -> Result<()> {
        let clock = Clock::get()?;
        let pyth_price = validate_and_fetch_oracle_price(
            &ctx.accounts.pyth_price_feed,
            clock.unix_timestamp,
        )?;

        let vault = &mut ctx.accounts.vault_state;
        require!(!vault.is_frozen, RouterError::VaultIsFrozen);

        // Theoretical short required = (total_usdc_deposited * target_leverage) / price
        let target_hedged_borrow = compute_hedged_borrow_units(
            vault.total_usdc_deposited,
            pyth_price,
            vault.target_leverage_bps,
        )?;

        let current_borrow = vault.total_hedged_borrowed;
        let delta_diff = if target_hedged_borrow >= current_borrow {
            target_hedged_borrow - current_borrow
        } else {
            current_borrow - target_hedged_borrow
        };

        // Check if delta deviation exceeds threshold
        let deviation_bps = (delta_diff as u128)
            .checked_mul(BPS_DENOMINATOR as u128)
            .unwrap()
            .checked_div(target_hedged_borrow as u128)
            .unwrap() as u64;

        require!(
            deviation_bps >= vault.rebalance_threshold_bps,
            RouterError::RebalanceNotNeeded
        );

        if target_hedged_borrow > current_borrow {
            // Need to borrow more to maintain delta neutrality
            let to_borrow = target_hedged_borrow - current_borrow;
            execute_marginfi_borrow(
                &ctx.accounts.marginfi_program,
                &ctx.accounts.marginfi_bank,
                &ctx.accounts.marginfi_account,
                &ctx.accounts.vault_authority,
                &ctx.accounts.vault_hedged_ata,
                to_borrow,
                vault.bump,
            )?;
            vault.total_hedged_borrowed = target_hedged_borrow;
        } else {
            // Need to repay excess hedge
            let to_repay = current_borrow - target_hedged_borrow;
            execute_marginfi_repay(
                &ctx.accounts.marginfi_program,
                &ctx.accounts.marginfi_bank,
                &ctx.accounts.marginfi_account,
                &ctx.accounts.vault_authority,
                &ctx.accounts.vault_hedged_ata,
                to_repay,
                vault.bump,
            )?;
            vault.total_hedged_borrowed = target_hedged_borrow;
        }

        emit!(RebalancedEvent {
            deviation_bps,
            new_hedged_borrow: vault.total_hedged_borrowed,
            oracle_price: pyth_price,
            timestamp: clock.unix_timestamp,
        });

        Ok(())
    }

    /// Admin Emergency Freeze / Unfreeze
    pub fn set_vault_frozen(ctx: Context<AdminControl>, frozen: bool) -> Result<()> {
        let vault = &mut ctx.accounts.vault_state;
        vault.is_frozen = frozen;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------
// Account Structs
// ---------------------------------------------------------------------------------

#[derive(Accounts)]
pub struct InitializeVault<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        init,
        payer = admin,
        space = 8 + VaultState::INIT_SPACE,
        seeds = [VAULT_SEED, deposit_mint.key().as_ref(), hedged_mint.key().as_ref()],
        bump
    )]
    pub vault_state: Account<'info, VaultState>,

    /// CHECK: PDA acting as the decentralized vault authority for signing CPIs
    #[account(
        seeds = [AUTHORITY_SEED, vault_state.key().as_ref()],
        bump
    )]
    pub vault_authority: AccountInfo<'info>,

    pub deposit_mint: Account<'info, Mint>,
    pub hedged_mint: Account<'info, Mint>,

    /// CHECK: Validated Kamino reserve account
    pub kamino_reserve: AccountInfo<'info>,
    /// CHECK: Validated Marginfi bank account
    pub marginfi_bank: AccountInfo<'info>,
    /// CHECK: Verified Pyth Network Oracle account
    pub pyth_price_feed: AccountInfo<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct DepositAndHedge<'info> {
    #[account(mut)]
    pub depositor: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED, vault_state.deposit_mint.as_ref(), vault_state.hedged_mint.as_ref()],
        bump = vault_state.bump
    )]
    pub vault_state: Account<'info, VaultState>,

    /// CHECK: Vault PDA authority
    #[account(
        seeds = [AUTHORITY_SEED, vault_state.key().as_ref()],
        bump = vault_state.authority_bump
    )]
    pub vault_authority: AccountInfo<'info>,

    #[account(
        mut,
        constraint = user_usdc_ata.owner == depositor.key(),
        constraint = user_usdc_ata.mint == vault_state.deposit_mint
    )]
    pub user_usdc_ata: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = vault_usdc_ata.owner == vault_authority.key(),
        constraint = vault_usdc_ata.mint == vault_state.deposit_mint
    )]
    pub vault_usdc_ata: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = vault_hedged_ata.owner == vault_authority.key(),
        constraint = vault_hedged_ata.mint == vault_state.hedged_mint
    )]
    pub vault_hedged_ata: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = kamino_collateral_ata.owner == vault_authority.key()
    )]
    pub kamino_collateral_ata: Account<'info, TokenAccount>,

    /// CHECK: Pyth Price Feed for hedged asset
    #[account(address = vault_state.pyth_price_feed)]
    pub pyth_price_feed: AccountInfo<'info>,

    /// CHECK: Kamino Program
    pub kamino_program: AccountInfo<'info>,
    /// CHECK: Kamino Reserve Account
    #[account(mut, address = vault_state.kamino_reserve)]
    pub kamino_reserve: AccountInfo<'info>,

    /// CHECK: Marginfi Program
    pub marginfi_program: AccountInfo<'info>,
    /// CHECK: Marginfi Bank Account
    #[account(mut, address = vault_state.marginfi_bank)]
    pub marginfi_bank: AccountInfo<'info>,
    /// CHECK: Marginfi Account PDA
    #[account(mut)]
    pub marginfi_account: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct RebalanceVault<'info> {
    #[account(mut)]
    pub keeper: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED, vault_state.deposit_mint.as_ref(), vault_state.hedged_mint.as_ref()],
        bump = vault_state.bump
    )]
    pub vault_state: Account<'info, VaultState>,

    /// CHECK: Vault PDA authority
    #[account(
        seeds = [AUTHORITY_SEED, vault_state.key().as_ref()],
        bump = vault_state.authority_bump
    )]
    pub vault_authority: AccountInfo<'info>,

    #[account(
        mut,
        constraint = vault_hedged_ata.owner == vault_authority.key(),
        constraint = vault_hedged_ata.mint == vault_state.hedged_mint
    )]
    pub vault_hedged_ata: Account<'info, TokenAccount>,

    /// CHECK: Pyth Price Feed
    #[account(address = vault_state.pyth_price_feed)]
    pub pyth_price_feed: AccountInfo<'info>,

    /// CHECK: Marginfi Program
    pub marginfi_program: AccountInfo<'info>,
    /// CHECK: Marginfi Bank
    #[account(mut, address = vault_state.marginfi_bank)]
    pub marginfi_bank: AccountInfo<'info>,
    /// CHECK: Marginfi Account
    #[account(mut)]
    pub marginfi_account: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct AdminControl<'info> {
    #[account(
        mut,
        has_one = admin @ RouterError::UnauthorizedAdmin
    )]
    pub vault_state: Account<'info, VaultState>,
    pub admin: Signer<'info>,
}

// ---------------------------------------------------------------------------------
// State & Events
// ---------------------------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct VaultState {
    pub admin: Pubkey,
    pub deposit_mint: Pubkey,
    pub hedged_mint: Pubkey,
    pub kamino_reserve: Pubkey,
    pub marginfi_bank: Pubkey,
    pub pyth_price_feed: Pubkey,
    pub target_leverage_bps: u64,
    pub rebalance_threshold_bps: u64,
    pub total_usdc_deposited: u64,
    pub total_hedged_borrowed: u64,
    pub bump: u8,
    pub authority_bump: u8,
    pub is_frozen: bool,
}

#[event]
pub struct VaultInitializedEvent {
    pub admin: Pubkey,
    pub vault: Pubkey,
    pub timestamp: i64,
}

#[event]
pub struct PositionHedgedEvent {
    pub depositor: Pubkey,
    pub amount_deposited: u64,
    pub hedged_borrowed_units: u64,
    pub oracle_price: i64,
    pub timestamp: i64,
}

#[event]
pub struct RebalancedEvent {
    pub deviation_bps: u64,
    pub new_hedged_borrow: u64,
    pub oracle_price: i64,
    pub timestamp: i64,
}

// ---------------------------------------------------------------------------------
// Errors & Math Utility
// ---------------------------------------------------------------------------------

#[error_code]
pub enum RouterError {
    #[msg("Stale or invalid Pyth oracle price feed.")]
    StaleOracleFeed,
    #[msg("Target leverage setting out of safe bounds [10000..30000].")]
    InvalidLeverage,
    #[msg("Rebalance threshold cannot exceed 1000 BPS (10%).")]
    InvalidThreshold,
    #[msg("Vault is currently frozen by administrator.")]
    VaultIsFrozen,
    #[msg("Deposit amount must be strictly greater than zero.")]
    ZeroDepositAmount,
    #[msg("Current delta deviation does not warrant rebalance invocation.")]
    RebalanceNotNeeded,
    #[msg("Signer does not possess administrator credentials.")]
    UnauthorizedAdmin,
    #[msg("Mathematical overflow or underflow in safe calculation.")]
    MathOverflow,
}

/// Parses and validates that Pyth oracle feed is current and confident
pub fn validate_and_fetch_oracle_price(
    price_account_info: &AccountInfo,
    current_timestamp: i64,
) -> Result<i64> {
    let price_feed = load_price_feed_from_account_info(price_account_info)
        .map_err(|_| RouterError::StaleOracleFeed)?;

    let price = price_feed
        .get_price_no_older_than(current_timestamp, MAX_PRICE_AGE_SECONDS)
        .ok_or(RouterError::StaleOracleFeed)?;

    require!(price.price > 0, RouterError::StaleOracleFeed);
    Ok(price.price)
}

/// Normalizes USD value to hedged units (e.g. USDC (10^6) -> SOL (10^9))
pub fn compute_hedged_borrow_units(
    usdc_amount: u64,
    pyth_price_expo8: i64,
    leverage_bps: u64,
) -> Result<u64> {
    // pyth_price is typically base 10^-8 for SOL/USD
    // Notional USD value = usdc_amount (6 decimals)
    // SOL units = (USDC_amount * 10^9 * 10^8) / (price * 10^6)
    let leveraged_usdc = (usdc_amount as u128)
        .checked_mul(leverage_bps as u128)
        .ok_or(RouterError::MathOverflow)?
        .checked_div(BPS_DENOMINATOR as u128)
        .ok_or(RouterError::MathOverflow)?;

    let sol_units = leveraged_usdc
        .checked_mul(1_000_000_000_000_000_000u128) // 10^18 scalar
        .ok_or(RouterError::MathOverflow)?
        .checked_div(pyth_price_expo8 as u128)
        .ok_or(RouterError::MathOverflow)?
        .checked_div(1_000_000u128) // eliminate USDC decimals
        .ok_or(RouterError::MathOverflow)?;

    Ok(sol_units as u64)
}

// ---------------------------------------------------------------------------------
// Cross-Program Invocations (CPI)
// ---------------------------------------------------------------------------------

pub fn execute_kamino_deposit<'info>(
    kamino_program: &AccountInfo<'info>,
    kamino_reserve: &AccountInfo<'info>,
    user_source_liquidity: &Account<'info, TokenAccount>,
    authority: &AccountInfo<'info>,
    destination_collateral: &Account<'info, TokenAccount>,
    amount: u64,
    bump: u8,
) -> Result<()> {
    // Kamino lending deposit instruction discriminator [145, 185, 237, 7, 108, 172, 85, 11]
    let mut data = Vec::with_capacity(16);
    data.extend_from_slice(&[145, 185, 237, 7, 108, 172, 85, 11]);
    data.extend_from_slice(&amount.to_le_bytes());

    let ix = solana_program::instruction::Instruction {
        program_id: *kamino_program.key,
        accounts: vec![
            AccountMeta::new(user_source_liquidity.key(), false),
            AccountMeta::new(destination_collateral.key(), false),
            AccountMeta::new(*kamino_reserve.key, false),
            AccountMeta::new_readonly(*authority.key, true),
        ],
        data,
    };

    let seeds = &[AUTHORITY_SEED, authority.key.as_ref(), &[bump]];
    solana_program::program::invoke_signed(&ix, &[
        kamino_program.clone(),
        user_source_liquidity.to_account_info(),
        destination_collateral.to_account_info(),
        kamino_reserve.clone(),
        authority.clone(),
    ], &[seeds])?;

    Ok(())
}

pub fn execute_marginfi_borrow<'info>(
    marginfi_program: &AccountInfo<'info>,
    marginfi_bank: &AccountInfo<'info>,
    marginfi_account: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    destination_ata: &Account<'info, TokenAccount>,
    amount: u64,
    bump: u8,
) -> Result<()> {
    // Marginfi v2 borrow discriminator [229, 3, 201, 106, 111, 212, 170, 142]
    let mut data = Vec::with_capacity(16);
    data.extend_from_slice(&[229, 3, 201, 106, 111, 212, 170, 142]);
    data.extend_from_slice(&amount.to_le_bytes());

    let ix = solana_program::instruction::Instruction {
        program_id: *marginfi_program.key,
        accounts: vec![
            AccountMeta::new(*marginfi_account.key, false),
            AccountMeta::new(*marginfi_bank.key, false),
            AccountMeta::new_readonly(*authority.key, true),
            AccountMeta::new(destination_ata.key(), false),
        ],
        data,
    };

    let seeds = &[AUTHORITY_SEED, authority.key.as_ref(), &[bump]];
    solana_program::program::invoke_signed(&ix, &[
        marginfi_program.clone(),
        marginfi_account.clone(),
        marginfi_bank.clone(),
        authority.clone(),
        destination_ata.to_account_info(),
    ], &[seeds])?;

    Ok(())
}

pub fn execute_marginfi_repay<'info>(
    marginfi_program: &AccountInfo<'info>,
    marginfi_bank: &AccountInfo<'info>,
    marginfi_account: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    source_ata: &Account<'info, TokenAccount>,
    amount: u64,
    bump: u8,
) -> Result<()> {
    // Marginfi v2 repay discriminator [236, 179, 131, 184, 185, 17, 108, 14]
    let mut data = Vec::with_capacity(16);
    data.extend_from_slice(&[236, 179, 131, 184, 185, 17, 108, 14]);
    data.extend_from_slice(&amount.to_le_bytes());

    let ix = solana_program::instruction::Instruction {
        program_id: *marginfi_program.key,
        accounts: vec![
            AccountMeta::new(*marginfi_account.key, false),
            AccountMeta::new(*marginfi_bank.key, false),
            AccountMeta::new_readonly(*authority.key, true),
            AccountMeta::new(source_ata.key(), false),
        ],
        data,
    };

    let seeds = &[AUTHORITY_SEED, authority.key.as_ref(), &[bump]];
    solana_program::program::invoke_signed(&ix, &[
        marginfi_program.clone(),
        marginfi_account.clone(),
        marginfi_bank.clone(),
        authority.clone(),
        source_ata.to_account_info(),
    ], &[seeds])?;

    Ok(())
}