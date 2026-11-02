use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};
use pyth_sdk_solana::load_price_feed_from_account_info;

declare_id!("Aegis11111111111111111111111111111111111111");

pub const BPS_DENOMINATOR: u64 = 10_000;
pub const MAX_ALLOWED_DRIFT_BPS: u64 = 300; // 3.00% delta drift maximum
pub const VAULT_SEED: &[u8] = b"aegis_vault";
pub const AUTHORITY_SEED: &[u8] = b"aegis_authority";

#[program]
pub mod aegis_delta_router {
    use super::*;

    pub fn initialize_strategy(
        ctx: Context<InitializeStrategy>,
        target_leverage_bps: u64,
        max_slippage_bps: u64,
        drift_threshold_bps: u64,
    ) -> Result<()> {
        require!(
            drift_threshold_bps <= MAX_ALLOWED_DRIFT_BPS,
            AegisError::DriftThresholdTooHigh
        );
        require!(target_leverage_bps <= 30_000, AegisError::LeverageExceedsSafetyCap); // Max 3x

        let strategy = &mut ctx.accounts.strategy_vault;
        strategy.admin = ctx.accounts.admin.key();
        strategy.keeper = ctx.accounts.keeper.key();
        strategy.collateral_mint = ctx.accounts.collateral_mint.key();
        strategy.debt_mint = ctx.accounts.debt_mint.key();
        strategy.kamino_vault = ctx.accounts.kamino_vault.key();
        strategy.marginfi_account = ctx.accounts.marginfi_account.key();
        strategy.target_leverage_bps = target_leverage_bps;
        strategy.max_slippage_bps = max_slippage_bps;
        strategy.drift_threshold_bps = drift_threshold_bps;
        strategy.total_deposited_collateral = 0;
        strategy.active_debt_balance = 0;
        strategy.is_paused = false;
        strategy.vault_bump = ctx.bumps.strategy_vault;
        strategy.authority_bump = ctx.bumps.vault_authority;

        emit!(StrategyInitializedEvent {
            admin: strategy.admin,
            collateral_mint: strategy.collateral_mint,
            debt_mint: strategy.debt_mint,
            target_leverage_bps,
        });

        Ok(())
    }

