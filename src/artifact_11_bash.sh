# 1. Install Solana toolchain & Anchor
sh -c "$(curl -sSfL https://release.solana.com/v1.18.26/install)"
cargo install --git https://github.com/coral-xyz/anchor --tag v0.30.1 anchor-cli --locked

# 2. Build program
anchor build

# 3. Synchronize program keys
anchor keys sync

# 4. Deploy to Devnet / Mainnet
solana config set --url https://api.devnet.solana.com
anchor deploy --program-name neutra_yield --program-keypair ./target/deploy/neutra_yield-keypair.json