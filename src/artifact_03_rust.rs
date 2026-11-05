use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("AURAdnr7j8MvY2sE2CjS9rZ57oU8kGkQ3gZ7Y2r1aVR");

pub const BPS_DENOMINATOR: u64 = 10_000;
pub const MAX_LEVERAGE_BPS: u64 = 30_000; // 3.0x Max Leverage
pub const MAX_DELTA_TOLERANCE_BPS: u64 = 250; // 2.5% Delta tolerance
pub const ORACLE_EXPIRY_THRESHOLD_SECS: i64 = 60;

#[program]
pub mod delta_neutral_router {
    use super::*;

    /// Initializes the Master Vault for a designated underlying pair (e.g., SOL-USDC).
    pub fn initialize_vault(
        ctx: Context<InitializeVault>,
        strategy_id: u64,
        target_delta_bps: i64,
        rebalance_threshold_bps: u64,
    ) -> Result<()> {
        require!(
            rebalance_threshold_bps <= MAX_DELTA_TOLERANCE_BPS,
            RouterError::ExcessiveDeltaThreshold
        );

        let vault = &mut ctx.accounts.vault_state;
        vault.authority = ctx.accounts.authority.key();
        vault.keeper = ctx.accounts.keeper.key();
        vault.base_mint = ctx.accounts.base_mint.key();
        vault.quote_mint = ctx.accounts.quote_mint.key();
        vault.shares_mint = ctx.accounts.shares_mint.key();
        vault.vault_base_ata = ctx.accounts.vault_base_ata.key();
        vault.vault_quote_ata = ctx.accounts.vault_quote_ata.key();
        
        vault.strategy_id = strategy_id;
        vault.target_delta_bps = target_delta_bps;
        vault.rebalance_threshold_bps = rebalance_threshold_bps;
        vault.total_vault_shares = 0;
        vault.is_paused = false;
        vault.bump = ctx.bumps.vault_state;

        emit!(VaultInitializedEvent {
            vault: vault.key(),
            authority: vault.authority,
            strategy_id,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// Deposits Quote Assets (e.g. USDC), issuing Vault Shares 1:1 or net-asset-value-proportional.
    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        let vault = &mut ctx.accounts.vault_state;
        require!(!vault.is_paused, RouterError::ProtocolPaused);
        require!(amount > 0, RouterError::ZeroDepositAmount);

        let total_shares = vault.total_vault_shares;
        let total_assets = ctx.accounts.vault_quote_ata.amount;

        let shares_to_mint = if total_shares == 0 || total_assets == 0 {
            amount
        } else {
            (amount as u128)
                .checked_mul(total_shares as u128)
                .ok_or(RouterError::MathOverflow)?
                .checked_div(total_assets as u128)
                .ok_or(RouterError::MathOverflow)? as u64
        };

        // Transfer quote collateral from user to vault
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.user_quote_ata.to_account_info(),
                    to: ctx.accounts.vault_quote_ata.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            amount,
        )?;

        // Mint liquidity receipt shares to depositor
        let strategy_id_bytes = vault.strategy_id.to_le_bytes();
        let seeds = &[
            b"vault_state".as_ref(),
            strategy_id_bytes.as_ref(),
            &[vault.bump],
        ];
        let signer_seeds = &[&seeds[..]];

        token::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                token::MintTo {
                    mint: ctx.accounts.shares_mint.to_account_info(),
                    to: ctx.accounts.user_shares_ata.to_account_info(),
                    authority: vault.to_account_info(),
                },
                signer_seeds,
            ),
            shares_to_mint,
        )?;

        vault.total_vault_shares = vault
            .total_vault_shares
            .checked_add(shares_to_mint)
            .ok_or(RouterError::MathOverflow)?;

