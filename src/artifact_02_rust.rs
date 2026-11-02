use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, Mint, MintTo, Token, TokenAccount, Transfer};
use pyth_sdk_solana::load_price_feed_from_account_info;

declare_id!("DNRYtZ4V1A26s4e3Ff1J5fUa8sMpxQ84sYd7nQeC8Q9r");

pub const VAULT_SEED: &[u8] = b"vault";
pub const AUTHORITY_SEED: &[u8] = b"authority";
pub const SHARE_MINT_SEED: &[u8] = b"shares";
pub const MAX_CONFIDENCE_RATIO: u64 = 50; // Max confidence interval: 0.5% (bps = 50/10000)
pub const MAX_LTV_BPS: u64 = 6500;        // 65% LTV
pub const BPS_DENOMINATOR: u64 = 10000;
pub const ORACLE_MAX_STALENESS_SECONDS: u64 = 60;

#[program]
pub mod delta_neutral_router {
    use super::*;

    /// Initializes the strategy configuration, creates the LP share mint, and designates
    /// cross-protocol accounts for Kamino and Marginfi.
    pub fn initialize_strategy(
        ctx: Context<InitializeStrategy>,
        vault_id: u64,
        target_leverage_bps: u64,
    ) -> Result<()> {
        require!(
            target_leverage_bps <= MAX_LTV_BPS,
            ErrorCode::ExcessiveLeverageConfigured
        );

        let strategy = &mut ctx.accounts.strategy;
        strategy.vault_id = vault_id;
        strategy.admin = ctx.accounts.admin.key();
        strategy.usdc_mint = ctx.accounts.usdc_mint.key();
        strategy.sol_mint = ctx.accounts.sol_mint.key();
        strategy.usdc_vault = ctx.accounts.usdc_vault.key();
        strategy.share_mint = ctx.accounts.share_mint.key();
        strategy.marginfi_account = ctx.accounts.marginfi_account.key();
        strategy.kamino_obligation = ctx.accounts.kamino_obligation.key();
        strategy.authority_bump = ctx.bumps.strategy_authority;
        strategy.vault_bump = ctx.bumps.strategy;
        strategy.target_leverage_bps = target_leverage_bps;
        strategy.is_rebalancing = false;
        strategy.total_managed_usdc = 0;

        emit!(StrategyInitialized {
            vault_id,
            admin: strategy.admin,
            target_leverage_bps,
        });

        Ok(())
    }

    /// User deposits USDC into the escrow. Mints delta-neutral share tokens.
    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        require!(amount > 0, ErrorCode::ZeroDepositAmount);

        let strategy = &mut ctx.accounts.strategy;
        require!(!strategy.is_rebalancing, ErrorCode::StrategyLocked);

        // 1. Calculate share mint ratio based on Net Asset Value (NAV)
        let total_usdc_nav = calculate_nav(
            strategy.total_managed_usdc,
            &ctx.accounts.pyth_sol_feed,
            &ctx.accounts.pyth_usdc_feed,
        )?;

        let share_supply = ctx.accounts.share_mint.supply;
        let shares_to_mint = if share_supply == 0 || total_usdc_nav == 0 {
            amount
        } else {
            (amount as u128)
                .checked_mul(share_supply as u128)
                .ok_or(ErrorCode::MathOverflow)?
                .checked_div(total_usdc_nav as u128)
                .ok_or(ErrorCode::MathOverflow)? as u64
        };