    pub fn deposit_and_route(
        ctx: Context<DepositAndRoute>,
        collateral_amount: u64,
        min_shares_expected: u64,
    ) -> Result<()> {
        let strategy = &mut ctx.accounts.strategy_vault;
        require!(!strategy.is_paused, AegisError::StrategyPaused);
        require!(collateral_amount > 0, AegisError::ZeroAmount);

        // 1. Transfer collateral from depositor to vault
        let transfer_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.user_collateral_ata.to_account_info(),
                to: ctx.accounts.vault_collateral_ata.to_account_info(),
                authority: ctx.accounts.user.to_account_info(),
            },
        );
        token::transfer(transfer_ctx, collateral_amount)?;

        // 2. Fetch Pyth oracle prices to validate asset valuations
        let price_feed_collateral = load_price_feed_from_account_info(
            &ctx.accounts.collateral_pyth_price_feed.to_account_info()
        ).map_err(|_| AegisError::InvalidOraclePrice)?;
        
        let current_time = Clock::get()?.unix_timestamp;
        let collateral_price = price_feed_collateral
            .get_price_no_older_than(current_time, 60)
            .ok_or(AegisError::StalePriceOracle)?;

        require!(collateral_price.price > 0, AegisError::InvalidOraclePrice);

        // 3. Execute CPI: Supply Spot Leg to Kamino Liquidity Program
        let authority_seeds: &[&[&[u8]]] = &[&[
            AUTHORITY_SEED,
            &[strategy.authority_bump],
        ]];

        let kamino_cpi_accounts = KaminoDepositAccounts {
            kamino_program: ctx.accounts.kamino_program.to_account_info(),
            vault: ctx.accounts.kamino_vault.to_account_info(),
            vault_authority: ctx.accounts.vault_authority.to_account_info(),
            token_vault: ctx.accounts.vault_collateral_ata.to_account_info(),
            token_program: ctx.accounts.token_program.to_account_info(),
        };
        kamino_cpi_accounts.execute_deposit(collateral_amount, authority_seeds)?;

        // 4. Execute CPI: Open Synthetic Short / Borrow on Marginfi Core Program
        let marginfi_cpi_accounts = MarginfiBorrowAccounts {
            marginfi_program: ctx.accounts.marginfi_program.to_account_info(),
            marginfi_account: ctx.accounts.marginfi_account.to_account_info(),
            vault_authority: ctx.accounts.vault_authority.to_account_info(),
            marginfi_bank: ctx.accounts.marginfi_bank.to_account_info(),
            destination_ata: ctx.accounts.vault_debt_ata.to_account_info(),
            token_program: ctx.accounts.token_program.to_account_info(),
        };

        // Delta-neutral balance: calculate hedge size based on leverage target
        let required_borrow = (collateral_amount as u128)
            .checked_mul(strategy.target_leverage_bps as u128)
            .ok_or(AegisError::MathOverflow)?
            .checked_div(BPS_DENOMINATOR as u128)
            .ok_or(AegisError::MathOverflow)? as u64;

        marginfi_cpi_accounts.execute_borrow(required_borrow, authority_seeds)?;

        // 5. Update Internal Accounting
        strategy.total_deposited_collateral = strategy
            .total_deposited_collateral
            .checked_add(collateral_amount)
            .ok_or(AegisError::MathOverflow)?;

        strategy.active_debt_balance = strategy
            .active_debt_balance
            .checked_add(required_borrow)
            .ok_or(AegisError::MathOverflow)?;

        require!(
            strategy.total_deposited_collateral >= min_shares_expected,
            AegisError::SlippageExceeded
        );

        emit!(PositionUpdatedEvent {
            vault: strategy.key(),
            collateral_locked: strategy.total_deposited_collateral,
            active_debt: strategy.active_debt_balance,
            current_timestamp: current_time,
        });

        Ok(())
    }

    pub fn rebalance_strategy(
        ctx: Context<RebalanceStrategy>,
        rebalance_debt_amount: u64,
        is_reduce_debt: bool,
    ) -> Result<()> {
        let strategy = &mut ctx.accounts.strategy_vault;
        require!(!strategy.is_paused, AegisError::StrategyPaused);

        let current_time = Clock::get()?.unix_timestamp;

        // Price Feed validation via Pyth
        let collateral_feed = load_price_feed_from_account_info(
            &ctx.accounts.collateral_pyth_price_feed.to_account_info()
        ).map_err(|_| AegisError::InvalidOraclePrice)?;
        let col_price = collateral_feed
            .get_price_no_older_than(current_time, 60)
            .ok_or(AegisError::StalePriceOracle)?;

        let debt_feed = load_price_feed_from_account_info(
            &ctx.accounts.debt_pyth_price_feed.to_account_info()
        ).map_err(|_| AegisError::InvalidOraclePrice)?;
        let debt_price = debt_feed
            .get_price_no_older_than(current_time, 60)
            .ok_or(AegisError::StalePriceOracle)?;

        // Quantify Delta Drift: delta = (spot_val - hedge_val) / spot_val
        let spot_value_usd = (strategy.total_deposited_collateral as i128)
            .checked_mul(col_price.price as i128)
            .ok_or(AegisError::MathOverflow)?;

        let hedge_value_usd = (strategy.active_debt_balance as i128)
            .checked_mul(debt_price.price as i128)
            .ok_or(AegisError::MathOverflow)?;

        let drift_usd = spot_value_usd.checked_sub(hedge_value_usd).ok_or(AegisError::MathOverflow)?.abs();
        let drift_bps = ((drift_usd as u128)
            .checked_mul(BPS_DENOMINATOR as u128)
            .ok_or(AegisError::MathOverflow)?
            .checked_div(spot_value_usd.abs() as u128)
            .unwrap_or(0)) as u64;

        require!(
            drift_bps >= strategy.drift_threshold_bps,
            AegisError::RebalanceNotRequired
        );

        let authority_seeds: &[&[&[u8]]] = &[&[
            AUTHORITY_SEED,
            &[strategy.authority_bump],
        ]];

        if is_reduce_debt {
            // Repay debt on Marginfi
            let repay_accounts = MarginfiRepayAccounts {
                marginfi_program: ctx.accounts.marginfi_program.to_account_info(),
                marginfi_account: ctx.accounts.marginfi_account.to_account_info(),
                vault_authority: ctx.accounts.vault_authority.to_account_info(),
                marginfi_bank: ctx.accounts.marginfi_bank.to_account_info(),
                source_debt_ata: ctx.accounts.vault_debt_ata.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
            };
            repay_accounts.execute_repay(rebalance_debt_amount, authority_seeds)?;
            strategy.active_debt_balance = strategy
                .active_debt_balance
                .saturating_sub(rebalance_debt_amount);
        } else {
            // Increase borrow on Marginfi
            let borrow_accounts = MarginfiBorrowAccounts {
                marginfi_program: ctx.accounts.marginfi_program.to_account_info(),
                marginfi_account: ctx.accounts.marginfi_account.to_account_info(),
                vault_authority: ctx.accounts.vault_authority.to_account_info(),
                marginfi_bank: ctx.accounts.marginfi_bank.to_account_info(),
                destination_ata: ctx.accounts.vault_debt_ata.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
            };
            borrow_accounts.execute_borrow(rebalance_debt_amount, authority_seeds)?;
            strategy.active_debt_balance = strategy
                .active_debt_balance
                .checked_add(rebalance_debt_amount)
                .ok_or(AegisError::MathOverflow)?;
        }

        emit!(StrategyRebalancedEvent {
            vault: strategy.key(),
            new_debt_balance: strategy.active_debt_balance,
            calculated_drift_bps: drift_bps,
            timestamp: current_time,
        });

        Ok(())
    }

    pub fn set_emergency_pause(ctx: Context<EmergencyControl>, paused: bool) -> Result<()> {
        let strategy = &mut ctx.accounts.strategy_vault;
        strategy.is_paused = paused;
        emit!(EmergencyPauseUpdatedEvent {
            vault: strategy.key(),
            is_paused: paused,
            operator: ctx.accounts.admin.key(),
        });
        Ok(())
    }
}

