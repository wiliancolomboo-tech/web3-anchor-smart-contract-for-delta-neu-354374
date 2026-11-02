import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { PublicKey, Keypair, Connection, clusterApiUrl } from "@solana/web3.js";
import axios from "axios";
import * as dotenv from "dotenv";

dotenv.config();

interface HealthPayload {
  vault: string;
  spotValueUsd: number;
  hedgeValueUsd: number;
  driftBps: number;
  timestamp: string;
}

export class AegisDeltaKeeper {
  private connection: Connection;
  private wallet: anchor.Wallet;
  private program: Program;
  private vaultPubkey: PublicKey;
  private webhookUrl: string;

  constructor() {
    const rpcUrl = process.env.SOLANA_RPC_URL || clusterApiUrl("devnet");
    this.connection = new Connection(rpcUrl, "confirmed");

    const secretKey = JSON.parse(process.env.KEEPER_PRIVATE_KEY || "[]");
    const keypair = Keypair.fromSecretKey(Uint8Array.from(secretKey));
    this.wallet = new anchor.Wallet(keypair);

    const provider = new anchor.AnchorProvider(this.connection, this.wallet, {
      preflightCommitment: "confirmed",
    });
    anchor.setProvider(provider);

    // Anchor program IDL initialization
    const programId = new PublicKey(process.env.PROGRAM_ID!);
    const idl = JSON.parse(process.env.PROGRAM_IDL!);
    this.program = new Program(idl, provider);

    this.vaultPubkey = new PublicKey(process.env.STRATEGY_VAULT_PUBKEY!);
    this.webhookUrl = process.env.ENTERPRISE_WEBHOOK_URL || "";
  }

  public async monitorAndRebalance(): Promise<void> {
    console.log(`[AEGIS-DAEMON] Monitoring AegisDelta Vault: ${this.vaultPubkey.toBase58()}`);

    setInterval(async () => {
      try {
        const vaultAccount: any = await this.program.account.strategyVault.fetch(this.vaultPubkey);
        
        // Mock oracle price resolution (production reads from Pyth Client)
        const collateralPriceUsd = 145.50; // SOL Price
        const debtPriceUsd = 1.00;        // USDC Price

        const spotUsd = (vaultAccount.totalDepositedCollateral.toNumber() / 1e9) * collateralPriceUsd;
        const hedgeUsd = (vaultAccount.activeDebtBalance.toNumber() / 1e6) * debtPriceUsd;

        const deltaDriftBps = Math.abs((spotUsd - hedgeUsd) / spotUsd) * 10000;
        console.log(`[METRICS] Spot: $${spotUsd.toFixed(2)} | Hedge: $${hedgeUsd.toFixed(2)} | Drift: ${deltaDriftBps.toFixed(2)} bps`);

        if (deltaDriftBps > vaultAccount.driftThresholdBps.toNumber()) {
          console.warn(`[TRIGGER] Drift ${deltaDriftBps} bps exceeds threshold ${vaultAccount.driftThresholdBps.toNumber()} bps! Rebalancing...`);
          await this.executeRebalance(spotUsd, hedgeUsd);
        }

        // Telemetry Heartbeat
        await this.postEnterpriseTelemetry({
          vault: this.vaultPubkey.toBase58(),
          spotValueUsd: spotUsd,
          hedgeValueUsd: hedgeUsd,
          driftBps: deltaDriftBps,
          timestamp: new Date().toISOString(),
        });
      } catch (err: any) {
        console.error(`[ERROR] Daemon monitoring cycle failed: ${err.message}`);
      }
    }, 15000); // 15s polling frequency
  }

  private async executeRebalance(spotUsd: number, hedgeUsd: number): Promise<void> {
    const isReduceDebt = hedgeUsd > spotUsd;
    const diffUsd = Math.abs(spotUsd - hedgeUsd);
    const adjustmentAmountUnits = new anchor.BN(Math.floor(diffUsd * 1e6));

    const tx = await this.program.methods
      .rebalanceStrategy(adjustmentAmountUnits, isReduceDebt)
      .accounts({
        strategyVault: this.vaultPubkey,
        keeper: this.wallet.publicKey,
      })
      .rpc();

    console.log(`[SUCCESS] Rebalanced on-chain. TX: ${tx}`);
  }

  private async postEnterpriseTelemetry(payload: HealthPayload): Promise<void> {
    if (!this.webhookUrl) return;

    try {
      await axios.post(this.webhookUrl, {
        username: "AegisDelta Guardian",
        embeds: [
          {
            title: "Delta Neutrality Telemetry Heartbeat",
            color: payload.driftBps > 150 ? 16711680 : 65280,
            fields: [
              { name: "Strategy Vault", value: payload.vault, inline: false },
              { name: "Spot Value (USD)", value: `$${payload.spotValueUsd.toFixed(2)}`, inline: true },
              { name: "Hedge Value (USD)", value: `$${payload.hedgeValueUsd.toFixed(2)}`, inline: true },
              { name: "Drift (BPS)", value: `${payload.driftBps.toFixed(2)} bps`, inline: true },
            ],
            timestamp: payload.timestamp,
          },
        ],
      });
    } catch (err: any) {
      console.error(`[ALERT] Failed to push webhook notification: ${err.message}`);
    }
  }
}

// Entrypoint
if (require.main === module) {
  const daemon = new AegisDeltaKeeper();
  daemon.monitorAndRebalance();
}