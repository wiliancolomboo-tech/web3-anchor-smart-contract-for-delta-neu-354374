use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::Instruction,
    program::{invoke, invoke_signed},
};
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("DNRouter111111111111111111111111111111111");

pub const VAULT_SEED: &[u8] = b"vault";
pub const AUTHORITY_SEED: &[u8] = b"authority";
pub const SHARE_MINT_SEED: &[u8] = b"shares";
pub const BPS_DENOMINATOR: u128 = 10_000;
pub const MAX_LEVERAGE_BPS: u64 = 30_000; // 3.0x Max Leverage
pub const MIN_HEALTH_BUFFER_BPS: u64 = 1_500; // 15% liquidation buffer

// Program IDs for CPI targets (Mainnet addresses)
pub mod external_programs {
    use anchor_lang::prelude::declare_id;
    pub mod marginfi {
        use super::*;
        declare_id!("MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA");
    }
    pub mod kamino {
        use super::*;
        declare_id!("KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD");
    }
}

#[program]
pub mod delta_neutral_router {
    use super::*;

    pub fn initialize_vault(
        ctx: Context<InitializeVault>,
        rebalance_threshold_bps: u16,
        target_leverage_bps: u16,
    ) -> Result<()> {
        require!(
            target_leverage_bps as u64 <= MAX_LEVERAGE_BPS,
            RouterError::ExcessiveLeverageConfigured
        );

        let vault = &mut ctx.accounts.vault;
        vault.admin = ctx.accounts.admin.key();
        vault.asset_mint = ctx.accounts.asset_mint.key();
        vault.share_mint = ctx.accounts.share_mint.key();
        vault.vault_asset_account = ctx.accounts.vault_asset_account.key();
        vault.rebalance_threshold_bps = rebalance_threshold_bps;
        vault.target_leverage_bps = target_leverage_bps;
        vault.total_underlying_deposited = 0;
        vault.is_paused = false;
        vault.bump = ctx.bumps.vault;
        vault.auth_bump = ctx.bumps.vault_authority;

        emit!(VaultInitializedEvent {
            admin: vault.admin,
            asset_mint: vault.asset_mint,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        require!(!ctx.accounts.vault.is_paused, RouterError::ProgramPaused);
        require!(amount > 0, RouterError::ZeroDepositAmount);

        let vault = &ctx.accounts.vault;
        let total_assets = vault.total_underlying_deposited;
        let total_shares = ctx.accounts.share_mint.supply;

        let shares_to_mint = if total_shares == 0 || total_assets == 0 {
            amount
        } else {
            (amount as u128)
                .checked_mul(total_shares as u128)
                .ok_or(RouterError::MathOverflow)?
                .checked_div(total_assets as u128)
                .ok_or(RouterError::MathOverflow)? as u64
        };

        require!(shares_to_mint > 0, RouterError::ZeroSharesCalculated);

        // 1. Transfer user asset to vault token account
        let transfer_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.user_token_account.to_account_info(),
                to: ctx.accounts.vault_asset_account.to_account_info(),
                authority: ctx.accounts.user.to_account_info(),
            },
        );
        token::transfer(transfer_ctx, amount)?;

        // 2. Mint share tokens to user
        let asset_key = vault.asset_mint.key();
        let auth_seeds = &[
            VAULT_SEED,
            asset_key.as_ref(),
            AUTHORITY_SEED,
            &[vault.auth_bump],
        ];
        let signer_seeds = &[&auth_seeds[..]];

        let mint_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            token::MintTo {
                mint: ctx.accounts.share_mint.to_account_info(),
                to: ctx.accounts.user_share_account.to_account_info(),
                authority: ctx.accounts.vault_authority.to_account_info(),
            },
            signer_seeds,
        );
        token::mint_to(mint_ctx, shares_to_mint)?;

        let vault_mut = &mut ctx.accounts.vault;
        vault_mut.total_underlying_deposited = vault_mut
            .total_underlying_deposited
            .checked_add(amount)
            .ok_or(RouterError::MathOverflow)?;

