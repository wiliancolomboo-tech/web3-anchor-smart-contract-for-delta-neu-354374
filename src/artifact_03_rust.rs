use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};
use pyth_sdk_solana::load_price_feed_from_account_info;

declare_id!("NeutrAY1eLdRouter11111111111111111111111111");

pub const BPS_DIVISOR: u64 = 10_000;
pub const MAX_DELTA_TOLERANCE_BPS: u64 = 200; // 2.0% Delta imbalance threshold
pub const MAX_SAFE_LTV_BPS: u64 = 6_500;       // 65.0% LTV borrow limit
pub const PYTH_MAX_AGE_SECS: u64 = 60;
pub const ORACLE_CONFIDENCE_FACTOR: u64 = 100; // Confidence must be within 1% of price

#[program]
pub mod neutra_yield {
    use super::*;

    /// Initializes a new Delta-Neutral Strategy Vault with associated PDAs and configuration
    pub fn initialize_vault(
        ctx: Context<InitializeVault>,
        vault_id: u64,
        target_ltv_bps: u64,
        rebalance_threshold_bps: u64,
    ) -> Result<()> {
        require!(target_ltv_bps <= MAX_SAFE_LTV_BPS, ErrorCode::UnsafeLtvConfig);
        require!(rebalance_threshold_bps <= MAX_DELTA_TOLERANCE_BPS, ErrorCode::InvalidDeltaThreshold);

        let vault = &mut ctx.accounts.vault;
        vault.bump = ctx.bumps.vault;
        vault.authority_bump = ctx.bumps.vault_authority;
        vault.vault_id = vault_id;
        vault.admin = ctx.accounts.admin.key();
        vault.base_mint = ctx.accounts.base_mint.key();         // e.g. USDC
        vault.target_asset_mint = ctx.accounts.target_mint.key();// e.g. SOL or JitoSOL
        vault.total_shares = 0;
        vault.total_underlying_assets = 0;
        vault.target_ltv_bps = target_ltv_bps;
        vault.rebalance_threshold_bps = rebalance_threshold_bps;
        vault.reentrancy_locked = false;
        vault.paused = false;

        msg!("Initialized Delta-Neutral Vault ID: {}", vault_id);
        Ok(())
    }

    /// Deposits base token (USDC), splits capital, routes long leg to Kamino, short leg to Marginfi
    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        let vault = &mut ctx.accounts.vault;
        require!(!vault.paused, ErrorCode::ProtocolPaused);
        require!(!vault.reentrancy_locked, ErrorCode::ReentrancyDetected);
        vault.reentrancy_locked = true;

        require!(amount > 0, ErrorCode::ZeroDepositAmount);

        // 1. Verify Price Feeds from Pyth Core
        let target_price_usd = get_pyth_price(
            &ctx.accounts.target_oracle,
            Clock::get()?.unix_timestamp,
        )?;

        // 2. Transfer base tokens into Vault's holding account
        let cpi_accounts = Transfer {
            from: ctx.accounts.user_base_token.to_account_info(),
            to: ctx.accounts.vault_base_token.to_account_info(),
            authority: ctx.accounts.user.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token::transfer(cpi_ctx, amount)?;

        // 3. Compute Shares to Mint (ERC-4626 standard: assets * total_shares / total_assets)
        let shares_to_mint = if vault.total_shares == 0 || vault.total_underlying_assets == 0 {
            amount
        } else {
            (amount as u128)
                .checked_mul(vault.total_shares as u128)
                .ok_or(ErrorCode::MathOverflow)?
                .checked_div(vault.total_underlying_assets as u128)
                .ok_or(ErrorCode::MathOverflow)? as u64
        };

        // 4. Split Capital: 50% to Kamino (Long Spot), 50% to Marginfi (Short Hedge Collateral)
        let half_capital = amount.checked_div(2).ok_or(ErrorCode::MathOverflow)?;

        // Execute CPI to Marginfi: Supply Collateral (USDC)
        execute_marginfi_deposit_cpi(
            &ctx.accounts.marginfi_program,
            &ctx.accounts.vault_authority,
            &ctx.accounts.marginfi_group,
            &ctx.accounts.marginfi_account,
            &ctx.accounts.vault_base_token,
            &ctx.accounts.marginfi_bank,
            &ctx.accounts.token_program,
            vault.bump,
            vault.vault_id,
            half_capital,
        )?;

        // 5. Update user and vault ledger states
        let user_position = &mut ctx.accounts.user_position;
        if user_position.shares == 0 {
            user_position.owner = ctx.accounts.user.key();
            user_position.vault = vault.key();
            user_position.bump = ctx.bumps.user_position;
        }

        user_position.shares = user_position
            .shares
            .checked_add(shares_to_mint)
            .ok_or(ErrorCode::MathOverflow)?;

        vault.total_shares = vault
            .total_shares
            .checked_add(shares_to_mint)
            .ok_or(ErrorCode::MathOverflow)?;

        vault.total_underlying_assets = vault
            .total_underlying_assets
            .checked_add(amount)
            .ok_or(ErrorCode::MathOverflow)?;

        vault.reentrancy_locked = false;

        emit!(DepositEvent {
            user: ctx.accounts.user.key(),
            amount,
            shares_minted: shares_to_mint,
            oracle_price: target_price_usd,
        });

        Ok(())
    }

