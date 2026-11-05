use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};
use crate::state::{VaultState, VAULT_SEED};
use crate::errors::RouterError;
use crate::events::RebalanceEvent;

#[derive(Accounts)]
pub struct RebalanceRoute<'info> {
    #[account(
        mut,
        constraint = keeper.key() == vault.keeper || keeper.key() == vault.admin @ RouterError::Unauthorized
    )]
    pub keeper: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED, vault.base_mint.as_ref()],
        bump = vault.vault_bump,
        has_one = base_vault,
        has_one = target_mint
    )]
    pub vault: Account<'info, VaultState>,

    #[account(mut)]
    pub base_vault: Account<'info, TokenAccount>,

    pub target_mint: Account<'info, Mint>,

    /// Intermediate account receiving borrowed target tokens (SOL)
    #[account(
        mut,
        token::mint = target_mint,
        token::authority = vault
    )]
    pub vault_target_token: Account<'info, TokenAccount>,

    // External CPI interface: Marginfi Program & Accounts
    /// CHECK: Verified in CPI invoke
    pub marginfi_program: UncheckedAccount<'info>,
    /// CHECK: Marginfi group account
    pub marginfi_group: UncheckedAccount<'info>,
    /// CHECK: Vault's Marginfi Account PDA
    #[account(mut)]
    pub marginfi_account: UncheckedAccount<'info>,
    /// CHECK: Marginfi Bank for Base Mint (USDC)
    #[account(mut)]
    pub marginfi_base_bank: UncheckedAccount<'info>,
    /// CHECK: Marginfi Vault for Base Mint
    #[account(mut)]
    pub marginfi_base_vault: UncheckedAccount<'info>,
    /// CHECK: Marginfi Bank for Target Mint (SOL)
    #[account(mut)]
    pub marginfi_target_bank: UncheckedAccount<'info>,
    /// CHECK: Marginfi Vault for Target Mint
    #[account(mut)]
    pub marginfi_target_vault: UncheckedAccount<'info>,

    // External CPI interface: Kamino Lending/Liquidity Program & Accounts
    /// CHECK: Verified in CPI invoke
    pub kamino_program: UncheckedAccount<'info>,
    /// CHECK: Kamino Lending Market
    pub kamino_market: UncheckedAccount<'info>,
    /// CHECK: Kamino Reserve for Target Mint
    #[account(mut)]
    pub kamino_reserve: UncheckedAccount<'info>,
    /// CHECK: Kamino Target Reserve Liquidity Supply Account
    #[account(mut)]
    pub kamino_liquidity_supply: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(
    ctx: Context<RebalanceRoute>,
    amount_base_to_deposit: u64,
    amount_target_to_borrow: u64,
    effective_target_price_scaled: u64, // Normalized target price with 6 decimals
) -> Result<()> {
    require!(!ctx.accounts.vault.is_paused, RouterError::VaultPaused);

    let vault = &mut ctx.accounts.vault;

    // Validate LTV constraint:
    // (borrowed_target * price) / total_base <= target_ltv_bps / 10000
    let total_base_collateral = vault.total_managed_base;
    require!(total_base_collateral > 0, RouterError::InsufficientLiquidity);

    let prospective_target_total = vault
        .total_borrowed_target
        .checked_add(amount_target_to_borrow)
        .ok_or(RouterError::MathOverflow)?;

    let borrow_value_in_base = (prospective_target_total as u128)
        .checked_mul(effective_target_price_scaled as u128)
        .ok_or(RouterError::MathOverflow)?
        .checked_div(1_000_000) // target price scale
        .ok_or(RouterError::MathOverflow)? as u64;

    let computed_ltv_bps = ((borrow_value_in_base as u128)
        .checked_mul(10_000)
        .ok_or(RouterError::MathOverflow)?
        .checked_div(total_base_collateral as u128)
        .ok_or(RouterError::MathOverflow)?) as u16;

    require!(
        computed_ltv_bps <= vault.max_ltv_bps,
        RouterError::LtvLimitExceeded
    );

    let base_mint_key = vault.base_mint;
    let seeds = &[
        VAULT_SEED,
        base_mint_key.as_ref(),
        &[vault.vault_bump],
    ];
    let signer = &[&seeds[..]];

    // 1. CPI: Transfer base collateral (USDC) from Base Vault to Marginfi Reserve
    if amount_base_to_deposit > 0 {
        let deposit_cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.base_vault.to_account_info(),
                to: ctx.accounts.marginfi_base_vault.to_account_info(),
                authority: vault.to_account_info(),
            },
            signer,
        );
        token::transfer(deposit_cpi_ctx, amount_base_to_deposit)?;
    }

    // 2. CPI: Borrow Target Asset (SOL) from Marginfi into Vault Target Token
    // Marginfi CPI: `marginfi_account_borrow` instruction data layout: [discriminator: 8, amount: 8]
    if amount_target_to_borrow > 0 {
        let mut borrow_ix_data = Vec::with_capacity(16);
        // Anchor Marginfi Borrow ix discriminator
        let borrow_discriminator: [u8; 8] = [0x50, 0x1d, 0x5a, 0x6e, 0x55, 0x05, 0xcb, 0xc1];
        borrow_ix_data.extend_from_slice(&borrow_discriminator);
        borrow_ix_data.extend_from_slice(&amount_target_to_borrow.to_le_bytes());

        let borrow_accounts = vec![
            AccountMeta::new_readonly(ctx.accounts.marginfi_group.key(), false),
            AccountMeta::new(ctx.accounts.marginfi_account.key(), false),
            AccountMeta::new_readonly(vault.key(), true),
            AccountMeta::new(ctx.accounts.marginfi_target_bank.key(), false),
            AccountMeta::new(ctx.accounts.marginfi_target_vault.key(), false),
            AccountMeta::new(ctx.accounts.vault_target_token.key(), false),
            AccountMeta::new_readonly(ctx.accounts.token_program.key(), false),
        ];

        let ix = solana_program::instruction::Instruction {
            program_id: ctx.accounts.marginfi_program.key(),
            accounts: borrow_accounts,
            data: borrow_ix_data,
        };

        solana_program::program::invoke_signed(
            &ix,
            &[
                ctx.accounts.marginfi_program.to_account_info(),
                ctx.accounts.marginfi_group.to_account_info(),
                ctx.accounts.marginfi_account.to_account_info(),
                vault.to_account_info(),
                ctx.accounts.marginfi_target_bank.to_account_info(),
                ctx.accounts.marginfi_target_vault.to_account_info(),
                ctx.accounts.vault_target_token.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
            ],
            signer,
        )?;

        // 3. CPI: Deposit Borrowed Target Asset (SOL) into Kamino Reserve for Yield
        // Kamino Lending CPI: `deposit_reserve_liquidity` instruction layout: [discriminator: 8, liquidity_amount: 8]
        let mut kamino_ix_data = Vec::with_capacity(16);
        let kamino_discriminator: [u8; 8] = [0xfa, 0x22, 0xd6, 0x9b, 0x5f, 0x31, 0x10, 0x82];
        kamino_ix_data.extend_from_slice(&kamino_discriminator);
        kamino_ix_data.extend_from_slice(&amount_target_to_borrow.to_le_bytes());

        let kamino_accounts = vec![
            AccountMeta::new(ctx.accounts.vault_target_token.key(), false),
            AccountMeta::new(ctx.accounts.kamino_liquidity_supply.key(), false),
            AccountMeta::new(ctx.accounts.kamino_reserve.key(), false),
            AccountMeta::new_readonly(ctx.accounts.kamino_market.key(), false),
            AccountMeta::new_readonly(vault.key(), true),
            AccountMeta::new_readonly(ctx.accounts.token_program.key(), false),
        ];

        let kamino_ix = solana_program::instruction::Instruction {
            program_id: ctx.accounts.kamino_program.key(),
            accounts: kamino_accounts,
            data: kamino_ix_data,
        };

        solana_program::program::invoke_signed(
            &kamino_ix,
            &[
                ctx.accounts.kamino_program.to_account_info(),
                ctx.accounts.vault_target_token.to_account_info(),
                ctx.accounts.kamino_liquidity_supply.to_account_info(),
                ctx.accounts.kamino_reserve.to_account_info(),
                ctx.accounts.kamino_market.to_account_info(),
                vault.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
            ],
            signer,
        )?;

        vault.total_borrowed_target = prospective_target_total;
        vault.total_deployed_target = vault
            .total_deployed_target
            .checked_add(amount_target_to_borrow)
            .ok_or(RouterError::MathOverflow)?;
    }

    emit!(RebalanceEvent {
        keeper: ctx.accounts.keeper.key(),
        collateral_deposited_mfi: amount_base_to_deposit,
        borrowed_target_mfi: amount_target_to_borrow,
        supplied_target_kamino: amount_target_to_borrow,
        current_ltv_bps: computed_ltv_bps,
        timestamp: Clock::get()?.unix_timestamp,
    });

    Ok(())
}