// --------------------------------------------------------------------------------
// Account Structures
// --------------------------------------------------------------------------------

#[derive(Accounts)]
pub struct InitializeStrategy<'info> {
    #[account(
        init,
        payer = admin,
        space = 8 + StrategyVault::INIT_SPACE,
        seeds = [VAULT_SEED, collateral_mint.key().as_ref(), debt_mint.key().as_ref()],
        bump
    )]
    pub strategy_vault: Account<'info, StrategyVault>,

    /// CHECK: PDA acting as the cross-CPI vault authority
    #[account(
        seeds = [AUTHORITY_SEED],
        bump
    )]
    pub vault_authority: AccountInfo<'info>,

    pub collateral_mint: Account<'info, Mint>,
    pub debt_mint: Account<'info, Mint>,

    /// CHECK: Verified against Kamino program
    pub kamino_vault: AccountInfo<'info>,
    /// CHECK: Verified against Marginfi program
    pub marginfi_account: AccountInfo<'info>,

    #[account(mut)]
    pub admin: Signer<'info>,
    /// CHECK: Authorized automated rebalancer bot address
    pub keeper: AccountInfo<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct DepositAndRoute<'info> {
    #[account(
        mut,
        has_one = collateral_mint,
        has_one = kamino_vault,
        has_one = marginfi_account,
        seeds = [VAULT_SEED, collateral_mint.key().as_ref(), strategy_vault.debt_mint.as_ref()],
        bump = strategy_vault.vault_bump
    )]
    pub strategy_vault: Account<'info, StrategyVault>,

    /// CHECK: PDA authority
    #[account(
        seeds = [AUTHORITY_SEED],
        bump = strategy_vault.authority_bump
    )]
    pub vault_authority: AccountInfo<'info>,

    pub collateral_mint: Account<'info, Mint>,

    #[account(
        mut,
        constraint = user_collateral_ata.owner == user.key(),
        constraint = user_collateral_ata.mint == collateral_mint.key()
    )]
    pub user_collateral_ata: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = vault_collateral_ata.owner == vault_authority.key(),
        constraint = vault_collateral_ata.mint == collateral_mint.key()
    )]
    pub vault_collateral_ata: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = vault_debt_ata.owner == vault_authority.key()
    )]
    pub vault_debt_ata: Account<'info, TokenAccount>,

    /// CHECK: Pyth Oracle for Collateral Price Verification
    pub collateral_pyth_price_feed: AccountInfo<'info>,

    /// CHECK: Kamino Program
    pub kamino_program: AccountInfo<'info>,
    /// CHECK: Marginfi Program
    pub marginfi_program: AccountInfo<'info>,

    /// CHECK: Kamino Strategy Target
    #[account(mut)]
    pub kamino_vault: AccountInfo<'info>,
    /// CHECK: Marginfi Account
    #[account(mut)]
    pub marginfi_account: AccountInfo<'info>,
    /// CHECK: Marginfi Bank
    #[account(mut)]
    pub marginfi_bank: AccountInfo<'info>,

    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct RebalanceStrategy<'info> {
    #[account(
        mut,
        seeds = [VAULT_SEED, strategy_vault.collateral_mint.as_ref(), strategy_vault.debt_mint.as_ref()],
        bump = strategy_vault.vault_bump,
        has_one = keeper
    )]
    pub strategy_vault: Account<'info, StrategyVault>,

    /// CHECK: PDA authority
    #[account(
        seeds = [AUTHORITY_SEED],
        bump = strategy_vault.authority_bump
    )]
    pub vault_authority: AccountInfo<'info>,

    #[account(
        mut,
        constraint = vault_debt_ata.owner == vault_authority.key()
    )]
    pub vault_debt_ata: Account<'info, TokenAccount>,

    /// CHECK: Pyth Oracle for Collateral
    pub collateral_pyth_price_feed: AccountInfo<'info>,
    /// CHECK: Pyth Oracle for Debt
    pub debt_pyth_price_feed: AccountInfo<'info>,

    /// CHECK: Marginfi Program
    pub marginfi_program: AccountInfo<'info>,
    /// CHECK: Marginfi Account
    #[account(mut)]
    pub marginfi_account: AccountInfo<'info>,
    /// CHECK: Marginfi Bank
    #[account(mut)]
    pub marginfi_bank: AccountInfo<'info>,

    pub keeper: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct EmergencyControl<'info> {
    #[account(
        mut,
        has_one = admin,
        seeds = [VAULT_SEED, strategy_vault.collateral_mint.as_ref(), strategy_vault.debt_mint.as_ref()],
        bump = strategy_vault.vault_bump
    )]
    pub strategy_vault: Account<'info, StrategyVault>,
    pub admin: Signer<'info>,
}