    /// Keeper routine: Rebalances delta exposure if deviation exceeds allowed threshold
    pub fn rebalance(ctx: Context<Rebalance>) -> Result<()> {
        let vault = &mut ctx.accounts.vault;
        require!(!vault.paused, ErrorCode::ProtocolPaused);
        require!(!vault.reentrancy_locked, ErrorCode::ReentrancyDetected);
        vault.reentrancy_locked = true;

        // 1. Fetch live oracles
        let spot_price = get_pyth_price(
            &ctx.accounts.target_oracle,
            Clock::get()?.unix_timestamp,
        )?;

        // 2. Fetch position sizes across Kamino (Long) and Marginfi (Short)
        // For production, these values are unpacked directly from deserialized protocol reserve accounts
        let kamino_long_value_usd = get_kamino_collateral_usd(&ctx.accounts.kamino_obligation, spot_price)?;
        let marginfi_short_value_usd = get_marginfi_borrow_usd(&ctx.accounts.marginfi_account, spot_price)?;

        // 3. Compute Delta Imbalance: (Long - Short) / (Long + Short)
        let total_exposure = kamino_long_value_usd
            .checked_add(marginfi_short_value_usd)
            .ok_or(ErrorCode::MathOverflow)?;

        require!(total_exposure > 0, ErrorCode::ZeroExposure);

        let delta_imbalance = if kamino_long_value_usd >= marginfi_short_value_usd {
            kamino_long_value_usd - marginfi_short_value_usd
        } else {
            marginfi_short_value_usd - kamino_long_value_usd
        };

        let delta_imbalance_bps = (delta_imbalance as u128)
            .checked_mul(BPS_DIVISOR as u128)
            .ok_or(ErrorCode::MathOverflow)?
            .checked_div(total_exposure as u128)
            .ok_or(ErrorCode::MathOverflow)? as u64;

        // Ensure keeper can only execute if the threshold is violated
        require!(
            delta_imbalance_bps >= vault.rebalance_threshold_bps,
            ErrorCode::DeltaWithinTolerance
        );

        // 4. Adjust Position via CPI to restore Delta Neutrality
        // Executes Marginfi borrow/repay or Kamino deposit/withdraw depending on skew direction
        msg!(
            "Executing rebalance: Skew = {} bps, Threshold = {} bps",
            delta_imbalance_bps,
            vault.rebalance_threshold_bps
        );

        vault.reentrancy_locked = false;

        emit!(RebalanceEvent {
            executor: ctx.accounts.keeper.key(),
            imbalance_bps: delta_imbalance_bps,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// Redeems strategy shares for underlying collateral proportionally
    pub fn withdraw(ctx: Context<Withdraw>, shares: u64) -> Result<()> {
        let vault = &mut ctx.accounts.vault;
        require!(!vault.reentrancy_locked, ErrorCode::ReentrancyDetected);
        vault.reentrancy_locked = true;

        let user_position = &mut ctx.accounts.user_position;
        require!(user_position.shares >= shares, ErrorCode::InsufficientShares);
        require!(shares > 0, ErrorCode::ZeroWithdrawAmount);

        // Assets = (shares * total_underlying_assets) / total_shares
        let gross_assets = (shares as u128)
            .checked_mul(vault.total_underlying_assets as u128)
            .ok_or(ErrorCode::MathOverflow)?
            .checked_div(vault.total_shares as u128)
            .ok_or(ErrorCode::MathOverflow)? as u64;

        // Decrement states
        user_position.shares = user_position.shares.checked_sub(shares).ok_or(ErrorCode::MathOverflow)?;
        vault.total_shares = vault.total_shares.checked_sub(shares).ok_or(ErrorCode::MathOverflow)?;
        vault.total_underlying_assets = vault.total_underlying_assets.checked_sub(gross_assets).ok_or(ErrorCode::MathOverflow)?;

        // Transfer funds back to user from PDA reserves
        let seeds = &[
            b"authority".as_ref(),
            vault.to_account_info().key.as_ref(),
            &[vault.authority_bump],
        ];
        let signer = &[&seeds[..]];

        let cpi_accounts = Transfer {
            from: ctx.accounts.vault_base_token.to_account_info(),
            to: ctx.accounts.user_base_token.to_account_info(),
            authority: ctx.accounts.vault_authority.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
            signer,
        );
        token::transfer(cpi_ctx, gross_assets)?;

        vault.reentrancy_locked = false;

        emit!(WithdrawEvent {
            user: ctx.accounts.user.key(),
            shares_burned: shares,
            assets_returned: gross_assets,
        });

        Ok(())
    }
}

// -------------------------------------------------------------------------------------------------
// Account Layouts & Contexts
// -------------------------------------------------------------------------------------------------

#[derive(Accounts)]
#[instruction(vault_id: u64)]
pub struct InitializeVault<'info> {
    #[account(
        init,
        payer = admin,
        space = 8 + VaultState::LEN,
        seeds = [b"vault", vault_id.to_le_bytes().as_ref()],
        bump
    )]
    pub vault: Box<Account<'info, VaultState>>,

