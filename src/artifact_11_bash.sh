# 1. Build & Test Solana Anchor Program
anchor build
anchor test

# 2. Deploy Program to Solana Mainnet-Beta
solana program deploy \
  --program-id target/deploy/delta_neutral_router-keypair.json \
  target/deploy/delta_neutral_router.so \
  --url-override https://api.mainnet-beta.solana.com

# 3. Bootstrap Keeper Bot & Start Dockerized Daemon
docker-compose up -d --build

# 4. Verify Telemetry & Live Healthcheck
curl -f http://localhost:8080/health
curl -s http://localhost:8080/metrics | grep dnyr_