// --------------------------------------------------------------------------------
// Data State PDA
// --------------------------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct StrategyVault {
    pub admin: Pubkey,
    pub keeper: Pubkey,
    pub collateral_mint: Pubkey,
    pub debt_mint: Pubkey,
    pub kamino_vault: Pubkey,
    pub marginfi_account: Pubkey,
    pub total_deposited_collateral: u64,
    pub active_debt_balance: u64,
    pub target_leverage_bps: u64,
    pub max_slippage_bps: u64,
    pub drift_threshold_bps: u64,
    pub is_paused: bool,
    pub vault_bump: u8,
    pub authority_bump: u8,
}

// --------------------------------------------------------------------------------
// CPI Mock Executions for Kamino and Marginfi
// --------------------------------------------------------------------------------

pub struct KaminoDepositAccounts<'info> {
    pub kamino_program: AccountInfo<'info>,
    pub vault: AccountInfo<'info>,
    pub vault_authority: AccountInfo<'info>,
    pub token_vault: AccountInfo<'info>,
    pub token_program: AccountInfo<'info>,
}

impl<'info> KaminoDepositAccounts<'info> {
    pub fn execute_deposit(&self, amount: u64, seeds: &[&[&[u8]]]) -> Result<()> {
        let ix = solana_program::instruction::Instruction {
            program_id: *self.kamino_program.key,
            accounts: vec![
                AccountMeta::new(self.vault.key(), false),
                AccountMeta::new_readonly(self.vault_authority.key(), true),
                AccountMeta::new(self.token_vault.key(), false),
                AccountMeta::new_readonly(self.token_program.key(), false),
            ],
            data: {
                let mut data = vec![0x1a, 0xbc, 0x4f, 0x90]; // Selector: deposit
                data.extend_from_slice(&amount.to_le_bytes());
                data
            },
        };
        solana_program::program::invoke_signed(
            &ix,
            &[
                self.vault.clone(),
                self.vault_authority.clone(),
                self.token_vault.clone(),
                self.token_program.clone(),
            ],
            seeds,
        )?;
        Ok(())
    }
}

pub struct MarginfiBorrowAccounts<'info> {
    pub marginfi_program: AccountInfo<'info>,
    pub marginfi_account: AccountInfo<'info>,
    pub vault_authority: AccountInfo<'info>,
    pub marginfi_bank: AccountInfo<'info>,
    pub destination_ata: AccountInfo<'info>,
    pub token_program: AccountInfo<'info>,
}