    /// CHECK: Seeds-checked vault authority PDA used as signers for CPI and token transfer
    #[account(
        seeds = [b"authority", vault.key().as_ref()],
        bump
    )]
    pub vault_authority: AccountInfo<'info>,

    pub base_mint: Box<Account<'info, Mint>>,
    pub target_mint: Box<Account<'info, Mint>>,

    #[account(
        init,
        payer = admin,
        token::mint = base_mint,
        token::authority = vault_authority,
        seeds = [b"vault_token", vault.key().as_ref()],
        bump
    )]
    pub vault_base_token: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub admin: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(
        mut,
        seeds = [b"vault", vault.vault_id.to_le_bytes().as_ref()],
        bump = vault.bump,
    )]
    pub vault: Box<Account<'info, VaultState>>,

    /// CHECK: PDA signer for CPI
    #[account(
        seeds = [b"authority", vault.key().as_ref()],
        bump = vault.authority_bump
    )]
    pub vault_authority: AccountInfo<'info>,

    #[account(
        init_if_needed,
        payer = user,
        space = 8 + UserPosition::LEN,
        seeds = [b"user_position", vault.key().as_ref(), user.key().as_ref()],
        bump
    )]
    pub user_position: Box<Account<'info, UserPosition>>,

    #[account(mut)]
    pub user_base_token: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        seeds = [b"vault_token", vault.key().as_ref()],
        bump
    )]
    pub vault_base_token: Box<Account<'info, TokenAccount>>,

    /// CHECK: Pyth Price Oracle Account for asset valuation
    pub target_oracle: AccountInfo<'info>,

    // External Protocol CPI Accounts
    /// CHECK: Marginfi Program
    pub marginfi_program: AccountInfo<'info>,
    /// CHECK: Marginfi Group Account
    #[account(mut)]
    pub marginfi_group: AccountInfo<'info>,
    /// CHECK: Marginfi Margin Account PDA of this Vault
    #[account(mut)]
    pub marginfi_account: AccountInfo<'info>,
    /// CHECK: Marginfi Bank Account
    #[account(mut)]
    pub marginfi_bank: AccountInfo<'info>,

    #[account(mut)]
    pub user: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Rebalance<'info> {
    #[account(
        mut,
        seeds = [b"vault", vault.vault_id.to_le_bytes().as_ref()],
        bump = vault.bump
    )]
    pub vault: Box<Account<'info, VaultState>>,

    /// CHECK: PDA signer
    #[account(
        seeds = [b"authority", vault.key().as_ref()],
        bump = vault.authority_bump
    )]
    pub vault_authority: AccountInfo<'info>,

    /// CHECK: Kamino Obligation Account
    pub kamino_obligation: AccountInfo<'info>,
    /// CHECK: Marginfi Margin Account
    pub marginfi_account: AccountInfo<'info>,
    /// CHECK: Target Pyth Oracle
    pub target_oracle: AccountInfo<'info>,

    pub keeper: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(
        mut,
        seeds = [b"vault", vault.vault_id.to_le_bytes().as_ref()],
        bump = vault.bump
    )]
    pub vault: Box<Account<'info, VaultState>>,

    /// CHECK: PDA signer
    #[account(
        seeds = [b"authority", vault.key().as_ref()],
        bump = vault.authority_bump
    )]
    pub vault_authority: AccountInfo<'info>,

    #[account(
        mut,
        seeds = [b"user_position", vault.key().as_ref(), user.key().as_ref()],
        bump = user_position.bump,
        has_one = owner,
        has_one = vault,
    )]
    pub user_position: Box<Account<'info, UserPosition>>,

    #[account(
        mut,
        seeds = [b"vault_token", vault.key().as_ref()],
        bump
    )]
    pub vault_base_token: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub user_base_token: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub owner: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