        emit!(DepositEvent {
            user: ctx.accounts.user.key(),
            amount,
            shares_issued: shares_to_mint,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// Rebalances Kamino Long and Marginfi Short hedges to retain neutral portfolio delta.
    pub fn rebalance_delta_neutral(
        ctx: Context<RebalanceDeltaNeutral>,
        kamino_allocation: u64,
        marginfi_borrow_target: u64,
        max_base_price_slippage: u64,
        min_base_price_slippage: u64,
    ) -> Result<()> {
        let vault = &mut ctx.accounts.vault_state;
        require!(!vault.is_paused, RouterError::ProtocolPaused);
        require!(
            ctx.accounts.caller.key() == vault.keeper || ctx.accounts.caller.key() == vault.authority,
            RouterError::UnauthorizedKeeper
        );

        // Parse Pyth Network price account securely
        let oracle_price = ctx.accounts.pyth_oracle_feed.to_account_info();
        let oracle_data = oracle_price.try_borrow_data()?;
        let current_price = parse_pyth_price(&oracle_data, Clock::get()?.unix_timestamp)?;

        require!(
            current_price >= min_base_price_slippage && current_price <= max_base_price_slippage,
            RouterError::PriceSlippageExceeded
        );

        let strategy_id_bytes = vault.strategy_id.to_le_bytes();
        let seeds = &[
            b"vault_state".as_ref(),
            strategy_id_bytes.as_ref(),
            &[vault.bump],
        ];
        let signer_seeds = &[&seeds[..]];

        // --- STEP 1: Supply capital into Kamino to generate long yield ---
        if kamino_allocation > 0 {
            let kamino_cpi_accounts = KaminoDepositAccounts {
                lending_market: ctx.accounts.kamino_lending_market.to_account_info(),
                reserve: ctx.accounts.kamino_reserve.to_account_info(),
                reserve_liquidity_supply: ctx.accounts.kamino_reserve_supply.to_account_info(),
                user_source_liquidity: ctx.accounts.vault_quote_ata.to_account_info(),
                user_destination_collateral: ctx.accounts.vault_kamino_collateral.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
                vault_signer: vault.to_account_info(),
            };
            kamino_deposit_cpi(kamino_cpi_accounts, kamino_allocation, signer_seeds)?;
        }

        // --- STEP 2: Borrow base token from Marginfi to synthetically short & offset delta ---
        if marginfi_borrow_target > 0 {
            let marginfi_cpi_accounts = MarginfiBorrowAccounts {
                marginfi_group: ctx.accounts.marginfi_group.to_account_info(),
                marginfi_account: ctx.accounts.marginfi_account.to_account_info(),
                marginfi_bank: ctx.accounts.marginfi_bank.to_account_info(),
                destination_token_account: ctx.accounts.vault_base_ata.to_account_info(),
                bank_liquidity_vault: ctx.accounts.marginfi_bank_vault.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
                vault_signer: vault.to_account_info(),
            };
            marginfi_borrow_cpi(marginfi_cpi_accounts, marginfi_borrow_target, signer_seeds)?;
        }

        // Update strategy position state
        let pos = &mut ctx.accounts.strategy_position;
        pos.kamino_supplied_amount = kamino_allocation;
        pos.marginfi_borrowed_amount = marginfi_borrow_target;
        pos.last_oracle_price = current_price;
        pos.last_rebalance_slot = Clock::get()?.slot;

        emit!(RebalancedEvent {
            vault: vault.key(),
            kamino_supplied: kamino_allocation,
            marginfi_borrowed: marginfi_borrow_target,
            oracle_price: current_price,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// Emergency shutdown toggle to protect depositor principal
    pub fn set_emergency_pause(ctx: Context<ManageVault>, pause: bool) -> Result<()> {
        let vault = &mut ctx.accounts.vault_state;
        vault.is_paused = pause;

        emit!(EmergencyHaltEvent {
            vault: vault.key(),
            is_paused: pause,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }
}

// -----------------------------------------------------------------------------
// ACCOUNT VALIDATION CONTEXTS
// -----------------------------------------------------------------------------

#[derive(Accounts)]
#[instruction(strategy_id: u64)]
pub struct InitializeVault<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + VaultState::INIT_SPACE,
        seeds = [b"vault_state", strategy_id.to_le_bytes().as_ref()],
        bump
    )]
    pub vault_state: Box<Account<'info, VaultState>>,

    pub base_mint: Box<Account<'info, Mint>>,
    pub quote_mint: Box<Account<'info, Mint>>,

    #[account(
        init,
        payer = authority,
        mint::decimals = 6,
        mint::authority = vault_state,
        seeds = [b"shares_mint", vault_state.key().as_ref()],
        bump
    )]
    pub shares_mint: Box<Account<'info, Mint>>,

    #[account(
        init,
        payer = authority,
        associated_token::mint = base_mint,
        associated_token::authority = vault_state
    )]
    pub vault_base_ata: Box<Account<'info, TokenAccount>>,