        emit!(DepositEvent {
            depositor: ctx.accounts.user.key(),
            amount,
            shares_minted: shares_to_mint,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    pub fn rebalance_delta_neutral(
        ctx: Context<RebalanceRoute>,
        kamino_allocation_amount: u64,
        marginfi_hedge_borrow_amount: u64,
        min_net_yield_bps: u16,
    ) -> Result<()> {
        let vault = &ctx.accounts.vault;
        require!(!vault.is_paused, RouterError::ProgramPaused);
        require!(
            ctx.accounts.keeper.key() == vault.admin,
            RouterError::UnauthorizedKeeper
        );

        let asset_key = vault.asset_mint.key();
        let auth_seeds = &[
            VAULT_SEED,
            asset_key.as_ref(),
            AUTHORITY_SEED,
            &[vault.auth_bump],
        ];
        let signer_seeds = &[&auth_seeds[..]];

        // 1. Cross-Program Invocation to Kamino Lending: Deposit Collateral / Supply
        if kamino_allocation_amount > 0 {
            // Build Kamino Deposit Reserve Liquidity instruction data
            // Discriminator for Kamino Lending deposit (instruction index 14)
            let mut data = Vec::with_capacity(9);
            data.push(14u8);
            data.extend_from_slice(&kamino_allocation_amount.to_le_bytes());

            let kamino_ix = Instruction {
                program_id: ctx.accounts.kamino_program.key(),
                accounts: vec![
                    AccountMeta::new(ctx.accounts.vault_asset_account.key(), false),
                    AccountMeta::new(ctx.accounts.kamino_reserve_liquidity_supply.key(), false),
                    AccountMeta::new(ctx.accounts.kamino_reserve.key(), false),
                    AccountMeta::new(ctx.accounts.kamino_lending_market.key(), false),
                    AccountMeta::new_readonly(ctx.accounts.kamino_lending_market_authority.key(), false),
                    AccountMeta::new(ctx.accounts.kamino_reserve_collateral_mint.key(), false),
                    AccountMeta::new(ctx.accounts.vault_kamino_collateral.key(), false),
                    AccountMeta::new_readonly(ctx.accounts.vault_authority.key(), true),
                    AccountMeta::new_readonly(ctx.accounts.token_program.key(), false),
                ],
                data,
            };

            invoke_signed(
                &kamino_ix,
                &[
                    ctx.accounts.vault_asset_account.to_account_info(),
                    ctx.accounts.kamino_reserve_liquidity_supply.to_account_info(),
                    ctx.accounts.kamino_reserve.to_account_info(),
                    ctx.accounts.kamino_lending_market.to_account_info(),
                    ctx.accounts.kamino_lending_market_authority.to_account_info(),
                    ctx.accounts.kamino_reserve_collateral_mint.to_account_info(),
                    ctx.accounts.vault_kamino_collateral.to_account_info(),
                    ctx.accounts.vault_authority.to_account_info(),
                    ctx.accounts.token_program.to_account_info(),
                ],
                signer_seeds,
            )?;
        }

        // 2. Cross-Program Invocation to Marginfi v2: Deposit Marginfi & Borrow Offsetting Leg
        if marginfi_hedge_borrow_amount > 0 {
            // Marginfi instruction 0: LendingAccountDeposit
            let mut deposit_data = Vec::with_capacity(9);
            deposit_data.push(0u8);
            deposit_data.extend_from_slice(&(kamino_allocation_amount / 2).to_le_bytes());

            let marginfi_deposit_ix = Instruction {
                program_id: ctx.accounts.marginfi_program.key(),
                accounts: vec![
                    AccountMeta::new_readonly(ctx.accounts.marginfi_group.key(), false),
                    AccountMeta::new(ctx.accounts.marginfi_account.key(), false),
                    AccountMeta::new_readonly(ctx.accounts.vault_authority.key(), true),
                    AccountMeta::new(ctx.accounts.vault_asset_account.key(), false),
                    AccountMeta::new(ctx.accounts.marginfi_bank_vault.key(), false),
                    AccountMeta::new_readonly(ctx.accounts.token_program.key(), false),
                ],
                data: deposit_data,
            };

            invoke_signed(
                &marginfi_deposit_ix,
                &[
                    ctx.accounts.marginfi_group.to_account_info(),
                    ctx.accounts.marginfi_account.to_account_info(),
                    ctx.accounts.vault_authority.to_account_info(),
                    ctx.accounts.vault_asset_account.to_account_info(),
                    ctx.accounts.marginfi_bank_vault.to_account_info(),
                    ctx.accounts.token_program.to_account_info(),
                ],
                signer_seeds,
            )?;

            // Marginfi instruction 1: LendingAccountBorrow (Delta Hedge leg)
            let mut borrow_data = Vec::with_capacity(9);
            borrow_data.push(1u8);
            borrow_data.extend_from_slice(&marginfi_hedge_borrow_amount.to_le_bytes());

            let marginfi_borrow_ix = Instruction {
                program_id: ctx.accounts.marginfi_program.key(),
                accounts: vec![
                    AccountMeta::new_readonly(ctx.accounts.marginfi_group.key(), false),
                    AccountMeta::new(ctx.accounts.marginfi_account.key(), false),
                    AccountMeta::new_readonly(ctx.accounts.vault_authority.key(), true),
                    AccountMeta::new(ctx.accounts.marginfi_bank.key(), false),
                    AccountMeta::new(ctx.accounts.vault_hedge_account.key(), false),
                    AccountMeta::new(ctx.accounts.marginfi_bank_vault.key(), false),
                    AccountMeta::new_readonly(ctx.accounts.token_program.key(), false),
                ],
                data: borrow_data,
            };

            invoke_signed(
                &marginfi_borrow_ix,
                &[
                    ctx.accounts.marginfi_group.to_account_info(),
                    ctx.accounts.marginfi_account.to_account_info(),
                    ctx.accounts.vault_authority.to_account_info(),
                    ctx.accounts.marginfi_bank.to_account_info(),
                    ctx.accounts.vault_hedge_account.to_account_info(),
                    ctx.accounts.marginfi_bank_vault.to_account_info(),
                    ctx.accounts.token_program.to_account_info(),
                ],
                signer_seeds,
            )?;
        }

        emit!(RebalanceExecutedEvent {
            kamino_supplied: kamino_allocation_amount,
            marginfi_hedged: marginfi_hedge_borrow_amount,
            expected_net_yield_bps: min_net_yield_bps,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    pub fn withdraw(ctx: Context<Withdraw>, shares_in: u64) -> Result<()> {
        require!(shares_in > 0, RouterError::ZeroWithdrawShares);

        let vault = &ctx.accounts.vault;
        let total_shares = ctx.accounts.share_mint.supply;
        let total_assets = vault.total_underlying_deposited;

        require!(total_shares > 0, RouterError::NoActiveLiquidity);

        let gross_asset_amount = (shares_in as u128)
            .checked_mul(total_assets as u128)
            .ok_or(RouterError::MathOverflow)?
            .checked_div(total_shares as u128)
            .ok_or(RouterError::MathOverflow)? as u64;

        require!(gross_asset_amount > 0, RouterError::ZeroWithdrawAmount);

        // 1. Burn user shares
        let burn_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            token::Burn {
                mint: ctx.accounts.share_mint.to_account_info(),
                from: ctx.accounts.user_share_account.to_account_info(),
                authority: ctx.accounts.user.to_account_info(),
            },
        );
        token::burn(burn_ctx, shares_in)?;

        // 2. Transfer asset to user from vault asset buffer
        let asset_key = vault.asset_mint.key();
        let auth_seeds = &[
            VAULT_SEED,
            asset_key.as_ref(),
            AUTHORITY_SEED,
            &[vault.auth_bump],
        ];
        let signer_seeds = &[&auth_seeds[..]];

        let transfer_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.vault_asset_account.to_account_info(),
                to: ctx.accounts.user_token_account.to_account_info(),
                authority: ctx.accounts.vault_authority.to_account_info(),
            },
            signer_seeds,
        );
        token::transfer(transfer_ctx, gross_asset_amount)?;

        let vault_mut = &mut ctx.accounts.vault;
        vault_mut.total_underlying_deposited = vault_mut
            .total_underlying_deposited
            .checked_sub(gross_asset_amount)
            .ok_or(RouterError::MathOverflow)?;

        emit!(WithdrawEvent {
            withdrawer: ctx.accounts.user.key(),
            shares_burned: shares_in,
            amount_returned: gross_asset_amount,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    pub fn set_paused(ctx: Context<EmergencyControl>, pause_state: bool) -> Result<()> {
        let vault = &mut ctx.accounts.vault;
        vault.is_paused = pause_state;
        emit!(EmergencyStateToggledEvent {
            is_paused: pause_state,
            admin: ctx.accounts.admin.key(),
            timestamp: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// ACCOUNTS CONTEXT
// ----------------------------------------------------------------------------

#[derive(Accounts)]
pub struct InitializeVault<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    pub asset_mint: Box<Account<'info, Mint>>,

    #[account(
        init,
        payer = admin,
        space = 8 + VaultState::INIT_SPACE,
        seeds = [VAULT_SEED, asset_mint.key().as_ref()],
        bump
    )]
    pub vault: Box<Account<'info, VaultState>>,

    /// CHECK: Vault Authority PDA used to sign CPI invocations
    #[account(
        seeds = [VAULT_SEED, asset_mint.key().as_ref(), AUTHORITY_SEED],
        bump
    )]
    pub vault_authority: AccountInfo<'info>,

    #[account(
        init,
        payer = admin,
        seeds = [VAULT_SEED, asset_mint.key().as_ref(), SHARE_MINT_SEED],
        bump,
        mint::decimals = asset_mint.decimals,
        mint::authority = vault_authority,
    )]
    pub share_mint: Box<Account<'info, Mint>>,

    #[account(
        init,
        payer = admin,
        associated_token::mint = asset_mint,
        associated_token::authority = vault_authority,
    )]
    pub vault_asset_account: Box<Account<'info, TokenAccount>>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED, vault.asset_mint.as_ref()],
        bump = vault.bump
    )]
    pub vault: Box<Account<'info, VaultState>>,

    /// CHECK: Validated by PDA seeds
    #[account(
        seeds = [VAULT_SEED, vault.asset_mint.as_ref(), AUTHORITY_SEED],
        bump = vault.auth_bump
    )]
    pub vault_authority: AccountInfo<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED, vault.asset_mint.as_ref(), SHARE_MINT_SEED],
        bump
    )]
    pub share_mint: Box<Account<'info, Mint>>,

    #[account(mut)]
    pub user_token_account: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub user_share_account: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        address = vault.vault_asset_account
    )]
    pub vault_asset_account: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct RebalanceRoute<'info> {
    pub keeper: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED, vault.asset_mint.as_ref()],
        bump = vault.bump
    )]
    pub vault: Box<Account<'info, VaultState>>,

    /// CHECK: Validated with PDA seeds
    #[account(
        seeds = [VAULT_SEED, vault.asset_mint.as_ref(), AUTHORITY_SEED],
        bump = vault.auth_bump
    )]
    pub vault_authority: AccountInfo<'info>,

    #[account(mut, address = vault.vault_asset_account)]
    pub vault_asset_account: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub vault_kamino_collateral: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub vault_hedge_account: Box<Account<'info, TokenAccount>>,

    // Kamino CPI Accounts
    /// CHECK: Checked by Kamino CPI
    #[account(address = external_programs::kamino::ID)]
    pub kamino_program: AccountInfo<'info>,
    /// CHECK: Kamino accounts validated by Kamino program
    #[account(mut)]
    pub kamino_reserve: AccountInfo<'info>,
    /// CHECK: Kamino accounts validated by Kamino program
    #[account(mut)]
    pub kamino_reserve_liquidity_supply: AccountInfo<'info>,
    /// CHECK: Kamino accounts validated by Kamino program
    pub kamino_lending_market: AccountInfo<'info>,
    /// CHECK: Kamino accounts validated by Kamino program
    pub kamino_lending_market_authority: AccountInfo<'info>,
    /// CHECK: Kamino accounts validated by Kamino program
    #[account(mut)]
    pub kamino_reserve_collateral_mint: AccountInfo<'info>,

    // Marginfi CPI Accounts
    /// CHECK: Checked by Marginfi CPI
    #[account(address = external_programs::marginfi::ID)]
    pub marginfi_program: AccountInfo<'info>,
    /// CHECK: Marginfi accounts validated by Marginfi program
    pub marginfi_group: AccountInfo<'info>,
    /// CHECK: Marginfi accounts validated by Marginfi program
    #[account(mut)]
    pub marginfi_account: AccountInfo<'info>,
    /// CHECK: Marginfi accounts validated by Marginfi program
    #[account(mut)]
    pub marginfi_bank: AccountInfo<'info>,
    /// CHECK: Marginfi accounts validated by Marginfi program
    #[account(mut)]
    pub marginfi_bank_vault: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED, vault.asset_mint.as_ref()],
        bump = vault.bump
    )]
    pub vault: Box<Account<'info, VaultState>>,

    /// CHECK: Validated by PDA seeds
    #[account(
        seeds = [VAULT_SEED, vault.asset_mint.as_ref(), AUTHORITY_SEED],
        bump = vault.auth_bump
    )]
    pub vault_authority: AccountInfo<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED, vault.asset_mint.as_ref(), SHARE_MINT_SEED],
        bump
    )]
    pub share_mint: Box<Account<'info, Mint>>,

    #[account(mut)]
    pub user_token_account: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub user_share_account: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        address = vault.vault_asset_account
    )]
    pub vault_asset_account: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct EmergencyControl<'info> {
    #[account(
        mut,
        address = vault.admin @ RouterError::UnauthorizedAdmin
    )]
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED, vault.asset_mint.as_ref()],
        bump = vault.bump
    )]
    pub vault: Box<Account<'info, VaultState>>,
}