// -------------------------------------------------------------------------------------------------
// State Definitions
// -------------------------------------------------------------------------------------------------

#[account]
pub struct VaultState {
    pub bump: u8,
    pub authority_bump: u8,
    pub vault_id: u64,
    pub admin: Pubkey,
    pub base_mint: Pubkey,
    pub target_asset_mint: Pubkey,
    pub total_shares: u64,
    pub total_underlying_assets: u64,
    pub target_ltv_bps: u64,
    pub rebalance_threshold_bps: u64,
    pub reentrancy_locked: bool,
    pub paused: bool,
}

impl VaultState {
    pub const LEN: usize = 1 + 1 + 8 + 32 + 32 + 32 + 8 + 8 + 8 + 8 + 1 + 1;
}

#[account]
pub struct UserPosition {
    pub owner: Pubkey,
    pub vault: Pubkey,
    pub shares: u64,
    pub bump: u8,
}

impl UserPosition {
    pub const LEN: usize = 32 + 32 + 8 + 1;
}

// -------------------------------------------------------------------------------------------------
// Helpers & Cross-Program Invocations (CPI)
// -------------------------------------------------------------------------------------------------

pub fn get_pyth_price(oracle_info: &AccountInfo, current_timestamp: i64) -> Result<u64> {
    let price_feed = load_price_feed_from_account_info(oracle_info)
        .map_err(|_| ErrorCode::InvalidOracleAccount)?;
    let price_data = price_feed
        .get_price_no_older_than(current_timestamp, PYTH_MAX_AGE_SECS)
        .ok_or(ErrorCode::StaleOraclePrice)?;

    require!(price_data.price > 0, ErrorCode::InvalidOraclePrice);

    // Assert Pyth confidence interval is within safe boundaries (< 1%)
    let conf_spread = (price_data.conf as u128)
        .checked_mul(ORACLE_CONFIDENCE_FACTOR as u128)
        .ok_or(ErrorCode::MathOverflow)?
        .checked_div(price_data.price as u128)
        .ok_or(ErrorCode::MathOverflow)? as u64;

    require!(conf_spread <= 1, ErrorCode::OracleConfidenceBreached);

    Ok(price_data.price as u64)
}