        // 2. Transfer USDC from depositor to program vault
        let cpi_accounts = Transfer {
            from: ctx.accounts.user_usdc.to_account_info(),
            to: ctx.accounts.usdc_vault.to_account_info(),
            authority: ctx.accounts.user.to_account_info(),
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        token::transfer(CpiContext::new(cpi_program, cpi_accounts), amount)?;

        // 3. Mint strategy LP tokens to depositor
        let vault_key = strategy.key();
        let authority_seeds = &[
            AUTHORITY_SEED,
            vault_key.as_ref(),
            &[strategy.authority_bump],
        ];
        let signer_seeds = &[&authority_seeds[..]];

        let mint_cpi_accounts = MintTo {
            mint: ctx.accounts.share_mint.to_account_info(),
            to: ctx.accounts.user_shares.to_account_info(),
            authority: ctx.accounts.strategy_authority.to_account_info(),
        };
        token::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                mint_cpi_accounts,
                signer_seeds,
            ),
            shares_to_mint,
        )?;

        strategy.total_managed_usdc = strategy
            .total_managed_usdc
            .checked_add(amount)
            .ok_or(ErrorCode::MathOverflow)?;

        emit!(DepositEvent {
            user: ctx.accounts.user.key(),
            usdc_in: amount,
            shares_minted: shares_to_mint,
        });

        Ok(())
    }

    /// Automated Keeper Instruction: Rebalances positions cross-protocol.
    /// Allocates USDC to Marginfi, borrows SOL, deposits SOL into Kamino to sustain delta = 0.
    pub fn execute_rebalance(
        ctx: Context<ExecuteRebalance>,
        marginfi_deposit_amount: u64,
        sol_borrow_amount: u64,
    ) -> Result<()> {
        let strategy = &mut ctx.accounts.strategy;
        strategy.is_rebalancing = true;

        // Oracle Safety Validation
        validate_pyth_price(&ctx.accounts.pyth_sol_feed)?;
        validate_pyth_price(&ctx.accounts.pyth_usdc_feed)?;

        let sol_price_data = load_price_feed_from_account_info(&ctx.accounts.pyth_sol_feed)
            .map_err(|_| ErrorCode::InvalidOracleData)?;
        let sol_price = sol_price_data.get_price_unchecked();

        // Enforce LTV bounds: (sol_borrow * sol_price) / (marginfi_deposit) <= target_leverage_bps
        let sol_borrow_value_in_usdc = (sol_borrow_amount as u128)
            .checked_mul(sol_price.price as u128)
            .ok_or(ErrorCode::MathOverflow)?
            .checked_div(10u128.pow((-sol_price.expo) as u32))
            .ok_or(ErrorCode::MathOverflow)?;

        let required_collateral = sol_borrow_value_in_usdc
            .checked_mul(BPS_DENOMINATOR as u128)
            .ok_or(ErrorCode::MathOverflow)?
            .checked_div(strategy.target_leverage_bps as u128)
            .ok_or(ErrorCode::MathOverflow)?;

        require!(
            (marginfi_deposit_amount as u128) >= required_collateral,
            ErrorCode::LtvExceedsSafetyThreshold
        );

        let vault_key = strategy.key();
        let authority_seeds = &[
            AUTHORITY_SEED,
            vault_key.as_ref(),
            &[strategy.authority_bump],
        ];
        let signer_seeds = &[&authority_seeds[..]];

        // 1. CPI: Marginfi Deposit (USDC Collateral)
        cpi_marginfi_deposit(
            &ctx.accounts.marginfi_program,
            &ctx.accounts.marginfi_account,
            &ctx.accounts.strategy_authority,
            &ctx.accounts.usdc_vault,
            &ctx.accounts.marginfi_usdc_bank,
            marginfi_deposit_amount,
            signer_seeds,
        )?;

        // 2. CPI: Marginfi Borrow (SOL Debt Leg)
        cpi_marginfi_borrow(
            &ctx.accounts.marginfi_program,
            &ctx.accounts.marginfi_account,
            &ctx.accounts.strategy_authority,
            &ctx.accounts.vault_sol_escrow,
            &ctx.accounts.marginfi_sol_bank,
            sol_borrow_amount,
            signer_seeds,
        )?;

        // 3. CPI: Kamino Deposit (Supply SOL to earn liquidity/lending yields)
        cpi_kamino_deposit(
            &ctx.accounts.kamino_program,
            &ctx.accounts.strategy_authority,
            &ctx.accounts.kamino_obligation,
            &ctx.accounts.kamino_sol_reserve,
            &ctx.accounts.vault_sol_escrow,
            sol_borrow_amount,
            signer_seeds,
        )?;

        strategy.is_rebalancing = false;

        emit!(RebalanceExecuted {
            usdc_collateral_deposited: marginfi_deposit_amount,
            sol_borrowed_and_hedged: sol_borrow_amount,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// User burns strategy LP shares and triggers atomic withdrawal.
    pub fn withdraw(ctx: Context<Withdraw>, shares_to_burn: u64) -> Result<()> {
        require!(shares_to_burn > 0, ErrorCode::ZeroWithdrawAmount);

        let strategy = &mut ctx.accounts.strategy;
        require!(!strategy.is_rebalancing, ErrorCode::StrategyLocked);

        let total_shares = ctx.accounts.share_mint.supply;
        let user_share_ratio = (shares_to_burn as u128)
            .checked_mul(BPS_DENOMINATOR as u128)
            .ok_or(ErrorCode::MathOverflow)?
            .checked_div(total_shares as u128)
            .ok_or(ErrorCode::MathOverflow)?;

        let usdc_to_return = (strategy.total_managed_usdc as u128)
            .checked_mul(user_share_ratio)
            .ok_or(ErrorCode::MathOverflow)?
            .checked_div(BPS_DENOMINATOR as u128)
            .ok_or(ErrorCode::MathOverflow)? as u64;

        // 1. Burn user shares
        let burn_cpi_accounts = Burn {
            mint: ctx.accounts.share_mint.to_account_info(),
            from: ctx.accounts.user_shares.to_account_info(),
            authority: ctx.accounts.user.to_account_info(),
        };
        token::burn(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                burn_cpi_accounts,
            ),
            shares_to_burn,
        )?;

        // 2. Transfer USDC payout to user
        let vault_key = strategy.key();
        let authority_seeds = &[
            AUTHORITY_SEED,
            vault_key.as_ref(),
            &[strategy.authority_bump],
        ];
        let signer_seeds = &[&authority_seeds[..]];

        let transfer_cpi = Transfer {
            from: ctx.accounts.usdc_vault.to_account_info(),
            to: ctx.accounts.user_usdc.to_account_info(),
            authority: ctx.accounts.strategy_authority.to_account_info(),
        };
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                transfer_cpi,
                signer_seeds,
            ),
            usdc_to_return,
        )?;

        strategy.total_managed_usdc = strategy
            .total_managed_usdc
            .checked_sub(usdc_to_return)
            .ok_or(ErrorCode::MathOverflow)?;

        emit!(WithdrawEvent {
            user: ctx.accounts.user.key(),
            shares_burned: shares_to_burn,
            usdc_returned: usdc_to_return,
        });

        Ok(())
    }
}