// ----------------------------------------------------------------------------
// STATE
// ----------------------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct VaultState {
    pub admin: Pubkey,
    pub asset_mint: Pubkey,
    pub share_mint: Pubkey,
    pub vault_asset_account: Pubkey,
    pub total_underlying_deposited: u64,
    pub rebalance_threshold_bps: u16,
    pub target_leverage_bps: u16,
    pub is_paused: bool,
    pub bump: u8,
    pub auth_bump: u8,
}

// ----------------------------------------------------------------------------
// EVENTS & ERRORS
// ----------------------------------------------------------------------------

#[event]
pub struct VaultInitializedEvent {
    pub admin: Pubkey,
    pub asset_mint: Pubkey,
    pub timestamp: i64,
}

#[event]
pub struct DepositEvent {
    pub depositor: Pubkey,
    pub amount: u64,
    pub shares_minted: u64,
    pub timestamp: i64,
}

#[event]
pub struct RebalanceExecutedEvent {
    pub kamino_supplied: u64,
    pub marginfi_hedged: u64,
    pub expected_net_yield_bps: u16,
    pub timestamp: i64,
}

#[event]
pub struct WithdrawEvent {
    pub withdrawer: Pubkey,
    pub shares_burned: u64,
    pub amount_returned: u64,
    pub timestamp: i64,
}

#[event]
pub struct EmergencyStateToggledEvent {
    pub is_paused: bool,
    pub admin: Pubkey,
    pub timestamp: i64,
}

#[error_code]
pub enum RouterError {
    #[msg("Calculation overflowed standard bounds")]
    MathOverflow,
    #[msg("Deposit amount must be strictly positive")]
    ZeroDepositAmount,
    #[msg("Computed share amount equals zero")]
    ZeroSharesCalculated,
    #[msg("Withdraw share count must be positive")]
    ZeroWithdrawShares,
    #[msg("Computed return asset amount is zero")]
    ZeroWithdrawAmount,
    #[msg("Target leverage exceeds hardcoded risk maximum of 3x")]
    ExcessiveLeverageConfigured,
    #[msg("Vault is currently paused by admin/risk circuit breaker")]
    ProgramPaused,
    #[msg("Caller is not the authorized keeper")]
    UnauthorizedKeeper,
    #[msg("Caller is not the authorized vault admin")]
    UnauthorizedAdmin,
    #[msg("Zero active liquidity in pool")]
    NoActiveLiquidity,
}