    #[account(
        init,
        payer = authority,
        associated_token::mint = quote_mint,
        associated_token::authority = vault_state
    )]
    pub vault_quote_ata: Box<Account<'info, TokenAccount>>,

    /// CHECK: Designate keeper bot signer pubkey
    pub keeper: UncheckedAccount<'info>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(
        mut,
        seeds = [b"vault_state", vault_state.strategy_id.to_le_bytes().as_ref()],
        bump = vault_state.bump
    )]
    pub vault_state: Box<Account<'info, VaultState>>,

    #[account(
        mut,
        seeds = [b"shares_mint", vault_state.key().as_ref()],
        bump
    )]
    pub shares_mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        associated_token::mint = vault_state.quote_mint,
        associated_token::authority = vault_state
    )]
    pub vault_quote_ata: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        associated_token::mint = vault_state.quote_mint,
        associated_token::authority = user
    )]
    pub user_quote_ata: Box<Account<'info, TokenAccount>>,

    #[account(
        init_if_needed,
        payer = user,
        associated_token::mint = shares_mint,
        associated_token::authority = user
    )]
    pub user_shares_ata: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub user: Signer<'info>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RebalanceDeltaNeutral<'info> {
    #[account(
        mut,
        seeds = [b"vault_state", vault_state.strategy_id.to_le_bytes().as_ref()],
        bump = vault_state.bump
    )]
    pub vault_state: Box<Account<'info, VaultState>>,

    #[account(
        init_if_needed,
        payer = caller,
        space = 8 + StrategyPosition::INIT_SPACE,
        seeds = [b"strategy_position", vault_state.key().as_ref()],
        bump
    )]
    pub strategy_position: Box<Account<'info, StrategyPosition>>,

    #[account(
        mut,
        associated_token::mint = vault_state.quote_mint,
        associated_token::authority = vault_state
    )]
    pub vault_quote_ata: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        associated_token::mint = vault_state.base_mint,
        associated_token::authority = vault_state
    )]
    pub vault_base_ata: Box<Account<'info, TokenAccount>>,

    /// Pyth Network Pull or Push Price Account
    /// CHECK: Validated inside `parse_pyth_price`
    pub pyth_oracle_feed: UncheckedAccount<'info>,

    // Kamino Specific Accounts
    /// CHECK: External protocol lending market
    pub kamino_lending_market: UncheckedAccount<'info>,
    /// CHECK: External protocol reserve
    #[account(mut)]
    pub kamino_reserve: UncheckedAccount<'info>,
    /// CHECK: Supply vault
    #[account(mut)]
    pub kamino_reserve_supply: UncheckedAccount<'info>,
    /// CHECK: Vault collateral account receiving Kamino cTokens
    #[account(mut)]
    pub vault_kamino_collateral: UncheckedAccount<'info>,

    // Marginfi Specific Accounts
    /// CHECK: Marginfi verification group
    pub marginfi_group: UncheckedAccount<'info>,
    /// CHECK: Marginfi user margin account PDA
    #[account(mut)]
    pub marginfi_account: UncheckedAccount<'info>,
    /// CHECK: Marginfi Bank account for Base token
    #[account(mut)]
    pub marginfi_bank: UncheckedAccount<'info>,
    /// CHECK: Marginfi Bank Liquidity Vault
    #[account(mut)]
    pub marginfi_bank_vault: UncheckedAccount<'info>,

    pub caller: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ManageVault<'info> {
    #[account(
        mut,
        seeds = [b"vault_state", vault_state.strategy_id.to_le_bytes().as_ref()],
        bump = vault_state.bump,
        has_one = authority @ RouterError::UnauthorizedAdmin
    )]
    pub vault_state: Box<Account<'info, VaultState>>,
    pub authority: Signer<'info>,
}

