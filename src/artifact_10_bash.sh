# 1. Clean build of Anchor Program & Type Generation
anchor build

# 2. Get Program ID derived from keypair
solana address -k target/deploy/aura_delta_router-keypair.json

# 3. Synchronize program keys and re-compile
anchor keys sync
anchor build

# 4. Deploy to Solana Devnet / Mainnet-Beta
solana program deploy \
  --url https://api.devnet.solana.com \
  --program-id target/deploy/aura_delta_router-keypair.json \
  target/deploy/aura_delta_router.so