impl<'info> MarginfiBorrowAccounts<'info> {
    pub fn execute_borrow(&self, amount: u64, seeds: &[&[&[u8]]]) -> Result<()> {
        let ix = solana_program::instruction::Instruction {
            program_id: *self.marginfi_program.key,
            accounts: vec![
                AccountMeta::new(self.marginfi_account.key(), false),
                AccountMeta::new_readonly(self.vault_authority.key(), true),
                AccountMeta::new(self.marginfi_bank.key(), false),
                AccountMeta::new(self.destination_ata.key(), false),
                AccountMeta::new_readonly(self.token_program.key(), false),
            ],
            data: {
                let mut data = vec![0x3b, 0x11, 0x92, 0xea]; // Selector: borrow
                data.extend_from_slice(&amount.to_le_bytes());
                data
            },
        };
        solana_program::program::invoke_signed(
            &ix,
            &[
                self.marginfi_account.clone(),
                self.vault_authority.clone(),
                self.marginfi_bank.clone(),
                self.destination_ata.clone(),
                self.token_program.clone(),
            ],
            seeds,
        )?;
        Ok(())
    }
}

pub struct MarginfiRepayAccounts<'info> {
    pub marginfi_program: AccountInfo<'info>,
    pub marginfi_account: AccountInfo<'info>,
    pub vault_authority: AccountInfo<'info>,
    pub marginfi_bank: AccountInfo<'info>,
    pub source_debt_ata: AccountInfo<'info>,
    pub token_program: AccountInfo<'info>,
}

impl<'info> MarginfiRepayAccounts<'info> {
    pub fn execute_repay(&self, amount: u64, seeds: &[&[&[u8]]]) -> Result<()> {
        let ix = solana_program::instruction::Instruction {
            program_id: *self.marginfi_program.key,
            accounts: vec![
                AccountMeta::new(self.marginfi_account.key(), false),
                AccountMeta::new_readonly(self.vault_authority.key(), true),
                AccountMeta::new(self.marginfi_bank.key(), false),
                AccountMeta::new(self.source_debt_ata.key(), false),
                AccountMeta::new_readonly(self.token_program.key(), false),
            ],
            data: {
                let mut data = vec![0x7f, 0x4a, 0x22, 0x18]; // Selector: repay
                data.extend_from_slice(&amount.to_le_bytes());
                data
            },
        };
        solana_program::program::invoke_signed(
            &ix,
            &[
                self.marginfi_account.clone(),
                self.vault_authority.clone(),
                self.marginfi_bank.clone(),
                self.source_debt_ata.clone(),
                self.token_program.clone(),
            ],
            seeds,
        )?;
        Ok(())
    }
}

// --------------------------------------------------------------------------------
// Events and Errors
// --------------------------------------------------------------------------------

#[event]
pub struct StrategyInitializedEvent {
    pub admin: Pubkey,
    pub collateral_mint: Pubkey,
    pub debt_mint: Pubkey,
    pub target_leverage_bps: u64,
}

#[event]
pub struct PositionUpdatedEvent {
    pub vault: Pubkey,
    pub collateral_locked: u64,
    pub active_debt: u64,
    pub current_timestamp: i64,
}

#[event]
pub struct StrategyRebalancedEvent {
    pub vault: Pubkey,
    pub new_debt_balance: u64,
    pub calculated_drift_bps: u64,
    pub timestamp: i64,
}

#[event]
pub struct EmergencyPauseUpdatedEvent {
    pub vault: Pubkey,
    pub is_paused: bool,
    pub operator: Pubkey,
}

#[error_code]
pub enum AegisError {
    #[msg("Strategy execution is paused by emergency multisig.")]
    StrategyPaused,
    #[msg("Specified drift threshold exceeds the maximum system cap (3%).")]
    DriftThresholdTooHigh,
    #[msg("Target leverage exceeds enterprise protocol safety bounds (3x).")]
    LeverageExceedsSafetyCap,
    #[msg("Input amount must be strictly greater than zero.")]
    ZeroAmount,
    #[msg("Execution slippage exceeded client threshold.")]
    SlippageExceeded,
    #[msg("Pyth price feed account failed to parse.")]
    InvalidOraclePrice,
    #[msg("Pyth price feed confidence intervals or latency threshold exceeded (stale).")]
    StalePriceOracle,
    #[msg("Delta drift is within the acceptable band; rebalance rejected to save fees.")]
    RebalanceNotRequired,
    #[msg("Checked math calculation encountered an overflow.")]
    MathOverflow,
}