// -----------------------------------------------------------------------------
// STATE SCHEMAS
// -----------------------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct VaultState {
    pub authority: Pubkey,
    pub keeper: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub shares_mint: Pubkey,
    pub vault_base_ata: Pubkey,
    pub vault_quote_ata: Pubkey,
    pub strategy_id: u64,
    pub target_delta_bps: i64,
    pub rebalance_threshold_bps: u64,
    pub total_vault_shares: u64,
    pub is_paused: bool,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct StrategyPosition {
    pub kamino_supplied_amount: u64,
    pub marginfi_borrowed_amount: u64,
    pub last_oracle_price: u64,
    pub last_rebalance_slot: u64,
}

// -----------------------------------------------------------------------------
// CROSS-PROGRAM INVOCATION WRAPPERS
// -----------------------------------------------------------------------------

pub struct KaminoDepositAccounts<'info> {
    pub lending_market: AccountInfo<'info>,
    pub reserve: AccountInfo<'info>,
    pub reserve_liquidity_supply: AccountInfo<'info>,
    pub user_source_liquidity: AccountInfo<'info>,
    pub user_destination_collateral: AccountInfo<'info>,
    pub token_program: AccountInfo<'info>,
    pub vault_signer: AccountInfo<'info>,
}

pub fn kamino_deposit_cpi<'info>(
    accounts: KaminoDepositAccounts<'info>,
    amount: u64,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    // Instruction discriminator for Kamino deposit reserve liquidity
    let mut data = Vec::with_capacity(16);
    data.extend_from_slice(&[242, 35, 198, 137, 82, 225, 242, 182]); // DepositReserveLiquidity discriminator
    data.extend_from_slice(&amount.to_le_bytes());

    let instruction = anchor_lang::solana_program::instruction::Instruction {
        program_id: *accounts.lending_market.owner,
        accounts: vec![
            AccountMeta::new(accounts.user_source_liquidity.key(), false),
            AccountMeta::new(accounts.user_destination_collateral.key(), false),
            AccountMeta::new(accounts.reserve.key(), false),
            AccountMeta::new(accounts.reserve_liquidity_supply.key(), false),
            AccountMeta::new_readonly(accounts.lending_market.key(), false),
            AccountMeta::new_readonly(accounts.vault_signer.key(), true),
            AccountMeta::new_readonly(accounts.token_program.key(), false),
        ],
        data,
    };

    anchor_lang::solana_program::program::invoke_signed(
        &instruction,
        &[
            accounts.user_source_liquidity,
            accounts.user_destination_collateral,
            accounts.reserve,
            accounts.reserve_liquidity_supply,
            accounts.lending_market,
            accounts.vault_signer,
            accounts.token_program,
        ],
        signer_seeds,
    )?;

    Ok(())
}

pub struct MarginfiBorrowAccounts<'info> {
    pub marginfi_group: AccountInfo<'info>,
    pub marginfi_account: AccountInfo<'info>,
    pub marginfi_bank: AccountInfo<'info>,
    pub destination_token_account: AccountInfo<'info>,
    pub bank_liquidity_vault: AccountInfo<'info>,
    pub token_program: AccountInfo<'info>,
    pub vault_signer: AccountInfo<'info>,
}

