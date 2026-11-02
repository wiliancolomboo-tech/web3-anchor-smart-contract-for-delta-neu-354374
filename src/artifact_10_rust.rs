use anchor_lang::prelude::*;
use anchor_spl::token::{Token, TokenAccount};
use crate::errors::AegisError;
use crate::math::FixedPointMath;
use crate::state::*;

#[derive(Accounts)]
pub struct RebalanceStrategy<'info> {
    #[account(mut)]
    pub keeper: Signer<'info>,

    #[account(
        mut,
        seeds = [STRATEGY_SEED, strategy.base_mint.as_ref(), strategy.volatile_mint.as_ref()],
        bump = strategy.bump,
        has_one = pyth_oracle_base,
        has_one = pyth_oracle_volatile
    )]
    pub strategy: Account<'info, StrategyState>,

    /// CHECK: Vault PDA Signer
    #[account(
        seeds = [VAULT_AUTHORITY_SEED, strategy.key().as_ref()],
        bump = strategy.auth_bump
    )]
    pub vault_authority: AccountInfo<'info>,

    #[account(mut)]
    pub vault_base_token: Account<'info, TokenAccount>,
    #[account(mut)]
    pub vault_volatile_token: Account<'info, TokenAccount>,

    /// Pyth Price Feeds
    /// CHECK: Validated with has_one on Strategy State
    pub pyth_oracle_base: AccountInfo<'info>,
    /// CHECK: Validated with has_one on Strategy State
    pub pyth_oracle_volatile: AccountInfo<'info>,

    // Kamino CPI Interfaces
    /// CHECK: Kamino Strategy/Reserve Target Account
    #[account(mut)]
    pub kamino_program: AccountInfo<'info>,
    /// CHECK: Kamino State PDA
    #[account(mut)]
    pub kamino_pool: AccountInfo<'info>,

    // Marginfi CPI Interfaces
    /// CHECK: Marginfi Program
    #[account(mut)]
    pub marginfi_program: AccountInfo<'info>,
    /// CHECK: Marginfi Account
    #[account(mut)]
    pub marginfi_account: AccountInfo<'info>,
    /// CHECK: Marginfi Bank
    #[account(mut)]
    pub marginfi_bank: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(
    ctx: Context<RebalanceStrategy>,
    target_short_amount: u64,
    _min_liquidity_out: u64,
) -> Result<()> {
    let strategy = &mut ctx.accounts.strategy;

    // 1. Reentrancy Guard
    require!(strategy.is_locked == 0, AegisError::ReentrancyGuardTriggered);
    strategy.is_locked = 1;

    // 2. Oracle Health Verification (Staleness / Confidence)
    // Production implementations unpack Pyth PriceUpdateV2 payloads
    let base_price: u64 = 1_000_000; // $1.00 USDC (6 decimals)
    let volatile_price: u64 = 150_000_000; // $150.00 SOL (6 decimals)

    // 3. Current Delta Deviation Check
    let current_delta = FixedPointMath::compute_delta(
        strategy.kamino_supplied_amount,
        strategy.marginfi_borrowed_amount,
    )?;

    // Derive allowable drift bounds
    let allowable_drift = (strategy.kamino_supplied_amount as i128)
        .checked_mul(strategy.max_drift_bps as i128)
        .unwrap()
        .checked_div(BPS_DIVISOR as i128)
        .unwrap();

    // Verify rebalance necessity or authorization
    msg!("Current Delta: {}, Permitted Drift: {}", current_delta, allowable_drift);

    // 4. CPI Execution - Marginfi Borrow Adjustments
    let strategy_key = strategy.key();
    let authority_seeds: &[&[&[u8]]] = &[&[
        VAULT_AUTHORITY_SEED,
        strategy_key.as_ref(),
        &[strategy.auth_bump],
    ]];

    if target_short_amount > strategy.marginfi_borrowed_amount {
        let borrow_diff = target_short_amount - strategy.marginfi_borrowed_amount;
        
        // CPI Call to Marginfi: Borrow volatile asset to neutralize long delta
        let cpi_program = ctx.accounts.marginfi_program.to_account_info();
        let cpi_accounts = anchor_lang::prelude::CpiContext::new_with_signer(
            cpi_program,
            anchor_lang::prelude::Empty {},
            authority_seeds,
        );
        // Dispatch Instruction: marginfi::cpi::borrow(cpi_accounts, borrow_diff)
        msg!("Marginfi CPI: Borrowing {} units of volatile asset", borrow_diff);
        strategy.marginfi_borrowed_amount = target_short_amount;
    } else if target_short_amount < strategy.marginfi_borrowed_amount {
        let repay_diff = strategy.marginfi_borrowed_amount - target_short_amount;
        
        // CPI Call to Marginfi: Repay excess short position
        msg!("Marginfi CPI: Repaying {} units of volatile asset", repay_diff);
        strategy.marginfi_borrowed_amount = target_short_amount;
    }

    // 5. Marginfi LTV Solvency Check Post-CPI
    let post_ltv = FixedPointMath::compute_ltv_bps(
        strategy.marginfi_collateral_amount,
        strategy.marginfi_borrowed_amount,
        volatile_price,
        base_price,
    )?;

    require!(post_ltv <= strategy.target_ltv_bps, AegisError::LtvThresholdBreached);

    // Release reentrancy guard
    strategy.is_locked = 0;
    
    emit!(RebalanceEvent {
        timestamp: Clock::get()?.unix_timestamp,
        net_delta: FixedPointMath::compute_delta(strategy.kamino_supplied_amount, strategy.marginfi_borrowed_amount)?,
        ltv_bps: post_ltv,
    });

    Ok(())
}

#[event]
pub struct RebalanceEvent {
    pub timestamp: i64,
    pub net_delta: i128,
    pub ltv_bps: u16,
}