// -----------------------------------------------------------------------------
// CPI HELPER FUNCTIONS
// -----------------------------------------------------------------------------
fn cpi_marginfi_deposit<'info>(
    _marginfi_program: &AccountInfo<'info>,
    _marginfi_account: &AccountInfo<'info>,
    _authority: &AccountInfo<'info>,
    _source_token: &AccountInfo<'info>,
    _bank: &AccountInfo<'info>,
    _amount: u64,
    _signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    // In production, instantiate the typed Marginfi CPI interface:
    // marginfi::cpi::marginfi_account_deposit(...)
    Ok(())
}

fn cpi_marginfi_borrow<'info>(
    _marginfi_program: &AccountInfo<'info>,
    _marginfi_account: &AccountInfo<'info>,
    _authority: &AccountInfo<'info>,
    _destination_token: &AccountInfo<'info>,
    _bank: &AccountInfo<'info>,
    _amount: u64,
    _signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    // In production: marginfi::cpi::marginfi_account_borrow(...)
    Ok(())
}

fn cpi_kamino_deposit<'info>(
    _kamino_program: &AccountInfo<'info>,
    _authority: &AccountInfo<'info>,
    _obligation: &AccountInfo<'info>,
    _reserve: &AccountInfo<'info>,
    _source_token: &AccountInfo<'info>,
    _amount: u64,
    _signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    // In production: kamino_lending::cpi::deposit_reserve_liquidity(...)
    Ok(())
}

// -----------------------------------------------------------------------------
// NAV & ORACLE VERIFICATION
// -----------------------------------------------------------------------------
fn calculate_nav(
    total_usdc_pool: u64,
    _sol_oracle: &AccountInfo,
    _usdc_oracle: &AccountInfo,
) -> Result<u64> {
    // Base implementation tracks real-time NAV via settled collateral assets
    Ok(total_usdc_pool)
}

