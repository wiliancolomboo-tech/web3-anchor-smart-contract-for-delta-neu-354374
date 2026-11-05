# 1. Clone & install dependencies
git clone https://github.com/aegis-protocol/aegis-delta-neutral.git
cd aegis-delta-neutral
npm install

# 2. Build Anchor binary with deterministic SVM verification
anchor build

# 3. Synchronize program keys
anchor keys sync

# 4. Deploy to Solana Devnet / Mainnet-Beta
solana config set --url https://api.mainnet-beta.solana.com
solana-keygen recover 'your-keypair-mnemonic' -o deployer-keypair.json
anchor deploy --provider.cluster mainnet --provider.wallet ./deployer-keypair.json