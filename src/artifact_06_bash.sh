# Set Solana CLI to Target Network
solana config set --url https://api.mainnet-beta.solana.com

# Build program binaries
anchor build

# Recover or verify program ID address
solana address -k target/deploy/delta_neutral_router-keypair.json

# Deploy to network using buffer account
solana program deploy \
  --program-id target/deploy/delta_neutral_router-keypair.json \
  target/deploy/delta_neutral_router.so \
  --with-compute-unit-price 10000 \
  --max-sign-attempts 5