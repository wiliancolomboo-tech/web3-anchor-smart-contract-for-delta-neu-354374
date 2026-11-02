use crate::errors::AegisError;
use anchor_lang::prelude::*;

pub struct FixedPointMath;

impl FixedPointMath {
    /// Calculate current net delta of strategy:
    /// Delta = Volatile_Asset_Long (from Kamino) - Volatile_Asset_Short (from Marginfi)
    pub fn compute_delta(
        kamino_long_units: u64,
        marginfi_borrowed_units: u64,
    ) -> Result<i128> {
        let long_i128 = kamino_long_units as i128;
        let short_i128 = marginfi_borrowed_units as i128;
        
        long_i128
            .checked_sub(short_i128)
            .ok_or_else(|| error!(AegisError::MathOverflow))
    }

    /// Calculate Marginfi LTV:
    /// LTV = (Borrowed_Value_USD / Collateral_Value_USD) * 10,000 bps
    pub fn compute_ltv_bps(
        collateral_base_units: u64,
        borrowed_volatile_units: u64,
        volatile_price_usd_scaled: u64, // 6 decimals precision
        base_price_usd_scaled: u64,     // 6 decimals precision
    ) -> Result<u16> {
        if collateral_base_units == 0 {
            return Ok(0);
        }

        let collateral_val = (collateral_base_units as u128)
            .checked_mul(base_price_usd_scaled as u128)
            .ok_or_else(|| error!(AegisError::MathOverflow))?;

        let borrowed_val = (borrowed_volatile_units as u128)
            .checked_mul(volatile_price_usd_scaled as u128)
            .ok_or_else(|| error!(AegisError::MathOverflow))?;

        if collateral_val == 0 {
            return Err(error!(AegisError::MathOverflow));
        }

        let ltv_bps = borrowed_val
            .checked_mul(10_000)
            .ok_or_else(|| error!(AegisError::MathOverflow))?
            .checked_div(collateral_val)
            .ok_or_else(|| error!(AegisError::MathOverflow))?;

        Ok(ltv_bps as u16)
    }

    /// Linear share valuation for deposits:
    /// Shares = (DepositAmount * TotalShares) / TotalStrategyValuation
    pub fn compute_shares_to_mint(
        deposit_amount: u64,
        total_strategy_value: u64,
        total_shares: u64,
    ) -> Result<u64> {
        if total_shares == 0 || total_strategy_value == 0 {
            return Ok(deposit_amount); // 1:1 initial pricing
        }

        let shares = (deposit_amount as u128)
            .checked_mul(total_shares as u128)
            .ok_or_else(|| error!(AegisError::MathOverflow))?
            .checked_div(total_strategy_value as u128)
            .ok_or_else(|| error!(AegisError::MathOverflow))?;

        Ok(shares as u64)
    }
}