pub fn marginfi_borrow_cpi<'info>(
    accounts: MarginfiBorrowAccounts<'info>,
    amount: u64,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    // Instruction discriminator for Marginfi account borrow
    let mut data = Vec::with_capacity(16);
    data.extend_from_slice(&[41, 237, 233, 40, 115, 203, 15, 172]); // Marginfi AccountBorrow discriminator
    data.extend_from_slice(&amount.to_le_bytes());

    let instruction = anchor_lang::solana_program::instruction::Instruction {
        program_id: *accounts.marginfi_group.owner,
        accounts: vec![
            AccountMeta::new_readonly(accounts.marginfi_group.key(), false),
            AccountMeta::new(accounts.marginfi_account.key(), false),
            AccountMeta::new_readonly(accounts.vault_signer.key(), true),
            AccountMeta::new(accounts.marginfi_bank.key(), false),
            AccountMeta::new(accounts.destination_token_account.key(), false),
            AccountMeta::new(accounts.bank_liquidity_vault.key(), false),
            AccountMeta::new_readonly(accounts.token_program.key(), false),
        ],
        data,
    };

    anchor_lang::solana_program::program::invoke_signed(
        &instruction,
        &[
            accounts.marginfi_group,
            accounts.marginfi_account,
            accounts.vault_signer,
            accounts.marginfi_bank,
            accounts.destination_token_account,
            accounts.bank_liquidity_vault,
            accounts.token_program,
        ],
        signer_seeds,
    )?;

    Ok(())
}

// -----------------------------------------------------------------------------
// ORACLE VALIDATION HELPERS
// -----------------------------------------------------------------------------

pub fn parse_pyth_price(data: &[u8], current_timestamp: i64) -> Result<u64> {
    require!(data.len() >= 64, RouterError::InvalidOracleAccount);
    
    // Pyth binary payload layout: offset 208 contains price (i64), offset 216 contains conf (u64), offset 224 contains publish_time (i64)
    let price_bytes = data.get(208..216).ok_or(RouterError::InvalidOracleAccount)?;
    let timestamp_bytes = data.get(224..232).ok_or(RouterError::InvalidOracleAccount)?;

    let price = i64::from_le_bytes(price_bytes.try_into().unwrap());
    let publish_time = i64::from_le_bytes(timestamp_bytes.try_into().unwrap());

    require!(price > 0, RouterError::NegativePriceObserved);
    require!(
        current_timestamp - publish_time <= ORACLE_EXPIRY_THRESHOLD_SECS,
        RouterError::StaleOracleFeed
    );

    Ok(price as u64)
}

// -----------------------------------------------------------------------------
// EVENTS & ERRORS
// -----------------------------------------------------------------------------

#[event]
pub struct VaultInitializedEvent {
    pub vault: Pubkey,
    pub authority: Pubkey,
    pub strategy_id: u64,
    pub timestamp: i64,
}

#[event]
pub struct DepositEvent {
    pub user: Pubkey,
    pub amount: u64,
    pub shares_issued: u64,
    pub timestamp: i64,
}

#[event]
pub struct RebalancedEvent {
    pub vault: Pubkey,
    pub kamino_supplied: u64,
    pub marginfi_borrowed: u64,
    pub oracle_price: u64,
    pub timestamp: i64,
}

#[event]
pub struct EmergencyHaltEvent {
    pub vault: Pubkey,
    pub is_paused: bool,
    pub timestamp: i64,
}

#[error_code]
pub enum RouterError {
    #[msg("Protocol is currently halted by emergency pause")]
    ProtocolPaused,
    #[msg("Deposit amount must be strictly positive")]
    ZeroDepositAmount,
    #[msg("Rebalance threshold exceeds permissible system maximum")]
    ExcessiveDeltaThreshold,
    #[msg("Caller lacks authorized keeper permissions")]
    UnauthorizedKeeper,
    #[msg("Caller is not the designated admin authority")]
    UnauthorizedAdmin,
    #[msg("Execution price violates strict slippage parameters")]
    PriceSlippageExceeded,
    #[msg("Pyth oracle data account is corrupt or malformed")]
    InvalidOracleAccount,
    #[msg("Pyth oracle reported negative price data")]
    NegativePriceObserved,
    #[msg("Oracle price feed timestamp exceeds staleness threshold")]
    StaleOracleFeed,
    #[msg("Math arithmetic overflow")]
    MathOverflow,
}