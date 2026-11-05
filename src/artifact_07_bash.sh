# 1. Build optimized program binary using verified Anchor build container
anchor build --verifiable

# 2. Deploy to Solana Devnet / Mainnet
solana program deploy \
  --program-id target/deploy/delta_neutral_router-keypair.json \
  target/deploy/delta_neutral_router.so \
  --url https://api.mainnet-beta.solana.com \
  --keypair ~/.config/solana/deployer.json

# 3. Synchronize IDL with on-chain program
anchor idl init \
  --filepath target/idl/delta_neutral_router.json \
  DeltaRouter1111111111111111111111111111111111 \
  --provider.cluster mainnet