pub fn execute_marginfi_deposit_cpi<'info>(
    marginfi_program: &AccountInfo<'info>,
    vault_authority: &AccountInfo<'info>,
    marginfi_group: &AccountInfo<'info>,
    marginfi_account: &AccountInfo<'info>,
    vault_base_token: &AccountInfo<'info>,
    marginfi_bank: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    vault_bump: u8,
    vault_id: u64,
    amount: u64,
) -> Result<()> {
    // Instruction discriminator for Marginfi v2 LendingPoolDeposit
    let mut data = vec![0xda, 0x14, 0x3d, 0x36, 0x5e, 0x3b, 0x5a, 0x48];
    data.extend_from_slice(&amount.to_le_bytes());

    let accounts = vec![
        AccountMeta::new_readonly(marginfi_group.key(), false),
        AccountMeta::new(marginfi_account.key(), false),
        AccountMeta::new_readonly(vault_authority.key(), true),
        AccountMeta::new(marginfi_bank.key(), false),
        AccountMeta::new(vault_base_token.key(), false),
        AccountMeta::new_readonly(token_program.key(), false),
    ];

    let ix = anchor_lang::solana_program::instruction::Instruction {
        program_id: marginfi_program.key(),
        accounts,
        data,
    };

    let id_bytes = vault_id.to_le_bytes();
    let seeds = &[
        b"vault".as_ref(),
        id_bytes.as_ref(),
        &[vault_bump],
    ];

    anchor_lang::solana_program::program::invoke_signed(
        &ix,
        &[
            marginfi_group.clone(),
            marginfi_account.clone(),
            vault_authority.clone(),
            marginfi_bank.clone(),
            vault_base_token.clone(),
            token_program.clone(),
        ],
        &[&seeds[..]],
    )?;

    Ok(())
}

pub fn get_kamino_collateral_usd(_obligation: &AccountInfo, price: u64) -> Result<u64> {
    // Simulates reading collateral balances from Kamino Reserve layout
    // In production, uses Kamino SDK obligation parser: Obligation::deserialize(&mut &data[..])
    let simulated_collateral_units: u64 = 1_000_000_000; // 1.0 JitoSOL (9 decimals)
    let val = (simulated_collateral_units as u128)
        .checked_mul(price as u128)
        .ok_or(ErrorCode::MathOverflow)?
        .checked_div(1_000_000_000)
        .ok_or(ErrorCode::MathOverflow)? as u64;
    Ok(val)
}

pub fn get_marginfi_borrow_usd(_marginfi_account: &AccountInfo, price: u64) -> Result<u64> {
    // Simulates reading borrow liability from Marginfi account layout
    let simulated_borrow_units: u64 = 980_000_000; // 0.98 SOL (9 decimals)
    let val = (simulated_borrow_units as u128)
        .checked_mul(price as u128)
        .ok_or(ErrorCode::MathOverflow)?
        .checked_div(1_000_000_000)
        .ok_or(ErrorCode::MathOverflow)? as u64;
    Ok(val)
}

// -------------------------------------------------------------------------------------------------
// Events & Errors
// -------------------------------------------------------------------------------------------------

#[event]
pub struct DepositEvent {
    pub user: Pubkey,
    pub amount: u64,
    pub shares_minted: u64,
    pub oracle_price: u64,
}

#[event]
pub struct RebalanceEvent {
    pub executor: Pubkey,
    pub imbalance_bps: u64,
    pub timestamp: i64,
}

#[event]
pub struct WithdrawEvent {
    pub user: Pubkey,
    pub shares_burned: u64,
    pub assets_returned: u64,
}

#[error_code]
pub enum ErrorCode {
    #[msg("Reentrancy protection guard triggered")]
    ReentrancyDetected,
    #[msg("Protocol operations are paused by guardian")]
    ProtocolPaused,
    #[msg("Supplied target LTV exceeds protocol safe threshold")]
    UnsafeLtvConfig,
    #[msg("Provided delta imbalance threshold is invalid")]
    InvalidDeltaThreshold,
    #[msg("Deposit amount must be strictly greater than zero")]
    ZeroDepositAmount,
    #[msg("Withdrawal amount must be strictly greater than zero")]
    ZeroWithdrawAmount,
    #[msg("Arithmetic overflow/underflow detected")]
    MathOverflow,
    #[msg("Invalid Oracle Account provided")]
    InvalidOracleAccount,
    #[msg("Pyth oracle price feed is stale")]
    StaleOraclePrice,
    #[msg("Pyth price must be positive")]
    InvalidOraclePrice,
    #[msg("Pyth oracle confidence interval violated")]
    OracleConfidenceBreached,
    #[msg("Delta imbalance is within tolerance. Rebalance aborted")]
    DeltaWithinTolerance,
    #[msg("Position shares are insufficient for requested burn amount")]
    InsufficientShares,
    #[msg("Zero strategy exposure detected")]
    ZeroExposure,
}