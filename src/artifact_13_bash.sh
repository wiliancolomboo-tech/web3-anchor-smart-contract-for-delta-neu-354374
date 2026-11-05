# Set cluster configuration
solana config set --url https://api.mainnet-beta.solana.com

# Deploy Program Binary with Multisig Buffer
anchor deploy \
  --provider.cluster mainnet \
  --program-name delta_neutral_router \
  --program-keypair ./target/deploy/delta_neutral_router-keypair.json

# Initialize Vault for USDC Mint with 50 bps threshold & 2.0x target leverage
node dist/cli.js init-vault \
  --asset-mint EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
  --threshold-bps 50 \
  --leverage-bps 20000