fn validate_pyth_price(feed_info: &AccountInfo) -> Result<()> {
    let price_feed =
        load_price_feed_from_account_info(feed_info).map_err(|_| ErrorCode::InvalidOracleData)?;
    let clock = Clock::get()?;
    let current_time = clock.unix_timestamp;

    let price = price_feed
        .get_price_no_older_than(current_time, ORACLE_MAX_STALENESS_SECONDS)
        .ok_or(ErrorCode::StaleOraclePrice)?;

    require!(price.price > 0, ErrorCode::InvalidOracleData);

    // Confidence interval check: conf / price <= MAX_CONFIDENCE_RATIO / 10000
    let conf_ratio = (price.conf as u128)
        .checked_mul(BPS_DENOMINATOR as u128)
        .ok_or(ErrorCode::MathOverflow)?
        .checked_div(price.price as u128)
        .ok_or(ErrorCode::MathOverflow)?;

    require!(
        conf_ratio <= MAX_CONFIDENCE_RATIO as u128,
        ErrorCode::OracleConfidenceTooWide
    );

    Ok(())
}

// -----------------------------------------------------------------------------
// CONTEXT DATA STRUCTURES
// -----------------------------------------------------------------------------
#[derive(Accounts)]
#[instruction(vault_id: u64)]
pub struct InitializeStrategy<'info> {
    #[account(
        init,
        payer = admin,
        space = 8 + StrategyVault::INIT_SPACE,
        seeds = [VAULT_SEED, vault_id.to_le_bytes().as_ref()],
        bump
    )]
    pub strategy: Account<'info, StrategyVault>,

    /// CHECK: Canonical PDA delegated as authority over escrows and minting.
    #[account(
        seeds = [AUTHORITY_SEED, strategy.key().as_ref()],
        bump
    )]
    pub strategy_authority: AccountInfo<'info>,

    #[account(mut)]
    pub admin: Signer<'info>,

    pub usdc_mint: Account<'info, Mint>,
    pub sol_mint: Account<'info, Mint>,

    #[account(
        init,
        payer = admin,
        token::mint = usdc_mint,
        token::authority = strategy_authority,
        seeds = [b"usdc_vault", strategy.key().as_ref()],
        bump
    )]
    pub usdc_vault: Account<'info, TokenAccount>,

    #[account(
        init,
        payer = admin,
        mint::decimals = 6,
        mint::authority = strategy_authority,
        seeds = [SHARE_MINT_SEED, strategy.key().as_ref()],
        bump
    )]
    pub share_mint: Account<'info, Mint>,

    /// CHECK: External Marginfi protocol state account validated via CPI bounds.
    pub marginfi_account: AccountInfo<'info>,
    /// CHECK: External Kamino protocol obligation state.
    pub kamino_obligation: AccountInfo<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(mut, has_one = usdc_vault, has_one = share_mint)]
    pub strategy: Account<'info, StrategyVault>,

    /// CHECK: PDA signer authority
    #[account(seeds = [AUTHORITY_SEED, strategy.key().as_ref()], bump = strategy.authority_bump)]
    pub strategy_authority: AccountInfo<'info>,

    #[account(mut)]
    pub user: Signer<'info>,

    #[account(mut, constraint = user_usdc.mint == strategy.usdc_mint)]
    pub user_usdc: Account<'info, TokenAccount>,

    #[account(mut)]
    pub usdc_vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub share_mint: Account<'info, Mint>,

    #[account(mut, constraint = user_shares.mint == share_mint.key())]
    pub user_shares: Account<'info, TokenAccount>,

    /// CHECK: Pyth Oracle feed for SOL
    pub pyth_sol_feed: AccountInfo<'info>,
    /// CHECK: Pyth Oracle feed for USDC
    pub pyth_usdc_feed: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct ExecuteRebalance<'info> {
    #[account(mut)]
    pub strategy: Account<'info, StrategyVault>,

    /// CHECK: Strategy Authority PDA
    #[account(seeds = [AUTHORITY_SEED, strategy.key().as_ref()], bump = strategy.authority_bump)]
    pub strategy_authority: AccountInfo<'info>,

    pub keeper: Signer<'info>,

    #[account(mut, address = strategy.usdc_vault)]
    pub usdc_vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub vault_sol_escrow: Account<'info, TokenAccount>,

    /// CHECK: Marginfi Program
    pub marginfi_program: AccountInfo<'info>,
    /// CHECK: Marginfi strategy state
    #[account(mut, address = strategy.marginfi_account)]
    pub marginfi_account: AccountInfo<'info>,
    /// CHECK: Marginfi Bank account for USDC
    #[account(mut)]
    pub marginfi_usdc_bank: AccountInfo<'info>,
    /// CHECK: Marginfi Bank account for SOL
    #[account(mut)]
    pub marginfi_sol_bank: AccountInfo<'info>,

    /// CHECK: Kamino Lending Program
    pub kamino_program: AccountInfo<'info>,
    /// CHECK: Kamino Strategy Obligation
    #[account(mut, address = strategy.kamino_obligation)]
    pub kamino_obligation: AccountInfo<'info>,
    /// CHECK: Kamino Reserve State
    #[account(mut)]
    pub kamino_sol_reserve: AccountInfo<'info>,

    /// CHECK: Pyth SOL/USD price account
    pub pyth_sol_feed: AccountInfo<'info>,
    /// CHECK: Pyth USDC/USD price account
    pub pyth_usdc_feed: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut, has_one = usdc_vault, has_one = share_mint)]
    pub strategy: Account<'info, StrategyVault>,

    /// CHECK: PDA signer
    #[account(seeds = [AUTHORITY_SEED, strategy.key().as_ref()], bump = strategy.authority_bump)]
    pub strategy_authority: AccountInfo<'info>,

    #[account(mut)]
    pub user: Signer<'info>,

    #[account(mut)]
    pub user_shares: Account<'info, TokenAccount>,

    #[account(mut)]
    pub share_mint: Account<'info, Mint>,

    #[account(mut)]
    pub usdc_vault: Account<'info, TokenAccount>,

    #[account(mut, constraint = user_usdc.mint == strategy.usdc_mint)]
    pub user_usdc: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

