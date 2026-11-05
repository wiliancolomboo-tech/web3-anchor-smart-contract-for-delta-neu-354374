# 1. Build optimized program binary with verifiable build flags
anchor build

# 2. Configure Solana CLI to Devnet / Mainnet
solana config set --url https://api.devnet.solana.com

# 3. Synchronize program keys
anchor keys sync

# 4. Deploy Anchor Program
solana program deploy ./target/deploy/delta_neutral_router.so \
  --program-id ./target/deploy/delta_neutral_router-keypair.json

# 5. Initialize Aegis Production Vault
npx ts-node scripts/init-vault.ts \
  --base-mint EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
  --target-mint So11111111111111111111111111111111111111112 \
  --target-ltv 6000 \
  --max-ltv 7500