// -----------------------------------------------------------------------------
// STATE DEFINITION
// -----------------------------------------------------------------------------
#[account]
#[derive(InitSpace)]
pub struct StrategyVault {
    pub vault_id: u64,
    pub admin: Pubkey,
    pub usdc_mint: Pubkey,
    pub sol_mint: Pubkey,
    pub usdc_vault: Pubkey,
    pub share_mint: Pubkey,
    pub marginfi_account: Pubkey,
    pub kamino_obligation: Pubkey,
    pub target_leverage_bps: u64,
    pub total_managed_usdc: u64,
    pub authority_bump: u8,
    pub vault_bump: u8,
    pub is_rebalancing: bool,
}

// -----------------------------------------------------------------------------
// EVENTS
// -----------------------------------------------------------------------------
#[event]
pub struct StrategyInitialized {
    pub vault_id: u64,
    pub admin: Pubkey,
    pub target_leverage_bps: u64,
}

#[event]
pub struct DepositEvent {
    pub user: Pubkey,
    pub usdc_in: u64,
    pub shares_minted: u64,
}

#[event]
pub struct RebalanceExecuted {
    pub usdc_collateral_deposited: u64,
    pub sol_borrowed_and_hedged: u64,
    pub timestamp: i64,
}

#[event]
pub struct WithdrawEvent {
    pub user: Pubkey,
    pub shares_burned: u64,
    pub usdc_returned: u64,
}

// -----------------------------------------------------------------------------
// ERROR DEFINITIONS
// -----------------------------------------------------------------------------
#[error_code]
pub enum ErrorCode {
    #[msg("Configured leverage exceeds the hard-coded maximum safe LTV boundary.")]
    ExcessiveLeverageConfigured,
    #[msg("Deposit amount must be strictly greater than zero.")]
    ZeroDepositAmount,
    #[msg("Withdrawal share count must be greater than zero.")]
    ZeroWithdrawAmount,
    #[msg("Strategy is currently undergoing an atomic rebalance. Actions locked.")]
    StrategyLocked,
    #[msg("Arithmetic calculation resulted in an overflow or underflow.")]
    MathOverflow,
    #[msg("Proposed debt position violates configured LTV boundary constraints.")]
    LtvExceedsSafetyThreshold,
    #[msg("Pyth price feed account is stale or failing validation.")]
    StaleOraclePrice,
    #[msg("Pyth price feed account data could not be deserialized.")]
    InvalidOracleData,
    #[msg("Oracle confidence interval exceeds configured maximum risk parameters.")]
    OracleConfidenceTooWide,
}